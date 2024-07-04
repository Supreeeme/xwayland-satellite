use super::*;
use log::{debug, trace, warn};
use std::collections::HashSet;
use std::os::fd::AsFd;
use wayland_client::{protocol as client, Proxy};
use wayland_protocols::{
    wp::{
        pointer_constraints::zv1::{
            client::{
                zwp_confined_pointer_v1::{self, ZwpConfinedPointerV1 as ConfinedPointerClient},
                zwp_locked_pointer_v1::{self, ZwpLockedPointerV1 as LockedPointerClient},
            },
            server::{
                zwp_confined_pointer_v1::ZwpConfinedPointerV1 as ConfinedPointerServer,
                zwp_locked_pointer_v1::ZwpLockedPointerV1 as LockedPointerServer,
            },
        },
        relative_pointer::zv1::{
            client::zwp_relative_pointer_v1::{
                self, ZwpRelativePointerV1 as RelativePointerClient,
            },
            server::zwp_relative_pointer_v1::ZwpRelativePointerV1 as RelativePointerServer,
        },
    },
    xdg::{
        shell::client::{xdg_popup, xdg_surface, xdg_toplevel},
        xdg_output::zv1::{
            client::zxdg_output_v1::{self, ZxdgOutputV1 as ClientXdgOutput},
            server::zxdg_output_v1::ZxdgOutputV1 as ServerXdgOutput,
        },
    },
};
use wayland_server::protocol::{
    wl_buffer::WlBuffer, wl_keyboard::WlKeyboard, wl_output::WlOutput, wl_pointer::WlPointer,
    wl_seat::WlSeat, wl_touch::WlTouch,
};

/// Lord forgive me, I am a sinner, who's probably gonna sin again
/// This macro takes an enum variant name and a list of the field names of the enum
/// and/or closures that take an argument that must be named the same as the field name,
/// and converts that into a destructured enum
/// shunt_helper_enum!(Foo [a, |b| b.do_thing(), c]) -> Foo {a, b, c}
macro_rules! shunt_helper_enum {
    // No fields
    ($variant:ident) => { $variant };
    // Starting state: variant destructure
    ($variant:ident $([$($body:tt)+])?) => {
        shunt_helper_enum!($variant [$($($body)+)?] -> [])
    };
    // Add field to list
    ($variant:ident [$field:ident $(, $($rest:tt)+)?] -> [$($body:tt)*]) => {
        shunt_helper_enum!($variant [$($($rest)+)?] -> [$($body)*, $field])
    };
    // Add closure field to list
    ($variant:ident [|$field:ident| $conv:expr $(, $($rest:tt)+)?] -> [$($body:tt)*]) => {
        shunt_helper_enum!($variant [$($($rest)+)?] -> [$($body)*, $field])
    };
    // Finalize into enum variant
    ($variant:ident [] -> [,$($body:tt)+]) => { $variant { $($body)+ } };
}

/// This does the same thing as shunt_helper_enum, except it transforms the fields into the given
/// function/method call.
/// shunt_helper_fn!({obj.foo} [a, |b| b.do_thing(), c]) -> obj.foo(a, b.do_thing(), c)
macro_rules! shunt_helper_fn {
    // No fields
    ({$($fn:tt)+}) => { $($fn)+() };
    // Starting state
    ($fn:tt [$($body:tt)+]) => {
        shunt_helper_fn!($fn [$($body)+] -> [])
    };
    // Add field to list
    ($fn:tt [$field:ident $(, $($rest:tt)+)?] -> [$($body:tt)*]) => {
        shunt_helper_fn!($fn [$($($rest)+)?] -> [$($body)*, $field])
    };
    // Add closure expression to list
    ($fn:tt [|$field:ident| $conv:expr $(, $($rest:tt)+)?] -> [$($body:tt)*]) => {
        shunt_helper_fn!($fn [$($($rest)+)?] -> [$($body)*, $conv])
    };
    // Finalize into function call
    ({$($fn:tt)+} [] -> [,$($body:tt)+]) => { $($fn)+($($body)+) };
}

/// Takes an object, the name of a variable holding an event, the event type, and a list of the
/// variants with their fields, and converts them into function calls on their arguments
/// Event { field1, field2 } => obj.event(field1, field2)
macro_rules! simple_event_shunt {
    ($obj:expr, $event:ident: $event_type:path => [
        $( $variant:ident $({ $($fields:tt)* })? ),+
    ]) => {
        {
        use $event_type::*;
        match $event {
            $(
                shunt_helper_enum!( $variant $( [ $($fields)* ] )? ) => {
                    paste::paste! {
                        shunt_helper_fn!( { $obj.[<$variant:snake>] } $( [ $($fields)* ] )? )
                    }
                }
            )+
            _ => log::warn!(concat!("unhandled ", stringify!($event_type), ": {:?}"), $event)
        }
        }
    }
}

pub(crate) use shunt_helper_enum;
pub(crate) use shunt_helper_fn;
pub(crate) use simple_event_shunt;

#[derive(Debug)]
pub(crate) enum SurfaceEvents {
    WlSurface(client::wl_surface::Event),
    XdgSurface(xdg_surface::Event),
    Toplevel(xdg_toplevel::Event),
    Popup(xdg_popup::Event),
}
macro_rules! impl_from {
    ($type:ty, $variant:ident) => {
        impl From<$type> for ObjectEvent {
            fn from(value: $type) -> Self {
                Self::Surface(SurfaceEvents::$variant(value))
            }
        }
    };
}
impl_from!(client::wl_surface::Event, WlSurface);
impl_from!(xdg_surface::Event, XdgSurface);
impl_from!(xdg_toplevel::Event, Toplevel);
impl_from!(xdg_popup::Event, Popup);

impl HandleEvent for SurfaceData {
    type Event = SurfaceEvents;
    fn handle_event<C: XConnection>(&mut self, event: Self::Event, state: &mut ServerState<C>) {
        match event {
            SurfaceEvents::WlSurface(event) => self.surface_event(event, state),
            SurfaceEvents::XdgSurface(event) => self.xdg_event(event, state),
            SurfaceEvents::Toplevel(event) => self.toplevel_event(event, state),
            SurfaceEvents::Popup(event) => self.popup_event(event, state),
        }
    }
}

impl SurfaceData {
    fn surface_event<C: XConnection>(
        &self,
        event: client::wl_surface::Event,
        state: &mut ServerState<C>,
    ) {
        use client::wl_surface::Event;

        match event {
            Event::Enter { output } => {
                let key: ObjectKey = output.data().copied().unwrap();
                let Some(object) = state.objects.get_mut(key) else {
                    return;
                };
                let output: &mut Output = object.as_mut();

                if let Some(win_data) = self
                    .window
                    .as_ref()
                    .map(|win| state.windows.get_mut(&win).unwrap())
                {
                    win_data.update_output_offset(
                        key,
                        WindowOutputOffset {
                            x: output.x,
                            y: output.y,
                        },
                        state.connection.as_mut().unwrap(),
                    );
                    output.windows.insert(win_data.window);
                }
                self.server.enter(&output.server);
                debug!("{} entered {}", self.server.id(), output.server.id());
            }
            Event::Leave { output } => {
                let key: ObjectKey = output.data().copied().unwrap();
                let Some(object) = state.objects.get_mut(key) else {
                    return;
                };
                let output: &mut Output = object.as_mut();
                self.server.leave(&output.server);
            }
            Event::PreferredBufferScale { factor } => self.server.preferred_buffer_scale(factor),
            other => warn!("unhandled surface request: {other:?}"),
        }
    }

    fn xdg_event<C: XConnection>(&mut self, event: xdg_surface::Event, state: &mut ServerState<C>) {
        let connection = state.connection.as_mut().unwrap();
        let xdg_surface::Event::Configure { serial } = event else {
            unreachable!();
        };

        let xdg = self.xdg_mut().unwrap();
        xdg.surface.ack_configure(serial);
        xdg.configured = true;

        if let Some(pending) = xdg.pending.take() {
            let window = state.associated_windows[self.key];
            let window = state.windows.get_mut(&window).unwrap();
            let x = pending.x + window.output_offset.x;
            let y = pending.y + window.output_offset.y;
            let width = if pending.width > 0 {
                pending.width as u16
            } else {
                window.attrs.dims.width
            };
            let height = if pending.height > 0 {
                pending.height as u16
            } else {
                window.attrs.dims.height
            };
            debug!("configuring {:?}: {x}x{y}, {width}x{height}", window.window);
            connection.set_window_dims(
                window.window,
                PendingSurfaceState {
                    x,
                    y,
                    width: width as _,
                    height: height as _,
                },
            );
            window.attrs.dims = WindowDims {
                x: x as i16,
                y: y as i16,
                width,
                height,
            };
        }

        if let Some(SurfaceAttach { buffer, x, y }) = self.attach.take() {
            self.client.attach(buffer.as_ref(), x, y);
        }
        if let Some(cb) = self.frame_callback.take() {
            self.client.frame(&state.qh, cb);
        }
        self.client.commit();
    }

    fn toplevel_event<C: XConnection>(
        &mut self,
        event: xdg_toplevel::Event,
        state: &mut ServerState<C>,
    ) {
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                debug!("configuring toplevel {width}x{height}, {states:?}");
                let activated = states.contains(&(u32::from(xdg_toplevel::State::Activated) as u8));

                if activated {
                    state.to_focus = Some(self.window.unwrap());
                }

                if let Some(SurfaceRole::Toplevel(Some(toplevel))) = &mut self.role {
                    let prev_fs = toplevel.fullscreen;
                    toplevel.fullscreen =
                        states.contains(&(u32::from(xdg_toplevel::State::Fullscreen) as u8));
                    if toplevel.fullscreen != prev_fs {
                        let window = state.associated_windows[self.key];
                        let data = C::ExtraData::create(state);
                        state.connection.as_mut().unwrap().set_fullscreen(
                            window,
                            toplevel.fullscreen,
                            data,
                        );
                    }
                };

                self.xdg_mut().unwrap().pending = Some(PendingSurfaceState {
                    width,
                    height,
                    ..Default::default()
                });
            }
            xdg_toplevel::Event::Close => {
                let window = state.associated_windows[self.key];
                state.close_x_window(window);
            }
            ref other => warn!("unhandled xdgtoplevel event: {other:?}"),
        }
    }

    fn popup_event<C: XConnection>(&mut self, event: xdg_popup::Event, _: &mut ServerState<C>) {
        match event {
            xdg_popup::Event::Configure {
                x,
                y,
                width,
                height,
            } => {
                trace!("popup configure: {x}x{y}, {width}x{height}");
                self.xdg_mut().unwrap().pending = Some(PendingSurfaceState {
                    x,
                    y,
                    width,
                    height,
                });
            }
            xdg_popup::Event::Repositioned { .. } => {}
            other => todo!("{other:?}"),
        }
    }
}

pub struct GenericObject<Server: Resource, Client: Proxy> {
    pub server: Server,
    pub client: Client,
}

pub type Buffer = GenericObject<WlBuffer, client::wl_buffer::WlBuffer>;
impl HandleEvent for Buffer {
    type Event = client::wl_buffer::Event;
    fn handle_event<C: XConnection>(&mut self, _: Self::Event, _: &mut ServerState<C>) {
        // The only event from a buffer would be the release.
        self.server.release();
    }
}

pub type XdgOutput = GenericObject<ServerXdgOutput, ClientXdgOutput>;
impl HandleEvent for XdgOutput {
    type Event = zxdg_output_v1::Event;
    fn handle_event<C: XConnection>(&mut self, event: Self::Event, _: &mut ServerState<C>) {
        simple_event_shunt! {
            self.server, event: zxdg_output_v1::Event => [
                LogicalPosition { x, y },
                LogicalSize { width, height },
                Done,
                Name { name },
                Description { description }
            ]
        }
    }
}

pub type Seat = GenericObject<WlSeat, client::wl_seat::WlSeat>;
impl HandleEvent for Seat {
    type Event = client::wl_seat::Event;

    fn handle_event<C: XConnection>(&mut self, event: Self::Event, _: &mut ServerState<C>) {
        simple_event_shunt! {
            self.server, event: client::wl_seat::Event => [
                Capabilities { |capabilities| convert_wenum(capabilities) },
                Name { name }
            ]
        }
    }
}

pub struct Pointer {
    server: WlPointer,
    pub client: client::wl_pointer::WlPointer,
    pending_enter: PendingEnter,
}

impl Pointer {
    pub fn new(server: WlPointer, client: client::wl_pointer::WlPointer) -> Self {
        Self {
            server,
            client,
            pending_enter: PendingEnter(None),
        }
    }
}

struct PendingEnter(Option<client::wl_pointer::Event>);

impl HandleEvent for Pointer {
    type Event = client::wl_pointer::Event;

    fn handle_event<C: XConnection>(&mut self, event: Self::Event, state: &mut ServerState<C>) {
        // Workaround GTK (stupidly) autoclosing popups if it receives an wl_pointer.enter
        // event shortly after creation.
        // When Niri creates a popup, it immediately sends wl_pointer.enter on the new surface,
        // generating an EnterNotify event, and Xwayland will send a release button event.
        // In its menu implementation, GTK treats EnterNotify "this menu is now active" and will
        // destroy the menu if this occurs within a 500 ms interval (which it always does with
        // Niri). Other compositors do not run into this problem because they appear to not send
        // wl_pointer.enter until the user actually moves the mouse in the popup.
        let mut process_event = Vec::new();
        match event {
            client::wl_pointer::Event::Enter {
                serial,
                ref surface,
                surface_x,
                surface_y,
            } => 'enter: {
                let surface_key: ObjectKey = surface.data().copied().unwrap();
                let Some(surface_data): Option<&SurfaceData> =
                    state.objects.get(surface_key).map(|o| o.as_ref())
                else {
                    warn!("could not enter surface: stale surface");
                    break 'enter;
                };

                let mut do_enter = || {
                    debug!("entering surface ({serial})");
                    self.server
                        .enter(serial, &surface_data.server, surface_x, surface_y);
                    let window = surface_data.window.unwrap();
                    state.connection.as_mut().unwrap().raise_to_top(window);
                    state.last_hovered = Some(window);
                };

                if matches!(surface_data.role, Some(SurfaceRole::Popup(_))) {
                    match self.pending_enter.0.take() {
                        Some(e) => {
                            let client::wl_pointer::Event::Enter {
                                serial: pending_serial,
                                ..
                            } = e
                            else {
                                unreachable!();
                            };
                            if serial == pending_serial {
                                do_enter();
                            } else {
                                self.pending_enter.0 = Some(event);
                            }
                        }
                        None => {
                            self.pending_enter.0 = Some(event);
                        }
                    }
                } else {
                    self.pending_enter.0.take();
                    do_enter();
                }
            }
            client::wl_pointer::Event::Leave { serial, surface } => {
                if !surface.is_alive() {
                    return;
                }
                debug!("leaving surface ({serial})");
                self.pending_enter.0.take();
                if let Some(surface) = state.get_server_surface_from_client(surface) {
                    self.server.leave(serial, surface);
                } else {
                    warn!("could not leave surface: stale surface");
                }
            }
            client::wl_pointer::Event::Motion {
                time,
                surface_x,
                surface_y,
            } => {
                if let Some(p) = &self.pending_enter.0 {
                    let client::wl_pointer::Event::Enter {
                        serial,
                        surface,
                        surface_x,
                        surface_y,
                    } = p
                    else {
                        unreachable!();
                    };
                    process_event.push(client::wl_pointer::Event::Enter {
                        serial: *serial,
                        surface: surface.clone(),
                        surface_x: *surface_x,
                        surface_y: *surface_y,
                    });
                    process_event.push(event);
                    trace!("resending enter ({serial}) before motion");
                } else {
                    self.server.motion(time, surface_x, surface_y);
                }
            }
            _ => simple_event_shunt! {
                self.server, event: client::wl_pointer::Event => [
                    Enter {
                        serial,
                        |surface| {
                            let Some(surface_data) = state.get_server_surface_from_client(surface) else {
                                return;
                            };
                            surface_data
                        },
                        surface_x,
                        surface_y
                    },
                    Leave {
                        serial,
                        |surface| {
                            let Some(surface_data) = state.get_server_surface_from_client(surface) else {
                                return;
                            };
                            surface_data
                        }
                    },
                    Motion {
                        time,
                        surface_x,
                        surface_y
                    },
                    Frame,
                    Button {
                        serial,
                        time,
                        button,
                        |state| convert_wenum(state)
                    },
                    Axis {
                        time,
                        |axis| convert_wenum(axis),
                        value
                    },
                    AxisSource {
                        |axis_source| convert_wenum(axis_source)
                    },
                    AxisStop {
                        time,
                        |axis| convert_wenum(axis)
                    },
                    AxisDiscrete {
                        |axis| convert_wenum(axis),
                        discrete
                    },
                    AxisValue120 {
                        |axis| convert_wenum(axis),
                        value120
                    },
                    AxisRelativeDirection {
                        |axis| convert_wenum(axis),
                        |direction| convert_wenum(direction)
                    }
                ]
            },
        }

        for event in process_event {
            self.handle_event(event, state);
        }
    }
}

pub type Keyboard = GenericObject<WlKeyboard, client::wl_keyboard::WlKeyboard>;
impl HandleEvent for Keyboard {
    type Event = client::wl_keyboard::Event;

    fn handle_event<C: XConnection>(&mut self, event: Self::Event, state: &mut ServerState<C>) {
        match event {
            client::wl_keyboard::Event::Enter {
                serial,
                surface,
                keys,
            } => {
                state.last_kb_serial = Some(serial);
                if let Some(surface_data) = state.get_server_surface_from_client(surface) {
                    self.server.enter(serial, surface_data, keys);
                }
            }
            _ => simple_event_shunt! {
                self.server, event: client::wl_keyboard::Event => [
                    Keymap {
                        |format| convert_wenum(format),
                        |fd| fd.as_fd(),
                        size
                    },
                    Leave {
                        serial,
                        |surface| {
                            if !surface.is_alive() {
                                return;
                            }
                            let Some(surface_data) = state.get_server_surface_from_client(surface) else {
                                return;
                            };
                            surface_data
                        }
                    },
                    Key {
                        serial,
                        time,
                        key,
                        |state| convert_wenum(state)
                    },
                    Modifiers {
                        serial,
                        mods_depressed,
                        mods_latched,
                        mods_locked,
                        group
                    },
                    RepeatInfo {
                        rate,
                        delay
                    }
                ]
            },
        }
    }
}

pub type Touch = GenericObject<WlTouch, client::wl_touch::WlTouch>;
impl HandleEvent for Touch {
    type Event = client::wl_touch::Event;
    fn handle_event<C: XConnection>(&mut self, event: Self::Event, state: &mut ServerState<C>) {
        simple_event_shunt! {
            self.server, event: client::wl_touch::Event => [
                Down {
                    serial,
                    time,
                    |surface| {
                        let Some(surface_data) = state.get_server_surface_from_client(surface) else {
                            return;
                        };
                        surface_data
                    },
                    id,
                    x,
                    y
                },
                Up {
                    serial,
                    time,
                    id
                },
                Motion {
                    time,
                    id,
                    x,
                    y
                },
                Frame,
                Cancel,
                Shape {
                    id,
                    major,
                    minor
                },
                Orientation {
                    id,
                    orientation
                }
            ]
        }
    }
}

pub struct Output {
    pub client: client::wl_output::WlOutput,
    pub server: WlOutput,
    pub windows: HashSet<x::Window>,
    pub x: i32,
    pub y: i32,
}

impl Output {
    pub fn new(client: client::wl_output::WlOutput, server: WlOutput) -> Self {
        Self {
            client,
            server,
            windows: HashSet::new(),
            x: 0,
            y: 0,
        }
    }
}
impl HandleEvent for Output {
    type Event = client::wl_output::Event;

    fn handle_event<C: XConnection>(&mut self, event: Self::Event, state: &mut ServerState<C>) {
        if let client::wl_output::Event::Geometry { x, y, .. } = event {
            debug!("moving {} to {x}x{y}", self.server.id());
            self.x = x;
            self.y = y;

            self.windows.retain(|window| {
                let Some(data): Option<&mut WindowData> = state.windows.get_mut(window) else {
                    return false;
                };

                data.update_output_offset(
                    self.server.data().copied().unwrap(),
                    WindowOutputOffset {
                        x: self.x,
                        y: self.y,
                    },
                    state.connection.as_mut().unwrap(),
                );

                return true;
            });
        }

        simple_event_shunt! {
            self.server, event: client::wl_output::Event => [
                Name { name },
                Description { description },
                Mode {
                    |flags| convert_wenum(flags),
                    width,
                    height,
                    refresh
                },
                Scale { factor },
                Geometry {
                    x,
                    y,
                    physical_width,
                    physical_height,
                    |subpixel| convert_wenum(subpixel),
                    make,
                    model,
                    |transform| convert_wenum(transform)
                },
                Done
            ]
        }
    }
}

pub type Drm = GenericObject<WlDrmServer, WlDrmClient>;
impl HandleEvent for Drm {
    type Event = wl_drm::client::wl_drm::Event;

    fn handle_event<C: XConnection>(&mut self, event: Self::Event, _: &mut ServerState<C>) {
        simple_event_shunt! {
            self.server, event: wl_drm::client::wl_drm::Event => [
                Device { name },
                Format { format },
                Authenticated,
                Capabilities { value }
            ]
        }
    }
}

pub type DmabufFeedback = GenericObject<
    s_dmabuf::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
    c_dmabuf::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
>;
impl HandleEvent for DmabufFeedback {
    type Event = c_dmabuf::zwp_linux_dmabuf_feedback_v1::Event;

    fn handle_event<C: XConnection>(&mut self, event: Self::Event, _: &mut ServerState<C>) {
        simple_event_shunt! {
            self.server, event: c_dmabuf::zwp_linux_dmabuf_feedback_v1::Event => [
                Done,
                FormatTable { |fd| fd.as_fd(), size },
                MainDevice { device },
                TrancheDone,
                TrancheTargetDevice { device },
                TrancheFormats { indices },
                TrancheFlags { |flags| convert_wenum(flags) }
            ]
        }
    }
}

pub type RelativePointer = GenericObject<RelativePointerServer, RelativePointerClient>;
impl HandleEvent for RelativePointer {
    type Event = zwp_relative_pointer_v1::Event;

    fn handle_event<C: XConnection>(&mut self, event: Self::Event, _: &mut ServerState<C>) {
        simple_event_shunt! {
            self.server, event: zwp_relative_pointer_v1::Event => [
                RelativeMotion {
                    utime_hi,
                    utime_lo,
                    dx,
                    dy,
                    dx_unaccel,
                    dy_unaccel
                }
            ]
        }
    }
}

pub type LockedPointer = GenericObject<LockedPointerServer, LockedPointerClient>;
impl HandleEvent for LockedPointer {
    type Event = zwp_locked_pointer_v1::Event;

    fn handle_event<C: XConnection>(&mut self, event: Self::Event, _: &mut ServerState<C>) {
        simple_event_shunt! {
            self.server, event: zwp_locked_pointer_v1::Event => [
                Locked,
                Unlocked
            ]
        }
    }
}

pub type ConfinedPointer = GenericObject<ConfinedPointerServer, ConfinedPointerClient>;
impl HandleEvent for ConfinedPointer {
    type Event = zwp_confined_pointer_v1::Event;

    fn handle_event<C: XConnection>(&mut self, event: Self::Event, _: &mut ServerState<C>) {
        simple_event_shunt! {
            self.server, event: zwp_confined_pointer_v1::Event => [
                Confined,
                Unconfined
            ]
        }
    }
}
