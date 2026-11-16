//! Wayland -> X11 drag and drop: satellite plays the XDND source for a Wayland drag that hovers
//! one of our X windows, and feeds the drop from the Wayland data offer.
use super::{XState, get_atom_name};
use crate::RealServerState;
use crate::server::dnd::DndEvent;
use log::{debug, warn};
use smithay_client_toolkit::data_device_manager::data_offer::DragOffer;
use wayland_client::protocol::wl_data_device_manager::DndAction;
use xcb::{Xid, x};

const XDND_VERSION: u32 = 5;

struct Target {
    /// Window named in XDND messages.
    window: x::Window,
    /// Window the messages go to (differs when the target uses XdndProxy).
    dest: x::Window,
    version: u32,
}

struct WlDrag {
    offer: DragOffer,
    /// Wayland mime type and its X target atom.
    mimes: Vec<(String, x::Atom)>,
    toplevel: x::Window,
    pos: (i16, i16),
    target: Option<Target>,
    awaiting_status: bool,
    moved_while_waiting: bool,
    accepted: bool,
    dropped: bool,
}

pub(super) struct Dnd {
    window: x::Window,
    drag: Option<WlDrag>,
}

impl Dnd {
    pub fn new(connection: &xcb::Connection, root: x::Window) -> Self {
        let window = connection.generate_id();
        connection
            .send_and_check_request(&x::CreateWindow {
                wid: window,
                width: 1,
                height: 1,
                depth: 0,
                parent: root,
                x: 0,
                y: 0,
                border_width: 0,
                class: x::WindowClass::InputOnly,
                visual: x::COPY_FROM_PARENT,
                value_list: &[],
            })
            .expect("Couldn't create window for drag and drop");
        Self { window, drag: None }
    }
}

fn send_xdnd(
    connection: &xcb::Connection,
    source: x::Window,
    target: &Target,
    r#type: x::Atom,
    data: [u32; 4],
) {
    let event = x::ClientMessageEvent::new(
        target.window,
        r#type,
        x::ClientMessageData::Data32([source.resource_id(), data[0], data[1], data[2], data[3]]),
    );
    if let Err(e) = connection.send_and_check_request(&x::SendEvent {
        propagate: false,
        destination: x::SendEventDest::Window(target.dest),
        event_mask: x::EventMask::empty(),
        event: &event,
    }) {
        warn!("Failed to send XDND message: {e:?}");
    }
}

impl XState {
    fn intern(&self, name: &str) -> x::Atom {
        self.connection
            .wait_for_reply(self.connection.send_request(&x::InternAtom {
                only_if_exists: false,
                name: name.as_bytes(),
            }))
            .map(|r| r.atom())
            .unwrap_or(x::ATOM_NONE)
    }

    fn window_property(
        &self,
        window: x::Window,
        property: x::Atom,
        r#type: x::Atom,
    ) -> Option<u32> {
        let reply = self
            .connection
            .wait_for_reply(self.connection.send_request(&x::GetProperty {
                delete: false,
                window,
                property,
                r#type,
                long_offset: 0,
                long_length: 1,
            }))
            .ok()?;
        reply.value::<u32>().first().copied()
    }

    fn xdnd_target_of(&self, window: x::Window) -> Option<Target> {
        let proxy = self
            .window_property(window, self.atoms.xdnd_proxy, x::ATOM_WINDOW)
            .map(<x::Window as xcb::XidNew>::new)
            .filter(|w| !w.is_none());
        let version =
            self.window_property(proxy.unwrap_or(window), self.atoms.xdnd_aware, x::ATOM_ATOM)?;
        Some(Target {
            window,
            dest: proxy.unwrap_or(window),
            version: version.min(XDND_VERSION),
        })
    }

    /// The deepest XdndAware window under the root position, starting at our toplevel.
    fn find_xdnd_target(&self, toplevel: x::Window, (x, y): (i16, i16)) -> Option<Target> {
        let mut window = toplevel;
        loop {
            if let Some(target) = self.xdnd_target_of(window) {
                return Some(target);
            }
            let child = self
                .connection
                .wait_for_reply(self.connection.send_request(&x::TranslateCoordinates {
                    src_window: self.root,
                    dst_window: window,
                    src_x: x,
                    src_y: y,
                }))
                .ok()?
                .child();
            if child.is_none() {
                return None;
            }
            window = child;
        }
    }

    fn send_position(&mut self) {
        let Some(drag) = &mut self.dnd.drag else {
            return;
        };
        let Some(target) = &drag.target else {
            return;
        };
        if drag.awaiting_status {
            drag.moved_while_waiting = true;
            return;
        }
        drag.awaiting_status = true;
        drag.moved_while_waiting = false;
        let (x, y) = drag.pos;
        let pos = ((x as u16 as u32) << 16) | y as u16 as u32;
        // Always copy: a "move" would let the Wayland source delete the user's file.
        let data = [
            0,
            pos,
            x::CURRENT_TIME,
            self.atoms.xdnd_action_copy.resource_id(),
        ];
        send_xdnd(
            &self.connection,
            self.dnd.window,
            target,
            self.atoms.xdnd_position,
            data,
        );
    }

    /// Point the drag at whatever X window is under it now; enter/leave as needed.
    fn retarget(&mut self, server_state: &RealServerState) {
        let Some(drag) = &self.dnd.drag else {
            return;
        };
        let new = self.find_xdnd_target(drag.toplevel, drag.pos);
        let same = match (&drag.target, &new) {
            (Some(a), Some(b)) => a.window == b.window,
            (None, None) => true,
            _ => false,
        };
        if !same {
            if let Some(old) = &drag.target {
                send_xdnd(
                    &self.connection,
                    self.dnd.window,
                    old,
                    self.atoms.xdnd_leave,
                    [0; 4],
                );
            }
            if let Some(target) = &new {
                let types: Vec<u32> = drag.mimes.iter().map(|(_, a)| a.resource_id()).collect();
                let more = (types.len() > 3) as u32;
                let t = |i: usize| types.get(i).copied().unwrap_or(0);
                debug!(
                    "XDND enter {:?} (version {})",
                    target.window, target.version
                );
                send_xdnd(
                    &self.connection,
                    self.dnd.window,
                    target,
                    self.atoms.xdnd_enter,
                    [(target.version << 24) | more, t(0), t(1), t(2)],
                );
            }
            let drag = self.dnd.drag.as_mut().unwrap();
            drag.target = new;
            drag.awaiting_status = false;
            if drag.accepted {
                drag.accepted = false;
                drag.offer.accept_mime_type(drag.offer.serial, None);
                server_state.flush_wayland();
            }
        }
        self.send_position();
    }

    fn end_drag(&mut self) {
        if let Some(drag) = self.dnd.drag.take() {
            drag.offer.destroy();
        }
    }

    pub(crate) fn handle_wl_dnd(&mut self, server_state: &mut RealServerState) {
        for event in server_state.take_dnd_events() {
            match event {
                DndEvent::Enter {
                    offer,
                    window,
                    x,
                    y,
                } => {
                    self.end_drag();
                    let mut mimes: Vec<(String, x::Atom)> = offer
                        .with_mime_types(|m| m.to_vec())
                        .into_iter()
                        .map(|m| {
                            let atom = self.intern(&m);
                            (m, atom)
                        })
                        .collect();
                    if mimes.iter().any(|(m, _)| m == "text/plain;charset=utf-8")
                        && !mimes.iter().any(|(m, _)| m == "UTF8_STRING")
                    {
                        mimes.push(("text/plain;charset=utf-8".into(), self.atoms.utf8_string));
                    }
                    // Put the likely payload first: X targets often look at the first types only.
                    if let Some(i) = mimes.iter().position(|(m, _)| m == "text/uri-list") {
                        mimes.swap(0, i);
                    }
                    let types: Vec<x::Atom> = mimes.iter().map(|(_, a)| *a).collect();
                    let _ = self.connection.send_and_check_request(&x::ChangeProperty {
                        mode: x::PropMode::Replace,
                        window: self.dnd.window,
                        property: self.atoms.xdnd_type_list,
                        r#type: x::ATOM_ATOM,
                        data: &types,
                    });
                    let _ = self
                        .connection
                        .send_and_check_request(&x::SetSelectionOwner {
                            owner: self.dnd.window,
                            selection: self.atoms.xdnd_selection,
                            time: x::CURRENT_TIME,
                        });
                    debug!("Wayland drag entered {window:?} at {x},{y}");
                    self.dnd.drag = Some(WlDrag {
                        offer,
                        mimes,
                        toplevel: window,
                        pos: (x, y),
                        target: None,
                        awaiting_status: false,
                        moved_while_waiting: false,
                        accepted: false,
                        dropped: false,
                    });
                    self.retarget(server_state);
                }
                DndEvent::Motion { x, y } => {
                    if let Some(drag) = self.dnd.drag.as_mut().filter(|d| !d.dropped) {
                        drag.pos = (x, y);
                        self.retarget(server_state);
                    }
                }
                DndEvent::Leave => {
                    if let Some(drag) = self.dnd.drag.as_ref().filter(|d| !d.dropped) {
                        if let Some(target) = &drag.target {
                            send_xdnd(
                                &self.connection,
                                self.dnd.window,
                                target,
                                self.atoms.xdnd_leave,
                                [0; 4],
                            );
                        }
                        self.end_drag();
                    }
                }
                DndEvent::Drop(offer) => {
                    let Some(drag) = &mut self.dnd.drag else {
                        offer.destroy();
                        continue;
                    };
                    // The drop snapshot of the offer is the one that may still receive data.
                    drag.offer = offer;
                    drag.dropped = true;
                    match &drag.target {
                        Some(target) if drag.accepted => {
                            debug!("XDND drop on {:?}", target.window);
                            send_xdnd(
                                &self.connection,
                                self.dnd.window,
                                target,
                                self.atoms.xdnd_drop,
                                [0, x::CURRENT_TIME, 0, 0],
                            );
                        }
                        target => {
                            if let Some(target) = target {
                                send_xdnd(
                                    &self.connection,
                                    self.dnd.window,
                                    target,
                                    self.atoms.xdnd_leave,
                                    [0; 4],
                                );
                            }
                            self.end_drag();
                        }
                    }
                }
            }
        }
        server_state.flush_wayland();
    }

    /// XDND replies from the target and data requests for XdndSelection.
    pub(super) fn handle_dnd_event(
        &mut self,
        event: &xcb::Event,
        server_state: &mut RealServerState,
    ) -> bool {
        match event {
            xcb::Event::X(x::Event::ClientMessage(e)) if e.window() == self.dnd.window => {
                let x::ClientMessageData::Data32(data) = e.data() else {
                    return true;
                };
                let Some(drag) = &mut self.dnd.drag else {
                    return true;
                };
                if drag.target.as_ref().map(|t| t.window.resource_id()) != Some(data[0]) {
                    return true;
                }
                if e.r#type() == self.atoms.xdnd_status {
                    let accepted = data[1] & 1 != 0;
                    drag.awaiting_status = false;
                    if accepted != drag.accepted && !drag.dropped {
                        drag.accepted = accepted;
                        let mime = accepted.then(|| drag.mimes[0].0.clone());
                        debug!("XDND target accepts: {mime:?}");
                        drag.offer.accept_mime_type(drag.offer.serial, mime);
                        drag.offer.set_actions(
                            if accepted {
                                DndAction::Copy
                            } else {
                                DndAction::empty()
                            },
                            DndAction::Copy,
                        );
                    }
                    if drag.moved_while_waiting && !drag.dropped {
                        self.send_position();
                    }
                } else if e.r#type() == self.atoms.xdnd_finished {
                    debug!("XDND finished");
                    if drag.dropped {
                        drag.offer.finish();
                    }
                    self.end_drag();
                }
                server_state.flush_wayland();
                true
            }
            xcb::Event::X(x::Event::SelectionRequest(e))
                if e.selection() == self.atoms.xdnd_selection =>
            {
                let property = self.convert_dnd(e, server_state);
                if let Err(err) = self.connection.send_and_check_request(&x::SendEvent {
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
                    warn!("Failed to answer XdndSelection request: {err:?}");
                }
                true
            }
            xcb::Event::X(x::Event::SelectionClear(e))
                if e.selection() == self.atoms.xdnd_selection =>
            {
                true
            }
            _ => false,
        }
    }

    fn convert_dnd(&self, e: &x::SelectionRequestEvent, server_state: &RealServerState) -> x::Atom {
        let Some(drag) = &self.dnd.drag else {
            return x::ATOM_NONE;
        };
        let property = if e.property() == x::ATOM_NONE {
            e.target()
        } else {
            e.property()
        };
        let write = |r#type: x::Atom, data: &[u8]| {
            self.connection.send_and_check_request(&x::ChangeProperty {
                mode: x::PropMode::Replace,
                window: e.requestor(),
                property,
                r#type,
                data,
            })
        };
        let result = if e.target() == self.atoms.targets {
            let atoms: Vec<x::Atom> = drag.mimes.iter().map(|(_, a)| *a).collect();
            self.connection.send_and_check_request(&x::ChangeProperty {
                mode: x::PropMode::Replace,
                window: e.requestor(),
                property,
                r#type: x::ATOM_ATOM,
                data: &atoms,
            })
        } else if let Some((mime, _)) = drag.mimes.iter().find(|(_, a)| *a == e.target()) {
            let data = server_state.receive_dnd(&drag.offer, mime.clone());
            // shortcut: no INCR here; drops are paths and short text, a huge payload is refused.
            if data.len() > self.max_req_bytes {
                warn!("Dropped data too large ({} bytes)", data.len());
                return x::ATOM_NONE;
            }
            write(e.target(), &data)
        } else {
            debug!(
                "refusing XDND conversion to {}",
                get_atom_name(&self.connection, e.target())
            );
            return x::ATOM_NONE;
        };
        match result {
            Ok(()) => property,
            Err(err) => {
                warn!("Failed to write dropped data: {err:?}");
                x::ATOM_NONE
            }
        }
    }
}
