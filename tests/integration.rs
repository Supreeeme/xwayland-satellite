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
use xwayland_satellite::xstate::WmSizeHintsFlags;

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

        assert!(ready, "connecting to xwayland failed");

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
        self.testwl.focus_toplevel(surface);
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
    fn wm_delete_window(
        &mut self,
        connection: &mut Connection,
        window: x::Window,
        surface: testwl::SurfaceId,
    ) {
        connection.set_property(
            window,
            x::ATOM_ATOM,
            connection.atoms.wm_protocols,
            &[connection.atoms.wm_delete_window],
        );
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
        net_active_window => b"_NET_ACTIVE_WINDOW",
        wm_delete_window => b"WM_DELETE_WINDOW",
        clipboard => b"CLIPBOARD",
        targets => b"TARGETS",
        multiple => b"MULTIPLE",
        wm_check => b"_NET_SUPPORTING_WM_CHECK",
        mime1 => b"text/plain" only_if_exists = false,
        mime2 => b"blah/blah" only_if_exists = false,
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

    #[track_caller]
    fn map_window(&self, window: x::Window) {
        self.send_and_check_request(&x::MapWindow { window })
            .unwrap();
    }

    #[track_caller]
    fn destroy_window(&self, window: x::Window) {
        self.send_and_check_request(&x::DestroyWindow { window })
            .unwrap();
    }

    #[track_caller]
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

    #[track_caller]
    fn get_reply<R: xcb::Request>(
        &self,
        req: &R,
    ) -> <R::Cookie as xcb::CookieWithReplyChecked>::Reply
    where
        R::Cookie: xcb::CookieWithReplyChecked,
    {
        self.wait_for_reply(self.send_request(req)).unwrap()
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
    connection.set_property(
        window,
        x::ATOM_STRING,
        x::ATOM_WM_NAME,
        c"window".to_bytes(),
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
        c"boink".to_bytes(),
    );
    connection.set_property(
        window,
        x::ATOM_WM_SIZE_HINTS,
        x::ATOM_WM_NORMAL_HINTS,
        &[flags, 1, 2, 3, 4, 25, 50, 150, 200],
    );
    f.wait_and_dispatch();
    let data = f.testwl.get_surface_data(surface).unwrap();
    let toplevel = data.toplevel().toplevel.clone();
    assert_eq!(data.toplevel().title, Some("bindow".into()));
    assert_eq!(data.toplevel().app_id, Some("boink".into()));
    assert_eq!(
        data.toplevel().min_size,
        Some(testwl::Vec2 { x: 25, y: 50 })
    );
    assert_eq!(
        data.toplevel().max_size,
        Some(testwl::Vec2 { x: 150, y: 200 })
    );

    f.wm_delete_window(&mut connection, window, surface);

    // Simulate killing client
    drop(connection);
    f.wait_and_dispatch();

    assert!(!toplevel.is_alive());
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
fn window_properties_after_reparent() {
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);

    let child = connection.new_window(connection.root, 0, 0, 1, 1, true);
    connection.map_window(child);
    f.wait_and_dispatch();
    let child_surface = f
        .testwl
        .last_created_surface_id()
        .expect("No surface created!");
    f.configure_and_verify_new_toplevel(&mut connection, child, child_surface);

    let other = connection.new_window(connection.root, 0, 0, 100, 100, false);
    connection.map_window(other);
    f.wait_and_dispatch();
    let other_surface = f
        .testwl
        .last_created_surface_id()
        .expect("No surface created!");
    f.configure_and_verify_new_toplevel(&mut connection, other, other_surface);

    connection.send_request(&x::UnmapWindow { window: child });
    let parent = connection.new_window(connection.root, 0, 0, 20, 20, false);

    connection.send_request(&x::ReparentWindow {
        window: child,
        parent,
        x: 0,
        y: 0,
    });

    // The server should get the notifications for these properties and shouldn't crash
    connection.set_property(
        child,
        x::ATOM_WM_SIZE_HINTS,
        x::ATOM_WM_NORMAL_HINTS,
        &[16u32, 0, 0, 0, 0, 200, 400],
    );
    connection.set_property(child, x::ATOM_STRING, x::ATOM_WM_NAME, b"title\0");
    connection.set_property(child, x::ATOM_STRING, x::ATOM_WM_CLASS, c"class".to_bytes());

    f.wait_and_dispatch();
}

#[test]
fn input_focus() {
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);

    let conn = std::cell::RefCell::new(&mut connection);
    let check_focus = |win: x::Window| {
        let connection = conn.borrow();
        let focus = connection
            .wait_for_reply(connection.send_request(&x::GetInputFocus {}))
            .unwrap()
            .focus();
        assert_eq!(win, focus);

        let reply = connection
            .wait_for_reply(connection.send_request(&x::GetProperty {
                delete: false,
                window: connection.root,
                property: connection.atoms.net_active_window,
                r#type: x::ATOM_WINDOW,
                long_offset: 0,
                long_length: 1,
            }))
            .unwrap();

        assert_eq!(&[win], reply.value::<x::Window>());
    };

    let mut create_win = || {
        let mut connection = conn.borrow_mut();
        let win = connection.new_window(connection.root, 0, 0, 20, 20, false);
        connection.map_window(win);
        f.wait_and_dispatch();
        let surface = f
            .testwl
            .last_created_surface_id()
            .expect("No surface created!");
        f.configure_and_verify_new_toplevel(&mut connection, win, surface);
        (win, surface)
    };

    let (win1, surface1) = create_win();
    check_focus(win1);
    let (win2, surface2) = create_win();
    check_focus(win2);

    f.testwl.focus_toplevel(surface1);
    // Seems the event doesn't get caught by wait_and_dispatch...
    std::thread::sleep(std::time::Duration::from_millis(10));
    check_focus(win1);

    f.testwl.unfocus_toplevel();
    std::thread::sleep(std::time::Duration::from_millis(10));
    check_focus(x::WINDOW_NONE);

    f.testwl.focus_toplevel(surface2);
    std::thread::sleep(std::time::Duration::from_millis(10));
    check_focus(win2);

    conn.borrow().destroy_window(win2);
    f.wait_and_dispatch();
    check_focus(x::WINDOW_NONE);

    f.wm_delete_window(&mut connection, win1, surface1);
}

#[test]
fn quick_delete() {
    let mut f = Fixture::new();
    let connection = Connection::new(&f.display);

    let window = connection.new_window(connection.root, 0, 0, 20, 20, false);
    connection.map_window(window);
    f.wait_and_dispatch();
    let surf = f
        .testwl
        .last_created_surface_id()
        .expect("No surface created");
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
    let flags = (WmSizeHintsFlags::ProgramMaxSize | WmSizeHintsFlags::ProgramMinSize).bits();
    connection.set_property(
        window,
        x::ATOM_WM_SIZE_HINTS,
        x::ATOM_WM_NORMAL_HINTS,
        &[flags, 1, 2, 3, 4, 25, 50, 150, 200],
    );
    connection
        .send_and_check_request(&x::ConfigureWindow {
            window,
            value_list: &[x::ConfigWindow::X(10), x::ConfigWindow::Y(40)],
        })
        .unwrap();
    f.testwl
        .configure_toplevel(surf, 100, 100, vec![xdg_toplevel::State::Activated]);
    connection.destroy_window(window);
    f.wait_and_dispatch();

    assert_eq!(f.testwl.get_surface_data(surf), None);
}

// aaaaaaaaaa
#[test]
fn copy_from_x11() {
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);

    let window = connection.new_window(connection.root, 0, 0, 20, 20, false);
    connection.map_window(window);
    f.wait_and_dispatch();
    let surface = f
        .testwl
        .last_created_surface_id()
        .expect("No surface created");
    f.configure_and_verify_new_toplevel(&mut connection, window, surface);

    // set data
    connection
        .send_and_check_request(&x::SetSelectionOwner {
            owner: window,
            selection: connection.atoms.clipboard,
            time: x::CURRENT_TIME,
        })
        .unwrap();
    let owner = connection
        .wait_for_reply(connection.send_request(&x::GetSelectionOwner {
            selection: connection.atoms.clipboard,
        }))
        .unwrap();
    assert_eq!(window, owner.owner());

    // wait for requests to come through
    std::thread::sleep(std::time::Duration::from_millis(100));
    let request = match connection.poll_for_event().unwrap() {
        Some(xcb::Event::X(x::Event::SelectionRequest(r))) => r,
        other => panic!("Didn't get selection request event, instead got {other:?}"),
    };

    assert_eq!(request.target(), connection.atoms.targets);
    connection.set_property(
        request.requestor(),
        x::ATOM_ATOM,
        request.property(),
        &[connection.atoms.mime1, connection.atoms.mime2],
    );
    connection
        .send_and_check_request(&x::SendEvent {
            propagate: false,
            destination: x::SendEventDest::Window(request.requestor()),
            event_mask: x::EventMask::empty(),
            event: &x::SelectionNotifyEvent::new(
                request.time(),
                request.requestor(),
                request.selection(),
                request.target(),
                request.property(),
            ),
        })
        .unwrap();

    connection.await_event();
    let mut mime_data = vec![
        (
            connection.atoms.mime1,
            x::ATOM_STRING,
            b"hello world".as_slice(),
        ),
        (connection.atoms.mime2, x::ATOM_INTEGER, &[1u8, 2, 3, 4]),
    ];

    while let Some(request) = connection.poll_for_event().unwrap() {
        let xcb::Event::X(x::Event::SelectionRequest(request)) = request else {
            continue;
        };

        let target = request.target();
        let Some(idx) = mime_data.iter().position(|(atom, _, _)| *atom == target) else {
            panic!("Expected atom in {mime_data:?}, got {target:?}");
        };

        let (_, ty, data) = mime_data.swap_remove(idx);
        connection.set_property(request.requestor(), ty, request.property(), data);

        connection
            .send_and_check_request(&x::SendEvent {
                propagate: false,
                destination: x::SendEventDest::Window(request.requestor()),
                event_mask: x::EventMask::empty(),
                event: &x::SelectionNotifyEvent::new(
                    request.time(),
                    request.requestor(),
                    request.selection(),
                    request.target(),
                    request.property(),
                ),
            })
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    assert!(
        mime_data.is_empty(),
        "Didn't get all mime types: {mime_data:?}"
    );
    f.wait_and_dispatch();

    let owner = connection
        .wait_for_reply(connection.send_request(&x::GetSelectionOwner {
            selection: connection.atoms.clipboard,
        }))
        .unwrap();
    assert_ne!(window, owner.owner());

    let mimes = f.testwl.data_source_mimes();
    assert!(
        mimes.contains(&"text/plain".into()),
        "text/plain not in mimes: {mimes:?}"
    ); // mime1
    assert!(
        mimes.contains(&"blah/blah".into()),
        "blah/blah not in mimes: {mimes:?}"
    ); // mime2

    let data = f.testwl.paste_data();
    f.testwl.dispatch();
    let data = data.resolve();
    for testwl::PasteData { mime_type, data } in data {
        match mime_type {
            x if x == "text/plain" => {
                assert_eq!(&data, b"hello world");
            }
            x if x == "blah/blah" => {
                assert_eq!(&data, &[1, 2, 3, 4]);
            }
            other => panic!("unexpected mime type: {other} ({data:?})"),
        }
    }
}

#[test]
fn copy_from_wayland() {
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);

    let window = connection.new_window(connection.root, 0, 0, 20, 20, false);
    connection.map_window(window);
    f.wait_and_dispatch();
    let surface = f
        .testwl
        .last_created_surface_id()
        .expect("No surface created");
    f.configure_and_verify_new_toplevel(&mut connection, window, surface);
    let offer = vec![
        testwl::PasteData {
            mime_type: "text/plain".into(),
            data: b"boingloings".to_vec(),
        },
        testwl::PasteData {
            mime_type: "yah/hah".into(),
            data: vec![1, 2, 3, 2, 1],
        },
    ];

    f.testwl.create_data_offer(offer.clone());

    let wm_window: x::Window = connection
        .get_reply(&x::GetProperty {
            delete: false,
            window: connection.root,
            property: connection.atoms.wm_check,
            r#type: x::ATOM_WINDOW,
            long_offset: 0,
            long_length: 1,
        })
        .value()[0];

    let reply = connection.get_reply(&x::GetSelectionOwner {
        selection: connection.atoms.clipboard,
    });
    assert_eq!(reply.owner(), wm_window);
    let dest1_atom = connection
        .get_reply(&x::InternAtom {
            name: b"dest1",
            only_if_exists: false,
        })
        .atom();

    // I don't know why, but omitting this little sleep prevents the SelectionRequest notification
    // from being sent, and I don't have the heart to determine why.
    std::thread::sleep(std::time::Duration::from_millis(1));
    connection
        .send_and_check_request(&x::ConvertSelection {
            requestor: window,
            selection: connection.atoms.clipboard,
            target: connection.atoms.targets,
            property: dest1_atom,
            time: x::CURRENT_TIME,
        })
        .unwrap();

    connection.await_event();
    let request = match connection.poll_for_event().unwrap() {
        Some(xcb::Event::X(x::Event::SelectionNotify(r))) => r,
        other => panic!("Didn't get selection notify event, instead got {other:?}"),
    };

    assert_eq!(request.requestor(), window);
    assert_eq!(request.selection(), connection.atoms.clipboard);
    assert_eq!(request.target(), connection.atoms.targets);
    assert_eq!(request.property(), dest1_atom);

    let reply = connection.get_reply(&x::GetProperty {
        delete: true,
        window,
        property: dest1_atom,
        r#type: x::ATOM_ATOM,
        long_offset: 0,
        long_length: 10,
    });
    let targets: &[x::Atom] = reply.value();
    assert_eq!(targets.len(), 2);

    for testwl::PasteData { mime_type, data } in offer {
        let atom = connection
            .get_reply(&x::InternAtom {
                only_if_exists: true,
                name: mime_type.as_bytes(),
            })
            .atom();
        assert_ne!(atom, x::ATOM_NONE);
        assert!(targets.contains(&atom));

        std::thread::sleep(std::time::Duration::from_millis(50));
        connection
            .send_and_check_request(&x::ConvertSelection {
                requestor: window,
                selection: connection.atoms.clipboard,
                target: atom,
                property: dest1_atom,
                time: x::CURRENT_TIME,
            })
            .unwrap();

        f.wait_and_dispatch();
        let request = match connection.poll_for_event().unwrap() {
            Some(xcb::Event::X(x::Event::SelectionNotify(r))) => r,
            other => panic!("Didn't get selection notify event, instead got {other:?}"),
        };

        assert_eq!(request.requestor(), window);
        assert_eq!(request.selection(), connection.atoms.clipboard);
        assert_eq!(request.target(), atom);
        assert_eq!(request.property(), dest1_atom);

        let val: Vec<u8> = connection
            .get_reply(&x::GetProperty {
                delete: true,
                window,
                property: dest1_atom,
                r#type: x::ATOM_ANY,
                long_offset: 0,
                long_length: 10,
            })
            .value()
            .to_vec();

        assert_eq!(val, data);
    }
}

// TODO: this test doesn't actually match real behavior for some reason...
#[test]
fn different_output_position() {
    let mut f = Fixture::new();
    //f.testwl.enable_xdg_output_manager();
    let mut connection = Connection::new(&f.display);

    let window = connection.new_window(connection.root, 0, 0, 200, 200, false);
    connection.map_window(window);
    f.wait_and_dispatch();
    let surface = f
        .testwl
        .last_created_surface_id()
        .expect("No surface created!");
    f.configure_and_verify_new_toplevel(&mut connection, window, surface);

    f.testwl.new_output(0, 0);
    f.wait_and_dispatch();
    let output = f.testwl.last_created_output();
    f.testwl.move_surface_to_output(surface, &output);
    f.testwl.move_pointer_to(surface, 10.0, 10.0);
    f.wait_and_dispatch();
    let reply = connection.get_reply(&x::QueryPointer { window });
    assert_eq!(reply.same_screen(), true);
    assert_eq!(reply.win_x(), 10);
    assert_eq!(reply.win_y(), 10);

    f.testwl.new_output(100, 0);
    f.wait_and_dispatch();
    let output = f.testwl.last_created_output();
    //f.testwl.move_xdg_output(&output, 100, 0);
    f.testwl.move_surface_to_output(surface, &output);
    f.testwl.move_pointer_to(surface, 150.0, 12.0);
    f.wait_and_dispatch();
    let reply = connection.get_reply(&x::QueryPointer { window });
    println!("reply: {reply:?}");
    assert_eq!(reply.same_screen(), true);
    assert_eq!(reply.win_x(), 150);
    assert_eq!(reply.win_y(), 12);
}

#[test]
fn bad_clipboard_data() {
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);
    let window = connection.new_window(connection.root, 0, 0, 20, 20, false);
    connection.map_window(window);
    f.wait_and_dispatch();
    let surface = f
        .testwl
        .last_created_surface_id()
        .expect("No surface created");
    f.configure_and_verify_new_toplevel(&mut connection, window, surface);

    connection
        .send_and_check_request(&x::SetSelectionOwner {
            owner: window,
            selection: connection.atoms.clipboard,
            time: x::CURRENT_TIME,
        })
        .unwrap();

    connection.await_event();
    let request = match connection.poll_for_event().unwrap() {
        Some(xcb::Event::X(x::Event::SelectionRequest(r))) => r,
        other => panic!("Didn't get selection request event, instead got {other:?}"),
    };
    assert_eq!(request.target(), connection.atoms.targets);
    connection.set_property(
        request.requestor(),
        x::ATOM_ATOM,
        request.property(),
        &[connection.atoms.mime2],
    );
    connection
        .send_and_check_request(&x::SendEvent {
            propagate: false,
            destination: x::SendEventDest::Window(request.requestor()),
            event_mask: x::EventMask::empty(),
            event: &x::SelectionNotifyEvent::new(
                request.time(),
                request.requestor(),
                request.selection(),
                request.target(),
                request.property(),
            ),
        })
        .unwrap();

    connection.await_event();
    let request = match connection.poll_for_event().unwrap() {
        Some(xcb::Event::X(x::Event::SelectionRequest(r))) => r,
        other => panic!("Didn't get selection request event, instead got {other:?}"),
    };
    assert_eq!(request.target(), connection.atoms.mime2);

    // Don't actually set any data as requested - just report success

    connection
        .send_and_check_request(&x::SendEvent {
            propagate: false,
            destination: x::SendEventDest::Window(request.requestor()),
            event_mask: x::EventMask::empty(),
            event: &x::SelectionNotifyEvent::new(
                request.time(),
                request.requestor(),
                request.selection(),
                request.target(),
                request.property(),
            ),
        })
        .unwrap();

    std::thread::sleep(std::time::Duration::from_millis(50));
    let owner = connection
        .wait_for_reply(connection.send_request(&x::GetSelectionOwner {
            selection: connection.atoms.clipboard,
        }))
        .unwrap();
    assert_ne!(window, owner.owner());

    connection
        .send_and_check_request(&x::ConvertSelection {
            requestor: window,
            selection: connection.atoms.clipboard,
            target: connection.atoms.mime2,
            property: connection.atoms.mime1,
            time: x::CURRENT_TIME,
        })
        .unwrap();

    connection.await_event();
    let mut e = None;
    while let Some(event) = connection.poll_for_event().unwrap() {
        if let xcb::Event::X(x::Event::SelectionNotify(event)) = event {
            e = Some(event);
            break;
        }
    }
    let e = e.expect("No selection notify event");
    assert_eq!(e.property(), x::ATOM_NONE);
}

// issue #42
#[test]
fn funny_window_title() {
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);
    let window = connection.new_window(connection.root, 0, 0, 20, 20, false);
    connection.set_property(window, x::ATOM_STRING, x::ATOM_WM_NAME, b"title\0\0\0\0");
    connection.map_window(window);
    f.wait_and_dispatch();

    let surface = f
        .testwl
        .last_created_surface_id()
        .expect("No surface created!");
    f.configure_and_verify_new_toplevel(&mut connection, window, surface);
    let data = f.testwl.get_surface_data(surface).unwrap();
    assert_eq!(data.toplevel().title, Some("title".into()));

    connection.set_property(
        window,
        x::ATOM_STRING,
        x::ATOM_WM_NAME,
        b"title\0irrelevantdata\0",
    );
    f.wait_and_dispatch();

    let data = f.testwl.get_surface_data(surface).unwrap();
    assert_eq!(data.toplevel().title, Some("title".into()));
}

#[test]
fn close_window() {
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);
    let window = connection.new_window(connection.root, 0, 0, 20, 20, false);
    connection.map_window(window);
    f.wait_and_dispatch();
    let surface = f
        .testwl
        .last_created_surface_id()
        .expect("No surface created");
    f.configure_and_verify_new_toplevel(&mut connection, window, surface);
    f.wm_delete_window(&mut connection, window, surface);

    connection
        .send_and_check_request(&x::DeleteProperty {
            window,
            property: connection.atoms.wm_protocols,
        })
        .unwrap();
    f.testwl.close_toplevel(surface);
    connection.await_event();
    f.wait_and_dispatch();

    // Connection should no longer work (KillClient)
    assert!(connection.poll_for_event().is_err());
}
