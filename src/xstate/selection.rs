use super::{XState, get_atom_name};
use crate::server::selection::{Clipboard, ForeignSelection, Primary, SelectionType};
use crate::{RealServerState, X11Selection};
use log::{debug, error, warn};
use rustix::event::{PollFd, PollFlags, poll};
use smithay_client_toolkit::data_device_manager::WritePipe;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{Error, ErrorKind, Result, Write};
use std::rc::Rc;
use xcb::x;

#[derive(Debug)]
struct SelectionTargetId {
    name: String,
    target: x::Atom,
    property: x::Atom,
    source: Option<String>,
}

struct PendingSelectionData {
    target: x::Atom,
    property: x::Atom,
    pipe: WritePipe,
    incr: bool,
    active: bool,
}

pub struct Selection {
    mimes: Vec<SelectionTargetId>,
    connection: Rc<xcb::Connection>,
    window: x::Window,
    pending: RefCell<VecDeque<PendingSelectionData>>,
    selection: x::Atom,
    selection_time: u32,
    incr: x::Atom,
}

impl X11Selection for Selection {
    fn mime_types(&self) -> Vec<&str> {
        self.mimes
            .iter()
            .map(|target| target.name.as_str())
            .collect()
    }

    fn write_to(&self, mime: &str, pipe: WritePipe) {
        if let Some(target) = self.mimes.iter().find(|target| target.name == mime) {
            // A lot of X applications do not anticipate the possibility of multiple requests for
            // its owned selection to need the INCR transfer mechanism and will stop sending the
            // necessary `PropertyNotify` events, hanging Wayland transfer receivers.
            // To remedy this, every target requested by Wayland is put into a FIFO queue and the
            // `ConvertSelection` starting the next request is not sent until `next_conversion`
            // closes the `WritePipe`, marking termination of that request.
            self.pending.borrow_mut().push_back(PendingSelectionData {
                target: target.target,
                property: target.property,
                pipe,
                incr: false,
                active: false,
            });
            if self.pending.borrow().len() == 1 {
                self.next_conversion();
            }
        } else {
            warn!("Could not find mime type {mime}");
        }
    }
}

impl Selection {
    /// Finish handling the current pending selection (if any) and queue the next one (if any).
    ///
    /// Regardless of whether the selection conversion succeeds or fails, this function must be
    /// called in order to drop the `WritePipe` to signal to the Wayland program no more data is
    /// coming and to start processing the next `PendingSelectionData`.
    ///
    /// # Panics:
    /// This function will panic if the `pending` `RefCell` is actively being borrowed.
    fn next_conversion(&self) {
        let mut pending = self.pending.borrow_mut();
        if pending.front().is_some_and(|p| p.active) {
            pending.pop_front();
        }

        while let Some(psd) = pending.front_mut() {
            if let Err(e) = self
                .connection
                .send_and_check_request(&x::ConvertSelection {
                    requestor: self.window,
                    selection: self.selection,
                    target: psd.target,
                    property: psd.property,
                    time: self.selection_time,
                })
            {
                error!(
                    "Failed to request selection data (target: {}, error: {e})",
                    get_atom_name(&self.connection, psd.target),
                );
                pending.pop_front();
            } else {
                psd.active = true;
                break;
            }
        }
    }

    fn handle_notify(&self, target: x::Atom) {
        let mut pending = self.pending.borrow_mut();
        let Some(psd) = pending.front_mut().filter(|t| t.target == target) else {
            warn!(
                "Got selection notify for unexpected target {}",
                get_atom_name(&self.connection, target),
            );
            drop(pending);
            self.next_conversion();
            return;
        };

        let request = self.connection.send_request(&x::GetProperty {
            delete: true,
            window: self.window,
            property: psd.property,
            r#type: x::ATOM_ANY,
            long_offset: 0,
            long_length: u32::MAX,
        });
        let reply = match self.connection.wait_for_reply(request) {
            Ok(reply) => reply,
            Err(e) => {
                warn!(
                    "Couldn't get mime type for {}: {e:?}",
                    get_atom_name(&self.connection, psd.target)
                );
                drop(pending);
                self.next_conversion();
                return;
            }
        };

        debug!(
            "got type {} for mime type {}",
            get_atom_name(&self.connection, reply.r#type()),
            get_atom_name(&self.connection, psd.target),
        );

        if reply.r#type() == self.incr {
            debug!(
                "beginning incr for {}",
                get_atom_name(&self.connection, psd.property)
            );
            psd.incr = true;
            return;
        }

        let data = match reply.format() {
            8 => reply.value::<u8>(),
            32 => unsafe { reply.value::<u32>().align_to().1 },
            other => {
                warn!("Unexpected format {other} in selection reply");
                drop(pending);
                self.next_conversion();
                return;
            }
        };

        // Since the WritePipe given to us can have the O_NONBLOCK flag, we must respect that and
        // use `select` to monitor when the pipe is available to do more I/O.
        fn write_all(pipe: &mut WritePipe, mut buf: &[u8]) -> Result<()> {
            while !buf.is_empty() {
                match pipe.write(buf) {
                    Ok(0) => return Err(Error::from(ErrorKind::WriteZero)),
                    Ok(n) => buf = &buf[n..],
                    Err(ref e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                        let mut pollfds = [PollFd::new(pipe, PollFlags::OUT)];
                        poll(&mut pollfds, None)?;
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }

        if !psd.incr || !data.is_empty() {
            if let Err(e) = write_all(&mut psd.pipe, data) {
                warn!("Failed to write selection data: {e:?}");
            } else if psd.incr {
                debug!(
                    "received some incr data for {}",
                    get_atom_name(&self.connection, psd.target)
                );
                return;
            }
        } else if psd.incr {
            // data is empty
            debug!(
                "completed incr for mime {}",
                get_atom_name(&self.connection, target)
            );
        }

        drop(pending);
        self.next_conversion();
    }

    fn check_for_incr(&self, event: &x::PropertyNotifyEvent) -> bool {
        debug_assert_eq!(event.state(), x::Property::NewValue);
        if event.window() != self.window {
            return false;
        }

        let target = self.pending.borrow().front().and_then(|pending| {
            (pending.property == event.atom() && pending.incr).then_some(pending.target)
        });
        if let Some(target) = target {
            self.handle_notify(target);
            true
        } else {
            false
        }
    }
}

pub struct WaylandIncrInfo {
    data: Vec<u8>,
    start: usize,
    property: x::Atom,
    target_window: x::Window,
    target_type: x::Atom,
    max_req_bytes: usize,
}

pub struct WaylandSelection<T: SelectionType> {
    mimes: Vec<SelectionTargetId>,
    inner: ForeignSelection<T>,
    incr_data: Option<WaylandIncrInfo>,
}

impl<T: SelectionType> WaylandSelection<T> {
    fn check_for_incr(
        &mut self,
        event: &x::PropertyNotifyEvent,
        connection: &xcb::Connection,
    ) -> bool {
        let Some(incr_data) = self.incr_data.as_mut() else {
            return false;
        };
        if incr_data.property != event.atom() {
            return false;
        }

        let incr_end = std::cmp::min(
            incr_data.max_req_bytes + incr_data.start,
            incr_data.data.len(),
        );

        if let Err(e) = connection.send_and_check_request(&x::ChangeProperty {
            mode: x::PropMode::Append,
            window: incr_data.target_window,
            property: incr_data.property,
            r#type: incr_data.target_type,
            data: &incr_data.data[incr_data.start..incr_end],
        }) {
            warn!("failed to write selection data: {e:?}");
            self.incr_data = None;
            return true;
        }

        if incr_data.start == incr_end {
            debug!(
                "completed incr for mime {}",
                get_atom_name(connection, incr_data.target_type)
            );
            self.incr_data = None;
        } else {
            debug!(
                "received some incr data for {}",
                get_atom_name(connection, incr_data.target_type)
            );
            incr_data.start = incr_end;
        }
        true
    }
}

enum CurrentSelection<T: SelectionType> {
    X11(Rc<Selection>),
    Wayland(WaylandSelection<T>),
}

struct SelectionData<T: SelectionType> {
    last_selection_timestamp: u32,
    atom: x::Atom,
    targets_atom: x::Atom,
    current_selection: Option<CurrentSelection<T>>,
}

// This is a trait so that we can use &dyn
trait SelectionDataImpl {
    fn set_owner(&self, connection: &xcb::Connection, wm_window: x::Window) -> bool;
    fn handle_new_owner(
        &mut self,
        connection: &xcb::Connection,
        wm_window: x::Window,
        atoms: &super::Atoms,
        owner: x::Window,
        timestamp: u32,
    );
    fn handle_target_list(
        &mut self,
        connection: &Rc<xcb::Connection>,
        wm_window: x::Window,
        atoms: &super::Atoms,
        target_window: x::Window,
        dest_property: x::Atom,
        server_state: &mut RealServerState,
    );
    fn x11_selection(&self) -> Option<&Selection>;
    fn handle_selection_request(
        &mut self,
        connection: &xcb::Connection,
        atoms: &super::Atoms,
        request: &x::SelectionRequestEvent,
        max_req_bytes: usize,
        server_state: &mut RealServerState,
    ) -> bool;
    fn atom(&self) -> x::Atom;
    fn selection_clear(&mut self);
}

impl<T: SelectionType> SelectionData<T> {
    fn new(atom: x::Atom, targets_atom: x::Atom) -> Self {
        Self {
            last_selection_timestamp: x::CURRENT_TIME,
            atom,
            targets_atom,
            current_selection: None,
        }
    }
    fn wayland_selection_mut(&mut self) -> Option<&mut WaylandSelection<T>> {
        match &mut self.current_selection {
            Some(CurrentSelection::Wayland(sel)) => Some(sel),
            _ => None,
        }
    }
}

impl<T: SelectionType> SelectionDataImpl for SelectionData<T> {
    fn atom(&self) -> x::Atom {
        self.atom
    }
    fn set_owner(&self, connection: &xcb::Connection, wm_window: x::Window) -> bool {
        if let Err(e) = connection.send_and_check_request(&x::SetSelectionOwner {
            owner: wm_window,
            selection: self.atom,
            time: self.last_selection_timestamp,
        }) {
            warn!(
                "Could not become owner of {}: {e:?}",
                get_atom_name(connection, self.atom)
            );
            return false;
        };

        match connection.wait_for_reply(connection.send_request(&x::GetSelectionOwner {
            selection: self.atom,
        })) {
            Ok(reply) if reply.owner() == wm_window => true,
            Ok(reply) => {
                warn!(
                    "Could not become owner of {} (owned by {:?})",
                    get_atom_name(connection, self.atom),
                    reply.owner()
                );
                false
            }
            Err(e) => {
                warn!(
                    "Could not validate ownership of {}: {e:?}",
                    get_atom_name(connection, self.atom)
                );
                false
            }
        }
    }

    fn selection_clear(&mut self) {
        self.current_selection = None;
    }

    fn handle_new_owner(
        &mut self,
        connection: &xcb::Connection,
        wm_window: x::Window,
        atoms: &super::Atoms,
        owner: x::Window,
        timestamp: u32,
    ) {
        // Grab targets
        match connection.send_and_check_request(&x::ConvertSelection {
            requestor: wm_window,
            selection: self.atom,
            target: atoms.targets,
            property: self.targets_atom,
            time: timestamp,
        }) {
            Ok(_) => {
                debug!(
                    "new {} owner: {owner:?}",
                    get_atom_name(connection, self.atom)
                );
                self.last_selection_timestamp = timestamp;
            }
            Err(e) => warn!(
                "could not set new {} owner: {e:?}",
                get_atom_name(connection, self.atom)
            ),
        }
    }

    fn handle_target_list(
        &mut self,
        connection: &Rc<xcb::Connection>,
        wm_window: x::Window,
        atoms: &super::Atoms,
        target_window: x::Window,
        dest_property: x::Atom,
        server_state: &mut RealServerState,
    ) {
        let reply = match connection.wait_for_reply(connection.send_request(&x::GetProperty {
            delete: true,
            window: wm_window,
            property: dest_property,
            r#type: x::ATOM_ATOM,
            long_offset: 0,
            long_length: 20,
        })) {
            Ok(reply) => reply,
            Err(e) => {
                warn!("Could not obtain target list: {e:?}");
                return;
            }
        };

        let targets: &[x::Atom] = reply.value();
        if targets.is_empty() {
            warn!("Got empty selection target list, trying again...");
            match connection.wait_for_reply(connection.send_request(&x::GetSelectionOwner {
                selection: self.atom,
            })) {
                Ok(reply) if reply.owner() != wm_window => {
                    self.handle_new_owner(
                        connection,
                        wm_window,
                        atoms,
                        reply.owner(),
                        self.last_selection_timestamp,
                    );
                }
                Ok(_) => {
                    warn!("We are unexpectedly the selection owner? Clipboard may be broken!");
                }
                Err(e) => {
                    error!("Couldn't grab selection owner: {e:?}. Clipboard is stale!");
                }
            }
            return;
        }
        if log::log_enabled!(log::Level::Debug) {
            let targets_str: Vec<String> = targets
                .iter()
                .map(|t| get_atom_name(connection, *t))
                .collect();
            debug!("got targets: {targets_str:?}");
        }

        let selection = get_atom_name(connection, self.atom);
        let mimes = targets
            .iter()
            .copied()
            .filter(|atom| ![atoms.targets, atoms.multiple, atoms.save_targets].contains(atom))
            .map(|target| {
                let name = get_atom_name(connection, target);
                let property = connection
                    .wait_for_reply(connection.send_request(&x::InternAtom {
                        only_if_exists: false,
                        name: &[name.as_bytes(), b"_", selection.as_bytes()].concat(),
                    }))
                    .unwrap()
                    .atom();
                SelectionTargetId {
                    name,
                    target,
                    property,
                    source: None,
                }
            })
            .collect();

        let selection = Rc::new(Selection {
            mimes,
            connection: connection.clone(),
            window: target_window,
            pending: RefCell::default(),
            selection: self.atom,
            selection_time: self.last_selection_timestamp,
            incr: atoms.incr,
        });

        server_state.set_selection_source::<T>(&selection);
        self.current_selection = Some(CurrentSelection::X11(selection));
        debug!("{} set from X11", get_atom_name(connection, self.atom));
    }

    fn x11_selection(&self) -> Option<&Selection> {
        match &self.current_selection {
            Some(CurrentSelection::X11(selection)) => Some(selection),
            _ => None,
        }
    }

    fn handle_selection_request(
        &mut self,
        connection: &xcb::Connection,
        atoms: &super::Atoms,
        request: &x::SelectionRequestEvent,
        max_req_bytes: usize,
        server_state: &mut RealServerState,
    ) -> bool {
        let Some(CurrentSelection::Wayland(WaylandSelection {
            mimes,
            inner,
            incr_data,
        })) = &mut self.current_selection
        else {
            warn!("Got selection request, but we don't seem to be the selection owner");
            return false;
        };

        let req_target = request.target();
        if req_target == atoms.targets {
            let atoms: Box<[x::Atom]> = mimes.iter().map(|t| t.target).collect();

            match connection.send_and_check_request(&x::ChangeProperty {
                mode: x::PropMode::Replace,
                window: request.requestor(),
                property: request.property(),
                r#type: x::ATOM_ATOM,
                data: &atoms,
            }) {
                Ok(_) => true,
                Err(e) => {
                    warn!("Failed to set targets for selection request: {e:?}");
                    false
                }
            }
        } else {
            let Some(target) = mimes.iter().find(|t| t.target == req_target) else {
                if log::log_enabled!(log::Level::Debug) {
                    let name = get_atom_name(connection, req_target);
                    debug!(
                        "refusing selection request because given atom could not be found ({name})"
                    );
                }
                return false;
            };

            let mime_name = target
                .source
                .as_ref()
                .cloned()
                .unwrap_or_else(|| target.name.clone());
            let data = inner.receive(mime_name, server_state);
            if data.len() > max_req_bytes {
                if let Err(e) = connection.send_and_check_request(&x::ChangeWindowAttributes {
                    window: request.requestor(),
                    value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
                }) {
                    warn!("Failed to set up property change notifications: {e:?}");
                    return false;
                }
                if let Err(e) = connection.send_and_check_request(&x::ChangeProperty {
                    mode: x::PropMode::Replace,
                    window: request.requestor(),
                    property: request.property(),
                    r#type: atoms.incr,
                    data: &[data.len() as u32],
                }) {
                    warn!("Failed to set incr property for large transfer: {e:?}");
                    return false;
                }
                debug!(
                    "beginning incr for {}",
                    get_atom_name(connection, target.target)
                );
                *incr_data = Some(WaylandIncrInfo {
                    data,
                    start: 0,
                    target_window: request.requestor(),
                    property: request.property(),
                    target_type: target.target,
                    max_req_bytes,
                });
                true
            } else {
                match connection.send_and_check_request(&x::ChangeProperty {
                    mode: x::PropMode::Replace,
                    window: request.requestor(),
                    property: request.property(),
                    r#type: target.target,
                    data: &data,
                }) {
                    Ok(_) => true,
                    Err(e) => {
                        warn!("Failed setting selection property: {e:?}");
                        false
                    }
                }
            }
        }
    }
}

pub(super) struct SelectionState {
    clipboard: SelectionData<Clipboard>,
    primary: SelectionData<Primary>,
    target_window: x::Window,
}

impl SelectionState {
    pub fn new(connection: &xcb::Connection, root: x::Window, atoms: &super::Atoms) -> Self {
        let target_window = connection.generate_id();
        connection
            .send_and_check_request(&x::CreateWindow {
                wid: target_window,
                width: 1,
                height: 1,
                depth: 0,
                parent: root,
                x: 0,
                y: 0,
                border_width: 0,
                class: x::WindowClass::InputOnly,
                visual: x::COPY_FROM_PARENT,
                // Watch for INCR property changes.
                value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
            })
            .expect("Couldn't create window for selections");
        Self {
            target_window,
            clipboard: SelectionData::new(atoms.clipboard, atoms.clipboard_targets),
            primary: SelectionData::new(atoms.primary, atoms.primary_targets),
        }
    }
}

impl XState {
    fn intern_target_property_atoms(&self, mime: &[u8], suffix: &[u8]) -> (x::Atom, x::Atom) {
        // A concatenation of the target and the selection type are used to create a distinct
        // property to write to
        let target = self
            .connection
            .wait_for_reply(self.connection.send_request(&x::InternAtom {
                only_if_exists: false,
                name: mime,
            }))
            .unwrap()
            .atom();
        let property = self
            .connection
            .wait_for_reply(self.connection.send_request(&x::InternAtom {
                only_if_exists: false,
                name: &[mime, suffix].concat(),
            }))
            .unwrap()
            .atom();
        (target, property)
    }

    pub(crate) fn set_clipboard(&mut self, selection: ForeignSelection<Clipboard>) {
        let mut utf8_xwl = false;
        let mut utf8_wl = false;
        let mut mimes: Vec<SelectionTargetId> = selection
            .mime_types
            .iter()
            .map(|mime| {
                match mime.as_str() {
                    "UTF8_STRING" => utf8_xwl = true,
                    "text/plain;charset=utf-8" => utf8_wl = true,
                    _ => {}
                }

                let (target, property) =
                    self.intern_target_property_atoms(mime.as_bytes(), b"_CLIPBOARD");
                SelectionTargetId {
                    name: mime.clone(),
                    target,
                    property,
                    source: None,
                }
            })
            .collect();

        if utf8_wl && !utf8_xwl {
            let name = "UTF8_STRING".to_string();
            let (target, property) =
                self.intern_target_property_atoms(name.as_bytes(), b"_CLIPBOARD");
            mimes.push(SelectionTargetId {
                name,
                target,
                property,
                source: Some("text/plain;charset=utf-8".to_string()),
            });
        }

        if self
            .selection_state
            .clipboard
            .set_owner(&self.connection, self.wm_window)
        {
            self.selection_state.clipboard.current_selection =
                Some(CurrentSelection::Wayland(WaylandSelection {
                    mimes,
                    inner: selection,
                    incr_data: None,
                }));

            debug!("Clipboard set from Wayland");
        }
    }

    pub(crate) fn set_primary_selection(&mut self, selection: ForeignSelection<Primary>) {
        let mut utf8_xwl = false;
        let mut utf8_wl = false;
        let mut mimes: Vec<SelectionTargetId> = selection
            .mime_types
            .iter()
            .map(|mime| {
                match mime.as_str() {
                    "UTF8_STRING" => utf8_xwl = true,
                    "text/plain;charset=utf-8" => utf8_wl = true,
                    _ => {}
                }

                let (target, property) =
                    self.intern_target_property_atoms(mime.as_bytes(), b"_PRIMARY");
                SelectionTargetId {
                    name: mime.clone(),
                    target,
                    property,
                    source: None,
                }
            })
            .collect();

        if utf8_wl && !utf8_xwl {
            let name = "UTF8_STRING".to_string();
            let (target, property) =
                self.intern_target_property_atoms(name.as_bytes(), b"_PRIMARY");
            mimes.push(SelectionTargetId {
                name,
                target,
                property,
                source: Some("text/plain;charset=utf-8".to_string()),
            });
        }

        if self
            .selection_state
            .primary
            .set_owner(&self.connection, self.wm_window)
        {
            self.selection_state.primary.current_selection =
                Some(CurrentSelection::Wayland(WaylandSelection {
                    mimes,
                    inner: selection,
                    incr_data: None,
                }));
            debug!("Primary set from Wayland");
        }
    }

    pub(super) fn handle_selection_event(
        &mut self,
        event: &xcb::Event,
        server_state: &mut RealServerState,
    ) -> bool {
        macro_rules! get_selection_data {
            ($selection:expr) => {
                match $selection {
                    x if x == self.atoms.clipboard => {
                        &mut self.selection_state.clipboard as &mut dyn SelectionDataImpl
                    }
                    x if x == self.atoms.primary => &mut self.selection_state.primary as _,
                    _ => return true,
                }
            };
        }
        match event {
            xcb::Event::X(x::Event::SelectionClear(e)) => {
                let data = get_selection_data!(e.selection());
                data.selection_clear();
            }
            xcb::Event::X(x::Event::SelectionNotify(e)) => {
                if e.property() == x::ATOM_NONE {
                    // Since the requested conversion could not be made, the request is invalid and
                    // should be removed, dropping the WritePipe to signal no data will be sent
                    if e.requestor() == self.selection_state.target_window {
                        let data = get_selection_data!(e.selection());
                        if let Some(selection) = data.x11_selection() {
                            selection.next_conversion();
                        }
                    }
                    warn!(
                        "selection notify fail? {}",
                        get_atom_name(&self.connection, e.selection())
                    );
                    return true;
                }

                let data = get_selection_data!(e.selection());
                debug!(
                    "selection notify requestor: {:?} target: {} selection: {}",
                    e.requestor(),
                    get_atom_name(&self.connection, e.target()),
                    get_atom_name(&self.connection, e.selection()),
                );

                if e.requestor() == self.wm_window {
                    match e.target() {
                        x if x == self.atoms.targets => data.handle_target_list(
                            &self.connection,
                            self.wm_window,
                            &self.atoms,
                            self.selection_state.target_window,
                            e.property(),
                            server_state,
                        ),
                        other => warn!(
                            "got unexpected selection notify for target {}",
                            get_atom_name(&self.connection, other)
                        ),
                    }
                } else if e.requestor() == self.selection_state.target_window {
                    if let Some(selection) = data.x11_selection() {
                        selection.handle_notify(e.target());
                    }
                } else {
                    warn!(
                        "Got selection notify from unexpected requestor: {:?}",
                        e.requestor()
                    );
                }
            }
            xcb::Event::X(x::Event::SelectionRequest(e)) => {
                let data = get_selection_data!(e.selection());
                let send_notify = |property| {
                    if let Err(e) = self.connection.send_and_check_request(&x::SendEvent {
                        propagate: false,
                        destination: x::SendEventDest::Window(e.requestor()),
                        event_mask: x::EventMask::empty(),
                        event: &x::SelectionNotifyEvent::new(
                            e.time(),
                            e.requestor(),
                            e.selection(),
                            e.target(),
                            property,
                        ),
                    }) {
                        warn!("Failed to send selection request notify: {e:?}");
                    };
                };
                let refuse = || send_notify(x::ATOM_NONE);
                let success = || send_notify(e.property());

                if log::log_enabled!(log::Level::Debug) {
                    let target = get_atom_name(&self.connection, e.target());
                    let selection = get_atom_name(&self.connection, data.atom());
                    debug!("Got selection request for target {target} (selection: {selection})");
                }

                if e.property() == x::ATOM_NONE {
                    debug!("refusing - property is set to none");
                    refuse();
                    return true;
                }

                if data.handle_selection_request(
                    &self.connection,
                    &self.atoms,
                    e,
                    self.max_req_bytes,
                    server_state,
                ) {
                    success()
                } else {
                    refuse()
                }
            }

            xcb::Event::XFixes(xcb::xfixes::Event::SelectionNotify(e)) => match e.selection() {
                x if x == self.atoms.clipboard || x == self.atoms.primary => match e.subtype() {
                    xcb::xfixes::SelectionEvent::SetSelectionOwner => {
                        if e.owner() == self.wm_window {
                            return true;
                        }

                        let data = get_selection_data!(x);

                        data.handle_new_owner(
                            &self.connection,
                            self.wm_window,
                            &self.atoms,
                            e.owner(),
                            e.timestamp(),
                        );
                    }
                    xcb::xfixes::SelectionEvent::SelectionClientClose
                    | xcb::xfixes::SelectionEvent::SelectionWindowDestroy => {
                        debug!("Selection owner destroyed, selection will be unset");
                        self.selection_state.clipboard.current_selection = None;
                    }
                },
                x if x == self.atoms.xsettings => match e.subtype() {
                    xcb::xfixes::SelectionEvent::SelectionClientClose
                    | xcb::xfixes::SelectionEvent::SelectionWindowDestroy => {
                        debug!("Xsettings owner disappeared, reacquiring");
                        self.set_xsettings_owner();
                    }
                    _ => {}
                },
                _ => {}
            },
            _ => return false,
        }

        true
    }

    /// Check if a PropertyNotifyEvent refers to a supported selection property, then make progress
    /// on an incremental data transfer if that property is in that process.
    /// Returns `true` if an attempt at progressing the data transfer was made, whether or not it
    /// succeeded, and `false` otherwise (e.g. the event does not target a selection property or
    /// that property is not in the process of an incremental data transfer)
    pub(super) fn handle_selection_property_change(
        &mut self,
        event: &x::PropertyNotifyEvent,
    ) -> bool {
        fn inner<T: SelectionType>(
            connection: &xcb::Connection,
            event: &x::PropertyNotifyEvent,
            data: &mut SelectionData<T>,
        ) -> bool {
            match event.state() {
                x::Property::NewValue => {
                    if let Some(selection) = &data.x11_selection() {
                        return selection.check_for_incr(event);
                    }
                }
                x::Property::Delete => {
                    if let Some(selection) = data.wayland_selection_mut() {
                        return selection.check_for_incr(event, connection);
                    }
                }
            }
            false
        }
        inner(&self.connection, event, &mut self.selection_state.primary)
            || inner(&self.connection, event, &mut self.selection_state.clipboard)
    }
}
