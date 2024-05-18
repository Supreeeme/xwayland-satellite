use crate::server::{ObjectEvent, ObjectKey};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_callback::WlCallback, wl_compositor::WlCompositor,
    wl_keyboard::WlKeyboard, wl_output::WlOutput, wl_pointer::WlPointer, wl_registry::WlRegistry,
    wl_seat::WlSeat, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_surface::WlSurface,
    wl_touch::WlTouch,
};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::relative_pointer::zv1::client::{
    zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
    zwp_relative_pointer_v1::ZwpRelativePointerV1,
};
use wayland_protocols::{
    wp::{
        linux_dmabuf::zv1::client::{
            self as dmabuf,
            zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1 as DmabufFeedback,
            zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        },
        viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
    },
    xdg::{
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

#[derive(Debug)]
pub struct GlobalData {
    pub name: u32,
    pub interface: String,
    pub version: u32,
}

#[derive(Default)]
pub struct Globals {
    pub(crate) events: Vec<(ObjectKey, ObjectEvent)>,
    pub new_globals: Vec<GlobalData>,
}

pub type ClientQueueHandle = QueueHandle<Globals>;

pub struct ClientShmPool {
    pub pool: WlShmPool,
    pub fd: OwnedFd,
}

pub struct ClientState {
    pub connection: Connection,
    pub queue: EventQueue<Globals>,
    pub qh: ClientQueueHandle,
    pub globals: Globals,
    pub registry: WlRegistry,
}

impl ClientState {
    pub fn new(server_connection: Option<UnixStream>) -> Self {
        let connection = if let Some(stream) = server_connection {
            Connection::from_socket(stream)
        } else {
            Connection::connect_to_env()
        }
        .unwrap();
        let mut queue = connection.new_event_queue::<Globals>();
        let qh = queue.handle();
        let mut globals = Globals::default();

        let registry = connection.display().get_registry(&qh, ());
        // Get initial globals
        queue.roundtrip(&mut globals).unwrap();

        Self {
            connection,
            queue,
            qh,
            globals,
            registry,
        }
    }
}

pub type Event<T> = <T as Proxy>::Event;

delegate_noop!(Globals: WlCompositor);
delegate_noop!(Globals: ignore WlShm);
delegate_noop!(Globals: ignore ZwpLinuxDmabufV1);
delegate_noop!(Globals: ZwpRelativePointerManagerV1);
delegate_noop!(Globals: ignore dmabuf::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);
delegate_noop!(Globals: XdgPositioner);
delegate_noop!(Globals: WlShmPool);
delegate_noop!(Globals: WpViewporter);
delegate_noop!(Globals: WpViewport);
delegate_noop!(Globals: ZxdgOutputManagerV1);

impl Dispatch<WlRegistry, ()> for Globals {
    fn event(
        state: &mut Self,
        _: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        _: &(),
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        if let Event::<WlRegistry>::Global {
            name,
            interface,
            version,
        } = event
        {
            state.new_globals.push(GlobalData {
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

impl Dispatch<WlOutput, mpsc::Sender<Event<WlOutput>>> for Globals {
    fn event(
        _: &mut Self,
        _: &WlOutput,
        event: <WlOutput as Proxy>::Event,
        data: &mpsc::Sender<Event<WlOutput>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let _ = data.send(event);
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
