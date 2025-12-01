use super::{selection::Clipboard, InnerServerState, NoConnection, ServerState, WindowDims};
use crate::server::selection::{Primary, SelectionType};
use crate::xstate::{SetState, WinSize, WmName};
use crate::{timespec_from_millis, XConnection};
use rustix::event::{poll, PollFd, PollFlags};
use std::collections::HashMap;
use std::io::Write;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use testwl::{SendDataForMimeFn, SurfaceRole};
use wayland_client::{
    backend::{protocol::Message, Backend, ObjectData, ObjectId, WaylandError},
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_display::WlDisplay,
        wl_keyboard::WlKeyboard,
        wl_output::{self, WlOutput},
        wl_pointer::WlPointer,
        wl_registry::WlRegistry,
        wl_seat::{self, WlSeat},
        wl_shm::{Format, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
        wl_touch::{self, WlTouch},
    },
    Connection, Proxy, WEnum,
};
use wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1;
use wayland_protocols::{
    wp::{
        linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        pointer_constraints::zv1::client::{
            zwp_locked_pointer_v1::ZwpLockedPointerV1,
            zwp_pointer_constraints_v1::{self, ZwpPointerConstraintsV1},
        },
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
    },
    xdg::{
        shell::server::{xdg_positioner, xdg_toplevel},
        xdg_output::zv1::client::{
            zxdg_output_manager_v1::{self, ZxdgOutputManagerV1},
            zxdg_output_v1::{self, ZxdgOutputV1},
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
        } => $name_optional:ident
    ) => {
        $( #[$attr] )?
        struct $name$(<$($lifetimes),+>)? {
            $(
                $field: $type
            ),+
        }

        #[derive(Default)]
        struct $name_optional {
            $(
                $field: Option<$type>
            ),+
        }

        impl From<$name_optional> for $name {
            fn from(opt: $name_optional) -> Self {
                Self {
                    $(
                        $field: opt.$field.expect(concat!("uninitialized field ", stringify!($field)))
                    ),+
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
    tablet_man: TestObject<ZwpTabletManagerV2>,
    pointer_constraints: TestObject<ZwpPointerConstraintsV1>,
} => CompositorOptional

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
#[derive(Default)]
struct FakeXConnection {
    focused_window: Option<Window>,
    windows: HashMap<Window, WindowData>,
}

impl FakeXConnection {
    #[track_caller]
    fn window_mut(&mut self, window: Window) -> &mut WindowData {
        self.windows
            .get_mut(&window)
            .unwrap_or_else(|| panic!("Unknown window: {window:?}"))
    }

    #[track_caller]
    fn window(&self, window: Window) -> &WindowData {
        self.windows
            .get(&window)
            .unwrap_or_else(|| panic!("Unknown window: {window:?}"))
    }
}

type FakeX11Selection = Vec<testwl::PasteData>;
impl crate::X11Selection for Vec<testwl::PasteData> {
    fn mime_types(&self) -> Vec<&str> {
        self.iter().map(|data| data.mime_type.as_str()).collect()
    }

    fn write_to(
        &self,
        mime: &str,
        mut pipe: smithay_client_toolkit::data_device_manager::WritePipe,
    ) {
        let data = self
            .iter()
            .find(|data| data.mime_type == mime)
            .unwrap_or_else(|| panic!("Couldn't find mime type {mime}"));
        pipe.write_all(&data.data)
            .expect("Couldn't write paste data");
    }
}

impl super::XConnection for FakeXConnection {
    type X11Selection = FakeX11Selection;
    #[track_caller]
    fn close_window(&mut self, window: Window) {
        log::debug!("closing window {window:?}");
        self.window_mut(window).mapped = false;
    }

    #[track_caller]
    fn set_fullscreen(&mut self, window: xcb::x::Window, fullscreen: bool) {
        self.window_mut(window).fullscreen = fullscreen;
    }

    #[track_caller]
    fn set_window_dims(&mut self, window: Window, state: super::PendingSurfaceState) -> bool {
        self.window_mut(window).dims = WindowDims {
            x: state.x as _,
            y: state.y as _,
            width: state.width as _,
            height: state.height as _,
        };
        true
    }

    #[track_caller]
    fn focus_window(&mut self, window: Window, _output_name: Option<String>) {
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

    fn unmap_window(&mut self, _: x::Window) {
        todo!()
    }
}

type EarlyTestFixture = TestFixture<NoConnection<FakeX11Selection>>;

struct TestFixture<C: XConnection> {
    testwl: testwl::Server,
    satellite: ServerState<C>,
    /// Our connection to satellite - i.e., where Xwayland sends requests to
    xwls_connection: Arc<Connection>,
    /// Satellite's display - must dispatch this for our server state to advance
    xwls_display: Display<InnerServerState<C::X11Selection>>,
    surface_serial: u64,
    registry: TestObject<WlRegistry>,
}

static INIT: std::sync::Once = std::sync::Once::new();

struct PopupBuilder {
    window: Window,
    parent_window: Window,
    parent_surface: testwl::SurfaceId,
    dims: WindowDims,
    scale: f64,
    check_size_and_pos: bool,
}

impl PopupBuilder {
    fn new(window: Window, parent_window: Window, parent_surface: testwl::SurfaceId) -> Self {
        Self {
            window,
            parent_window,
            parent_surface,
            dims: WindowDims {
                x: 0,
                y: 0,
                width: 50,
                height: 50,
            },
            scale: 1.0,
            check_size_and_pos: true,
        }
    }

    fn x(mut self, x: i16) -> Self {
        self.dims.x = x;
        self
    }

    fn y(mut self, y: i16) -> Self {
        self.dims.y = y;
        self
    }

    fn width(mut self, width: u16) -> Self {
        self.dims.width = width;
        self
    }

    fn height(mut self, height: u16) -> Self {
        self.dims.height = height;
        self
    }

    fn check_size_and_pos(mut self, check: bool) -> Self {
        self.check_size_and_pos = check;
        self
    }

    fn scale(mut self, scale: impl Into<f64>) -> Self {
        self.scale = scale.into();
        self
    }
}

trait PreConnectFn: Sized {
    fn call(self, _: &mut testwl::Server) {}
}
impl<F: FnOnce(&mut testwl::Server)> PreConnectFn for F {
    fn call(self, server: &mut testwl::Server) {
        self(server);
    }
}
impl PreConnectFn for () {}

struct SetupOptions<F> {
    pre_connect: Option<F>,
}

impl<F: PreConnectFn> SetupOptions<F> {
    fn pre_connect(pre_connect: F) -> Self {
        Self {
            pre_connect: Some(pre_connect),
        }
    }
}

impl Default for SetupOptions<()> {
    fn default() -> Self {
        Self { pre_connect: None }
    }
}

impl EarlyTestFixture {
    fn new_early_with_options<F: PreConnectFn>(options: SetupOptions<F>) -> Self {
        INIT.call_once(|| {
            pretty_env_logger::env_logger::builder()
                .is_test(true)
                .filter_level(log::LevelFilter::Trace)
                .init()
        });

        let (client_s, server_s) = UnixStream::pair().unwrap();
        let mut testwl = testwl::Server::new(true);
        if let Some(pre_connect) = options.pre_connect {
            pre_connect.call(&mut testwl);
        }
        let display = Display::new().unwrap();
        testwl.connect(server_s);
        // Handle initial globals roundtrip setup requirement
        let thread = std::thread::spawn(move || {
            let mut pollfd = [PollFd::from_borrowed_fd(testwl.poll_fd(), PollFlags::IN)];
            let timeout = timespec_from_millis(1000);
            if poll(&mut pollfd, Some(&timeout)).unwrap() == 0 {
                panic!("Did not get events for testwl!");
            }
            testwl.dispatch();
            testwl
        });

        let (fake_client, xwls_server) = UnixStream::pair().unwrap();
        let satellite = ServerState::new(display.handle(), Some(client_s), xwls_server);
        let testwl = thread.join().unwrap();

        let xwls_connection = Connection::from_socket(fake_client).unwrap();
        let registry = TestObject::<WlRegistry>::from_request(
            &xwls_connection.display(),
            Req::<WlDisplay>::GetRegistry {},
        );

        let mut f = EarlyTestFixture {
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

    fn upgrade_connection(self, connection: FakeXConnection) -> TestFixture<FakeXConnection> {
        TestFixture {
            testwl: self.testwl,
            satellite: self.satellite.upgrade_connection(connection),
            xwls_connection: self.xwls_connection,
            xwls_display: self.xwls_display,
            surface_serial: self.surface_serial,
            registry: self.registry,
        }
    }
}

impl<C: XConnection> TestFixture<C> {
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
                    x if x == ZwpPointerConstraintsV1::interface().name => {
                        bind!(pointer_constraints)
                    }
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

    /// Cascade our requests/events through satellite and testwl
    fn run(&mut self) {
        // Flush our requests to satellite
        self.xwls_connection.flush().unwrap();

        // Have satellite dispatch our requests
        self.xwls_display
            .dispatch_clients(&mut *self.satellite)
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
        self.run();
        (output, self.testwl.last_created_output())
    }
}

impl TestFixture<FakeXConnection> {
    fn new_with_options<F: PreConnectFn>(options: SetupOptions<F>) -> Self {
        EarlyTestFixture::new_early_with_options(options)
            .upgrade_connection(FakeXConnection::default())
    }

    fn new() -> Self {
        Self::new_with_options(SetupOptions::default())
    }

    fn new_pre_connect(pre_connect: impl FnOnce(&mut testwl::Server)) -> Self {
        Self::new_with_options(SetupOptions::pre_connect(pre_connect))
    }

    fn new_with_compositor() -> (Self, Compositor) {
        let mut f = Self::new();
        let compositor = f.compositor();
        (f, compositor)
    }

    fn connection(&self) -> &FakeXConnection {
        &self.satellite.connection
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

    fn create_xdg_output(
        &mut self,
        man: &TestObject<ZxdgOutputManagerV1>,
        output: WlOutput,
    ) -> TestObject<ZxdgOutputV1> {
        let xdg = TestObject::<ZxdgOutputV1>::from_request(
            &man.obj,
            zxdg_output_manager_v1::Request::GetXdgOutput { output },
        );
        self.run();
        self.run();
        xdg
    }

    fn register_window(&mut self, window: Window, data: WindowData) {
        self.satellite.connection.windows.insert(window, data);
    }

    fn new_window(&mut self, window: Window, override_redirect: bool, data: WindowData) {
        let dims = data.dims;
        self.register_window(window, data);
        self.satellite
            .new_window(window, override_redirect, dims, None);
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
        let serial_hi = ((serial >> 8) & 0xFF) as u32;
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

        self.new_window(window, false, data);
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

    #[track_caller]
    fn create_popup(
        &mut self,
        comp: &Compositor,
        builder: PopupBuilder,
    ) -> (TestObject<WlSurface>, testwl::SurfaceId) {
        let (buffer, surface) = comp.create_surface();
        let PopupBuilder {
            window,
            parent_window,
            parent_surface,
            dims,
            scale,
            check_size_and_pos,
        } = builder;

        let data = WindowData {
            mapped: true,
            dims,
            fullscreen: false,
        };
        self.new_window(window, true, data);
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

            let parent_xdg = &self
                .testwl
                .get_surface_data(parent_surface)
                .unwrap()
                .xdg()
                .surface;
            assert_eq!(surface_data.popup().parent.id(), parent_xdg.id());

            let pos = &surface_data.popup().positioner_state;
            if check_size_and_pos {
                assert_eq!(
                    pos.size.as_ref().unwrap(),
                    &testwl::Vec2 {
                        x: (dims.width as f64 / scale) as i32,
                        y: (dims.height as f64 / scale) as i32
                    }
                );

                let parent_win = &self.connection().windows[&parent_window];
                assert_eq!(
                    pos.anchor_rect.as_ref().unwrap(),
                    &testwl::Rect {
                        size: testwl::Vec2 {
                            x: (parent_win.dims.width as f64 / scale) as i32,
                            y: (parent_win.dims.height as f64 / scale) as i32
                        },
                        offset: testwl::Vec2::default()
                    }
                );
                assert_eq!(
                    pos.offset,
                    testwl::Vec2 {
                        x: ((dims.x - parent_win.dims.x) as f64 / scale) as i32,
                        y: ((dims.y - parent_win.dims.y) as f64 / scale) as i32
                    }
                );
            }
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

    #[track_caller]
    fn assert_window_dimensions(
        &self,
        window: x::Window,
        surface_id: testwl::SurfaceId,
        dims: WindowDims,
    ) {
        let data = self.testwl.get_surface_data(surface_id).unwrap();
        match data.role {
            Some(SurfaceRole::Popup(_)) => {
                assert_eq!(
                    data.popup().positioner_state.offset,
                    testwl::Vec2 {
                        x: dims.x as _,
                        y: dims.y as _
                    }
                );
                assert_eq!(
                    data.popup().positioner_state.size,
                    Some(testwl::Vec2 {
                        x: dims.width as _,
                        y: dims.height as _
                    })
                );
                let win_data = &self.connection().windows[&window];
                assert_eq!(win_data.dims, dims);
            }
            Some(SurfaceRole::Toplevel(_)) => {
                let win_data = self
                    .satellite
                    .windows
                    .get(&window)
                    .copied()
                    .and_then(|id| {
                        let d = self.satellite.world.entity(id).unwrap();
                        d.get::<&crate::server::WindowData>()
                    })
                    .unwrap();
                assert_eq!(win_data.attrs.dims, dims);

                let viewport = data.viewport.as_ref().expect("Missing viewport");
                assert_eq!(viewport.width, dims.width as _);
                assert_eq!(viewport.height, dims.height as _);
            }
            ref e => {
                panic!("tried to assert dimensions of something not a toplevel or popup: {e:?}",);
            }
        }
    }

    fn reconfigure_window(&mut self, window: Window, dims: WindowDims, override_redirect: bool) {
        self.satellite
            .reconfigure_window(x::ConfigureNotifyEvent::new(
                window,
                window,
                x::WINDOW_NONE,
                dims.x,
                dims.y,
                dims.width,
                dims.height,
                0,
                override_redirect,
            ));
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

    assert!(!f.satellite.connection.windows[&window].mapped);

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
    let (popup_surface, popup_id) = f.create_popup(
        &compositor,
        PopupBuilder::new(win_popup, win_toplevel, toplevel_id),
    );

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
    let (surface, old_id) = f.create_popup(&comp, PopupBuilder::new(win, t_win, toplevel_id));

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
    f.new_window(win2, true, WindowData::default());
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

trait SelectionTest {
    type SelectionType: SelectionType;
    fn mimes(testwl: &mut testwl::Server) -> Vec<String>;
    fn paste_data(
        testwl: &mut testwl::Server,
        send_data: impl SendDataForMimeFn,
    ) -> Vec<testwl::PasteData>;
    fn create_offer(testwl: &mut testwl::Server, data: Vec<testwl::PasteData>);
}

macro_rules! selection_tests {
    ($name:ident, $selection_type:ty, $get_mime_fn:ident, $get_paste_data_fn:ident, $create_offer_fn:ident) => {
        impl SelectionTest for $selection_type {
            type SelectionType = $selection_type;
            fn mimes(testwl: &mut testwl::Server) -> Vec<String> {
                testwl.$get_mime_fn()
            }
            fn paste_data(
                testwl: &mut testwl::Server,
                send_data: impl SendDataForMimeFn,
            ) -> Vec<testwl::PasteData> {
                testwl.$get_paste_data_fn(send_data)
            }
            fn create_offer(testwl: &mut testwl::Server, data: Vec<testwl::PasteData>) {
                testwl.$create_offer_fn(data);
            }
        }

        mod $name {
            use super::*;
            #[test]
            fn copy_from_x11() {
                super::copy_from_x11::<$selection_type>();
            }

            #[test]
            fn copy_from_wayland() {
                super::copy_from_wayland::<$selection_type>();
            }

            #[test]
            fn x11_then_wayland() {
                super::selection_x11_then_wayland::<$selection_type>();
            }
        }
    };
}

selection_tests!(
    clipboard,
    Clipboard,
    data_source_mimes,
    clipboard_paste_data,
    create_data_offer
);
selection_tests!(
    primary,
    Primary,
    primary_source_mimes,
    primary_paste_data,
    create_primary_offer
);

fn copy_from_x11<T: SelectionTest>() {
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

    f.satellite.set_selection_source::<T::SelectionType>(&mimes);
    f.run();

    let server_mimes = T::mimes(&mut f.testwl);
    for mime in mimes.iter() {
        assert!(server_mimes.contains(&mime.mime_type));
    }

    let data = T::paste_data(&mut f.testwl, |_, _| {
        f.satellite.run();
        true
    });
    assert_eq!(*mimes, data);
}

fn copy_from_wayland<T: SelectionTest>() {
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
    T::create_offer(&mut f.testwl, mimes.clone());
    f.run();

    let selection = f
        .satellite
        .new_selection::<T::SelectionType>()
        .expect("No new selection");
    for mime in &mimes {
        let data = std::thread::scope(|s| {
            // receive requires a queue flush - dispatch testwl from another thread
            s.spawn(|| {
                let pollfd = unsafe { BorrowedFd::borrow_raw(f.testwl.poll_fd().as_raw_fd()) };
                let mut pollfd = [PollFd::from_borrowed_fd(pollfd, PollFlags::IN)];
                let timeout = timespec_from_millis(100);
                if poll(&mut pollfd, Some(&timeout)).unwrap() == 0 {
                    panic!("Did not get events for testwl!");
                }
                f.testwl.dispatch();
                while poll(&mut pollfd, Some(&timeout)).unwrap() > 0 {
                    f.testwl.dispatch();
                }
            });
            selection.receive(mime.mime_type.clone(), &f.satellite)
        });
        f.run();
        assert_eq!(data, mime.data);
    }
}

fn selection_x11_then_wayland<T: SelectionTest>() {
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

    f.satellite
        .set_selection_source::<T::SelectionType>(&x11data);
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
    T::create_offer(&mut f.testwl, waylanddata.clone());
    f.run();
    f.run();

    let selection = f
        .satellite
        .new_selection::<T::SelectionType>()
        .expect("No new selection");
    for mime in &waylanddata {
        let data = std::thread::scope(|s| {
            // receive requires a queue flush - dispatch testwl from another thread
            s.spawn(|| {
                let pollfd = unsafe { BorrowedFd::borrow_raw(f.testwl.poll_fd().as_raw_fd()) };
                let mut pollfd = [PollFd::from_borrowed_fd(pollfd, PollFlags::IN)];
                let timeout = timespec_from_millis(100);
                if poll(&mut pollfd, Some(&timeout)).unwrap() == 0 {
                    panic!("Did not get events for testwl!");
                }
                f.testwl.dispatch();
                while poll(&mut pollfd, Some(&timeout)).unwrap() > 0 {
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
    f.new_window(
        win3,
        true,
        WindowData {
            dims: WindowDims {
                width: 1,
                height: 1,
                ..Default::default()
            },
            ..Default::default()
        },
    );
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
    let (p_surface, p_id) =
        f.create_popup(&comp, PopupBuilder::new(popup, window, t_id).x(510).y(110));
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

    let test_position = |f: &TestFixture<_>, x, y| {
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
fn reconfigure_popup() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let toplevel = unsafe { Window::new(1) };
    let (_, t_id) = f.create_toplevel(&comp, toplevel);

    let popup = unsafe { Window::new(2) };
    let (_, p_id) = f.create_popup(&comp, PopupBuilder::new(popup, toplevel, t_id).x(20).y(40));

    let new_dims = WindowDims {
        x: 40,
        y: 60,
        width: 80,
        height: 100,
    };
    f.reconfigure_window(popup, new_dims, true);
    f.run();
    f.run();
    f.assert_window_dimensions(popup, p_id, new_dims);
}

#[test]
fn reconfigure_popup_after_map() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let toplevel = unsafe { Window::new(1) };
    f.create_toplevel(&comp, toplevel);

    let popup = unsafe { Window::new(2) };
    let old_dims = WindowDims {
        x: 20,
        y: 40,
        width: 10,
        height: 10,
    };
    let new_dims = WindowDims {
        x: 40,
        y: 60,
        width: 80,
        height: 100,
    };

    let (buffer, surface) = comp.create_surface();
    let popup_data = WindowData {
        mapped: true,
        dims: old_dims,
        fullscreen: false,
    };
    f.new_window(popup, true, popup_data);
    f.satellite.map_window(popup);
    f.reconfigure_window(popup, new_dims, true);
    f.associate_window(&comp, popup, &surface);
    f.run();
    surface
        .send_request(Req::<WlSurface>::Attach {
            buffer: Some(buffer.obj.clone()),
            x: 0,
            y: 0,
        })
        .unwrap();
    f.run();
    let p_id = f.check_new_surface();
    f.testwl.configure_popup(p_id);
    f.run();
    f.assert_window_dimensions(popup, p_id, new_dims);
}

#[test]
fn reconfigure_toplevel() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let toplevel = unsafe { Window::new(1) };
    let (_, surface) = f.create_toplevel(&comp, toplevel);

    let mut dims = WindowDims {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
    };
    f.assert_window_dimensions(toplevel, surface, dims);

    dims.width = 80;
    dims.height = 120;
    let final_dims = dims;
    // A toplevel can be resized, but not change position
    dims.x = 20;
    dims.y = 20;
    f.reconfigure_window(toplevel, dims, false);
    f.run();
    f.run();

    f.assert_window_dimensions(toplevel, surface, final_dims);
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
        f.new_window(window, override_redirect, data);
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

#[track_caller]
fn check_output_position_event(output: &TestObject<WlOutput>, x: i32, y: i32) {
    let events = std::mem::take(&mut *output.data.events.lock().unwrap());
    assert!(!events.is_empty());
    let mut done = false;
    let mut geo = false;
    for event in events {
        match event {
            wl_output::Event::Geometry {
                x: geo_x, y: geo_y, ..
            } => {
                assert_eq!(geo_x, x);
                assert_eq!(geo_y, y);
                geo = true;
            }
            wl_output::Event::Done => {
                done = true;
            }
            _ => {}
        }
    }

    assert!(geo, "Didn't get geometry event");
    assert!(done, "Didn't get done event");
}

#[test]
fn negative_output_position() {
    let mut f = TestFixture::new();
    std::mem::take(&mut *f.registry.data.events.lock().unwrap());
    let (output, _) = f.new_output(-500, -500);
    f.run();
    f.run();
    check_output_position_event(&output, 0, 0);

    let (output2, _) = f.new_output(0, 0);
    f.run();
    f.run();
    check_output_position_event(&output2, 500, 500);
    assert!(output.data.events.lock().unwrap().is_empty());

    let (output3, _) = f.new_output(500, 500);
    f.run();
    f.run();
    check_output_position_event(&output3, 1000, 1000);
    assert!(output.data.events.lock().unwrap().is_empty());
    assert!(output2.data.events.lock().unwrap().is_empty());
}

#[test]
fn negative_output_position_update_offset() {
    let mut f = TestFixture::new();
    std::mem::take(&mut *f.registry.data.events.lock().unwrap());

    let (output, _) = f.new_output(-500, -500);
    f.run();
    f.run();
    check_output_position_event(&output, 0, 0);

    let (output2, _) = f.new_output(0, -1000);
    f.run();
    f.run();
    check_output_position_event(&output, 0, 500);
    check_output_position_event(&output2, 500, 0);

    let (output3, _) = f.new_output(-1000, 0);
    f.run();
    f.run();
    check_output_position_event(&output, 500, 500);
    check_output_position_event(&output2, 1000, 0);
    check_output_position_event(&output3, 0, 1000);
}

#[test]
fn negative_output_xdg_position_update_offset() {
    let mut f = TestFixture::new();
    std::mem::take(&mut *f.registry.data.events.lock().unwrap());
    let xdg = f.enable_xdg_output();

    let (output, _) = f.new_output(-500, -500);
    f.run();
    f.run();
    check_output_position_event(&output, 0, 0);

    let (output2, output_s) = f.new_output(0, 0);
    let xdg_output = f.create_xdg_output(&xdg, output2.obj);
    f.testwl.move_xdg_output(&output_s, 0, -1000);
    f.run();
    f.run();
    check_output_position_event(&output, 0, 500);

    let mut found = false;
    let mut first = false;
    for event in std::mem::take(&mut *xdg_output.data.events.lock().unwrap()) {
        if let zxdg_output_v1::Event::LogicalPosition { x, y } = event {
            // Testwl sends a logical position event when the output is first created
            // We are interested in the second one generated by satellite
            if !first {
                first = true;
                continue;
            }
            assert_eq!(x, 500);
            assert_eq!(y, 0);
            found = true;
            break;
        }
    }
    assert!(found, "Did not get xdg output logical position");
    found = false;
    for event in std::mem::take(&mut *output2.data.events.lock().unwrap()) {
        if let wl_output::Event::Done = event {
            found = true;
            break;
        }
    }
    assert!(found, "Did not get done event");
}

#[test]
fn negative_output_position_remove_offset() {
    let mut f = TestFixture::new();
    std::mem::take(&mut *f.registry.data.events.lock().unwrap());

    let (c_output, s_output) = f.new_output(-500, -500);
    f.run();
    f.run();
    check_output_position_event(&c_output, 0, 0);

    f.testwl.move_output(&s_output, 500, 500);
    f.run();
    f.run();
    check_output_position_event(&c_output, 500, 500);
}

#[test]
fn scaled_output_popup() {
    let (mut f, comp) = TestFixture::new_with_compositor();

    let (_, output) = f.new_output(0, 0);
    let scale = 2;
    output.scale(scale);
    output.done();
    f.run();
    f.run();

    let toplevel = unsafe { Window::new(1) };
    let (_, toplevel_id) = f.create_toplevel(&comp, toplevel);
    f.testwl.move_surface_to_output(toplevel_id, &output);
    f.run();

    let popup = unsafe { Window::new(2) };
    let builder = PopupBuilder::new(popup, toplevel, toplevel_id)
        .x(50)
        .y(50)
        .scale(scale);
    let initial_dims = builder.dims;
    let (_, popup_id) = f.create_popup(&comp, builder);
    f.testwl.move_surface_to_output(popup_id, &output);
    f.run();
    assert_eq!(
        initial_dims,
        f.connection().window(popup).dims,
        "X11 dimensions changed after configure"
    );
}

#[test]
fn fractional_scale_popup() {
    let mut f = TestFixture::new_pre_connect(|testwl| {
        testwl.enable_fractional_scale();
    });
    let comp = f.compositor();
    let (_, output) = f.new_output(0, 0);

    let toplevel = unsafe { Window::new(1) };
    let (_, toplevel_id) = f.create_toplevel(&comp, toplevel);
    let surface_data = f
        .testwl
        .get_surface_data(toplevel_id)
        .expect("No surface data");
    let fractional = surface_data
        .fractional
        .as_ref()
        .expect("No fractional scale for surface");

    fractional.preferred_scale(180); // 1.5 scale
    f.testwl.move_surface_to_output(toplevel_id, &output);
    f.run();
    f.run();

    let popup = unsafe { Window::new(2) };
    let builder = PopupBuilder::new(popup, toplevel, toplevel_id)
        .x(60)
        .y(60)
        .width(60)
        .height(60)
        .scale(1.5);
    let initial_dims = builder.dims;
    f.create_popup(&comp, builder);
    f.run();
    assert_eq!(
        initial_dims,
        f.connection().window(popup).dims,
        "X11 dimensions changed after configure"
    );
}

#[test]
fn scaled_output_small_popup() {
    let (mut f, comp) = TestFixture::new_with_compositor();

    let (_, output) = f.new_output(0, 0);
    output.scale(2);
    output.done();
    f.run();
    f.run();

    let toplevel = unsafe { Window::new(1) };
    let (_, toplevel_id) = f.create_toplevel(&comp, toplevel);
    f.testwl.move_surface_to_output(toplevel_id, &output);
    f.run();

    let popup = unsafe { Window::new(2) };
    let builder = PopupBuilder::new(popup, toplevel, toplevel_id)
        .x(50)
        .y(50)
        .width(1)
        .height(1)
        .check_size_and_pos(false);

    let (_, popup_id) = f.create_popup(&comp, builder);
    f.testwl.move_surface_to_output(popup_id, &output);
    f.run();

    let dims = f.connection().window(popup).dims;

    assert!(dims.width > 0);
    assert!(dims.height > 0);
}

#[test]
fn fractional_scale_small_popup() {
    let mut f = TestFixture::new_pre_connect(|testwl| {
        testwl.enable_fractional_scale();
    });
    let comp = f.compositor();

    let (_, output) = f.new_output(0, 0);
    let toplevel = unsafe { Window::new(1) };
    let (_, toplevel_id) = f.create_toplevel(&comp, toplevel);
    let data = f.testwl.get_surface_data(toplevel_id).unwrap();
    let fractional = data
        .fractional
        .as_ref()
        .expect("Missing fracitonal scale data");
    fractional.preferred_scale(180); // 1.5 scale
    f.testwl.move_surface_to_output(toplevel_id, &output);
    f.run();
    f.run();

    {
        let data = f.testwl.get_surface_data(toplevel_id).unwrap();
        let viewport = data.viewport.as_ref().expect("Missing viewport");
        assert_eq!(viewport.width, 66);
        assert_eq!(viewport.height, 66);
    }

    let popup = unsafe { Window::new(2) };
    let builder = PopupBuilder::new(popup, toplevel, toplevel_id)
        .width(1)
        .height(1)
        .check_size_and_pos(false);

    let (_, popup_id) = f.create_popup(&comp, builder);
    let dims = f.connection().window(popup).dims;
    assert!(dims.width > 0);
    assert!(dims.height > 0);

    let data = f
        .testwl
        .get_surface_data(popup_id)
        .expect("Missing popup data");
    let pos = &data.popup().positioner_state;
    assert_eq!(pos.size.unwrap(), testwl::Vec2 { x: 1, y: 1 });

    let dims = WindowDims {
        x: 0,
        y: 0,
        width: 2,
        height: 1,
    };
    f.reconfigure_window(popup, dims, true);
    f.run();
    f.run();

    let dims = f.connection().window(popup).dims;
    assert!(dims.width > 0);
    assert!(dims.height > 0);

    let data = f
        .testwl
        .get_surface_data(popup_id)
        .expect("Missing popup data");
    let pos = &data.popup().positioner_state;
    assert_eq!(pos.size.unwrap(), testwl::Vec2 { x: 1, y: 1 });
}

#[test]
fn toplevel_size_limits_scaled() {
    let (mut f, comp) = TestFixture::new_with_compositor();

    let (_, output) = f.new_output(0, 0);
    output.scale(2);
    output.done();
    f.run();
    f.run();

    let window = unsafe { Window::new(1) };
    let (buffer, surface) = comp.create_surface();
    let data = WindowData {
        mapped: true,
        dims: WindowDims {
            width: 50,
            height: 50,
            ..Default::default()
        },
        fullscreen: false,
    };
    f.new_window(window, false, data);
    f.satellite.set_size_hints(
        window,
        super::WmNormalHints {
            min_size: Some(WinSize {
                width: 20,
                height: 20,
            }),
            max_size: Some(WinSize {
                width: 100,
                height: 100,
            }),
        },
    );

    f.map_window(&comp, window, &surface.obj, &buffer);
    f.run();

    let id = f.check_new_surface();
    f.testwl.configure_toplevel(id, 50, 50, vec![]);
    f.run();

    f.testwl.move_surface_to_output(id, &output);
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    let toplevel = data.toplevel();
    assert_eq!(toplevel.min_size, Some(testwl::Vec2 { x: 10, y: 10 }));
    assert_eq!(toplevel.max_size, Some(testwl::Vec2 { x: 50, y: 50 }));

    f.satellite.set_size_hints(
        window,
        super::WmNormalHints {
            min_size: Some(WinSize {
                width: 40,
                height: 40,
            }),
            max_size: Some(WinSize {
                width: 200,
                height: 200,
            }),
        },
    );
    f.run();
    let data = f.testwl.get_surface_data(id).unwrap();
    let toplevel = data.toplevel();
    assert_eq!(toplevel.min_size, Some(testwl::Vec2 { x: 20, y: 20 }));
    assert_eq!(toplevel.max_size, Some(testwl::Vec2 { x: 100, y: 100 }));

    // test sizing with decorations
    f.testwl
        .force_decoration_mode(id, zxdg_toplevel_decoration_v1::Mode::ClientSide);
    f.testwl.configure_toplevel(id, 100, 100, vec![]);
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    let toplevel = data.toplevel();
    assert_eq!(toplevel.min_size, Some(testwl::Vec2 { x: 20, y: 45 }));
    assert_eq!(toplevel.max_size, Some(testwl::Vec2 { x: 100, y: 125 }));
}

#[test]
fn subpopup_positioning() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    TestObject::<WlPointer>::from_request(&comp.seat.obj, wl_seat::Request::GetPointer {});
    let win_toplevel = unsafe { Window::new(1) };
    let (_, id_toplevel) = f.create_toplevel(&comp, win_toplevel);

    f.testwl.move_pointer_to(id_toplevel, 0.0, 0.0);
    f.run();

    let win_popup = unsafe { Window::new(2) };
    let (_, id_popup) = f.create_popup(
        &comp,
        PopupBuilder::new(win_popup, win_toplevel, id_toplevel)
            .x(25)
            .y(25),
    );

    f.testwl.move_pointer_to(id_popup, 1.0, 1.0);
    f.run();

    let win_subpopup = unsafe { Window::new(3) };

    f.create_popup(
        &comp,
        PopupBuilder::new(win_subpopup, win_toplevel, id_toplevel)
            .x(50)
            .y(50),
    );

    let dims = f.connection().window(win_subpopup).dims;
    assert_eq!(dims.x, 50);
    assert_eq!(dims.y, 50);
}

#[test]
fn transient_for_toplevel() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let toplevel = unsafe { Window::new(1) };
    let (_, toplevel_id) = f.create_toplevel(&comp, toplevel);

    let sub_toplevel = unsafe { Window::new(2) };
    let (buffer, surface) = comp.create_surface();
    f.new_window(
        sub_toplevel,
        false,
        WindowData {
            mapped: true,
            dims: WindowDims {
                width: 50,
                height: 50,
                ..Default::default()
            },
            fullscreen: false,
        },
    );

    f.satellite.set_transient_for(sub_toplevel, toplevel);
    f.map_window(&comp, sub_toplevel, &surface.obj, &buffer);
    f.run();
    let id = f.check_new_surface();
    let toplevel_data = f.testwl.get_surface_data(toplevel_id).unwrap();
    let sub_data = f.testwl.get_surface_data(id).unwrap();
    assert_eq!(
        sub_data.toplevel().parent,
        Some(toplevel_data.toplevel().toplevel.clone())
    );
}

#[test]
fn touch_fractional_scale() {
    let mut f = TestFixture::new_pre_connect(|testwl| {
        testwl.enable_fractional_scale();
    });
    let comp = f.compositor();
    let (_, output) = f.new_output(0, 0);
    let touch = TestObject::<WlTouch>::from_request(&comp.seat.obj, wl_seat::Request::GetTouch {});
    f.run();

    let toplevel = unsafe { Window::new(1) };
    let (_, id) = f.create_toplevel(&comp, toplevel);
    f.testwl.move_surface_to_output(id, &output);

    let data = f.testwl.get_surface_data(id).unwrap();
    let server_surface = data.surface.clone();
    let fractional = data.fractional.as_ref().cloned().unwrap();

    let do_touch = |f: &mut TestFixture<_>, x, y| {
        f.testwl.touch().down(0, 0, &server_surface, 0, x, y);
        f.testwl.dispatch();
        f.run();
        f.run();
        let events = &mut touch.data.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        let event = events.pop().unwrap();
        let wl_touch::Event::Down { x, y, .. } = event else {
            panic!("Got unexpected event: {event:?}");
        };
        (x, y)
    };

    let (x, y) = do_touch(&mut f, 20.0, 40.0);
    assert_eq!(x, 20.0);
    assert_eq!(y, 40.0);

    fractional.preferred_scale(180); // 1.5 scale
    f.run();
    let (x, y) = do_touch(&mut f, 20.0, 40.0);
    assert_eq!(x, 20.0 * 1.5);
    assert_eq!(y, 40.0 * 1.5);
}

#[test]
fn tablet_tool_fractional_scale() {
    let mut f = TestFixture::new_pre_connect(|testwl| {
        testwl.enable_fractional_scale();
    });
    let comp = f.compositor();
    let (_, output) = f.new_output(0, 0);
    let toplevel = unsafe { Window::new(1) };
    let (_, id) = f.create_toplevel(&comp, toplevel);
    let surface_data = f.testwl.get_surface_data(id).unwrap();
    let fractional = surface_data.fractional.as_ref().cloned().unwrap();
    let server_surface = surface_data.surface.clone();
    f.testwl.move_surface_to_output(id, &output);

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

    let mut tool = None;
    for event in seat.data.events.lock().unwrap().drain(..) {
        if let zwp_tablet_seat_v2::Event::ToolAdded { id } = event {
            tool = Some(id.clone());
            break;
        }
    }

    let client_tool = tool.expect("Didn't get tool");
    let client_data = f.object_data(&client_tool);
    let server_tool = f.testwl.tablet_tool().clone();
    let tablet = f.testwl.tablet().clone();

    let get_motion = || {
        let events = &mut *client_data.events.lock().unwrap();
        let event = events.pop();
        let Some(zwp_tablet_tool_v2::Event::Motion { x, y }) = event else {
            panic!("Didn't get motion event: {event:?}");
        };
        (x, y)
    };

    server_tool.proximity_in(0, &tablet, &server_surface);
    server_tool.motion(20.0, 40.0);
    f.testwl.dispatch();
    f.run();
    f.run();

    let (x, y) = get_motion();
    assert_eq!(x, 20.0);
    assert_eq!(y, 40.0);

    fractional.preferred_scale(180); // 1.5 scale
    server_tool.proximity_in(0, &tablet, &server_surface);
    server_tool.motion(20.0, 40.0);
    f.testwl.dispatch();
    f.run();
    f.run();

    let (x, y) = get_motion();
    assert_eq!(x, 20.0 * 1.5);
    assert_eq!(y, 40.0 * 1.5);
}

#[test]
fn output_updated_before_x_connection() {
    let mut f = EarlyTestFixture::new_early_with_options(SetupOptions::default());
    let comp = f.compositor();
    let (_, output) = f.new_output(-20, -20);

    let mut f = f.upgrade_connection(FakeXConnection::default());

    let window = unsafe { Window::new(1) };
    let (_, surface_id) = f.create_toplevel(&comp, window);
    f.testwl.move_surface_to_output(surface_id, &output);
    f.run();
    f.run();
    let data = &f.connection().windows[&window];
    assert_eq!(data.dims.x, 0);
    assert_eq!(data.dims.y, 0);
}

#[test]
fn quick_empty_data_offer() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    TestObject::<WlKeyboard>::from_request(&comp.seat.obj, wl_seat::Request::GetKeyboard {});
    let win = unsafe { Window::new(1) };
    let (_surface, _id) = f.create_toplevel(&comp, win);
    f.testwl.create_data_offer(vec![testwl::PasteData {
        mime_type: "text".to_string(),
        data: b"abc".to_vec(),
    }]);
    f.testwl.empty_data_offer();
    f.run();

    let selection = f.satellite.new_selection::<Clipboard>();
    assert!(selection.is_none());
}

#[test]
fn quick_destroy_window_with_serial() {
    let (mut f, comp) = TestFixture::new_with_compositor();

    let window = unsafe { Window::new(1) };
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
    f.new_window(window, false, data);
    f.satellite.map_window(window);

    let (_, surface) = comp.create_surface();
    let xwl = TestObject::<XwaylandSurfaceV1>::from_request(
        &comp.shell.obj,
        Req::<XwaylandShellV1>::GetXwaylandSurface {
            surface: surface.clone(),
        },
    );

    let serial = f.surface_serial;
    let serial_lo = (serial & 0xFF) as u32;
    let serial_hi = ((serial >> 8) & 0xFF) as u32;
    xwl.send_request(Req::<XwaylandSurfaceV1>::SetSerial {
        serial_lo,
        serial_hi,
    })
    .unwrap();
    f.satellite
        .set_window_serial(window, [serial_lo, serial_hi]);
    f.satellite.unmap_window(window);
    f.satellite.destroy_window(window);
    f.run();

    let id = f.testwl.last_created_surface_id().unwrap();
    let surface_data = f.testwl.get_surface_data(id).unwrap();
    assert!(
        surface_data.role.is_none(),
        "Surface unexpectedly has role: {:?}",
        surface_data.role
    );
}

#[test]
fn scaled_pointer_lock_position_hint() {
    let mut f = TestFixture::new_pre_connect(|testwl| {
        testwl.enable_fractional_scale();
    });
    let comp = f.compositor();
    let pointer =
        TestObject::<WlPointer>::from_request(&comp.seat.obj, wl_seat::Request::GetPointer {});

    let (_, output) = f.new_output(0, 0);
    let win = unsafe { Window::new(1) };
    let (surface, id) = f.create_toplevel(&comp, win);
    let surface_data = f.testwl.get_surface_data(id).expect("No surface data");
    let fractional = surface_data
        .fractional
        .as_ref()
        .expect("No fractional scale for surface");
    fractional.preferred_scale(180); // 1.5 scale
    f.testwl.move_surface_to_output(id, &output);
    f.run();
    f.run();

    let locked_pointer = TestObject::<ZwpLockedPointerV1>::from_request(
        &comp.pointer_constraints.obj,
        zwp_pointer_constraints_v1::Request::LockPointer {
            surface: surface.obj.clone(),
            pointer: pointer.obj.clone(),
            region: None,
            lifetime: WEnum::Value(zwp_pointer_constraints_v1::Lifetime::Persistent),
        },
    );
    locked_pointer.set_cursor_position_hint(75.0, 75.0);
    f.run();
    f.run();

    let lock_data = f
        .testwl
        .locked_pointer()
        .expect("Missing locked pointer data");
    assert_eq!(lock_data.surface, id);
    assert_eq!(
        lock_data.cursor_hint,
        Some(testwl::Vec2f { x: 50.0, y: 50.0 })
    );
}

#[test]
fn client_side_decorations() {
    let (mut f, compositor) = TestFixture::new_with_compositor();
    let window = unsafe { Window::new(1) };
    let (_, id) = f.create_toplevel(&compositor, window);
    f.testwl
        .force_decoration_mode(id, zxdg_toplevel_decoration_v1::Mode::ClientSide);
    f.testwl.configure_toplevel(id, 100, 100, vec![]);
    f.run();

    let data = f.connection().window(window);
    assert_eq!(
        data.dims,
        WindowDims {
            x: 0,
            y: 0,
            width: 100,
            height: 75
        }
    );
    let subsurface_id = f.testwl.last_created_surface_id().unwrap();
    assert_ne!(subsurface_id, id);
    let data = f.testwl.get_surface_data(subsurface_id).unwrap();
    assert!(data.buffer.is_some());
    let Some(SurfaceRole::Subsurface(subsurface)) = &data.role else {
        panic!("surface was not a subsurface: {:?}", data.role);
    };
    assert_eq!(subsurface.position, testwl::Vec2 { x: 0, y: -25 });
    assert_eq!(subsurface.parent, id);
    let subsurface = subsurface.subsurface.clone();

    f.testwl
        .configure_toplevel(id, 100, 100, vec![xdg_toplevel::State::Fullscreen]);
    f.run();
    let data = f.connection().window(window);
    assert_eq!(
        data.dims,
        WindowDims {
            x: 0,
            y: 0,
            width: 100,
            height: 100
        }
    );
    let data = f.testwl.get_surface_data(subsurface_id).unwrap();
    assert!(data.buffer.is_none());

    f.testwl
        .force_decoration_mode(id, zxdg_toplevel_decoration_v1::Mode::ServerSide);
    f.testwl.configure_toplevel(id, 100, 100, vec![]);
    f.run();

    let data = f.connection().window(window);
    assert_eq!(
        data.dims,
        WindowDims {
            x: 0,
            y: 0,
            width: 100,
            height: 100
        }
    );
    assert!(f.testwl.get_surface_data(subsurface_id).is_none());
    assert!(!subsurface.is_alive());
}

#[test]
fn client_side_decorations_no_global() {
    let mut f = TestFixture::new_pre_connect(|testwl| {
        testwl.disable_decorations_global();
    });
    let compositor = f.compositor();
    let window = unsafe { Window::new(1) };
    let (buffer, surface) = compositor.create_surface();

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

    f.new_window(window, false, data);
    f.map_window(&compositor, window, &surface.obj, &buffer);
    f.run();

    let surfaces = f.testwl.created_surfaces();
    assert_eq!(surfaces.len(), 2);
    let mut toplevel = None;
    let mut subsurface_parent = None;
    for id in surfaces {
        let data = f.testwl.get_surface_data(*id).unwrap();
        match data
            .role
            .as_ref()
            .expect("A surface was created without a role")
        {
            SurfaceRole::Toplevel(_) => {
                toplevel = Some(*id);
            }
            SurfaceRole::Subsurface(sub) => {
                assert_eq!(sub.position, testwl::Vec2 { x: 0, y: -25 });
                subsurface_parent = Some(sub.parent);
            }
            other => panic!("got surface with unexpected role: {other:?}"),
        }
    }

    assert_eq!(toplevel.unwrap(), subsurface_parent.unwrap());
}

#[test]
fn resize_decorations_on_reconfigure() {
    let (mut f, compositor) = TestFixture::new_with_compositor();
    let window = unsafe { Window::new(1) };
    let (_, id) = f.create_toplevel(&compositor, window);
    f.testwl
        .force_decoration_mode(id, zxdg_toplevel_decoration_v1::Mode::ClientSide);
    f.testwl.configure_toplevel(id, 100, 100, vec![]);
    f.run();

    let data = f.connection().window(window);
    assert_eq!(
        data.dims,
        WindowDims {
            x: 0,
            y: 0,
            width: 100,
            height: 75
        }
    );
    let subsurface_id = f.testwl.last_created_surface_id().unwrap();
    assert_ne!(subsurface_id, id);
    let data = f.testwl.get_surface_data(subsurface_id).unwrap();
    let buf_dims = f
        .testwl
        .get_buffer_dimensions(data.buffer.as_ref().expect("Missing buffer for subsurface"));
    assert_eq!(buf_dims, testwl::Vec2 { x: 100, y: 25 });
    assert!(
        matches!(data.role, Some(SurfaceRole::Subsurface(_))),
        "surface was not a subsurface: {:?}",
        data.role
    );

    let dims = WindowDims {
        x: 0,
        y: 0,
        width: 200,
        height: 200,
    };
    f.reconfigure_window(window, dims, false);
    f.run();
    f.run();

    let data = f.testwl.get_surface_data(subsurface_id).unwrap();
    let buf_dims = f
        .testwl
        .get_buffer_dimensions(data.buffer.as_ref().expect("Missing buffer for subsurface"));
    assert_eq!(buf_dims, testwl::Vec2 { x: 200, y: 25 });
}

/// See Pointer::handle_event for an explanation.
#[test]
fn popup_pointer_motion_workaround() {}
