mod clientside;
mod decoration;
mod dispatch;
mod event;
pub(crate) mod selection;
#[cfg(test)]
mod tests;

use self::event::*;
use crate::xstate::{Decorations, MoveResizeDirection, WindowDims, WmHints, WmName, WmNormalHints};
use crate::{timespec_from_millis, X11Selection, XConnection};
use clientside::MyWorld;
use decoration::{DecorationsData, DecorationsDataSatellite};
use hecs::Entity;
use log::{debug, warn};
use rustix::event::{poll, PollFd, PollFlags};
use smithay_client_toolkit::activation::ActivationState;
use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use wayland_client::protocol::wl_subcompositor::WlSubcompositor;
use wayland_client::{
    globals::{registry_queue_init, Global},
    protocol as client, Connection, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols::xdg::decoration::zv1::client::zxdg_decoration_manager_v1::ZxdgDecorationManagerV1;
use wayland_protocols::xdg::decoration::zv1::client::zxdg_toplevel_decoration_v1::{self};
use wayland_protocols::xdg::shell::client::xdg_positioner::ConstraintAdjustment;
use wayland_protocols::{
    wp::{
        fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        linux_dmabuf::zv1::{client as c_dmabuf, server as s_dmabuf},
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
    protocol::{
        wl_callback::WlCallback, wl_compositor::WlCompositor, wl_output::WlOutput, wl_shm::WlShm,
        wl_surface::WlSurface,
    },
    Client, DisplayHandle, Resource, WEnum,
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

#[derive(Default, Debug)]
struct WindowAttributes {
    is_popup: bool,
    dims: WindowDims,
    size_hints: Option<WmNormalHints>,
    title: Option<WmName>,
    class: Option<String>,
    group: Option<x::Window>,
    decorations: Option<Decorations>,
    transient_for: Option<x::Window>,
}

#[derive(Debug, Default, PartialEq, Eq, Copy, Clone)]
struct WindowOutputOffset {
    x: i32,
    y: i32,
}

#[derive(Debug)]
struct WindowData {
    mapped: bool,
    attrs: WindowAttributes,
    output_offset: WindowOutputOffset,
    activation_token: Option<String>,
}

impl WindowData {
    fn new(override_redirect: bool, dims: WindowDims, activation_token: Option<String>) -> Self {
        Self {
            mapped: false,
            attrs: WindowAttributes {
                is_popup: override_redirect,
                dims,
                ..Default::default()
            },
            output_offset: WindowOutputOffset::default(),
            activation_token,
        }
    }

    fn update_output_offset<C: XConnection>(
        &mut self,
        window: x::Window,
        offset: WindowOutputOffset,
        connection: &mut C,
    ) {
        log::trace!("offset: {offset:?}");
        if offset == self.output_offset {
            return;
        }

        let dims = &mut self.attrs.dims;
        dims.x += (offset.x - self.output_offset.x) as i16;
        dims.y += (offset.y - self.output_offset.y) as i16;
        self.output_offset = offset;

        if connection.set_window_dims(
            window,
            PendingSurfaceState {
                x: dims.x as i32,
                y: dims.y as i32,
                width: self.attrs.dims.width as _,
                height: self.attrs.dims.height as _,
            },
        ) {
            debug!("set {:?} offset to {:?}", window, self.output_offset);
        }
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
            SurfaceRole::Toplevel(ref t) => t.as_ref().map(|t| &t.xdg),
            SurfaceRole::Popup(ref p) => p.as_ref().map(|p| &p.xdg),
        }
    }

    fn xdg_mut(&mut self) -> Option<&mut XdgSurfaceData> {
        match self {
            SurfaceRole::Toplevel(ref mut t) => t.as_mut().map(|t| &mut t.xdg),
            SurfaceRole::Popup(ref mut p) => p.as_mut().map(|p| &mut p.xdg),
        }
    }

    fn destroy(&mut self) {
        match self {
            SurfaceRole::Toplevel(Some(ref mut t)) => {
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

fn handle_globals<'a, S: X11Selection + 'static>(
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
                            dh.create_global::<InnerServerState<S>, $global, Global>(version, global.clone());
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
            ZwpTabletManagerV2
        ];
    }
}

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
            warn!("xdg_wm_base version 2 detected. Popup repositioning will not work, and some popups may not work correctly.");
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

        let fractional_scale = global_list.bind::<WpFractionalScaleManagerV1, _, _>(&qh, 1..=1, ())
            .inspect_err(|e| warn!("Couldn't bind fractional scale manager: {e}. Fractional scaling will not work."))
            .ok();

        let activation_state = ActivationState::bind(&global_list, &qh)
            .inspect_err(|e| {
                warn!("Could not bind xdg activation ({e:?}). Windows might not receive focus depending on compositor focus stealing policy.")
            })
            .ok();

        let decoration_manager = global_list
            .bind::<ZxdgDecorationManagerV1, _, _>(&qh, 1..=1, ())
            .inspect_err(|e| {
                warn!("Could not bind xdg decoration ({e:?}). Windows might not have decorations.")
            })
            .ok();

        let selection_states = selection::SelectionStates::new(&global_list, &qh);

        dh.create_global::<InnerServerState<S>, XwaylandShellV1, _>(1, ());

        global_list
            .contents()
            .with_list(|globals| handle_globals::<S>(&dh, globals));

        let world = MyWorld::new(global_list);
        let client = dh.insert_client(client, std::sync::Arc::new(())).unwrap();

        let inner = InnerServerState {
            windows: HashMap::new(),
            pids: HashSet::new(),
            client,
            queue,
            qh,
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

    pub fn handle_clientside_events(&mut self) {
        self.handle_new_globals();

        for (target, event) in self.world.read_events() {
            if !self.world.contains(target) {
                warn!("could not handle clientside event: stale object");
                continue;
            }
            event.handle(target, self);
        }

        if self.global_offset_updated {
            if self.global_output_offset.x.owner.is_none()
                || self.global_output_offset.y.owner.is_none()
            {
                self.calc_global_output_offset();
            }

            debug!(
                "updated global output offset: {}x{}",
                self.global_output_offset.x.value, self.global_output_offset.y.value
            );
            let state = &self.inner;
            for (e, _) in state.world.query::<&WlOutput>().iter() {
                event::update_global_output_offset(
                    e,
                    &state.global_output_offset,
                    &state.world,
                    &mut self.connection,
                );
            }
            self.global_offset_updated = false;
        }

        if !self.updated_outputs.is_empty() {
            for output in self.updated_outputs.iter() {
                let output_scale = self.world.get::<&OutputScaleFactor>(*output).unwrap();
                if matches!(*output_scale, OutputScaleFactor::Output(..)) {
                    let mut surface_query = self
                        .world
                        .query::<(&OnOutput, &mut SurfaceScaleFactor)>()
                        .with::<(&WindowData, &WlSurface)>();

                    let mut surfaces = vec![];
                    for (surface, (OnOutput(s_output), surface_scale)) in surface_query.iter() {
                        if s_output == output {
                            surface_scale.0 = output_scale.get();
                            surfaces.push(surface);
                        }
                    }

                    drop(surface_query);
                    for surface in surfaces {
                        update_surface_viewport(
                            &self.world,
                            self.world.query_one(surface).unwrap(),
                        );
                    }
                }
            }
            self.updated_outputs.clear();

            let mut mixed_scale = false;
            let mut scale;

            let mut outputs = self.world.query_mut::<&OutputScaleFactor>().into_iter();
            let (_, output_scale) = outputs.next().unwrap();

            scale = output_scale.get();

            for (_, output_scale) in outputs {
                if output_scale.get() != scale {
                    mixed_scale = true;
                    scale = scale.min(output_scale.get());
                }
            }

            if mixed_scale {
                warn!("Mixed output scales detected, choosing to give apps the smallest detected scale ({scale}x)");
            }

            debug!("Using new scale {scale}");
            self.new_scale = Some(scale);
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
        self.queue
            .flush()
            .expect("Failed flushing clientside events");
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

    fn handle_new_globals(&mut self) {
        let globals = std::mem::take(&mut self.world.new_globals);
        handle_globals::<S>(&self.dh, globals.iter());
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
                    debug!("skipping setting window name to {name:?} because a _NET_WM_NAME title is already set");
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

        if win.attrs.size_hints.is_none() || *win.attrs.size_hints.as_ref().unwrap() != hints {
            debug!("setting {window:?} hints {hints:?}");
            let mut query = data.query::<(&SurfaceRole, &SurfaceScaleFactor)>();
            if let Some((SurfaceRole::Toplevel(Some(data)), scale_factor)) = query.get() {
                if let Some(min_size) = &hints.min_size {
                    data.toplevel.set_min_size(
                        (min_size.width as f64 / scale_factor.0) as i32,
                        (min_size.height as f64 / scale_factor.0) as i32,
                    );
                }
                if let Some(max_size) = &hints.max_size {
                    data.toplevel.set_max_size(
                        (max_size.width as f64 / scale_factor.0) as i32,
                        (max_size.height as f64 / scale_factor.0) as i32,
                    );
                }
            }
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

    pub fn can_change_position(&self, window: x::Window) -> bool {
        let Some(win) = self
            .windows
            .get(&window)
            .copied()
            .and_then(|id| self.world.entity(id).ok())
            .map(|data| data.get::<&WindowData>().unwrap())
        else {
            return true;
        };

        !win.mapped || win.attrs.is_popup
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
        if dims == win.attrs.dims {
            return;
        } else if win.attrs.is_popup {
            win.attrs.dims = dims;
        }

        debug!("Reconfiguring {:?} {:?}", event.window(), dims);

        if !win.mapped {
            win.attrs.dims = dims;
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
                win.attrs.dims.width = dims.width;
                win.attrs.dims.height = dims.height;
                drop(query);
                drop(win);
                update_surface_viewport(&self.world, self.world.query_one(data.entity()).unwrap());
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

        let SurfaceRole::Toplevel(Some(ref toplevel)) = &*role else {
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

        let Some(last_click_data) = data.get::<&LastClickSerial>() else {
            warn!("Requested move of window {window:?} but we don't have a click serial for it");
            return;
        };

        let role = data.get::<&SurfaceRole>();
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

        let Some(last_click_data) = data.get::<&LastClickSerial>() else {
            warn!("Requested resize of window {window:?} but we don't have a click serial for it");
            return;
        };

        let role = data.get::<&SurfaceRole>();
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
        self.new_scale.take()
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
        if let Some(hints) = &window.attrs.size_hints {
            if let Some(min) = &hints.min_size {
                toplevel.set_min_size(min.width, min.height);
            }
            if let Some(max) = &hints.max_size {
                toplevel.set_max_size(max.width, max.height);
            }
        }

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
        let needs_satellite_decorations = wl_decoration.is_none()
            && window
                .attrs
                .decorations
                .is_none_or(|d| d == Decorations::Server);
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

#[derive(Default, Debug)]
pub struct PendingSurfaceState {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}
