mod data_device;
pub mod xdg_activation;

use crate::server::{ObjectEvent, ObjectKey};
use std::os::unix::net::UnixStream;
use std::sync::{mpsc, Mutex, OnceLock};
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_callback::WlCallback, wl_compositor::WlCompositor,
    wl_keyboard::WlKeyboard, wl_output::WlOutput, wl_pointer::WlPointer, wl_region::WlRegion,
    wl_registry::WlRegistry, wl_seat::WlSeat, wl_shm::WlShm, wl_shm_pool::WlShmPool,
    wl_surface::WlSurface, wl_touch::WlTouch,
};
use wayland_client::{
    delegate_noop, event_created_child,
    globals::{registry_queue_init, Global, GlobalList, GlobalListContents},
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols::wp::relative_pointer::zv1::client::{
    zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
    zwp_relative_pointer_v1::ZwpRelativePointerV1,
};
use wayland_protocols::xdg::decoration::zv1::client::zxdg_decoration_manager_v1::ZxdgDecorationManagerV1;
use wayland_protocols::xdg::decoration::zv1::client::zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1;
use wayland_protocols::{
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

#[derive(Default)]
pub struct Globals {
    events: Vec<(ObjectKey, ObjectEvent)>,
    queued_events: Vec<mpsc::Receiver<(ObjectKey, ObjectEvent)>>,
    pub new_globals: Vec<Global>,
    pub selection: Option<wayland_client::protocol::wl_data_device::WlDataDevice>,
    pub selection_requests: Vec<(
        String,
        smithay_client_toolkit::data_device_manager::WritePipe,
    )>,
    pub cancelled: bool,
    pub pending_activations: Vec<(xcb::x::Window, String)>,
}

pub type ClientQueueHandle = QueueHandle<Globals>;

pub struct ClientState {
    _connection: Connection,
    pub queue: EventQueue<Globals>,
    pub qh: ClientQueueHandle,
    pub globals: Globals,
    pub global_list: GlobalList,
}

impl ClientState {
    pub fn new(server_connection: Option<UnixStream>) -> Self {
        let connection = if let Some(stream) = server_connection {
            Connection::from_socket(stream)
        } else {
            Connection::connect_to_env()
        }
        .unwrap();
        let (global_list, queue) = registry_queue_init::<Globals>(&connection).unwrap();
        let globals = Globals::default();
        let qh = queue.handle();

        Self {
            _connection: connection,
            queue,
            qh,
            globals,
            global_list,
        }
    }

    pub fn read_events(&mut self) -> Vec<(ObjectKey, ObjectEvent)> {
        let mut events = std::mem::take(&mut self.globals.events);
        self.globals.queued_events.retain(|rx| {
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

delegate_noop!(Globals: WlCompositor);
delegate_noop!(Globals: WlRegion);
delegate_noop!(Globals: ignore WlShm);
delegate_noop!(Globals: ignore ZwpLinuxDmabufV1);
delegate_noop!(Globals: ZwpRelativePointerManagerV1);
delegate_noop!(Globals: ignore dmabuf::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);
delegate_noop!(Globals: XdgPositioner);
delegate_noop!(Globals: WlShmPool);
delegate_noop!(Globals: WpViewporter);
delegate_noop!(Globals: WpViewport);
delegate_noop!(Globals: ZxdgOutputManagerV1);
delegate_noop!(Globals: ZwpPointerConstraintsV1);
delegate_noop!(Globals: ZwpTabletManagerV2);
delegate_noop!(Globals: XdgActivationV1);
delegate_noop!(Globals: ZxdgDecorationManagerV1);
delegate_noop!(Globals: WpFractionalScaleManagerV1);
delegate_noop!(Globals: ignore ZxdgToplevelDecorationV1);

impl Dispatch<WlRegistry, GlobalListContents> for Globals {
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

impl Dispatch<XdgWmBase, ()> for Globals {
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

impl Dispatch<WlCallback, server::wl_callback::WlCallback> for Globals {
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

macro_rules! push_events {
    ($type:ident) => {
        impl Dispatch<$type, ObjectKey> for Globals {
            fn event(
                state: &mut Self,
                _: &$type,
                event: <$type as Proxy>::Event,
                key: &ObjectKey,
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

pub(crate) struct LateInitObjectKey<P: Proxy> {
    key: OnceLock<ObjectKey>,
    queued_events: Mutex<Vec<P::Event>>,
    sender: Mutex<Option<mpsc::Sender<(ObjectKey, ObjectEvent)>>>,
}

impl<P: Proxy> LateInitObjectKey<P>
where
    P::Event: Into<ObjectEvent>,
{
    pub fn init(&self, key: ObjectKey) {
        self.key.set(key).expect("Object key should not be set");
        if let Some(sender) = self.sender.lock().unwrap().take() {
            for event in self.queued_events.lock().unwrap().drain(..) {
                sender.send((key, event.into())).unwrap();
            }
        }
    }

    fn new() -> Self {
        Self {
            key: OnceLock::new(),
            queued_events: Mutex::default(),
            sender: Mutex::default(),
        }
    }

    fn push_or_queue_event(&self, state: &mut Globals, event: P::Event) {
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
    type Target = ObjectKey;

    #[track_caller]
    fn deref(&self) -> &Self::Target {
        self.key.get().expect("object key has not been initialized")
    }
}

impl Dispatch<ZwpTabletSeatV2, ObjectKey> for Globals {
    fn event(
        state: &mut Self,
        _: &ZwpTabletSeatV2,
        event: <ZwpTabletSeatV2 as Proxy>::Event,
        key: &ObjectKey,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.events.push((*key, event.into()));
    }

    event_created_child!(Globals, ZwpTabletSeatV2, [
        EVT_TABLET_ADDED_OPCODE => (ZwpTabletV2, LateInitObjectKey::new()),
        EVT_PAD_ADDED_OPCODE => (ZwpTabletPadV2, LateInitObjectKey::new()),
        EVT_TOOL_ADDED_OPCODE => (ZwpTabletToolV2, LateInitObjectKey::new())
    ]);
}

macro_rules! push_or_queue_events {
    ($type:ty) => {
        impl Dispatch<$type, LateInitObjectKey<$type>> for Globals {
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

impl Dispatch<ZwpTabletPadV2, LateInitObjectKey<ZwpTabletPadV2>> for Globals {
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

    event_created_child!(Globals, ZwpTabletPadV2, [
        EVT_GROUP_OPCODE => (ZwpTabletPadGroupV2, LateInitObjectKey::new())
    ]);
}

impl Dispatch<ZwpTabletPadGroupV2, LateInitObjectKey<ZwpTabletPadGroupV2>> for Globals {
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

    event_created_child!(Globals, ZwpTabletPadGroupV2, [
        EVT_RING_OPCODE => (ZwpTabletPadRingV2, LateInitObjectKey::new()),
        EVT_STRIP_OPCODE => (ZwpTabletPadStripV2, LateInitObjectKey::new())
    ]);
}
