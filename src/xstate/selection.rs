use super::XState;
use crate::server::ForeignSelection;
use crate::{MimeTypeData, RealServerState};
use log::{debug, warn};
use std::rc::Rc;
use xcb::x;

enum TargetValue {
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
    Foreign,
}

pub struct SelectionTarget {
    name: String,
    atom: x::Atom,
    value: Option<TargetValue>,
}

impl MimeTypeData for SelectionTarget {
    fn name(&self) -> &str {
        &self.name
    }

    fn data(&self) -> &[u8] {
        match self.value.as_ref() {
            Some(TargetValue::U8(v)) => v,
            other => {
                if let Some(other) = other {
                    warn!(
                    "Unexpectedly requesting data from mime type with data type {} - nothing will be copied",
                    std::any::type_name_of_val(other)
                );
                }
                &[]
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct SelectionData {
    clear_time: Option<u32>,
    mime_types: Rc<Vec<SelectionTarget>>,
    /// List of property on self.wm_window and corresponding index in mime_types
    mime_destinations: Vec<(x::Atom, usize)>,
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
                    name: mime.clone(),
                    atom: atom.atom(),
                    value: Some(TargetValue::Foreign),
                }
            })
            .collect();

        self.selection_data.mime_types = Rc::new(types);
        self.selection_data.foreign_data = Some(selection);
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
                            .map(|t| t.atom)
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
                            .find(|t| t.atom == other)
                        else {
                            debug!("refusing selection requst because given atom could not be found ({other:?})");
                            refuse();
                            return true;
                        };

                        macro_rules! set_property {
                            ($data:expr) => {
                                self.connection
                                    .send_and_check_request(&x::ChangeProperty {
                                        mode: x::PropMode::Replace,
                                        window: e.requestor(),
                                        property: e.property(),
                                        r#type: target.atom,
                                        data: $data,
                                    })
                                    .unwrap()
                            };
                        }

                        match target.value.as_ref().unwrap() {
                            TargetValue::U8(v) => set_property!(v),
                            TargetValue::U16(v) => set_property!(v),
                            TargetValue::U32(v) => set_property!(v),
                            TargetValue::Foreign => {
                                let data = self
                                    .selection_data
                                    .foreign_data
                                    .as_ref()
                                    .unwrap()
                                    .receive(target.name.clone(), server_state);
                                set_property!(&data);
                            }
                        }

                        success();
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

        self.connection
            .send_and_check_request(&x::ChangeProperty {
                mode: x::PropMode::Replace,
                window: self.wm_window,
                property: self.atoms.selection_reply,
                r#type: x::ATOM_ATOM,
                data: &target_props,
            })
            .unwrap();

        self.connection
            .send_and_check_request(&x::ConvertSelection {
                requestor: self.wm_window,
                selection: self.atoms.clipboard,
                target: self.atoms.multiple,
                property: self.atoms.selection_reply,
                time: self.selection_data.clear_time.as_ref().copied().unwrap(),
            })
            .unwrap();

        let (types, dests) = target_props
            .chunks_exact(2)
            .enumerate()
            .map(|(idx, atoms)| {
                let [target, property] = atoms.try_into().unwrap();
                let name = self
                    .connection
                    .wait_for_reply(
                        self.connection
                            .send_request(&x::GetAtomName { atom: target }),
                    )
                    .unwrap();
                let name = name.name().to_string();
                let target = SelectionTarget {
                    atom: target,
                    name,
                    value: None,
                };
                let dest = (property, idx);
                (target, dest)
            })
            .unzip();

        self.selection_data.mime_types = Rc::new(types);
        self.selection_data.mime_destinations = dests;
    }

    fn handle_new_clipboard_data(&mut self, server_state: &mut RealServerState) {
        for (property, idx) in std::mem::take(&mut self.selection_data.mime_destinations) {
            let types = Rc::get_mut(&mut self.selection_data.mime_types).unwrap();
            let target = &mut types[idx];
            let data = {
                if target.atom == self.atoms.timestamp {
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
                            property,
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
                            let atom = target.atom;
                            let target = self.get_atom_name(atom);
                            let ty = if reply.r#type() == x::ATOM_NONE {
                                "None".to_string()
                            } else {
                                self.get_atom_name(reply.r#type())
                            };
                            warn!("unexpected format: {other} (atom: {target}, type: {ty:?}, property: {property:?}) - copies as this type will fail!");
                            continue;
                        }
                    }
                }
            };

            target.value = Some(data);
        }

        self.connection
            .send_and_check_request(&x::DeleteProperty {
                window: self.wm_window,
                property: self.atoms.selection_reply,
            })
            .unwrap();

        self.set_clipboard_owner(self.selection_data.clear_time.unwrap());
        server_state.set_copy_paste_source(Rc::clone(&self.selection_data.mime_types));
    }
}
