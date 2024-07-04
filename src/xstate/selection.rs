use super::XState;
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

#[derive(Default)]
pub(crate) struct SelectionData {
    clear_time: Option<u32>,
    // Selection ID and destination atom
    tmp_mimes: Vec<(SelectionTargetId, x::Atom)>,
    mime_types: Rc<Vec<SelectionTarget>>,
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

        self.selection_data.mime_types = Rc::new(types);
        self.selection_data.foreign_data = Some(selection);
        trace!("Clipboard set from Wayland");
    }

    pub(crate) fn handle_selection_event(
        &mut self,
        event: &xcb::Event,
        server_state: &mut RealServerState,
    ) -> bool {
        match event {
            xcb::Event::X(x::Event::SelectionClear(e)) => {
                if e.selection() != self.atoms.clipboard {
                    warn!(
                        "Got SelectionClear for unexpected atom {}, ignoring",
                        self.get_atom_name(e.selection())
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
                    x if x == self.atoms.multiple => self.handle_new_clipboard_data(server_state),
                    atom => {
                        warn!(
                            "unexpected SelectionNotify type: {}",
                            self.get_atom_name(atom)
                        )
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
                    let target = self.get_atom_name(e.target());
                    debug!("Got selection request for target {target}");
                }

                if e.property() == x::ATOM_NONE {
                    debug!("refusing - property is set to none");
                    refuse();
                    return true;
                }

                match e.target() {
                    x if x == self.atoms.targets => {
                        let atoms: Box<[x::Atom]> = self
                            .selection_data
                            .mime_types
                            .iter()
                            .map(|t| t.id.atom)
                            .collect();

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
                        let Some(target) = self
                            .selection_data
                            .mime_types
                            .iter()
                            .find(|t| t.id.atom == other)
                        else {
                            if log::log_enabled!(log::Level::Debug) {
                                let name = self.get_atom_name(other);
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
        let target_props: Box<[x::Atom]> = targets
            .iter()
            .copied()
            .filter(|atom| ![self.atoms.targets, self.atoms.multiple].contains(atom))
            .enumerate()
            .flat_map(|(idx, target)| {
                let name = [b"dest", idx.to_string().as_bytes()].concat();
                let reply = self
                    .connection
                    .wait_for_reply(self.connection.send_request(&x::InternAtom {
                        name: &name,
                        only_if_exists: false,
                    }))
                    .unwrap();
                let dest = reply.atom();

                [target, dest]
            })
            .collect();

        // Setup target list
        self.connection
            .send_and_check_request(&x::ChangeProperty {
                mode: x::PropMode::Replace,
                window: self.wm_window,
                property: self.atoms.selection_reply,
                r#type: x::ATOM_ATOM,
                data: &target_props,
            })
            .unwrap();

        // Request data for our targets
        self.connection
            .send_and_check_request(&x::ConvertSelection {
                requestor: self.wm_window,
                selection: self.atoms.clipboard,
                target: self.atoms.multiple,
                property: self.atoms.selection_reply,
                time: self.selection_data.clear_time.as_ref().copied().unwrap(),
            })
            .unwrap();

        let tmp = target_props
            .chunks_exact(2)
            .map(|atoms| {
                let [target, property] = atoms.try_into().unwrap();
                let name = self
                    .connection
                    .wait_for_reply(
                        self.connection
                            .send_request(&x::GetAtomName { atom: target }),
                    )
                    .unwrap();
                let name = name.name().to_string();
                let target = SelectionTargetId { atom: target, name };
                (target, property)
            })
            .collect();

        self.selection_data.tmp_mimes = tmp;
    }

    fn handle_new_clipboard_data(&mut self, server_state: &mut RealServerState) {
        let mut mime_types = Vec::new();
        for (id, dest) in std::mem::take(&mut self.selection_data.tmp_mimes) {
            let value = {
                if id.atom == self.atoms.timestamp {
                    TargetValue::U32(vec![self
                        .selection_data
                        .clear_time
                        .as_ref()
                        .copied()
                        .unwrap()])
                } else {
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
                                let atom = id.atom;
                                let target = self.get_atom_name(atom);
                                let ty = if reply.r#type() == x::ATOM_NONE {
                                    "None".to_string()
                                } else {
                                    self.get_atom_name(reply.r#type())
                                };
                                debug!("unexpected format: {other} (atom: {target}, type: {ty:?}, property: {dest:?})");
                            }
                            continue;
                        }
                    }
                }
            };

            trace!("Selection data: {id:?} {value:?}");
            mime_types.push(SelectionTarget { id, value });
        }

        self.selection_data.mime_types = Rc::new(mime_types);
        self.connection
            .send_and_check_request(&x::DeleteProperty {
                window: self.wm_window,
                property: self.atoms.selection_reply,
            })
            .unwrap();

        self.set_clipboard_owner(self.selection_data.clear_time.unwrap());
        server_state.set_copy_paste_source(Rc::clone(&self.selection_data.mime_types));
        trace!("Clipboard set from X11");
    }
}
