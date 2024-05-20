use rustix::event::{poll, PollFd, PollFlags};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, Once,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wayland_protocols::xdg::shell::server::xdg_toplevel;
use wayland_server::Resource;
use xcb::{x, Xid};
use xwayland_satellite as xwls;
use xwayland_satellite::xstate::{WmHintsFlags, WmSizeHintsFlags};

#[derive(Default)]
struct TestDataInner {
    server_created: AtomicBool,
    server_connected: AtomicBool,
    display: Mutex<Option<String>>,
    server: Mutex<Option<UnixStream>>,
}

#[derive(Default, Clone)]
struct TestData(Arc<TestDataInner>);

impl TestData {
    fn new(server: UnixStream) -> Self {
        Self(Arc::new(TestDataInner {
            server: Mutex::new(server.into()),
            ..Default::default()
        }))
    }
}

impl std::ops::Deref for TestData {
    type Target = Arc<TestDataInner>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl xwls::RunData for TestData {
    fn created_server(&self) {
        self.server_created.store(true, Ordering::Relaxed);
    }

    fn connected_server(&self) {
        self.server_connected.store(true, Ordering::Relaxed);
    }

    fn xwayland_ready(&self, display: String) {
        *self.display.lock().unwrap() = Some(display);
    }

    fn display(&self) -> Option<&str> {
        None
    }

    fn server(&self) -> Option<UnixStream> {
        let mut server = self.server.lock().unwrap();
        assert!(server.is_some());
        server.take()
    }
}

struct Fixture {
    testwl: testwl::Server,
    thread: ManuallyDrop<JoinHandle<Option<()>>>,
    pollfd: PollFd<'static>,
    display: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let thread = unsafe { ManuallyDrop::take(&mut self.thread) };
        if thread.is_finished() {
            thread.join().expect("Main thread panicked");
        }
    }
}

impl Fixture {
    fn new() -> Self {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            env_logger::builder()
                .is_test(true)
                .filter_level(log::LevelFilter::Debug)
                .parse_default_env()
                .init();
        });

        let (a, b) = UnixStream::pair().unwrap();
        let mut testwl = testwl::Server::new(false);
        testwl.connect(a);
        let our_data = TestData::new(b);
        let data = our_data.clone();
        let thread = std::thread::spawn(move || xwls::main(data));

        // wait for connection
        let fd = unsafe { BorrowedFd::borrow_raw(testwl.poll_fd().as_raw_fd()) };
        let pollfd = PollFd::from_borrowed_fd(fd, PollFlags::IN);
        assert!(poll(&mut [pollfd.clone()], 100).unwrap() > 0);
        testwl.dispatch();

        let try_bool_timeout = |b: &AtomicBool| {
            let timeout = Duration::from_secs(1);
            let mut res = b.load(Ordering::Relaxed);
            let start = Instant::now();
            while !res && start.elapsed() < timeout {
                res = b.load(Ordering::Relaxed);
            }

            res
        };
        assert!(
            try_bool_timeout(&our_data.server_created),
            "creating server"
        );
        assert!(
            try_bool_timeout(&our_data.server_connected),
            "connecting to server"
        );

        let mut f = [pollfd.clone()];
        let start = std::time::Instant::now();
        // Give Xwayland time to do its thing

        let mut ready = our_data.display.lock().unwrap().is_some();
        while !ready && start.elapsed() < Duration::from_millis(2000) {
            let n = poll(&mut f, 100).unwrap();
            if n > 0 {
                testwl.dispatch();
            }
            ready = our_data.display.lock().unwrap().is_some();
        }

        assert!(ready, "connecting to xwayland");

        let display = our_data.display.lock().unwrap().take().unwrap();
        Self {
            testwl,
            thread: ManuallyDrop::new(thread),
            pollfd,
            display,
        }
    }

    #[track_caller]
    fn wait_and_dispatch(&mut self) {
        let mut pollfd = [self.pollfd.clone()];
        assert!(
            poll(&mut pollfd, 50).unwrap() > 0,
            "Did not receive any events"
        );
        self.pollfd.clear_revents();
        self.testwl.dispatch();

        while poll(&mut pollfd, 50).unwrap() > 0 {
            self.testwl.dispatch();
            self.pollfd.clear_revents();
        }
    }

    fn configure_and_verify_new_toplevel(
        &mut self,
        connection: &mut Connection,
        window: x::Window,
        surface: testwl::SurfaceId,
    ) {
        let data = self.testwl.get_surface_data(surface).unwrap();
        assert!(
            matches!(data.role, Some(testwl::SurfaceRole::Toplevel(_))),
            "surface role was wrong: {:?}",
            data.role
        );

        self.testwl
            .configure_toplevel(surface, 100, 100, vec![xdg_toplevel::State::Activated]);
        self.wait_and_dispatch();
        let geometry = connection
            .wait_for_reply(connection.send_request(&x::GetGeometry {
                drawable: x::Drawable::Window(window),
            }))
            .unwrap();

        assert_eq!(geometry.x(), 0);
        assert_eq!(geometry.y(), 0);
        assert_eq!(geometry.width(), 100);
        assert_eq!(geometry.height(), 100);
    }

    /// Triggers a Wayland side toplevel Close event and processes the corresponding
    /// X11 side WM_DELETE_WINDOW client message
    fn close_toplevel(
        &mut self,
        connection: &mut Connection,
        window: x::Window,
        surface: testwl::SurfaceId,
    ) {
        self.testwl.close_toplevel(surface);
        connection.await_event();
        let event = connection
            .inner
            .poll_for_event()
            .unwrap()
            .expect("No close event");

        let xcb::Event::X(x::Event::ClientMessage(event)) = event else {
            panic!("Expected ClientMessage event, got {event:?}");
        };

        assert_eq!(event.window(), window);
        assert_eq!(event.format(), 32);
        assert_eq!(event.r#type(), connection.atoms.wm_protocols);
        match event.data() {
            x::ClientMessageData::Data32(d) => {
                assert_eq!(d[0], connection.atoms.wm_delete_window.resource_id())
            }
            other => panic!("wrong data type: {other:?}"),
        }
    }
}

xcb::atoms_struct! {
    struct Atoms {
        wm_protocols => b"WM_PROTOCOLS",
        wm_delete_window => b"WM_DELETE_WINDOW",
    }
}

struct Connection {
    inner: xcb::Connection,
    pollfd: PollFd<'static>,
    atoms: Atoms,
    root: x::Window,
    visual: u32,
}

impl std::ops::Deref for Connection {
    type Target = xcb::Connection;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Connection {
    fn new(display: &str) -> Self {
        let (inner, _) = xcb::Connection::connect(Some(display)).unwrap();
        let fd = unsafe { BorrowedFd::borrow_raw(inner.as_raw_fd()) };
        let pollfd = PollFd::from_borrowed_fd(fd, PollFlags::IN);
        let atoms = Atoms::intern_all(&inner).unwrap();
        let screen = inner.get_setup().roots().next().unwrap();
        let root = screen.root();
        let visual = screen.root_visual();

        Self {
            inner,
            pollfd,
            atoms,
            root,
            visual,
        }
    }

    fn new_window(
        &self,
        parent: x::Window,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        override_redirect: bool,
    ) -> x::Window {
        let wid = self.inner.generate_id();
        let req = x::CreateWindow {
            depth: 0,
            wid,
            parent,
            x,
            y,
            width,
            height,
            border_width: 0,
            class: x::WindowClass::InputOutput,
            visual: self.visual,
            value_list: &[x::Cw::OverrideRedirect(override_redirect)],
        };
        self.inner
            .send_and_check_request(&req)
            .expect("creating window failed");

        wid
    }

    fn map_window(&self, window: x::Window) {
        self.send_and_check_request(&x::MapWindow { window })
            .unwrap();
    }

    fn unmap_window(&self, window: x::Window) {
        self.send_and_check_request(&x::UnmapWindow { window })
            .unwrap();
    }

    fn set_property<P: x::PropEl>(
        &self,
        window: x::Window,
        r#type: x::Atom,
        property: x::Atom,
        data: &[P],
    ) {
        self.send_and_check_request(&x::ChangeProperty {
            mode: x::PropMode::Replace,
            window,
            r#type,
            property,
            data,
        })
        .unwrap();
    }

    #[track_caller]
    fn await_event(&mut self) {
        assert!(
            poll(&mut [self.pollfd.clone()], 100).expect("poll failed") > 0,
            "Did not get any X11 events"
        );
    }
}

#[test]
fn toplevel_flow() {
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);
    let window = connection.new_window(connection.root, 0, 0, 200, 200, false);

    // Pre-map properties
    connection.set_property(
        window,
        x::ATOM_STRING,
        x::ATOM_WM_NAME,
        c"window".to_bytes(),
    );
    connection.set_property(
        window,
        x::ATOM_STRING,
        x::ATOM_WM_CLASS,
        &[
            c"instance".to_bytes_with_nul(),
            c"class".to_bytes_with_nul(),
        ]
        .concat(),
    );

    let flags = (WmSizeHintsFlags::ProgramMaxSize | WmSizeHintsFlags::ProgramMinSize).bits();
    connection.set_property(
        window,
        x::ATOM_WM_SIZE_HINTS,
        x::ATOM_WM_NORMAL_HINTS,
        &[flags, 0, 0, 0, 0, 50, 100, 300, 400],
    );
    connection.map_window(window);
    f.wait_and_dispatch();

    let surface = f
        .testwl
        .last_created_surface_id()
        .expect("No surface created!");
    f.configure_and_verify_new_toplevel(&mut connection, window, surface);

    let data = f.testwl.get_surface_data(surface).unwrap();
    assert_eq!(data.toplevel().title, Some("window".into()));
    assert_eq!(data.toplevel().app_id, Some("class".into()));
    assert_eq!(
        data.toplevel().min_size,
        Some(testwl::Vec2 { x: 50, y: 100 })
    );
    assert_eq!(
        data.toplevel().max_size,
        Some(testwl::Vec2 { x: 300, y: 400 })
    );

    // Post map properties
    connection.set_property(
        window,
        x::ATOM_STRING,
        x::ATOM_WM_NAME,
        c"bindow".to_bytes(),
    );
    connection.set_property(
        window,
        x::ATOM_STRING,
        x::ATOM_WM_CLASS,
        &[c"f".to_bytes_with_nul(), c"ssalc".to_bytes_with_nul()].concat(),
    );
    connection.set_property(
        window,
        x::ATOM_WM_SIZE_HINTS,
        x::ATOM_WM_NORMAL_HINTS,
        &[flags, 1, 2, 3, 4, 25, 50, 150, 200],
    );
    f.wait_and_dispatch();
    let data = f.testwl.get_surface_data(surface).unwrap();
    assert_eq!(data.toplevel().title, Some("bindow".into()));
    assert_eq!(data.toplevel().app_id, Some("ssalc".into()));
    assert_eq!(
        data.toplevel().min_size,
        Some(testwl::Vec2 { x: 25, y: 50 })
    );
    assert_eq!(
        data.toplevel().max_size,
        Some(testwl::Vec2 { x: 150, y: 200 })
    );

    f.close_toplevel(&mut connection, window, surface);

    // Simulate killing client
    drop(connection);
    f.wait_and_dispatch();

    let data = f.testwl.get_surface_data(surface).expect("No surface data");
    assert!(!data.toplevel().toplevel.is_alive());
}

#[test]
fn reparent() {
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);

    let parent = connection.new_window(connection.root, 0, 0, 1, 1, false);
    let child = connection.new_window(parent, 0, 0, 20, 20, false);

    connection
        .send_and_check_request(&x::ReparentWindow {
            window: child,
            parent: connection.root,
            x: 0,
            y: 0,
        })
        .unwrap();

    connection.map_window(child);
    f.wait_and_dispatch();
    let surface = f
        .testwl
        .last_created_surface_id()
        .expect("No surface created!");
    f.configure_and_verify_new_toplevel(&mut connection, child, surface);
}

#[test]
fn input_focus() {
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);

    fn tst(
        f: &mut Fixture,
        connection: &mut Connection,
        set_props: impl FnOnce(&mut Connection, x::Window),
        check: impl FnOnce(/* win: */ x::Window, /* focus: */ x::Window),
    ) {
        let win = connection.new_window(connection.root, 0, 0, 20, 20, false);
        set_props(connection, win);
        connection.map_window(win);
        f.wait_and_dispatch();
        let surface = f
            .testwl
            .last_created_surface_id()
            .expect("No surface created!");
        f.configure_and_verify_new_toplevel(connection, win, surface);

        let focus = connection
            .wait_for_reply(connection.send_request(&x::GetInputFocus {}))
            .unwrap()
            .focus();
        check(win, focus);

        f.close_toplevel(connection, win, surface);
    }

    // Input field unset
    tst(
        &mut f,
        &mut connection,
        |_, _| {},
        |win, focus| assert_eq!(win, focus),
    );

    // Input field set to false
    tst(
        &mut f,
        &mut connection,
        |connection, win| {
            connection.set_property(
                win,
                x::ATOM_WM_HINTS,
                x::ATOM_WM_HINTS,
                &[WmHintsFlags::Input.bits(), 0],
            );
        },
        |win, focus| assert_ne!(win, focus),
    );

    // Input field set to true
    tst(
        &mut f,
        &mut connection,
        |connection, win| {
            connection.set_property(
                win,
                x::ATOM_WM_HINTS,
                x::ATOM_WM_HINTS,
                &[WmHintsFlags::Input.bits(), 1],
            );
        },
        |win, focus| assert_eq!(win, focus),
    );
}
