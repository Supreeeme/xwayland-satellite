use super::clientside::MyWorld;
use super::{InnerServerState, ServerState};
use crate::{X11Selection, XConnection};
use log::info;
use rustix::pipe::{PipeFlags, pipe_with};
use std::io::Read;
use std::marker::PhantomData;
use std::os::fd::{AsFd, OwnedFd};
use std::rc::{Rc, Weak};
use std::sync::Mutex;
use wayland_client::Proxy;
use wayland_client::QueueHandle;
use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_seat::WlSeat;

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

pub(crate) fn offer_mimes(offer: &ExtDataControlOfferV1) -> Vec<String> {
    let data: &Mutex<Vec<String>> = offer.data().unwrap();
    data.lock().unwrap().clone()
}

pub(super) struct SelectionStates<S: X11Selection> {
    manager: Option<ExtDataControlManagerV1>,
    device: Option<ExtDataControlDeviceV1>,
    clipboard: SlotState<S>,
    primary: SlotState<S>,
}

struct SlotState<S: X11Selection> {
    source: Option<SlotSource<S>>,
}

impl<S: X11Selection> Default for SlotState<S> {
    fn default() -> Self {
        Self { source: None }
    }
}

enum SlotSource<S: X11Selection> {
    X11 {
        inner: ExtDataControlSourceV1,
        data: Weak<S>,
    },
    Foreign(AnyForeignSelection),
}

// Not a `Drop` impl: `new_selection` partial-moves the inner offer out of `AnyForeignSelection`
// into `ForeignSelection`, which has its own Drop. Anywhere we drop an `AnyForeignSelection`
// without transferring its offer first goes through `destroy_slot` instead.
struct AnyForeignSelection {
    mime_types: Box<[String]>,
    inner: ExtDataControlOfferV1,
}

fn destroy_slot<S: X11Selection>(s: Option<SlotSource<S>>) {
    match s {
        Some(SlotSource::X11 { inner, .. }) => inner.destroy(),
        Some(SlotSource::Foreign(f)) => f.inner.destroy(),
        None => {}
    }
}

impl<S: X11Selection> SelectionStates<S> {
    pub fn new(global_list: &GlobalList, qh: &QueueHandle<MyWorld>) -> Self {
        let manager = global_list
            .bind::<ExtDataControlManagerV1, _, _>(qh, 1..=1, ())
            .ok();
        if manager.is_none() {
            info!(
                "compositor does not expose ext-data-control-v1; X11 selection bridge disabled"
            );
        }
        Self {
            manager,
            device: None,
            clipboard: SlotState::default(),
            primary: SlotState::default(),
        }
    }

    pub fn seat_created(&mut self, qh: &QueueHandle<MyWorld>, seat: &WlSeat) {
        let Some(m) = self.manager.as_ref() else {
            return;
        };
        let device = m.get_data_device(seat, qh, ());
        // Push any pending X11 selections to the new device. set_selection_source may have
        // been called before the wl_seat global arrived (during startup).
        if let Some(SlotSource::X11 { inner, .. }) = &self.clipboard.source {
            device.set_selection(Some(inner));
        }
        if let Some(SlotSource::X11 { inner, .. }) = &self.primary.source {
            device.set_primary_selection(Some(inner));
        }
        self.device = Some(device);
    }

    fn slot_mut(&mut self, kind: SourceKind) -> &mut SlotState<S> {
        match kind {
            SourceKind::Clipboard => &mut self.clipboard,
            SourceKind::Primary => &mut self.primary,
        }
    }
}

impl<S: X11Selection> InnerServerState<S> {
    pub(super) fn handle_selection_events(&mut self) {
        self.handle_impl(SourceKind::Clipboard);
        self.handle_impl(SourceKind::Primary);
    }

    fn handle_impl(&mut self, kind: SourceKind) {
        if self.selection_states.manager.is_none() {
            return;
        }

        let events = match kind {
            SourceKind::Clipboard => &mut self.world.clipboard,
            SourceKind::Primary => &mut self.world.primary,
        };

        let requests = std::mem::take(&mut events.requests);
        let cancelled = std::mem::replace(&mut events.cancelled, false);
        let offer_taken = events.offer.take();
        let slot = self.selection_states.slot_mut(kind);

        for (mime_type, fd) in requests {
            if let Some(SlotSource::X11 { data, .. }) = slot.source.as_ref() {
                if let Some(data) = data.upgrade() {
                    data.write_to(&mime_type, fd);
                }
            }
        }

        if cancelled {
            destroy_slot(slot.source.take());
        }

        if let Some(offer) = offer_taken {
            match slot.source.as_ref() {
                Some(SlotSource::X11 { .. }) => {
                    // Our X11-owned source is still active; the compositor will deliver a
                    // Cancelled to it shortly. Drop this incoming foreign offer.
                    offer.destroy();
                }
                _ => {
                    // None or stale Foreign: replace with the new offer.
                    destroy_slot(slot.source.take());
                    let mime_types = offer_mimes(&offer);
                    slot.source = Some(SlotSource::Foreign(AnyForeignSelection {
                        mime_types: mime_types.into_boxed_slice(),
                        inner: offer,
                    }));
                }
            }
        }
    }

    pub(crate) fn set_selection_source<T: SelectionType>(&mut self, selection: &Rc<S>) {
        let Some(manager) = self.selection_states.manager.as_ref() else {
            return;
        };
        let kind = T::KIND;
        let src = manager.create_data_source(&self.qh, kind);
        for mime in selection.mime_types() {
            src.offer(mime.to_owned());
        }
        if let Some(device) = self.selection_states.device.as_ref() {
            T::set_on_device(device, Some(&src));
        }
        let slot = self.selection_states.slot_mut(kind);
        destroy_slot(slot.source.take());
        slot.source = Some(SlotSource::X11 {
            inner: src,
            data: Rc::downgrade(selection),
        });
    }

    pub(crate) fn new_selection<T: SelectionType>(&mut self) -> Option<ForeignSelection<T>> {
        let slot = self.selection_states.slot_mut(T::KIND);
        match slot.source.take()? {
            SlotSource::Foreign(f) => Some(ForeignSelection {
                mime_types: f.mime_types,
                inner: f.inner,
                _kind: PhantomData,
            }),
            other => {
                slot.source = Some(other);
                None
            }
        }
    }
}

pub struct ForeignSelection<T: SelectionType> {
    pub mime_types: Box<[String]>,
    inner: ExtDataControlOfferV1,
    _kind: PhantomData<fn() -> T>,
}

impl<T: SelectionType> ForeignSelection<T> {
    pub(crate) fn receive(
        &self,
        mime_type: String,
        state: &ServerState<impl XConnection>,
    ) -> Vec<u8> {
        let (read, write) = pipe_with(PipeFlags::CLOEXEC).unwrap();
        self.inner.receive(mime_type, OwnedFd::from(write).as_fd());
        state.queue.flush().unwrap();
        let mut data = Vec::new();
        let mut read = std::fs::File::from(OwnedFd::from(read));
        read.read_to_end(&mut data).unwrap();
        data
    }
}

impl<T: SelectionType> Drop for ForeignSelection<T> {
    fn drop(&mut self) {
        self.inner.destroy();
    }
}

#[allow(private_bounds, private_interfaces)]
pub trait SelectionType: sealed::Sealed {
    const KIND: SourceKind;
    fn set_on_device(device: &ExtDataControlDeviceV1, source: Option<&ExtDataControlSourceV1>);
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::Clipboard {}
    impl Sealed for super::Primary {}
}

pub enum Clipboard {}
pub enum Primary {}

#[allow(private_interfaces)]
impl SelectionType for Clipboard {
    const KIND: SourceKind = SourceKind::Clipboard;

    fn set_on_device(device: &ExtDataControlDeviceV1, source: Option<&ExtDataControlSourceV1>) {
        device.set_selection(source);
    }
}

#[allow(private_interfaces)]
impl SelectionType for Primary {
    const KIND: SourceKind = SourceKind::Primary;

    fn set_on_device(device: &ExtDataControlDeviceV1, source: Option<&ExtDataControlSourceV1>) {
        device.set_primary_selection(source);
    }
}
