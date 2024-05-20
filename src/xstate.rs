use crate::server::WindowAttributes;
use bitflags::bitflags;
use log::{debug, trace, warn};
use std::ffi::CString;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::Arc;
use xcb::{x, Xid, XidNew};
use xcb_util_cursor::{Cursor, CursorContext};

pub struct XState {
    pub connection: Arc<xcb::Connection>,
    root: x::Window,
    pub atoms: Atoms,
}

/// Essentially a trait alias.
trait PropertyResolver {
    type Output;
    fn resolve(self, reply: x::GetPropertyReply) -> Self::Output;
}
impl<T, Output> PropertyResolver for T
where
    T: FnOnce(x::GetPropertyReply) -> Output,
{
    type Output = Output;
    fn resolve(self, reply: x::GetPropertyReply) -> Self::Output {
        (self)(reply)
    }
}

struct PropertyCookieWrapper<'a, F: PropertyResolver> {
    connection: &'a xcb::Connection,
    cookie: x::GetPropertyCookie,
    resolver: F,
}

impl<F: PropertyResolver> PropertyCookieWrapper<'_, F> {
    /// Get the result from our property cookie.
    fn resolve(self) -> Option<F::Output> {
        let reply = self.connection.wait_for_reply(self.cookie).unwrap();
        if reply.r#type() == x::ATOM_NONE {
            None
        } else {
            Some(self.resolver.resolve(reply))
        }
    }
}

#[derive(Debug)]
pub enum WmName {
    WmName(String),
    NetWmName(String),
}

impl WmName {
    pub fn name(&self) -> &str {
        match self {
            Self::WmName(n) => n,
            Self::NetWmName(n) => n,
        }
    }
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
        self.set_root_property(self.atoms.supported, x::ATOM_ATOM, &[self.atoms.active_win]);

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
                property: self.atoms.net_wm_name,
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
                    let parent = e.parent();
                    let parent = if parent.is_none() || parent == self.root {
                        None
                    } else {
                        Some(parent)
                    };
                    server_state.new_window(e.window(), e.override_redirect(), (&e).into(), parent);
                }
                xcb::Event::X(x::Event::ReparentNotify(e)) => {
                    debug!("reparent event: {e:?}");
                    if e.parent() == self.root {
                        let attrs = self.get_window_attributes(e.window());
                        server_state.new_window(
                            e.window(),
                            attrs.override_redirect,
                            attrs.dims,
                            None,
                        );
                        self.handle_window_attributes(server_state, e.window(), attrs);
                    } else {
                        debug!("destroying window since its parent is no longer root!");
                        server_state.destroy_window(e.window());
                    }
                }
                xcb::Event::X(x::Event::MapRequest(e)) => {
                    debug!("requested to map {:?}", e.window());
                    self.connection
                        .send_and_check_request(&x::MapWindow { window: e.window() })
                        .unwrap();
                }
                xcb::Event::X(x::Event::MapNotify(e)) => {
                    let attrs = self.get_window_attributes(e.window());
                    self.handle_window_attributes(server_state, e.window(), attrs);
                    server_state.map_window(e.window());
                    self.connection
                        .send_and_check_request(&x::ChangeWindowAttributes {
                            window: e.window(),
                            value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
                        })
                        .unwrap();
                }
                xcb::Event::X(x::Event::ConfigureNotify(e)) => {
                    server_state.reconfigure_window(e);
                }
                xcb::Event::X(x::Event::UnmapNotify(e)) => {
                    trace!("unmap event: {:?}", e.event());
                    server_state.unmap_window(e.window());
                    match self
                        .connection
                        .send_and_check_request(&x::ChangeWindowAttributes {
                            window: e.window(),
                            value_list: &[x::Cw::EventMask(x::EventMask::empty())],
                        }) {
                        // Window error may occur if the window has been destroyed,
                        // which is fine
                        Ok(_) | Err(xcb::ProtocolError::X(x::Error::Window(_), _)) => {}
                        Err(other) => panic!("Error removing event mask from window: {other:?}"),
                    }
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
                        panic!("Xserver should be using WL_SURFACE_SERIAL, not WL_SURFACE_ID");
                    }
                    x if x == self.atoms.wl_surface_serial => {
                        let x::ClientMessageData::Data32(data) = e.data() else {
                            unreachable!();
                        };
                        server_state.set_window_serial(e.window(), [data[0], data[1]]);
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

    fn get_window_attributes(&self, window: x::Window) -> WindowAttributes {
        let geometry = self.connection.send_request(&x::GetGeometry {
            drawable: x::Drawable::Window(window),
        });
        let attrs = self
            .connection
            .send_request(&x::GetWindowAttributes { window });

        let name = self.get_net_wm_name(window);
        let class = self.get_wm_class(window);
        let wm_hints = self.get_wm_hints(window);
        let size_hints = self.get_wm_size_hints(window);

        let geometry = self.connection.wait_for_reply(geometry).unwrap();
        let attrs = self.connection.wait_for_reply(attrs).unwrap();
        let title = name
            .resolve()
            .or_else(|| self.get_wm_name(window).resolve());
        let class = class.resolve();
        let wm_hints = wm_hints.resolve();
        let size_hints = size_hints.resolve();

        WindowAttributes {
            override_redirect: attrs.override_redirect(),
            popup_for: None,
            dims: WindowDims {
                x: geometry.x(),
                y: geometry.y(),
                width: geometry.width(),
                height: geometry.height(),
            },
            title,
            class,
            group: wm_hints.and_then(|h| h.window_group),
            size_hints,
        }
    }

    fn handle_window_attributes(
        &self,
        server_state: &mut super::RealServerState,
        window: x::Window,
        attrs: WindowAttributes,
    ) {
        if let Some(name) = attrs.title {
            debug!("setting {window:?} title to {name:?}");
            server_state.set_win_title(window, name);
        }
        if let Some(class) = attrs.class {
            debug!("setting {window:?} class to {class}");
            server_state.set_win_class(window, class);
        }
        if let Some(hints) = attrs.size_hints {
            debug!("{window:?} size hints: {hints:?}");
            server_state.set_size_hints(window, hints);
        }
    }

    fn get_property_cookie(
        &self,
        window: x::Window,
        property: x::Atom,
        ty: x::Atom,
        long_length: u32,
    ) -> x::GetPropertyCookie {
        self.connection.send_request(&x::GetProperty {
            delete: false,
            window,
            property,
            r#type: ty,
            long_offset: 0,
            long_length,
        })
    }

    fn get_wm_class(
        &self,
        window: x::Window,
    ) -> PropertyCookieWrapper<impl PropertyResolver<Output = String>> {
        let cookie = self.get_property_cookie(window, x::ATOM_WM_CLASS, x::ATOM_STRING, 256);
        let resolver = move |reply: x::GetPropertyReply| {
            let data: &[u8] = reply.value();
            // wm class is instance + class - ignore instance
            let class_start = data.iter().copied().position(|b| b == 0u8).unwrap() + 1;
            let data = data[class_start..].to_vec();
            let class = CString::from_vec_with_nul(data).unwrap();
            debug!("{:?} class: {class:?}", window);
            class.to_string_lossy().to_string()
        };
        PropertyCookieWrapper {
            connection: &self.connection,
            cookie,
            resolver,
        }
    }

    fn get_wm_name(
        &self,
        window: x::Window,
    ) -> PropertyCookieWrapper<impl PropertyResolver<Output = WmName>> {
        let cookie = self.get_property_cookie(window, x::ATOM_WM_NAME, x::ATOM_STRING, 256);
        let resolver = |reply: x::GetPropertyReply| {
            let data: &[u8] = reply.value();
            WmName::WmName(String::from_utf8(data.to_vec()).unwrap())
        };

        PropertyCookieWrapper {
            connection: &self.connection,
            cookie,
            resolver,
        }
    }

    fn get_net_wm_name(
        &self,
        window: x::Window,
    ) -> PropertyCookieWrapper<impl PropertyResolver<Output = WmName>> {
        let cookie =
            self.get_property_cookie(window, self.atoms.net_wm_name, self.atoms.utf8_string, 256);
        let resolver = |reply: x::GetPropertyReply| {
            let data: &[u8] = reply.value();
            WmName::NetWmName(String::from_utf8(data.to_vec()).unwrap())
        };

        PropertyCookieWrapper {
            connection: &self.connection,
            cookie,
            resolver,
        }
    }

    fn get_wm_hints(
        &self,
        window: x::Window,
    ) -> PropertyCookieWrapper<impl PropertyResolver<Output = WmHints>> {
        let cookie = self.get_property_cookie(window, x::ATOM_WM_HINTS, x::ATOM_WM_HINTS, 9);
        let resolver = |reply: x::GetPropertyReply| {
            let data: &[u32] = reply.value();
            let hints = WmHints::from(data);
            debug!("wm hints: {hints:?}");
            hints
        };
        PropertyCookieWrapper {
            connection: &self.connection,
            cookie,
            resolver,
        }
    }

    fn get_wm_size_hints(
        &self,
        window: x::Window,
    ) -> PropertyCookieWrapper<impl PropertyResolver<Output = WmNormalHints>> {
        let cookie =
            self.get_property_cookie(window, x::ATOM_WM_NORMAL_HINTS, x::ATOM_WM_SIZE_HINTS, 9);
        let resolver = |reply: x::GetPropertyReply| {
            let data: &[u32] = reply.value();
            WmNormalHints::from(data)
        };

        PropertyCookieWrapper {
            connection: &self.connection,
            cookie,
            resolver,
        }
    }

    fn handle_property_change(
        &self,
        event: x::PropertyNotifyEvent,
        server_state: &mut super::RealServerState,
    ) {
        if event.state() != x::Property::NewValue {
            println!("ignoring non newvalue for property {:?}", event.atom());
            return;
        }

        let window = event.window();
        let get_prop = |r#type, long_length| {
            self.connection
                .wait_for_reply(self.connection.send_request(&x::GetProperty {
                    window,
                    property: event.atom(),
                    r#type,
                    long_offset: 0,
                    long_length,
                    delete: false,
                }))
        };

        match event.atom() {
            x if x == x::ATOM_WM_HINTS => {
                let hints = self.get_wm_hints(window).resolve().unwrap();
                server_state.set_win_hints(window, hints);
            }
            x if x == x::ATOM_WM_NORMAL_HINTS => {
                let hints = self.get_wm_size_hints(window).resolve().unwrap();
                server_state.set_size_hints(window, hints);
            }
            x if x == x::ATOM_WM_NAME || x == self.atoms.net_wm_name => {
                let ty = if x == x::ATOM_WM_NAME {
                    x::ATOM_STRING
                } else {
                    self.atoms.utf8_string
                };
                let prop = get_prop(ty, 256).unwrap();
                let data: &[u8] = prop.value();
                let name = String::from_utf8(data.to_vec()).unwrap();
                debug!("{:?} named: {name}", window);
                let name = if x == x::ATOM_WM_NAME {
                    WmName::WmName(name)
                } else {
                    WmName::NetWmName(name)
                };
                server_state.set_win_title(window, name);
            }
            x if x == x::ATOM_WM_CLASS => {
                let class = self.get_wm_class(window).resolve().unwrap();
                server_state.set_win_class(window, class);
            }
            _ => {
                if log::log_enabled!(log::Level::Debug) {
                    let prop = self
                        .connection
                        .wait_for_reply(
                            self.connection
                                .send_request(&x::GetAtomName { atom: event.atom() }),
                        )
                        .unwrap();

                    debug!("changed property {:?} for {:?}", prop.name(), window);
                }
            }
        }
    }
}

xcb::atoms_struct! {
    #[derive(Clone, Debug)]
    pub struct Atoms {
        pub wl_surface_id => b"WL_SURFACE_ID" only_if_exists = false,
        pub wl_surface_serial => b"WL_SURFACE_SERIAL" only_if_exists = false,
        pub wm_protocols => b"WM_PROTOCOLS" only_if_exists = false,
        pub wm_delete_window => b"WM_DELETE_WINDOW" only_if_exists = false,
        pub wm_transient_for => b"WM_TRANSIENT_FOR" only_if_exists = false,
        pub wm_check => b"_NET_SUPPORTING_WM_CHECK" only_if_exists = false,
        pub net_wm_name => b"_NET_WM_NAME" only_if_exists = false,
        pub wm_pid => b"_NET_WM_PID" only_if_exists = false,
        pub net_wm_state => b"_NET_WM_STATE" only_if_exists = false,
        pub wm_fullscreen => b"_NET_WM_STATE_FULLSCREEN" only_if_exists = false,
        pub active_win => b"_NET_ACTIVE_WINDOW" only_if_exists = false,
        pub client_list => b"_NET_CLIENT_LIST" only_if_exists = false,
        pub supported => b"_NET_SUPPORTED" only_if_exists = false,
        pub utf8_string => b"UTF8_STRING" only_if_exists = false,
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
        const ProgramMinSize = 16;
        const ProgramMaxSize = 32;
    }
}

bitflags! {
    /// https://tronche.com/gui/x/icccm/sec-4.html#s-4.1.2.4
    pub struct WmHintsFlags: u32 {
        const Input = 1;
        const WindowGroup = 64;
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

impl From<&[u32]> for WmNormalHints {
    fn from(value: &[u32]) -> Self {
        let mut ret = Self::default();
        let flags = WmSizeHintsFlags::from_bits_truncate(value[0]);

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

#[derive(Default, Debug, PartialEq, Eq)]
pub struct WmHints {
    pub input: Option<bool>,
    pub window_group: Option<x::Window>,
}

impl From<&[u32]> for WmHints {
    fn from(value: &[u32]) -> Self {
        let mut ret = Self::default();
        let flags = WmHintsFlags::from_bits_truncate(value[0]);

        if flags.contains(WmHintsFlags::Input) {
            ret.input = Some(value[1] == 1);
        }
        if flags.contains(WmHintsFlags::WindowGroup) {
            let window = unsafe { x::Window::new(value[8]) };
            ret.window_group = Some(window);
        }

        ret
    }
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
                property: x::ATOM_WM_HINTS,
                r#type: x::ATOM_WM_HINTS,
                long_offset: 0,
                long_length: 9,
            }))
            .unwrap();

        let set_focus = if prop.r#type() == x::ATOM_NONE {
            true
        } else {
            WmHints::from(prop.value()).input.unwrap_or(true)
        };

        if set_focus {
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
