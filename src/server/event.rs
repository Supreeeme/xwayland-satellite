use super::*;
use crate::clientside::LateInitObjectKey;
use log::{debug, trace, warn};
use macros::simple_event_shunt;
use std::collections::HashSet;
use std::os::fd::AsFd;
use wayland_client::{protocol as client, Proxy};
use wayland_protocols::{
    wp::{
        fractional_scale::v1::client::wp_fractional_scale_v1,
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
        tablet::zv2::{
            client::{
                zwp_tablet_pad_group_v2::{self, ZwpTabletPadGroupV2 as TabletPadGroupClient},
                zwp_tablet_pad_ring_v2::{self, ZwpTabletPadRingV2 as TabletPadRingClient},
                zwp_tablet_pad_strip_v2::{self, ZwpTabletPadStripV2 as TabletPadStripClient},
                zwp_tablet_pad_v2::{self, ZwpTabletPadV2 as TabletPadClient},
                zwp_tablet_seat_v2::{self, ZwpTabletSeatV2 as TabletSeatClient},
                zwp_tablet_tool_v2::{self, ZwpTabletToolV2 as TabletToolClient},
                zwp_tablet_v2::{self, ZwpTabletV2 as TabletClient},
            },
            server::{
                zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2 as TabletPadGroupServer,
                zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2 as TabletPadRingServer,
                zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2 as TabletPadStripServer,
                zwp_tablet_pad_v2::ZwpTabletPadV2 as TabletPadServer,
                zwp_tablet_seat_v2::ZwpTabletSeatV2 as TabletSeatServer,
                zwp_tablet_tool_v2::ZwpTabletToolV2 as TabletToolServer,
                zwp_tablet_v2::ZwpTabletV2 as TabletServer,
            },
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

#[derive(Debug)]
pub(crate) enum SurfaceEvents {
    WlSurface(client::wl_surface::Event),
    XdgSurface(xdg_surface::Event),
    Toplevel(xdg_toplevel::Event),
    Popup(xdg_popup::Event),
    FractionalScale(wp_fractional_scale_v1::Event),
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
impl_from!(wp_fractional_scale_v1::Event, FractionalScale);

impl HandleEvent for SurfaceData {
    type Event = SurfaceEvents;
    fn handle_event<C: XConnection>(&mut self, event: Self::Event, state: &mut ServerState<C>) {
        match event {
            SurfaceEvents::WlSurface(event) => self.surface_event(event, state),
            SurfaceEvents::XdgSurface(event) => self.xdg_event(event, state),
            SurfaceEvents::Toplevel(event) => self.toplevel_event(event, state),
            SurfaceEvents::Popup(event) => self.popup_event(event, state),
            SurfaceEvents::FractionalScale(event) => match event {
                wp_fractional_scale_v1::Event::PreferredScale { scale } => {
                    self.scale_factor = scale as f64 / 120.0;
                    log::debug!("{} scale factor: {}", self.server.id(), self.scale_factor);
                    if let Some(win_data) = self
                        .window
                        .as_ref()
                        .and_then(|win| state.windows.get_mut(win))
                    {
                        self.update_viewport(win_data.attrs.dims, win_data.attrs.size_hints);
                    }
                }
                _ => unreachable!(),
            },
        }
    }
}

impl SurfaceData {
    fn get_output_name(&self, state: &ServerState<impl XConnection>) -> Option<String> {
        let output_name = self
            .output_key
            .and_then(|key| state.objects.get(key))
            .map(|obj| <_ as AsRef<Output>>::as_ref(obj).name.clone());

        if output_name.is_none() {
            warn!(
                "{} has no output name ({:?})",
                self.server.id(),
                self.output_key
            );
        }

        output_name
    }

    pub(super) fn update_viewport(&self, dims: WindowDims, size_hints: Option<WmNormalHints>) {
        let width = (dims.width as f64 / self.scale_factor) as i32;
        let height = (dims.height as f64 / self.scale_factor) as i32;
        if width > 0 && height > 0 {
            self.viewport.set_destination(width, height);
        }
        debug!("{} viewport: {width}x{height}", self.server.id());
        if let Some(hints) = size_hints {
            let Some(SurfaceRole::Toplevel(Some(data))) = &self.role else {
                warn!(
                    "Trying to update size hints on {}, but toplevel role data is missing",
                    self.server.id()
                );
                return;
            };

            if let Some(min) = hints.min_size {
                data.toplevel.set_min_size(
                    (min.width as f64 / self.scale_factor) as i32,
                    (min.height as f64 / self.scale_factor) as i32,
                );
            }
            if let Some(max) = hints.max_size {
                data.toplevel.set_max_size(
                    (max.width as f64 / self.scale_factor) as i32,
                    (max.height as f64 / self.scale_factor) as i32,
                );
            }
        }
    }

    fn surface_event<C: XConnection>(
        &mut self,
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
                self.server.enter(&output.server);
                if state.fractional_scale.is_none() {
                    self.scale_factor = output.scale as f64;
                }
                self.output_key = Some(key);
                debug!("{} entered {}", self.server.id(), output.server.id());

                let windows = &mut state.windows;
                if let Some(win_data) = self.window.as_ref().and_then(|win| windows.get_mut(win)) {
                    if state.fractional_scale.is_none() {
                        self.update_viewport(win_data.attrs.dims, win_data.attrs.size_hints);
                    }
                    win_data.update_output_offset(
                        key,
                        WindowOutputOffset {
                            x: output.dimensions.x - state.global_output_offset.x.value,
                            y: output.dimensions.y - state.global_output_offset.y.value,
                        },
                        state.connection.as_mut().unwrap(),
                    );
                    let window = win_data.window;
                    output.windows.insert(window);
                    if self.window.is_some() && state.last_focused_toplevel == self.window {
                        let output = self.get_output_name(state);
                        let conn = state.connection.as_mut().unwrap();
                        debug!("focused window changed outputs - resetting primary output");
                        conn.focus_window(window, output);
                    }
                }
            }
            Event::Leave { output } => {
                let key: ObjectKey = output.data().copied().unwrap();
                let Some(object) = state.objects.get_mut(key) else {
                    return;
                };
                let output: &mut Output = object.as_mut();
                self.server.leave(&output.server);
                if self.output_key == Some(key) {
                    self.output_key = None;
                }
            }
            Event::PreferredBufferScale { .. } => {}
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
            let x = (pending.x as f64 * self.scale_factor) as i32 + window.output_offset.x;
            let y = (pending.y as f64 * self.scale_factor) as i32 + window.output_offset.y;
            let width = if pending.width > 0 {
                (pending.width as f64 * self.scale_factor) as u16
            } else {
                window.attrs.dims.width
            };
            let height = if pending.height > 0 {
                (pending.height as f64 * self.scale_factor) as u16
            } else {
                window.attrs.dims.height
            };
            debug!(
                "configuring {} ({:?}): {x}x{y}, {width}x{height}",
                self.server.id(),
                window.window
            );
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
            self.update_viewport(window.attrs.dims, window.attrs.size_hints);
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
                debug!(
                    "configuring toplevel {} {width}x{height}, {states:?}",
                    self.server.id()
                );
                if let Some(SurfaceRole::Toplevel(Some(toplevel))) = &mut self.role {
                    let prev_fs = toplevel.fullscreen;
                    toplevel.fullscreen =
                        states.contains(&(u32::from(xdg_toplevel::State::Fullscreen) as u8));
                    if toplevel.fullscreen != prev_fs {
                        let window = state.associated_windows[self.key];
                        state
                            .connection
                            .as_mut()
                            .unwrap()
                            .set_fullscreen(window, toplevel.fullscreen);
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
            // TODO: support capabilities (minimize, maximize, etc)
            xdg_toplevel::Event::WmCapabilities { .. } => {}
            xdg_toplevel::Event::ConfigureBounds { .. } => {}
            ref other => warn!("unhandled xdgtoplevel event: {other:?}"),
        }
    }

    fn popup_event<C: XConnection>(&mut self, event: xdg_popup::Event, state: &mut ServerState<C>) {
        match event {
            xdg_popup::Event::Configure {
                x,
                y,
                width,
                height,
            } => {
                trace!(
                    "popup configure {}: {x}x{y}, {width}x{height}",
                    self.server.id()
                );
                self.xdg_mut().unwrap().pending = Some(PendingSurfaceState {
                    x,
                    y,
                    width,
                    height,
                });
            }
            xdg_popup::Event::Repositioned { .. } => {}
            xdg_popup::Event::PopupDone => {
                state
                    .connection
                    .as_mut()
                    .unwrap()
                    .unmap_window(self.window.unwrap());
            }
            other => todo!("{other:?}"),
        }
    }
}

pub struct GenericObject<Server: Resource, Client: Proxy> {
    pub server: Server,
    pub client: Client,
}

impl<S: Resource + 'static, C: Proxy + 'static> GenericObject<S, C> {
    fn from_client<XC: XConnection>(client: C, state: &mut ServerState<XC>) -> &Self
    where
        Self: Into<WrappedObject>,
        for<'a> &'a Self: TryFrom<&'a Object, Error = String>,
        ServerState<XC>: wayland_server::Dispatch<S, ObjectKey>,
        C::Event: Send + Into<ObjectEvent>,
    {
        let key = state.objects.insert_with_key(|key| {
            let server = state
                .client
                .as_ref()
                .unwrap()
                .create_resource::<_, _, ServerState<XC>>(&state.dh, 1, key)
                .unwrap();
            let obj_key: &LateInitObjectKey<C> = client.data().unwrap();
            obj_key.init(key);

            Self { client, server }.into()
        });

        state.objects[key].as_ref()
    }
}

pub trait GenericObjectExt {
    type Server: Resource;
    type Client: Proxy;
}

impl<S: Resource, C: Proxy> GenericObjectExt for GenericObject<S, C> {
    type Server = S;
    type Client = C;
}

pub type Buffer = GenericObject<WlBuffer, client::wl_buffer::WlBuffer>;
impl HandleEvent for Buffer {
    type Event = client::wl_buffer::Event;
    fn handle_event<C: XConnection>(&mut self, _: Self::Event, _: &mut ServerState<C>) {
        // The only event from a buffer would be the release.
        self.server.release();
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
    scale: f64,
}

impl Pointer {
    pub fn new(server: WlPointer, client: client::wl_pointer::WlPointer) -> Self {
        Self {
            server,
            client,
            pending_enter: PendingEnter(None),
            scale: 1.0,
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
        match event {
            client::wl_pointer::Event::Enter {
                serial,
                ref surface,
                surface_x,
                surface_y,
            } => 'enter: {
                let Some(surface_data): Option<&SurfaceData> = surface
                    .data::<ObjectKey>()
                    .copied()
                    .and_then(|key| state.objects.get(key))
                    .map(|o| o.as_ref())
                else {
                    warn!("could not enter surface: stale surface");
                    break 'enter;
                };

                self.scale = surface_data.scale_factor;
                let surface_is_popup = matches!(surface_data.role, Some(SurfaceRole::Popup(_)));
                let mut do_enter = || {
                    debug!("pointer entering {} ({serial})", surface_data.server.id());
                    self.server.enter(
                        serial,
                        &surface_data.server,
                        surface_x * self.scale,
                        surface_y * self.scale,
                    );
                    let window = surface_data.window.unwrap();
                    state.connection.as_mut().unwrap().raise_to_top(window);
                    if !surface_is_popup {
                        state.last_hovered = Some(window);
                    }
                };

                if surface_is_popup {
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
                    if surface
                        .data()
                        .copied()
                        .and_then(|key| state.objects.get(key))
                        .is_some()
                    {
                        trace!("resending enter ({serial}) before motion");
                        let enter_event = client::wl_pointer::Event::Enter {
                            serial: *serial,
                            surface: surface.clone(),
                            surface_x: *surface_x,
                            surface_y: *surface_y,
                        };
                        self.handle_event(enter_event, state);
                        self.handle_event(event, state);
                    } else {
                        warn!("could not move pointer to surface ({serial}): stale surface");
                    }
                } else {
                    trace!(
                        target: "pointer_position",
                        "pointer motion {} {}",
                        surface_x * self.scale,
                        surface_y * self.scale
                    );
                    self.server
                        .motion(time, surface_x * self.scale, surface_y * self.scale);
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
    }
}

pub struct Keyboard {
    pub server: WlKeyboard,
    pub client: client::wl_keyboard::WlKeyboard,
    pub seat: client::wl_seat::WlSeat,
}

impl HandleEvent for Keyboard {
    type Event = client::wl_keyboard::Event;

    fn handle_event<C: XConnection>(&mut self, event: Self::Event, state: &mut ServerState<C>) {
        match event {
            client::wl_keyboard::Event::Enter {
                serial,
                surface,
                keys,
            } => {
                if let Some(data) = surface.data().copied().and_then(|key| {
                    state
                        .objects
                        .get(key)
                        .map(<_ as AsRef<SurfaceData>>::as_ref)
                }) {
                    state.last_kb_serial = Some((
                        state
                            .last_kb_serial
                            .take()
                            .and_then(|(seat, _)| (seat == self.seat).then_some(seat))
                            .unwrap_or_else(|| self.seat.clone()),
                        serial,
                    ));
                    let output_name = data.get_output_name(state);
                    state.to_focus = Some(FocusData {
                        window: data.window.unwrap(),
                        output_name,
                    });
                    self.server.enter(serial, &data.server, keys);
                }
            }
            client::wl_keyboard::Event::Leave { serial, surface } => {
                if !surface.is_alive() {
                    return;
                }
                if let Some(data) = surface.data().copied().and_then(|key| {
                    state
                        .objects
                        .get(key)
                        .map(<_ as AsRef<SurfaceData>>::as_ref)
                }) {
                    if state.to_focus.as_ref().map(|d| d.window) == Some(data.window.unwrap()) {
                        state.to_focus.take();
                    } else {
                        state.unfocus = true;
                    }
                    self.server.leave(serial, &data.server);
                }
            }
            client::wl_keyboard::Event::Key {
                serial,
                time,
                key,
                state: key_state,
            } => {
                state.last_kb_serial = Some((
                    state
                        .last_kb_serial
                        .take()
                        .and_then(|(seat, _)| (seat == self.seat).then_some(seat))
                        .unwrap_or_else(|| self.seat.clone()),
                    serial,
                ));
                self.server.key(serial, time, key, convert_wenum(key_state));
            }
            _ => simple_event_shunt! {
                self.server, event: client::wl_keyboard::Event => [
                    Keymap {
                        |format| convert_wenum(format),
                        |fd| fd.as_fd(),
                        size
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

pub struct XdgOutput {
    pub client: ClientXdgOutput,
    pub server: ServerXdgOutput,
}

enum OutputDimensionsSource {
    // The data in this variant is the values needed for the wl_output.geometry event.
    Wl {
        physical_width: i32,
        physical_height: i32,
        subpixel: WEnum<client::wl_output::Subpixel>,
        make: String,
        model: String,
        transform: WEnum<client::wl_output::Transform>,
    },
    Xdg,
}

pub(super) struct OutputDimensions {
    source: OutputDimensionsSource,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub struct Output {
    pub client: client::wl_output::WlOutput,
    pub server: WlOutput,
    pub xdg: Option<XdgOutput>,
    windows: HashSet<x::Window>,
    pub(super) dimensions: OutputDimensions,
    name: String,
    scale: i32,
    swap_dimensions: bool,
}

impl Output {
    pub fn new(client: client::wl_output::WlOutput, server: WlOutput) -> Self {
        Self {
            client,
            server,
            xdg: None,
            windows: HashSet::new(),
            dimensions: OutputDimensions {
                source: OutputDimensionsSource::Wl {
                    physical_height: 0,
                    physical_width: 0,
                    subpixel: WEnum::Value(client::wl_output::Subpixel::Unknown),
                    make: String::new(),
                    model: String::new(),
                    transform: WEnum::Value(client::wl_output::Transform::Normal),
                },
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
            name: "<unknown>".to_string(),
            scale: 1,
            swap_dimensions: false,
        }
    }

    pub(super) fn scale(&self) -> i32 {
        self.scale
    }
}

#[derive(Debug)]
pub enum OutputEvent {
    Wl(client::wl_output::Event),
    Xdg(zxdg_output_v1::Event),
}

impl From<client::wl_output::Event> for ObjectEvent {
    fn from(value: client::wl_output::Event) -> Self {
        Self::Output(OutputEvent::Wl(value))
    }
}

impl From<zxdg_output_v1::Event> for ObjectEvent {
    fn from(value: zxdg_output_v1::Event) -> Self {
        Self::Output(OutputEvent::Xdg(value))
    }
}

impl HandleEvent for Output {
    type Event = OutputEvent;

    fn handle_event<C: XConnection>(&mut self, event: Self::Event, state: &mut ServerState<C>) {
        match event {
            OutputEvent::Xdg(event) => self.xdg_event(event, state),
            OutputEvent::Wl(event) => self.wl_event(event, state),
        }
    }
}

impl Output {
    pub(super) fn global_offset_updated(&mut self, state: &mut ServerState<impl XConnection>) {
        let x = self.dimensions.x - state.global_output_offset.x.value;
        let y = self.dimensions.y - state.global_output_offset.y.value;

        match &self.dimensions.source {
            OutputDimensionsSource::Wl {
                physical_width,
                physical_height,
                subpixel,
                make,
                model,
                transform,
            } => {
                self.server.geometry(
                    x,
                    y,
                    *physical_width,
                    *physical_height,
                    convert_wenum(*subpixel),
                    make.clone(),
                    model.clone(),
                    convert_wenum(*transform),
                );
            }
            OutputDimensionsSource::Xdg => {
                self.xdg.as_ref().unwrap().server.logical_position(x, y);
            }
        }
        self.server.done();

        self.update_window_offsets(state);
    }

    fn update_window_offsets(&mut self, state: &mut ServerState<impl XConnection>) {
        self.windows.retain(|window| {
            let Some(data): Option<&mut WindowData> = state.windows.get_mut(window) else {
                return false;
            };

            data.update_output_offset(
                self.server.data().copied().unwrap(),
                WindowOutputOffset {
                    x: self.dimensions.x - state.global_output_offset.x.value,
                    y: self.dimensions.y - state.global_output_offset.y.value,
                },
                state.connection.as_mut().unwrap(),
            );

            true
        });
    }

    fn update_offset<C: XConnection>(
        &mut self,
        source: OutputDimensionsSource,
        x: i32,
        y: i32,
        state: &mut ServerState<C>,
    ) {
        if matches!(source, OutputDimensionsSource::Wl { .. })
            && matches!(self.dimensions.source, OutputDimensionsSource::Xdg)
        {
            return;
        }

        let key: ObjectKey = self.server.data().copied().unwrap();
        let global_offset = &mut state.global_output_offset;
        let mut maybe_update_dimension = |value, dim: &mut GlobalOutputOffsetDimension| {
            if value < dim.value {
                *dim = GlobalOutputOffsetDimension {
                    owner: Some(key),
                    value,
                };
                state.global_offset_updated = true;
            } else if dim.owner == Some(key) && value > dim.value {
                *dim = Default::default();
                state.global_offset_updated = true;
            }
        };

        maybe_update_dimension(x, &mut global_offset.x);
        maybe_update_dimension(y, &mut global_offset.y);

        self.dimensions.source = source;
        self.dimensions.x = x;
        self.dimensions.y = y;
        let id = match self.dimensions.source {
            OutputDimensionsSource::Xdg => self.xdg.as_ref().unwrap().server.id(),
            OutputDimensionsSource::Wl { .. } => self.server.id(),
        };
        debug!("moving {id} to {x}x{y}");

        self.update_window_offsets(state);
    }

    fn wl_event<C: XConnection>(
        &mut self,
        event: client::wl_output::Event,
        state: &mut ServerState<C>,
    ) {
        use client::wl_output::Event;
        match event {
            Event::Geometry {
                x,
                y,
                physical_width,
                physical_height,
                subpixel,
                make,
                model,
                transform,
            } => {
                self.update_offset(
                    OutputDimensionsSource::Wl {
                        physical_width,
                        physical_height,
                        subpixel,
                        make: make.clone(),
                        model: model.clone(),
                        transform,
                    },
                    x,
                    y,
                    state,
                );
                self.server.geometry(
                    x - state.global_output_offset.x.value,
                    y - state.global_output_offset.y.value,
                    physical_width,
                    physical_height,
                    convert_wenum(subpixel),
                    make,
                    model,
                    convert_wenum(transform),
                );
                self.swap_dimensions = transform.into_result().is_ok_and(|t| {
                    matches!(
                        t,
                        client::wl_output::Transform::_90
                            | client::wl_output::Transform::_270
                            | client::wl_output::Transform::Flipped90
                            | client::wl_output::Transform::Flipped270
                    )
                });
                if let Some(xdg) = &self.xdg {
                    if self.swap_dimensions {
                        xdg.server
                            .logical_size(self.dimensions.height, self.dimensions.width);
                    } else {
                        xdg.server
                            .logical_size(self.dimensions.width, self.dimensions.height);
                    }
                }
            }
            Event::Mode {
                flags,
                width,
                height,
                refresh,
            } => {
                if flags
                    .into_result()
                    .is_ok_and(|f| f.contains(client::wl_output::Mode::Current))
                {
                    self.dimensions.width = width;
                    self.dimensions.height = height;
                    debug!("{} dimensions: {width}x{height}", self.server.id());
                }
                self.server
                    .mode(convert_wenum(flags), width, height, refresh);
            }
            Event::Scale { factor } => {
                debug!("{} scale: {factor}", self.server.id());
                self.scale = factor;
                if state.fractional_scale.is_none() {
                    self.windows.retain(|window| {
                        let Some(data): Option<&WindowData> = state.windows.get(window) else {
                            return false;
                        };

                        if let Some::<&mut SurfaceData>(surface) = data
                            .surface_key
                            .and_then(|key| state.objects.get_mut(key))
                            .map(AsMut::as_mut)
                        {
                            surface.scale_factor = factor as f64;
                            surface.update_viewport(data.attrs.dims, data.attrs.size_hints);
                        }

                        true
                    });

                    self.server.scale(factor);
                }
                state.output_scales_updated = true;
            }
            _ => simple_event_shunt! {
                self.server, event: Event => [
                    Name {
                        |name| {
                            self.name = name.clone();
                            name
                        }
                    },
                    Description { description },
                    Done
                ]
            },
        }
    }

    fn xdg_event<C: XConnection>(
        &mut self,
        event: zxdg_output_v1::Event,
        state: &mut ServerState<C>,
    ) {
        use zxdg_output_v1::Event;

        let xdg = &self.xdg.as_ref().unwrap().server;
        match event {
            Event::LogicalPosition { x, y } => {
                self.update_offset(OutputDimensionsSource::Xdg, x, y, state);
                self.xdg.as_ref().unwrap().server.logical_position(
                    x - state.global_output_offset.x.value,
                    y - state.global_output_offset.y.value,
                );
            }
            Event::LogicalSize { .. } => {
                if self.swap_dimensions {
                    xdg.logical_size(self.dimensions.height, self.dimensions.width);
                } else {
                    xdg.logical_size(self.dimensions.width, self.dimensions.height);
                }
            }
            _ => simple_event_shunt! {
                xdg, event: zxdg_output_v1::Event => [
                    Done,
                    Name { name },
                    Description { description }
                ]
            },
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

pub type TabletSeat = GenericObject<TabletSeatServer, TabletSeatClient>;
impl HandleEvent for TabletSeat {
    type Event = zwp_tablet_seat_v2::Event;

    fn handle_event<C: XConnection>(&mut self, event: Self::Event, state: &mut ServerState<C>) {
        simple_event_shunt! {
            self.server, event: zwp_tablet_seat_v2::Event => [
                TabletAdded {
                    |id| &Tablet::from_client(id, state).server
                },
                ToolAdded {
                    |id| &TabletTool::from_client(id, state).server
                },
                PadAdded {
                    |id| &TabletPad::from_client(id, state).server
                }
            ]
        }
    }
}

pub type Tablet = GenericObject<TabletServer, TabletClient>;
impl HandleEvent for Tablet {
    type Event = zwp_tablet_v2::Event;

    fn handle_event<C: XConnection>(&mut self, event: Self::Event, _: &mut ServerState<C>) {
        simple_event_shunt! {
            self.server, event: zwp_tablet_v2::Event => [
                Name { name },
                Id { vid, pid },
                Path { path },
                Done,
                Removed
            ]
        }
    }
}

pub type TabletPad = GenericObject<TabletPadServer, TabletPadClient>;
impl HandleEvent for TabletPad {
    type Event = zwp_tablet_pad_v2::Event;

    fn handle_event<C: XConnection>(&mut self, event: Self::Event, state: &mut ServerState<C>) {
        simple_event_shunt! {
            self.server, event: zwp_tablet_pad_v2::Event => [
                Group { |pad_group| &TabletPadGroup::from_client(pad_group, state).server },
                Path { path },
                Buttons { buttons },
                Done,
                Button {
                    time,
                    button,
                    |state| convert_wenum(state)
                },
                Enter {
                    serial,
                    |tablet| {
                        let key: &LateInitObjectKey<TabletClient> = tablet.data().unwrap();
                        let Some(tablet): Option<&Tablet> = state.objects.get(**key).map(|o| o.as_ref()) else {
                            return;
                        };
                        &tablet.server
                    },
                    |surface| {
                        let Some(surface_data) = state.get_server_surface_from_client(surface) else {
                            return;
                        };
                        surface_data
                    }
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
                Removed
            ]
        }
    }
}

pub type TabletTool = GenericObject<TabletToolServer, TabletToolClient>;
impl HandleEvent for TabletTool {
    type Event = zwp_tablet_tool_v2::Event;

    fn handle_event<C: XConnection>(&mut self, event: Self::Event, state: &mut ServerState<C>) {
        simple_event_shunt! {
            self.server, event: zwp_tablet_tool_v2::Event => [
                Type { |tool_type| convert_wenum(tool_type) },
                HardwareSerial { hardware_serial_hi, hardware_serial_lo },
                HardwareIdWacom { hardware_id_hi, hardware_id_lo },
                Capability { |capability| convert_wenum(capability) },
                Done,
                Removed,
                ProximityIn {
                    serial,
                    |tablet| {
                        let key: &LateInitObjectKey<TabletClient> = tablet.data().unwrap();
                        let Some(tablet): Option<&Tablet> = state.objects.get(**key).map(|o| o.as_ref()) else {
                            return;
                        };
                        &tablet.server
                    },
                    |surface| {
                        let Some(surface_data) = state.get_server_surface_from_client(surface) else {
                            return;
                        };
                        surface_data
                    }
                },
                ProximityOut,
                Down { serial },
                Up,
                Motion { x, y },
                Pressure { pressure },
                Tilt { tilt_x, tilt_y },
                Rotation { degrees },
                Slider { position },
                Wheel { degrees, clicks },
                Button { serial, button, |state| convert_wenum(state) },
                Frame { time },
            ]
        }
    }
}

pub type TabletPadGroup = GenericObject<TabletPadGroupServer, TabletPadGroupClient>;
impl HandleEvent for TabletPadGroup {
    type Event = zwp_tablet_pad_group_v2::Event;

    fn handle_event<C: XConnection>(&mut self, event: Self::Event, state: &mut ServerState<C>) {
        simple_event_shunt! {
            self.server, event: zwp_tablet_pad_group_v2::Event => [
                Buttons { buttons },
                Ring { |ring| &TabletPadRing::from_client(ring, state).server },
                Strip { |strip| &TabletPadStrip::from_client(strip, state).server },
                Modes { modes },
                Done,
                ModeSwitch { time, serial, mode }
            ]
        }
    }
}

pub type TabletPadRing = GenericObject<TabletPadRingServer, TabletPadRingClient>;
impl HandleEvent for TabletPadRing {
    type Event = zwp_tablet_pad_ring_v2::Event;

    fn handle_event<C: XConnection>(&mut self, event: Self::Event, _: &mut ServerState<C>) {
        simple_event_shunt! {
            self.server, event: zwp_tablet_pad_ring_v2::Event => [
                Source { |source| convert_wenum(source) },
                Angle { degrees },
                Stop,
                Frame { time }
            ]
        }
    }
}

pub type TabletPadStrip = GenericObject<TabletPadStripServer, TabletPadStripClient>;
impl HandleEvent for TabletPadStrip {
    type Event = zwp_tablet_pad_strip_v2::Event;

    fn handle_event<C: XConnection>(&mut self, event: Self::Event, _: &mut ServerState<C>) {
        simple_event_shunt! {
            self.server, event: zwp_tablet_pad_strip_v2::Event => [
                Source { |source| convert_wenum(source) },
                Position { position },
                Stop,
                Frame { time }
            ]
        }
    }
}
