use bitflags::bitflags;
use log::{debug, trace, warn};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::Arc;
use xcb::{x, Xid, XidNew};
use xcb_util_cursor::{Cursor, CursorContext};

pub struct XState {
    pub connection: Arc<xcb::Connection>,
    root: x::Window,
    pub atoms: Atoms,
    window_types: WindowTypes,
}

impl XState {
    pub fn new(fd: BorrowedFd) -> Self {
        let connection = Arc::new(xcb::Connection::connect_to_fd(fd.as_raw_fd(), None).unwrap());
        let setup = connection.get_setup();
        let screen = setup.roots().next().unwrap();
        let root = screen.root();

        connection
            .send_and_check_request(&x::ChangeWindowAttributes {
                window: root,
                value_list: &[x::Cw::EventMask(
                    x::EventMask::SUBSTRUCTURE_REDIRECT // To have Xwayland send us WL_SURFACE_ID
                    | x::EventMask::SUBSTRUCTURE_NOTIFY // To get notified whenever new windows are created
                    | x::EventMask::RESIZE_REDIRECT,
                )],
            })
            .unwrap();

        let atoms = Atoms::intern_all(&connection).unwrap();
        trace!("atoms: {atoms:#?}");
        let window_types = WindowTypes::new(&connection);

        // This makes Xwayland spit out damage tracking
        connection
            .send_and_check_request(&xcb::composite::RedirectSubwindows {
                window: screen.root(),
                update: xcb::composite::Redirect::Manual,
            })
            .unwrap();

        // Setup default cursor theme
        let ctx = CursorContext::new(&connection, screen).unwrap();
        let left_ptr = ctx.load_cursor(Cursor::LeftPtr);
        connection
            .send_and_check_request(&x::ChangeWindowAttributes {
                window: root,
                value_list: &[x::Cw::Cursor(left_ptr)],
            })
            .unwrap();

        let mut r = Self {
            connection,
            root,
            atoms,
            window_types,
        };
        r.create_ewmh_window();
        r
    }

    fn set_root_property<P: x::PropEl>(&self, property: x::Atom, r#type: x::Atom, data: &[P]) {
        self.connection
            .send_and_check_request(&x::ChangeProperty {
                mode: x::PropMode::Replace,
                window: self.root,
                property,
                r#type,
                data,
            })
            .unwrap();
    }

    fn create_ewmh_window(&mut self) {
        let window = self.connection.generate_id();
        self.connection
            .send_and_check_request(&x::CreateWindow {
                depth: 0,
                wid: window,
                parent: self.root,
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                border_width: 0,
                class: x::WindowClass::InputOnly,
                visual: x::COPY_FROM_PARENT,
                value_list: &[],
            })
            .unwrap();

        self.set_root_property(self.atoms.wm_check, x::ATOM_WINDOW, &[window]);
        self.set_root_property(self.atoms.active_win, x::ATOM_WINDOW, &[x::Window::none()]);
        self.set_root_property(
            self.atoms.supported,
            x::ATOM_ATOM,
            &[self.atoms.active_win, self.atoms.client_list],
        );

        self.connection
            .send_and_check_request(&x::ChangeProperty {
                mode: x::PropMode::Replace,
                window,
                property: self.atoms.wm_check,
                r#type: x::ATOM_WINDOW,
                data: &[window],
            })
            .unwrap();

        self.connection
            .send_and_check_request(&x::ChangeProperty {
                mode: x::PropMode::Replace,
                window,
                property: self.atoms.wm_name,
                r#type: x::ATOM_STRING,
                data: b"exwayland wm",
            })
            .unwrap();
    }

    pub fn handle_events(&mut self, server_state: &mut super::RealServerState) {
        while let Some(event) = self.connection.poll_for_event().unwrap() {
            trace!("x11 event: {event:?}");
            match event {
                xcb::Event::X(x::Event::CreateNotify(e)) => {
                    debug!("new window: {:?}", e);
                    match self
                        .connection
                        .send_and_check_request(&x::ChangeWindowAttributes {
                            window: e.window(),
                            value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
                        }) {
                        // This can sometimes fail if the window was created and then immediately
                        // destroyed.
                        Ok(()) | Err(xcb::ProtocolError::X(x::Error::Window(_), _)) => {}
                        Err(other) => {
                            panic!("error subscribing to property change on new window: {other:?}")
                        }
                    }

                    let parent = e.parent();
                    let parent = if parent.is_none() || parent == self.root {
                        None
                    } else {
                        Some(parent)
                    };
                    server_state.new_window(e.window(), e.override_redirect(), (&e).into(), parent);
                }
                xcb::Event::X(x::Event::MapRequest(e)) => {
                    debug!("requested to map {:?}", e.window());
                    self.connection
                        .send_and_check_request(&x::MapWindow { window: e.window() })
                        .unwrap();
                }
                xcb::Event::X(x::Event::MapNotify(e)) => {
                    server_state.map_window(e.window());
                }
                xcb::Event::X(x::Event::ConfigureNotify(e)) => {
                    server_state.reconfigure_window(e);
                }
                xcb::Event::X(x::Event::UnmapNotify(e)) => {
                    trace!("unmap event: {:?}", e.event());
                    server_state.unmap_window(e.window());
                }
                xcb::Event::X(x::Event::DestroyNotify(e)) => {
                    debug!("destroying window {:?}", e.window());
                    server_state.destroy_window(e.window());
                }
                xcb::Event::X(x::Event::PropertyNotify(e)) => {
                    self.handle_property_change(e, server_state);
                }
                xcb::Event::X(x::Event::ConfigureRequest(e)) => {
                    debug!("{:?} request: {:?}", e.window(), e.value_mask());
                    let mut list = Vec::new();
                    let mask = e.value_mask();

                    if mask.contains(x::ConfigWindowMask::X) {
                        list.push(x::ConfigWindow::X(e.x().into()));
                    }
                    if mask.contains(x::ConfigWindowMask::Y) {
                        list.push(x::ConfigWindow::Y(e.y().into()));
                    }
                    if mask.contains(x::ConfigWindowMask::WIDTH) {
                        list.push(x::ConfigWindow::Width(e.width().into()));
                    }
                    if mask.contains(x::ConfigWindowMask::HEIGHT) {
                        list.push(x::ConfigWindow::Height(e.height().into()));
                    }

                    self.connection
                        .send_and_check_request(&x::ConfigureWindow {
                            window: e.window(),
                            value_list: &list,
                        })
                        .unwrap();
                }
                xcb::Event::X(x::Event::ClientMessage(e)) => match e.r#type() {
                    x if x == self.atoms.wl_surface_id => {
                        let x::ClientMessageData::Data32(data) = e.data() else {
                            unreachable!();
                        };
                        let id: u32 = (data[0] as u64 | ((data[1] as u64) << 32)) as u32;
                        server_state.associate_window(e.window(), id);
                    }
                    x if x == self.atoms.net_wm_state => {
                        let x::ClientMessageData::Data32(data) = e.data() else {
                            unreachable!();
                        };
                        let Ok(action) = SetState::try_from(data[0]) else {
                            warn!("unknown action for _NET_WM_STATE: {}", data[0]);
                            continue;
                        };
                        let prop1 = unsafe { x::Atom::new(data[1]) };
                        let prop2 = unsafe { x::Atom::new(data[2]) };

                        trace!("_NET_WM_STATE ({action:?}) props: {prop1:?} {prop2:?}");

                        for prop in [prop1, prop2] {
                            match prop {
                                x if x == self.atoms.wm_fullscreen => {
                                    server_state.set_fullscreen(e.window(), action);
                                }
                                _ => {}
                            }
                        }
                    }
                    t => warn!("unrecognized message: {t:?}"),
                },
                xcb::Event::X(x::Event::MappingNotify(_)) => {}
                other => {
                    warn!("unhandled event: {other:?}");
                }
            }

            server_state.run();
        }
    }

    fn handle_property_change(
        &self,
        event: x::PropertyNotifyEvent,
        server_state: &mut super::RealServerState,
    ) {
        let get_prop = |r#type, long_length| {
            self.connection
                .wait_for_reply(self.connection.send_request(&x::GetProperty {
                    window: event.window(),
                    property: event.atom(),
                    r#type,
                    long_offset: 0,
                    long_length,
                    delete: false,
                }))
        };
        if event.state() != x::Property::NewValue {
            return;
        }

        match event.atom() {
            x if x == self.atoms.wm_window_type => {
                let Ok(prop) = get_prop(x::ATOM_ATOM, 8) else {
                    return;
                };
                let types: &[x::Atom] = prop.value();
                let win_type = types.iter().find_map(|a| self.window_types.get_type(*a));
                debug!(
                    "set {:?} type to {} ({})",
                    event.window(),
                    win_type.unwrap_or("[Unknown/Unrecognized]".to_string()),
                    types.len()
                );
            }
            x if x == x::ATOM_WM_NORMAL_HINTS => {
                let Ok(prop) = get_prop(x::ATOM_WM_SIZE_HINTS, 9) else {
                    return;
                };
                let data: &[u32] = prop.value();
                let hints = WmNormalHints::from(data);
                server_state.set_win_hints(event.window(), hints);
            }
            _ => {
                let prop = self
                    .connection
                    .wait_for_reply(
                        self.connection
                            .send_request(&x::GetAtomName { atom: event.atom() }),
                    )
                    .unwrap();

                debug!(
                    "changed property {:?} for {:?}",
                    prop.name(),
                    event.window()
                );
            }
        }
    }
}

xcb::atoms_struct! {
    #[derive(Clone, Debug)]
    pub struct Atoms {
        pub wl_surface_id => b"WL_SURFACE_ID" only_if_exists = false,
        pub wm_protocols => b"WM_PROTOCOLS" only_if_exists = false,
        pub wm_delete_window => b"WM_DELETE_WINDOW" only_if_exists = false,
        pub wm_transient_for => b"WM_TRANSIENT_FOR" only_if_exists = false,
        pub wm_hints => b"WM_HINTS" only_if_exists = false,
        pub wm_check => b"_NET_SUPPORTING_WM_CHECK" only_if_exists = false,
        pub wm_name => b"_NET_WM_NAME" only_if_exists = false,
        pub wm_window_type => b"_NET_WM_WINDOW_TYPE" only_if_exists = false,
        pub wm_pid => b"_NET_WM_PID" only_if_exists = false,
        pub net_wm_state => b"_NET_WM_STATE" only_if_exists = false,
        pub wm_fullscreen => b"_NET_WM_STATE_FULLSCREEN" only_if_exists = false,
        pub active_win => b"_NET_ACTIVE_WINDOW" only_if_exists = false,
        pub client_list => b"_NET_CLIENT_LIST" only_if_exists = false,
        pub supported => b"_NET_SUPPORTED" only_if_exists = false,
    }
}

xcb::atoms_struct! {
    pub struct WindowTypes {
        pub normal => b"_NET_WM_WINDOW_TYPE_NORMAL" only_if_exists = false,
        pub dialog => b"_NET_WM_WINDOW_TYPE_DIALOG" only_if_exists = false,
        pub splash => b"_NET_WM_WINDOW_TYPE_SPLASH" only_if_exists = false,
        pub menu => b"_NET_WM_WINDOW_TYPE_MENU" only_if_exists = false,
        pub utility => b"_NET_WM_WINDOW_TYPE_UTILITY" only_if_exists = false,
    }
}

impl WindowTypes {
    pub fn new(connection: &xcb::Connection) -> Self {
        let r = Self::intern_all(connection).unwrap();
        assert_ne!(r.normal, x::ATOM_NONE);
        assert_ne!(r.dialog, x::ATOM_NONE);
        assert_ne!(r.utility, x::ATOM_NONE);
        r
    }
    pub fn get_type(&self, atom: x::Atom) -> Option<String> {
        match atom {
            x if x == self.normal => Some("Normal".to_string()),
            x if x == self.dialog => Some("Dialog".to_string()),
            x if x == self.utility => Some("Utility".to_string()),
            x if x == self.menu => Some("Menu".to_string()),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WindowDims {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

bitflags! {
    /// From ICCCM spec.
    /// https://tronche.com/gui/x/icccm/sec-4.html#s-4.1.2.3
    pub struct WmSizeHintsFlags: u32 {
        const UserPosition = 1;
        const UserSize = 2;
        const ProgramPosition = 4;
        const ProgramSize = 8;
        const ProgramMinSize = 16;
        const ProgramMaxSize = 32;
        const ProgramResizeIncrement = 64;
        const ProgramAspect = 128;
        const ProgramBaseSize = 256;
        const ProgramWinGravity = 512;
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct WinSize {
    pub width: i32,
    pub height: i32,
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct WmNormalHints {
    pub min_size: Option<WinSize>,
    pub max_size: Option<WinSize>,
}

#[derive(Debug, Clone, Copy)]
pub enum SetState {
    Remove,
    Add,
    Toggle,
}

impl TryFrom<u32> for SetState {
    type Error = ();
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Remove),
            1 => Ok(Self::Add),
            2 => Ok(Self::Toggle),
            _ => Err(()),
        }
    }
}

impl From<&[u32]> for WmNormalHints {
    fn from(value: &[u32]) -> Self {
        let mut ret = Self::default();
        let flags = WmSizeHintsFlags::from_bits(value[0]).unwrap();

        if flags.contains(WmSizeHintsFlags::ProgramMinSize) {
            ret.min_size = Some(WinSize {
                width: value[5] as _,
                height: value[6] as _,
            });
        }

        if flags.contains(WmSizeHintsFlags::ProgramMaxSize) {
            ret.max_size = Some(WinSize {
                width: value[7] as _,
                height: value[8] as _,
            });
        }

        ret
    }
}

impl super::XConnection for Arc<xcb::Connection> {
    type ExtraData = Atoms;

    fn root_window(&self) -> x::Window {
        self.get_setup().roots().next().unwrap().root()
    }

    fn set_window_dims(&mut self, window: x::Window, dims: crate::server::PendingSurfaceState) {
        trace!("reconfiguring window {window:?}");
        self.send_and_check_request(&x::ConfigureWindow {
            window,
            value_list: &[
                x::ConfigWindow::X(dims.x),
                x::ConfigWindow::Y(dims.y),
                x::ConfigWindow::Width(dims.width as _),
                x::ConfigWindow::Height(dims.height as _),
            ],
        })
        .unwrap();
    }

    fn set_fullscreen(&mut self, window: x::Window, fullscreen: bool, atoms: Self::ExtraData) {
        let data = if fullscreen {
            std::slice::from_ref(&atoms.wm_fullscreen)
        } else {
            &[]
        };
        self.send_and_check_request(&x::ChangeProperty::<x::Atom> {
            mode: x::PropMode::Replace,
            window,
            property: atoms.net_wm_state,
            r#type: x::ATOM_ATOM,
            data,
        })
        .unwrap();
    }

    fn focus_window(&mut self, window: x::Window, atoms: Self::ExtraData) {
        let prop = self
            .wait_for_reply(self.send_request(&x::GetProperty {
                delete: false,
                window,
                property: atoms.wm_hints,
                r#type: atoms.wm_hints,
                long_offset: 0,
                long_length: 8,
            }))
            .unwrap();

        let fields: &[u32] = prop.value();
        let mut input = false;
        if !fields.is_empty() {
            let flags = fields[0];
            if (flags & 0x1) > 0 {
                input = fields[1] > 0;
            }
        }

        if input {
            // might fail if window is not visible but who cares
            let _ = self.send_and_check_request(&x::SetInputFocus {
                focus: window,
                revert_to: x::InputFocus::None,
                time: x::CURRENT_TIME,
            });
        }

        self.send_and_check_request(&x::ConfigureWindow {
            window,
            value_list: &[x::ConfigWindow::StackMode(x::StackMode::Above)],
        })
        .unwrap();
        self.send_and_check_request(&x::ChangeProperty {
            mode: x::PropMode::Replace,
            window: self.root_window(),
            property: atoms.active_win,
            r#type: x::ATOM_WINDOW,
            data: &[window],
        })
        .unwrap();
    }

    fn close_window(&mut self, window: x::Window, atoms: Self::ExtraData) {
        let data = [atoms.wm_delete_window.resource_id(), 0, 0, 0, 0];
        let event = &x::ClientMessageEvent::new(
            window,
            atoms.wm_protocols,
            x::ClientMessageData::Data32(data),
        );

        self.send_and_check_request(&x::SendEvent {
            destination: x::SendEventDest::Window(window),
            propagate: false,
            event_mask: x::EventMask::empty(),
            event,
        })
        .unwrap();
    }
}

impl super::FromServerState<Arc<xcb::Connection>> for Atoms {
    fn create(state: &super::RealServerState) -> Self {
        state.atoms.as_ref().unwrap().clone()
    }
}
