use super::clientside::SelectionEvents;
use super::{InnerServerState, MyWorld, ServerState};
use crate::{X11Selection, XConnection};
use log::{info, warn};
use rustix::pipe::{pipe_with, PipeFlags};
use smithay_client_toolkit::data_device_manager::ReadPipe;
use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Proxy, QueueHandle};

use smithay_client_toolkit::data_device_manager::{
    DataDeviceManagerState, data_device::DataDevice,
    data_offer::SelectionOffer as WlSelectionOffer, data_source::CopyPasteSource,
};
use smithay_client_toolkit::primary_selection::PrimarySelectionManagerState;
use smithay_client_toolkit::primary_selection::device::PrimarySelectionDevice;
use smithay_client_toolkit::primary_selection::offer::PrimarySelectionOffer;
use smithay_client_toolkit::primary_selection::selection::PrimarySelectionSource;
use std::io::Read;
use std::os::fd::AsFd;
use std::rc::{Rc, Weak};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::ExtDataControlDeviceV1,
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::ExtDataControlOfferV1,
    ext_data_control_source_v1::ExtDataControlSourceV1,
};

#[derive(Copy, Clone, Debug)]
pub(super) enum SourceKind {
    Clipboard,
    Primary,
}

pub(super) struct SelectionStates<S: X11Selection> {
    clipboard: Option<SelectionState<S, Clipboard>>,
    primary: Option<SelectionState<S, Primary>>,
    control: Option<DataControlState>,
}

struct DataControlState {
    manager: ExtDataControlManagerV1,
    device: Option<ExtDataControlDeviceV1>,
}

impl DataControlState {
    /// Whether data-control can carry selections right now. A device that has not been created
    /// yet (no seat) still counts, since its sources are published once the seat shows up.
    fn is_usable(&self) -> bool {
        match &self.device {
            Some(device) => device.is_alive(),
            None => true,
        }
    }

    fn live_device(&self) -> Option<&ExtDataControlDeviceV1> {
        self.device.as_ref().filter(|device| device.is_alive())
    }
}

impl<S: X11Selection> SelectionStates<S> {
    /// Whether Wayland selections should be read through ext-data-control. The core protocols
    /// remain the fallback when the compositor does not advertise it or its device is gone.
    fn control_usable(&self) -> bool {
        self.control.as_ref().is_some_and(DataControlState::is_usable)
    }

    pub fn new(global_list: &GlobalList, qh: &QueueHandle<MyWorld>) -> Self {
        Self {
            clipboard: DataDeviceManagerState::bind(global_list, qh)
                .inspect_err(|e| {
                    warn!("Could not bind data device manager ({e:?}). Clipboard will not work.")
                })
                .ok()
                .map(SelectionState::new),
            primary: PrimarySelectionManagerState::bind(global_list, qh)
                .inspect_err(|_| info!("Primary selection unsupported."))
                .ok()
                .map(SelectionState::new),
            control: global_list
                .bind::<ExtDataControlManagerV1, _, _>(qh, 1..=1, ())
                .ok()
                .map(|manager| DataControlState {
                    manager,
                    device: None,
                }),
        }
    }

    pub fn seat_created(&mut self, qh: &QueueHandle<MyWorld>, seat: &WlSeat) {
        if let Some(c) = &mut self.clipboard {
            c.device = Some(c.manager.get_data_device(qh, seat));
        }

        if let Some(d) = &mut self.primary {
            d.device = Some(d.manager.get_selection_device(qh, seat));
        }

        let Some(control) = &mut self.control else {
            return;
        };
        let device = control.manager.get_data_device(seat, qh, ());

        if let Some(SelectionData::X11 {
            inner: SelectionSource::Control { inner, published },
            ..
        }) = self
            .clipboard
            .as_mut()
            .and_then(|state| state.source.as_mut())
        {
            if !*published {
                device.set_selection(Some(inner));
                *published = true;
            }
        }
        if let Some(SelectionData::X11 {
            inner: SelectionSource::Control { inner, published },
            ..
        }) = self
            .primary
            .as_mut()
            .and_then(|state| state.source.as_mut())
        {
            if !*published {
                device.set_primary_selection(Some(inner));
                *published = true;
            }
        }

        control.device = Some(device);
    }
}

enum SelectionData<S: X11Selection, T: SelectionType> {
    X11 {
        inner: SelectionSource<T::Source>,
        data: Weak<S>,
    },
    Foreign(ForeignSelection<T>),
}

enum SelectionSource<T> {
    Core(T),
    Control {
        inner: ExtDataControlSourceV1,
        published: bool,
    },
}

fn destroy_selection<S: X11Selection, T: SelectionType>(selection: SelectionData<S, T>) {
    match selection {
        SelectionData::X11 {
            inner: SelectionSource::Control { inner, .. },
            ..
        } => inner.destroy(),
        _ => {}
    }
}

struct SelectionState<S: X11Selection, T: SelectionType> {
    manager: T::Manager,
    device: Option<T::DataDevice>,
    source: Option<SelectionData<S, T>>,
}

impl<S: X11Selection, T: SelectionType> SelectionState<S, T> {
    fn new(manager: T::Manager) -> Self {
        Self {
            manager,
            device: None,
            source: None,
        }
    }
}

impl<S: X11Selection> InnerServerState<S> {
    pub(super) fn handle_selection_events(&mut self) {
        self.handle_impl::<Clipboard>();
        self.handle_impl::<Primary>();
    }

    fn handle_impl<T: SelectionType>(&mut self) {
        let control_usable = self.selection_states.control_usable();
        let (requests, cancelled) = {
            let events = T::get_events(&mut self.world);
            (
                std::mem::take(&mut events.requests),
                std::mem::take(&mut events.cancelled),
            )
        };
        let (control_requests, control_cancellations, control_offer) = {
            let events = T::get_control_events(&mut self.world);
            (
                std::mem::take(&mut events.requests),
                std::mem::take(&mut events.cancelled),
                events.offer.take(),
            )
        };

        let Some(state) = T::selection_state(&mut self.selection_states) else {
            if let Some((offer, _)) = control_offer {
                offer.destroy();
            }
            return;
        };

        // Serve requests for the active X11-backed source. Requests are matched against the
        // protocol object currently parked in the slot, so requests for a replaced source are
        // dropped instead of being answered with newer data.
        match state.source.as_ref() {
            Some(SelectionData::X11 {
                inner: SelectionSource::Core(inner),
                data,
            }) => {
                let source_id = T::source_id(inner);
                for (request_source, mime_type, fd) in requests {
                    if request_source == source_id {
                        if let Some(data) = data.upgrade() {
                            data.write_to(&mime_type, fd);
                        }
                    }
                }
            }
            Some(SelectionData::X11 {
                inner: SelectionSource::Control { inner, .. },
                data,
            }) => {
                for (source, mime_type, fd) in control_requests {
                    if source == *inner {
                        if let Some(data) = data.upgrade() {
                            data.write_to(&mime_type, fd);
                        }
                    }
                }
            }
            _ => {}
        }

        let core_cancelled = matches!(
            state.source.as_ref(),
            Some(SelectionData::X11 {
                inner: SelectionSource::Core(inner),
                ..
            }) if cancelled.contains(&T::source_id(inner))
        );
        if core_cancelled {
            let old = state.source.take().unwrap();
            destroy_selection(old);
        }

        // Only clear the slot when the cancelled source is still the active one; a cancellation
        // for a source that has already been replaced must not drop the newer source.
        let control_cancelled = matches!(
            state.source.as_ref(),
            Some(SelectionData::X11 {
                inner: SelectionSource::Control { inner, .. },
                ..
            }) if control_cancellations.contains(inner)
        );
        if control_cancelled {
            let old = state.source.take().unwrap();
            destroy_selection(old);
        }

        if state.source.is_none() {
            if let Some((offer, mime_types)) = control_offer {
                let foreign = ForeignSelection::from_control(offer, mime_types);
                state.source = Some(SelectionData::Foreign(foreign));
            } else if !control_usable {
                // Compositors without ext-data-control keep delivering Wayland selections
                // through the core data device and primary-selection protocols.
                let events = T::get_events(&mut self.world);
                if let Some(offer) = T::take_offer(&mut events.offer) {
                    let mime_types = T::get_mimes(&offer);
                    let foreign = ForeignSelection::from_core(offer, mime_types);
                    state.source = Some(SelectionData::Foreign(foreign));
                }
            }
        } else if matches!(state.source.as_ref(), Some(SelectionData::Foreign(_))) {
            // The X11 side has not picked up the previous offer yet, so keep the newest one.
            if let Some((offer, mime_types)) = control_offer {
                let old = state.source.take().unwrap();
                destroy_selection(old);
                let foreign = ForeignSelection::from_control(offer, mime_types);
                state.source = Some(SelectionData::Foreign(foreign));
            }
        } else if let Some((offer, _)) = control_offer {
            // The slot holds our own X11-backed source. A data-control offer received while we
            // own the selection is either that source echoed back by the compositor or a
            // selection change whose cancellation has not been processed yet; neither may be
            // adopted as a foreign selection.
            offer.destroy();
        }
    }

    pub(crate) fn set_selection_source<T: SelectionType>(&mut self, selection: &Rc<S>) {
        let control = self
            .selection_states
            .control
            .as_ref()
            .filter(|control| control.is_usable())
            .map(|control| (control.manager.clone(), control.live_device().cloned()));
        let serial = self.last_kb_serial.as_ref().map(|(_seat, serial)| *serial);

        let Some(state) = T::selection_state(&mut self.selection_states) else {
            return;
        };

        let inner = if let Some((manager, device)) = control {
            // ext-data-control has no serial requirement, so use it whenever the compositor
            // advertises it instead of relying on keyboard focus.
            let src = manager.create_data_source(&self.qh, T::KIND);
            for mime in selection.mime_types() {
                src.offer(mime.to_owned());
            }
            let published = if let Some(device) = device.as_ref() {
                T::set_control_selection(device, Some(&src));
                true
            } else {
                false
            };
            SelectionSource::Control {
                inner: src,
                published,
            }
        } else {
            // Preserve the core-only behavior when ext-data-control is unavailable. Without an
            // input serial the source is retained, but cannot be published.
            let src = T::create_source(&state.manager, &self.qh, selection.mime_types());
            if let Some(serial) = serial {
                T::set_selection(&src, state.device.as_ref().unwrap(), serial);
            }
            SelectionSource::Core(src)
        };

        if let Some(old) = state.source.take() {
            destroy_selection(old);
        }
        state.source = Some(SelectionData::X11 {
            inner,
            data: Rc::downgrade(selection),
        });
    }

    pub(crate) fn new_selection<T: SelectionType>(&mut self) -> Option<ForeignSelection<T>> {
        T::selection_state(&mut self.selection_states)
            .as_mut()
            .and_then(|state| {
                state.source.take().and_then(|s| match s {
                    SelectionData::Foreign(f) => Some(f),
                    SelectionData::X11 { .. } => {
                        state.source = Some(s);
                        None
                    }
                })
            })
    }
}

pub struct ForeignSelection<T: SelectionType> {
    pub mime_types: Box<[String]>,
    inner: ForeignOffer<T::Offer>,
}

enum ForeignOffer<O> {
    Core(O),
    Control(ExtDataControlOfferV1),
}

#[allow(private_bounds)]
impl<T: SelectionType> ForeignSelection<T> {
    fn from_core(offer: T::Offer, mime_types: Box<[String]>) -> Self {
        Self {
            mime_types,
            inner: ForeignOffer::Core(offer),
        }
    }

    fn from_control(offer: ExtDataControlOfferV1, mime_types: Box<[String]>) -> Self {
        Self {
            mime_types,
            inner: ForeignOffer::Control(offer),
        }
    }

    pub(crate) fn receive(
        &self,
        mime_type: String,
        state: &ServerState<impl XConnection>,
    ) -> Vec<u8> {
        let mut pipe = match &self.inner {
            ForeignOffer::Core(offer) => T::receive_offer(offer, mime_type),
            ForeignOffer::Control(offer) => receive_control_offer(offer, mime_type),
        }
        .unwrap();
        state.queue.flush().unwrap();
        let mut data = Vec::new();
        pipe.read_to_end(&mut data).unwrap();
        data
    }
}

impl<T: SelectionType> Drop for ForeignSelection<T> {
    fn drop(&mut self) {
        if let ForeignOffer::Control(offer) = &self.inner {
            offer.destroy();
        }
    }
}

fn receive_control_offer(
    offer: &ExtDataControlOfferV1,
    mime_type: String,
) -> std::io::Result<ReadPipe> {
    let (read, write) = pipe_with(PipeFlags::CLOEXEC)?;
    offer.receive(mime_type, write.as_fd());
    Ok(ReadPipe::from(read))
}

#[allow(private_bounds, private_interfaces)]
pub trait SelectionType: Sized {
    type Source;
    type Offer;
    type Manager;
    type DataDevice;

    const KIND: SourceKind;

    // The methods in this trait shouldn't be used outside of this file.

    fn selection_state<S: X11Selection>(
        state: &mut SelectionStates<S>,
    ) -> &mut Option<SelectionState<S, Self>>;

    fn create_source(
        manager: &Self::Manager,
        qh: &QueueHandle<MyWorld>,
        mime_types: Vec<&str>,
    ) -> Self::Source;

    fn set_selection(source: &Self::Source, device: &Self::DataDevice, serial: u32);

    fn source_id(source: &Self::Source) -> wayland_client::backend::ObjectId;

    fn set_control_selection(
        device: &ExtDataControlDeviceV1,
        source: Option<&ExtDataControlSourceV1>,
    );

    fn get_events(world: &mut MyWorld) -> &mut SelectionEvents<Self::Offer>;

    fn get_control_events(world: &mut MyWorld) -> &mut super::clientside::ControlSourceEvents;

    fn receive_offer(offer: &Self::Offer, mime_type: String) -> std::io::Result<ReadPipe>;

    fn take_offer(offer: &mut Option<Self::Offer>) -> Option<Self::Offer> {
        offer.take()
    }

    fn get_mimes(offer: &Self::Offer) -> Box<[String]>;
}

pub enum Clipboard {}
pub enum Primary {}

#[allow(private_bounds, private_interfaces)]
impl SelectionType for Clipboard {
    type Source = CopyPasteSource;
    type Offer = WlSelectionOffer;
    type Manager = DataDeviceManagerState;
    type DataDevice = DataDevice;

    const KIND: SourceKind = SourceKind::Clipboard;

    fn selection_state<S: X11Selection>(
        state: &mut SelectionStates<S>,
    ) -> &mut Option<SelectionState<S, Self>> {
        &mut state.clipboard
    }

    fn create_source(
        manager: &Self::Manager,
        qh: &QueueHandle<MyWorld>,
        mime_types: Vec<&str>,
    ) -> Self::Source {
        manager.create_copy_paste_source(qh, mime_types)
    }

    fn set_selection(source: &Self::Source, device: &Self::DataDevice, serial: u32) {
        source.set_selection(device, serial);
    }

    fn source_id(source: &Self::Source) -> wayland_client::backend::ObjectId {
        source.inner().id()
    }

    fn set_control_selection(
        device: &ExtDataControlDeviceV1,
        source: Option<&ExtDataControlSourceV1>,
    ) {
        device.set_selection(source);
    }

    fn get_events(world: &mut MyWorld) -> &mut SelectionEvents<Self::Offer> {
        &mut world.clipboard
    }

    fn get_control_events(world: &mut MyWorld) -> &mut super::clientside::ControlSourceEvents {
        &mut world.control_clipboard
    }

    fn take_offer(offer: &mut Option<Self::Offer>) -> Option<Self::Offer> {
        offer.take().filter(|offer| offer.inner().is_alive())
    }

    fn get_mimes(offer: &Self::Offer) -> Box<[String]> {
        offer.with_mime_types(|mimes| mimes.into())
    }

    fn receive_offer(offer: &Self::Offer, mime_type: String) -> std::io::Result<ReadPipe> {
        offer.receive(mime_type).map_err(|e| {
            use smithay_client_toolkit::data_device_manager::data_offer::DataOfferError;
            match e {
                DataOfferError::InvalidReceive => std::io::Error::from(std::io::ErrorKind::Other),
                DataOfferError::Io(e) => e,
            }
        })
    }
}

#[allow(private_bounds, private_interfaces)]
impl SelectionType for Primary {
    type Source = PrimarySelectionSource;
    type Offer = PrimarySelectionOffer;
    type Manager = PrimarySelectionManagerState;
    type DataDevice = PrimarySelectionDevice;

    const KIND: SourceKind = SourceKind::Primary;

    fn selection_state<S: X11Selection>(
        state: &mut SelectionStates<S>,
    ) -> &mut Option<SelectionState<S, Self>> {
        &mut state.primary
    }

    fn create_source(
        manager: &Self::Manager,
        qh: &QueueHandle<MyWorld>,
        mime_types: Vec<&str>,
    ) -> Self::Source {
        manager.create_selection_source(qh, mime_types)
    }

    fn set_selection(source: &Self::Source, device: &Self::DataDevice, serial: u32) {
        source.set_selection(device, serial);
    }

    fn source_id(source: &Self::Source) -> wayland_client::backend::ObjectId {
        source.inner().id()
    }

    fn set_control_selection(
        device: &ExtDataControlDeviceV1,
        source: Option<&ExtDataControlSourceV1>,
    ) {
        device.set_primary_selection(source);
    }

    fn get_events(world: &mut MyWorld) -> &mut SelectionEvents<Self::Offer> {
        &mut world.primary
    }

    fn get_control_events(world: &mut MyWorld) -> &mut super::clientside::ControlSourceEvents {
        &mut world.control_primary
    }

    fn receive_offer(offer: &Self::Offer, mime_type: String) -> std::io::Result<ReadPipe> {
        offer.receive(mime_type)
    }

    fn get_mimes(offer: &Self::Offer) -> Box<[String]> {
        offer.with_mime_types(|mimes| mimes.into())
    }
}
