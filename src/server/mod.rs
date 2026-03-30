mod clientside;
mod decoration;
mod dispatch;
mod event;
pub(crate) mod selection;
#[cfg(test)]
mod tests;

use self::event::*;
use crate::xstate::{Decorations, MoveResizeDirection, WindowDims, WmHints, WmName, WmNormalHints};
use crate::{X11Selection, XConnection, timespec_from_millis};
use clientside::MyWorld;
use decoration::{DecorationsData, DecorationsDataSatellite};
use hecs::{Entity, World};
use log::{debug, error, warn};
use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::Timespec;
use smithay_client_toolkit::activation::ActivationState;
use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use wayland_client::protocol::wl_subcompositor::WlSubcompositor;
use wayland_client::{
    Connection, EventQueue, Proxy, QueueHandle,
    globals::{Global, registry_queue_init},
    protocol as client,
};
use wayland_protocols::xdg::decoration::zv1::client::zxdg_decoration_manager_v1::ZxdgDecorationManagerV1;
use wayland_protocols::xdg::decoration::zv1::client::zxdg_toplevel_decoration_v1::{self};
use wayland_protocols::xdg::shell::client::xdg_positioner::ConstraintAdjustment;
use wayland_protocols::{
    wp::{
        fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        linux_dmabuf::zv1::{client as c_dmabuf, server as s_dmabuf},
        linux_drm_syncobj::v1::server::wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1,
        pointer_constraints::zv1::{
            client::{zwp_confined_pointer_v1, zwp_locked_pointer_v1},
            server::zwp_pointer_constraints_v1::ZwpPointerConstraintsV1,
        },
        relative_pointer::zv1::{
            client::zwp_relative_pointer_v1,
            server::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
        },
        tablet::zv2::client::{
            zwp_tablet_pad_group_v2, zwp_tablet_pad_ring_v2, zwp_tablet_pad_strip_v2,
            zwp_tablet_pad_v2, zwp_tablet_seat_v2, zwp_tablet_tool_v2, zwp_tablet_v2,
        },
        tablet::zv2::server::zwp_tablet_manager_v2::ZwpTabletManagerV2,
        viewporter::client::wp_viewporter::WpViewporter,
    },
    xdg::{
        shell::client::{
            xdg_popup::XdgPopup,
            xdg_positioner::{Anchor, Gravity, XdgPositioner},
            xdg_surface::XdgSurface,
            xdg_toplevel::{self, XdgToplevel},
            xdg_wm_base::XdgWmBase,
        },
        xdg_output::zv1::server::zxdg_output_manager_v1::ZxdgOutputManagerV1,
    },
    xwayland::shell::v1::server::xwayland_shell_v1::XwaylandShellV1,
};
use wayland_server::protocol::wl_seat::WlSeat;
use wayland_server::{
    Client, DisplayHandle, Resource, WEnum,
    backend::GlobalId,
    protocol::{
        wl_callback::WlCallback, wl_compositor::WlCompositor, wl_output::WlOutput, wl_shm::WlShm,
        wl_surface::WlSurface,
    },
};
use wl_drm::{client::wl_drm::WlDrm as WlDrmClient, server::wl_drm::WlDrm as WlDrmServer};
use xcb::x;

impl From<&x::CreateNotifyEvent> for WindowDims {
    fn from(value: &x::CreateNotifyEvent) -> Self {
        Self {
            x: value.x(),
            y: value.y(),
            width: value.width(),
            height: value.height(),
        }
    }
}

type Request<T> = <T as Resource>::Request;

/// Converts a WEnum from its client side version to its server side version
fn convert_wenum<Client, Server>(wenum: WEnum<Client>) -> Server
where
    u32: From<WEnum<Client>>,
    Server: TryFrom<u32>,
    <Server as TryFrom<u32>>::Error: std::fmt::Debug,
{
    u32::from(wenum).try_into().unwrap()
}

#[derive(Debug)]
struct WindowAttributes {
    is_popup: bool,
    dims: WindowDims,
    size_hints: Option<WmNormalHints>,
    hints_x11_scale: f64,
    title: Option<WmName>,
    class: Option<String>,
    group: Option<x::Window>,
    decorations: Option<Decorations>,
    transient_for: Option<x::Window>,
}

impl Default for WindowAttributes {
    fn default() -> Self {
        Self {
            is_popup: false,
            dims: WindowDims::default(),
            size_hints: None,
            hints_x11_scale: 1.0,
            title: None,
            class: None,
            group: None,
            decorations: None,
            transient_for: None,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq, Copy, Clone)]
struct WindowOutputOffset {
    x: i32,
    y: i32,
}

#[derive(Debug, Default)]
enum InitialToplevelMapState {
    #[default]
    None,
    Deferred { deadline: Instant },
    Released,
}

#[derive(Debug)]
struct WindowData {
    mapped: bool,
    attrs: WindowAttributes,
    output_offset: WindowOutputOffset,
    output_offset_initialized: bool,
    activation_token: Option<String>,
    initial_toplevel_map_state: InitialToplevelMapState,
    configure_move_serial: Option<u32>,
}

impl WindowData {
    const DEFERRED_INITIAL_TOPLEVEL_TIMEOUT: Duration = Duration::from_millis(200);

    fn new(override_redirect: bool, dims: WindowDims, activation_token: Option<String>) -> Self {
        Self {
            mapped: false,
            attrs: WindowAttributes {
                is_popup: override_redirect,
                dims,
                ..Default::default()
            },
            output_offset: WindowOutputOffset::default(),
            output_offset_initialized: false,
            activation_token,
            initial_toplevel_map_state: InitialToplevelMapState::None,
            configure_move_serial: None,
        }
    }

    fn has_output_offset(&self) -> bool {
        self.output_offset_initialized
    }

    fn is_popup(&self) -> bool {
        self.attrs.is_popup
    }

    fn record_output_offset(&mut self, offset: WindowOutputOffset) {
        self.output_offset = offset;
        self.output_offset_initialized = true;
    }

    fn update_output_offset_on_enter<C: XConnection>(
        &mut self,
        window: x::Window,
        offset: WindowOutputOffset,
        connection: &mut C,
    ) {
        if !self.is_popup() && self.has_output_offset() {
            return;
        }

        self.update_output_offset(window, offset, connection);
    }

    fn should_defer_initial_toplevel_map(&self) -> bool {
        !self.attrs.is_popup
            && !self
                .attrs
                .decorations
                .is_some_and(|decorations| decorations.is_serverside())
            && self.attrs.dims.x == 0
            && self.attrs.dims.y == 0
            && matches!(self.initial_toplevel_map_state, InitialToplevelMapState::None)
    }

    fn defer_initial_toplevel_map(&mut self) {
        self.initial_toplevel_map_state = InitialToplevelMapState::Deferred {
            deadline: Instant::now() + Self::DEFERRED_INITIAL_TOPLEVEL_TIMEOUT,
        };
    }

    fn has_deferred_initial_toplevel_map(&self) -> bool {
        matches!(
            self.initial_toplevel_map_state,
            InitialToplevelMapState::Deferred { .. }
        )
    }

    fn deferred_initial_toplevel_deadline(&self) -> Option<Instant> {
        match self.initial_toplevel_map_state {
            InitialToplevelMapState::Deferred { deadline } => Some(deadline),
            InitialToplevelMapState::None | InitialToplevelMapState::Released => None,
        }
    }

    fn release_initial_toplevel_map(&mut self) -> bool {
        if matches!(self.initial_toplevel_map_state, InitialToplevelMapState::Deferred { .. }) {
            self.initial_toplevel_map_state = InitialToplevelMapState::Released;
            return true;
        }

        false
    }

    fn update_output_offset<C: XConnection>(
        &mut self,
        window: x::Window,
        offset: WindowOutputOffset,
        connection: &mut C,
    ) {
        log::trace!(target: "output_offset", "offset: {offset:?}");
        if self.output_offset_initialized && offset == self.output_offset {
            return;
        }

        let previous_offset = self.output_offset;

        // A non-zero startup position is already in global X11 coordinates when the
        // surface first enters an output. Record the output origin without shifting
        // the window again; later output moves should still apply deltas normally.
        if !self.output_offset_initialized
            && (self.attrs.dims.x != 0 || self.attrs.dims.y != 0)
        {
            self.record_output_offset(offset);
            return;
        }

        self.attrs.dims.x += (offset.x - previous_offset.x) as i16;
        self.attrs.dims.y += (offset.y - previous_offset.y) as i16;
        let new_x = self.attrs.dims.x as i32;
        let new_y = self.attrs.dims.y as i32;
        let width = self.attrs.dims.width as _;
        let height = self.attrs.dims.height as _;
        self.record_output_offset(offset);

        if connection.set_window_dims(
            window,
            PendingSurfaceState {
                x: new_x,
                y: new_y,
                width,
                height,
            },
        ) {
            debug!(target: "output_offset", "set {:?} offset to {:?}", window, self.output_offset);
        }
    }
}

pub(in crate::server) fn max_available_output_scale(world: &World) -> f64 {
    world
        .query::<(&WlOutput, &OutputScaleFactor)>()
        .iter()
        .map(|(entity, (_, scale))| {
            world
                .get::<&OutputDimensions>(entity)
                .map(|dimensions| dimensions.constraint_scale(scale.get()))
                .unwrap_or_else(|_| scale.get().max(1.0))
        })
        .fold(1.0, f64::max)
        .max(1.0)
}

fn apply_toplevel_size_hints(
    toplevel: &XdgToplevel,
    hints: &WmNormalHints,
    hints_x11_scale: f64,
    decorations_height: i32,
) {
    let divisor = hints_x11_scale.max(1.0);
    if let Some(min_size) = &hints.min_size {
        let min_width = (min_size.width as f64 / divisor) as i32;
        let min_height = (min_size.height as f64 / divisor) as i32 + decorations_height;
        toplevel.set_min_size(min_width, min_height);
    }

    if let Some(max_size) = &hints.max_size {
        let max_width = (max_size.width as f64 / divisor) as i32;
        let max_height = (max_size.height as f64 / divisor) as i32 + decorations_height;
        toplevel.set_max_size(max_width, max_height);
    }
}

struct SurfaceAttach {
    buffer: Option<client::wl_buffer::WlBuffer>,
    x: i32,
    y: i32,
}

#[derive(PartialEq, Eq, Debug)]
struct SurfaceSerial([u32; 2]);

#[derive(Debug)]
enum SurfaceRole {
    Toplevel(Option<ToplevelData>),
    Popup(Option<PopupData>),
}

impl SurfaceRole {
    fn xdg(&self) -> Option<&XdgSurfaceData> {
        match self {
            SurfaceRole::Toplevel(t) => t.as_ref().map(|t| &t.xdg),
            SurfaceRole::Popup(p) => p.as_ref().map(|p| &p.xdg),
        }
    }

    fn xdg_mut(&mut self) -> Option<&mut XdgSurfaceData> {
        match self {
            SurfaceRole::Toplevel(t) => t.as_mut().map(|t| &mut t.xdg),
            SurfaceRole::Popup(p) => p.as_mut().map(|p| &mut p.xdg),
        }
    }

    fn destroy(&mut self) {
        match self {
            SurfaceRole::Toplevel(Some(t)) => {
                if let Some(decoration) = t.decoration.wl.take() {
                    decoration.destroy();
                }
                t.toplevel.destroy();
                t.xdg.surface.destroy();
            }
            SurfaceRole::Popup(Some(p)) => {
                p.positioner.destroy();
                p.popup.destroy();
                p.xdg.surface.destroy();
            }
            _ => {}
        }
    }
}

#[derive(Debug)]
struct XdgSurfaceData {
    surface: XdgSurface,
    configured: bool,
    pending: Option<PendingSurfaceState>,
}

#[derive(Debug)]
struct ToplevelData {
    toplevel: XdgToplevel,
    xdg: XdgSurfaceData,
    fullscreen: bool,
    decoration: decoration::DecorationsData,
}

#[derive(Debug)]
struct PopupData {
    popup: XdgPopup,
    positioner: XdgPositioner,
    xdg: XdgSurfaceData,
}

trait Event {
    fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>);
}

macro_rules! enum_try_from {
    (
        $(#[$meta:meta])*
        $pub:vis enum $enum:ident {
            $( $variant:ident($ty:ty) ),+
        }
    ) => {
        $(#[$meta])*
        $pub enum $enum {
            $( $variant($ty) ),+
        }

        $(
            impl TryFrom<$enum> for $ty {
                type Error = String;
                fn try_from(value: $enum) -> Result<Self, Self::Error> {
                    enum_try_from!(@variant_match value $enum $variant)
                }
            }

            impl<'a> TryFrom<&'a $enum> for &'a $ty {
                type Error = String;
                fn try_from(value: &'a $enum) -> Result<Self, Self::Error> {
                    enum_try_from!(@variant_match value $enum $variant)
                }
            }

            impl<'a> TryFrom<&'a mut $enum> for &'a mut $ty {
                type Error = String;
                fn try_from(value: &'a mut $enum) -> Result<Self, Self::Error> {
                    enum_try_from!(@variant_match value $enum $variant)
                }
            }

            impl From<$ty> for $enum {
                fn from(value: $ty) -> Self {
                    $enum::$variant(value)
                }
            }
        )+
    };
    (@variant_match $value:ident $enum:ident $variant:ident) => {
        match $value {
            $enum::$variant(obj) => Ok(obj),
            other => Err(format!("wrong variant type: {}", std::any::type_name_of_val(&other)))
        }
    }
}

macro_rules! impl_event {
    (
        $(#[$meta:meta])*
        $pub:vis enum $enum:ident {
            $( $variant:ident($ty:ty) ),+
        }
    ) => {
        enum_try_from! {
            $(#[$meta])*
            $pub enum $enum {
                $( $variant($ty) ),+
            }
        }

        impl Event for $enum {
            fn handle<C: XConnection>(self, target: Entity, state: &mut ServerState<C>) {
                match self {
                    $(
                        Self::$variant(v) => {
                            v.handle(target, state)
                        }
                    ),+
                }
            }
        }
    }
}

impl_event! {
enum ObjectEvent {
    Surface(event::SurfaceEvents),
    Buffer(client::wl_buffer::Event),
    Seat(client::wl_seat::Event),
    Pointer(client::wl_pointer::Event),
    Keyboard(client::wl_keyboard::Event),
    Touch(client::wl_touch::Event),
    Output(event::OutputEvent),
    Drm(wl_drm::client::wl_drm::Event),
    DmabufFeedback(c_dmabuf::zwp_linux_dmabuf_feedback_v1::Event),
    RelativePointer(zwp_relative_pointer_v1::Event),
    LockedPointer(zwp_locked_pointer_v1::Event),
    ConfinedPointer(zwp_confined_pointer_v1::Event),
    TabletSeat(zwp_tablet_seat_v2::Event),
    Tablet(zwp_tablet_v2::Event),
    TabletPad(zwp_tablet_pad_v2::Event),
    TabletTool(zwp_tablet_tool_v2::Event),
    TabletPadGroup(zwp_tablet_pad_group_v2::Event),
    TabletPadRing(zwp_tablet_pad_ring_v2::Event),
    TabletPadStrip(zwp_tablet_pad_strip_v2::Event)
}
}

fn handle_new_globals<'a, S: X11Selection + 'static>(
    globals_map: &mut HashMap<GlobalName, (Global, GlobalId)>,
    dh: &DisplayHandle,
    globals: impl IntoIterator<Item = &'a Global>,
) {
    for global in globals {
        macro_rules! server_global {
            ($($global:ty),+) => {
                match global.interface {
                    $(
                        ref x if x == <$global>::interface().name => {
                            let version = u32::min(global.version, <$global>::interface().version);
                            let global_id = dh.create_global::<InnerServerState<S>, $global, Global>(version, global.clone());
                            globals_map.insert(GlobalName(global.name), (global.clone(), global_id));
                        }
                    )+
                    _ => {}
                }
            }
        }

        server_global![
            WlCompositor,
            WlShm,
            WlSeat,
            WlOutput,
            ZwpRelativePointerManagerV1,
            WlDrmServer,
            s_dmabuf::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
            ZxdgOutputManagerV1,
            ZwpPointerConstraintsV1,
            ZwpTabletManagerV2,
            WpLinuxDrmSyncobjManagerV1
        ];
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub(super) struct GlobalName(pub u32);

struct FocusData {
    window: x::Window,
    output_name: Option<String>,
}

#[derive(Copy, Clone, Default)]
struct GlobalOutputOffsetDimension {
    owner: Option<Entity>,
    value: i32,
}

#[derive(Copy, Clone)]
struct GlobalOutputOffset {
    x: GlobalOutputOffsetDimension,
    y: GlobalOutputOffsetDimension,
}

/// The state of the X11 connection before XState has been fully initialized.
/// It implements XConnection minimally, gracefully doing nothing but logging the called functions.
pub struct NoConnection<S: X11Selection + 'static> {
    _p: std::marker::PhantomData<S>,
}
impl<S: X11Selection> XConnection for NoConnection<S> {
    type X11Selection = S;
    fn focus_window(&mut self, _: x::Window, _: Option<String>) {
        debug!("could not focus window without XWayland initialized");
    }
    fn close_window(&mut self, _: x::Window) {
        debug!("could not close window without XWayland initialized");
    }
    fn unmap_window(&mut self, _: x::Window) {
        debug!("could not unmap window without XWayland initialized");
    }
    fn raise_to_top(&mut self, _: x::Window) {
        debug!("could not raise window to top without XWayland initialized");
    }
    fn set_fullscreen(&mut self, _: x::Window, _: bool) {
        debug!("could not toggle fullscreen without XWayland initialized");
    }
    fn set_window_dims(&mut self, _: x::Window, _: crate::server::PendingSurfaceState) -> bool {
        debug!("could not set window dimensions without XWayland initialized");
        false
    }
}

pub struct ServerState<C: XConnection> {
    inner: InnerServerState<C::X11Selection>,
    pub connection: C,
}
impl<C: XConnection> Deref for ServerState<C> {
    type Target = InnerServerState<C::X11Selection>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl<C: XConnection> DerefMut for ServerState<C> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

pub struct InnerServerState<S: X11Selection> {
    dh: DisplayHandle,
    windows: HashMap<x::Window, Entity>,
    pids: HashSet<u32>,

    world: MyWorld,
    queue: EventQueue<MyWorld>,
    qh: QueueHandle<MyWorld>,
    globals_map: HashMap<GlobalName, (Global, GlobalId)>,
    client: Client,
    to_focus: Option<FocusData>,
    unfocus: bool,
    last_focused_toplevel: Option<x::Window>,
    last_hovered: Option<x::Window>,

    xdg_wm_base: XdgWmBase,
    compositor: client::wl_compositor::WlCompositor,
    subcompositor: WlSubcompositor,
    shm: client::wl_shm::WlShm,
    viewporter: WpViewporter,
    fractional_scale: Option<WpFractionalScaleManagerV1>,
    decoration_manager: Option<ZxdgDecorationManagerV1>,
    selection_states: selection::SelectionStates<S>,
    last_kb_serial: Option<(client::wl_seat::WlSeat, u32)>,
    activation_state: Option<ActivationState>,
    global_output_offset: GlobalOutputOffset,
    global_offset_updated: bool,
    updated_outputs: Vec<Entity>,
    new_scale: Option<f64>,
    pub published_x11_scale: f64,
}

impl<S: X11Selection> ServerState<NoConnection<S>> {
    pub fn new(
        mut dh: DisplayHandle,
        server_connection: Option<UnixStream>,
        client: UnixStream,
    ) -> Self {
        let connection = if let Some(stream) = server_connection {
            Connection::from_socket(stream).unwrap()
        } else {
            Connection::connect_to_env().unwrap()
        };

        let (global_list, queue) = registry_queue_init::<MyWorld>(&connection).unwrap();
        let qh = queue.handle();

        let xdg_wm_base = global_list
            .bind::<XdgWmBase, _, _>(&qh, 2..=6, ())
            .expect("Could not bind xdg_wm_base");

        if xdg_wm_base.version() < 3 {
            warn!(
                "xdg_wm_base version 2 detected. Popup repositioning will not work, and some popups may not work correctly."
            );
        }

        let compositor = global_list
            .bind::<client::wl_compositor::WlCompositor, _, _>(&qh, 4..=6, ())
            .expect("Could not bind wl_compositor");

        let subcompositor = global_list
            .bind::<WlSubcompositor, _, _>(&qh, 1..=1, ())
            .expect("Could not bind wl_subcompositor");

        let shm = global_list
            .bind::<client::wl_shm::WlShm, _, _>(&qh, 1..=1, ())
            .expect("Could not bind wl_shm");

        let viewporter = global_list
            .bind::<WpViewporter, _, _>(&qh, 1..=1, ())
            .expect("Could not bind wp_viewporter");

        let fractional_scale = global_list
            .bind::<WpFractionalScaleManagerV1, _, _>(&qh, 1..=1, ())
            .inspect_err(|e| {
                warn!(
                    "Couldn't bind fractional scale manager: {e}. Fractional scaling will not work."
                )
            })
            .ok();

        let activation_state = ActivationState::bind(&global_list, &qh)
            .inspect_err(|e| {
                warn!("Could not bind xdg activation ({e:?}). Windows might not receive focus depending on compositor focus stealing policy.")
            })
            .ok();

        let decoration_manager = global_list
            .bind::<ZxdgDecorationManagerV1, _, _>(&qh, 1..=1, ())
            .ok();

        let selection_states = selection::SelectionStates::new(&global_list, &qh);

        dh.create_global::<InnerServerState<S>, XwaylandShellV1, _>(1, ());

        let mut globals_map = HashMap::new();
        global_list
            .contents()
            .with_list(|globals| handle_new_globals::<S>(&mut globals_map, &dh, globals));

        let world = MyWorld::new(global_list);
        let client = dh.insert_client(client, std::sync::Arc::new(())).unwrap();

        let inner = InnerServerState {
            windows: HashMap::new(),
            pids: HashSet::new(),
            client,
            queue,
            qh,
            globals_map,
            dh,
            to_focus: None,
            unfocus: false,
            last_focused_toplevel: None,
            last_hovered: None,
            xdg_wm_base,
            compositor,
            subcompositor,
            shm,
            viewporter,
            fractional_scale,
            selection_states,
            last_kb_serial: None,
            activation_state,
            global_output_offset: GlobalOutputOffset {
                x: GlobalOutputOffsetDimension {
                    owner: None,
                    value: 0,
                },
                y: GlobalOutputOffsetDimension {
                    owner: None,
                    value: 0,
                },
            },
            global_offset_updated: false,
            updated_outputs: Vec::new(),
            new_scale: None,
            published_x11_scale: 1.0,
            decoration_manager,
            world,
        };
        Self {
            inner,
            connection: NoConnection {
                _p: std::marker::PhantomData,
            },
        }
    }

    pub fn upgrade_connection<C>(self, connection: C) -> ServerState<C>
    where
        C: XConnection<X11Selection = S>,
    {
        ServerState {
            inner: self.inner,
            connection,
        }
    }
}

impl<C: XConnection> ServerState<C> {
    pub fn run(&mut self) {
        if let Some(r) = self.queue.prepare_read() {
            let fd = r.connection_fd();
            let pollfd = PollFd::new(&fd, PollFlags::IN);
            let timeout = timespec_from_millis(0);
            if poll(&mut [pollfd], Some(&timeout)).unwrap() > 0 {
                let _ = r.read();
            }
        }
        let state = self.deref_mut();
        state
            .queue
            .dispatch_pending(&mut state.world)
            .expect("Failed dispatching client side Wayland events");
        self.handle_clientside_events();
    }

    pub fn flush_pending_x11_configures(&mut self) {
        let query = self.world.query_mut::<(&x::Window, &PendingSurfaceState)>();
        let iter = query
            .into_iter()
            .map(|(e, (win, dims))| (e, (*win, *dims)))
            .collect::<Vec<_>>();
        for (entity, (win, dims)) in iter.into_iter() {
            self.connection.set_window_dims(win, dims);
            self.world
                .remove_one::<PendingSurfaceState>(entity)
                .unwrap();
        }
    }

    pub fn republish_xwayland_outputs_for_x11_scale(&mut self) {
        let outputs = self
            .world
            .query::<&WlOutput>()
            .iter()
            .map(|(entity, _)| entity)
            .collect::<Vec<_>>();

        for output in outputs {
            event::republish_xwayland_output_for_x11_scale(
                output,
                &self.global_output_offset,
                &self.world,
                self.published_x11_scale,
            );
        }
    }

    fn apply_global_surface_scale(&mut self, new_scale: f64) {
        let new_scale = new_scale.max(1.0);
        let surfaces = self
            .world
            .query::<(&WindowData, &SurfaceScaleFactor)>()
            .iter()
            .map(|(entity, _)| entity)
            .collect::<Vec<_>>();

        for entity in surfaces {
            let pending = {
                let Ok(mut query) = self.world.query_one::<(
                    &mut WindowData,
                    &mut SurfaceScaleFactor,
                    Option<&SurfaceRole>,
                )>(entity)
                else {
                    continue;
                };
                let Some((window_data, scale, role)) = query.get() else {
                    continue;
                };
                let old_scale = scale.0.max(1.0);
                if (old_scale - new_scale).abs() <= 0.01 {
                    None
                } else {
                    let logical_width = (f64::from(window_data.attrs.dims.width) / old_scale)
                        .ceil()
                        .max(1.0) as i32;
                    let logical_height = (f64::from(window_data.attrs.dims.height) / old_scale)
                        .ceil()
                        .max(1.0) as i32;
                    scale.0 = new_scale;

                    let is_mapped_toplevel = window_data.mapped
                        && role
                            .is_some_and(|role| matches!(role, SurfaceRole::Toplevel(Some(_))));
                    if is_mapped_toplevel {
                        let width = ((logical_width as f64) * new_scale)
                            .ceil()
                            .clamp(1.0, f64::from(u16::MAX))
                            as i32;
                        let height = ((logical_height as f64) * new_scale)
                            .ceil()
                            .clamp(1.0, f64::from(u16::MAX))
                            as i32;
                        window_data.attrs.dims.width = width as u16;
                        window_data.attrs.dims.height = height as u16;
                        Some(PendingSurfaceState {
                            x: i32::from(window_data.attrs.dims.x),
                            y: i32::from(window_data.attrs.dims.y),
                            width,
                            height,
                        })
                    } else {
                        None
                    }
                }
            };

            if let Some(pending) = pending {
                if let Ok(mut current_pending) = self.world.get::<&mut PendingSurfaceState>(entity) {
                    *current_pending = pending;
                } else {
                    self.world.insert_one(entity, pending).unwrap();
                }
            }

            update_surface_viewport(&self.world, self.world.query_one(entity).unwrap());
        }
    }

    pub fn handle_clientside_events(&mut self) {
        self.handle_globals();

        for (target, event) in self.world.read_events() {
            if !self.world.contains(target) {
                warn!("could not handle clientside event: stale object");
                continue;
            }
            event.handle(target, self);
        }

        if self.global_output_offset.x.owner.is_none()
            || self.global_output_offset.y.owner.is_none()
        {
            self.calc_global_output_offset();
            self.global_offset_updated = true;
        }
        if self.global_offset_updated {
            debug!(
                target: "output_offset",
                "updated global output offset: {}x{}",
                self.global_output_offset.x.value, self.global_output_offset.y.value
            );
            let x11_scale = self.published_x11_scale;
            let state = &self.inner;
            for (e, _) in state.world.query::<&WlOutput>().iter() {
                event::update_global_output_offset(
                    e,
                    &state.global_output_offset,
                    &state.world,
                    &mut self.connection,
                    x11_scale,
                );
            }
            self.global_offset_updated = false;
        }

        if !self.updated_outputs.is_empty() {
            self.updated_outputs.clear();
            let scale = max_available_output_scale(&self.world);
            self.apply_global_surface_scale(scale);
            if self
                .new_scale
                .is_none_or(|queued| (queued - scale).abs() > 0.01)
            {
                self.new_scale = Some(scale);
            }
        }

        {
            if let Some(FocusData {
                window,
                output_name,
            }) = self.to_focus.take()
            {
                debug!("focusing window {window:?}");
                self.connection.focus_window(window, output_name);
                self.last_focused_toplevel = Some(window);
            } else if self.unfocus {
                self.connection.focus_window(x::WINDOW_NONE, None);
            }
            self.unfocus = false;
        }

        self.handle_selection_events();
        self.handle_activations();
        self.release_expired_deferred_initial_toplevels();
        if let Err(e) = self.queue.flush() {
            match e {
                wayland_client::backend::WaylandError::Io(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    let fd = PollFd::new(&self.queue, PollFlags::OUT);
                    match poll(
                        &mut [fd],
                        Some(&Timespec {
                            tv_sec: 0,
                            tv_nsec: Duration::from_millis(50).as_nanos() as _,
                        }),
                    ) {
                        Ok(0) => {
                            error!(
                                "Failed to flush clientside events (timeout)! Will try again later."
                            );
                        }
                        Ok(_) => {
                            self.queue.flush().unwrap();
                        }
                        Err(e) => {
                            error!(
                                "Failed to flush clientside events ({e})! Will try again later."
                            );
                        }
                    }
                }
                other => {
                    panic!("Failed flushing clientside events: {other:#?}");
                }
            }
        }
    }

    fn close_x_window(&mut self, window: x::Window) {
        debug!("sending close request to {window:?}");
        self.connection.close_window(window);
        if self.last_focused_toplevel == Some(window) {
            self.last_focused_toplevel.take();
        }
        if self.last_hovered == Some(window) {
            self.last_hovered.take();
        }
    }
}

impl<S: X11Selection + 'static> InnerServerState<S> {
    pub fn clientside_fd(&self) -> BorrowedFd<'_> {
        self.queue.as_fd()
    }

    fn handle_globals(&mut self) {
        let globals = std::mem::take(&mut self.world.new_globals);
        handle_new_globals::<S>(&mut self.globals_map, &self.dh, &globals);

        let globals = std::mem::take(&mut self.world.removed_globals);
        for global in globals {
            let (global_struct, global_id) = self.globals_map.remove(&global).unwrap();
            self.dh.disable_global::<InnerServerState<S>>(global_id);
            if global_struct.interface == <WlOutput>::interface().name {
                self.remove_output(global);
            }
        }
    }

    fn remove_output(&mut self, global: GlobalName) {
        let query = self
            .world
            .query_mut::<(&WlOutput, &GlobalName)>()
            .into_iter()
            .map(|(e, (_, name))| (e, *name))
            .collect::<Vec<_>>();
        for (entity, name) in query.iter() {
            if *name == global {
                self.updated_outputs.push(*entity);
                self.world
                    .remove::<(OutputScaleFactor, OutputDimensions)>(*entity)
                    .unwrap();
                let query = self
                    .world
                    .query_mut::<&OnOutput>()
                    .into_iter()
                    .map(|(e, on_out)| (e, *on_out))
                    .collect::<Vec<_>>();
                for (e, on_out) in query.iter() {
                    if *on_out == OnOutput(*entity) {
                        self.world.remove_one::<OnOutput>(*e).unwrap();
                    }
                }
                if self.global_output_offset.x.owner == Some(*entity) {
                    self.global_offset_updated = true;
                    self.global_output_offset.x.owner = None;
                }
                if self.global_output_offset.y.owner == Some(*entity) {
                    self.global_offset_updated = true;
                    self.global_output_offset.y.owner = None;
                }
                break;
            }
        }
    }

    pub fn new_window(
        &mut self,
        window: x::Window,
        override_redirect: bool,
        dims: WindowDims,
        pid: Option<u32>,
    ) {
        let activation_token = pid
            .filter(|pid| self.pids.insert(*pid))
            .and_then(|pid| std::fs::read(format!("/proc/{pid}/environ")).ok())
            .and_then(|environ| {
                environ
                    .split(|byte| *byte == 0)
                    .find_map(|line| line.strip_prefix(b"XDG_ACTIVATION_TOKEN="))
                    .and_then(|token| String::from_utf8(token.to_vec()).ok())
            });

        let id = self.world.spawn((
            window,
            WindowData::new(override_redirect, dims, activation_token),
        ));

        self.windows.insert(window, id);
    }

    pub fn set_popup(&mut self, window: x::Window, is_popup: bool) {
        let Some(id) = self.windows.get(&window).copied() else {
            debug!("not setting popup for unknown window {window:?}");
            return;
        };

        self.world
            .get::<&mut WindowData>(id)
            .unwrap()
            .attrs
            .is_popup = is_popup;
    }

    pub fn set_win_title(&mut self, window: x::Window, name: WmName) {
        let Some(data) = self
            .windows
            .get(&window)
            .copied()
            .and_then(|id| self.world.entity(id).ok())
        else {
            debug!("not setting title for unknown window {window:?}");
            return;
        };

        let mut win = data.get::<&mut WindowData>().unwrap();

        let new_title = match &mut win.attrs.title {
            Some(w) => {
                if matches!(w, WmName::NetWmName(_)) && matches!(name, WmName::WmName(_)) {
                    debug!(
                        "skipping setting window name to {name:?} because a _NET_WM_NAME title is already set"
                    );
                    None
                } else {
                    debug!("setting {window:?} title to {name:?}");
                    *w = name;
                    Some(w)
                }
            }
            None => Some(win.attrs.title.insert(name)),
        };

        let Some(title) = new_title else {
            return;
        };

        if let Some(mut role) = data.get::<&mut SurfaceRole>() {
            if let SurfaceRole::Toplevel(Some(data)) = &mut *role {
                data.toplevel.set_title(title.name().to_string());
                if let Some(d) = &mut data.decoration.satellite {
                    d.set_title(&self.world, title.name());
                }
            }
        }
    }

    pub fn set_win_class(&mut self, window: x::Window, class: String) {
        let Some(data) = self
            .windows
            .get(&window)
            .copied()
            .and_then(|id| self.world.entity(id).ok())
        else {
            debug!("not setting class for unknown window {window:?}");
            return;
        };

        let mut win = data.get::<&mut WindowData>().unwrap();

        let class = win.attrs.class.insert(class);
        if let Some(role) = data.get::<&SurfaceRole>() {
            if let SurfaceRole::Toplevel(Some(data)) = &*role {
                data.toplevel.set_app_id(class.to_string());
            }
        }
    }

    pub fn set_win_hints(&mut self, window: x::Window, hints: WmHints) {
        let Some(id) = self.windows.get(&window).copied() else {
            debug!("not setting hints for unknown window {window:?}");
            return;
        };

        self.world.get::<&mut WindowData>(id).unwrap().attrs.group = hints.window_group;
    }

    pub fn set_size_hints(&mut self, window: x::Window, hints: WmNormalHints) {
        let Some(data) = self
            .windows
            .get(&window)
            .copied()
            .and_then(|id| self.world.entity(id).ok())
        else {
            debug!("not setting size hints for unknown window {window:?}");
            return;
        };

        let mut win = data.get::<&mut WindowData>().unwrap();

        if win.attrs.size_hints.is_none_or(|h| h != hints) {
            debug!("setting {window:?} hints {hints:?}");
            let mut query = data.query::<&SurfaceRole>();
            if let Some(SurfaceRole::Toplevel(Some(data))) = query.get() {
                let decorations_height = data
                    .decoration
                    .satellite
                    .as_ref()
                    .map_or(0, |_| DecorationsDataSatellite::TITLEBAR_HEIGHT);
                apply_toplevel_size_hints(
                    &data.toplevel,
                    &hints,
                    self.published_x11_scale,
                    decorations_height,
                );

                if let Some(surface) = data
                    .xdg
                    .surface
                    .data()
                    .copied()
                    .and_then(|entity| self.world.get::<&client::wl_surface::WlSurface>(entity).ok())
                {
                    surface.commit();
                }
            }
            win.attrs.hints_x11_scale = self.published_x11_scale;
            win.attrs.size_hints = Some(hints);
        }
    }

    pub fn set_win_decorations(&mut self, window: x::Window, decorations: Decorations) {
        let Some(data) = self
            .windows
            .get(&window)
            .copied()
            .and_then(|id| self.world.entity(id).ok())
        else {
            debug!("not setting decorations for unknown window {window:?}");
            return;
        };

        let mut win = data.get::<&mut WindowData>().unwrap();

        if win.attrs.decorations != Some(decorations) {
            debug!("setting {window:?} decorations {decorations:?}");
            if let Some(role) = data.get::<&SurfaceRole>() {
                if let SurfaceRole::Toplevel(Some(data)) = &*role {
                    if let Some(decoration) = &data.decoration.wl {
                        decoration.set_mode(decorations.into());
                    }
                }
            }
            win.attrs.decorations = Some(decorations);
        }
    }

    pub fn set_window_serial(&mut self, window: x::Window, serial: [u32; 2]) {
        let Some(id) = self.windows.get(&window).copied() else {
            warn!("Tried to set serial for unknown window {window:?}");
            return;
        };

        self.world.insert(id, (SurfaceSerial(serial),)).unwrap();
    }

    fn maybe_create_mapped_window_role(&mut self, window: x::Window) -> bool {
        let Some(entity) = self.windows.get(&window).copied() else {
            return false;
        };

        if self.world.get::<&WlSurface>(entity).is_err()
            || self.world.get::<&SurfaceRole>(entity).is_ok()
        {
            return false;
        }

        let mut win = self.world.get::<&mut WindowData>(entity).unwrap();
        if !win.mapped {
            return false;
        }

        if win.should_defer_initial_toplevel_map() {
            debug!(
                "deferring initial toplevel creation for {window:?} until startup geometry settles; provisional dims {:?}",
                win.attrs.dims
            );
            win.defer_initial_toplevel_map();
            return false;
        }

        if win.has_deferred_initial_toplevel_map() {
            return false;
        }

        drop(win);

        self.create_role_window_and_activate(window, entity)
    }

    fn create_role_window_and_activate(&mut self, window: x::Window, entity: Entity) -> bool {
        let is_toplevel = self.create_role_window(window, entity);
        if is_toplevel {
            self.activate_window(window);
        }

        is_toplevel
    }

    fn release_deferred_initial_toplevel_map(&mut self, window: x::Window, reason: &str) -> bool {
        let Some(entity) = self.windows.get(&window).copied() else {
            return false;
        };

        let Some(mut win) = self
            .world
            .entity(entity)
            .ok()
            .map(|data| data.get::<&mut WindowData>().unwrap())
        else {
            return false;
        };

        let dims = win.attrs.dims;
        if !win.release_initial_toplevel_map() {
            return false;
        }

        debug!(
            "releasing deferred initial toplevel creation for {window:?} after {reason}; dims={dims:?}"
        );
        drop(win);

        if self.world.get::<&WlSurface>(entity).is_ok()
            && self.world.get::<&SurfaceRole>(entity).is_err()
        {
            self.create_role_window_and_activate(window, entity);
        }

        true
    }

    fn release_expired_deferred_initial_toplevels(&mut self) {
        let now = Instant::now();
        let deferred = self
            .world
            .query::<(&x::Window, &WindowData)>()
            .with::<&WlSurface>()
            .without::<&SurfaceRole>()
            .iter()
            .filter_map(|(_, (window, win))| {
                let deadline = win.deferred_initial_toplevel_deadline()?;
                (win.mapped && deadline <= now).then_some(*window)
            })
            .collect::<Vec<_>>();

        for window in deferred {
            self.release_deferred_initial_toplevel_map(window, "fallback timeout");
        }
    }

    pub(crate) fn should_forward_configure_position(
        &self,
        window: x::Window,
        mask: x::ConfigWindowMask,
    ) -> bool {
        let Some(entity) = self.windows.get(&window).copied() else {
            return true;
        };
        let Ok(data) = self.world.entity(entity) else {
            return true;
        };
        let Some(win) = data.get::<&WindowData>() else {
            return true;
        };

        if !win.mapped
            || win.attrs.is_popup
            || win.has_deferred_initial_toplevel_map()
            || self.world.get::<&SurfaceRole>(entity).is_err()
        {
            return true;
        }

        if !mask.intersects(x::ConfigWindowMask::WIDTH | x::ConfigWindowMask::HEIGHT) {
            return false;
        }

        match data.get::<&SurfaceRole>() {
            Some(role) => matches!(&*role, SurfaceRole::Toplevel(Some(_))),
            None => true,
        }
    }

    pub fn handle_configure_request(
        &mut self,
        window: x::Window,
        mask: x::ConfigWindowMask,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) {
        let Some(entity) = self.windows.get(&window).copied() else {
            return;
        };

        let Some(mut win) = self
            .world
            .entity(entity)
            .ok()
            .map(|data| data.get::<&mut WindowData>().unwrap())
        else {
            return;
        };

        let has_role = self.world.get::<&SurfaceRole>(entity).is_ok();
        let has_deferred_initial_map = win.has_deferred_initial_toplevel_map();

        if has_role && !has_deferred_initial_map {
            return;
        }

        if mask.contains(x::ConfigWindowMask::X) {
            win.attrs.dims.x = x;
        }
        if mask.contains(x::ConfigWindowMask::Y) {
            win.attrs.dims.y = y;
        }
        if mask.contains(x::ConfigWindowMask::WIDTH) {
            win.attrs.dims.width = width;
        }
        if mask.contains(x::ConfigWindowMask::HEIGHT) {
            win.attrs.dims.height = height;
        }

        drop(win);

        if !has_role {
            self.maybe_create_mapped_window_role(window);
        }
    }

    pub fn begin_configure_move(
        &mut self,
        window: x::Window,
        dims: WindowDims,
        mask: x::ConfigWindowMask,
    ) -> bool {
        let should_move = {
            let Some(data) = self
                .windows
                .get(&window)
                .copied()
                .and_then(|id| self.world.entity(id).ok())
            else {
                return false;
            };

            let Some(last_click_data) = data.get::<&LastClickSerial>() else {
                return false;
            };
            let serial = last_click_data.1;

            let Some(role) = data.get::<&SurfaceRole>() else {
                return false;
            };
            if !matches!(&*role, SurfaceRole::Toplevel(Some(_))) {
                return false;
            }
            drop(role);

            let mut win = data.get::<&mut WindowData>().unwrap();
            let current_dims = win.attrs.dims;
            let requested_x = if mask.contains(x::ConfigWindowMask::X) {
                dims.x
            } else {
                current_dims.x
            };
            let requested_y = if mask.contains(x::ConfigWindowMask::Y) {
                dims.y
            } else {
                current_dims.y
            };
            let requested_width = if mask.contains(x::ConfigWindowMask::WIDTH) {
                dims.width
            } else {
                current_dims.width
            };
            let requested_height = if mask.contains(x::ConfigWindowMask::HEIGHT) {
                dims.height
            } else {
                current_dims.height
            };
            let has_position_change =
                requested_x != current_dims.x || requested_y != current_dims.y;
            let has_size_change =
                requested_width != current_dims.width || requested_height != current_dims.height;

            if !win.mapped || win.attrs.is_popup || !has_position_change || has_size_change {
                return false;
            }

            if win.configure_move_serial == Some(serial) {
                return true;
            }

            win.configure_move_serial = Some(serial);
            true
        };

        if !should_move {
            return false;
        }

        self.move_window(window);
        true
    }

    pub fn reconfigure_window(&mut self, event: x::ConfigureNotifyEvent) {
        let Some((mut win, data)) = self
            .windows
            .get(&event.window())
            .copied()
            .and_then(|id| self.world.entity(id).ok())
            .and_then(|d| Some((d.get::<&mut WindowData>()?, d)))
        else {
            debug!("not reconfiguring unknown window {:?}", event.window());
            return;
        };

        let dims = WindowDims {
            x: event.x(),
            y: event.y(),
            width: event.width(),
            height: event.height(),
        };
        let previous_dims = win.attrs.dims;
        let had_deferred_initial_map = win.has_deferred_initial_toplevel_map();

        if had_deferred_initial_map {
            win.attrs.dims = dims;
        } else if dims == previous_dims {
            return;
        } else if win.attrs.is_popup {
            win.attrs.dims = dims;
        }

        debug!("Reconfiguring {:?} {:?}", event.window(), dims);

        if !win.mapped {
            win.attrs.dims = dims;
            return;
        }

        if had_deferred_initial_map {
            drop(win);
            self.release_deferred_initial_toplevel_map(event.window(), "post-map configure");
            return;
        }

        if self.xdg_wm_base.version() < 3 {
            return;
        }

        let mut query = data.query::<(&mut SurfaceRole, &SurfaceScaleFactor)>();
        let Some((role, scale_factor)) = query.get() else {
            return;
        };

        match role {
            SurfaceRole::Popup(Some(popup)) => {
                popup.positioner.set_offset(
                    ((event.x() as i32 - win.output_offset.x) as f64 / scale_factor.0) as i32,
                    ((event.y() as i32 - win.output_offset.y) as f64 / scale_factor.0) as i32,
                );
                popup.positioner.set_size(
                    1.max((event.width() as f64 / scale_factor.0) as i32),
                    1.max((event.height() as f64 / scale_factor.0) as i32),
                );
                popup.popup.reposition(&popup.positioner, 0);
            }
            SurfaceRole::Toplevel(Some(_)) => {
                if dims.width != win.attrs.dims.width || dims.height != win.attrs.dims.height {
                    win.configure_move_serial = None;
                }
                win.attrs.dims.width = dims.width;
                win.attrs.dims.height = dims.height;
                drop(query);
                drop(win);
                update_surface_viewport(
                    &self.world,
                    self.world.query_one(data.entity()).unwrap(),
                );
            }
            other => warn!("Non popup ({other:?}) being reconfigured, behavior may be off."),
        }
    }

    pub fn map_window(&mut self, window: x::Window) {
        debug!("mapping {window:?}");

        let Some(mut win) = self
            .windows
            .get(&window)
            .copied()
            .and_then(|id| self.world.entity(id).ok())
            .map(|data| data.get::<&mut WindowData>().unwrap())
        else {
            debug!("not mapping unknown window {window:?}");
            return;
        };

        win.mapped = true;
        drop(win);
        self.maybe_create_mapped_window_role(window);
    }

    pub fn unmap_window(&mut self, window: x::Window) {
        let entity = self.windows.get(&window).copied();

        {
            let Some(data) = entity.and_then(|id| self.world.entity(id).ok()) else {
                return;
            };

            let mut win = data.get::<&mut WindowData>().unwrap();
            if !win.mapped {
                return;
            }
            debug!("unmapping {window:?}");

            if matches!(self.last_focused_toplevel, Some(x) if x == window) {
                self.last_focused_toplevel.take();
            }
            if self.last_hovered == Some(window) {
                self.last_hovered.take();
            }
            win.mapped = false;
            win.initial_toplevel_map_state = InitialToplevelMapState::None;
        }

        if let Ok(mut role) = self.world.remove_one::<SurfaceRole>(entity.unwrap()) {
            role.destroy();
        }
    }

    pub fn set_fullscreen(&mut self, window: x::Window, state: super::xstate::SetState) {
        let Some(data) = self
            .windows
            .get(&window)
            .copied()
            .and_then(|id| self.world.entity(id).ok())
        else {
            warn!("Tried to set unknown window {window:?} fullscreen");
            return;
        };

        let Some(role) = data.get::<&SurfaceRole>() else {
            warn!("Tried to set window without role fullscreen: {window:?}");
            return;
        };

        let SurfaceRole::Toplevel(Some(toplevel)) = &*role else {
            warn!("Tried to set an unmapped toplevel or non toplevel fullscreen: {window:?}");
            return;
        };

        use crate::xstate::SetState;
        match state {
            SetState::Add => toplevel.toplevel.set_fullscreen(None),
            SetState::Remove => toplevel.toplevel.unset_fullscreen(),
            SetState::Toggle => {
                if toplevel.fullscreen {
                    toplevel.toplevel.unset_fullscreen()
                } else {
                    toplevel.toplevel.set_fullscreen(None)
                }
            }
        }
    }

    pub fn set_transient_for(&mut self, window: x::Window, parent: x::Window) {
        let Some(mut win) = self
            .windows
            .get(&window)
            .copied()
            .and_then(|id| self.world.entity(id).ok())
            .map(|data| data.get::<&mut WindowData>().unwrap())
        else {
            return;
        };

        win.attrs.transient_for = Some(parent);
    }

    pub fn activate_window(&mut self, window: x::Window) {
        let Some(activation_state) = self.activation_state.as_ref() else {
            return;
        };

        let Some(last_focused_toplevel) = self.last_focused_toplevel else {
            warn!("No last focused toplevel, cannot focus window {window:?}");
            return;
        };

        let Some(data) = self
            .windows
            .get(&last_focused_toplevel)
            .copied()
            .and_then(|id| self.world.entity(id).ok())
        else {
            warn!("Unknown last focused toplevel, cannot focus window {window:?}");
            return;
        };

        let Some(surface) = data.get::<&client::wl_surface::WlSurface>() else {
            warn!("Last focused toplevel has no surface, cannot focus window {window:?}");
            return;
        };
        activation_state.request_token_with_data(
            &self.qh,
            clientside::ActivationData::new(
                window,
                smithay_client_toolkit::activation::RequestData {
                    app_id: data.get::<&WindowData>().unwrap().attrs.class.clone(),
                    seat_and_serial: self.last_kb_serial.clone(),
                    surface: Some((*surface).clone()),
                },
            ),
        );
    }

    pub fn move_window(&mut self, window: x::Window) {
        let Some(data) = self
            .windows
            .get(&window)
            .copied()
            .and_then(|e| self.world.entity(e).ok())
        else {
            warn!("Requested move of unknown window {window:?}");
            return;
        };

        let last_click_data = data.get::<&LastClickSerial>();
        let role = data.get::<&SurfaceRole>();

        let Some(last_click_data) = last_click_data else {
            warn!("Requested move of window {window:?} but we don't have a click serial for it");
            return;
        };

        let Some(SurfaceRole::Toplevel(Some(data))) = role.as_deref() else {
            warn!("Requested move of non toplevel {window:?} ({role:?})");
            return;
        };

        data.toplevel._move(&last_click_data.0, last_click_data.1);
    }

    pub fn resize_window(&mut self, window: x::Window, direction: MoveResizeDirection) {
        let Some(data) = self
            .windows
            .get(&window)
            .copied()
            .and_then(|e| self.world.entity(e).ok())
        else {
            warn!("Requested resize of unknown window {window:?}");
            return;
        };

        let last_click_data = data.get::<&LastClickSerial>();
        let role = data.get::<&SurfaceRole>();

        let Some(last_click_data) = last_click_data else {
            warn!("Requested resize of window {window:?} but we don't have a click serial for it");
            return;
        };

        let Some(SurfaceRole::Toplevel(Some(data))) = role.as_deref() else {
            warn!("Requested resize of non toplevel {window:?} ({role:?})");
            return;
        };

        let edge = match direction {
            MoveResizeDirection::SizeTopLeft => xdg_toplevel::ResizeEdge::TopLeft,
            MoveResizeDirection::SizeTop => xdg_toplevel::ResizeEdge::Top,
            MoveResizeDirection::SizeTopRight => xdg_toplevel::ResizeEdge::TopRight,
            MoveResizeDirection::SizeRight => xdg_toplevel::ResizeEdge::Right,
            MoveResizeDirection::SizeBottomRight => xdg_toplevel::ResizeEdge::BottomRight,
            MoveResizeDirection::SizeBottom => xdg_toplevel::ResizeEdge::Bottom,
            MoveResizeDirection::SizeBottomLeft => xdg_toplevel::ResizeEdge::BottomLeft,
            MoveResizeDirection::SizeLeft => xdg_toplevel::ResizeEdge::Left,
            MoveResizeDirection::MoveKeyboard
            | MoveResizeDirection::SizeKeyboard
            | MoveResizeDirection::Move
            | MoveResizeDirection::Cancel => unreachable!(),
        };

        data.toplevel
            .resize(&last_click_data.0, last_click_data.1, edge);
    }

    pub fn destroy_window(&mut self, window: x::Window) {
        if let Some(id) = self.windows.remove(&window) {
            self.world.remove::<(x::Window, WindowData)>(id).unwrap();
            if self.world.entity(id).unwrap().is_empty() {
                self.world.despawn(id).unwrap();
            }
        }
    }

    pub fn new_global_scale(&mut self) -> Option<f64> {
        self.new_scale
            .take()
            .filter(|scale| (scale - self.published_x11_scale).abs() > 0.01)
    }

    fn handle_activations(&mut self) {
        let Some(activation_state) = self.activation_state.as_ref() else {
            return;
        };

        self.world.pending_activations.retain(|(window, token)| {
            if let Some(surface) = self.windows.get(window).copied().and_then(|id| {
                self.world
                    .world
                    .get::<&client::wl_surface::WlSurface>(id)
                    .ok()
            }) {
                activation_state.activate::<Self>(&surface, token.clone());
                return false;
            }
            true
        });
    }

    fn calc_global_output_offset(&mut self) {
        self.global_output_offset.x.value = i32::MAX;
        self.global_output_offset.y.value = i32::MAX;
        for (entity, dimensions) in self.world.query_mut::<&OutputDimensions>() {
            if dimensions.x < self.global_output_offset.x.value {
                self.global_output_offset.x = GlobalOutputOffsetDimension {
                    owner: Some(entity),
                    value: dimensions.x,
                }
            }
            if dimensions.y < self.global_output_offset.y.value {
                self.global_output_offset.y = GlobalOutputOffsetDimension {
                    owner: Some(entity),
                    value: dimensions.y,
                }
            }
        }
    }

    /// Returns true if the created window is a toplevel.
    fn create_role_window(&mut self, window: x::Window, entity: Entity) -> bool {
        let xdg_surface;
        let mut popup_for = None;
        let mut fullscreen = false;

        {
            let data = self.world.entity(entity).unwrap();
            let surface = data.get::<&client::wl_surface::WlSurface>().unwrap();
            surface.attach(None, 0, 0);
            surface.commit();

            xdg_surface = self.xdg_wm_base.get_xdg_surface(&surface, &self.qh, entity);

            let window_data = data.get::<&WindowData>().unwrap();
            if window_data.attrs.is_popup {
                popup_for = self.last_hovered.or(self.last_focused_toplevel);
            }

            let (width, height) = (window_data.attrs.dims.width, window_data.attrs.dims.height);
            for (_, dimensions) in self.world.query::<&OutputDimensions>().iter() {
                if dimensions.width == width as i32 && dimensions.height == height as i32 {
                    fullscreen = true;
                    popup_for = None;
                    break;
                }
            }
        }

        let (role, is_toplevel) = if let Some(parent) = popup_for {
            let data = self.create_popup(entity, xdg_surface, parent);
            (SurfaceRole::Popup(Some(data)), false)
        } else {
            let data = self.create_toplevel(entity, xdg_surface, fullscreen);
            (SurfaceRole::Toplevel(Some(data)), true)
        };

        let (surface_role, client) = self
            .world
            .query_one_mut::<(Option<&SurfaceRole>, &client::wl_surface::WlSurface)>(entity)
            .unwrap();

        let new_role_type = std::mem::discriminant(&role);
        if let Some(role) = surface_role {
            let old_role_type = std::mem::discriminant(role);
            assert_eq!(
                new_role_type, old_role_type,
                "Surface for {window:?} already had a role: {role:?}"
            );
        }

        client.commit();
        self.world.insert(entity, (role,)).unwrap();

        is_toplevel
    }

    fn create_toplevel(
        &mut self,
        entity: Entity,
        xdg: XdgSurface,
        fullscreen: bool,
    ) -> ToplevelData {
        let window = self.world.get::<&WindowData>(entity).unwrap();
        debug!(
            "creating toplevel for {:?} fullscreen: {fullscreen:?}",
            *self.world.get::<&x::Window>(entity).unwrap()
        );

        let toplevel = xdg.get_toplevel(&self.qh, entity);

        let group = window.attrs.group.and_then(|win| {
            let id = self.windows.get(&win).copied()?;
            Some(self.world.get::<&WindowData>(id).unwrap())
        });
        if let Some(class) = window
            .attrs
            .class
            .as_ref()
            .or(group.as_ref().and_then(|g| g.attrs.class.as_ref()))
        {
            toplevel.set_app_id(class.to_string());
        }
        if let Some(title) = window
            .attrs
            .title
            .as_ref()
            .or(group.as_ref().and_then(|g| g.attrs.title.as_ref()))
        {
            toplevel.set_title(title.name().to_string());
        }

        if fullscreen {
            toplevel.set_fullscreen(None);
        }

        let wl_decoration = self.decoration_manager.as_ref().map(|decoration_manager| {
            let decoration =
                decoration_manager.get_toplevel_decoration(&toplevel, &self.qh, entity);
            decoration.set_mode(
                window
                    .attrs
                    .decorations
                    .map_or(zxdg_toplevel_decoration_v1::Mode::ServerSide, From::from),
            );
            decoration
        });

        // X11 side wants server side decorations, but compositor won't provide them
        // so we provide our own

        let surface = self
            .world
            .get::<&client::wl_surface::WlSurface>(entity)
            .unwrap();
        let needs_satellite_decorations =
            wl_decoration.is_none() && window.attrs.decorations.is_none_or(|d| d.is_serverside());
        let (sat_decoration, buf) = needs_satellite_decorations
            .then(|| {
                DecorationsDataSatellite::try_new(
                    self,
                    &surface,
                    window.attrs.title.as_ref().map(WmName::name),
                )
            })
            .flatten()
            .unzip();

        if let (Some(activation_state), Some(token)) = (
            self.activation_state.as_ref(),
            window.activation_token.clone(),
        ) {
            activation_state.activate::<Self>(&surface, token);
        }

        if let Some(parent) = window.attrs.transient_for {
            // TODO: handle transient_for window not being mapped/not a toplevel
            'b: {
                let Some(parent_id) = self.windows.get(&parent).copied() else {
                    warn!(
                        "Window {:?} is marked transient for unknown window {:?}",
                        *self.world.get::<&x::Window>(entity).unwrap(),
                        parent
                    );
                    break 'b;
                };

                let role = self.world.get::<&SurfaceRole>(parent_id);
                let Ok(SurfaceRole::Toplevel(Some(parent_toplevel))) = role.as_deref() else {
                    warn!("Window {parent:?} was not an active toplevel, not setting as parent");
                    break 'b;
                };

                toplevel.set_parent(Some(&parent_toplevel.toplevel));
            }
        }

        drop(window);
        drop(group);
        drop(surface);
        if let Some(mut b) = buf.flatten() {
            b.run_on(&mut self.world);
        }

        if let Some(hints) = &self.world.get::<&WindowData>(entity).unwrap().attrs.size_hints {
            let decorations_height = sat_decoration
                .as_ref()
                .map_or(0, |_| DecorationsDataSatellite::TITLEBAR_HEIGHT);
            let hints_x11_scale = self
                .world
                .get::<&WindowData>(entity)
                .unwrap()
                .attrs
                .hints_x11_scale;
            apply_toplevel_size_hints(&toplevel, hints, hints_x11_scale, decorations_height);
        }

        ToplevelData {
            xdg: XdgSurfaceData {
                surface: xdg,
                configured: false,
                pending: None,
            },
            toplevel,
            fullscreen: false,
            decoration: DecorationsData {
                wl: wl_decoration,
                satellite: sat_decoration,
            },
        }
    }

    fn create_popup(&mut self, entity: Entity, xdg: XdgSurface, parent: x::Window) -> PopupData {
        let mut query = self
            .world
            .query_one::<(&WindowData, &mut SurfaceScaleFactor)>(entity)
            .unwrap();

        let (window, scale) = query.get().unwrap();
        let mut parent_query = self
            .world
            .query_one::<(&WindowData, &SurfaceScaleFactor, &SurfaceRole)>(self.windows[&parent])
            .unwrap();
        let (parent_window, parent_scale, parent_role) = parent_query.get().unwrap();
        let parent_dims = parent_window.attrs.dims;
        let initial_scale = parent_scale.0;
        *scale = *parent_scale;

        debug!(
            "creating popup ({:?}) {:?} {:?} {:?} {entity:?} (scale: {initial_scale})",
            *self.world.get::<&x::Window>(entity).unwrap(),
            parent,
            window.attrs.dims,
            xdg.id()
        );

        let positioner = self.xdg_wm_base.create_positioner(&self.qh, ());
        positioner.set_size(
            1.max((window.attrs.dims.width as f64 / initial_scale) as i32),
            1.max((window.attrs.dims.height as f64 / initial_scale) as i32),
        );
        let x = ((window.attrs.dims.x - parent_dims.x) as f64 / initial_scale) as i32;
        let y = ((window.attrs.dims.y - parent_dims.y) as f64 / initial_scale) as i32;
        positioner.set_offset(x, y);
        positioner.set_anchor(Anchor::TopLeft);
        positioner.set_gravity(Gravity::BottomRight);
        positioner.set_anchor_rect(
            0,
            0,
            (parent_window.attrs.dims.width as f64 / initial_scale) as i32,
            (parent_window.attrs.dims.height as f64 / initial_scale) as i32,
        );
        positioner
            .set_constraint_adjustment(ConstraintAdjustment::SlideX | ConstraintAdjustment::SlideY);
        let popup = xdg.get_popup(
            Some(&parent_role.xdg().unwrap().surface),
            &positioner,
            &self.qh,
            entity,
        );

        PopupData {
            popup,
            positioner,
            xdg: XdgSurfaceData {
                surface: xdg,
                configured: false,
                pending: None,
            },
        }
    }
}

#[derive(Default, Debug, Copy, Clone)]
pub struct PendingSurfaceState {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}
