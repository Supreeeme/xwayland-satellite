use super::clientside::LateInitObjectKey;
use super::decoration::DecorationMarker;
use super::*;
use hecs::{CommandBuffer, World};
use log::{debug, error, trace, warn};
use macros::simple_event_shunt;
use std::os::fd::AsFd;
use wayland_client::{Proxy, protocol as client};
use wayland_protocols::{
    wp::{
        fractional_scale::v1::client::wp_fractional_scale_v1,
        pointer_constraints::zv1::server::{
            zwp_confined_pointer_v1::ZwpConfinedPointerV1 as ConfinedPointerServer,
            zwp_locked_pointer_v1::ZwpLockedPointerV1 as LockedPointerServer,
        },
        relative_pointer::zv1::{
            client::zwp_relative_pointer_v1,
            server::zwp_relative_pointer_v1::ZwpRelativePointerV1 as RelativePointerServer,
        },
        tablet::zv2::{
            client::{
                zwp_tablet_pad_group_v2, zwp_tablet_pad_ring_v2, zwp_tablet_pad_strip_v2,
                zwp_tablet_pad_v2, zwp_tablet_seat_v2, zwp_tablet_tool_v2,
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
        viewporter::client::wp_viewport::WpViewport,
    },
    xdg::{
        decoration::zv1::client::zxdg_toplevel_decoration_v1,
        shell::client::{xdg_popup, xdg_surface, xdg_toplevel},
        xdg_output::zv1::{
            client::zxdg_output_v1, server::zxdg_output_v1::ZxdgOutputV1 as XdgOutputServer,
        },
    },
};
use wayland_server::protocol::{
    wl_buffer::WlBuffer, wl_keyboard::WlKeyboard, wl_output::WlOutput, wl_pointer::WlPointer,
    wl_seat::WlSeat, wl_touch::WlTouch,
};

#[derive(Copy, Clone)]
pub(super) struct SurfaceScaleFactor(pub f64);

#[derive(hecs::Bundle)]
pub(super) struct SurfaceBundle {
    pub client: client::wl_surface::WlSurface,
    pub server: WlSurface,
    pub viewport: WpViewport,
    pub scale: SurfaceScaleFactor,
}

#[derive(Debug)]
pub(crate) enum SurfaceEvents {
    WlSurface(client::wl_surface::Event),
    XdgSurface(xdg_surface::Event),
    Toplevel(xdg_toplevel::Event),
    Popup(xdg_popup::Event),
    FractionalScale(wp_fractional_scale_v1::Event),
    DecorationEvent(zxdg_toplevel_decoration_v1::Event),
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
impl_from!(zxdg_toplevel_decoration_v1::Event, DecorationEvent);

impl Event for SurfaceEvents {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        match self {
            SurfaceEvents::WlSurface(event) => Self::surface_event(event, target, state),
            SurfaceEvents::XdgSurface(event) => Self::xdg_event(event, target, state),
            SurfaceEvents::Toplevel(event) => Self::toplevel_event(event, target, state),
            SurfaceEvents::Popup(event) => Self::popup_event(event, target, state),
            SurfaceEvents::FractionalScale(event) => match event {
                wp_fractional_scale_v1::Event::PreferredScale { scale } => {
                    let state = state.deref_mut();
                    let entity = state.world.entity(target).unwrap();
                    let factor = scale as f64 / 120.0;
                    debug!(
                        "{} scale factor: {}",
                        entity.get::<&WlSurface>().unwrap().id(),
                        factor
                    );

                    entity.get::<&mut SurfaceScaleFactor>().unwrap().0 = factor;

                    if let Some(OnOutput(output)) = entity.get::<&OnOutput>().as_deref().copied() {
                        if update_output_scale(
                            state.world.query_one(output).unwrap(),
                            OutputScaleFactor::Fractional(factor),
                        ) {
                            state.updated_outputs.push(output);
                        }
                    }
                    if entity.has::<WindowData>() {
                        update_surface_viewport(
                            &state.world,
                            state.world.query_one(target).unwrap(),
                        );
                    }
                }
                _ => unreachable!(),
            },
            SurfaceEvents::DecorationEvent(event) => {
                use zxdg_toplevel_decoration_v1::{Event, Mode};
                let Event::Configure { mode } = event else {
                    error!("unhandled toplevel decoration event: {event:?}");
                    return;
                };

                let entity = state.world.entity(target).unwrap();
                let Some(window_data) = entity.get::<&WindowData>() else {
                    return;
                };
                let Ok(mode) = mode.into_result() else {
                    warn!("unknown decoration mode: {mode:?}");
                    return;
                };

                let needs_server_side_decorations = window_data
                    .attrs
                    .decorations
                    .is_none_or(|d| d.is_serverside());

                if mode == Mode::ServerSide || !needs_server_side_decorations {
                    let mut role = entity.get::<&mut SurfaceRole>().unwrap();
                    if let SurfaceRole::Toplevel(Some(toplevel)) = &mut *role {
                        toplevel.decoration.satellite.take();
                    }
                    return;
                }

                let Some((sat_decoration, buf)) = entity
                    .get::<&client::wl_surface::WlSurface>()
                    .and_then(|surface| {
                        DecorationsDataSatellite::try_new(
                            state,
                            &surface,
                            window_data.attrs.title.as_ref().map(WmName::name),
                        )
                    })
                else {
                    warn!("Needed to create decorations for window, but couldn't create them!");
                    return;
                };

                let mut role = entity.get::<&mut SurfaceRole>().unwrap();
                // This should always be the case, but, you never know.
                if let SurfaceRole::Toplevel(Some(toplevel)) = &mut *role {
                    toplevel.decoration.satellite = Some(sat_decoration);
                } else {
                    warn!("Created a decoration for a surface that isn't a toplevel?");
                }

                drop(window_data);
                drop(role);
                if let Some(mut buf) = buf {
                    buf.run_on(&mut state.world);
                }
            }
        }
    }
}

impl SurfaceEvents {
    fn surface_event(
        event: client::wl_surface::Event,
        target: Entity,
        state: &mut ServerState<impl XConnection>,
    ) {
        use client::wl_surface::Event;
        let connection = &mut state.connection;
        let state = &mut state.inner;

        let data = state.world.entity(target).unwrap();
        let surface = data.get::<&WlSurface>().unwrap();
        let mut cmd = CommandBuffer::new();
        match event {
            Event::Enter { output } => {
                let output_entity = output.data().copied().unwrap();
                let Ok(output_data) = state.world.entity(output_entity) else {
                    return;
                };
                let Some(output) = output_data.get::<&WlOutput>() else {
                    return;
                };

                surface.enter(&output);
                let on_output = OnOutput(output_entity);

                debug!("{} entered {}", surface.id(), output.id());

                let mut query = data.query::<(&x::Window, &mut WindowData)>();
                if let Some((window, win_data)) = query.get() {
                    let Some(dimensions) = output_data.get::<&OutputDimensions>() else {
                        return;
                    };
                    win_data.update_output_offset(
                        *window,
                        WindowOutputOffset {
                            x: dimensions.x - state.global_output_offset.x.value,
                            y: dimensions.y - state.global_output_offset.y.value,
                        },
                        connection,
                    );
                    if state.last_focused_toplevel == Some(*window) {
                        let output = get_output_name(Some(&on_output), &state.world);
                        debug!("focused window changed outputs - resetting primary output");
                        connection.focus_window(*window, output);
                    }

                    if state.fractional_scale.is_none() {
                        let output_scale = output_data.get::<&OutputScaleFactor>().unwrap().get();
                        data.get::<&mut SurfaceScaleFactor>().unwrap().0 = output_scale;
                        drop(query);
                        update_surface_viewport(
                            &state.world,
                            state.world.query_one(target).unwrap(),
                        );
                    } else {
                        let scale = data.get::<&SurfaceScaleFactor>().unwrap();
                        if update_output_scale(
                            state.world.query_one(on_output.0).unwrap(),
                            OutputScaleFactor::Fractional(scale.0),
                        ) {
                            state.updated_outputs.push(on_output.0);
                        }
                    }
                }
                cmd.insert_one(target, on_output);
            }
            Event::Leave { output } => {
                let output_entity = output.data().copied().unwrap();
                let Ok(output) = state.world.get::<&WlOutput>(output_entity) else {
                    return;
                };
                surface.leave(&output);
                if data
                    .get::<&OnOutput>()
                    .is_some_and(|o| o.0 == output_entity)
                {
                    cmd.remove_one::<OnOutput>(target);
                }
            }
            Event::PreferredBufferScale { .. } => {}
            other => warn!("unhandled surface request: {other:?}"),
        }

        drop(surface);
        cmd.run_on(&mut state.world);
    }

    fn xdg_event<C: XConnection>(
        event: xdg_surface::Event,
        target: Entity,
        state: &mut ServerState<C>,
    ) {
        let state = &mut state.inner;
        let xdg_surface::Event::Configure { serial } = event else {
            unreachable!();
        };

        let data = state.world.entity(target).unwrap();
        let mut xdg = hecs::RefMut::map(data.get::<&mut SurfaceRole>().unwrap(), |r| {
            r.xdg_mut().unwrap()
        });
        xdg.surface.ack_configure(serial);
        xdg.configured = true;

        let pending = xdg.pending.take();
        drop(xdg);

        if let Some(pending) = pending {
            let mut query = data.query::<(
                &SurfaceScaleFactor,
                &x::Window,
                &mut WindowData,
                &SurfaceRole,
            )>();
            let (scale_factor, window, window_data, role) = query.get().unwrap();

            let window = *window;
            let x = (pending.x.max(0) as f64 * scale_factor.0) as i32 + window_data.output_offset.x;
            let y = (pending.y.max(0) as f64 * scale_factor.0) as i32 + window_data.output_offset.y;
            let width = if pending.width > 0 {
                (pending.width as f64 * scale_factor.0) as u16
            } else {
                window_data.attrs.dims.width
            };
            let height = if pending.height > 0 {
                let mut logical_height = pending.height;
                if let SurfaceRole::Toplevel(Some(toplevel)) = role {
                    if let Some(d) = &toplevel.decoration.satellite {
                        let surface_width = (width as f64 / scale_factor.0).ceil() as i32;
                        if d.will_draw_decorations(surface_width) {
                            logical_height = (logical_height
                                - DecorationsDataSatellite::TITLEBAR_HEIGHT)
                                .max(DecorationsDataSatellite::TITLEBAR_HEIGHT);
                        }
                    }
                }
                (logical_height as f64 * scale_factor.0) as u16
            } else {
                window_data.attrs.dims.height
            };
            debug!(
                "configuring {} ({window:?}): {x}x{y}, {width}x{height}",
                data.get::<&WlSurface>().unwrap().id(),
            );

            window_data.attrs.dims = WindowDims {
                x: x as i16,
                y: y as i16,
                width,
                height,
            };
            let pending = PendingSurfaceState {
                x,
                y,
                width: width as _,
                height: height as _,
            };
            drop(query);
            state.world.insert_one(target, pending).unwrap();
            update_surface_viewport(&state.world, state.world.query_one(target).unwrap());
        }

        let (surface, attach, callback) = state
            .world
            .query_one_mut::<(
                &client::wl_surface::WlSurface,
                Option<&SurfaceAttach>,
                Option<&WlCallback>,
            )>(target)
            .unwrap();

        let mut cmd = CommandBuffer::new();

        if let Some(SurfaceAttach { buffer, x, y }) = attach {
            surface.attach(buffer.as_ref(), *x, *y);
            cmd.remove_one::<SurfaceAttach>(target);
        }
        if let Some(cb) = callback {
            surface.frame(&state.qh, cb.clone());
            cmd.remove_one::<client::wl_callback::WlCallback>(target);
        }
        surface.commit();
        cmd.run_on(&mut state.world);
    }

    fn toplevel_event<C: XConnection>(
        event: xdg_toplevel::Event,
        target: Entity,
        state: &mut ServerState<C>,
    ) {
        let data = state.inner.world.entity(target).unwrap();
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                debug!(
                    "configuring toplevel {} {width}x{height}, {states:?}",
                    data.get::<&WlSurface>().unwrap().id()
                );

                let mut role = data.get::<&mut SurfaceRole>().unwrap();
                if let SurfaceRole::Toplevel(Some(toplevel)) = &mut *role {
                    let prev_fs = toplevel.fullscreen;
                    toplevel.fullscreen =
                        states.contains(&(u32::from(xdg_toplevel::State::Fullscreen) as u8));
                    if toplevel.fullscreen != prev_fs {
                        state.connection.set_fullscreen(
                            *data.get::<&x::Window>().unwrap(),
                            toplevel.fullscreen,
                        );
                        if let Some(decorations) = toplevel.decoration.satellite.as_mut() {
                            decorations.handle_fullscreen(toplevel.fullscreen);
                        }
                    }
                };

                role.xdg_mut().unwrap().pending = Some(PendingSurfaceState {
                    width,
                    height,
                    ..Default::default()
                });
            }
            xdg_toplevel::Event::Close => {
                let window = *data.get::<&x::Window>().unwrap();
                state.close_x_window(window);
            }
            // TODO: support capabilities (minimize, maximize, etc)
            xdg_toplevel::Event::WmCapabilities { .. } => {}
            xdg_toplevel::Event::ConfigureBounds { .. } => {}
            ref other => warn!("unhandled xdgtoplevel event: {other:?}"),
        }
    }

    fn popup_event<C: XConnection>(
        event: xdg_popup::Event,
        target: Entity,
        state: &mut ServerState<C>,
    ) {
        let data = state.inner.world.entity(target).unwrap();
        match event {
            xdg_popup::Event::Configure {
                x,
                y,
                width,
                height,
            } => {
                trace!(
                    "popup configure {}: {x}x{y}, {width}x{height}",
                    data.get::<&WlSurface>().unwrap().id()
                );

                let mut role = data.get::<&mut SurfaceRole>().unwrap();
                let xdg = role.xdg_mut().unwrap();
                let first_configure = !xdg.configured;
                xdg.pending = Some(PendingSurfaceState {
                    x,
                    y,
                    width,
                    height,
                });

                if first_configure {
                    let window_data = data.get::<&WindowData>().unwrap();
                    if window_data.attrs.require_wm_focus() {
                        let window = *data.get::<&x::Window>().unwrap();
                        state.inner.to_focus = Some(FocusData {
                            window,
                            output_name: None,
                            is_popup: true,
                            has_take_focus: window_data.attrs.has_take_focus,
                        });
                    }
                }
            }
            xdg_popup::Event::Repositioned { .. } => {}
            xdg_popup::Event::PopupDone => {
                state
                    .connection
                    .unmap_window(*data.get::<&x::Window>().unwrap());
            }
            other => todo!("{other:?}"),
        }
    }
}

pub(super) fn update_surface_viewport(
    world: &World,
    mut surface_query: hecs::QueryOne<(
        &WindowData,
        &WpViewport,
        &SurfaceScaleFactor,
        Option<&mut SurfaceRole>,
        &WlSurface,
    )>,
) {
    let (window_data, viewport, scale_factor, mut role, surface) = surface_query.get().unwrap();
    let dims = &window_data.attrs.dims;
    let size_hints = &window_data.attrs.size_hints;

    let width = (dims.width as f64 / scale_factor.0).ceil() as i32;
    let height = (dims.height as f64 / scale_factor.0).ceil() as i32;
    if width > 0 && height > 0 {
        viewport.set_destination(width, height);
    }

    let mut toplevel_data = match &mut role {
        Some(SurfaceRole::Toplevel(Some(data))) => Some(data),
        _ => None,
    };
    if let Some(d) = toplevel_data
        .as_mut()
        .and_then(|d| d.decoration.satellite.as_deref_mut())
    {
        d.draw_decorations(world, width, scale_factor.0 as f32);
    }
    debug!("{} viewport: {width}x{height}", surface.id());

    if let Some(hints) = size_hints {
        if let Some(data) = toplevel_data {
            update_size_hints(data, hints, scale_factor.0);
        };
    }
}

pub(super) fn update_size_hints(data: &ToplevelData, hints: &WmNormalHints, scale: f64) {
    let decorations_height = if data.decoration.satellite.is_some() {
        DecorationsDataSatellite::TITLEBAR_HEIGHT
    } else {
        0
    };
    if let Some(min_size) = &hints.min_size {
        data.toplevel.set_min_size(
            (min_size.width as f64 / scale) as i32,
            ((min_size.height as f64 / scale) as i32).saturating_add(decorations_height),
        );
    } else {
        data.toplevel.set_min_size(0, 0);
    }
    if let Some(max_size) = &hints.max_size {
        data.toplevel.set_max_size(
            (max_size.width as f64 / scale) as i32,
            ((max_size.height as f64 / scale) as i32).saturating_add(decorations_height),
        );
    } else {
        data.toplevel.set_max_size(0, 0);
    }
}

impl Event for client::wl_buffer::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        // The only event from a buffer would be the release.
        state.world.get::<&WlBuffer>(target).unwrap().release();
    }
}

impl Event for client::wl_seat::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        // Losing the pointer capability ends any press we observed through it. Xwayland will
        // release the pointer once it processes this event, but the candidate is dropped right
        // away instead of waiting for that request.
        if let Self::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = &self
        {
            if !capabilities.contains(client::wl_seat::Capability::Pointer) {
                invalidate_move_candidate(&mut state.world, target);
            }
        }

        let server = state.world.get::<&WlSeat>(target).unwrap();
        simple_event_shunt! {
            server, self => [
                Capabilities { |capabilities| convert_wenum(capabilities) },
                Name { name }
            ]
        }
    }
}

pub(super) struct PendingEnter(pub(super) client::wl_pointer::Event);
pub(super) enum CurrentSurface {
    Xwayland(Entity),
    Decoration(Entity),
}

impl CurrentSurface {
    fn is_decoration(&self) -> bool {
        matches!(self, Self::Decoration(..))
    }
}
pub struct LastClickSerial(pub client::wl_seat::WlSeat, pub u32);

/// A one shot record of a left pointer press we actually delivered, hosted on the entity that
/// owns the seat's currently bound pointer.
///
/// This only exists for the `_NET_WM_MOVERESIZE` button 0 + Move compatibility path.
/// Chromium ungrabs its own X11 pointer grab and then sends the request with button 0, which
/// EWMH §4.3 allows: the button field is a SHOULD. [`LastClickSerial`] cannot stand in for the
/// missing button: nothing clears it when the button is released.
///
/// A candidate means "the pointer events we have processed for this seat end with a left press
/// over `source`". It is not a mirror of the physical button state - the compositor owns that -
/// and it is consumed by the first request that uses it.
pub(super) struct MoveCandidate {
    /// The Xwayland surface entity that owned the pointer when the press was delivered, i.e. the
    /// [`CurrentSurface`], not the last hovered or keyboard focused window.
    pub source: Entity,
    /// Serial of the press.
    pub serial: u32,
    /// The pointer instance that delivered the press. A pointer that is released and recreated
    /// through `wl_seat.get_pointer` reuses this entity, so the instance is compared explicitly.
    pub pointer: client::wl_pointer::WlPointer,
}

/// Why no move candidate could be used for a window.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub(super) enum NoMoveCandidate {
    /// No live candidate belongs to this window.
    Missing,
    /// Several seats have a candidate for this window. An X11 client message has no seat field,
    /// so there is nothing to pick between them with.
    Ambiguous,
}

/// Invalidates the move candidate hosted on `seat`, if there is one.
pub(super) fn invalidate_move_candidate(world: &mut World, seat: Entity) {
    if world.remove_one::<MoveCandidate>(seat).is_ok() {
        debug!(target: "move_candidate", "invalidated move candidate on {seat:?}");
    }
}

/// Invalidates every move candidate that was created over `source`.
pub(super) fn invalidate_move_candidates_for_source(world: &mut World, source: Entity) {
    let seats: Vec<Entity> = world
        .query::<&MoveCandidate>()
        .iter()
        .filter(|(_, candidate)| candidate.source == source)
        .map(|(seat, _)| seat)
        .collect();

    for seat in seats {
        invalidate_move_candidate(world, seat);
    }
}

/// Clears pointer ownership and move candidates that still refer to `source`.
///
/// An unmapped or destroyed Xwayland surface can keep its entity alive, so entity generations do
/// not make either record stale automatically.
pub(super) fn invalidate_pointer_source(world: &mut World, source: Entity) {
    let owners: Vec<Entity> = world
        .query::<&CurrentSurface>()
        .iter()
        .filter_map(|(seat, current)| match current {
            CurrentSurface::Xwayland(entity) if *entity == source => Some(seat),
            _ => None,
        })
        .collect();

    for seat in owners {
        let _ = world.remove_one::<CurrentSurface>(seat);
    }

    invalidate_move_candidates_for_source(world, source);
}

/// Invalidates every move candidate hosted on a seat bound from `global`.
///
/// A global may be bound any number of times, so unlike `remove_output` this must not stop at the
/// first match.
pub(super) fn invalidate_move_candidates_for_seat_global(world: &mut World, global: GlobalName) {
    let seats: Vec<Entity> = world
        .query::<(&MoveCandidate, &GlobalName)>()
        .iter()
        .filter(|(_, (_, name))| **name == global)
        .map(|(seat, _)| seat)
        .collect();

    for seat in seats {
        invalidate_move_candidate(world, seat);
    }
}

/// Records a real left press over `source` as this seat's move candidate, replacing any previous
/// one.
///
/// The seat's global has to still be advertised: `handle_globals` runs before the pointer events
/// that were queued in the same batch, so a removal that already invalidated the candidates must
/// not be undone by one of those older presses.
///
/// The pointer stored below is the currently bound pointer. It is also the event's source because
/// `clientside::Dispatch<WlPointer, Entity>` drops events from any older pointer instance before
/// they reach this handler, and `ServerState::run` drains that queue without dispatching new client
/// requests between the two steps. Keep that ordering, or carry the source pointer on the queued
/// event instead.
pub(super) fn create_move_candidate<V>(
    world: &mut World,
    globals_map: &HashMap<GlobalName, V>,
    seat: Entity,
    source: Entity,
    serial: u32,
) {
    invalidate_move_candidate(world, seat);

    let Ok(global) = world.get::<&GlobalName>(seat).map(|global| *global) else {
        return;
    };
    if !globals_map.contains_key(&global) {
        debug!(
            target: "move_candidate",
            "not creating move candidate: seat global {global:?} is gone"
        );
        return;
    }
    let Ok(pointer) = world
        .get::<&client::wl_pointer::WlPointer>(seat)
        .map(|pointer| (*pointer).clone())
    else {
        return;
    };

    world
        .insert_one(
            seat,
            MoveCandidate {
                source,
                serial,
                pointer,
            },
        )
        .unwrap();
    debug!(
        target: "move_candidate",
        "new move candidate on {seat:?} for {source:?} (serial {serial})"
    );
}

/// Finds the single seat holding a usable move candidate for `source`.
pub(super) fn find_move_candidate<V>(
    world: &World,
    globals_map: &HashMap<GlobalName, V>,
    source: Entity,
) -> Result<Entity, NoMoveCandidate> {
    let mut found = None;

    for (seat, (candidate, wl_seat, pointer, global)) in world
        .query::<(
            &MoveCandidate,
            &client::wl_seat::WlSeat,
            &client::wl_pointer::WlPointer,
            &GlobalName,
        )>()
        .iter()
    {
        // Lifecycle handlers invalidate these states eagerly. Re-check them here as
        // defense-in-depth at the authorization boundary.
        if candidate.source != source
            || !globals_map.contains_key(global)
            || !wl_seat.is_alive()
            || !pointer.is_alive()
            // The press has to have come from the pointer that is still bound to this seat.
            || candidate.pointer != *pointer
        {
            continue;
        }

        if found.is_some() {
            return Err(NoMoveCandidate::Ambiguous);
        }
        found = Some(seat);
    }

    found.ok_or(NoMoveCandidate::Missing)
}

impl Event for client::wl_pointer::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        // Workaround GTK (stupidly) autoclosing popups if it receives an wl_pointer.enter
        // event shortly after creation.
        // When Niri creates a popup, it immediately sends wl_pointer.enter on the new surface,
        // generating an EnterNotify event, and Xwayland will send a release button event.
        // In its menu implementation, GTK treats EnterNotify "this menu is now active" and will
        // destroy the menu if this occurs within a 500 ms interval (which it always does with
        // Niri). Other compositors do not run into this problem because they appear to not send
        // wl_pointer.enter until the user actually moves the mouse in the popup.

        fn handle_pending_enter<C: XConnection>(
            target: Entity,
            state: &mut ServerState<C>,
            event_str: &str,
        ) -> bool {
            loop {
                let Ok(pe) = state.world.get::<&PendingEnter>(target) else {
                    return true;
                };
                let PendingEnter(client::wl_pointer::Event::Enter {
                    serial,
                    surface,
                    surface_x,
                    surface_y,
                }) = pe.deref()
                else {
                    unreachable!();
                };
                if surface
                    .data()
                    .copied()
                    .is_some_and(|key| state.world.contains(key))
                {
                    trace!("resending enter ({serial}) before {}", event_str);
                    let enter_event = client::wl_pointer::Event::Enter {
                        serial: *serial,
                        surface: surface.clone(),
                        surface_x: *surface_x,
                        surface_y: *surface_y,
                    };

                    drop(pe);
                    Event::handle(enter_event, target, state);
                } else {
                    warn!("could not move pointer to surface: stale surface");
                    drop(pe);
                    // The queued enter cannot be resolved, so we don't know what the pointer is
                    // over anymore. Forget the previous owner instead of letting the next press
                    // borrow it, and drop the candidate it may have created.
                    let _ = state.world.remove_one::<PendingEnter>(target);
                    let _ = state.world.remove_one::<CurrentSurface>(target);
                    invalidate_move_candidate(&mut state.world, target);
                    return false;
                }
            }
        }

        match self {
            Self::Enter {
                serial,
                ref surface,
                surface_x,
                surface_y,
            } => {
                // Entering a surface never authorizes a move by itself, and it ends the press
                // ownership of whatever we were over before.
                invalidate_move_candidate(&mut state.world, target);
                let connection = &mut state.connection;
                let state = &mut state.inner;
                let mut cmd = CommandBuffer::new();
                let pending_enter = state.world.remove_one::<PendingEnter>(target).ok();
                let surface_entity = surface.data().copied();
                let mut query = surface_entity.and_then(|e| {
                    state
                        .world
                        .query_one::<(&WlSurface, &SurfaceRole, &SurfaceScaleFactor, &x::Window)>(e)
                        .ok()
                });
                let Some((surface, role, scale, window)) = query.as_mut().and_then(|q| q.get())
                else {
                    if let Some(&DecorationMarker { parent }) = surface.data() {
                        drop(query);
                        state
                            .world
                            .insert_one(target, CurrentSurface::Decoration(parent))
                            .unwrap();
                    } else {
                        warn!("could not enter surface {}: stale surface", surface.id());
                        drop(query);
                        // We can't tell what the pointer is over now, so the old owner must not
                        // survive into the next press.
                        let _ = state.world.remove_one::<CurrentSurface>(target);
                    }

                    return;
                };

                let server = state.world.get::<&WlPointer>(target).unwrap();
                cmd.insert(target, (*scale,));

                let surface_is_popup = matches!(role, SurfaceRole::Popup(_));
                let mut do_enter = || {
                    debug!("pointer entering {} ({serial} {})", surface.id(), scale.0);
                    server.enter(serial, surface, surface_x * scale.0, surface_y * scale.0);
                    connection.raise_to_top(*window);
                    if !surface_is_popup {
                        state.last_hovered = Some(*window);
                    }
                    cmd.insert_one(target, CurrentSurface::Xwayland(surface_entity.unwrap()));
                };

                if !surface_is_popup {
                    do_enter();
                } else {
                    match pending_enter {
                        Some(e) => {
                            let PendingEnter(client::wl_pointer::Event::Enter {
                                serial: pending_serial,
                                ..
                            }) = e
                            else {
                                unreachable!();
                            };
                            if serial == pending_serial {
                                do_enter();
                            } else {
                                cmd.insert(target, (PendingEnter(self),));
                            }
                        }
                        None => {
                            cmd.insert(target, (PendingEnter(self),));
                        }
                    }
                }
                drop(query);
                drop(server);
                cmd.run_on(&mut state.world);
            }
            Self::Leave { serial, surface } => {
                // Invalidate before the early return below: a dead surface is still a leave, and
                // the candidate must not outlive the pointer being over its source.
                invalidate_move_candidate(&mut state.world, target);
                let _ = state.world.remove_one::<PendingEnter>(target);
                if !surface.is_alive() {
                    // Same reasoning as the stale enter path: an unknown owner is dropped rather
                    // than handed to the next press.
                    let _ = state.world.remove_one::<CurrentSurface>(target);
                    return;
                }
                debug!("leaving surface ({})", surface.id());
                if let Ok(CurrentSurface::Decoration(parent)) =
                    state.world.remove_one::<CurrentSurface>(target)
                {
                    decoration::handle_pointer_leave(state, parent);
                    return;
                }

                if let Some(surface) = surface
                    .data()
                    .copied()
                    .and_then(|key| state.world.get::<&WlSurface>(key).ok())
                {
                    state
                        .world
                        .get::<&WlPointer>(target)
                        .unwrap()
                        .leave(serial, &surface);
                } else {
                    warn!("could not leave surface: stale surface");
                }
            }
            Self::Motion {
                time,
                surface_x,
                surface_y,
            } => {
                let _ = handle_pending_enter(target, state, "motion");
                {
                    match state.world.get::<&CurrentSurface>(target) {
                        Ok(surface) => {
                            if let CurrentSurface::Decoration(parent) = &*surface {
                                decoration::handle_pointer_motion(
                                    state, *parent, surface_x, surface_y,
                                );
                                return;
                            }
                        }
                        Err(_) => {
                            // The compositor still owns pointer focus. Forward the event to
                            // Xwayland, but do not infer a window owner from stale local state.
                            debug!("forwarding motion with unknown pointer owner");
                        }
                    }
                }
                let Ok((server, scale)) = state
                    .world
                    .query_one_mut::<(&WlPointer, Option<&SurfaceScaleFactor>)>(target)
                else {
                    warn!("could not forward motion: stale pointer");
                    return;
                };
                // Unknown ownership can occur before the first resolvable enter, so there may be
                // no surface-derived scale yet. Keep ordinary input flowing at the protocol's
                // unit scale without inventing an owner or a move authorization candidate.
                let scale = scale.copied().unwrap_or(SurfaceScaleFactor(1.0));
                trace!(
                    target: "pointer_position",
                    "pointer motion {} {}",
                    surface_x * scale.0,
                    surface_y * scale.0
                );
                server.motion(time, surface_x * scale.0, surface_y * scale.0);
            }
            Self::Button {
                serial,
                time,
                button,
                state: button_state,
            } => {
                // from linux/input-event-codes.h
                mod button_codes {
                    pub const LEFT: u32 = 0x110;
                }

                let pressed =
                    button_state == WEnum::Value(client::wl_pointer::ButtonState::Pressed);

                // A left release ends the candidate even if other buttons are still held, and it
                // has to happen before the early returns below - a candidate must never outlive
                // the press that created it.
                if button == button_codes::LEFT && !pressed {
                    invalidate_move_candidate(&mut state.world, target);
                }

                let _ = handle_pending_enter(target, state, "click");
                let mut cmd = CommandBuffer::new();

                let Ok(mut query) = state.world.query_one::<(
                    &WlPointer,
                    &client::wl_seat::WlSeat,
                    Option<&CurrentSurface>,
                )>(target) else {
                    warn!("could not click on surface: stale surface");
                    return;
                };

                // The pointer may already be gone (released client side) while its events are
                // still being drained, and we may not know what it is over.
                let Some((server, seat, current_surface)) = query.get() else {
                    warn!("could not click on surface: no pointer");
                    return;
                };

                // Set when this press should become the seat's move candidate.
                let mut press_owner = None;

                match current_surface {
                    Some(CurrentSurface::Xwayland(entity)) => {
                        if pressed && button == button_codes::LEFT {
                            cmd.insert(*entity, (LastClickSerial(seat.clone(), serial),));
                            press_owner = Some(*entity);
                        }
                    }
                    Some(CurrentSurface::Decoration(parent)) => {
                        if pressed && button == button_codes::LEFT {
                            let seat = seat.clone();
                            let parent = *parent;
                            drop(query);
                            decoration::handle_pointer_click(state, parent, &seat, serial);
                            return;
                        }
                    }
                    // We invalidated the owner (stale enter, dead leave, unresolvable pending
                    // enter), so this event must not create a click serial or move candidate.
                    // The compositor still owns pointer focus, so preserve ordinary input
                    // delivery and let Xwayland route the event.
                    None => {
                        warn!("forwarding button {button} with unknown pointer owner");
                    }
                }

                server.button(serial, time, button, convert_wenum(button_state));
                drop(query);
                cmd.run_on(&mut state.world);

                if let Some(source) = press_owner {
                    let state = state.deref_mut();
                    create_move_candidate(
                        &mut state.world,
                        &state.globals_map,
                        target,
                        source,
                        serial,
                    );
                }
            }
            _ => {
                let (server, current_surface) = state
                    .world
                    .query_one_mut::<(&WlPointer, Option<&CurrentSurface>)>(target)
                    .unwrap();

                if current_surface.is_some_and(CurrentSurface::is_decoration) {
                    return;
                }
                simple_event_shunt! {
                    server, self => [
                        Frame,
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
                }
            }
        }
    }
}

impl Event for client::wl_keyboard::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let state = state.deref_mut();
        let data = state.world.entity(target).unwrap();
        let keyboard = data.get::<&WlKeyboard>().unwrap();
        match self {
            client::wl_keyboard::Event::Enter {
                serial,
                surface,
                keys,
            } => {
                let mut query = surface.data().copied().and_then(|key| {
                    state
                        .world
                        .query_one::<(&x::Window, &WlSurface, Option<&OnOutput>)>(key)
                        .ok()
                });
                let Some((window, surface, output)) = query.as_mut().and_then(|q| q.get()) else {
                    return;
                };
                state.last_kb_serial = Some((
                    data.get::<&client::wl_seat::WlSeat>()
                        .as_deref()
                        .unwrap()
                        .clone(),
                    serial,
                ));
                let output_name = get_output_name(output, &state.world);
                let window_data = data.get::<&WindowData>();
                let has_take_focus = window_data.as_ref().is_some_and(|d| d.attrs.has_take_focus);
                state.to_focus = Some(FocusData {
                    window: *window,
                    output_name,
                    is_popup: false,
                    has_take_focus,
                });
                keyboard.enter(serial, surface, keys);
            }
            client::wl_keyboard::Event::Leave { serial, surface } => {
                if !surface.is_alive() {
                    return;
                }
                let mut query = surface
                    .data()
                    .copied()
                    .and_then(|key| state.world.query_one::<(&x::Window, &WlSurface)>(key).ok());
                let Some((window, surface)) = query.as_mut().and_then(|q| q.get()) else {
                    return;
                };
                if state.to_focus.as_ref().map(|d| d.window) == Some(*window) {
                    state.to_focus.take();
                } else {
                    state.unfocus = true;
                }
                keyboard.leave(serial, surface);
            }
            client::wl_keyboard::Event::Key {
                serial,
                time,
                key,
                state: key_state,
            } => {
                state.last_kb_serial = Some((
                    data.get::<&client::wl_seat::WlSeat>()
                        .as_deref()
                        .unwrap()
                        .clone(),
                    serial,
                ));
                keyboard.key(serial, time, key, convert_wenum(key_state));
            }
            _ => simple_event_shunt! {
                keyboard, self => [
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

impl Event for client::wl_touch::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        match self {
            Self::Down {
                serial,
                time,
                surface,
                id,
                x,
                y,
            } => {
                let mut cmd = CommandBuffer::new();
                {
                    let connection = &mut state.connection;
                    let world = &mut state.inner.world;
                    let s_entity = surface.data().copied();
                    let mut s_query = s_entity.and_then(|key| {
                        world
                            .query_one::<(&WlSurface, &SurfaceScaleFactor, &x::Window)>(key)
                            .ok()
                    });
                    if let Some((s_surface, s_factor, window)) =
                        s_query.as_mut().and_then(|q| q.get())
                    {
                        cmd.insert_one(target, *s_factor);
                        connection.raise_to_top(*window);
                        let touch = world.get::<&WlTouch>(target).unwrap();
                        touch.down(serial, time, s_surface, id, x * s_factor.0, y * s_factor.0);
                    } else if let Some(&DecorationMarker { parent }) = surface.data() {
                        drop(s_query);
                        let seat = {
                            let seat =
                                &*state.world.get::<&client::wl_seat::WlSeat>(target).unwrap();
                            seat.clone()
                        };
                        decoration::handle_pointer_motion(state, parent, x, y);
                        decoration::handle_pointer_click(state, parent, &seat, serial);
                    }
                }
                cmd.run_on(&mut state.world);
            }
            Self::Motion { time, id, x, y } => {
                let Ok((touch, scale)) = state
                    .world
                    .query_one_mut::<(&WlTouch, &SurfaceScaleFactor)>(target)
                else {
                    return;
                };
                touch.motion(time, id, x * scale.0, y * scale.0);
            }
            _ => {
                let touch = state.world.get::<&WlTouch>(target).unwrap();
                simple_event_shunt! {
                    touch, self => [
                        Up { serial, time, id },
                        Frame,
                        Cancel,
                        Shape { id, major, minor },
                        Orientation { id, orientation }
                    ]
                }
            }
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub(super) struct OnOutput(pub Entity);
struct OutputName(String);
fn get_output_name(output: Option<&OnOutput>, world: &World) -> Option<String> {
    output.map(|o| world.get::<&OutputName>(o.0).unwrap().0.clone())
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub(super) enum OutputScaleFactor {
    Output(i32),
    Fractional(f64),
}

impl OutputScaleFactor {
    pub(super) fn get(&self) -> f64 {
        match *self {
            Self::Output(o) => o as _,
            Self::Fractional(f) => f,
        }
    }
}

#[must_use]
fn update_output_scale(
    mut output_scale: hecs::QueryOne<&mut OutputScaleFactor>,
    factor: OutputScaleFactor,
) -> bool {
    let Some(output_scale) = output_scale.get() else {
        return false;
    };

    if matches!(output_scale, OutputScaleFactor::Fractional(..))
        && matches!(factor, OutputScaleFactor::Output(..))
    {
        return false;
    }

    if *output_scale != factor {
        *output_scale = factor;
        return true;
    }

    false
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
    rotated_90: bool,
}

impl Default for OutputDimensions {
    fn default() -> Self {
        Self {
            source: OutputDimensionsSource::Wl {
                physical_height: 0,
                physical_width: 0,
                subpixel: WEnum::Value(client::wl_output::Subpixel::Unknown),
                make: "<unknown>".to_string(),
                model: "<unknown>".to_string(),
                transform: WEnum::Value(client::wl_output::Transform::Normal),
            },
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            rotated_90: false,
        }
    }
}

fn update_output_offset(
    output: Entity,
    source: OutputDimensionsSource,
    x: i32,
    y: i32,
    state: &mut ServerState<impl XConnection>,
) {
    let connection = &mut state.connection;
    let state = &mut state.inner;
    {
        let Ok(mut dimensions) = state.world.get::<&mut OutputDimensions>(output) else {
            return;
        };
        if matches!(source, OutputDimensionsSource::Wl { .. })
            && matches!(dimensions.source, OutputDimensionsSource::Xdg)
        {
            return;
        }

        let global_offset = &mut state.global_output_offset;
        let mut maybe_update_dimension = |value, dim: &mut GlobalOutputOffsetDimension| {
            if value < dim.value {
                *dim = GlobalOutputOffsetDimension {
                    owner: Some(output),
                    value,
                };
                state.global_offset_updated = true;
            } else if dim.owner == Some(output) && value > dim.value {
                // Another output's position could be less than the new value, so recalculate
                dim.owner = None;
                state.global_offset_updated = true;
            }
        };

        maybe_update_dimension(x, &mut global_offset.x);
        maybe_update_dimension(y, &mut global_offset.y);

        dimensions.source = source;
        dimensions.x = x;
        dimensions.y = y;
        debug!(
            "moving {} to {x}x{y}",
            state.world.get::<&WlOutput>(output).unwrap().id()
        );
    }

    update_window_output_offsets(
        output,
        &state.global_output_offset,
        &state.world,
        connection,
    );
}

fn update_window_output_offsets(
    output: Entity,
    global_output_offset: &GlobalOutputOffset,
    world: &World,
    connection: &mut impl XConnection,
) {
    let Ok(dimensions) = world.get::<&OutputDimensions>(output) else {
        return;
    };
    let mut query = world.query::<(&x::Window, &mut WindowData, &OnOutput)>();

    for (_, (window, data, _)) in query
        .into_iter()
        .filter(|(_, (_, _, on_output))| on_output.0 == output)
    {
        data.update_output_offset(
            *window,
            WindowOutputOffset {
                x: dimensions.x - global_output_offset.x.value,
                y: dimensions.y - global_output_offset.y.value,
            },
            connection,
        );
    }
}

pub(super) fn update_global_output_offset(
    output: Entity,
    global_output_offset: &GlobalOutputOffset,
    world: &World,
    connection: &mut impl XConnection,
) {
    let entity = world.entity(output).unwrap();
    let mut query = entity.query::<(&OutputDimensions, &WlOutput)>();
    let Some((dimensions, server)) = query.get() else {
        return;
    };

    let x = dimensions.x - global_output_offset.x.value;
    let y = dimensions.y - global_output_offset.y.value;

    match &dimensions.source {
        OutputDimensionsSource::Wl {
            physical_width,
            physical_height,
            subpixel,
            make,
            model,
            transform,
        } => {
            server.geometry(
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
            entity
                .get::<&XdgOutputServer>()
                .unwrap()
                .logical_position(x, y);
        }
    }

    server.done();
    drop(query);

    update_window_output_offsets(output, global_output_offset, world, connection);
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

impl Event for OutputEvent {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        match self {
            OutputEvent::Xdg(event) => Self::xdg_event(event, target, state),
            OutputEvent::Wl(event) => Self::wl_event(event, target, state),
        }
    }
}

impl OutputEvent {
    fn wl_event<C: XConnection>(
        event: client::wl_output::Event,
        target: Entity,
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
                update_output_offset(
                    target,
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
                let global_output_offset = state.global_output_offset;

                let Ok((output, dimensions, xdg)) = state.world.query_one_mut::<(
                    &WlOutput,
                    &mut OutputDimensions,
                    Option<&XdgOutputServer>,
                )>(target) else {
                    return;
                };

                output.geometry(
                    x - global_output_offset.x.value,
                    y - global_output_offset.y.value,
                    physical_width,
                    physical_height,
                    convert_wenum(subpixel),
                    make,
                    model,
                    convert_wenum(transform),
                );
                dimensions.rotated_90 = transform.into_result().is_ok_and(|t| {
                    matches!(
                        t,
                        client::wl_output::Transform::_90
                            | client::wl_output::Transform::_270
                            | client::wl_output::Transform::Flipped90
                            | client::wl_output::Transform::Flipped270
                    )
                });
                if let Some(xdg) = xdg {
                    if dimensions.rotated_90 {
                        xdg.logical_size(dimensions.height, dimensions.width);
                    } else {
                        xdg.logical_size(dimensions.width, dimensions.height);
                    }
                }
            }
            Event::Mode {
                flags,
                width,
                height,
                refresh,
            } => {
                let Ok((output, dimensions)) = state
                    .world
                    .query_one_mut::<(&WlOutput, &mut OutputDimensions)>(target)
                else {
                    return;
                };

                if flags
                    .into_result()
                    .is_ok_and(|f| f.contains(client::wl_output::Mode::Current))
                {
                    dimensions.width = width;
                    dimensions.height = height;
                    debug!("{} dimensions: {width}x{height}", output.id());
                }
                output.mode(convert_wenum(flags), width, height, refresh);
            }
            Event::Scale { factor } => {
                debug!(
                    "{} scale: {factor}",
                    state.world.get::<&WlOutput>(target).unwrap().id()
                );
                if update_output_scale(
                    state.world.query_one(target).unwrap(),
                    OutputScaleFactor::Output(factor),
                ) {
                    state.updated_outputs.push(target);
                }
                if state.fractional_scale.is_none() {
                    state.world.get::<&WlOutput>(target).unwrap().scale(factor);
                }
            }
            Event::Name { name } => {
                state
                    .world
                    .get::<&WlOutput>(target)
                    .unwrap()
                    .name(name.clone());
                state.world.insert(target, (OutputName(name),)).unwrap();
            }
            _ => simple_event_shunt! {
                state.world.get::<&WlOutput>(target).unwrap(),
                event: client::wl_output::Event => [
                    Description { description },
                    Done
                ]
            },
        }
    }

    fn xdg_event<C: XConnection>(
        event: zxdg_output_v1::Event,
        target: Entity,
        state: &mut ServerState<C>,
    ) {
        use zxdg_output_v1::Event;

        match event {
            Event::LogicalPosition { x, y } => {
                update_output_offset(target, OutputDimensionsSource::Xdg, x, y, state);
                if !state.global_offset_updated {
                    state
                        .world
                        .get::<&XdgOutputServer>(target)
                        .unwrap()
                        .logical_position(
                            x - state.global_output_offset.x.value,
                            y - state.global_output_offset.y.value,
                        );
                }
            }
            Event::LogicalSize { .. } => {
                let Ok((xdg, dimensions)) = state
                    .world
                    .query_one_mut::<(&XdgOutputServer, &OutputDimensions)>(target)
                else {
                    return;
                };
                if dimensions.rotated_90 {
                    xdg.logical_size(dimensions.height, dimensions.width);
                } else {
                    xdg.logical_size(dimensions.width, dimensions.height);
                }
            }
            _ => simple_event_shunt! {
                state.world.get::<&XdgOutputServer>(target).unwrap(),
                event: zxdg_output_v1::Event => [
                    Done,
                    Name { name },
                    Description { description }
                ]
            },
        }
    }
}

impl Event for wl_drm::client::wl_drm::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let server = state.world.get::<&WlDrmServer>(target).unwrap();
        simple_event_shunt! {
            server, self => [
                Device { name },
                Format { format },
                Authenticated,
                Capabilities { value }
            ]
        }
    }
}

impl Event for c_dmabuf::zwp_linux_dmabuf_feedback_v1::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let server = state
            .world
            .get::<&s_dmabuf::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1>(target)
            .unwrap();
        simple_event_shunt! {
            server, self => [
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

impl Event for zwp_relative_pointer_v1::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let server = state.world.get::<&RelativePointerServer>(target).unwrap();
        simple_event_shunt! {
            server, self => [
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

impl Event for zwp_locked_pointer_v1::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let server = state.world.get::<&LockedPointerServer>(target).unwrap();
        simple_event_shunt! {
            server, self => [
                Locked,
                Unlocked
            ]
        }
    }
}

impl Event for zwp_confined_pointer_v1::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let server = state.world.get::<&ConfinedPointerServer>(target).unwrap();
        simple_event_shunt! {
            server, self => [
                Confined,
                Unconfined
            ]
        }
    }
}

impl Event for zwp_tablet_seat_v2::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let seat = state.world.get::<&TabletSeatServer>(target).unwrap();
        match self {
            Self::TabletAdded { id } => {
                let (e, tab) = from_client::<TabletServer, _, _>(&id, state);
                seat.tablet_added(&tab);
                drop(seat);
                state.world.spawn_at(e, (tab, id));
            }
            Self::ToolAdded { id } => {
                let (e, tool) = from_client::<TabletToolServer, _, _>(&id, state);
                seat.tool_added(&tool);
                drop(seat);
                state.world.spawn_at(e, (tool, id));
            }
            Self::PadAdded { id } => {
                let (e, pad) = from_client::<TabletPadServer, _, _>(&id, state);
                seat.pad_added(&pad);
                drop(seat);
                state.world.spawn_at(e, (pad, id));
            }
            _ => log::warn!("unhandled {}: {self:?}", std::any::type_name::<Self>()),
        }
    }
}

impl Event for zwp_tablet_v2::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let tab = state.world.get::<&TabletServer>(target).unwrap();
        simple_event_shunt! {
            tab, self => [
                Name { name },
                Id { vid, pid },
                Path { path },
                Done,
                Removed
            ]
        }
    }
}

impl Event for zwp_tablet_pad_v2::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let pad = state.world.get::<&TabletPadServer>(target).unwrap();
        let s_surf;
        match self {
            Self::Group { pad_group } => {
                let (e, s_group) = from_client::<TabletPadGroupServer, _, _>(&pad_group, state);
                pad.group(&s_group);
                drop(pad);
                state.world.spawn_at(e, (pad_group, s_group));
            }
            Self::Enter {
                serial,
                tablet,
                surface,
            } => {
                let Some(surface) = surface
                    .data()
                    .copied()
                    .and_then(|key| state.world.get::<&WlSurface>(key).ok())
                else {
                    return;
                };
                let Some(s_tablet) =
                    tablet
                        .data()
                        .and_then(|key: &LateInitObjectKey<TabletClient>| {
                            state.world.get::<&TabletServer>(key.get()).ok()
                        })
                else {
                    return;
                };
                pad.enter(serial, &s_tablet, &surface);
            }
            _ => simple_event_shunt! {
                pad, self => [
                    Path { path },
                    Buttons { buttons },
                    Done,
                    Button {
                        time,
                        button,
                        |state| convert_wenum(state)
                    },
                    Leave {
                        serial,
                        |surface| {
                            s_surf = surface.data().copied().and_then(|key| state.world.get::<&WlSurface>(key).ok());
                            if let Some(s) = &s_surf { s } else { return; }
                        }
                    },
                    Removed
                ]
            },
        }
    }
}

impl Event for zwp_tablet_tool_v2::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        match self {
            Self::ProximityIn {
                serial,
                tablet,
                surface,
            } => {
                let mut cmd = CommandBuffer::new();
                {
                    let connection = &mut state.connection;
                    let world = &mut state.inner.world;

                    let Some(mut query) = surface.data().copied().and_then(|key| {
                        world
                            .query_one::<(&WlSurface, &SurfaceScaleFactor, &x::Window)>(key)
                            .ok()
                    }) else {
                        warn!("tablet tool proximity_in failed: stale surface");
                        return;
                    };
                    let (surface, scale, window) = query.get().unwrap();
                    cmd.insert(target, (*scale,));

                    let Some(s_tablet) =
                        tablet
                            .data()
                            .and_then(|key: &LateInitObjectKey<TabletClient>| {
                                world.get::<&TabletServer>(key.get()).ok()
                            })
                    else {
                        warn!("tablet tool proximity_in failed: stale tablet");
                        return;
                    };

                    world
                        .get::<&TabletToolServer>(target)
                        .unwrap()
                        .proximity_in(serial, &s_tablet, surface);

                    connection.raise_to_top(*window);
                }
                cmd.run_on(&mut state.world);
            }
            Self::Motion { x, y } => {
                let (tool, scale) = state
                    .world
                    .query_one_mut::<(&TabletToolServer, Option<&SurfaceScaleFactor>)>(target)
                    .unwrap();
                let scale = scale.map(|s| s.0).unwrap_or(1.0);
                tool.motion(x * scale, y * scale);
            }
            _ => {
                let tool = state.world.get::<&TabletToolServer>(target).unwrap();
                simple_event_shunt! {
                    tool, self => [
                        Type { |tool_type| convert_wenum(tool_type) },
                        HardwareSerial { hardware_serial_hi, hardware_serial_lo },
                        HardwareIdWacom { hardware_id_hi, hardware_id_lo },
                        Capability { |capability| convert_wenum(capability) },
                        Done,
                        Removed,
                        ProximityOut,
                        Down { serial },
                        Up,
                        Distance { distance },
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
    }
}

#[must_use]
fn from_client<
    Server: Resource + 'static,
    Client: Proxy + Send + Sync + 'static,
    S: X11Selection + 'static,
>(
    client: &Client,
    state: &InnerServerState<S>,
) -> (Entity, Server)
where
    Client::Event: Send + Into<ObjectEvent>,
    InnerServerState<S>: wayland_server::Dispatch<Server, Entity>,
{
    let entity = state.world.reserve_entity();
    let server = state
        .client
        .create_resource::<_, _, InnerServerState<S>>(&state.dh, 1, entity)
        .unwrap();
    let obj_key: &LateInitObjectKey<Client> = client.data().unwrap();
    obj_key.init(entity);
    (entity, server)
}

impl Event for zwp_tablet_pad_group_v2::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let group = state.world.get::<&TabletPadGroupServer>(target).unwrap();
        match self {
            Self::Buttons { buttons } => group.buttons(buttons),
            Self::Ring { ring } => {
                let (e, s_ring) = from_client::<TabletPadRingServer, _, _>(&ring, state);
                group.ring(&s_ring);
                drop(group);
                state.world.spawn_at(e, (s_ring, ring));
            }
            Self::Strip { strip } => {
                let (e, s_strip) = from_client::<TabletPadStripServer, _, _>(&strip, state);
                group.strip(&s_strip);
                drop(group);
                state.world.spawn_at(e, (s_strip, strip));
            }
            Self::Modes { modes } => group.modes(modes),
            Self::ModeSwitch { time, serial, mode } => group.mode_switch(time, serial, mode),
            Self::Done => group.done(),
            _ => log::warn!(
                "unhandled {} event: {self:?}",
                std::any::type_name::<Self>()
            ),
        }
    }
}

impl Event for zwp_tablet_pad_ring_v2::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let ring = state.world.get::<&TabletPadRingServer>(target).unwrap();
        simple_event_shunt! {
            ring, self => [
                Source { |source| convert_wenum(source) },
                Angle { degrees },
                Stop,
                Frame { time }
            ]
        }
    }
}

impl Event for zwp_tablet_pad_strip_v2::Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
        let strip = state.world.get::<&TabletPadStripServer>(target).unwrap();
        simple_event_shunt! {
            strip, self => [
                Source { |source| convert_wenum(source) },
                Position { position },
                Stop,
                Frame { time }
            ]
        }
    }
}

/// Tests for the move candidate bookkeeping used by Chromium/Electron clients on the
/// `_NET_WM_MOVERESIZE` button 0 path.
///
/// These drive the production helpers directly on a `hecs::World` with real (client side) seat and
/// pointer proxies, which is the only part of a proxy that matters here: its identity. The
/// end to end behavior is covered by the `move_button0_*` integration tests.
#[cfg(test)]
mod move_candidate_tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use wayland_client::{
        Connection as ClientConnection, EventQueue, delegate_noop,
        protocol::wl_registry::WlRegistry,
    };

    type ClientSeat = client::wl_seat::WlSeat;
    type ClientPointer = client::wl_pointer::WlPointer;

    /// Dispatch target for the proxies below. Nothing is ever dispatched: the peer end of the
    /// socket is never served, so no events can arrive.
    struct NoEvents;
    delegate_noop!(NoEvents: ignore WlRegistry);
    delegate_noop!(NoEvents: ignore ClientSeat);
    delegate_noop!(NoEvents: ignore ClientPointer);

    /// Hands out distinct, live seat and pointer proxies.
    struct Proxies {
        /// Keeps the peer end of the socket open so the proxies stay alive.
        _peer: UnixStream,
        connection: ClientConnection,
        queue: EventQueue<NoEvents>,
    }

    impl Proxies {
        fn new() -> Self {
            let (ours, peer) = UnixStream::pair().unwrap();
            let connection = ClientConnection::from_socket(ours).unwrap();
            let queue = connection.new_event_queue::<NoEvents>();
            Self {
                _peer: peer,
                connection,
                queue,
            }
        }

        fn seat(&self) -> ClientSeat {
            let qh = self.queue.handle();
            let registry = self.connection.display().get_registry(&qh, ());
            registry.bind::<ClientSeat, _, _>(1, 5, &qh, ())
        }

        fn pointer(&self, seat: &ClientSeat) -> ClientPointer {
            seat.get_pointer(&self.queue.handle(), ())
        }
    }

    /// A world with one bound seat, as satellite lays it out: the seat entity also hosts the
    /// pointer and the name of the global it was bound from.
    struct SeatWorld {
        world: World,
        globals: HashMap<GlobalName, ()>,
        proxies: Proxies,
    }

    impl SeatWorld {
        fn new() -> Self {
            Self {
                world: World::new(),
                globals: HashMap::new(),
                proxies: Proxies::new(),
            }
        }

        /// Spawns a seat bound from `global`, advertising that global.
        fn add_seat(&mut self, global: u32) -> Entity {
            let seat = self.proxies.seat();
            let pointer = self.proxies.pointer(&seat);
            self.globals.insert(GlobalName(global), ());
            self.world.spawn((seat, pointer, GlobalName(global)))
        }

        /// Replaces the seat's pointer, as `wl_seat.get_pointer` does for an existing entity.
        fn rebind_pointer(&mut self, seat_entity: Entity) {
            let seat = (*self.world.get::<&ClientSeat>(seat_entity).unwrap()).clone();
            let pointer = self.proxies.pointer(&seat);
            self.world.insert_one(seat_entity, pointer).unwrap();
        }

        fn candidate(&self, seat_entity: Entity) -> Option<(Entity, u32)> {
            self.world
                .get::<&MoveCandidate>(seat_entity)
                .ok()
                .map(|candidate| (candidate.source, candidate.serial))
        }

        fn press(&mut self, seat_entity: Entity, source: Entity, serial: u32) {
            create_move_candidate(&mut self.world, &self.globals, seat_entity, source, serial);
        }

        fn find(&self, source: Entity) -> Result<Entity, NoMoveCandidate> {
            find_move_candidate(&self.world, &self.globals, source)
        }
    }

    #[test]
    fn press_creates_a_findable_candidate() {
        let mut w = SeatWorld::new();
        let seat = w.add_seat(1);
        let window = w.world.spawn(());

        w.press(seat, window, 42);

        assert_eq!(w.candidate(seat), Some((window, 42)));
        assert_eq!(w.find(window), Ok(seat));
    }

    #[test]
    fn a_new_press_replaces_the_previous_candidate() {
        let mut w = SeatWorld::new();
        let seat = w.add_seat(1);
        let first = w.world.spawn(());
        let second = w.world.spawn(());

        w.press(seat, first, 1);
        w.press(seat, second, 2);

        assert_eq!(w.candidate(seat), Some((second, 2)));
        assert_eq!(w.find(first), Err(NoMoveCandidate::Missing));
        assert_eq!(w.find(second), Ok(seat));
    }

    #[test]
    fn candidates_belong_to_the_pressed_window_only() {
        let mut w = SeatWorld::new();
        let seat = w.add_seat(1);
        let pressed = w.world.spawn(());
        let other = w.world.spawn(());

        w.press(seat, pressed, 7);

        assert_eq!(w.find(other), Err(NoMoveCandidate::Missing));
        // Asking for the wrong window doesn't spend the candidate either.
        assert_eq!(w.find(pressed), Ok(seat));
    }

    #[test]
    fn invalidation_drops_the_candidate() {
        let mut w = SeatWorld::new();
        let seat = w.add_seat(1);
        let window = w.world.spawn(());

        w.press(seat, window, 3);
        invalidate_move_candidate(&mut w.world, seat);

        assert_eq!(w.candidate(seat), None);
        assert_eq!(w.find(window), Err(NoMoveCandidate::Missing));

        // A later real press works again: nothing is stuck in a consumed state.
        w.press(seat, window, 4);
        assert_eq!(w.find(window), Ok(seat));
    }

    #[test]
    fn invalidating_a_source_clears_every_seat_holding_it() {
        let mut w = SeatWorld::new();
        let first = w.add_seat(1);
        let second = w.add_seat(2);
        let window = w.world.spawn(());
        let other_window = w.world.spawn(());

        w.press(first, window, 1);
        w.press(second, other_window, 2);
        invalidate_move_candidates_for_source(&mut w.world, window);

        assert_eq!(w.candidate(first), None);
        assert_eq!(w.candidate(second), Some((other_window, 2)));
    }

    #[test]
    fn removing_a_seat_global_clears_all_of_its_binds() {
        let mut w = SeatWorld::new();
        // The same global bound twice, plus an unrelated seat.
        let first = w.add_seat(1);
        let second = w.add_seat(1);
        let unrelated = w.add_seat(2);
        let window = w.world.spawn(());

        w.press(first, window, 1);
        w.press(second, window, 2);
        w.press(unrelated, window, 3);

        w.globals.remove(&GlobalName(1));
        invalidate_move_candidates_for_seat_global(&mut w.world, GlobalName(1));

        assert_eq!(w.candidate(first), None);
        assert_eq!(w.candidate(second), None);
        assert_eq!(w.candidate(unrelated), Some((window, 3)));
        assert_eq!(w.find(window), Ok(unrelated));
    }

    #[test]
    fn a_press_queued_behind_a_global_removal_does_not_recreate_a_candidate() {
        let mut w = SeatWorld::new();
        let seat = w.add_seat(1);
        let window = w.world.spawn(());

        w.globals.remove(&GlobalName(1));
        invalidate_move_candidates_for_seat_global(&mut w.world, GlobalName(1));
        w.press(seat, window, 5);

        assert_eq!(w.candidate(seat), None);
        assert_eq!(w.find(window), Err(NoMoveCandidate::Missing));
    }

    #[test]
    fn a_removed_seat_global_hides_an_existing_candidate() {
        let mut w = SeatWorld::new();
        let seat = w.add_seat(1);
        let window = w.world.spawn(());

        w.press(seat, window, 9);
        w.globals.remove(&GlobalName(1));

        assert_eq!(w.find(window), Err(NoMoveCandidate::Missing));
    }

    #[test]
    fn a_replaced_pointer_does_not_inherit_the_press() {
        let mut w = SeatWorld::new();
        let seat = w.add_seat(1);
        let window = w.world.spawn(());

        w.press(seat, window, 11);
        // Simulates the candidate outliving a get_pointer that recreated the pointer object.
        w.rebind_pointer(seat);

        assert_eq!(w.find(window), Err(NoMoveCandidate::Missing));
    }

    #[test]
    fn a_seat_without_a_pointer_has_no_candidate() {
        let mut w = SeatWorld::new();
        let seat = w.add_seat(1);
        let window = w.world.spawn(());

        w.world.remove_one::<ClientPointer>(seat).unwrap();
        w.press(seat, window, 13);

        assert_eq!(w.candidate(seat), None);
        assert_eq!(w.find(window), Err(NoMoveCandidate::Missing));
    }

    #[test]
    fn several_seats_pressing_the_same_window_are_ambiguous() {
        let mut w = SeatWorld::new();
        let first = w.add_seat(1);
        let second = w.add_seat(2);
        let window = w.world.spawn(());

        w.press(first, window, 1);
        w.press(second, window, 2);

        assert_eq!(w.find(window), Err(NoMoveCandidate::Ambiguous));

        // Once one of them is gone the other is unambiguous again.
        invalidate_move_candidate(&mut w.world, first);
        assert_eq!(w.find(window), Ok(second));
    }
}
