use super::{ServerState, WindowDims};
use crate::xstate::{SetState, WmName};
use paste::paste;
use rustix::event::{poll, PollFd, PollFlags};
use std::collections::HashMap;
use std::io::Write;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use wayland_client::{
    backend::{protocol::Message, Backend, ObjectData, ObjectId, WaylandError},
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_display::WlDisplay,
        wl_keyboard::WlKeyboard,
        wl_output::WlOutput,
        wl_pointer::WlPointer,
        wl_registry::WlRegistry,
        wl_seat::{self, WlSeat},
        wl_shm::{Format, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
    Connection, Proxy, WEnum,
};

use wayland_protocols::{
    wp::{
        linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        pointer_constraints::zv1::client::zwp_pointer_constraints_v1::ZwpPointerConstraintsV1,
        relative_pointer::zv1::client::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
        tablet::zv2::client::{
            zwp_tablet_manager_v2::{self, ZwpTabletManagerV2},
            zwp_tablet_pad_group_v2::{
                self, ZwpTabletPadGroupV2, EVT_RING_OPCODE, EVT_STRIP_OPCODE,
            },
            zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2,
            zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2,
            zwp_tablet_pad_v2::{self, ZwpTabletPadV2, EVT_GROUP_OPCODE},
            zwp_tablet_seat_v2::{
                self, ZwpTabletSeatV2, EVT_PAD_ADDED_OPCODE, EVT_TABLET_ADDED_OPCODE,
                EVT_TOOL_ADDED_OPCODE,
            },
            zwp_tablet_tool_v2::{self, ZwpTabletToolV2},
            zwp_tablet_v2::{self, ZwpTabletV2},
        },
        viewporter::client::wp_viewporter::WpViewporter,
    },
    xdg::{
        shell::server::{xdg_positioner, xdg_toplevel},
        xdg_output::zv1::client::{
            zxdg_output_manager_v1::{self, ZxdgOutputManagerV1},
            zxdg_output_v1::ZxdgOutputV1,
        },
    },
    xwayland::shell::v1::client::{
        xwayland_shell_v1::XwaylandShellV1, xwayland_surface_v1::XwaylandSurfaceV1,
    },
};
use wayland_server::{protocol as s_proto, Display, Resource};
use wl_drm::client::wl_drm::WlDrm;
use xcb::x::{self, Window};

use xcb::XidNew;

macro_rules! with_optional {
    (
        $( #[$attr:meta] )?
        struct $name:ident$(<$($lifetimes:lifetime),+>)? {
            $(
                $field:ident: $type:ty
            ),+$(,)?
        }
    ) => {
        $( #[$attr] )?
        struct $name$(<$($lifetimes),+>)? {
            $(
                $field: $type
            ),+
        }

        paste! {
            #[derive(Default)]
            struct [< $name Optional >] {
                $(
                    $field: Option<$type>
                ),+
            }
        }

        paste! {
            impl From<[<$name Optional>]> for $name {
                fn from(opt: [<$name Optional>]) -> Self {
                    Self {
                        $(
                            $field: opt.$field.expect(concat!("uninitialized field ", stringify!($field)))
                        ),+
                    }
                }
            }
        }
    }
}

with_optional! {

struct Compositor {
    compositor: TestObject<WlCompositor>,
    shm: TestObject<WlShm>,
    shell: TestObject<XwaylandShellV1>,
    seat: TestObject<WlSeat>,
    tablet_man: TestObject<ZwpTabletManagerV2>
}

}

impl Compositor {
    fn create_surface(&self) -> (TestObject<WlBuffer>, TestObject<WlSurface>) {
        let fd = unsafe { BorrowedFd::borrow_raw(0) };
        let pool = TestObject::<WlShmPool>::from_request(
            &self.shm.obj,
            Req::<WlShm>::CreatePool { fd, size: 1024 },
        );
        let buffer = TestObject::<WlBuffer>::from_request(
            &pool.obj,
            Req::<WlShmPool>::CreateBuffer {
                offset: 0,
                width: 10,
                height: 10,
                stride: 1,
                format: WEnum::Value(Format::Xrgb8888A8),
            },
        );
        let surface = TestObject::<WlSurface>::from_request(
            &self.compositor.obj,
            Req::<WlCompositor>::CreateSurface {},
        );
        surface
            .send_request(Req::<WlSurface>::Attach {
                buffer: Some(buffer.obj.clone()),
                x: 0,
                y: 0,
            })
            .unwrap();

        (buffer, surface)
    }
}

#[derive(Debug, Default)]
struct WindowData {
    mapped: bool,
    fullscreen: bool,
    dims: WindowDims,
}
struct FakeXConnection {
    root: Window,
    focused_window: Option<Window>,
    windows: HashMap<Window, WindowData>,
}

impl FakeXConnection {
    #[track_caller]
    fn window(&mut self, window: Window) -> &mut WindowData {
        self.windows
            .get_mut(&window)
            .unwrap_or_else(|| panic!("Unknown window: {window:?}"))
    }
}

impl Default for FakeXConnection {
    fn default() -> Self {
        Self {
            root: unsafe { Window::new(9001) },
            focused_window: None,
            windows: HashMap::new(),
        }
    }
}

impl super::FromServerState<FakeXConnection> for () {
    fn create(_: &FakeServerState) -> Self {}
}

impl crate::X11Selection for Vec<testwl::PasteData> {
    fn mime_types(&self) -> Vec<&str> {
        self.iter().map(|data| data.mime_type.as_str()).collect()
    }

    fn write_to(
        &self,
        mime: &str,
        mut pipe: smithay_client_toolkit::data_device_manager::WritePipe,
    ) {
        println!("writing");
        let data = self
            .iter()
            .find(|data| data.mime_type == mime)
            .unwrap_or_else(|| panic!("Couldn't find mime type {mime}"));
        pipe.write_all(&data.data)
            .expect("Couldn't write paste data");
        println!("goodbye pipe {mime}");
    }
}

impl super::XConnection for FakeXConnection {
    type ExtraData = ();
    type X11Selection = Vec<testwl::PasteData>;
    fn root_window(&self) -> Window {
        self.root
    }

    #[track_caller]
    fn close_window(&mut self, window: Window, _: ()) {
        log::debug!("closing window {window:?}");
        self.window(window).mapped = false;
    }

    #[track_caller]
    fn set_fullscreen(&mut self, window: xcb::x::Window, fullscreen: bool, _: ()) {
        self.window(window).fullscreen = fullscreen;
    }

    #[track_caller]
    fn set_window_dims(&mut self, window: Window, state: super::PendingSurfaceState) {
        self.window(window).dims = WindowDims {
            x: state.x as _,
            y: state.y as _,
            width: state.width as _,
            height: state.height as _,
        };
    }

    #[track_caller]
    fn focus_window(&mut self, window: Window, _output_name: Option<String>, _: ()) {
        assert!(
            self.windows.contains_key(&window),
            "Unknown window: {window:?}"
        );
        self.focused_window = window.into();
    }

    fn raise_to_top(&mut self, window: Window) {
        assert!(
            self.windows.contains_key(&window),
            "Unknown window: {window:?}"
        );
    }
}

type FakeServerState = ServerState<FakeXConnection>;

struct TestFixture {
    testwl: testwl::Server,
    satellite: FakeServerState,
    /// Our connection to satellite - i.e., where Xwayland sends requests to
    xwls_connection: Arc<Connection>,
    /// Satellite's display - must dispatch this for our server state to advance
    xwls_display: Display<FakeServerState>,
    surface_serial: u64,
    registry: TestObject<WlRegistry>,
}

static INIT: std::sync::Once = std::sync::Once::new();

impl TestFixture {
    fn new() -> Self {
        INIT.call_once(|| {
            env_logger::builder()
                .is_test(true)
                .filter_level(log::LevelFilter::Trace)
                .init()
        });

        let (client_s, server_s) = UnixStream::pair().unwrap();
        let mut testwl = testwl::Server::new(true);
        let display = Display::<FakeServerState>::new().unwrap();
        testwl.connect(server_s);
        // Handle initial globals roundtrip setup requirement
        let thread = std::thread::spawn(move || {
            let mut pollfd = [PollFd::from_borrowed_fd(testwl.poll_fd(), PollFlags::IN)];
            if poll(&mut pollfd, 1000).unwrap() == 0 {
                panic!("Did not get events for testwl!");
            }
            testwl.dispatch();
            testwl
        });
        let mut satellite = FakeServerState::new(display.handle(), Some(client_s));
        let testwl = thread.join().unwrap();

        let (fake_client, xwls_server) = UnixStream::pair().unwrap();
        satellite.connect(xwls_server);

        satellite.set_x_connection(FakeXConnection::default());

        let xwls_connection = Connection::from_socket(fake_client).unwrap();
        let registry = TestObject::<WlRegistry>::from_request(
            &xwls_connection.display(),
            Req::<WlDisplay>::GetRegistry {},
        );

        let mut f = TestFixture {
            testwl,
            satellite,
            xwls_connection: xwls_connection.into(),
            xwls_display: display,
            surface_serial: 1,
            registry,
        };
        f.run();
        f
    }

    fn new_with_compositor() -> (Self, Compositor) {
        let mut f = Self::new();
        let compositor = f.compositor();
        (f, compositor)
    }

    fn connection(&self) -> &FakeXConnection {
        self.satellite.connection.as_ref().unwrap()
    }

    fn compositor(&mut self) -> Compositor {
        let mut ret = CompositorOptional::default();
        let events = std::mem::take(&mut *self.registry.data.events.lock().unwrap());
        assert!(!events.is_empty());

        fn bind<T: Proxy + Sync + Send + 'static>(
            registry: &TestObject<WlRegistry>,
            name: u32,
            version: u32,
        ) -> TestObject<T>
        where
            T::Event: Sync + Send + std::fmt::Debug,
        {
            TestObject::from_request(
                &registry.obj,
                Req::<WlRegistry>::Bind {
                    name,
                    id: (T::interface(), version),
                },
            )
        }

        for event in events {
            if let Ev::<WlRegistry>::Global {
                name,
                interface,
                version,
            } = event
            {
                macro_rules! bind {
                    ($field:ident) => {
                        ret.$field = Some(bind(&self.registry, name, version))
                    };
                }

                match interface {
                    x if x == WlCompositor::interface().name => bind!(compositor),
                    x if x == WlShm::interface().name => bind!(shm),
                    x if x == XwaylandShellV1::interface().name => bind!(shell),
                    x if x == WlSeat::interface().name => bind!(seat),
                    x if x == ZwpTabletManagerV2::interface().name => bind!(tablet_man),
                    _ => {}
                }
            }
        }

        // Activate keyboard for focus.
        TestObject::<WlKeyboard>::from_request(
            &ret.seat.as_ref().expect("Seat global missing").obj,
            wl_seat::Request::GetKeyboard {},
        );

        ret.into()
    }

    fn object_data<P>(&self, obj: &P) -> Arc<TestObjectData<P>>
    where
        P: Proxy + Send + Sync + 'static,
        P::Event: Send + Sync + std::fmt::Debug,
    {
        self.xwls_connection
            .get_object_data(obj.id())
            .unwrap()
            .downcast_arc::<TestObjectData<P>>()
            .unwrap()
    }

    /// Cascade our requests/events through satellite and testwl
    fn run(&mut self) {
        // Flush our requests to satellite
        self.xwls_connection.flush().unwrap();

        // Have satellite dispatch our requests
        self.xwls_display
            .dispatch_clients(&mut self.satellite)
            .unwrap();
        self.xwls_display.flush_clients().unwrap();

        // Dispatch any clientside requests
        self.satellite.run();

        // Have testwl dispatch the clientside requests
        self.testwl.dispatch();

        // Handle clientside events
        self.satellite.handle_clientside_events();

        self.testwl.dispatch();

        // Get our events
        let res = self.xwls_connection.prepare_read().unwrap().read();
        if res.is_err()
            && matches!(res, Err(WaylandError::Io(ref e)) if e.kind() != std::io::ErrorKind::WouldBlock)
        {
            panic!("Read failed: {res:?}")
        }
    }

    fn new_output(
        &mut self,
        x: i32,
        y: i32,
    ) -> (
        TestObject<WlOutput>,
        wayland_server::protocol::wl_output::WlOutput,
    ) {
        self.testwl.new_output(x, y);
        self.run();
        self.run();
        let mut events = std::mem::take(&mut *self.registry.data.events.lock().unwrap());
        assert_eq!(events.len(), 1);
        let event = events.pop().unwrap();
        let Ev::<WlRegistry>::Global {
            name,
            interface,
            version,
        } = event
        else {
            panic!("Unexpected event: {event:?}");
        };

        assert_eq!(interface, WlOutput::interface().name);
        let output = TestObject::<WlOutput>::from_request(
            &self.registry.obj,
            Req::<WlRegistry>::Bind {
                name,
                id: (WlOutput::interface(), version),
            },
        );
        self.run();
        (output, self.testwl.last_created_output())
    }

    fn enable_xdg_output(&mut self) -> TestObject<ZxdgOutputManagerV1> {
        self.testwl.enable_xdg_output_manager();
        self.run();
        self.run();

        let mut events = std::mem::take(&mut *self.registry.data.events.lock().unwrap());
        assert_eq!(
            events.len(),
            1,
            "Unexpected number of global events after enabling xdg output"
        );
        let event = events.pop().unwrap();
        let Ev::<WlRegistry>::Global {
            name,
            interface,
            version,
        } = event
        else {
            panic!("Unexpected event: {event:?}");
        };

        assert_eq!(interface, ZxdgOutputManagerV1::interface().name);
        let man = TestObject::<ZxdgOutputManagerV1>::from_request(
            &self.registry.obj,
            Req::<WlRegistry>::Bind {
                name,
                id: (ZxdgOutputManagerV1::interface(), version),
            },
        );
        self.run();
        man
    }

    fn create_xdg_output(&mut self, man: &TestObject<ZxdgOutputManagerV1>, output: WlOutput) {
        TestObject::<ZxdgOutputV1>::from_request(
            &man.obj,
            zxdg_output_manager_v1::Request::GetXdgOutput { output },
        );
        self.run();
        self.run();
    }

    fn register_window(&mut self, window: Window, data: WindowData) {
        self.satellite
            .connection
            .as_mut()
            .unwrap()
            .windows
            .insert(window, data);
    }

    fn new_window(
        &mut self,
        window: Window,
        override_redirect: bool,
        data: WindowData,
        parent: Option<Window>,
    ) {
        let dims = data.dims;
        self.register_window(window, data);
        self.satellite
            .new_window(window, override_redirect, dims, parent);
    }

    fn map_window(
        &mut self,
        comp: &Compositor,
        window: Window,
        surface: &WlSurface,
        buffer: &TestObject<WlBuffer>,
    ) {
        self.satellite.map_window(window);
        self.associate_window(comp, window, surface);
        self.run();
        surface
            .send_request(Req::<WlSurface>::Attach {
                buffer: Some(buffer.obj.clone()),
                x: 0,
                y: 0,
            })
            .unwrap();
    }

    fn associate_window(&mut self, comp: &Compositor, window: Window, surface: &WlSurface) {
        let xwl = TestObject::<XwaylandSurfaceV1>::from_request(
            &comp.shell.obj,
            Req::<XwaylandShellV1>::GetXwaylandSurface {
                surface: surface.clone(),
            },
        );

        let serial = self.surface_serial;
        let serial_lo = (serial & 0xFF) as u32;
        let serial_hi = (serial >> 8 & 0xFF) as u32;
        self.surface_serial += 1;

        xwl.send_request(Req::<XwaylandSurfaceV1>::SetSerial {
            serial_lo,
            serial_hi,
        })
        .unwrap();
        self.satellite
            .set_window_serial(window, [serial_lo, serial_hi]);
    }

    #[track_caller]
    fn check_new_surface(&mut self) -> testwl::SurfaceId {
        let id = self
            .testwl
            .last_created_surface_id()
            .expect("Surface not created");
        assert!(self.testwl.get_surface_data(id).is_some());
        id
    }

    fn create_toplevel(
        &mut self,
        comp: &Compositor,
        window: Window,
    ) -> (TestObject<WlSurface>, testwl::SurfaceId) {
        let (buffer, surface) = comp.create_surface();

        let data = WindowData {
            mapped: true,
            dims: WindowDims {
                x: 0,
                y: 0,
                width: 50,
                height: 50,
            },
            fullscreen: false,
        };

        self.new_window(window, false, data, None);
        self.map_window(comp, window, &surface.obj, &buffer);
        self.run();
        let id = self.check_new_surface();

        {
            let surface_data = self.testwl.get_surface_data(id).unwrap();
            assert!(
                surface_data.surface
                    == self
                        .testwl
                        .get_object::<s_proto::wl_surface::WlSurface>(id)
                        .unwrap()
            );
            assert!(surface_data.buffer.is_none());
            assert!(
                matches!(surface_data.role, Some(testwl::SurfaceRole::Toplevel(_))),
                "surface role: {:?}",
                surface_data.role
            );
        }

        self.testwl
            .configure_toplevel(id, 100, 100, vec![xdg_toplevel::State::Activated]);
        self.testwl.focus_toplevel(id);
        self.run();

        {
            let surface_data = self.testwl.get_surface_data(id).unwrap();
            assert!(surface_data.buffer.is_some());
        }

        let win_data = self.connection().windows.get(&window).map(|d| &d.dims);
        assert!(
            matches!(
                win_data,
                Some(&super::WindowDims {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100
                })
            ),
            "Incorrect window geometry: {win_data:?}"
        );

        (surface, id)
    }

    fn create_popup(
        &mut self,
        comp: &Compositor,
        window: Window,
        parent_window: Window,
        parent_surface: testwl::SurfaceId,
        x: i16,
        y: i16,
    ) -> (TestObject<WlSurface>, testwl::SurfaceId) {
        let (buffer, surface) = comp.create_surface();
        let data = WindowData {
            mapped: true,
            dims: WindowDims {
                x,
                y,
                width: 50,
                height: 50,
            },
            fullscreen: false,
        };
        let dims = data.dims;
        self.new_window(window, true, data, None);
        self.map_window(comp, window, &surface.obj, &buffer);
        self.run();

        let popup_id = self.check_new_surface();
        assert_ne!(popup_id, parent_surface);

        {
            let surface_data = self.testwl.get_surface_data(popup_id).unwrap();
            assert!(
                surface_data.surface
                    == self
                        .testwl
                        .get_object::<s_proto::wl_surface::WlSurface>(popup_id)
                        .unwrap()
            );
            assert!(surface_data.buffer.is_none());
            assert!(
                matches!(surface_data.role, Some(testwl::SurfaceRole::Popup(_))),
                "surface was not a popup (role: {:?})",
                surface_data.role
            );

            let toplevel_xdg = &self
                .testwl
                .get_surface_data(parent_surface)
                .unwrap()
                .xdg()
                .surface;
            assert_eq!(&surface_data.popup().parent, toplevel_xdg);

            let pos = &surface_data.popup().positioner_state;
            assert_eq!(pos.size.as_ref().unwrap(), &testwl::Vec2 { x: 50, y: 50 });

            let parent_win = &self.connection().windows[&parent_window];
            assert_eq!(
                pos.anchor_rect.as_ref().unwrap(),
                &testwl::Rect {
                    size: testwl::Vec2 {
                        x: parent_win.dims.width as _,
                        y: parent_win.dims.height as _
                    },
                    offset: testwl::Vec2::default()
                }
            );
            assert_eq!(
                pos.offset,
                testwl::Vec2 {
                    x: (dims.x - parent_win.dims.x) as _,
                    y: (dims.y - parent_win.dims.y) as _
                }
            );
            assert_eq!(pos.anchor, xdg_positioner::Anchor::TopLeft);
            assert_eq!(pos.gravity, xdg_positioner::Gravity::BottomRight);
        }

        self.testwl.configure_popup(popup_id);
        self.run();

        {
            let surface_data = self.testwl.get_surface_data(popup_id).unwrap();
            assert!(surface_data.buffer.is_some());
        }

        (surface, popup_id)
    }
}

struct TestObjectData<T: Proxy> {
    events: Mutex<Vec<T::Event>>,
    _phantom: std::marker::PhantomData<T>,
}

impl<T: Proxy> Default for TestObjectData<T> {
    fn default() -> Self {
        Self {
            events: Default::default(),
            _phantom: Default::default(),
        }
    }
}

impl<T: Proxy + Send + Sync + 'static> ObjectData for TestObjectData<T>
where
    T::Event: Send + Sync + std::fmt::Debug,
{
    fn event(
        self: Arc<Self>,
        backend: &Backend,
        msg: Message<ObjectId, std::os::fd::OwnedFd>,
    ) -> Option<Arc<dyn ObjectData>> {
        fn obj_data<T>() -> Arc<dyn ObjectData>
        where
            T: Proxy + Send + Sync + 'static,
            T::Event: Send + Sync + std::fmt::Debug,
        {
            Arc::new(TestObjectData::<T>::default())
        }

        let new_data = match (msg.sender_id.interface().name, msg.opcode) {
            (x, opcode) if x == ZwpTabletSeatV2::interface().name => match opcode {
                EVT_TABLET_ADDED_OPCODE => Some(obj_data::<ZwpTabletV2>()),
                EVT_TOOL_ADDED_OPCODE => Some(obj_data::<ZwpTabletToolV2>()),
                EVT_PAD_ADDED_OPCODE => Some(obj_data::<ZwpTabletPadV2>()),
                _ => None,
            },
            (x, EVT_GROUP_OPCODE) if x == ZwpTabletPadV2::interface().name => {
                Some(obj_data::<ZwpTabletPadGroupV2>())
            }
            (x, opcode) if x == ZwpTabletPadGroupV2::interface().name => match opcode {
                EVT_RING_OPCODE => Some(obj_data::<ZwpTabletPadRingV2>()),
                EVT_STRIP_OPCODE => Some(obj_data::<ZwpTabletPadStripV2>()),
                _ => None,
            },
            _ => None,
        };
        let connection = Connection::from_backend(backend.clone());
        let event = T::parse_event(&connection, msg).unwrap().1;
        self.events.lock().unwrap().push(event);
        new_data
    }

    fn destroyed(&self, _: ObjectId) {}
}

struct TestObject<T: Proxy> {
    obj: T,
    data: Arc<TestObjectData<T>>,
}

impl<T: Proxy> std::ops::Deref for TestObject<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.obj
    }
}

impl<T: Proxy + Sync + Send + 'static> TestObject<T>
where
    T::Event: Sync + Send + std::fmt::Debug,
{
    fn from_request<P: Proxy>(object: &P, request: P::Request<'_>) -> Self {
        let data = Arc::<TestObjectData<T>>::default();
        let obj: T = P::send_constructor(object, request, data.clone()).unwrap();
        Self { obj, data }
    }
}

type Req<'a, T> = <T as Proxy>::Request<'a>;
type Ev<T> = <T as Proxy>::Event;

// Matches Xwayland flow.
#[test]
fn toplevel_flow() {
    let (mut f, compositor) = TestFixture::new_with_compositor();

    let window = unsafe { Window::new(1) };
    let (surface, testwl_id) = f.create_toplevel(&compositor, window);
    {
        let surface_data = f.testwl.get_surface_data(testwl_id).unwrap();
        assert!(surface_data.buffer.is_some());
    }

    f.testwl.close_toplevel(testwl_id);
    f.run();

    assert!(!f.satellite.connection.as_ref().unwrap().windows[&window].mapped);

    assert!(
        f.testwl.get_surface_data(testwl_id).is_some(),
        "Surface should still exist for closed toplevel"
    );
    assert!(surface.obj.is_alive());

    // For some reason, we can get two UnmapNotify events
    // https://tronche.com/gui/x/icccm/sec-4.html#s-4.1.4
    f.satellite.unmap_window(window);
    f.satellite.unmap_window(window);
    f.satellite.destroy_window(window);
    surface.obj.destroy();
    f.run();

    assert!(f.testwl.get_surface_data(testwl_id).is_none());
}

#[test]
fn popup_flow_simple() {
    let (mut f, compositor) = TestFixture::new_with_compositor();

    let win_toplevel = unsafe { Window::new(1) };
    let (_, toplevel_id) = f.create_toplevel(&compositor, win_toplevel);

    let win_popup = unsafe { Window::new(2) };
    let (popup_surface, popup_id) =
        f.create_popup(&compositor, win_popup, win_toplevel, toplevel_id, 10, 10);

    f.satellite.unmap_window(win_popup);
    f.satellite.destroy_window(win_popup);
    popup_surface.obj.destroy();
    f.run();

    assert!(f.testwl.get_surface_data(popup_id).is_none());
}

#[test]
fn pass_through_globals() {
    use wayland_client::protocol::wl_output::WlOutput;

    let mut f = TestFixture::new();
    f.testwl.new_output(0, 0);
    f.testwl.enable_xdg_output_manager();
    f.run();
    f.run();

    const fn check<T: Proxy>() {}

    macro_rules! globals_struct {
        ($($field:ident),+) => {
            $( check::<$field>(); )+
            #[derive(Default)]
            #[allow(non_snake_case)]
            struct SupportedGlobals {
                $( $field: bool ),+
            }

            impl SupportedGlobals {
                fn check_globals(&self) {
                    $( assert!(self.$field, "Missing global {}", stringify!($field)); )+
                }

                fn global_found(&mut self, interface: String) {
                    match interface {
                        $(
                            x if x == $field::interface().name => {
                                self.$field = true;
                            }
                        )+
                        _ => panic!("Found an unhandled global: {interface}")
                    }
                }
            }
        }
    }

    // New globals need to be added here and in testwl.
    globals_struct! {
        WlCompositor,
        WlShm,
        WlOutput,
        WlSeat,
        ZwpLinuxDmabufV1,
        ZwpRelativePointerManagerV1,
        ZxdgOutputManagerV1,
        WpViewporter,
        WlDrm,
        ZwpPointerConstraintsV1,
        XwaylandShellV1,
        ZwpTabletManagerV2
    }

    let mut globals = SupportedGlobals::default();
    f.run();
    let events = std::mem::take(&mut *f.registry.data.events.lock().unwrap());
    assert!(!events.is_empty());
    for event in events {
        let Ev::<WlRegistry>::Global { interface, .. } = event else {
            unreachable!();
        };

        globals.global_found(interface);
    }

    globals.check_globals();
}

#[test]
fn last_activated_toplevel_is_focused() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let win1 = unsafe { Window::new(1) };

    let (_surface1, id1) = f.create_toplevel(&comp, win1);
    assert_eq!(
        f.connection().focused_window,
        Some(win1),
        "new toplevel's window is not focused"
    );

    let win2 = unsafe { Window::new(2) };
    let _data2 = f.create_toplevel(&comp, win2);
    assert_eq!(
        f.connection().focused_window,
        Some(win2),
        "toplevel focus did not switch"
    );

    f.testwl.configure_toplevel(id1, 100, 100, vec![]);
    f.run();
    assert_eq!(
        f.connection().focused_window,
        Some(win2),
        "toplevel focus did not stay the same"
    );
}

#[test]
fn popup_window_changes_surface() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let t_win = unsafe { Window::new(1) };
    let (_, toplevel_id) = f.create_toplevel(&comp, t_win);

    let win = unsafe { Window::new(2) };
    let (surface, old_id) = f.create_popup(&comp, win, t_win, toplevel_id, 0, 0);

    f.satellite.unmap_window(win);
    surface.obj.destroy();
    f.run();

    assert!(f.testwl.get_surface_data(old_id).is_none());

    let (_, surface) = comp.create_surface();
    f.run();
    let id = f
        .testwl
        .last_created_surface_id()
        .expect("No surface created");

    assert_ne!(old_id, id);
    assert!(f.testwl.get_surface_data(id).is_some());

    f.satellite.map_window(win);
    f.associate_window(&comp, win, &surface.obj);
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert!(data.popup().popup.is_alive());

    f.testwl.configure_popup(id);
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert!(data.popup().popup.is_alive());
}

#[test]
fn override_redirect_window_after_toplevel_close() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let win1 = unsafe { Window::new(1) };
    let (obj, first) = f.create_toplevel(&comp, win1);
    f.testwl.close_toplevel(first);
    f.run();

    f.satellite.unmap_window(win1);
    f.satellite.destroy_window(win1);
    obj.obj.destroy();
    f.run();

    assert!(f.testwl.get_surface_data(first).is_none());

    let win2 = unsafe { Window::new(2) };
    let (buffer, surface) = comp.create_surface();
    f.new_window(win2, true, WindowData::default(), None);
    f.map_window(&comp, win2, &surface.obj, &buffer);
    let second = f.check_new_surface();
    let data = f.testwl.get_surface_data(second).unwrap();
    assert!(
        matches!(data.role, Some(testwl::SurfaceRole::Toplevel(_))),
        "wrong role: {:?}",
        data.role
    )
}

#[test]
fn fullscreen() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let win = unsafe { Window::new(1) };
    let (_, id) = f.create_toplevel(&comp, win);

    f.satellite.set_fullscreen(win, SetState::Add);
    f.run();
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert!(data
        .toplevel()
        .states
        .contains(&xdg_toplevel::State::Fullscreen));

    f.satellite.set_fullscreen(win, SetState::Remove);
    f.run();
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert!(!data
        .toplevel()
        .states
        .contains(&xdg_toplevel::State::Fullscreen));

    f.satellite.set_fullscreen(win, SetState::Toggle);
    f.run();
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert!(data
        .toplevel()
        .states
        .contains(&xdg_toplevel::State::Fullscreen));

    f.satellite.set_fullscreen(win, SetState::Toggle);
    f.run();
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert!(!data
        .toplevel()
        .states
        .contains(&xdg_toplevel::State::Fullscreen));
}

#[test]
fn window_title_and_class() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let win = unsafe { Window::new(1) };
    let (_, id) = f.create_toplevel(&comp, win);

    f.satellite
        .set_win_title(win, WmName::WmName("window".into()));
    f.satellite.set_win_class(win, "class".into());
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert_eq!(data.toplevel().title, Some("window".into()));
    assert_eq!(data.toplevel().app_id, Some("class".into()));

    f.satellite
        .set_win_title(win, WmName::NetWmName("superwindow".into()));
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert_eq!(data.toplevel().title, Some("superwindow".into()));

    f.satellite
        .set_win_title(win, WmName::WmName("shwindow".into()));
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert_eq!(data.toplevel().title, Some("superwindow".into()));
}

#[test]
fn window_group_properties() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let prop_win = unsafe { Window::new(1) };
    f.satellite.new_window(
        prop_win,
        false,
        super::WindowDims {
            width: 1,
            height: 1,
            ..Default::default()
        },
        None,
    );
    f.satellite
        .set_win_title(prop_win, WmName::WmName("window".into()));
    f.satellite.set_win_class(prop_win, "class".into());

    let win = unsafe { Window::new(2) };
    let data = WindowData {
        mapped: true,
        dims: WindowDims {
            width: 50,
            height: 50,
            ..Default::default()
        },
        fullscreen: false,
    };

    let (_, surface) = comp.create_surface();
    let dims = data.dims;
    f.register_window(win, data);
    f.satellite.new_window(win, false, dims, None);
    f.satellite.set_win_hints(
        win,
        super::WmHints {
            window_group: Some(prop_win),
        },
    );
    f.satellite.map_window(win);
    f.associate_window(&comp, win, &surface.obj);
    f.run();

    let id = f.testwl.last_created_surface_id().unwrap();
    let data = f.testwl.get_surface_data(id).unwrap();

    assert_eq!(data.toplevel().title, Some("window".into()));
    assert_eq!(data.toplevel().app_id, Some("class".into()));
}

#[test]
fn copy_from_x11() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let win = unsafe { Window::new(1) };
    let (_surface, _id) = f.create_toplevel(&comp, win);

    let mimes = std::rc::Rc::new(vec![
        testwl::PasteData {
            mime_type: "text".to_string(),
            data: b"abc".to_vec(),
        },
        testwl::PasteData {
            mime_type: "data".to_string(),
            data: vec![1, 2, 3, 4, 6, 10],
        },
    ]);

    f.satellite.set_copy_paste_source(&mimes);
    f.run();

    let server_mimes = f.testwl.data_source_mimes();
    for mime in mimes.iter() {
        assert!(server_mimes.contains(&mime.mime_type));
    }

    let data = f.testwl.paste_data(|_, _| {
        f.satellite.run();
        true
    });
    assert_eq!(*mimes, data);
}

#[test]
fn copy_from_wayland() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    TestObject::<WlKeyboard>::from_request(&comp.seat.obj, wl_seat::Request::GetKeyboard {});
    let win = unsafe { Window::new(1) };
    let (_surface, _id) = f.create_toplevel(&comp, win);

    let mimes = vec![
        testwl::PasteData {
            mime_type: "text".to_string(),
            data: b"abc".to_vec(),
        },
        testwl::PasteData {
            mime_type: "data".to_string(),
            data: vec![1, 2, 3, 4, 6, 10],
        },
    ];
    f.testwl.create_data_offer(mimes.clone());
    f.run();

    let selection = f.satellite.new_selection().expect("No new selection");
    for mime in &mimes {
        let data = std::thread::scope(|s| {
            // receive requires a queue flush - dispatch testwl from another thread
            s.spawn(|| {
                let pollfd = unsafe { BorrowedFd::borrow_raw(f.testwl.poll_fd().as_raw_fd()) };
                let mut pollfd = [PollFd::from_borrowed_fd(pollfd, PollFlags::IN)];
                if poll(&mut pollfd, 100).unwrap() == 0 {
                    panic!("Did not get events for testwl!");
                }
                f.testwl.dispatch();
                while poll(&mut pollfd, 100).unwrap() > 0 {
                    f.testwl.dispatch();
                }
            });
            selection.receive(mime.mime_type.clone(), &f.satellite)
        });
        f.run();
        assert_eq!(data, mime.data);
    }
}

#[test]
fn clipboard_x11_then_wayland() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    TestObject::<WlKeyboard>::from_request(&comp.seat.obj, wl_seat::Request::GetKeyboard {});
    let win = unsafe { Window::new(1) };
    let (_surface, _id) = f.create_toplevel(&comp, win);

    let x11data = std::rc::Rc::new(vec![
        testwl::PasteData {
            mime_type: "text".to_string(),
            data: b"abc".to_vec(),
        },
        testwl::PasteData {
            mime_type: "data".to_string(),
            data: vec![1, 2, 3, 4, 6, 10],
        },
    ]);

    f.satellite.set_copy_paste_source(&x11data);
    f.run();

    let waylanddata = vec![
        testwl::PasteData {
            mime_type: "asdf".to_string(),
            data: b"fdaa".to_vec(),
        },
        testwl::PasteData {
            mime_type: "boing".to_string(),
            data: vec![10, 20, 40, 50],
        },
    ];
    f.testwl.create_data_offer(waylanddata.clone());
    f.run();
    f.run();

    let selection = f.satellite.new_selection().expect("No new selection");
    for mime in &waylanddata {
        let data = std::thread::scope(|s| {
            // receive requires a queue flush - dispatch testwl from another thread
            s.spawn(|| {
                let pollfd = unsafe { BorrowedFd::borrow_raw(f.testwl.poll_fd().as_raw_fd()) };
                let mut pollfd = [PollFd::from_borrowed_fd(pollfd, PollFlags::IN)];
                if poll(&mut pollfd, 100).unwrap() == 0 {
                    panic!("Did not get events for testwl!");
                }
                f.testwl.dispatch();
                while poll(&mut pollfd, 100).unwrap() > 0 {
                    f.testwl.dispatch();
                }
            });
            selection.receive(mime.mime_type.clone(), &f.satellite)
        });
        f.run();
        assert_eq!(data, mime.data);
    }
}

#[test]
fn raise_window_on_pointer_event() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    TestObject::<WlPointer>::from_request(&comp.seat.obj, wl_seat::Request::GetPointer {});
    let win1 = unsafe { Window::new(1) };
    let (_, id1) = f.create_toplevel(&comp, win1);
    f.testwl.configure_toplevel(id1, 100, 100, vec![]);

    let win2 = unsafe { Window::new(2) };
    let (_, id2) = f.create_toplevel(&comp, win2);
    assert_eq!(f.connection().focused_window, Some(win2));

    f.testwl.move_pointer_to(id2, 0.0, 0.0);
    f.run();
    assert_eq!(f.connection().focused_window, Some(win2));
    assert_eq!(f.satellite.last_hovered, Some(win2));

    f.testwl.move_pointer_to(id1, 0.0, 0.0);
    f.run();
    assert_eq!(f.connection().focused_window, Some(win2));
    assert_eq!(f.satellite.last_hovered, Some(win1));
}

#[test]
fn override_redirect_choose_hover_window() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    TestObject::<WlPointer>::from_request(&comp.seat.obj, wl_seat::Request::GetPointer {});
    let win1 = unsafe { Window::new(1) };
    let (_, id1) = f.create_toplevel(&comp, win1);
    f.testwl.configure_toplevel(id1, 100, 100, vec![]);

    let win2 = unsafe { Window::new(2) };
    let _ = f.create_toplevel(&comp, win2);
    assert_eq!(f.connection().focused_window, Some(win2));

    f.testwl.move_pointer_to(id1, 0.0, 0.0);
    f.run();
    assert_eq!(f.satellite.last_hovered, Some(win1));

    let win3 = unsafe { Window::new(3) };
    let (buffer, surface) = comp.create_surface();
    f.new_window(win3, true, WindowData::default(), None);
    f.map_window(&comp, win3, &surface.obj, &buffer);
    f.run();
    let id3 = f.check_new_surface();
    let popup_data = f.testwl.get_surface_data(id3).unwrap();
    let win1_xdg = &f.testwl.get_surface_data(id1).unwrap().xdg().surface;
    assert_eq!(&popup_data.popup().parent, win1_xdg);
}

#[test]
fn output_offset() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let (output_obj, output) = f.new_output(0, 0);
    let man = f.enable_xdg_output();
    f.create_xdg_output(&man, output_obj.obj);
    f.testwl.move_xdg_output(&output, 500, 100);
    f.run();
    let window = unsafe { Window::new(1) };

    {
        let (surface, surface_id) = f.create_toplevel(&comp, window);
        f.testwl.move_surface_to_output(surface_id, &output);
        f.run();
        let data = &f.connection().windows[&window];
        assert_eq!(data.dims.x, 500);
        assert_eq!(data.dims.y, 100);

        f.satellite.unmap_window(window);
        surface.obj.destroy();
        f.run();
    }

    let (t_buffer, t_surface) = comp.create_surface();
    f.map_window(&comp, window, &t_surface.obj, &t_buffer);
    f.run();
    let t_id = f.testwl.last_created_surface_id().unwrap();
    f.testwl.move_surface_to_output(t_id, &output);
    f.run();
    {
        let data = f.testwl.get_surface_data(t_id).unwrap();
        assert!(
            matches!(data.role, Some(testwl::SurfaceRole::Toplevel(_))),
            "surface role: {:?}",
            data.role
        );
    }
    f.testwl.configure_toplevel(t_id, 100, 100, vec![]);
    f.testwl.focus_toplevel(t_id);
    f.run();

    {
        let data = &f.connection().windows[&window];
        assert_eq!(data.dims.x, 500);
        assert_eq!(data.dims.y, 100);
    }

    let popup = unsafe { Window::new(2) };
    let (p_surface, p_id) = f.create_popup(&comp, popup, window, t_id, 510, 110);
    f.testwl.move_surface_to_output(p_id, &output);
    f.run();
    let data = f.testwl.get_surface_data(p_id).unwrap();
    assert_eq!(
        data.popup().positioner_state.offset,
        testwl::Vec2 { x: 10, y: 10 }
    );

    f.satellite.unmap_window(popup);
    p_surface.obj.destroy();
    f.run();

    let (buffer, surface) = comp.create_surface();
    f.map_window(&comp, popup, &surface.obj, &buffer);
    f.run();
    let p_id = f.testwl.last_created_surface_id().unwrap();
    f.testwl.move_surface_to_output(p_id, &output);
    f.testwl.configure_popup(p_id);
    f.run();
    let data = f.testwl.get_surface_data(p_id).unwrap();
    assert_eq!(
        data.popup().positioner_state.offset,
        testwl::Vec2 { x: 10, y: 10 }
    );
}

#[test]
fn output_offset_change() {
    let (mut f, comp) = TestFixture::new_with_compositor();

    let (output_obj, output) = f.new_output(500, 100);
    let window = unsafe { Window::new(1) };
    let (_, id) = f.create_toplevel(&comp, window);
    f.testwl.move_surface_to_output(id, &output);
    f.run();

    let test_position = |f: &TestFixture, x, y| {
        let data = &f.connection().windows[&window];
        assert_eq!(data.dims.x, x);
        assert_eq!(data.dims.y, y);
    };
    test_position(&f, 500, 100);

    f.testwl.move_output(&output, 600, 200);
    f.run();
    f.run();
    test_position(&f, 600, 200);

    let man = f.enable_xdg_output();
    f.create_xdg_output(&man, output_obj.obj);
    // testwl inits xdg output position to 0, and it should take priority over wl_output position
    test_position(&f, 0, 0);

    f.testwl.move_xdg_output(&output, 1000, 22);
    f.run();
    test_position(&f, 1000, 22);

    f.testwl.move_output(&output, 600, 200);
    f.run();
    f.run();
    test_position(&f, 1000, 22);
}

#[test]
fn reposition_popup() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let toplevel = unsafe { Window::new(1) };
    let (_, t_id) = f.create_toplevel(&comp, toplevel);

    let popup = unsafe { Window::new(2) };
    let (_, p_id) = f.create_popup(&comp, popup, toplevel, t_id, 20, 40);

    f.satellite.reconfigure_window(x::ConfigureNotifyEvent::new(
        popup,
        popup,
        x::WINDOW_NONE,
        40,  // x
        60,  // y
        80,  // width
        100, // height
        0,
        true,
    ));
    f.run();
    f.run();
    let data = f.testwl.get_surface_data(p_id).unwrap();
    assert_eq!(
        data.popup().positioner_state.offset,
        testwl::Vec2 { x: 40, y: 60 }
    );
    assert_eq!(
        data.popup().positioner_state.size,
        Some(testwl::Vec2 { x: 80, y: 100 })
    );
    let win_data = &f.connection().windows[&popup];
    assert_eq!(
        win_data.dims,
        WindowDims {
            x: 40,
            y: 60,
            width: 80,
            height: 100
        }
    );
}

#[test]
fn ignore_toplevel_reconfigure() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let toplevel = unsafe { Window::new(1) };
    let _ = f.create_toplevel(&comp, toplevel);

    f.satellite.reconfigure_window(x::ConfigureNotifyEvent::new(
        toplevel,
        toplevel,
        x::WINDOW_NONE,
        40,  // x
        60,  // y
        80,  // width
        100, // height
        0,
        true,
    ));

    f.run();
    let win_data = &f.connection().windows[&toplevel];
    assert_eq!(
        win_data.dims,
        WindowDims {
            x: 0,
            y: 0,
            width: 100,
            height: 100
        }
    );
}

type EventMatcher<'a, Event> = Box<dyn FnMut(&Event) -> bool + 'a>;
#[track_caller]
fn events_check<Event: std::fmt::Debug, const N: usize>(
    mut it: impl Iterator<Item = Event>,
    mut matchers: [EventMatcher<'_, Event>; N],
) {
    for (idx, matcher) in matchers.iter_mut().enumerate() {
        let item = it.next();
        if item.is_none() {
            panic!("event {idx} does not exist");
        }
        if !matcher(item.as_ref().unwrap()) {
            panic!("event {idx} was wrong ({item:?})");
        }
    }

    let mut remaining = it.peekable();
    if remaining.peek().is_some() {
        panic!("remaining events: {:?}", remaining.collect::<Vec<_>>());
    }
}

#[test]
fn tablet_smoke_test() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let seat = TestObject::<ZwpTabletSeatV2>::from_request(
        &comp.tablet_man.obj,
        zwp_tablet_manager_v2::Request::GetTabletSeat {
            seat: comp.seat.obj,
        },
    );
    // Not sure why exactly this requires 4 runs but it works so idk
    for _ in 0..4 {
        f.run();
    }

    let events = std::mem::take(&mut *seat.data.events.lock().unwrap()).into_iter();
    let (mut tab_id, mut tool_id, mut pad_id) = (None, None, None);
    events_check(
        events,
        [
            Box::new(|e| match e {
                zwp_tablet_seat_v2::Event::TabletAdded { id } => {
                    tab_id = Some(id.clone());
                    true
                }
                _ => false,
            }),
            Box::new(|e| match e {
                zwp_tablet_seat_v2::Event::ToolAdded { id } => {
                    tool_id = Some(id.clone());
                    true
                }
                _ => false,
            }),
            Box::new(|e| match e {
                zwp_tablet_seat_v2::Event::PadAdded { id } => {
                    pad_id = Some(id.clone());
                    true
                }
                _ => false,
            }),
        ],
    );
    let (tab_id, tool_id, pad_id) = (tab_id.unwrap(), tool_id.unwrap(), pad_id.unwrap());

    // For reasons beyond my mortal understanding, `id.object_data()` does not work properly.
    let tab_data = f.object_data(&tab_id);
    let tab_events = std::mem::take(&mut *tab_data.events.lock().unwrap()).into_iter();
    events_check(
        tab_events,
        [
            Box::new(|e| matches!(e, zwp_tablet_v2::Event::Name { name } if name == "tabby")),
            Box::new(|e| matches!(e, zwp_tablet_v2::Event::Done)),
        ],
    );

    let tool_data = f.object_data(&tool_id);
    let tool_events = std::mem::take(&mut *tool_data.events.lock().unwrap()).into_iter();
    events_check(
        tool_events,
        [
            Box::new(|e| {
                matches!(
                    e,
                    zwp_tablet_tool_v2::Event::Type {
                        tool_type: WEnum::Value(zwp_tablet_tool_v2::Type::Finger)
                    }
                )
            }),
            Box::new(|e| matches!(e, zwp_tablet_tool_v2::Event::Done)),
        ],
    );

    let pad_data = f.object_data(&pad_id);
    let pad_events = std::mem::take(&mut *pad_data.events.lock().unwrap()).into_iter();
    let mut group = None;
    events_check(
        pad_events,
        [
            Box::new(|e| matches!(e, zwp_tablet_pad_v2::Event::Buttons { buttons: 5 })),
            Box::new(|e| match e {
                zwp_tablet_pad_v2::Event::Group { pad_group } => {
                    group = Some(pad_group.clone());
                    true
                }
                _ => false,
            }),
            Box::new(|e| matches!(e, zwp_tablet_pad_v2::Event::Done)),
        ],
    );

    let group = group.unwrap();
    let g_data = f.object_data(&group);
    let g_events = std::mem::take(&mut *g_data.events.lock().unwrap()).into_iter();
    events_check(
        g_events,
        [
            Box::new(|e| {
                matches!(e,
                zwp_tablet_pad_group_v2::Event::Buttons { buttons } if buttons.is_empty())
            }),
            Box::new(|e| matches!(e, zwp_tablet_pad_group_v2::Event::Ring { .. })),
            Box::new(|e| matches!(e, zwp_tablet_pad_group_v2::Event::Strip { .. })),
            Box::new(|e| matches!(e, zwp_tablet_pad_group_v2::Event::Done)),
        ],
    )
}

#[test]
fn fullscreen_heuristic() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let (_, output) = f.new_output(0, 0);

    let window1 = unsafe { Window::new(1) };
    let (_, id) = f.create_toplevel(&comp, window1);
    f.testwl.move_surface_to_output(id, &output);
    f.run();

    let mut check_fullscreen = |id, override_redirect| {
        let window = unsafe { Window::new(id) };
        let (buffer, surface) = comp.create_surface();
        let data = WindowData {
            mapped: true,
            dims: WindowDims {
                x: 0,
                y: 0,
                // Outputs default to 1000x1000 in testwl
                width: 1000,
                height: 1000,
            },
            fullscreen: false,
        };
        f.new_window(window, override_redirect, data, None);
        f.map_window(&comp, window, &surface.obj, &buffer);
        f.run();
        let id = f.check_new_surface();
        let surface_data = f.testwl.get_surface_data(id).unwrap();
        assert!(
            surface_data.surface
                == f.testwl
                    .get_object::<s_proto::wl_surface::WlSurface>(id)
                    .unwrap()
        );

        let Some(testwl::SurfaceRole::Toplevel(toplevel_data)) = &surface_data.role else {
            panic!("Expected toplevel, got {:?}", surface_data.role);
        };

        assert!(
            toplevel_data
                .states
                .contains(&xdg_toplevel::State::Fullscreen),
            "states: {:?}",
            toplevel_data.states
        );
    };

    check_fullscreen(2, false);
    check_fullscreen(3, true);
}

/// See Pointer::handle_event for an explanation.
#[test]
fn popup_pointer_motion_workaround() {}
