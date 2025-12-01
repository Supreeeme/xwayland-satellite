mod settings;
use settings::Settings;
mod selection;
use selection::{Selection, SelectionState};
use wayland_protocols::xdg::decoration::zv1::client::zxdg_toplevel_decoration_v1;

use crate::XConnection;
use bitflags::bitflags;
use log::{debug, trace, warn};
use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::BorrowedFd;
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
    ($err:expr, $skip:expr) => {
        match $err {
            Ok(v) => v,
            Err(e) => {
                let err = MaybeBadWindow::from(e);
                match err {
                    MaybeBadWindow::BadWindow => $skip,
                    MaybeBadWindow::Other(other) => panic!("X11 protocol error: {other:?}"),
                }
            }
        }
    };
}
macro_rules! unwrap_or_skip_bad_window_ret {
    ($err:expr) => {
        unwrap_or_skip_bad_window!($err, return)
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
    window_atoms: WindowTypes,
    root: x::Window,
    wm_window: x::Window,
    selection_state: SelectionState,
    settings: Settings,
    max_req_bytes: usize,
}

impl XState {
    pub fn new(fd: BorrowedFd) -> Self {
        let connection = Rc::new(
            xcb::Connection::connect_with_fd_and_extensions(
                BorrowedFd::try_clone_to_owned(&fd).unwrap(),
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
        connection
            .send_and_check_request(&xcb::xfixes::SelectSelectionInput {
                window: root,
                selection: atoms.xsettings,
                event_mask: SelectionEventMask::SELECTION_WINDOW_DESTROY
                    | SelectionEventMask::SELECTION_CLIENT_CLOSE,
            })
            .unwrap();
        connection
            .send_and_check_request(&xcb::xfixes::SelectSelectionInput {
                window: root,
                selection: atoms.primary,
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
        let selection_state = SelectionState::new(&connection, root, &atoms);
        let window_atoms = WindowTypes::intern_all(&connection).unwrap();
        let settings = Settings::new(&connection, &atoms, root);
        // maximum-request-length is returned in units of 4 bytes.
        // Additionally, requests use 32 bytes of metadata which cannot store arbitrary data
        let max_req_bytes = (connection.get_maximum_request_length() * 4 - 32) as usize;

        let mut r = Self {
            connection,
            wm_window,
            root,
            atoms,
            window_atoms,
            selection_state,
            settings,
            max_req_bytes,
        };
        r.create_ewmh_window();
        r.set_xsettings_owner();
        r
    }

    pub(super) fn set_max_req_bytes(&mut self, max_req_bytes: usize) {
        // `max_req_bytes` is initialized to the largest possible value before transfer problems
        // would begin to occur. This function is called once during initialization only in
        // integration tests, so this overly simple check is fine.
        assert!(self.max_req_bytes >= max_req_bytes);
        self.max_req_bytes = max_req_bytes;
    }

    pub fn server_state_setup(
        &self,
        server_state: super::EarlyServerState,
    ) -> super::RealServerState {
        let mut c = RealConnection::new(self.connection.clone(), self.atoms.clone());
        c.update_outputs(self.root);
        server_state.upgrade_connection(c)
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
            &[
                self.atoms.active_win,
                self.atoms.motif_wm_hints,
                self.atoms.net_wm_state,
                self.atoms.wm_fullscreen,
            ],
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

        self.connection
            .send_and_check_request(&x::SetSelectionOwner {
                owner: self.wm_window,
                selection: self.atoms.wm_s0,
                time: x::CURRENT_TIME,
            })
            .unwrap();
    }

    pub fn handle_events(&mut self, server_state: &mut super::RealServerState) {
        macro_rules! unwrap_or_skip_bad_window_cont {
            ($err:expr) => {
                unwrap_or_skip_bad_window!($err, continue)
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
                    debug!("new window: {e:?}");
                    server_state.new_window(
                        e.window(),
                        e.override_redirect(),
                        (&e).into(),
                        self.get_pid(e.window()),
                    );
                }
                xcb::Event::X(x::Event::ReparentNotify(e)) => {
                    debug!("reparent event: {e:?}");
                    if e.parent() == self.root {
                        let geometry = self.connection.send_request(&x::GetGeometry {
                            drawable: x::Drawable::Window(e.window()),
                        });
                        let attrs = self
                            .connection
                            .send_request(&x::GetWindowAttributes { window: e.window() });
                        let geometry = unwrap_or_skip_bad_window_cont!(self
                            .connection
                            .wait_for_reply(geometry));
                        let attrs =
                            unwrap_or_skip_bad_window_cont!(self.connection.wait_for_reply(attrs));

                        server_state.new_window(
                            e.window(),
                            attrs.override_redirect(),
                            WindowDims {
                                x: geometry.x(),
                                y: geometry.y(),
                                width: geometry.width(),
                                height: geometry.height(),
                            },
                            self.get_pid(e.window()),
                        );
                    } else {
                        debug!("destroying window since its parent is no longer root!");
                        server_state.destroy_window(e.window());
                        ignored_windows.push(e.window());
                    }
                }
                xcb::Event::X(x::Event::MapRequest(e)) => {
                    debug!("requested to map {:?}", e.window());
                    unwrap_or_skip_bad_window_cont!(self.connection.send_and_check_request(
                        &x::ConfigureWindow {
                            window: e.window(),
                            value_list: &[x::ConfigWindow::StackMode(x::StackMode::Below)]
                        }
                    ));
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
                    unwrap_or_skip_bad_window_cont!(
                        self.handle_window_properties(server_state, e.window())
                    );
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
                    debug!("{:?} request: {:?}", e.window(), e.value_mask());

                    let mut list = Vec::new();
                    let mask = e.value_mask();

                    if server_state.can_change_position(e.window()) {
                        if mask.contains(x::ConfigWindowMask::X) {
                            list.push(x::ConfigWindow::X(e.x().into()));
                        }
                        if mask.contains(x::ConfigWindowMask::Y) {
                            list.push(x::ConfigWindow::Y(e.y().into()));
                        }
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
                xcb::Event::X(x::Event::ClientMessage(e)) => {
                    self.handle_client_message(e, server_state);
                }
                xcb::Event::X(x::Event::MappingNotify(_)) => {}
                xcb::Event::RandR(xcb::randr::Event::Notify(e))
                    if matches!(e.u(), xcb::randr::NotifyData::Rc(_)) =>
                {
                    server_state.connection.update_outputs(self.root);
                }
                other => {
                    warn!("unhandled event: {other:?}");
                }
            }

            server_state.run();
        }
    }

    fn handle_client_message(
        &self,
        e: x::ClientMessageEvent,
        server_state: &mut super::RealServerState,
    ) {
        match e.r#type() {
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
                    return;
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
            x if x == self.atoms.moveresize => {
                let x::ClientMessageData::Data32(data) = e.data() else {
                    unreachable!();
                };

                let (_x_root, _y_root) = (data[0], data[1]);
                let Ok(direction) = MoveResizeDirection::try_from(data[2]) else {
                    warn!("unknown direction for _NET_WM_MOVERESIZE: {}", data[2]);
                    return;
                };
                let button = data[3];
                // XXX: This can technically be driven by keyboard events and other mouse buttons as well,
                // but I haven't found an application that does this yet. We'll cross that bridge when we get to it.
                if button != 1 {
                    warn!(
                        "Attempted move/resize of {:?} with non left click button ({button})",
                        e.window()
                    );
                    return;
                }

                match direction {
                    MoveResizeDirection::Move => {
                        server_state.move_window(e.window());
                    }
                    MoveResizeDirection::SizeTopLeft
                    | MoveResizeDirection::SizeTop
                    | MoveResizeDirection::SizeTopRight
                    | MoveResizeDirection::SizeRight
                    | MoveResizeDirection::SizeBottomRight
                    | MoveResizeDirection::SizeBottom
                    | MoveResizeDirection::SizeBottomLeft
                    | MoveResizeDirection::SizeLeft => {
                        server_state.resize_window(e.window(), direction);
                    }
                    MoveResizeDirection::SizeKeyboard
                    | MoveResizeDirection::MoveKeyboard
                    | MoveResizeDirection::Cancel => {
                        warn!(
                            "Unimplemented window move/resize action: {direction:?} ({:?})",
                            e.window()
                        );
                    }
                }
            }
            t => warn!(
                "unrecognized message: {:?}",
                get_atom_name(&self.connection, t)
            ),
        }
    }

    fn handle_window_properties(
        &self,
        server_state: &mut super::RealServerState,
        window: x::Window,
    ) -> XResult<()> {
        let name = self.get_net_wm_name(window);
        let class = self.get_wm_class(window);
        let size_hints = self.get_wm_size_hints(window);
        let motif_wm_hints = self.get_motif_wm_hints(window);
        let mut title = name.resolve()?;
        if title.is_none() {
            title = self.get_wm_name(window).resolve()?;
        }

        if let Some(name) = title {
            server_state.set_win_title(window, name);
        }
        if let Some(class) = class.resolve()? {
            server_state.set_win_class(window, class);
        }
        if let Some(hints) = size_hints.resolve()? {
            server_state.set_size_hints(window, hints);
        }

        let motif_hints = motif_wm_hints.resolve()?;
        if let Some(decorations) = motif_hints.as_ref().and_then(|m| m.decorations) {
            server_state.set_win_decorations(window, decorations);
        }

        let transient_for = self
            .property_cookie_wrapper(
                window,
                self.atoms.wm_transient_for,
                x::ATOM_WINDOW,
                1,
                |reply: x::GetPropertyReply| reply.value::<x::Window>().first().copied(),
            )
            .resolve()?
            .flatten();

        let is_popup = self.guess_is_popup(window, motif_hints, transient_for.is_some())?;
        server_state.set_popup(window, is_popup);
        if let Some(parent) = transient_for.and_then(|t| (!is_popup).then_some(t)) {
            server_state.set_transient_for(window, parent);
        }

        Ok(())
    }

    fn property_cookie_wrapper<F: PropertyResolver>(
        &self,
        window: x::Window,
        property: x::Atom,
        ty: x::Atom,
        len: u32,
        resolver: F,
    ) -> PropertyCookieWrapper<'_, F> {
        PropertyCookieWrapper {
            connection: &self.connection,
            cookie: self.get_property_cookie(window, property, ty, len),
            resolver,
        }
    }

    fn guess_is_popup(
        &self,
        window: x::Window,
        motif_hints: Option<motif::Hints>,
        has_transient_for: bool,
    ) -> XResult<bool> {
        if let Some(hints) = motif_hints {
            // If the motif hints indicate the user shouldn't be able to do anything
            // to the window at all, it stands to reason it's probably a popup.
            if hints.functions.is_some_and(|f| f.is_empty()) {
                return Ok(true);
            }
        }

        let attrs = self
            .connection
            .send_request(&x::GetWindowAttributes { window });

        let atoms_vec = |reply: x::GetPropertyReply| reply.value::<x::Atom>().to_vec();
        let window_types =
            self.property_cookie_wrapper(window, self.window_atoms.ty, x::ATOM_ATOM, 10, atoms_vec);
        let window_state = self.property_cookie_wrapper(
            window,
            self.atoms.net_wm_state,
            x::ATOM_ATOM,
            10,
            atoms_vec,
        );

        let override_redirect = self.connection.wait_for_reply(attrs)?.override_redirect();
        let mut is_popup = override_redirect;

        let window_types = window_types.resolve()?.unwrap_or_else(|| {
            if !override_redirect && has_transient_for {
                vec![self.window_atoms.dialog]
            } else {
                vec![self.window_atoms.normal]
            }
        });

        if log::log_enabled!(log::Level::Debug) {
            let win_types = window_types
                .iter()
                .copied()
                .map(|t| get_atom_name(&self.connection, t))
                .collect::<Vec<_>>();

            debug!("{window:?} window_types: {win_types:?}");
        }
        debug!("{window:?} override_redirect: {override_redirect:?}");

        let mut known_window_type = false;
        for ty in window_types {
            match ty {
                x if x == self.window_atoms.normal || x == self.window_atoms.dialog => {
                    is_popup = override_redirect;
                }
                x if [
                    self.window_atoms.menu,
                    self.window_atoms.popup_menu,
                    self.window_atoms.dropdown_menu,
                    self.window_atoms.tooltip,
                    self.window_atoms.drag_n_drop,
                    self.window_atoms.utility,
                ]
                .contains(&x) =>
                {
                    is_popup = true;
                }
                _ => {
                    continue;
                }
            }

            known_window_type = true;
            break;
        }

        if !known_window_type {
            if let Some(states) = window_state.resolve()? {
                is_popup = states.contains(&self.atoms.skip_taskbar);
            }
        }

        Ok(is_popup)
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
    ) -> PropertyCookieWrapper<'_, impl PropertyResolver<Output = String>> {
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
            trace!("{window:?} class: {class:?}");
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
    ) -> PropertyCookieWrapper<'_, impl PropertyResolver<Output = WmName>> {
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
    ) -> PropertyCookieWrapper<'_, impl PropertyResolver<Output = WmName>> {
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
    ) -> PropertyCookieWrapper<'_, impl PropertyResolver<Output = WmHints>> {
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
    ) -> PropertyCookieWrapper<'_, impl PropertyResolver<Output = WmNormalHints>> {
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

    fn get_motif_wm_hints(
        &self,
        window: x::Window,
    ) -> PropertyCookieWrapper<'_, impl PropertyResolver<Output = motif::Hints>> {
        let cookie = self.get_property_cookie(
            window,
            self.atoms.motif_wm_hints,
            self.atoms.motif_wm_hints,
            5,
        );
        let resolver = |reply: x::GetPropertyReply| {
            let data: &[u32] = reply.value();
            motif::Hints::from(data)
        };

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
        if self.handle_selection_property_change(&event) {
            return;
        }
        if event.state() == x::Property::Delete {
            debug!(
                "ignoring delete for property {:?}",
                get_atom_name(&self.connection, event.atom())
            );
            return;
        }

        let window = event.window();

        match event.atom() {
            x if x == x::ATOM_WM_HINTS => {
                let hints =
                    unwrap_or_skip_bad_window_ret!(self.get_wm_hints(window).resolve()).unwrap();
                server_state.set_win_hints(window, hints);
            }
            x if x == x::ATOM_WM_NORMAL_HINTS => {
                let hints =
                    unwrap_or_skip_bad_window_ret!(self.get_wm_size_hints(window).resolve())
                        .unwrap();
                server_state.set_size_hints(window, hints);
            }
            x if x == x::ATOM_WM_NAME => {
                let name =
                    unwrap_or_skip_bad_window_ret!(self.get_wm_name(window).resolve()).unwrap();
                server_state.set_win_title(window, name);
            }
            x if x == self.atoms.net_wm_name => {
                let name =
                    unwrap_or_skip_bad_window_ret!(self.get_net_wm_name(window).resolve()).unwrap();
                server_state.set_win_title(window, name);
            }
            x if x == x::ATOM_WM_CLASS => {
                let class =
                    unwrap_or_skip_bad_window_ret!(self.get_wm_class(window).resolve()).unwrap();
                server_state.set_win_class(window, class);
            }
            x if x == self.atoms.motif_wm_hints => {
                let motif_hints =
                    unwrap_or_skip_bad_window_ret!(self.get_motif_wm_hints(window).resolve())
                        .unwrap();
                if let Some(decorations) = motif_hints.decorations {
                    server_state.set_win_decorations(window, decorations);
                }
            }
            _ => {
                if log::log_enabled!(log::Level::Debug) {
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
        wm_s0 => b"WM_S0" only_if_exists = false,
        wm_check => b"_NET_SUPPORTING_WM_CHECK" only_if_exists = false,
        net_wm_name => b"_NET_WM_NAME" only_if_exists = false,
        wm_pid => b"_NET_WM_PID" only_if_exists = false,
        net_wm_state => b"_NET_WM_STATE" only_if_exists = false,
        wm_fullscreen => b"_NET_WM_STATE_FULLSCREEN" only_if_exists = false,
        skip_taskbar => b"_NET_WM_STATE_SKIP_TASKBAR" only_if_exists = false,
        active_win => b"_NET_ACTIVE_WINDOW" only_if_exists = false,
        client_list => b"_NET_CLIENT_LIST" only_if_exists = false,
        supported => b"_NET_SUPPORTED" only_if_exists = false,
        motif_wm_hints => b"_MOTIF_WM_HINTS" only_if_exists = false,
        utf8_string => b"UTF8_STRING" only_if_exists = false,
        clipboard => b"CLIPBOARD" only_if_exists = false,
        clipboard_targets => b"_clipboard_targets" only_if_exists = false,
        targets => b"TARGETS" only_if_exists = false,
        save_targets => b"SAVE_TARGETS" only_if_exists = false,
        multiple => b"MULTIPLE" only_if_exists = false,
        timestamp => b"TIMESTAMP" only_if_exists = false,
        incr => b"INCR" only_if_exists = false,
        xsettings => b"_XSETTINGS_S0" only_if_exists = false,
        xsettings_settings => b"_XSETTINGS_SETTINGS" only_if_exists = false,
        primary => b"PRIMARY" only_if_exists = false,
        primary_targets => b"_primary_targets" only_if_exists = false,
        moveresize => b"_NET_WM_MOVERESIZE" only_if_exists = false,
    }
}

xcb::atoms_struct! {
    struct WindowTypes {
        ty => b"_NET_WM_WINDOW_TYPE" only_if_exists = false,
        normal => b"_NET_WM_WINDOW_TYPE_NORMAL" only_if_exists = false,
        dialog => b"_NET_WM_WINDOW_TYPE_DIALOG" only_if_exists = false,
        drag_n_drop => b"_NET_WM_WINDOW_TYPE_DND" only_if_exists = false,
        splash => b"_NET_WM_WINDOW_TYPE_SPLASH" only_if_exists = false,
        menu => b"_NET_WM_WINDOW_TYPE_MENU" only_if_exists = false,
        popup_menu => b"_NET_WM_WINDOW_TYPE_POPUP_MENU" only_if_exists = false,
        dropdown_menu => b"_NET_WM_WINDOW_TYPE_DROPDOWN_MENU" only_if_exists = false,
        utility => b"_NET_WM_WINDOW_TYPE_UTILITY" only_if_exists = false,
        tooltip => b"_NET_WM_WINDOW_TYPE_TOOLTIP" only_if_exists = false,
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

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WinSize {
    pub width: i32,
    pub height: i32,
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
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

pub use motif::Decorations;
mod motif {
    use super::*;
    // Motif WM hints are incredibly poorly documented, I could only find this header:
    // https://www.opengroup.org/infosrv/openmotif/R2.1.30/motif/lib/Xm/MwmUtil.h
    // and these random Perl docs:
    // https://metacpan.org/pod/X11::Protocol::WM#_MOTIF_WM_HINTS

    bitflags! {
        struct HintsFlags: u32 {
            const Functions = 1;
            const Decorations = 2;
        }
    }

    bitflags! {
        pub(super) struct Functions: u32 {
            const All = 1;
            const Resize = 2;
            const Move = 4;
            const Minimize = 8;
            const Maximize = 16;
            const Close = 32;
        }
    }

    #[derive(Default)]
    pub(super) struct Hints {
        pub(super) functions: Option<Functions>,
        pub(super) decorations: Option<Decorations>,
    }

    impl From<&[u32]> for Hints {
        fn from(value: &[u32]) -> Self {
            let mut ret = Self::default();

            let flags = HintsFlags::from_bits_truncate(value[0]);

            if flags.contains(HintsFlags::Functions) {
                ret.functions = Some(Functions::from_bits_truncate(value[1]));
            }
            if flags.contains(HintsFlags::Decorations) {
                ret.decorations = value[2].try_into().ok();
            }

            ret
        }
    }

    #[derive(Debug, PartialEq, Eq, Clone, Copy, num_enum::TryFromPrimitive)]
    #[repr(u32)]
    pub enum Decorations {
        Client = 0,
        Server = 1,
    }

    impl From<Decorations> for zxdg_toplevel_decoration_v1::Mode {
        fn from(value: Decorations) -> Self {
            match value {
                Decorations::Client => zxdg_toplevel_decoration_v1::Mode::ClientSide,
                Decorations::Server => zxdg_toplevel_decoration_v1::Mode::ServerSide,
            }
        }
    }
}

#[derive(Debug, Clone, Copy, num_enum::TryFromPrimitive)]
#[repr(u32)]
pub enum SetState {
    Remove,
    Add,
    Toggle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, num_enum::TryFromPrimitive)]
#[repr(u32)]
pub enum WmState {
    Withdrawn = 0,
    Normal = 1,
    Iconic = 3,
}

#[derive(Debug, Copy, Clone, num_enum::TryFromPrimitive, num_enum::IntoPrimitive)]
#[repr(u32)]
pub enum MoveResizeDirection {
    SizeTopLeft,
    SizeTop,
    SizeTopRight,
    SizeRight,
    SizeBottomRight,
    SizeBottom,
    SizeBottomLeft,
    SizeLeft,
    Move,
    SizeKeyboard,
    MoveKeyboard,
    Cancel,
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

    fn root_window(&self) -> x::Window {
        self.connection.get_setup().roots().next().unwrap().root()
    }
}

impl XConnection for RealConnection {
    type X11Selection = Selection;
    fn set_window_dims(
        &mut self,
        window: x::Window,
        dims: crate::server::PendingSurfaceState,
    ) -> bool {
        trace!("set window dimensions {window:?} {dims:?}");
        unwrap_or_skip_bad_window!(
            self.connection.send_and_check_request(&x::ConfigureWindow {
                window,
                value_list: &[
                    x::ConfigWindow::X(dims.x),
                    x::ConfigWindow::Y(dims.y),
                    x::ConfigWindow::Width(dims.width as _),
                    x::ConfigWindow::Height(dims.height as _),
                ]
            }),
            return false
        );
        true
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
            debug!("SetInputFocus failed ({window:?}: {e:?})");
            return;
        }
        if let Err(e) = self.connection.send_and_check_request(&x::ChangeProperty {
            mode: x::PropMode::Replace,
            window: self.root_window(),
            property: self.atoms.active_win,
            r#type: x::ATOM_WINDOW,
            data: &[window],
        }) {
            debug!("ChangeProperty failed ({window:?}: {e:?})");
        }
        if let Err(e) = self.connection.send_and_check_request(&x::ChangeProperty {
            mode: x::PropMode::Replace,
            window,
            property: self.atoms.wm_state,
            r#type: self.atoms.wm_state,
            data: &[WmState::Normal as u32, 0],
        }) {
            debug!("ChangeProperty failed ({window:?}: {e:?})");
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
        let reply = unwrap_or_skip_bad_window_ret!(self.connection.wait_for_reply(cookie));

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

            unwrap_or_skip_bad_window_ret!(self.connection.send_and_check_request(&x::SendEvent {
                destination: x::SendEventDest::Window(window),
                propagate: false,
                event_mask: x::EventMask::empty(),
                event,
            }));
        } else {
            unwrap_or_skip_bad_window_ret!(self.connection.send_and_check_request(&x::KillClient {
                resource: window.resource_id()
            }))
        }
    }

    fn unmap_window(&mut self, window: x::Window) {
        unwrap_or_skip_bad_window_ret!(self
            .connection
            .send_and_check_request(&x::UnmapWindow { window }));
    }

    fn raise_to_top(&mut self, window: x::Window) {
        unwrap_or_skip_bad_window_ret!(self.connection.send_and_check_request(
            &x::ConfigureWindow {
                window,
                value_list: &[x::ConfigWindow::StackMode(x::StackMode::Above)],
            }
        ));
    }
}

fn get_atom_name(connection: &xcb::Connection, atom: x::Atom) -> String {
    match connection.wait_for_reply(connection.send_request(&x::GetAtomName { atom })) {
        Ok(reply) => reply.name().to_string(),
        Err(err) => {
            warn!("<error getting atom name: {err:?}> {atom:?}");
            format!("ATOM_{}", atom.resource_id())
        }
    }
}
