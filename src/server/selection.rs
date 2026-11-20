use super::clientside::{ControlDevice, ControlOffer, SelectionEvents};
use super::{InnerServerState, MyWorld, ServerState};
use crate::{X11Selection, XConnection};
use log::{info, warn};
use rustix::pipe::{PipeFlags, pipe_with};
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
pub(crate) enum SourceKind {
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
    seat: Option<WlSeat>,
}

impl DataControlState {
    fn usable(&self, device: Option<&ExtDataControlDeviceV1>) -> bool {
        self.seat.is_none() || device.is_some()
    }
}

impl<S: X11Selection> SelectionStates<S> {
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
                    seat: None,
                }),
        }
    }

    pub fn seat_created(&mut self, qh: &QueueHandle<MyWorld>, seat: &WlSeat, world: &mut MyWorld) {
        if let Some(c) = &mut self.clipboard {
            c.device = Some(c.manager.get_data_device(qh, seat));
        }

        if let Some(d) = &mut self.primary {
            d.device = Some(d.manager.get_selection_device(qh, seat));
        }

        let Some(control) = &mut self.control else {
            return;
        };
        control.seat = Some(seat.clone());
        // Share the initial device. Refreshing one selection later must not
        // invalidate an offer or interrupt a transfer for the other.
        let device = Rc::new(ControlDevice(control.manager.get_data_device(seat, qh, ())));
        if let Some(state) = &mut self.clipboard {
            state.bind_control(device.clone(), world);
        }
        if let Some(state) = &mut self.primary {
            state.bind_control(device, world);
        }
    }
}

struct X11Source<S, T: SelectionType> {
    inner: SelectionSource<T::Source>,
    data: Weak<S>,
}

pub(crate) enum SelectionUpdate<T> {
    Set(T),
    Clear,
}

enum SelectionSource<T> {
    Core(T),
    Control {
        inner: ExtDataControlSourceV1,
        published: bool,
    },
}

impl<T> Drop for SelectionSource<T> {
    fn drop(&mut self) {
        if let Self::Control { inner, .. } = self {
            inner.destroy();
        }
    }
}

struct SelectionState<S: X11Selection, T: SelectionType> {
    manager: T::Manager,
    device: Option<T::DataDevice>,
    source: Option<X11Source<S, T>>,
    pending: Option<SelectionUpdate<ForeignSelection<T>>>,
}

impl<S: X11Selection, T: SelectionType> SelectionState<S, T> {
    fn new(manager: T::Manager) -> Self {
        Self {
            manager,
            device: None,
            source: None,
            pending: None,
        }
    }

    fn bind_control(&mut self, device: Rc<ControlDevice>, world: &mut MyWorld) {
        if let Some(X11Source {
            inner: SelectionSource::Control { inner, published },
            ..
        }) = &mut self.source
        {
            if !*published {
                T::set_control_selection(&device.0, Some(inner));
                *published = true;
            }
        }
        T::get_events(world).control.subscribe(device);
    }
}

impl<S: X11Selection> InnerServerState<S> {
    pub(super) fn handle_selection_events(&mut self) {
        self.handle_impl::<Clipboard>();
        self.handle_impl::<Primary>();
    }

    fn handle_impl<T: SelectionType>(&mut self) {
        let events = T::get_events(&mut self.world);
        let source_events = std::mem::take(&mut events.source);
        let control_bound = events.control.device().is_some();
        let control_offer = events.control.offer.take();
        let Some(state) = T::selection_state(&mut self.selection_states) else {
            return;
        };

        if let Some(source) = &state.source {
            let id = match &source.inner {
                SelectionSource::Core(inner) => T::source_id(inner),
                SelectionSource::Control { inner, .. } => inner.id(),
            };
            if let Some(data) = source.data.upgrade() {
                for (sender, mime, fd) in source_events.requests {
                    if sender == id {
                        data.write_to(&mime, fd);
                    }
                }
            }
            if !source_events.cancelled.contains(&id) {
                return;
            }

            let refresh = control_bound && matches!(source.inner, SelectionSource::Control { .. });
            state.source = None;
            if refresh {
                // Offers do not identify their source. After cancellation a fresh device
                // reports the current selection, regardless of when the old echo arrived.
                let control = self.selection_states.control.as_ref().unwrap();
                let seat = control.seat.as_ref().unwrap();
                let device = control.manager.get_data_device(seat, &self.qh, ());
                events.control.subscribe(Rc::new(ControlDevice(device)));
                return;
            }
        }

        if control_bound {
            if let Some(update) = control_offer {
                state.pending = Some(match update {
                    SelectionUpdate::Set(offer) => {
                        SelectionUpdate::Set(ForeignSelection::from_control(offer))
                    }
                    SelectionUpdate::Clear => SelectionUpdate::Clear,
                });
            }
        } else if let Some(offer) = T::take_offer(&mut events.offer) {
            let mime_types = T::get_mimes(&offer);
            state.pending = Some(SelectionUpdate::Set(ForeignSelection::from_core(
                offer, mime_types,
            )));
        }
    }

    pub(crate) fn set_selection_source<T: SelectionType>(&mut self, selection: &Rc<S>) {
        let device = T::get_events(&mut self.world).control.device();
        let control = self
            .selection_states
            .control
            .as_ref()
            .filter(|c| c.usable(device))
            .map(|c| (c.manager.clone(), device.cloned()));
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

        state.pending = None;
        state.source = Some(X11Source {
            inner,
            data: Rc::downgrade(selection),
        });
    }

    pub(crate) fn take_selection_update<T: SelectionType>(
        &mut self,
    ) -> Option<SelectionUpdate<ForeignSelection<T>>> {
        T::selection_state(&mut self.selection_states)
            .as_mut()?
            .pending
            .take()
    }

    #[cfg(test)]
    pub(super) fn new_selection<T: SelectionType>(&mut self) -> Option<ForeignSelection<T>> {
        match self.take_selection_update::<T>()? {
            SelectionUpdate::Set(offer) => Some(offer),
            SelectionUpdate::Clear => None,
        }
    }
}

pub struct ForeignSelection<T: SelectionType> {
    pub mime_types: Box<[String]>,
    inner: ForeignOffer<T::Offer>,
}

enum ForeignOffer<O> {
    Core(O),
    Control(ControlOffer),
}

#[allow(private_bounds)]
impl<T: SelectionType> ForeignSelection<T> {
    fn from_core(offer: T::Offer, mime_types: Box<[String]>) -> Self {
        Self {
            mime_types,
            inner: ForeignOffer::Core(offer),
        }
    }

    fn from_control(offer: ControlOffer) -> Self {
        Self {
            mime_types: offer.mime_types(),
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
            ForeignOffer::Control(offer) => receive_control_offer(&offer.0, mime_type),
        }
        .unwrap();
        state.queue.flush().unwrap();
        let mut data = Vec::new();
        pipe.read_to_end(&mut data).unwrap();
        data
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

    fn receive_offer(offer: &Self::Offer, mime_type: String) -> std::io::Result<ReadPipe> {
        offer.receive(mime_type)
    }

    fn get_mimes(offer: &Self::Offer) -> Box<[String]> {
        offer.with_mime_types(|mimes| mimes.into())
    }
}
