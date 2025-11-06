use super::decoration::DecorationMarker;

use super::ObjectEvent;
use hecs::{Entity, World};
use smithay_client_toolkit::{
    activation::{ActivationHandler, RequestData, RequestDataExt},
    data_device_manager::{
        data_device::{DataDeviceData, DataDeviceHandler},
        data_offer::{DataOfferHandler, SelectionOffer},
        data_source::DataSourceHandler,
    },
    delegate_activation, delegate_data_device, delegate_primary_selection,
    primary_selection::{
        device::{PrimarySelectionDeviceData, PrimarySelectionDeviceHandler},
        offer::PrimarySelectionOffer,
        selection::PrimarySelectionSourceHandler,
    },
};
use std::sync::{mpsc, Mutex, OnceLock};
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_callback::WlCallback, wl_compositor::WlCompositor,
    wl_keyboard::WlKeyboard, wl_output::WlOutput, wl_pointer::WlPointer, wl_region::WlRegion,
    wl_registry::WlRegistry, wl_seat::WlSeat, wl_shm::WlShm, wl_shm_pool::WlShmPool,
    wl_subcompositor::WlSubcompositor, wl_subsurface::WlSubsurface, wl_surface::WlSurface,
    wl_touch::WlTouch,
};
use wayland_client::{
    delegate_noop, event_created_child,
    globals::{Global, GlobalList, GlobalListContents},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::{
    wp::relative_pointer::zv1::client::{
        zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
        zwp_relative_pointer_v1::ZwpRelativePointerV1,
    },
    wp::{
        fractional_scale::v1::client::{
            wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
            wp_fractional_scale_v1::WpFractionalScaleV1,
        },
        linux_dmabuf::zv1::client::{
            self as dmabuf,
            zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1 as DmabufFeedback,
            zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        },
        pointer_constraints::zv1::client::{
            zwp_confined_pointer_v1::ZwpConfinedPointerV1,
            zwp_locked_pointer_v1::ZwpLockedPointerV1,
            zwp_pointer_constraints_v1::ZwpPointerConstraintsV1,
        },
        primary_selection::zv1::client::{
            zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1,
            zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1,
            zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
        },
        tablet::zv2::client::{
            zwp_tablet_manager_v2::ZwpTabletManagerV2,
            zwp_tablet_pad_group_v2::{ZwpTabletPadGroupV2, EVT_RING_OPCODE, EVT_STRIP_OPCODE},
            zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2,
            zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2,
            zwp_tablet_pad_v2::{ZwpTabletPadV2, EVT_GROUP_OPCODE},
            zwp_tablet_seat_v2::{
                ZwpTabletSeatV2, EVT_PAD_ADDED_OPCODE, EVT_TABLET_ADDED_OPCODE,
                EVT_TOOL_ADDED_OPCODE,
            },
            zwp_tablet_tool_v2::ZwpTabletToolV2,
            zwp_tablet_v2::ZwpTabletV2,
        },
        viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
    },
    xdg::decoration::zv1::client::zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
    xdg::decoration::zv1::client::zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
    xdg::{
        activation::v1::client::xdg_activation_v1::XdgActivationV1,
        shell::client::{
            xdg_popup::XdgPopup, xdg_positioner::XdgPositioner, xdg_surface::XdgSurface,
            xdg_toplevel::XdgToplevel, xdg_wm_base::XdgWmBase,
        },
        xdg_output::zv1::client::{
            zxdg_output_manager_v1::ZxdgOutputManagerV1, zxdg_output_v1::ZxdgOutputV1 as XdgOutput,
        },
    },
};
use wayland_server::protocol as server;
use wl_drm::client::wl_drm::WlDrm;
use xcb::x;

pub(super) struct SelectionEvents<T> {
    pub offer: Option<T>,
    pub requests: Vec<(
        String,
        smithay_client_toolkit::data_device_manager::WritePipe,
    )>,
    pub cancelled: bool,
}

impl<T> Default for SelectionEvents<T> {
    fn default() -> Self {
        Self {
            offer: None,
            requests: Default::default(),
            cancelled: false,
        }
    }
}

pub(super) struct MyWorld {
    pub world: World,
    pub global_list: GlobalList,
    pub new_globals: Vec<Global>,
    events: Vec<(Entity, ObjectEvent)>,
    queued_events: Vec<mpsc::Receiver<(Entity, ObjectEvent)>>,
    pub clipboard: SelectionEvents<SelectionOffer>,
    pub primary: SelectionEvents<PrimarySelectionOffer>,
    pub pending_activations: Vec<(xcb::x::Window, String)>,
}

impl MyWorld {
    pub fn new(global_list: GlobalList) -> Self {
        Self {
            world: World::new(),
            global_list,
            new_globals: Vec::new(),
            events: Vec::new(),
            queued_events: Vec::new(),
            clipboard: Default::default(),
            primary: Default::default(),
            pending_activations: Vec::new(),
        }
    }
}

impl std::ops::Deref for MyWorld {
    type Target = World;
    fn deref(&self) -> &Self::Target {
        &self.world
    }
}

impl std::ops::DerefMut for MyWorld {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.world
    }
}

impl MyWorld {
    pub(crate) fn read_events(&mut self) -> Vec<(Entity, ObjectEvent)> {
        let mut events = std::mem::take(&mut self.events);
        self.queued_events.retain(|rx| {
            match rx.try_recv() {
                Ok(event) => {
                    events.push(event);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return true,

                Err(_) => unreachable!(),
            }

            events.extend(rx.try_iter());
            false
        });
        events
    }
}

pub type Event<T> = <T as Proxy>::Event;

delegate_noop!(MyWorld: WlCompositor);
delegate_noop!(MyWorld: WlSubcompositor);
delegate_noop!(MyWorld: WlRegion);
delegate_noop!(MyWorld: ignore WlShm);
delegate_noop!(MyWorld: ignore ZwpLinuxDmabufV1);
delegate_noop!(MyWorld: ZwpRelativePointerManagerV1);
delegate_noop!(MyWorld: ignore dmabuf::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);
delegate_noop!(MyWorld: XdgPositioner);
delegate_noop!(MyWorld: WlShmPool);
delegate_noop!(MyWorld: WpViewporter);
delegate_noop!(MyWorld: WpViewport);
delegate_noop!(MyWorld: ZxdgOutputManagerV1);
delegate_noop!(MyWorld: ZwpPointerConstraintsV1);
delegate_noop!(MyWorld: ZwpTabletManagerV2);
delegate_noop!(MyWorld: XdgActivationV1);
delegate_noop!(MyWorld: ZxdgDecorationManagerV1);
delegate_noop!(MyWorld: WpFractionalScaleManagerV1);
delegate_noop!(MyWorld: ZwpPrimarySelectionDeviceManagerV1);
delegate_noop!(MyWorld: WlSubsurface);

impl Dispatch<WlRegistry, GlobalListContents> for MyWorld {
    fn event(
        state: &mut Self,
        _: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        if let Event::<WlRegistry>::Global {
            name,
            interface,
            version,
        } = event
        {
            state.new_globals.push(Global {
                name,
                interface,
                version,
            });
        };
    }
}

impl Dispatch<XdgWmBase, ()> for MyWorld {
    fn event(
        _: &mut Self,
        base: &XdgWmBase,
        event: <XdgWmBase as Proxy>::Event,
        _: &(),
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        if let Event::<XdgWmBase>::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<WlCallback, server::wl_callback::WlCallback> for MyWorld {
    fn event(
        _: &mut Self,
        _: &WlCallback,
        event: <WlCallback as Proxy>::Event,
        s_callback: &server::wl_callback::WlCallback,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Event::<WlCallback>::Done { callback_data } = event {
            s_callback.done(callback_data);
        }
    }
}

impl Dispatch<WlSurface, DecorationMarker> for MyWorld {
    fn event(
        _: &mut Self,
        _: &WlSurface,
        _: <WlSurface as Proxy>::Event,
        _: &DecorationMarker,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

macro_rules! push_events {
    ($type:ident) => {
        impl Dispatch<$type, Entity> for MyWorld {
            fn event(
                state: &mut Self,
                _: &$type,
                event: <$type as Proxy>::Event,
                key: &Entity,
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                state.events.push((*key, event.into()));
            }
        }
    };
}

push_events!(WlSurface);
push_events!(WlBuffer);
push_events!(XdgSurface);
push_events!(XdgToplevel);
push_events!(XdgPopup);
push_events!(WlSeat);
push_events!(WlPointer);
push_events!(WlOutput);
push_events!(WlKeyboard);
push_events!(ZwpRelativePointerV1);
push_events!(WlDrm);
push_events!(DmabufFeedback);
push_events!(XdgOutput);
push_events!(WlTouch);
push_events!(ZwpConfinedPointerV1);
push_events!(ZwpLockedPointerV1);
push_events!(WpFractionalScaleV1);
push_events!(ZxdgToplevelDecorationV1);

pub(crate) struct LateInitObjectKey<P: Proxy> {
    key: OnceLock<Entity>,
    queued_events: Mutex<Vec<P::Event>>,
    sender: Mutex<Option<mpsc::Sender<(Entity, ObjectEvent)>>>,
}

impl<P: Proxy> LateInitObjectKey<P>
where
    P::Event: Into<ObjectEvent>,
{
    pub fn init(&self, key: Entity) {
        self.key.set(key).expect("Object key should not be set");
        if let Some(sender) = self.sender.lock().unwrap().take() {
            for event in self.queued_events.lock().unwrap().drain(..) {
                sender.send((key, event.into())).unwrap();
            }
        }
    }

    pub fn get(&self) -> Entity {
        self.key.get().copied().expect("Object key is not set")
    }

    fn new() -> Self {
        Self {
            key: OnceLock::new(),
            queued_events: Mutex::default(),
            sender: Mutex::default(),
        }
    }

    fn push_or_queue_event(&self, state: &mut MyWorld, event: P::Event) {
        if let Some(key) = self.key.get().copied() {
            state.events.push((key, event.into()));
        } else {
            let mut sender = self.sender.lock().unwrap();
            if sender.is_none() {
                let (send, recv) = mpsc::channel();
                *sender = Some(send);
                state.queued_events.push(recv);
            }
            self.queued_events.lock().unwrap().push(event);
        }
    }
}

impl<P: Proxy> std::ops::Deref for LateInitObjectKey<P> {
    type Target = Entity;

    #[track_caller]
    fn deref(&self) -> &Self::Target {
        self.key.get().expect("object key has not been initialized")
    }
}

impl Dispatch<ZwpTabletSeatV2, Entity> for MyWorld {
    fn event(
        state: &mut Self,
        _: &ZwpTabletSeatV2,
        event: <ZwpTabletSeatV2 as Proxy>::Event,
        key: &Entity,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.events.push((*key, event.into()));
    }

    event_created_child!(MyWorld, ZwpTabletSeatV2, [
        EVT_TABLET_ADDED_OPCODE => (ZwpTabletV2, LateInitObjectKey::new()),
        EVT_PAD_ADDED_OPCODE => (ZwpTabletPadV2, LateInitObjectKey::new()),
        EVT_TOOL_ADDED_OPCODE => (ZwpTabletToolV2, LateInitObjectKey::new())
    ]);
}

macro_rules! push_or_queue_events {
    ($type:ty) => {
        impl Dispatch<$type, LateInitObjectKey<$type>> for MyWorld {
            fn event(
                state: &mut Self,
                _: &$type,
                event: <$type as Proxy>::Event,
                key: &LateInitObjectKey<$type>,
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                key.push_or_queue_event(state, event);
            }
        }
    };
}

push_or_queue_events!(ZwpTabletV2);
push_or_queue_events!(ZwpTabletToolV2);
push_or_queue_events!(ZwpTabletPadRingV2);
push_or_queue_events!(ZwpTabletPadStripV2);

impl Dispatch<ZwpTabletPadV2, LateInitObjectKey<ZwpTabletPadV2>> for MyWorld {
    fn event(
        state: &mut Self,
        _: &ZwpTabletPadV2,
        event: <ZwpTabletPadV2 as Proxy>::Event,
        key: &LateInitObjectKey<ZwpTabletPadV2>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        key.push_or_queue_event(state, event);
    }

    event_created_child!(MyWorld, ZwpTabletPadV2, [
        EVT_GROUP_OPCODE => (ZwpTabletPadGroupV2, LateInitObjectKey::new())
    ]);
}

impl Dispatch<ZwpTabletPadGroupV2, LateInitObjectKey<ZwpTabletPadGroupV2>> for MyWorld {
    fn event(
        state: &mut Self,
        _: &ZwpTabletPadGroupV2,
        event: <ZwpTabletPadGroupV2 as Proxy>::Event,
        key: &LateInitObjectKey<ZwpTabletPadGroupV2>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        key.push_or_queue_event(state, event);
    }

    event_created_child!(MyWorld, ZwpTabletPadGroupV2, [
        EVT_RING_OPCODE => (ZwpTabletPadRingV2, LateInitObjectKey::new()),
        EVT_STRIP_OPCODE => (ZwpTabletPadStripV2, LateInitObjectKey::new())
    ]);
}

delegate_data_device!(MyWorld);

impl DataDeviceHandler for MyWorld {
    fn selection(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        data_device: &wayland_client::protocol::wl_data_device::WlDataDevice,
    ) {
        let data: &DataDeviceData = data_device.data().unwrap();
        self.clipboard.offer = data.selection_offer();
    }

    fn drop_performed(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &wayland_client::protocol::wl_data_device::WlDataDevice,
    ) {
    }

    fn motion(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &wayland_client::protocol::wl_data_device::WlDataDevice,
        _: f64,
        _: f64,
    ) {
    }

    fn leave(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &wayland_client::protocol::wl_data_device::WlDataDevice,
    ) {
    }

    fn enter(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &wayland_client::protocol::wl_data_device::WlDataDevice,
        _: f64,
        _: f64,
        _: &wayland_client::protocol::wl_surface::WlSurface,
    ) {
    }
}

impl DataSourceHandler for MyWorld {
    fn send_request(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &wayland_client::protocol::wl_data_source::WlDataSource,
        mime: String,
        fd: smithay_client_toolkit::data_device_manager::WritePipe,
    ) {
        self.clipboard.requests.push((mime, fd));
    }

    fn cancelled(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &wayland_client::protocol::wl_data_source::WlDataSource,
    ) {
        self.clipboard.cancelled = true;
    }

    fn action(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &wayland_client::protocol::wl_data_source::WlDataSource,
        _: wayland_client::protocol::wl_data_device_manager::DndAction,
    ) {
    }

    fn dnd_finished(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &wayland_client::protocol::wl_data_source::WlDataSource,
    ) {
    }

    fn dnd_dropped(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &wayland_client::protocol::wl_data_source::WlDataSource,
    ) {
    }

    fn accept_mime(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &wayland_client::protocol::wl_data_source::WlDataSource,
        _: Option<String>,
    ) {
    }
}

impl DataOfferHandler for MyWorld {
    fn selected_action(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &mut smithay_client_toolkit::data_device_manager::data_offer::DragOffer,
        _: wayland_client::protocol::wl_data_device_manager::DndAction,
    ) {
    }

    fn source_actions(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &mut smithay_client_toolkit::data_device_manager::data_offer::DragOffer,
        _: wayland_client::protocol::wl_data_device_manager::DndAction,
    ) {
    }
}

delegate_activation!(MyWorld, ActivationData);

pub struct ActivationData {
    window: x::Window,
    data: RequestData,
}

impl ActivationData {
    pub fn new(window: x::Window, data: RequestData) -> Self {
        Self { window, data }
    }
}

impl RequestDataExt for ActivationData {
    fn app_id(&self) -> Option<&str> {
        self.data.app_id()
    }

    fn seat_and_serial(&self) -> Option<(&wayland_client::protocol::wl_seat::WlSeat, u32)> {
        self.data.seat_and_serial()
    }

    fn surface(&self) -> Option<&wayland_client::protocol::wl_surface::WlSurface> {
        self.data.surface()
    }
}

impl ActivationHandler for MyWorld {
    type RequestData = ActivationData;

    fn new_token(&mut self, token: String, data: &Self::RequestData) {
        self.pending_activations.push((data.window, token));
    }
}

delegate_primary_selection!(MyWorld);

impl PrimarySelectionDeviceHandler for MyWorld {
    fn selection(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        primary_selection_device: &ZwpPrimarySelectionDeviceV1,
    ) {
        let Some(data) = primary_selection_device.data::<PrimarySelectionDeviceData>() else {
            return;
        };

        self.primary.offer = data.selection_offer();
    }
}

impl PrimarySelectionSourceHandler for MyWorld {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpPrimarySelectionSourceV1,
        mime: String,
        write_pipe: smithay_client_toolkit::data_device_manager::WritePipe,
    ) {
        self.primary.requests.push((mime, write_pipe));
    }

    fn cancelled(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpPrimarySelectionSourceV1,
    ) {
        self.primary.cancelled = true;
    }
}
