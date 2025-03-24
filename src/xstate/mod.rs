mod selection;
use selection::{Selection, SelectionData};

use crate::{server::WindowAttributes, XConnection};
use bitflags::bitflags;
use log::{debug, trace, warn};
use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::rc::Rc;
use xcb::{x, Xid, XidNew};
use xcb_util_cursor::{Cursor, CursorContext};

// Sometimes we'll get events on windows that have already been destroyed
#[derive(Debug)]
enum MaybeBadWindow {
    BadWindow,
    Other(xcb::Error),
}
impl From<xcb::Error> for MaybeBadWindow {
    fn from(value: xcb::Error) -> Self {
        match value {
            xcb::Error::Protocol(xcb::ProtocolError::X(
                x::Error::Window(_) | x::Error::Drawable(_),
                _,
            )) => Self::BadWindow,
            other => Self::Other(other),
        }
    }
}
impl From<xcb::ProtocolError> for MaybeBadWindow {
    fn from(value: xcb::ProtocolError) -> Self {
        match value {
            xcb::ProtocolError::X(x::Error::Window(_) | x::Error::Drawable(_), _) => {
                Self::BadWindow
            }
            other => Self::Other(xcb::Error::Protocol(other)),
        }
    }
}

type XResult<T> = Result<T, MaybeBadWindow>;
macro_rules! unwrap_or_skip_bad_window {
    ($err:expr) => {
        match $err {
            Ok(v) => v,
            Err(e) => {
                let err = MaybeBadWindow::from(e);
                match err {
                    MaybeBadWindow::BadWindow => return,
                    MaybeBadWindow::Other(other) => panic!("X11 protocol error: {other:?}"),
                }
            }
        }
    };
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
    fn resolve(self) -> XResult<Option<F::Output>> {
        let reply = self.connection.wait_for_reply(self.cookie)?;
        if reply.r#type() == x::ATOM_NONE {
            Ok(None)
        } else {
            Ok(Some(self.resolver.resolve(reply)))
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

pub struct XState {
    connection: Rc<xcb::Connection>,
    atoms: Atoms,
    root: x::Window,
    wm_window: x::Window,
    selection_data: SelectionData,
}

impl XState {
    pub fn new(fd: BorrowedFd) -> Self {
        let connection = Rc::new(
            xcb::Connection::connect_to_fd_with_extensions(
                fd.as_raw_fd(),
                None,
                &[
                    xcb::Extension::Composite,
                    xcb::Extension::RandR,
                    xcb::Extension::XFixes,
                    xcb::Extension::Res,
                ],
                &[],
            )
            .unwrap(),
        );
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

        // Track RandR output changes
        connection
            .send_and_check_request(&xcb::randr::SelectInput {
                window: root,
                enable: xcb::randr::NotifyMask::RESOURCE_CHANGE,
            })
            .unwrap();

        // negotiate xfixes version
        let reply = connection
            .wait_for_reply(connection.send_request(&xcb::xfixes::QueryVersion {
                client_major_version: 1,
                client_minor_version: 0,
            }))
            .unwrap();
        log::info!(
            "xfixes version: {}.{}",
            reply.major_version(),
            reply.minor_version()
        );
        use xcb::xfixes::SelectionEventMask;
        connection
            .send_and_check_request(&xcb::xfixes::SelectSelectionInput {
                window: root,
                selection: atoms.clipboard,
                event_mask: SelectionEventMask::SET_SELECTION_OWNER
                    | SelectionEventMask::SELECTION_WINDOW_DESTROY
                    | SelectionEventMask::SELECTION_CLIENT_CLOSE,
            })
            .unwrap();
        {
            // Setup default cursor theme
            let ctx = CursorContext::new(&connection, screen).unwrap();
            let left_ptr = ctx.load_cursor(Cursor::LeftPtr);
            connection
                .send_and_check_request(&x::ChangeWindowAttributes {
                    window: root,
                    value_list: &[x::Cw::Cursor(left_ptr)],
                })
                .unwrap();
        }

        let wm_window = connection.generate_id();
        let selection_data = SelectionData::new(&connection, root);

        let mut r = Self {
            connection,
            wm_window,
            root,
            atoms,
            selection_data,
        };
        r.create_ewmh_window();
        r
    }

    pub fn server_state_setup(&self, server_state: &mut super::RealServerState) {
        let mut c = RealConnection::new(self.connection.clone(), self.atoms.clone());
        c.update_outputs(self.root);
        server_state.set_x_connection(c);
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
        self.connection
            .send_and_check_request(&x::CreateWindow {
                depth: 0,
                wid: self.wm_window,
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

        self.set_root_property(self.atoms.wm_check, x::ATOM_WINDOW, &[self.wm_window]);
        self.set_root_property(self.atoms.active_win, x::ATOM_WINDOW, &[x::Window::none()]);
        self.set_root_property(
            self.atoms.supported,
            x::ATOM_ATOM,
            &[self.atoms.active_win, self.atoms.wm_opacity],
        );

        self.connection
            .send_and_check_request(&x::ChangeProperty {
                mode: x::PropMode::Replace,
                window: self.wm_window,
                property: self.atoms.wm_check,
                r#type: x::ATOM_WINDOW,
                data: &[self.wm_window],
            })
            .unwrap();

        self.connection
            .send_and_check_request(&x::ChangeProperty {
                mode: x::PropMode::Replace,
                window: self.wm_window,
                property: self.atoms.net_wm_name,
                r#type: x::ATOM_STRING,
                data: b"xwayland-satellite",
            })
            .unwrap();
    }

    pub fn handle_events(&mut self, server_state: &mut super::RealServerState) {
        macro_rules! unwrap_or_skip_bad_window_cont {
            ($err:expr) => {
                match $err {
                    Ok(v) => v,
                    Err(e) => {
                        let err = MaybeBadWindow::from(e);
                        match err {
                            MaybeBadWindow::BadWindow => continue,
                            MaybeBadWindow::Other(other) => panic!("X11 protocol error: {other:?}"),
                        }
                    }
                }
            };
        }

        let mut ignored_windows = Vec::new();
        while let Some(event) = self.connection.poll_for_event().unwrap() {
            trace!("x11 event: {event:?}");

            if self.handle_selection_event(&event, server_state) {
                continue;
            }

            match event {
                xcb::Event::X(x::Event::CreateNotify(e)) => {
                    debug!("new window: {:?}", e);
                    let parent = e.parent();
                    let parent = if parent.is_none() || parent == self.root {
                        None
                    } else {
                        Some(parent)
                    };
                    server_state.new_window(
                        e.window(),
                        e.override_redirect(),
                        (&e).into(),
                        parent,
                        self.get_pid(e.window()),
                    );
                }
                xcb::Event::X(x::Event::ReparentNotify(e)) => {
                    debug!("reparent event: {e:?}");
                    if e.parent() == self.root {
                        let attrs =
                            unwrap_or_skip_bad_window_cont!(self.get_window_attributes(e.window()));
                        server_state.new_window(
                            e.window(),
                            attrs.override_redirect,
                            attrs.dims,
                            None,
                            self.get_pid(e.window()),
                        );
                        self.handle_window_attributes(server_state, e.window(), attrs);
                    } else {
                        debug!("destroying window since its parent is no longer root!");
                        server_state.destroy_window(e.window());
                        ignored_windows.push(e.window());
                    }
                }
                xcb::Event::X(x::Event::MapRequest(e)) => {
                    debug!("requested to map {:?}", e.window());
                    unwrap_or_skip_bad_window_cont!(self
                        .connection
                        .send_and_check_request(&x::MapWindow { window: e.window() }));
                }
                xcb::Event::X(x::Event::MapNotify(e)) => {
                    unwrap_or_skip_bad_window_cont!(self.connection.send_and_check_request(
                        &x::ChangeWindowAttributes {
                            window: e.window(),
                            value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
                        }
                    ));
                    let attrs =
                        unwrap_or_skip_bad_window_cont!(self.get_window_attributes(e.window()));
                    self.handle_window_attributes(server_state, e.window(), attrs);
                    server_state.map_window(e.window());
                }
                xcb::Event::X(x::Event::ConfigureNotify(e)) => {
                    server_state.reconfigure_window(e);
                }
                xcb::Event::X(x::Event::UnmapNotify(e)) => {
                    trace!("unmap event: {:?}", e.event());
                    server_state.unmap_window(e.window());
                    let active_win = self
                        .connection
                        .wait_for_reply(self.get_property_cookie(
                            self.root,
                            self.atoms.active_win,
                            x::ATOM_WINDOW,
                            1,
                        ))
                        .unwrap();

                    let active_win: &[x::Window] = active_win.value();
                    if active_win[0] == e.window() {
                        // The connection on the server state stores state.
                        server_state
                            .connection
                            .as_mut()
                            .unwrap()
                            .focus_window(x::Window::none(), None);
                    }

                    unwrap_or_skip_bad_window_cont!(self.connection.send_and_check_request(
                        &x::ChangeWindowAttributes {
                            window: e.window(),
                            value_list: &[x::Cw::EventMask(x::EventMask::empty())],
                        }
                    ));
                }
                xcb::Event::X(x::Event::DestroyNotify(e)) => {
                    debug!("destroying window {:?}", e.window());
                    server_state.destroy_window(e.window());
                }
                xcb::Event::X(x::Event::PropertyNotify(e)) => {
                    if ignored_windows.contains(&e.window()) {
                        continue;
                    }
                    self.handle_property_change(e, server_state);
                }
                xcb::Event::X(x::Event::ConfigureRequest(e)) => {
                    if !server_state.can_reconfigure_window(e.window()) {
                        debug!("ignoring reconfigure request for {:?}", e.window());
                        continue;
                    }
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

                    unwrap_or_skip_bad_window_cont!(self.connection.send_and_check_request(
                        &x::ConfigureWindow {
                            window: e.window(),
                            value_list: &list,
                        }
                    ));
                }
                xcb::Event::X(x::Event::ClientMessage(e)) => match e.r#type() {
                    x if x == self.atoms.wl_surface_id => {
                        panic!(concat!(
                            "Xserver should be using WL_SURFACE_SERIAL, not WL_SURFACE_ID\n",
                            "Your Xwayland is likely too old, it should be version 23.1 or greater."
                        ));
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
                    x if x == self.atoms.active_win => {
                        server_state.activate_window(e.window());
                    }
                    t => warn!("unrecognized message: {t:?}"),
                },
                xcb::Event::X(x::Event::MappingNotify(_)) => {}
                xcb::Event::RandR(xcb::randr::Event::Notify(e))
                    if matches!(e.u(), xcb::randr::NotifyData::Rc(_)) =>
                {
                    server_state
                        .connection
                        .as_mut()
                        .unwrap()
                        .update_outputs(self.root);
                }
                other => {
                    warn!("unhandled event: {other:?}");
                }
            }

            server_state.run();
        }
    }

    fn get_window_attributes(&self, window: x::Window) -> XResult<WindowAttributes> {
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
        let opacity = self.get_wm_opacity(window);

        let geometry = self.connection.wait_for_reply(geometry)?;
        debug!("{window:?} geometry: {geometry:?}");
        let attrs = self.connection.wait_for_reply(attrs)?;
        let mut title = name.resolve()?;
        if title.is_none() {
            title = self.get_wm_name(window).resolve()?;
        }

        let class = class.resolve()?;
        let wm_hints = wm_hints.resolve()?;
        let size_hints = size_hints.resolve()?;
        let opacity = opacity.resolve()?;

        Ok(WindowAttributes {
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
            opacity,
        })
    }

    fn handle_window_attributes(
        &self,
        server_state: &mut super::RealServerState,
        window: x::Window,
        attrs: WindowAttributes,
    ) {
        if let Some(name) = attrs.title {
            server_state.set_win_title(window, name);
        }
        if let Some(class) = attrs.class {
            server_state.set_win_class(window, class);
        }
        if let Some(hints) = attrs.size_hints {
            server_state.set_size_hints(window, hints);
        }
        if let Some(opacity) = attrs.opacity {
            server_state.set_win_opacity(window, opacity);
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
            trace!("wm class data: {data:?}");
            // wm class (normally) is instance + class - ignore instance
            let class_start = if let Some(p) = data.iter().copied().position(|b| b == 0u8) {
                p + 1
            } else {
                0
            };
            let mut data = data[class_start..].to_vec();
            if data.last().copied() != Some(0) {
                data.push(0);
            }
            let class = CString::from_vec_with_nul(data).unwrap();
            trace!("{:?} class: {class:?}", window);
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
            // strip trailing zeros or wayland-rs will lose its mind
            // https://github.com/Smithay/wayland-rs/issues/748
            let data = data.split(|byte| *byte == 0).next().unwrap();
            let name = String::from_utf8_lossy(data).to_string();
            WmName::WmName(name)
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
            let data = data.split(|byte| *byte == 0).next().unwrap();
            let name = String::from_utf8_lossy(data).to_string();
            WmName::NetWmName(name)
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
            trace!("wm hints: {hints:?}");
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

    fn get_wm_opacity(
        &self,
        window: x::Window,
    ) -> PropertyCookieWrapper<impl PropertyResolver<Output = u32>> {
        let cookie = self.get_property_cookie(window, self.atoms.wm_opacity, x::ATOM_CARDINAL, 1);
        let resolver = |reply: x::GetPropertyReply| reply.value()[0];

        PropertyCookieWrapper {
            connection: &self.connection,
            cookie,
            resolver,
        }
    }

    fn get_pid(&self, window: x::Window) -> Option<u32> {
        let Some(pid) = self
            .connection
            .wait_for_reply(self.connection.send_request(&xcb::res::QueryClientIds {
                specs: &[xcb::res::ClientIdSpec {
                    client: window.resource_id(),
                    mask: xcb::res::ClientIdMask::LOCAL_CLIENT_PID,
                }],
            }))
            .ok()
            .and_then(|reply| Some(*reply.ids().next()?.value().first()?))
        else {
            warn!("Failed to get pid of window: {window:?}");
            return None;
        };
        Some(pid)
    }

    fn handle_property_change(
        &mut self,
        event: x::PropertyNotifyEvent,
        server_state: &mut super::RealServerState,
    ) {
        if event.state() != x::Property::NewValue {
            debug!(
                "ignoring non newvalue for property {:?}",
                get_atom_name(&self.connection, event.atom())
            );
            return;
        }

        let window = event.window();

        match event.atom() {
            x if x == x::ATOM_WM_HINTS => {
                let hints =
                    unwrap_or_skip_bad_window!(self.get_wm_hints(window).resolve()).unwrap();
                server_state.set_win_hints(window, hints);
            }
            x if x == x::ATOM_WM_NORMAL_HINTS => {
                let hints =
                    unwrap_or_skip_bad_window!(self.get_wm_size_hints(window).resolve()).unwrap();
                server_state.set_size_hints(window, hints);
            }
            x if x == x::ATOM_WM_NAME => {
                let name = unwrap_or_skip_bad_window!(self.get_wm_name(window).resolve()).unwrap();
                server_state.set_win_title(window, name);
            }
            x if x == self.atoms.net_wm_name => {
                let name =
                    unwrap_or_skip_bad_window!(self.get_net_wm_name(window).resolve()).unwrap();
                server_state.set_win_title(window, name);
            }
            x if x == x::ATOM_WM_CLASS => {
                let class =
                    unwrap_or_skip_bad_window!(self.get_wm_class(window).resolve()).unwrap();
                server_state.set_win_class(window, class);
            }
            x if x == self.atoms.wm_opacity => {
                let opacity =
                    unwrap_or_skip_bad_window!(self.get_wm_opacity(window).resolve()).unwrap();
                server_state.set_win_opacity(window, opacity);
            }
            _ => {
                if !self.handle_selection_property_change(&event)
                    && log::log_enabled!(log::Level::Debug)
                {
                    debug!(
                        "changed property {:?} for {:?}",
                        get_atom_name(&self.connection, event.atom()),
                        window
                    );
                }
            }
        }
    }
}

xcb::atoms_struct! {
    #[derive(Clone, Debug)]
    struct Atoms {
        wl_surface_id => b"WL_SURFACE_ID" only_if_exists = false,
        wl_surface_serial => b"WL_SURFACE_SERIAL" only_if_exists = false,
        wm_protocols => b"WM_PROTOCOLS" only_if_exists = false,
        wm_delete_window => b"WM_DELETE_WINDOW" only_if_exists = false,
        wm_transient_for => b"WM_TRANSIENT_FOR" only_if_exists = false,
        wm_state => b"WM_STATE" only_if_exists = false,
        wm_check => b"_NET_SUPPORTING_WM_CHECK" only_if_exists = false,
        net_wm_name => b"_NET_WM_NAME" only_if_exists = false,
        wm_pid => b"_NET_WM_PID" only_if_exists = false,
        net_wm_state => b"_NET_WM_STATE" only_if_exists = false,
        wm_fullscreen => b"_NET_WM_STATE_FULLSCREEN" only_if_exists = false,
        active_win => b"_NET_ACTIVE_WINDOW" only_if_exists = false,
        client_list => b"_NET_CLIENT_LIST" only_if_exists = false,
        wm_opacity => b"_NET_WM_WINDOW_OPACITY" only_if_exists = false,
        supported => b"_NET_SUPPORTED" only_if_exists = false,
        utf8_string => b"UTF8_STRING" only_if_exists = false,
        clipboard => b"CLIPBOARD" only_if_exists = false,
        targets => b"TARGETS" only_if_exists = false,
        save_targets => b"SAVE_TARGETS" only_if_exists = false,
        multiple => b"MULTIPLE" only_if_exists = false,
        timestamp => b"TIMESTAMP" only_if_exists = false,
        selection_reply => b"_selection_reply" only_if_exists = false,
        incr => b"INCR" only_if_exists = false,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
    pub window_group: Option<x::Window>,
}

impl From<&[u32]> for WmHints {
    fn from(value: &[u32]) -> Self {
        let mut ret = Self::default();
        let flags = WmHintsFlags::from_bits_truncate(value[0]);

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WmState {
    Withdrawn = 0,
    Normal = 1,
    Iconic = 3,
}

impl TryFrom<u32> for WmState {
    type Error = ();
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Withdrawn),
            1 => Ok(Self::Normal),
            3 => Ok(Self::Iconic),
            _ => Err(()),
        }
    }
}

pub struct RealConnection {
    atoms: Atoms,
    connection: Rc<xcb::Connection>,
    outputs: HashMap<String, xcb::randr::Output>,
    primary_output: xcb::randr::Output,
}

impl RealConnection {
    fn new(connection: Rc<xcb::Connection>, atoms: Atoms) -> Self {
        Self {
            atoms,
            connection,
            outputs: Default::default(),
            primary_output: Xid::none(),
        }
    }

    fn update_outputs(&mut self, root: x::Window) {
        self.outputs.clear();
        let reply = self
            .connection
            .wait_for_reply(
                self.connection
                    .send_request(&xcb::randr::GetScreenResources { window: root }),
            )
            .expect("Couldn't grab screen resources");

        for output in reply.outputs().iter().copied() {
            let reply = self
                .connection
                .wait_for_reply(self.connection.send_request(&xcb::randr::GetOutputInfo {
                    output,
                    config_timestamp: reply.config_timestamp(),
                }))
                .expect("Couldn't get output info");

            let name = std::str::from_utf8(reply.name())
                .unwrap_or_else(|_| panic!("couldn't parse output name: {:?}", reply.name()));

            self.outputs.insert(name.to_string(), output);
        }

        self.primary_output = self
            .connection
            .wait_for_reply(
                self.connection
                    .send_request(&xcb::randr::GetOutputPrimary { window: root }),
            )
            .expect("Couldn't get primary output")
            .output();

        debug!(
            "new outputs: {:?} | primary: {:?}",
            self.outputs, self.primary_output
        );
    }
}

impl XConnection for RealConnection {
    type X11Selection = Selection;

    fn root_window(&self) -> x::Window {
        self.connection.get_setup().roots().next().unwrap().root()
    }

    fn set_window_dims(&mut self, window: x::Window, dims: crate::server::PendingSurfaceState) {
        trace!("set window dimensions {window:?} {dims:?}");
        unwrap_or_skip_bad_window!(self.connection.send_and_check_request(&x::ConfigureWindow {
            window,
            value_list: &[
                x::ConfigWindow::X(dims.x),
                x::ConfigWindow::Y(dims.y),
                x::ConfigWindow::Width(dims.width as _),
                x::ConfigWindow::Height(dims.height as _),
            ]
        }));
    }

    fn set_fullscreen(&mut self, window: x::Window, fullscreen: bool) {
        let data = if fullscreen {
            std::slice::from_ref(&self.atoms.wm_fullscreen)
        } else {
            &[]
        };

        if let Err(e) = self
            .connection
            .send_and_check_request(&x::ChangeProperty::<x::Atom> {
                mode: x::PropMode::Replace,
                window,
                property: self.atoms.net_wm_state,
                r#type: x::ATOM_ATOM,
                data,
            })
        {
            warn!("Failed to set fullscreen state on {window:?} ({e})");
        }
    }

    fn focus_window(&mut self, window: x::Window, output_name: Option<String>) {
        trace!("{window:?} {output_name:?}");
        if let Err(e) = self.connection.send_and_check_request(&x::SetInputFocus {
            focus: window,
            revert_to: x::InputFocus::None,
            time: x::CURRENT_TIME,
        }) {
            debug!("SetInputFocus failed ({:?}: {:?})", window, e);
            return;
        }
        if let Err(e) = self.connection.send_and_check_request(&x::ChangeProperty {
            mode: x::PropMode::Replace,
            window: self.root_window(),
            property: self.atoms.active_win,
            r#type: x::ATOM_WINDOW,
            data: &[window],
        }) {
            debug!("ChangeProperty failed ({:?}: {:?})", window, e);
        }
        if let Err(e) = self.connection.send_and_check_request(&x::ChangeProperty {
            mode: x::PropMode::Replace,
            window,
            property: self.atoms.wm_state,
            r#type: self.atoms.wm_state,
            data: &[WmState::Normal as u32, 0],
        }) {
            debug!("ChangeProperty failed ({:?}: {:?})", window, e);
        }

        if let Some(name) = output_name {
            let Some(output) = self.outputs.get(&name).copied() else {
                warn!("Couldn't find output {name}, primary output will be wrong");
                return;
            };
            if output == self.primary_output {
                debug!("primary output is already {name}");
                return;
            }

            if let Err(e) = self
                .connection
                .send_and_check_request(&xcb::randr::SetOutputPrimary { window, output })
            {
                warn!("Couldn't set output {name} as primary: {e:?}");
            } else {
                debug!("set {name} as primary output");
                self.primary_output = output;
            }
        } else {
            let _ = self
                .connection
                .send_and_check_request(&xcb::randr::SetOutputPrimary {
                    window,
                    output: Xid::none(),
                });
            self.primary_output = Xid::none();
        }
    }

    fn close_window(&mut self, window: x::Window) {
        let cookie = self.connection.send_request(&x::GetProperty {
            window,
            delete: false,
            property: self.atoms.wm_protocols,
            r#type: x::ATOM_ATOM,
            long_offset: 0,
            long_length: 10,
        });
        let reply = unwrap_or_skip_bad_window!(self.connection.wait_for_reply(cookie));

        if reply
            .value::<x::Atom>()
            .contains(&self.atoms.wm_delete_window)
        {
            let data = [self.atoms.wm_delete_window.resource_id(), 0, 0, 0, 0];
            let event = &x::ClientMessageEvent::new(
                window,
                self.atoms.wm_protocols,
                x::ClientMessageData::Data32(data),
            );

            unwrap_or_skip_bad_window!(self.connection.send_and_check_request(&x::SendEvent {
                destination: x::SendEventDest::Window(window),
                propagate: false,
                event_mask: x::EventMask::empty(),
                event,
            }));
        } else {
            unwrap_or_skip_bad_window!(self.connection.send_and_check_request(&x::KillClient {
                resource: window.resource_id()
            }))
        }
    }

    fn unmap_window(&mut self, window: x::Window) {
        unwrap_or_skip_bad_window!(self
            .connection
            .send_and_check_request(&x::UnmapWindow { window }));
    }

    fn raise_to_top(&mut self, window: x::Window) {
        unwrap_or_skip_bad_window!(self.connection.send_and_check_request(&x::ConfigureWindow {
            window,
            value_list: &[x::ConfigWindow::StackMode(x::StackMode::Above)],
        }));
    }
}

fn get_atom_name(connection: &xcb::Connection, atom: x::Atom) -> String {
    match connection.wait_for_reply(connection.send_request(&x::GetAtomName { atom })) {
        Ok(reply) => reply.name().to_string(),
        Err(err) => format!("<error getting atom name: {err:?}> {atom:?}"),
    }
}
