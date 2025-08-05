mod server;
pub mod xstate;

use crate::server::{NoConnection, PendingSurfaceState, ServerState};
use crate::xstate::{RealConnection, XState};
use log::{error, info};
use rustix::event::{poll, PollFd, PollFlags};
use smithay_client_toolkit::data_device_manager::WritePipe;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Command, ExitStatus, Stdio};
use wayland_server::{Display, ListeningSocket};
use xcb::x;

pub trait XConnection: Sized + 'static {
    type X11Selection: X11Selection;

    fn set_window_dims(&mut self, window: x::Window, dims: PendingSurfaceState) -> bool;
    fn set_fullscreen(&mut self, window: x::Window, fullscreen: bool);
    fn focus_window(&mut self, window: x::Window, output_name: Option<String>);
    fn close_window(&mut self, window: x::Window);
    fn unmap_window(&mut self, window: x::Window);
    fn raise_to_top(&mut self, window: x::Window);
}

pub trait X11Selection {
    fn mime_types(&self) -> Vec<&str>;
    fn write_to(&self, mime: &str, pipe: WritePipe);
}

type EarlyServerState = ServerState<NoConnection<<RealConnection as XConnection>::X11Selection>>;
type RealServerState = ServerState<RealConnection>;

pub trait RunData {
    fn display(&self) -> Option<&str>;
    fn listenfds(&mut self) -> Vec<OwnedFd>;
    fn server(&self) -> Option<UnixStream> {
        None
    }
    fn created_server(&self) {}
    fn connected_server(&self) {}
    fn quit_rx(&self) -> Option<UnixStream> {
        None
    }
    fn xwayland_ready(&self, _display: String, _pid: u32) {}
}

pub fn main(mut data: impl RunData) -> Option<()> {
    let mut version = env!("VERGEN_GIT_DESCRIBE");
    if version == "VERGEN_IDEMPOTENT_OUTPUT" {
        version = env!("CARGO_PKG_VERSION");
    }
    info!("Starting xwayland-satellite version {version}");

    let socket = ListeningSocket::bind_auto("xwls", 1..=128).unwrap();
    let mut display = Display::new().unwrap();
    let dh = display.handle();
    data.created_server();

    let (xsock_wl, xsock_xwl) = UnixStream::pair().unwrap();
    // XCB takes responsibility for cleaning up this FD, but since connecting takes a RawFd at the
    // FFI level, see (`XState::new`), `xsock_wl`'s destructor also closes the FD, leading the FD
    // being closed twice. This mainly caused problems in the integration tests, where `xsock_wl`'s
    // destructor would be run after the descriptor was freed, leading to an opaque abort message.
    // See https://github.com/rust-x-bindings/rust-xcb/issues/282 for further explanation.
    let xsock_wl = Box::leak(Box::new(xsock_wl));
    // Prevent creation of new Xwayland command from closing fd
    rustix::io::fcntl_setfd(&xsock_xwl, rustix::io::FdFlags::empty()).unwrap();

    let (ready_tx, ready_rx) = UnixStream::pair().unwrap();
    rustix::io::fcntl_setfd(&ready_tx, rustix::io::FdFlags::empty()).unwrap();
    let mut xwayland = Command::new("Xwayland");
    if let Some(display) = data.display() {
        xwayland.arg(display);
    }

    let fds = data.listenfds();
    for fd in &fds {
        xwayland.args(["-listenfd", &fd.as_raw_fd().to_string()]);
    }

    let mut xwayland = xwayland
        .args([
            "-rootless",
            "-force-xrandr-emulation",
            "-wm",
            &xsock_xwl.as_raw_fd().to_string(),
            "-displayfd",
            &ready_tx.as_raw_fd().to_string(),
        ])
        .env("WAYLAND_DISPLAY", socket.socket_name().unwrap())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Now that Xwayland spawned and got the listenfds, we can close them here.
    drop(fds);

    let xwl_pid = xwayland.id();

    let (mut finish_tx, mut finish_rx) = UnixStream::pair().unwrap();
    let stderr = xwayland.stderr.take().unwrap();
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            let line = line.unwrap();
            info!(target: "xwayland_process", "{line}");
        }
        let status = Box::new(xwayland.wait().unwrap());
        let status = Box::into_raw(status) as usize;
        finish_tx.write_all(&status.to_ne_bytes()).unwrap();
    });

    let mut ready_fds = [
        PollFd::new(&socket, PollFlags::IN),
        PollFd::new(&finish_rx, PollFlags::IN),
    ];

    fn xwayland_exit_code(rx: &mut UnixStream) -> Box<ExitStatus> {
        let mut data = [0; (usize::BITS / 8) as usize];
        rx.read_exact(&mut data).unwrap();
        let data = usize::from_ne_bytes(data);
        unsafe { Box::from_raw(data as *mut _) }
    }

    let connection = match poll(&mut ready_fds, -1) {
        Ok(_) => {
            if !ready_fds[1].revents().is_empty() {
                let status = xwayland_exit_code(&mut finish_rx);
                error!("Xwayland exited early with {status}");
                return None;
            }

            data.connected_server();
            socket.accept().unwrap().unwrap()
        }
        Err(e) => {
            panic!("first poll failed: {e:?}")
        }
    };

    let mut server_state = EarlyServerState::new(dh, data.server(), connection);
    server_state.run();

    // Remove the lifetimes on our fds to avoid borrowing issues, since we know they will exist for
    // the rest of our program anyway
    let server_fd = unsafe { BorrowedFd::borrow_raw(server_state.clientside_fd().as_raw_fd()) };
    let display_fd = unsafe { BorrowedFd::borrow_raw(display.backend().poll_fd().as_raw_fd()) };

    // `finish_rx` only writes the status code of `Xwayland` exiting, so it is reasonable to use as
    // the UnixStream of choice when not running the integration tests.
    let mut quit_rx = data.quit_rx().unwrap_or(finish_rx);

    let mut fds = [
        PollFd::from_borrowed_fd(server_fd, PollFlags::IN),
        PollFd::new(&xsock_wl, PollFlags::IN),
        PollFd::from_borrowed_fd(display_fd, PollFlags::IN),
        PollFd::new(&quit_rx, PollFlags::IN),
        PollFd::new(&ready_rx, PollFlags::IN),
    ];

    loop {
        match poll(&mut fds, -1) {
            Ok(_) => {
                if !fds[3].revents().is_empty() {
                    let status = xwayland_exit_code(&mut quit_rx);
                    if *status != ExitStatus::default() {
                        error!("Xwayland exited early with {status}");
                    }
                    return None;
                }
                if !fds[4].revents().is_empty() {
                    break;
                }
            }
            Err(other) => panic!("Poll failed: {other:?}"),
        }

        display.dispatch_clients(&mut *server_state).unwrap();
        server_state.run();
        display.flush_clients().unwrap();
    }

    let mut xstate = XState::new(xsock_wl.as_fd());
    let mut reader = BufReader::new(&ready_rx);
    {
        let mut display = String::new();
        reader.read_line(&mut display).unwrap();
        display.pop();
        display.insert(0, ':');
        info!("Connected to Xwayland on {display}");
        data.xwayland_ready(display, xwl_pid);
    }
    let mut server_state = xstate.server_state_setup(server_state);

    #[cfg(feature = "systemd")]
    {
        match sd_notify::notify(true, &[sd_notify::NotifyState::Ready]) {
            Ok(()) => info!("Successfully notified systemd of ready state."),
            Err(e) => log::warn!("Systemd notify failed: {e:?}"),
        }
    }

    #[cfg(not(feature = "systemd"))]
    info!("Systemd support disabled.");

    loop {
        xstate.handle_events(&mut server_state);

        display.dispatch_clients(&mut *server_state).unwrap();
        server_state.run();
        display.flush_clients().unwrap();

        if let Some(sel) = server_state.new_selection() {
            xstate.set_clipboard(sel);
        }

        if let Some(scale) = server_state.new_global_scale() {
            xstate.update_global_scale(scale);
        }

        match poll(&mut fds, -1) {
            Ok(_) => {
                if !fds[3].revents().is_empty() {
                    let status = xwayland_exit_code(&mut quit_rx);
                    if *status != ExitStatus::default() {
                        error!("Xwayland exited early with {status}");
                    }
                    return None;
                }
            }
            Err(other) => panic!("Poll failed: {other:?}"),
        }
    }
}
