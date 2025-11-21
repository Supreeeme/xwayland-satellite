use rustix::event::{poll, PollFd, PollFlags};
use rustix::process::{Pid, Signal, WaitOptions};
use std::collections::HashMap;
use std::io::Write;
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, Once,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wayland_protocols::xdg::{
    decoration::zv1::server::zxdg_toplevel_decoration_v1, shell::server::xdg_toplevel,
};
use wayland_server::protocol::{wl_output, wl_pointer};
use wayland_server::Resource;
use xcb::{x, Xid};
use xwayland_satellite as xwls;
use xwayland_satellite::xstate::{MoveResizeDirection, WmSizeHintsFlags, WmState};
use xwls::timespec_from_millis;

#[derive(Default)]
struct TestDataInner {
    server_created: AtomicBool,
    server_connected: AtomicBool,
    display: Mutex<Option<String>>,
    server: Mutex<Option<UnixStream>>,
    pid: Mutex<Option<u32>>,
    quit_rx: Mutex<Option<UnixStream>>,
}

#[derive(Default, Clone)]
struct TestData(Arc<TestDataInner>);

impl TestData {
    fn new(server: UnixStream, quit_rx: UnixStream) -> Self {
        Self(Arc::new(TestDataInner {
            server: Mutex::new(server.into()),
            quit_rx: Mutex::new(Some(quit_rx)),
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

    fn quit_rx(&self) -> Option<UnixStream> {
        self.quit_rx.lock().unwrap().take()
    }

    fn xwayland_ready(&self, display: String, pid: u32) {
        *self.display.lock().unwrap() = Some(display);
        *self.pid.lock().unwrap() = Some(pid);
    }

    fn display(&self) -> Option<&str> {
        None
    }

    fn listenfds(&mut self) -> Vec<OwnedFd> {
        Vec::new()
    }

    fn server(&self) -> Option<UnixStream> {
        let mut server = self.server.lock().unwrap();
        assert!(server.is_some());
        server.take()
    }

    fn max_req_len_bytes(&self) -> Option<usize> {
        Some(500)
    }
}

struct Fixture {
    testwl: testwl::Server,
    thread: ManuallyDrop<JoinHandle<Option<()>>>,
    pollfd: PollFd<'static>,
    display: String,
    pid: Pid,
    quit_tx: UnixStream,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let thread = unsafe { ManuallyDrop::take(&mut self.thread) };
        // Sending anything to the quit receiver to stop the main loop. Then we guarantee a main
        // thread does not use file descriptors which outlive the Fixture's BorrowedFd
        let return_ptr = Box::into_raw(Box::new(0_usize)) as usize;
        // If the receiver end of the pipe closed, the main thread dropped it, which means that
        // thread already terminated
        if self
            .quit_tx
            .write_all(&return_ptr.to_ne_bytes())
            .is_err_and(|e| e.kind() != std::io::ErrorKind::BrokenPipe)
        {
            panic!("could not message the main thread to terminate");
        }
        if thread.join().is_err() {
            log::error!("main thread panicked");
        }
        if rustix::process::test_kill_process(self.pid).is_ok() {
            rustix::process::kill_process(self.pid, Signal::TERM).unwrap();
            rustix::process::waitpid(Some(self.pid), WaitOptions::NOHANG).unwrap();
        }
    }
}

impl Fixture {
    fn new_preset(pre_connect: impl FnOnce(&mut testwl::Server)) -> Self {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            pretty_env_logger::formatted_timed_builder()
                .is_test(true)
                .filter_level(log::LevelFilter::Debug)
                .parse_default_env()
                .init();
        });

        let (quit_tx, quit_rx) = UnixStream::pair().unwrap();

        let (a, b) = UnixStream::pair().unwrap();
        let mut testwl = testwl::Server::new(false);
        pre_connect(&mut testwl);
        testwl.connect(a);
        let our_data = TestData::new(b, quit_rx);
        let data = our_data.clone();
        let thread = std::thread::spawn(move || xwls::main(data));

        // wait for connection
        let fd = unsafe { BorrowedFd::borrow_raw(testwl.poll_fd().as_raw_fd()) };
        let pollfd = PollFd::from_borrowed_fd(fd, PollFlags::IN);
        let timeout = timespec_from_millis(1000);
        assert!(poll(&mut [pollfd.clone()], Some(&timeout)).unwrap() > 0);
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
            let timeout = timespec_from_millis(100);
            let n = poll(&mut f, Some(&timeout)).unwrap();
            if n > 0 {
                testwl.dispatch();
            }
            ready = our_data.display.lock().unwrap().is_some();
        }

        assert!(ready, "connecting to xwayland failed");

        let display = our_data.display.lock().unwrap().take().unwrap();
        let pid = our_data.pid.lock().unwrap().take().unwrap();
        Self {
            testwl,
            thread: ManuallyDrop::new(thread),
            pollfd,
            display,
            pid: Pid::from_raw(pid as _).expect("Xwayland PID was invalid?"),
            quit_tx,
        }
    }
    fn new() -> Self {
        Self::new_preset(|_| {})
    }

    #[track_caller]
    fn wait_and_dispatch(&mut self) {
        let mut pollfd = [self.pollfd.clone()];
        self.testwl.dispatch();
        let timeout = timespec_from_millis(50);
        assert!(
            poll(&mut pollfd, Some(&timeout)).unwrap() > 0,
            "Did not receive any events"
        );
        self.pollfd.clear_revents();
        self.testwl.dispatch();

        while poll(&mut pollfd, Some(&timeout)).unwrap() > 0 {
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
        let geometry = connection.get_reply(&x::GetGeometry {
            drawable: x::Drawable::Window(window),
        });

        assert_eq!(geometry.x(), 0);
        assert_eq!(geometry.y(), 0);
        assert_eq!(geometry.width(), 100);
        assert_eq!(geometry.height(), 100);
    }

    #[track_caller]
    fn map_as_toplevel(
        &mut self,
        connection: &mut Connection,
        window: x::Window,
    ) -> testwl::SurfaceId {
        connection.map_window(window);
        self.wait_and_dispatch();
        let surface = self
            .testwl
            .last_created_surface_id()
            .expect("No surface created");
        self.configure_and_verify_new_toplevel(connection, window, surface);
        surface
    }

    #[track_caller]
    fn map_as_popup(
        &mut self,
        connection: &mut Connection,
        window: x::Window,
    ) -> testwl::SurfaceId {
        connection.map_window(window);
        self.wait_and_dispatch();
        let surface = self
            .testwl
            .last_created_surface_id()
            .expect("No surface created");
        let data = self.testwl.get_surface_data(surface).unwrap();
        assert!(
            matches!(data.role, Some(testwl::SurfaceRole::Popup(_))),
            "surface role was wrong: {:?}",
            data.role
        );
        self.testwl.configure_popup(surface);
        self.wait_and_dispatch();
        surface
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

        let event = connection.await_event();
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

    fn create_output(&mut self, x: i32, y: i32) -> wayland_server::protocol::wl_output::WlOutput {
        self.testwl.new_output(x, y);
        self.wait_and_dispatch();
        self.testwl.last_created_output()
    }
}

xcb::atoms_struct! {
    struct Atoms {
        wm_protocols => b"WM_PROTOCOLS",
        net_active_window => b"_NET_ACTIVE_WINDOW",
        wm_delete_window => b"WM_DELETE_WINDOW",
        net_wm_state => b"_NET_WM_STATE",
        skip_taskbar => b"_NET_WM_STATE_SKIP_TASKBAR",
        transient_for => b"WM_TRANSIENT_FOR",
        clipboard => b"CLIPBOARD",
        primary => b"PRIMARY",
        targets => b"TARGETS",
        multiple => b"MULTIPLE",
        wm_state => b"WM_STATE",
        wm_check => b"_NET_SUPPORTING_WM_CHECK",
        win_type => b"_NET_WM_WINDOW_TYPE",
        win_type_normal => b"_NET_WM_WINDOW_TYPE_NORMAL",
        win_type_menu => b"_NET_WM_WINDOW_TYPE_MENU",
        win_type_popup_menu => b"_NET_WM_WINDOW_TYPE_POPUP_MENU",
        win_type_dropdown_menu => b"_NET_WM_WINDOW_TYPE_DROPDOWN_MENU",
        win_type_tooltip => b"_NET_WM_WINDOW_TYPE_TOOLTIP",
        win_type_dnd => b"_NET_WM_WINDOW_TYPE_DND",
        motif_wm_hints => b"_MOTIF_WM_HINTS" only_if_exists = false,
        mime1 => b"text/plain" only_if_exists = false,
        mime2 => b"blah/blah" only_if_exists = false,
        incr => b"INCR",
        xsettings => b"_XSETTINGS_S0",
        xsettings_setting => b"_XSETTINGS_SETTINGS",
        moveresize => b"_NET_WM_MOVERESIZE",
    }
}

struct Settings {
    serial: u32,
    data: HashMap<String, Setting>,
}
struct Setting {
    value: i32,
    last_change: u32,
}

struct Connection {
    inner: xcb::Connection,
    pollfd: PollFd<'static>,
    atoms: Atoms,
    root: x::Window,
    visual: u32,
    wm_window: x::Window,
}

impl std::ops::Deref for Connection {
    type Target = xcb::Connection;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Connection {
    fn new(display: &str) -> Self {
        let (inner, _) =
            xcb::Connection::connect_with_extensions(Some(display), &[xcb::Extension::XFixes], &[])
                .unwrap();
        // xfixes init
        let reply = inner
            .wait_for_reply(inner.send_request(&xcb::xfixes::QueryVersion {
                client_major_version: 1,
                client_minor_version: 0,
            }))
            .unwrap();
        assert_eq!(reply.major_version(), 1);

        let fd = unsafe { BorrowedFd::borrow_raw(inner.as_raw_fd()) };
        let pollfd = PollFd::from_borrowed_fd(fd, PollFlags::IN);
        let atoms = Atoms::intern_all(&inner).unwrap();
        let screen = inner.get_setup().roots().next().unwrap();
        let root = screen.root();
        let visual = screen.root_visual();

        let wm_window: x::Window = inner
            .wait_for_reply(inner.send_request(&x::GetProperty {
                delete: false,
                window: root,
                property: atoms.wm_check,
                r#type: x::ATOM_WINDOW,
                long_offset: 0,
                long_length: 1,
            }))
            .expect("Couldn't get WM window")
            .value()[0];

        Self {
            inner,
            pollfd,
            atoms,
            root,
            visual,
            wm_window,
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
    #[must_use]
    fn await_event(&mut self) -> xcb::Event {
        if let Some(event) = self.poll_for_event().expect("Failed to poll for event") {
            return event;
        }
        let timeout = timespec_from_millis(100);
        assert!(
            poll(&mut [self.pollfd.clone()], Some(&timeout)).expect("poll failed") > 0,
            "Did not get any X11 events"
        );
        self.pollfd.clear_revents();
        self.poll_for_event()
            .expect("Failed to poll for event after pollfd")
            .unwrap()
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

    #[track_caller]
    fn set_selection_owner(&self, window: x::Window, selection: x::Atom) {
        self.send_and_check_request(&x::SetSelectionOwner {
            owner: window,
            selection,
            time: x::CURRENT_TIME,
        })
        .unwrap();
        let owner = self.get_reply(&x::GetSelectionOwner { selection });

        assert_eq!(window, owner.owner(), "Unexpected selection owner");
    }

    #[track_caller]
    fn await_selection_request(&mut self) -> x::SelectionRequestEvent {
        match self.await_event() {
            xcb::Event::X(x::Event::SelectionRequest(r)) => r,
            other => panic!("Didn't get selection request event, instead got {other:?}"),
        }
    }

    #[track_caller]
    fn await_selection_notify(&mut self) -> x::SelectionNotifyEvent {
        match self.await_event() {
            xcb::Event::X(x::Event::SelectionNotify(r)) => r,
            other => panic!("Didn't get selection notify event, instead got {other:?}"),
        }
    }

    #[track_caller]
    fn send_selection_notify(&self, request: &x::SelectionRequestEvent) {
        self.send_and_check_request(&x::SendEvent {
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
    }

    #[track_caller]
    fn await_property_notify(&mut self) -> x::PropertyNotifyEvent {
        match self.await_event() {
            xcb::Event::X(x::Event::PropertyNotify(r)) => r,
            other => panic!("Didn't get property notify event, instead got {other:?}"),
        }
    }

    #[track_caller]
    fn get_property_change_events(&self, window: x::Window) {
        self.send_and_check_request(&x::ChangeWindowAttributes {
            window,
            value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
        })
        .unwrap();
    }

    #[track_caller]
    fn verify_clipboard_owner(&self, window: x::Window) {
        let owner = self.get_reply(&x::GetSelectionOwner {
            selection: self.atoms.clipboard,
        });
        assert_eq!(owner.owner(), window, "Clipboard owner does not match");
    }

    #[track_caller]
    fn await_selection_owner_change(&mut self) -> xcb::xfixes::SelectionNotifyEvent {
        match self.await_event() {
            xcb::Event::XFixes(xcb::xfixes::Event::SelectionNotify(e)) => e,
            other => panic!("Expected XFixes SelectionNotify, got {other:?}"),
        }
    }

    #[track_caller]
    fn get_selection_owner_change_events(&self, enable: bool, window: x::Window) {
        let event_mask = if enable {
            xcb::xfixes::SelectionEventMask::SET_SELECTION_OWNER
        } else {
            xcb::xfixes::SelectionEventMask::empty()
        };
        self.send_and_check_request(&xcb::xfixes::SelectSelectionInput {
            window,
            selection: self.atoms.clipboard,
            event_mask,
        })
        .unwrap();
    }

    #[track_caller]
    fn send_client_message(&self, request: &x::ClientMessageEvent) {
        self.send_and_check_request(&x::SendEvent {
            propagate: false,
            destination: x::SendEventDest::Window(self.root),
            event_mask: x::EventMask::SUBSTRUCTURE_NOTIFY | x::EventMask::SUBSTRUCTURE_REDIRECT,
            event: request,
        })
        .unwrap();
    }

    fn get_xsettings(&self) -> Settings {
        let owner = self
            .get_reply(&x::GetSelectionOwner {
                selection: self.atoms.xsettings,
            })
            .owner();

        let reply = self.get_reply(&x::GetProperty {
            delete: false,
            window: owner,
            property: self.atoms.xsettings_setting,
            r#type: self.atoms.xsettings_setting,
            long_offset: 0,
            long_length: 60,
        });
        assert_eq!(reply.r#type(), self.atoms.xsettings_setting);

        let data = reply.value::<u8>();

        let byte_order = data[0];
        assert_eq!(byte_order, 0);
        let serial = u32::from_le_bytes(data[4..8].try_into().unwrap());
        let num_settings = u32::from_le_bytes(data[8..12].try_into().unwrap());

        let mut current_idx = 12;
        let mut settings = HashMap::new();
        for _ in 0..num_settings {
            assert_eq!(&data[current_idx..current_idx + 2], &[0, 0]);
            let name_len =
                u16::from_le_bytes(data[current_idx + 2..current_idx + 4].try_into().unwrap());

            let padding_start = current_idx + 4 + name_len as usize;
            let name = String::from_utf8(data[current_idx + 4..padding_start].to_vec()).unwrap();
            let num_padding_bytes = (4 - (name_len as usize % 4)) % 4;
            let data_start = padding_start + num_padding_bytes;
            let last_change =
                u32::from_le_bytes(data[data_start..data_start + 4].try_into().unwrap());
            let value =
                i32::from_le_bytes(data[data_start + 4..data_start + 8].try_into().unwrap());

            settings.insert(name, Setting { value, last_change });
            current_idx = data_start + 8;
        }

        Settings {
            serial,
            data: settings,
        }
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

    let wm_state = connection.atoms.wm_state;

    let conn = std::cell::RefCell::new(&mut connection);
    let check_focus = |win: x::Window| {
        let connection = conn.borrow();
        let focus = connection.get_reply(&x::GetInputFocus {}).focus();
        assert_eq!(win, focus);

        let reply = connection.get_reply(&x::GetProperty {
            delete: false,
            window: connection.root,
            property: connection.atoms.net_active_window,
            r#type: x::ATOM_WINDOW,
            long_offset: 0,
            long_length: 1,
        });

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

    // Simulate exclusive fullscreen clients that set window state to mimized on focus loss
    conn.borrow()
        .set_property(win1, wm_state, wm_state, &[WmState::Iconic as u32, 0]);

    f.testwl.focus_toplevel(surface1);
    // Seems the event doesn't get caught by wait_and_dispatch...
    std::thread::sleep(std::time::Duration::from_millis(10));
    check_focus(win1);
    assert_eq!(
        conn.borrow()
            .get_reply(&x::GetProperty {
                delete: false,
                window: win1,
                property: wm_state,
                r#type: wm_state,
                long_offset: 0,
                long_length: 1,
            })
            .value::<u32>()
            .first()
            .and_then(|state| WmState::try_from(*state).ok()),
        Some(WmState::Normal)
    );

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
fn activation_x11_to_x11() {
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);

    let window1 = connection.new_window(connection.root, 0, 0, 20, 20, false);
    let surface1 = f.map_as_toplevel(&mut connection, window1);
    let window2 = connection.new_window(connection.root, 0, 0, 20, 20, false);
    let surface2 = f.map_as_toplevel(&mut connection, window2);

    f.testwl.focus_toplevel(surface2);
    std::thread::sleep(Duration::from_millis(10));
    connection.send_client_message(&x::ClientMessageEvent::new(
        window1,
        connection.atoms.net_active_window,
        x::ClientMessageData::Data32([2, x::CURRENT_TIME, 0, 0, 0]),
    ));
    f.wait_and_dispatch();

    assert_eq!(f.testwl.get_focused(), Some(surface1));
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

#[test]
fn copy_from_x11() {
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);

    let window = connection.new_window(connection.root, 0, 0, 20, 20, false);
    f.map_as_toplevel(&mut connection, window);

    connection.set_selection_owner(window, connection.atoms.primary);
    connection.set_selection_owner(window, connection.atoms.clipboard);

    for _ in [connection.atoms.primary, connection.atoms.clipboard] {
        let request = connection.await_selection_request();
        assert_eq!(request.target(), connection.atoms.targets);
        connection.set_property(
            request.requestor(),
            x::ATOM_ATOM,
            request.property(),
            &[connection.atoms.mime1, connection.atoms.mime2],
        );
        connection.send_selection_notify(&request);
    }
    f.wait_and_dispatch();

    struct MimeData {
        mime: x::Atom,
        data: testwl::PasteData,
    }
    let mimes_truth = [
        MimeData {
            mime: connection.atoms.mime1,
            data: testwl::PasteData {
                mime_type: "text/plain".to_string(),
                data: b"hello world".to_vec(),
            },
        },
        MimeData {
            mime: connection.atoms.mime2,
            data: testwl::PasteData {
                mime_type: "blah/blah".to_string(),
                data: vec![1, 2, 3, 4],
            },
        },
    ];

    // When requesting both primary and clipboard simultaneously, the first to be requested erases
    // its TARGETS (which is the correct behavior of a GetProperty request), but the second still
    // tried to use this data and would come up empty.
    let primary_mimes = f.testwl.primary_source_mimes();
    let clipboard_mimes = f.testwl.data_source_mimes();
    assert_eq!(
        primary_mimes.len(),
        mimes_truth.len(),
        "Wrong number of advertised primary mimes: {primary_mimes:?}"
    );
    assert_eq!(
        clipboard_mimes.len(),
        mimes_truth.len(),
        "Wrong number of advertised clipboard mimes: {clipboard_mimes:?}"
    );
    for MimeData { data, .. } in &mimes_truth {
        assert!(
            primary_mimes.contains(&data.mime_type),
            "Missing mime type {}",
            data.mime_type
        );
        assert!(
            clipboard_mimes.contains(&data.mime_type),
            "Missing mime type {}",
            data.mime_type
        );
    }

    // Type annotations hint the compiler to use HRTBs (needed since this closure is reused).
    // See: https://users.rust-lang.org/t/implementation-of-fnonce-is-not-general-enough/68294/3
    let mut send_data_for_mime = |mime: &str, _: &mut testwl::Server| {
        let request = connection.await_selection_request();
        let data = mimes_truth
            .iter()
            .find(|data| data.data.mime_type == mime)
            .unwrap_or_else(|| panic!("Asked for unknown mime: {mime}"));
        connection.set_property(
            request.requestor(),
            data.mime,
            request.property(),
            &data.data.data,
        );
        connection.send_selection_notify(&request);
        true
    };

    let clipboard_data = f.testwl.clipboard_paste_data(&mut send_data_for_mime);
    let primary_data = f.testwl.primary_paste_data(&mut send_data_for_mime);

    for data in [primary_data, clipboard_data] {
        let mut found_mimes = Vec::new();
        for testwl::PasteData { mime_type, data } in data {
            match &mime_type {
                x if x == "text/plain" => {
                    assert_eq!(&data, b"hello world");
                }
                x if x == "blah/blah" => {
                    assert_eq!(&data, &[1, 2, 3, 4]);
                }
                other => panic!("unexpected mime type: {other} ({data:?})"),
            }
            found_mimes.push(mime_type);
        }

        assert!(
            found_mimes.contains(&"text/plain".to_string()),
            "Didn't get mime data for text/plain"
        );
        assert!(
            found_mimes.contains(&"blah/blah".to_string()),
            "Didn't get mime data for blah/blah"
        );
    }
}

#[test]
fn copy_from_wayland() {
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);

    let window = connection.new_window(connection.root, 0, 0, 20, 20, false);
    connection.get_selection_owner_change_events(true, window);
    f.map_as_toplevel(&mut connection, window);
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
    connection.await_selection_owner_change();
    connection.verify_clipboard_owner(connection.wm_window);
    connection.get_selection_owner_change_events(false, window);

    let dest1_atom = connection
        .get_reply(&x::InternAtom {
            name: b"dest1",
            only_if_exists: false,
        })
        .atom();

    connection
        .send_and_check_request(&x::ConvertSelection {
            requestor: window,
            selection: connection.atoms.clipboard,
            target: connection.atoms.targets,
            property: dest1_atom,
            time: x::CURRENT_TIME,
        })
        .unwrap();

    let request = connection.await_selection_notify();
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
        let request = connection.await_selection_notify();
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

#[test]
fn incr_copy_from_wayland() {
    const BYTES: usize = 3000;
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);

    let window = connection.new_window(connection.root, 0, 0, 20, 20, false);
    connection.get_selection_owner_change_events(true, window);
    f.map_as_toplevel(&mut connection, window);
    let mut offer = vec![testwl::PasteData {
        mime_type: "text/plain".into(),
        data: std::iter::successors(Some(0u8), |n| Some(n.wrapping_add(1)))
            .take(BYTES)
            .collect(),
    }];

    f.testwl.create_data_offer(offer.clone());
    connection.await_selection_owner_change();
    connection.verify_clipboard_owner(connection.wm_window);
    connection.get_selection_owner_change_events(false, window);

    let dest1_atom = connection
        .get_reply(&x::InternAtom {
            name: b"dest1",
            only_if_exists: false,
        })
        .atom();

    connection
        .send_and_check_request(&x::ConvertSelection {
            requestor: window,
            selection: connection.atoms.clipboard,
            target: connection.atoms.targets,
            property: dest1_atom,
            time: x::CURRENT_TIME,
        })
        .unwrap();

    let request = connection.await_selection_notify();
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
    assert_eq!(targets.len(), 1);

    let offer_data = offer.pop().unwrap();
    let (mime_type, data) = (offer_data.mime_type, offer_data.data);
    let atom = connection
        .get_reply(&x::InternAtom {
            only_if_exists: true,
            name: mime_type.as_bytes(),
        })
        .atom();
    assert_ne!(atom, x::ATOM_NONE);
    assert!(targets.contains(&atom));

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
    let request = connection.await_selection_notify();
    assert_eq!(request.requestor(), window);
    assert_eq!(request.selection(), connection.atoms.clipboard);
    assert_eq!(request.target(), atom);
    assert_eq!(request.property(), dest1_atom);

    connection.get_property_change_events(window);
    let reply = connection.get_reply(&x::GetProperty {
        delete: true,
        window,
        property: dest1_atom,
        r#type: x::ATOM_ANY,
        long_offset: 0,
        long_length: (BYTES / 4 + BYTES % 4) as u32,
    });
    assert_eq!(reply.r#type(), connection.atoms.incr);
    assert_eq!(reply.value::<u32>().len(), 1);
    assert!(reply.value::<u32>()[0] <= data.len() as u32);

    let delete_property = connection.await_property_notify();
    assert_eq!(delete_property.state(), x::Property::Delete);
    assert_eq!(delete_property.atom(), dest1_atom);

    for (idx, chunk) in data.chunks(500).chain([]).enumerate() {
        let new_property = connection.await_property_notify();
        assert_eq!(new_property.state(), x::Property::NewValue, "chunk {idx}");
        assert_eq!(new_property.atom(), dest1_atom, "chunk {idx}");

        let incr_reply = connection.get_reply(&x::GetProperty {
            delete: true,
            window,
            property: dest1_atom,
            r#type: x::ATOM_ANY,
            long_offset: 0,
            long_length: (BYTES / 4 + BYTES % 4) as u32,
        });
        assert_eq!(incr_reply.r#type(), atom, "chunk {idx}");
        assert_eq!(incr_reply.value::<u8>(), chunk, "chunk {idx}");

        let delete_property = connection.await_property_notify();
        assert_eq!(delete_property.state(), x::Property::Delete, "chunk {idx}");
        assert_eq!(delete_property.atom(), dest1_atom, "chunk {idx}");
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

    let output = f.create_output(0, 0);
    f.testwl.move_surface_to_output(surface, &output);
    f.testwl.move_pointer_to(surface, 10.0, 10.0);
    f.wait_and_dispatch();
    let reply = connection.get_reply(&x::QueryPointer { window });
    assert!(reply.same_screen());
    assert_eq!(reply.win_x(), 10);
    assert_eq!(reply.win_y(), 10);

    let output = f.create_output(100, 0);
    //f.testwl.move_xdg_output(&output, 100, 0);
    f.testwl.move_surface_to_output(surface, &output);
    f.testwl.move_pointer_to(surface, 150.0, 12.0);
    f.wait_and_dispatch();
    let reply = connection.get_reply(&x::QueryPointer { window });
    assert!(reply.same_screen());
    assert_eq!(reply.win_x(), 150);
    assert_eq!(reply.win_y(), 12);
}

#[test]
fn bad_clipboard_data() {
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);
    let window = connection.new_window(connection.root, 0, 0, 20, 20, false);
    f.map_as_toplevel(&mut connection, window);
    connection.set_selection_owner(window, connection.atoms.clipboard);

    let request = connection.await_selection_request();
    assert_eq!(request.target(), connection.atoms.targets);
    connection.set_property(
        request.requestor(),
        x::ATOM_ATOM,
        request.property(),
        &[connection.atoms.mime2],
    );
    connection.send_selection_notify(&request);

    f.wait_and_dispatch();
    let mut data = f.testwl.clipboard_paste_data(|_, _| {
        let request = connection.await_selection_request();
        assert_eq!(request.target(), connection.atoms.mime2);
        // Don't actually set any data as requested - just report success
        connection.send_selection_notify(&request);
        true
    });

    connection.verify_clipboard_owner(window);
    assert_eq!(data.len(), 1, "Unexpected data: {data:?}");
    let data = data.pop().unwrap();
    assert_eq!(data.mime_type, "blah/blah");
    assert!(data.data.is_empty(), "Unexpected data: {:?}", data.data);
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
    f.wait_and_dispatch();

    // Connection should no longer work (KillClient)
    assert!(connection.poll_for_event().is_err());
}

// TODO: figure out if the sleeps in this test can be dealt with...
#[test]
fn primary_output() {
    let mut f = Fixture::new_preset(|testwl| {
        testwl.new_output(0, 0); // WL-1
        testwl.new_output(500, 500); // WL-2
    });
    let mut conn = Connection::new(&f.display);

    let reply = conn.get_reply(&xcb::randr::GetScreenResources { window: conn.root });
    let config_timestamp = reply.config_timestamp();
    let mut it = reply.outputs().iter().copied().map(|output| {
        let reply = conn.get_reply(&xcb::randr::GetOutputInfo {
            output,
            config_timestamp,
        });
        let name = std::str::from_utf8(reply.name()).unwrap();
        (
            f.testwl
                .get_output(name)
                .unwrap_or_else(|| panic!("Couldn't find output {name}")),
            output,
        )
    });
    let (wl_output1, output1) = it.next().expect("Couldn't find first output");
    let (wl_output2, output2) = it.next().expect("Couldn't find second output");
    assert_eq!(it.collect::<Vec<_>>(), vec![]);

    let window1 = conn.new_window(conn.root, 0, 0, 20, 20, false);
    let surface1 = f.map_as_toplevel(&mut conn, window1);
    f.testwl.move_surface_to_output(surface1, &wl_output1);
    let window2 = conn.new_window(conn.root, 0, 0, 20, 20, false);
    std::thread::sleep(std::time::Duration::from_millis(10));
    let surface2 = f.map_as_toplevel(&mut conn, window2);
    f.testwl.move_surface_to_output(surface2, &wl_output2);
    assert_ne!(surface1, surface2);
    f.wait_and_dispatch();

    f.testwl.focus_toplevel(surface1);
    std::thread::sleep(std::time::Duration::from_millis(10));
    let reply = conn.get_reply(&xcb::randr::GetOutputPrimary { window: conn.root });
    assert_eq!(reply.output(), output1);

    f.testwl.focus_toplevel(surface2);
    std::thread::sleep(std::time::Duration::from_millis(10));
    let reply = conn.get_reply(&xcb::randr::GetOutputPrimary { window: conn.root });
    assert_eq!(reply.output(), output2);

    let wl_output3 = f.create_output(24, 46);
    f.testwl.move_surface_to_output(surface2, &wl_output3);
    std::thread::sleep(std::time::Duration::from_millis(10));

    let reply = conn.get_reply(&xcb::randr::GetScreenResources { window: conn.root });
    assert_eq!(reply.outputs().len(), 3);
    let output3 = reply
        .outputs()
        .iter()
        .copied()
        .find(|o| ![output1, output2].contains(o))
        .unwrap();
    let reply = conn.get_reply(&xcb::randr::GetOutputPrimary { window: conn.root });
    assert_eq!(reply.output(), output3);
}

#[test]
fn incr_copy_from_x11() {
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);
    let window = connection.new_window(connection.root, 0, 0, 20, 20, false);
    f.map_as_toplevel(&mut connection, window);

    connection.set_selection_owner(window, connection.atoms.clipboard);
    let request = connection.await_selection_request();
    assert_eq!(request.target(), connection.atoms.targets);
    assert_eq!(request.selection(), connection.atoms.clipboard);
    connection.set_property(
        request.requestor(),
        x::ATOM_ATOM,
        request.property(),
        &[connection.atoms.targets, connection.atoms.mime1],
    );
    connection.send_selection_notify(&request);
    f.wait_and_dispatch();

    // Also give the window the primary selection.
    // Due to a bug introduced in primary selection support, `XState::selection_state` having both
    // a primary and clipboard X selection prevented clipboard INCR checks from occuring.
    connection.set_selection_owner(window, connection.atoms.primary);
    let request = connection.await_selection_request();
    assert_eq!(request.target(), connection.atoms.targets);
    assert_eq!(request.selection(), connection.atoms.primary);
    connection.set_property(
        request.requestor(),
        x::ATOM_ATOM,
        request.property(),
        &[connection.atoms.targets, connection.atoms.mime2],
    );
    connection.send_selection_notify(&request);
    f.wait_and_dispatch();

    let mut destination_property = x::Atom::none();
    let mut begin_incr = Some(|connection: &mut Connection| {
        let request = connection.await_selection_request();
        assert_eq!(request.target(), connection.atoms.mime1);

        connection.get_property_change_events(request.requestor());
        connection.set_property(
            request.requestor(),
            connection.atoms.incr,
            request.property(),
            &[3000u32],
        );
        connection.send_selection_notify(&request);
        // skip NewValue
        let notify = connection.await_property_notify();
        assert_eq!(notify.atom(), request.property());
        assert_eq!(notify.state(), x::Property::NewValue);
        request.property()
    });

    let data: Vec<u8> = std::iter::successors(Some(1u8), |n| Some(n.wrapping_add(1)))
        .take(3000)
        .collect();
    let mut it = data.chunks(500).enumerate();
    let mut paste_data = f.testwl.clipboard_paste_data(|_, testwl| {
        if let Some(begin) = begin_incr.take() {
            destination_property = begin(&mut connection);
            testwl.dispatch();
            return false;
        }
        assert_ne!(destination_property, x::Atom::none());

        let notify = connection.await_property_notify();
        match it.next() {
            Some((idx, chunk)) => {
                assert_eq!(notify.atom(), destination_property, "chunk {idx}");
                assert_eq!(notify.state(), x::Property::Delete, "chunk {idx}");
                connection.set_property(
                    notify.window(),
                    connection.atoms.mime1,
                    destination_property,
                    chunk,
                );
                testwl.dispatch();
                // skip NewValue
                let notify = connection.await_property_notify();
                assert_eq!(notify.atom(), destination_property, "chunk {idx}");
                assert_eq!(notify.state(), x::Property::NewValue, "chunk {idx}");
                false
            }
            None => {
                // INCR completed!
                assert_eq!(notify.atom(), destination_property);
                assert_eq!(notify.state(), x::Property::Delete);
                connection.set_property::<u8>(
                    notify.window(),
                    connection.atoms.mime1,
                    destination_property,
                    &[],
                );
                true
            }
        }
    });

    assert_eq!(f.testwl.data_source_mimes(), vec!["text/plain"]);
    assert_eq!(paste_data.len(), 1);
    let paste_data = paste_data.swap_remove(0);
    assert_eq!(paste_data.mime_type, "text/plain");
    assert_eq!(&paste_data.data, &data);
}

#[test]
fn wayland_then_x11_clipboard_owner() {
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);
    let window = connection.new_window(connection.root, 0, 0, 20, 20, false);
    connection.get_selection_owner_change_events(true, window);

    f.map_as_toplevel(&mut connection, window);
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

    connection.await_selection_owner_change();
    connection.verify_clipboard_owner(connection.wm_window);
    connection.get_selection_owner_change_events(false, window);

    connection.set_selection_owner(window, connection.atoms.clipboard);
    f.testwl.dispatch();
    connection.verify_clipboard_owner(window);

    let request = connection.await_selection_request();
    assert_eq!(request.selection(), connection.atoms.clipboard);
    assert_eq!(request.target(), connection.atoms.targets);
}

#[test]
fn fake_selection_targets() {
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);
    let window = connection.new_window(connection.root, 0, 0, 20, 20, false);
    connection.get_selection_owner_change_events(true, window);

    let data = b"boingloings";
    f.map_as_toplevel(&mut connection, window);
    let offer = vec![testwl::PasteData {
        mime_type: "text/plain;charset=utf-8".into(),
        data: data.to_vec(),
    }];
    f.testwl.create_data_offer(offer.clone());

    connection.await_selection_owner_change();
    connection.verify_clipboard_owner(connection.wm_window);
    connection.get_selection_owner_change_events(false, window);

    let utf8_string = connection
        .get_reply(&x::InternAtom {
            only_if_exists: false,
            name: b"UTF8_STRING",
        })
        .atom();

    connection
        .send_and_check_request(&x::ConvertSelection {
            requestor: window,
            selection: connection.atoms.clipboard,
            target: utf8_string,
            property: utf8_string,
            time: x::CURRENT_TIME,
        })
        .unwrap();

    f.wait_and_dispatch();
    let notify = connection.await_selection_notify();
    assert_eq!(notify.property(), utf8_string, "ConvertSelection failed");

    let reply = connection.get_reply(&x::GetProperty {
        delete: false,
        window,
        property: utf8_string,
        r#type: utf8_string,
        long_offset: 0,
        long_length: data.len() as u32,
    });
    let paste_data: &[u8] = reply.value();

    assert_eq!(
        std::str::from_utf8(paste_data).unwrap(),
        std::str::from_utf8(data).unwrap()
    );
}

#[test]
fn popup_done() {
    let mut f = Fixture::new();
    let mut conn = Connection::new(&f.display);
    let toplevel = conn.new_window(conn.root, 0, 0, 20, 20, false);
    f.map_as_toplevel(&mut conn, toplevel);

    let popup = conn.new_window(conn.root, 0, 0, 20, 20, true);
    let surface = f.map_as_popup(&mut conn, popup);
    let geometry = conn.get_reply(&x::GetGeometry {
        drawable: x::Drawable::Window(popup),
    });

    assert_eq!(geometry.x(), 0);
    assert_eq!(geometry.y(), 0);
    assert_eq!(geometry.width(), 20);
    assert_eq!(geometry.height(), 20);

    f.testwl.popup_done(surface);
    f.wait_and_dispatch();

    let reply = conn
        .wait_for_reply(conn.send_request(&x::GetWindowAttributes { window: popup }))
        .expect("Couldn't get window attributes");

    assert_eq!(reply.map_state(), x::MapState::Unmapped);
}

#[test]
fn negative_output_coordinates() {
    let mut f = Fixture::new();
    let output = f.create_output(-500, -500);
    let mut connection = Connection::new(&f.display);

    let window = connection.new_window(connection.root, 0, 0, 200, 200, false);
    let surface = f.map_as_toplevel(&mut connection, window);
    f.testwl.move_surface_to_output(surface, &output);
    f.testwl.move_pointer_to(surface, 30.0, 40.0);
    f.wait_and_dispatch();

    let tree = connection.get_reply(&x::QueryTree { window });
    let geo = connection.get_reply(&x::GetGeometry {
        drawable: x::Drawable::Window(window),
    });
    let reply = connection.get_reply(&x::TranslateCoordinates {
        src_window: tree.parent(),
        dst_window: connection.root,
        src_x: geo.x(),
        src_y: geo.y(),
    });

    assert!(reply.same_screen());
    assert_eq!(reply.dst_x(), 0);
    assert_eq!(reply.dst_y(), 0);

    let ptr_reply = connection.get_reply(&x::QueryPointer {
        window: connection.root,
    });
    assert!(ptr_reply.same_screen());
    assert_eq!(ptr_reply.child(), window);
    assert_eq!(ptr_reply.win_x(), 30);
    assert_eq!(ptr_reply.win_y(), 40);
}

#[test]
fn xdg_decorations() {
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);

    let window = connection.new_window(connection.root, 0, 0, 20, 20, false);
    let surface = f.map_as_toplevel(&mut connection, window);
    let data = f.testwl.get_surface_data(surface).unwrap();
    // The default decoration mode in x11 is SSD
    assert_eq!(
        data.toplevel()
            .decoration
            .as_ref()
            .and_then(|(_, decoration)| *decoration),
        Some(zxdg_toplevel_decoration_v1::Mode::ServerSide)
    );

    // CSD
    connection.set_property(
        window,
        connection.atoms.motif_wm_hints,
        connection.atoms.motif_wm_hints,
        &[2u32, 0, 0, 0, 0],
    );
    f.wait_and_dispatch();
    let data = f.testwl.get_surface_data(surface).unwrap();
    assert_eq!(
        data.toplevel()
            .decoration
            .as_ref()
            .and_then(|(_, decoration)| *decoration),
        Some(zxdg_toplevel_decoration_v1::Mode::ClientSide)
    );

    // SSD
    connection.set_property(
        window,
        connection.atoms.motif_wm_hints,
        connection.atoms.motif_wm_hints,
        &[2u32, 0, 1, 0, 0],
    );
    f.wait_and_dispatch();
    let data = f.testwl.get_surface_data(surface).unwrap();
    assert_eq!(
        data.toplevel()
            .decoration
            .as_ref()
            .and_then(|(_, decoration)| *decoration),
        Some(zxdg_toplevel_decoration_v1::Mode::ServerSide)
    );
}

#[test]
fn forced_1x_scale_consistent_x11_size() {
    let mut f = Fixture::new();
    f.testwl.enable_xdg_output_manager();
    let output = f.create_output(0, 0);
    output.scale(2);
    output.done();

    let mut conn = Connection::new(&f.display);
    let window = conn.new_window(conn.root, 0, 0, 200, 200, false);
    let surface = f.map_as_toplevel(&mut conn, window);
    f.testwl.move_surface_to_output(surface, &output);
    f.testwl.move_pointer_to(surface, 30.0, 40.0);
    f.wait_and_dispatch();

    let tree = conn.get_reply(&x::QueryTree { window });
    let geo = conn.get_reply(&x::GetGeometry {
        drawable: x::Drawable::Window(window),
    });
    let reply = conn.get_reply(&x::TranslateCoordinates {
        src_window: tree.parent(),
        dst_window: conn.root,
        src_x: geo.x(),
        src_y: geo.y(),
    });

    assert!(reply.same_screen());
    assert_eq!(reply.dst_x(), 0);
    assert_eq!(reply.dst_y(), 0);

    let ptr_reply = conn.get_reply(&x::QueryPointer { window: conn.root });
    assert!(ptr_reply.same_screen());
    assert_eq!(ptr_reply.child(), window);
    assert_eq!(ptr_reply.win_x(), 60);
    assert_eq!(ptr_reply.win_y(), 80);

    // Update scale
    output.scale(3);
    output.done();
    f.wait_and_dispatch();

    f.testwl
        .configure_toplevel(surface, 100, 100, vec![xdg_toplevel::State::Activated]);
    f.testwl.focus_toplevel(surface);
    f.testwl.move_pointer_to(surface, 30.0, 40.0);
    f.wait_and_dispatch();

    let ptr_reply = conn.get_reply(&x::QueryPointer { window: conn.root });
    assert!(ptr_reply.same_screen());
    assert_eq!(ptr_reply.child(), window);
    assert_eq!(ptr_reply.win_x(), 90);
    assert_eq!(ptr_reply.win_y(), 120);

    // Popup
    let popup = conn.new_window(conn.root, 60, 60, 30, 30, true);
    f.map_as_popup(&mut conn, popup);
    let tree = conn.get_reply(&x::QueryTree { window: popup });
    let geo = conn.get_reply(&x::GetGeometry {
        drawable: x::Drawable::Window(popup),
    });
    let reply = conn.get_reply(&x::TranslateCoordinates {
        src_window: tree.parent(),
        dst_window: conn.root,
        src_x: geo.x(),
        src_y: geo.y(),
    });

    assert_eq!(reply.dst_x(), 60);
    assert_eq!(reply.dst_y(), 60);
    assert_eq!(geo.width(), 30);
    assert_eq!(geo.height(), 30);
}

#[test]
fn popup_heuristics() {
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);

    let win_toplevel = connection.new_window(connection.root, 0, 0, 20, 20, false);
    f.map_as_toplevel(&mut connection, win_toplevel);

    let ghidra_popup = connection.new_window(connection.root, 10, 10, 50, 50, false);
    connection.set_property(
        ghidra_popup,
        x::ATOM_ATOM,
        connection.atoms.win_type,
        &[connection.atoms.win_type_normal],
    );
    connection.set_property(
        ghidra_popup,
        x::ATOM_ATOM,
        connection.atoms.net_wm_state,
        &[connection.atoms.skip_taskbar],
    );
    connection.set_property(
        ghidra_popup,
        connection.atoms.motif_wm_hints,
        connection.atoms.motif_wm_hints,
        &[0b11_u32, 0, 0, 0, 0],
    );
    f.map_as_popup(&mut connection, ghidra_popup);

    let reaper_dialog = connection.new_window(connection.root, 10, 10, 50, 50, false);
    connection.set_property(
        ghidra_popup,
        x::ATOM_ATOM,
        connection.atoms.win_type,
        &[connection.atoms.win_type_normal],
    );
    connection.set_property(
        ghidra_popup,
        x::ATOM_ATOM,
        connection.atoms.net_wm_state,
        &[connection.atoms.skip_taskbar],
    );
    connection.set_property(
        ghidra_popup,
        connection.atoms.motif_wm_hints,
        connection.atoms.motif_wm_hints,
        &[0x2_u32, 0, 0x2a, 0, 0],
    );
    f.map_as_toplevel(&mut connection, reaper_dialog);

    let chromium_menu = connection.new_window(connection.root, 10, 10, 50, 50, true);
    connection.set_property(
        chromium_menu,
        x::ATOM_ATOM,
        connection.atoms.win_type,
        &[connection.atoms.win_type_menu],
    );
    f.map_as_popup(&mut connection, chromium_menu);

    let chromium_tooltip = connection.new_window(connection.root, 10, 10, 50, 50, true);
    connection.set_property(
        chromium_tooltip,
        x::ATOM_ATOM,
        connection.atoms.win_type,
        &[connection.atoms.win_type_tooltip],
    );
    f.map_as_popup(&mut connection, chromium_tooltip);

    let discord_dnd = connection.new_window(connection.root, 20, 138, 48, 48, true);
    connection.set_property(
        discord_dnd,
        x::ATOM_ATOM,
        connection.atoms.win_type,
        &[connection.atoms.win_type_dnd],
    );
    f.map_as_popup(&mut connection, discord_dnd);

    let git_gui_popup = connection.new_window(connection.root, 10, 10, 50, 50, true);
    connection.set_property(
        git_gui_popup,
        x::ATOM_ATOM,
        connection.atoms.win_type,
        &[connection.atoms.win_type_popup_menu],
    );
    f.map_as_popup(&mut connection, git_gui_popup);

    let git_gui_dropdown = connection.new_window(connection.root, 10, 10, 50, 50, true);
    connection.set_property(
        git_gui_popup,
        x::ATOM_ATOM,
        connection.atoms.win_type,
        &[connection.atoms.win_type_dropdown_menu],
    );
    f.map_as_popup(&mut connection, git_gui_dropdown);
}

#[test]
fn xsettings_scale() {
    let mut f = Fixture::new_preset(|testwl| {
        testwl.new_output(0, 0); // WL-1
    });
    let connection = Connection::new(&f.display);
    f.testwl.enable_xdg_output_manager();

    let settings = connection.get_xsettings();
    let settings_serial = settings.serial;
    assert_eq!(settings.data["Xft/DPI"].value, 96 * 1024);
    let dpi_serial = settings.data["Xft/DPI"].last_change;
    assert_eq!(settings.data["Gdk/WindowScalingFactor"].value, 1);
    let window_serial = settings.data["Gdk/WindowScalingFactor"].last_change;
    assert_eq!(settings.data["Gdk/UnscaledDPI"].value, 96 * 1024);
    let unscaled_serial = settings.data["Gdk/UnscaledDPI"].last_change;

    let output = f.testwl.get_output("WL-1").unwrap();
    output.scale(2);
    output.done();
    f.wait_and_dispatch();

    let settings = connection.get_xsettings();
    assert!(settings.serial > settings_serial);
    assert_eq!(settings.data["Xft/DPI"].value, 2 * 96 * 1024);
    assert!(settings.data["Xft/DPI"].last_change > dpi_serial);
    assert_eq!(settings.data["Gdk/WindowScalingFactor"].value, 2);
    assert!(settings.data["Gdk/WindowScalingFactor"].last_change > window_serial);
    assert_eq!(settings.data["Gdk/UnscaledDPI"].value, 96 * 1024);
    assert!(settings.data["Gdk/UnscaledDPI"].last_change > unscaled_serial);

    let output2 = f.create_output(0, 0);
    let settings = connection.get_xsettings();
    assert_eq!(settings.data["Xft/DPI"].value, 96 * 1024);
    assert_eq!(settings.data["Gdk/WindowScalingFactor"].value, 1);
    assert_eq!(settings.data["Gdk/UnscaledDPI"].value, 96 * 1024);

    output2.scale(2);
    output2.done();
    f.testwl.dispatch();
    std::thread::sleep(Duration::from_millis(1));

    let settings = connection.get_xsettings();
    assert_eq!(settings.data["Xft/DPI"].value, 2 * 96 * 1024);
    assert_eq!(settings.data["Gdk/WindowScalingFactor"].value, 2);
    assert_eq!(settings.data["Gdk/UnscaledDPI"].value, 96 * 1024);
}

#[test]
fn xsettings_fractional_scale() {
    let mut f = Fixture::new_preset(|testwl| {
        testwl.new_output(0, 0); // WL-1
        testwl.enable_fractional_scale();
    });
    let mut connection = Connection::new(&f.display);
    f.testwl.enable_xdg_output_manager();

    let output = f.testwl.last_created_output();

    let window = connection.new_window(connection.root, 0, 0, 20, 20, false);
    let surface = f.map_as_toplevel(&mut connection, window);

    let data = f
        .testwl
        .get_surface_data(surface)
        .expect("Missing surface data");
    let fractional = data
        .fractional
        .as_ref()
        .expect("No fractional scale for surface");

    fractional.preferred_scale(180); // 1.5 scale
    f.testwl.move_surface_to_output(surface, &output);

    f.wait_and_dispatch();
    let settings = connection.get_xsettings();

    assert_eq!(
        settings.data["Xft/DPI"].value,
        (1.5 * 96_f64 * 1024_f64).round() as i32
    );
    assert_eq!(settings.data["Gdk/WindowScalingFactor"].value, 1);
    assert_eq!(
        settings.data["Gdk/UnscaledDPI"].value,
        (1.5 * 96_f64 * 1024_f64).round() as i32
    );

    let data = f.testwl.get_surface_data(surface).unwrap();
    let fractional = data.fractional.as_ref().unwrap();
    fractional.preferred_scale(300); // 2.5 scale
    f.wait_and_dispatch();

    let settings = connection.get_xsettings();
    assert_eq!(
        settings.data["Xft/DPI"].value,
        (2.5 * 96_f64 * 1024_f64).round() as i32
    );
    assert_eq!(settings.data["Gdk/WindowScalingFactor"].value, 2);
    assert_eq!(
        settings.data["Gdk/UnscaledDPI"].value,
        (2.5 / 2.0 * 96_f64 * 1024_f64).round() as i32
    );
}

#[test]
fn xsettings_switch_owner() {
    let f = Fixture::new();
    let mut connection = Connection::new(&f.display);

    let owner = connection
        .get_reply(&x::GetSelectionOwner {
            selection: connection.atoms.xsettings,
        })
        .owner();

    let win = connection.generate_id();
    connection
        .send_and_check_request(&x::CreateWindow {
            wid: win,
            x: 0,
            y: 0,
            parent: connection.root,
            depth: 0,
            width: 1,
            height: 1,
            border_width: 0,
            class: x::WindowClass::InputOnly,
            visual: x::COPY_FROM_PARENT,
            value_list: &[],
        })
        .unwrap();

    connection
        .send_and_check_request(&x::SetSelectionOwner {
            owner: win,
            selection: connection.atoms.xsettings,
            time: x::CURRENT_TIME,
        })
        .unwrap();

    assert_eq!(
        connection
            .get_reply(&x::GetSelectionOwner {
                selection: connection.atoms.xsettings,
            })
            .owner(),
        win
    );

    connection
        .send_and_check_request(&xcb::xfixes::SelectSelectionInput {
            window: connection.root,
            selection: connection.atoms.xsettings,
            event_mask: xcb::xfixes::SelectionEventMask::SET_SELECTION_OWNER
                | xcb::xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
                | xcb::xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
        })
        .unwrap();

    connection
        .send_and_check_request(&x::DestroyWindow { window: win })
        .unwrap();

    match connection.await_event() {
        xcb::Event::XFixes(xcb::xfixes::Event::SelectionNotify(x))
            if x.subtype() == xcb::xfixes::SelectionEvent::SelectionWindowDestroy => {}
        other => panic!("unexpected event {other:?}"),
    }
    match connection.await_event() {
        xcb::Event::XFixes(xcb::xfixes::Event::SelectionNotify(x))
            if x.subtype() == xcb::xfixes::SelectionEvent::SetSelectionOwner => {}
        other => panic!("unexpected event {other:?}"),
    }

    assert_eq!(
        connection
            .get_reply(&x::GetSelectionOwner {
                selection: connection.atoms.xsettings,
            })
            .owner(),
        owner
    );
}

#[test]
fn rotated_output() {
    let mut f = Fixture::new_preset(|testwl| {
        testwl.enable_xdg_output_manager();
        testwl.new_output(0, 0);
    });
    let mut connection = Connection::new(&f.display);

    connection
        .send_and_check_request(&x::ChangeWindowAttributes {
            window: connection.root,
            value_list: &[x::Cw::EventMask(x::EventMask::STRUCTURE_NOTIFY)],
        })
        .unwrap();

    let output = f.testwl.get_output("WL-1").unwrap();
    output.mode(wl_output::Mode::Current, 100, 1000, 60);
    output.geometry(
        0,
        0,
        50,
        50,
        wl_output::Subpixel::Unknown,
        "satellite".to_string(),
        "WL-1".to_string(),
        wl_output::Transform::_90,
    );
    output.done();
    f.testwl.dispatch();

    match connection.await_event() {
        xcb::Event::X(x::Event::ConfigureNotify(e)) => {
            assert_eq!(e.window(), connection.root);
            assert_eq!(e.width(), 1000);
            assert_eq!(e.height(), 100);
        }
        other => panic!("unexpected event {other:?}"),
    }
}

const BTN_LEFT: u32 = 0x110;

#[test]
fn client_init_move() {
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);

    let win_toplevel = connection.new_window(connection.root, 0, 0, 20, 20, false);
    let surface = f.map_as_toplevel(&mut connection, win_toplevel);
    f.testwl.move_pointer_to(surface, 10., 10.);
    let ptr = f.testwl.pointer();
    ptr.motion(10, 10.0, 10.0);
    ptr.frame();
    ptr.button(10, 20, BTN_LEFT, wl_pointer::ButtonState::Pressed);
    ptr.frame();
    f.testwl.dispatch();

    connection.send_client_message(&x::ClientMessageEvent::new(
        win_toplevel,
        connection.atoms.moveresize,
        x::ClientMessageData::Data32([0, 0, MoveResizeDirection::Move.into(), 1, 0]),
    ));

    f.wait_and_dispatch();
    let data = f.testwl.get_surface_data(surface).unwrap();
    assert!(data.moving);
}

#[test]
fn client_init_resize() {
    let mut f = Fixture::new();
    let mut connection = Connection::new(&f.display);

    let win_toplevel = connection.new_window(connection.root, 0, 0, 20, 20, false);
    let surface = f.map_as_toplevel(&mut connection, win_toplevel);
    f.testwl.move_pointer_to(surface, 10., 10.);
    let ptr = f.testwl.pointer();
    ptr.motion(10, 10.0, 10.0);
    ptr.frame();
    ptr.button(10, 20, BTN_LEFT, wl_pointer::ButtonState::Pressed);
    ptr.frame();
    f.testwl.dispatch();

    connection.send_client_message(&x::ClientMessageEvent::new(
        win_toplevel,
        connection.atoms.moveresize,
        x::ClientMessageData::Data32([0, 0, MoveResizeDirection::SizeBottomRight.into(), 1, 0]),
    ));

    f.wait_and_dispatch();
    let data = f.testwl.get_surface_data(surface).unwrap();
    assert!(
        matches!(data.resizing, Some(xdg_toplevel::ResizeEdge::BottomRight)),
        "Got wrong resizing edge: {:?}",
        data.resizing
    );
}
