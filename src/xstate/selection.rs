use super::{get_atom_name, XState};
use crate::server::ForeignSelection;
use crate::{MimeTypeData, RealServerState};
use log::{debug, trace, warn};
use std::rc::Rc;
use xcb::x;

#[derive(Debug)]
enum TargetValue {
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
    Foreign,
}

#[derive(Debug)]
struct SelectionTargetId {
    name: String,
    atom: x::Atom,
}

pub struct SelectionTarget {
    id: SelectionTargetId,
    value: TargetValue,
}

impl MimeTypeData for SelectionTarget {
    fn name(&self) -> &str {
        &self.id.name
    }

    fn data(&self) -> &[u8] {
        match &self.value {
            TargetValue::U8(v) => v,
            other => {
                warn!(
                    "Unexpectedly requesting data from mime type with data type {} - nothing will be copied",
                    std::any::type_name_of_val(other)
                );
                &[]
            }
        }
    }
}

enum MimeTypes {
    Temporary {
        /// Temporary mime data, being built
        data: Vec<SelectionTarget>,
        /// Mime types we still need to receive feedback on
        /// 2nd field is the destination property
        to_grab: Vec<(SelectionTargetId, x::Atom)>,
    },
    /// Done grabbing mime data
    Complete(Rc<Vec<SelectionTarget>>),
}

impl Default for MimeTypes {
    fn default() -> Self {
        Self::Complete(Default::default())
    }
}

#[derive(Default)]
pub(crate) struct SelectionData {
    clear_time: Option<u32>,
    mime_types: MimeTypes,
    foreign_data: Option<ForeignSelection>,
}

impl XState {
    pub(crate) fn set_clipboard_owner(&mut self, time: u32) {
        self.connection
            .send_and_check_request(&x::SetSelectionOwner {
                owner: self.wm_window,
                selection: self.atoms.clipboard,
                time,
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
        let types = selection
            .mime_types
            .iter()
            .map(|mime| {
                let atom = self
                    .connection
                    .wait_for_reply(self.connection.send_request(&x::InternAtom {
                        only_if_exists: false,
                        name: mime.as_bytes(),
                    }))
                    .unwrap();

                SelectionTarget {
                    id: SelectionTargetId {
                        name: mime.clone(),
                        atom: atom.atom(),
                    },
                    value: TargetValue::Foreign,
                }
            })
            .collect();

        self.selection_data.mime_types = MimeTypes::Complete(Rc::new(types));
        self.selection_data.foreign_data = Some(selection);
        trace!("Clipboard set from Wayland");
    }

    pub(crate) fn handle_selection_event(
        &mut self,
        event: &xcb::Event,
        server_state: &mut RealServerState,
    ) -> bool {
        match event {
            // Someone else is the clipboard owner - get the data from them and then reestablish
            // ourselves as the owner
            xcb::Event::X(x::Event::SelectionClear(e)) => {
                if e.selection() != self.atoms.clipboard {
                    warn!(
                        "Got SelectionClear for unexpected atom {}, ignoring",
                        get_atom_name(&self.connection, e.selection())
                    );
                    return true;
                }

                // get the mime types
                self.connection
                    .send_and_check_request(&x::ConvertSelection {
                        requestor: self.wm_window,
                        selection: self.atoms.clipboard,
                        target: self.atoms.targets,
                        property: self.atoms.selection_reply,
                        time: e.time(),
                    })
                    .unwrap();

                self.selection_data.clear_time = Some(e.time());
            }
            xcb::Event::X(x::Event::SelectionNotify(e)) => {
                if e.property() == x::ATOM_NONE {
                    warn!("selection notify fail?");
                    return true;
                }

                match e.target() {
                    x if x == self.atoms.targets => self.handle_target_list(e.property()),
                    atom => self.handle_clipboard_data(atom),
                }

                if let MimeTypes::Temporary { data, to_grab } = &mut self.selection_data.mime_types
                {
                    if to_grab.is_empty() {
                        let data = Rc::new(std::mem::take(data));
                        self.selection_data.mime_types = MimeTypes::Complete(data.clone());
                        self.set_clipboard_owner(self.selection_data.clear_time.unwrap());
                        server_state.set_copy_paste_source(data);
                        trace!("Clipboard set from X11");
                    }
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

                let MimeTypes::Complete(mime_data) = &self.selection_data.mime_types else {
                    warn!("Got selection request, but mime data is incomplete");
                    refuse();
                    return true;
                };

                match e.target() {
                    x if x == self.atoms.targets => {
                        let atoms: Box<[x::Atom]> = mime_data.iter().map(|t| t.id.atom).collect();

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
                        let Some(target) = mime_data.iter().find(|t| t.id.atom == other) else {
                            if log::log_enabled!(log::Level::Debug) {
                                let name = get_atom_name(&self.connection, other);
                                debug!("refusing selection request because given atom could not be found ({})", name);
                            }
                            refuse();
                            return true;
                        };

                        macro_rules! set_property {
                            ($data:expr) => {
                                match self.connection.send_and_check_request(&x::ChangeProperty {
                                    mode: x::PropMode::Replace,
                                    window: e.requestor(),
                                    property: e.property(),
                                    r#type: target.id.atom,
                                    data: $data,
                                }) {
                                    Ok(_) => success(),
                                    Err(e) => {
                                        warn!("Failed setting selection property: {e:?}");
                                        refuse();
                                    }
                                }
                            };
                        }

                        match &target.value {
                            TargetValue::U8(v) => set_property!(v),
                            TargetValue::U16(v) => set_property!(v),
                            TargetValue::U32(v) => set_property!(v),
                            TargetValue::Foreign => {
                                let data = self
                                    .selection_data
                                    .foreign_data
                                    .as_ref()
                                    .unwrap()
                                    .receive(target.id.name.clone(), server_state);
                                set_property!(&data);
                            }
                        }
                    }
                }
            }
            _ => return false,
        }

        true
    }

    fn handle_target_list(&mut self, dest_property: x::Atom) {
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
        if log::log_enabled!(log::Level::Debug) {
            let targets_str: Vec<String> = targets
                .iter()
                .map(|t| get_atom_name(&self.connection, *t))
                .collect();
            debug!("got targets: {targets_str:?}");
        }

        let to_grab = targets
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
            .enumerate()
            .map(|(idx, target_atom)| {
                let dest_name = [b"dest", idx.to_string().as_bytes()].concat();
                let reply = self
                    .connection
                    .wait_for_reply(self.connection.send_request(&x::InternAtom {
                        name: &dest_name,
                        only_if_exists: false,
                    }))
                    .unwrap();
                let dest = reply.atom();

                self.connection
                    .send_and_check_request(&x::ConvertSelection {
                        requestor: self.wm_window,
                        selection: self.atoms.clipboard,
                        target: target_atom,
                        property: dest,
                        time: self.selection_data.clear_time.as_ref().copied().unwrap(),
                    })
                    .unwrap();

                let target_name = get_atom_name(&self.connection, target_atom);
                (
                    SelectionTargetId {
                        name: target_name,
                        atom: target_atom,
                    },
                    dest,
                )
            })
            .collect();

        self.selection_data.mime_types = MimeTypes::Temporary {
            to_grab,
            data: Vec::new(),
        };
    }

    fn handle_clipboard_data(&mut self, atom: x::Atom) {
        let MimeTypes::Temporary { data, to_grab } = &mut self.selection_data.mime_types else {
            warn!("Got selection notify, but not awaiting selection data...");
            return;
        };

        let Some(idx) = to_grab.iter().position(|(id, _)| id.atom == atom) else {
            warn!(
                "unexpected SelectionNotify type: {}",
                get_atom_name(&self.connection, atom)
            );
            return;
        };

        let (id, dest) = to_grab.swap_remove(idx);

        let value = match atom {
            x if x == self.atoms.timestamp => TargetValue::U32(vec![self
                .selection_data
                .clear_time
                .as_ref()
                .copied()
                .unwrap()]),
            _ => {
                let reply = self
                    .connection
                    .wait_for_reply(self.connection.send_request(&x::GetProperty {
                        delete: true,
                        window: self.wm_window,
                        property: dest,
                        r#type: x::ATOM_ANY,
                        long_offset: 0,
                        long_length: u32::MAX,
                    }))
                    .unwrap();

                match reply.format() {
                    8 => TargetValue::U8(reply.value().to_vec()),
                    16 => TargetValue::U16(reply.value().to_vec()),
                    32 => TargetValue::U32(reply.value().to_vec()),
                    other => {
                        if log::log_enabled!(log::Level::Debug) {
                            let target_name = &id.name;
                            let ty = if reply.r#type() == x::ATOM_NONE {
                                "None".to_string()
                            } else {
                                get_atom_name(&self.connection, reply.r#type())
                            };
                            let dest = get_atom_name(&self.connection, dest);
                            let value = reply.value::<u8>().to_vec();
                            debug!("unexpected format: {other} (atom: {target_name}, type: {ty:?}, property: {dest}, value: {value:?})");
                        }
                        return;
                    }
                }
            }
        };

        trace!("Selection data: {id:?} {value:?}");
        data.push(SelectionTarget { id, value });
    }
}
