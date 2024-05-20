use super::{ServerState, WindowDims};
use crate::xstate::{SetState, WmName};
use paste::paste;
use rustix::event::{poll, PollFd, PollFlags};
use std::collections::HashMap;
use std::os::fd::BorrowedFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use wayland_client::{
    backend::{protocol::Message, Backend, ObjectData, ObjectId, WaylandError},
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_display::WlDisplay,
        wl_registry::WlRegistry,
        wl_seat::WlSeat,
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
        viewporter::client::wp_viewporter::WpViewporter,
    },
    xdg::{
        shell::server::{xdg_positioner, xdg_toplevel},
        xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1,
    },
};
use wayland_server::{protocol as s_proto, Display, Resource};
use wl_drm::client::wl_drm::WlDrm;
use xcb::x::Window;

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
            .expect(&format!("Unknown window: {window:?}"))
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
    fn create(_: &FakeServerState) -> Self {
        ()
    }
}

impl super::XConnection for FakeXConnection {
    type ExtraData = ();
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
    fn focus_window(&mut self, window: Window, _: ()) {
        assert!(
            self.windows.contains_key(&window),
            "Unknown window: {window:?}"
        );
        self.focused_window = window.into();
    }
}

type FakeServerState = ServerState<FakeXConnection>;

struct TestFixture {
    testwl: testwl::Server,
    exwayland: FakeServerState,
    /// Our connection to exwayland - i.e., where Xwayland sends requests to
    exwl_connection: Arc<Connection>,
    /// Exwayland's display - must dispatch this for our server state to advance
    exwl_display: Display<FakeServerState>,
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
        let mut exwayland = FakeServerState::new(display.handle(), Some(client_s));
        let testwl = thread.join().unwrap();

        let (fake_client, ex_server) = UnixStream::pair().unwrap();
        exwayland.connect(ex_server);

        exwayland.set_x_connection(FakeXConnection::default());
        let mut f = TestFixture {
            testwl,
            exwayland,
            exwl_connection: Connection::from_socket(fake_client).unwrap().into(),
            exwl_display: display,
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
        self.exwayland.connection.as_ref().unwrap()
    }

    fn compositor(&mut self) -> Compositor {
        let mut ret = CompositorOptional::default();
        let wl_display = self.exwl_connection.display();

        let registry =
            TestObject::<WlRegistry>::from_request(&wl_display, Req::<WlDisplay>::GetRegistry {});
        self.run();

        let events = std::mem::take(&mut *registry.data.events.lock().unwrap());
        assert!(events.len() > 0);

        let bind_req = |name, interface, version| Req::<WlRegistry>::Bind {
            name,
            id: (interface, version),
        };

        for event in events {
            if let Ev::<WlRegistry>::Global {
                name,
                interface,
                version,
            } = event
            {
                match interface {
                    x if x == WlCompositor::interface().name => {
                        ret.compositor = Some(TestObject::from_request(
                            &registry.obj,
                            bind_req(name, WlCompositor::interface(), version),
                        ));
                    }
                    x if x == WlShm::interface().name => {
                        ret.shm = Some(TestObject::from_request(
                            &registry.obj,
                            bind_req(name, WlShm::interface(), version),
                        ));
                    }
                    _ => {}
                }
            }
        }

        ret.into()
    }

    /// Cascade our requests/events through exwayland and testwl
    fn run(&mut self) {
        // Flush our requests to exwayland
        self.exwl_connection.flush().unwrap();

        // Have exwayland dispatch our requests
        self.exwl_display
            .dispatch_clients(&mut self.exwayland)
            .unwrap();
        self.exwl_display.flush_clients().unwrap();

        // Dispatch any clientside requests
        self.exwayland.run();

        // Have testwl dispatch the clientside requests
        self.testwl.dispatch();

        // Handle clientside events
        self.exwayland.handle_clientside_events();

        self.testwl.dispatch();

        // Get our events
        let res = self.exwl_connection.prepare_read().unwrap().read();
        if res.is_err()
            && !matches!(res, Err(WaylandError::Io(ref e)) if e.kind() == std::io::ErrorKind::WouldBlock)
        {
            panic!("Read failed: {res:?}")
        }
    }

    fn register_window(&mut self, window: Window, data: WindowData) {
        self.exwayland
            .connection
            .as_mut()
            .unwrap()
            .windows
            .insert(window, data);
    }

    fn create_and_map_window(
        &mut self,
        comp: &Compositor,
        window: Window,
        override_redirect: bool,
    ) -> (TestObject<WlSurface>, testwl::SurfaceId) {
        let (_buffer, surface) = comp.create_surface();

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

        let dims = data.dims;

        self.register_window(window, data);
        self.exwayland
            .new_window(window, override_redirect, dims, None);
        self.exwayland.map_window(window);
        self.exwayland
            .associate_window(window, surface.id().protocol_id());

        self.run();

        let testwl_id = self
            .testwl
            .last_created_surface_id()
            .expect("Surface not created");

        assert!(self.testwl.get_surface_data(testwl_id).is_some());

        (surface, testwl_id)
    }

    fn create_toplevel(
        &mut self,
        comp: &Compositor,
        window: Window,
    ) -> (TestObject<WlSurface>, testwl::SurfaceId) {
        let (_buffer, surface) = comp.create_surface();

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

        let dims = data.dims;

        self.register_window(window, data);
        self.exwayland.new_window(window, false, dims, None);
        self.exwayland.map_window(window);
        self.exwayland
            .associate_window(window, surface.id().protocol_id());

        self.run();

        let testwl_id = self
            .testwl
            .last_created_surface_id()
            .expect("Toplevel surface not created");
        {
            let surface_data = self.testwl.get_surface_data(testwl_id).unwrap();
            assert!(
                surface_data.surface
                    == self
                        .testwl
                        .get_object::<s_proto::wl_surface::WlSurface>(testwl_id)
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
            .configure_toplevel(testwl_id, 100, 100, vec![xdg_toplevel::State::Activated]);
        self.run();

        {
            let surface_data = self.testwl.get_surface_data(testwl_id).unwrap();
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

        (surface, testwl_id)
    }

    fn create_popup(
        &mut self,
        comp: &Compositor,
        window: Window,
        parent_id: testwl::SurfaceId,
    ) -> (TestObject<WlSurface>, testwl::SurfaceId) {
        let (_, popup_surface) = comp.create_surface();
        let data = WindowData {
            mapped: true,
            dims: WindowDims {
                x: 10,
                y: 10,
                width: 50,
                height: 50,
            },
            fullscreen: false,
        };
        let dims = data.dims;
        self.register_window(window, data);
        self.exwayland.new_window(window, true, dims, None);
        self.exwayland.map_window(window);
        self.exwayland
            .associate_window(window, popup_surface.id().protocol_id());
        self.run();
        let popup_id = self.testwl.last_created_surface_id().unwrap();
        assert_ne!(popup_id, parent_id);

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
                .get_surface_data(parent_id)
                .unwrap()
                .xdg()
                .surface;
            assert_eq!(&surface_data.popup().parent, toplevel_xdg);

            let pos = &surface_data.popup().positioner_state;
            assert_eq!(pos.size.as_ref().unwrap(), &testwl::Vec2 { x: 50, y: 50 });
            assert_eq!(
                pos.anchor_rect.as_ref().unwrap(),
                &testwl::Rect {
                    size: testwl::Vec2 { x: 100, y: 100 },
                    offset: testwl::Vec2::default()
                }
            );
            assert_eq!(
                pos.offset,
                testwl::Vec2 {
                    x: dims.x as _,
                    y: dims.y as _
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

        (popup_surface, popup_id)
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
        let connection = Connection::from_backend(backend.clone());
        let event = T::parse_event(&connection, msg).unwrap().1;
        self.events.lock().unwrap().push(event);
        None
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

// TODO: tests to add
// - destroy window before surface
// - destroy surface before window
// - destroy popup and reassociate with new surface
// - reconfigure window (popup) before mapping
// - associate window after surface is already created

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

    assert!(!f.exwayland.connection.as_ref().unwrap().windows[&window].mapped);

    assert!(
        f.testwl.get_surface_data(testwl_id).is_some(),
        "Surface should still exist for closed toplevel"
    );
    assert!(surface.obj.is_alive());

    // For some reason, we can get two UnmapNotify events
    // https://tronche.com/gui/x/icccm/sec-4.html#s-4.1.4
    f.exwayland.unmap_window(window);
    f.exwayland.unmap_window(window);
    f.exwayland.destroy_window(window);
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
    let (popup_surface, popup_id) = f.create_popup(&compositor, win_popup, toplevel_id);

    f.exwayland.unmap_window(win_popup);
    f.exwayland.destroy_window(win_popup);
    popup_surface.obj.destroy();
    f.run();

    assert!(f.testwl.get_surface_data(popup_id).is_none());
}

#[test]
fn pass_through_globals() {
    use wayland_client::protocol::wl_output::WlOutput;

    let mut f = TestFixture::new();

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
        ZwpPointerConstraintsV1
    }

    let mut globals = SupportedGlobals::default();
    let display = f.exwl_connection.display();
    let registry =
        TestObject::<WlRegistry>::from_request(&display, Req::<WlDisplay>::GetRegistry {});
    f.run();
    let events = std::mem::take(&mut *registry.data.events.lock().unwrap());
    assert!(events.len() > 0);
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
    let (surface, old_id) = f.create_popup(&comp, win, toplevel_id);

    f.exwayland.unmap_window(win);
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

    f.exwayland.map_window(win);
    f.exwayland
        .associate_window(win, surface.id().protocol_id());
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

    f.exwayland.unmap_window(win1);
    f.exwayland.destroy_window(win1);
    obj.obj.destroy();
    f.run();

    assert!(f.testwl.get_surface_data(first).is_none());

    let win2 = unsafe { Window::new(2) };
    let (_, second) = f.create_and_map_window(&comp, win2, true);
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

    f.exwayland.set_fullscreen(win, SetState::Add);
    f.run();
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert!(data
        .toplevel()
        .states
        .contains(&xdg_toplevel::State::Fullscreen));

    f.exwayland.set_fullscreen(win, SetState::Remove);
    f.run();
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert!(!data
        .toplevel()
        .states
        .contains(&xdg_toplevel::State::Fullscreen));

    f.exwayland.set_fullscreen(win, SetState::Toggle);
    f.run();
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert!(data
        .toplevel()
        .states
        .contains(&xdg_toplevel::State::Fullscreen));

    f.exwayland.set_fullscreen(win, SetState::Toggle);
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

    f.exwayland
        .set_win_title(win, WmName::WmName("window".into()));
    f.exwayland.set_win_class(win, "class".into());
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert_eq!(data.toplevel().title, Some("window".into()));
    assert_eq!(data.toplevel().app_id, Some("class".into()));

    f.exwayland
        .set_win_title(win, WmName::NetWmName("superwindow".into()));
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert_eq!(data.toplevel().title, Some("superwindow".into()));

    f.exwayland
        .set_win_title(win, WmName::WmName("shwindow".into()));
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert_eq!(data.toplevel().title, Some("superwindow".into()));
}

#[test]
fn window_group_properties() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let prop_win = unsafe { Window::new(1) };
    f.exwayland.new_window(
        prop_win,
        false,
        super::WindowDims {
            width: 1,
            height: 1,
            ..Default::default()
        },
        None,
    );
    f.exwayland
        .set_win_title(prop_win, WmName::WmName("window".into()));
    f.exwayland.set_win_class(prop_win, "class".into());

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
    f.exwayland.new_window(win, false, dims, None);
    f.exwayland.set_win_hints(
        win,
        super::WmHints {
            window_group: Some(prop_win),
            ..Default::default()
        },
    );
    f.exwayland.map_window(win);
    f.exwayland
        .associate_window(win, surface.id().protocol_id());
    f.run();

    let id = f.testwl.last_created_surface_id().unwrap();
    let data = f.testwl.get_surface_data(id).unwrap();

    assert_eq!(data.toplevel().title, Some("window".into()));
    assert_eq!(data.toplevel().app_id, Some("class".into()));
}
/// See Pointer::handle_event for an explanation.
#[test]
fn popup_pointer_motion_workaround() {}
