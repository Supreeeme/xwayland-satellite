mod clientside;
mod server;
mod xstate;

use crate::server::{PendingSurfaceState, ServerState};
use crate::xstate::XState;
use log::{error, info};
use rustix::event::{poll, PollFd, PollFlags};
use signal_hook::consts::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::{net::UnixStream, process::CommandExt};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::Sender,
    Arc,
};
use wayland_server::{Display, ListeningSocket};
use xcb::x;

pub trait XConnection: Sized + 'static {
    type ExtraData: FromServerState<Self>;

    fn root_window(&self) -> x::Window;
    fn set_window_dims(&mut self, window: x::Window, dims: PendingSurfaceState);
    fn set_fullscreen(&mut self, window: x::Window, fullscreen: bool, data: Self::ExtraData);
    fn focus_window(&mut self, window: x::Window, data: Self::ExtraData);
    fn close_window(&mut self, window: x::Window, data: Self::ExtraData);
}

pub trait FromServerState<C: XConnection> {
    fn create(state: &ServerState<C>) -> Self;
}

type RealServerState = ServerState<Arc<xcb::Connection>>;

#[derive(Debug, PartialEq, Eq)]
pub enum StateEvent {
    CreatedServer,
    ConnectedServer,
    XwaylandReady,
}

pub fn main(comp: Option<UnixStream>, state_updater: Option<Sender<StateEvent>>) -> Option<()> {
    let display_arg = get_display()?;

    let socket = ListeningSocket::bind_auto("wayland", 1..=128).unwrap();
    let mut display = Display::<RealServerState>::new().unwrap();
    let dh = display.handle();

    let mut server_state = RealServerState::new(dh, comp);
    if let Some(ref s) = state_updater {
        s.send(StateEvent::CreatedServer).unwrap();
    }

    let (xsock_wl, xsock_xwl) = UnixStream::pair().unwrap();
    // Prevent creation of new Xwayland command from closing fd
    rustix::io::fcntl_setfd(&xsock_xwl, rustix::io::FdFlags::empty()).unwrap();

    // Flag when Xwayland is ready to accept our connection
    let ready = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGUSR1, ready.clone()).unwrap();

    let mut xwayland = Command::new("Xwayland");
    let mut xwayland = unsafe {
        xwayland.pre_exec(|| {
            // Set SIGUSR1 to SIG_IGN for Xwayland to get SIGUSR1 to our main process,
            // which signifies that the X server is ready to accept connections
            let mut sa_mask = MaybeUninit::uninit();
            libc::sigemptyset(sa_mask.as_mut_ptr());
            let sa_mask = sa_mask.assume_init();
            let act = libc::sigaction {
                sa_sigaction: libc::SIG_IGN,
                sa_mask,
                sa_flags: 0,
                sa_restorer: None,
            };
            libc::sigaction(SIGUSR1, &act, std::ptr::null_mut());
            Ok(())
        })
    }
    .env("WAYLAND_DISPLAY", socket.socket_name().unwrap())
    //.env("WAYLAND_DEBUG", "1")
    .args([
        &display_arg,
        "-rootless",
        "-wm",
        &format!("{}", &xsock_xwl.as_raw_fd()),
    ])
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();

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

    let connection = match poll(&mut ready_fds, -1) {
        Ok(_) => {
            if !ready_fds[1].revents().is_empty() {
                let mut data = [0; (usize::BITS / 8) as usize];
                finish_rx.read_exact(&mut data).unwrap();
                let data = usize::from_ne_bytes(data);
                let status: Box<std::process::ExitStatus> =
                    unsafe { Box::from_raw(data as *mut _) };

                error!("Xwayland exited early with {status}");
                return None;
            }

            if let Some(ref s) = state_updater {
                s.send(StateEvent::ConnectedServer).unwrap();
            }
            socket.accept().unwrap().unwrap()
        }
        Err(e) => {
            panic!("first poll failed: {e:?}")
        }
    };
    drop(finish_rx);

    server_state.connect(connection);
    server_state.run();

    let mut xstate: Option<XState> = None;

    // Remove the lifetimes on our fds to avoid borrowing issues, since we know they will exist for
    // the rest of our program anyway
    let server_fd = unsafe { BorrowedFd::borrow_raw(server_state.clientside_fd().as_raw_fd()) };
    let display_fd = unsafe { BorrowedFd::borrow_raw(display.backend().poll_fd().as_raw_fd()) };

    let mut fds = [
        PollFd::from_borrowed_fd(server_fd, PollFlags::IN),
        PollFd::new(&xsock_wl, PollFlags::IN),
        PollFd::from_borrowed_fd(display_fd, PollFlags::IN),
    ];

    loop {
        match poll(&mut fds, -1) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => {
                // Typically caused by SIGUSR1
                if !ready.load(Ordering::Relaxed) {
                    continue;
                }
            }
            Err(other) => panic!("Poll failed: {other:?}"),
        }

        if xstate.is_none() && ready.load(Ordering::Relaxed) {
            let xstate = xstate.insert(XState::new(xsock_wl.as_fd()));
            info!("Connected to Xwayland on {display_arg}");
            if let Some(ref s) = state_updater {
                s.send(StateEvent::XwaylandReady).unwrap();
            }
            server_state.set_x_connection(xstate.connection.clone());
            server_state.atoms = Some(xstate.atoms.clone());
        }

        if let Some(state) = &mut xstate {
            state.handle_events(&mut server_state);
        }

        display.dispatch_clients(&mut server_state).unwrap();
        server_state.run();
        display.flush_clients().unwrap();
    }
}

fn get_display() -> Option<String> {
    let mut args: Vec<_> = std::env::args().collect();
    if args.len() > 2 {
        error!("Unexpected arguments: {:?}", &args[2..]);
        return None;
    }
    if args.len() == 1 {
        Some(":0".into())
    } else {
        Some(args.swap_remove(1))
    }
}
