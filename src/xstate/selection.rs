use super::{get_atom_name, XState};
use crate::server::ForeignSelection;
use crate::{RealServerState, X11Selection};
use log::{debug, error, warn};
use smithay_client_toolkit::data_device_manager::WritePipe;
use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;
use xcb::x;

#[derive(Debug)]
struct SelectionTargetId {
    name: String,
    atom: x::Atom,
    source: Option<String>,
}

struct PendingSelectionData {
    target: x::Atom,
    pipe: WritePipe,
    incr: bool,
}

pub struct Selection {
    mimes: Vec<SelectionTargetId>,
    connection: Rc<xcb::Connection>,
    window: x::Window,
    pending: RefCell<Vec<PendingSelectionData>>,
    clipboard: x::Atom,
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
            // We use the target as the property to write to
            if let Err(e) = self
                .connection
                .send_and_check_request(&x::ConvertSelection {
                    requestor: self.window,
                    selection: self.clipboard,
                    target: target.atom,
                    property: target.atom,
                    time: self.selection_time,
                })
            {
                error!("Failed to request clipboard data (mime type: {mime}, error: {e})");
                return;
            }

            self.pending.borrow_mut().push(PendingSelectionData {
                target: target.atom,
                pipe,
                incr: false,
            })
        } else {
            warn!("Could not find mime type {mime}");
        }
    }
}

impl Selection {
    fn handle_notify(&self, target: x::Atom) {
        let mut pending = self.pending.borrow_mut();
        let Some(idx) = pending.iter().position(|t| t.target == target) else {
            warn!(
                "Got selection notify for unknown target {}",
                get_atom_name(&self.connection, target),
            );
            return;
        };

        let PendingSelectionData {
            mut pipe,
            incr,
            target,
        } = pending.swap_remove(idx);
        let reply = match get_property_any(&self.connection, self.window, target) {
            Ok(reply) => reply,
            Err(e) => {
                warn!(
                    "Couldn't get mime type for {}: {e:?}",
                    get_atom_name(&self.connection, target)
                );
                return;
            }
        };

        debug!(
            "got type {} for mime type {}",
            get_atom_name(&self.connection, reply.r#type()),
            get_atom_name(&self.connection, target)
        );

        if reply.r#type() == self.incr {
            debug!(
                "beginning incr for {}",
                get_atom_name(&self.connection, target)
            );
            pending.push(PendingSelectionData {
                target,
                pipe,
                incr: true,
            });
            return;
        }

        let data = match reply.format() {
            8 => reply.value::<u8>(),
            32 => unsafe { reply.value::<u32>().align_to().1 },
            other => {
                warn!("Unexpected format {other} in selection reply");
                return;
            }
        };

        if !incr || !data.is_empty() {
            if let Err(e) = pipe.write_all(data) {
                warn!("Failed to write selection data: {e:?}");
            } else if incr {
                debug!(
                    "recieved some incr data for {}",
                    get_atom_name(&self.connection, target)
                );
                pending.push(PendingSelectionData {
                    target,
                    pipe,
                    incr: true,
                })
            }
        } else if incr {
            // data is empty
            debug!(
                "completed incr for mime {}",
                get_atom_name(&self.connection, target)
            );
        }
    }

    fn check_for_incr(&self, event: &x::PropertyNotifyEvent) -> bool {
        if event.window() != self.window || event.state() != x::Property::NewValue {
            return false;
        }

        let target = self.pending.borrow().iter().find_map(|pending| {
            (pending.target == event.atom() && pending.incr).then_some(pending.target)
        });
        if let Some(target) = target {
            self.handle_notify(target);
            true
        } else {
            false
        }
    }
}

enum CurrentSelection {
    X11(Rc<Selection>),
    Wayland {
        mimes: Vec<SelectionTargetId>,
        inner: ForeignSelection,
    },
}
pub(crate) struct SelectionData {
    last_selection_timestamp: u32,
    target_window: x::Window,
    current_selection: Option<CurrentSelection>,
}

impl SelectionData {
    pub fn new(connection: &xcb::Connection, root: x::Window) -> Self {
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
            last_selection_timestamp: x::CURRENT_TIME,
            target_window,
            current_selection: None,
        }
    }
}

impl XState {
    fn set_clipboard_owner(&mut self) {
        self.connection
            .send_and_check_request(&x::SetSelectionOwner {
                owner: self.wm_window,
                selection: self.atoms.clipboard,
                time: self.selection_data.last_selection_timestamp,
            })
            .unwrap();

        let reply = self
            .connection
            .wait_for_reply(self.connection.send_request(&x::GetSelectionOwner {
                selection: self.atoms.clipboard,
            }))
            .unwrap();

        if reply.owner() != self.wm_window {
            warn!(
                "Could not get CLIPBOARD selection (owned by {:?})",
                reply.owner()
            );
        }
    }

    pub(crate) fn set_clipboard(&mut self, selection: ForeignSelection) {
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

                let atom = self
                    .connection
                    .wait_for_reply(self.connection.send_request(&x::InternAtom {
                        only_if_exists: false,
                        name: mime.as_bytes(),
                    }))
                    .unwrap();

                SelectionTargetId {
                    name: mime.clone(),
                    atom: atom.atom(),
                    source: None,
                }
            })
            .collect();

        if utf8_wl && !utf8_xwl {
            let name = "UTF8_STRING".to_string();
            let atom = self
                .connection
                .wait_for_reply(self.connection.send_request(&x::InternAtom {
                    only_if_exists: false,
                    name: name.as_bytes(),
                }))
                .unwrap()
                .atom();
            mimes.push(SelectionTargetId {
                name,
                atom,
                source: Some("text/plain;charset=utf-8".to_string()),
            });
        }

        self.selection_data.current_selection = Some(CurrentSelection::Wayland {
            mimes,
            inner: selection,
        });
        self.set_clipboard_owner();
        debug!("Clipboard set from Wayland");
    }

    pub(crate) fn handle_selection_event(
        &mut self,
        event: &xcb::Event,
        server_state: &mut RealServerState,
    ) -> bool {
        match event {
            // Someone else took the clipboard owner
            xcb::Event::X(x::Event::SelectionClear(e)) => {
                self.handle_new_selection_owner(e.owner(), e.time());
            }
            xcb::Event::X(x::Event::SelectionNotify(e)) => {
                if e.property() == x::ATOM_NONE {
                    warn!("selection notify fail?");
                    return true;
                }

                debug!(
                    "selection notify requestor: {:?} target: {}",
                    e.requestor(),
                    get_atom_name(&self.connection, e.target())
                );

                if e.requestor() == self.wm_window {
                    match e.target() {
                        x if x == self.atoms.targets => {
                            self.handle_target_list(e.property(), server_state)
                        }
                        other => warn!(
                            "got unexpected selection notify for target {}",
                            get_atom_name(&self.connection, other)
                        ),
                    }
                } else if e.requestor() == self.selection_data.target_window {
                    if let Some(CurrentSelection::X11(selection)) =
                        &self.selection_data.current_selection
                    {
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
                let send_notify = |property| {
                    self.connection
                        .send_and_check_request(&x::SendEvent {
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
                        })
                        .unwrap();
                };
                let refuse = || send_notify(x::ATOM_NONE);
                let success = || send_notify(e.property());

                if log::log_enabled!(log::Level::Debug) {
                    let target = get_atom_name(&self.connection, e.target());
                    debug!("Got selection request for target {target}");
                }

                if e.property() == x::ATOM_NONE {
                    debug!("refusing - property is set to none");
                    refuse();
                    return true;
                }

                let Some(CurrentSelection::Wayland { mimes, inner }) =
                    &self.selection_data.current_selection
                else {
                    warn!("Got selection request, but we don't seem to be the selection owner");
                    refuse();
                    return true;
                };

                match e.target() {
                    x if x == self.atoms.targets => {
                        let atoms: Box<[x::Atom]> = mimes.iter().map(|t| t.atom).collect();

                        self.connection
                            .send_and_check_request(&x::ChangeProperty {
                                mode: x::PropMode::Replace,
                                window: e.requestor(),
                                property: e.property(),
                                r#type: x::ATOM_ATOM,
                                data: &atoms,
                            })
                            .unwrap();

                        success();
                    }
                    other => {
                        let Some(target) = mimes.iter().find(|t| t.atom == other) else {
                            if log::log_enabled!(log::Level::Debug) {
                                let name = get_atom_name(&self.connection, other);
                                debug!("refusing selection request because given atom could not be found ({})", name);
                            }
                            refuse();
                            return true;
                        };

                        let mime_name = target
                            .source
                            .as_ref()
                            .cloned()
                            .unwrap_or_else(|| target.name.clone());
                        let data = inner.receive(mime_name, server_state);
                        match self.connection.send_and_check_request(&x::ChangeProperty {
                            mode: x::PropMode::Replace,
                            window: e.requestor(),
                            property: e.property(),
                            r#type: target.atom,
                            data: &data,
                        }) {
                            Ok(_) => success(),
                            Err(e) => {
                                warn!("Failed setting selection property: {e:?}");
                                refuse();
                            }
                        }
                    }
                }
            }

            xcb::Event::XFixes(xcb::xfixes::Event::SelectionNotify(e)) => {
                assert_eq!(e.selection(), self.atoms.clipboard);
                match e.subtype() {
                    xcb::xfixes::SelectionEvent::SetSelectionOwner => {
                        if e.owner() == self.wm_window {
                            return true;
                        }

                        self.handle_new_selection_owner(e.owner(), e.selection_timestamp());
                    }
                    xcb::xfixes::SelectionEvent::SelectionClientClose
                    | xcb::xfixes::SelectionEvent::SelectionWindowDestroy => {
                        debug!("Selection owner destroyed, selection will be unset");
                        self.selection_data.current_selection = None;
                    }
                }
            }
            _ => return false,
        }

        true
    }

    fn handle_new_selection_owner(&mut self, owner: x::Window, timestamp: u32) {
        debug!("new selection owner: {:?}", owner);
        self.selection_data.last_selection_timestamp = timestamp;
        // Grab targets
        self.connection
            .send_and_check_request(&x::ConvertSelection {
                requestor: self.wm_window,
                selection: self.atoms.clipboard,
                target: self.atoms.targets,
                property: self.atoms.selection_reply,
                time: timestamp,
            })
            .unwrap();
    }

    fn handle_target_list(&mut self, dest_property: x::Atom, server_state: &mut RealServerState) {
        let reply = self
            .connection
            .wait_for_reply(self.connection.send_request(&x::GetProperty {
                delete: true,
                window: self.wm_window,
                property: dest_property,
                r#type: x::ATOM_ATOM,
                long_offset: 0,
                long_length: 20,
            }))
            .unwrap();

        let targets: &[x::Atom] = reply.value();
        if targets.is_empty() {
            warn!("Got empty selection target list, trying again...");
            match self.connection.wait_for_reply(self.connection.send_request(
                &x::GetSelectionOwner {
                    selection: self.atoms.clipboard,
                },
            )) {
                Ok(reply) => {
                    if reply.owner() == self.wm_window {
                        warn!("We are unexpectedly the selection owner? Clipboard may be broken!");
                    } else {
                        self.handle_new_selection_owner(
                            reply.owner(),
                            self.selection_data.last_selection_timestamp,
                        );
                    }
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
                .map(|t| get_atom_name(&self.connection, *t))
                .collect();
            debug!("got targets: {targets_str:?}");
        }

        let mimes = targets
            .iter()
            .copied()
            .filter(|atom| {
                ![
                    self.atoms.targets,
                    self.atoms.multiple,
                    self.atoms.save_targets,
                ]
                .contains(atom)
            })
            .map(|target_atom| SelectionTargetId {
                name: get_atom_name(&self.connection, target_atom),
                atom: target_atom,
                source: None,
            })
            .collect();

        let selection = Rc::new(Selection {
            mimes,
            connection: self.connection.clone(),
            window: self.selection_data.target_window,
            pending: RefCell::default(),
            clipboard: self.atoms.clipboard,
            selection_time: self.selection_data.last_selection_timestamp,
            incr: self.atoms.incr,
        });

        server_state.set_copy_paste_source(&selection);
        self.selection_data.current_selection = Some(CurrentSelection::X11(selection));
        debug!("Clipboard set from X11");
    }

    pub(super) fn handle_selection_property_change(
        &mut self,
        event: &x::PropertyNotifyEvent,
    ) -> bool {
        if let Some(CurrentSelection::X11(selection)) = &self.selection_data.current_selection {
            return selection.check_for_incr(event);
        }
        false
    }
}

fn get_property_any(
    connection: &xcb::Connection,
    window: x::Window,
    property: x::Atom,
) -> xcb::Result<x::GetPropertyReply> {
    connection.wait_for_reply(connection.send_request(&x::GetProperty {
        delete: true,
        window,
        property,
        r#type: x::ATOM_ANY,
        long_offset: 0,
        long_length: u32::MAX,
    }))
}
