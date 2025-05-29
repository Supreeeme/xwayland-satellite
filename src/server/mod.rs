mod dispatch;
mod event;

#[cfg(test)]
mod tests;

use self::event::*;
use crate::clientside::*;
use crate::xstate::{Decorations, WindowDims, WmHints, WmName, WmNormalHints};
use crate::{X11Selection, XConnection};
use log::{debug, warn};
use rustix::event::{poll, PollFd, PollFlags};
use slotmap::{new_key_type, HopSlotMap, SparseSecondaryMap};
use smithay_client_toolkit::activation::ActivationState;
use smithay_client_toolkit::data_device_manager::{
    data_device::DataDevice, data_offer::SelectionOffer, data_source::CopyPasteSource,
    DataDeviceManagerState,
};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::rc::{Rc, Weak};
use wayland_client::{globals::Global, protocol as client, Proxy};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::WpFractionalScaleV1;
use wayland_protocols::xdg::decoration::zv1::client::zxdg_decoration_manager_v1::ZxdgDecorationManagerV1;
use wayland_protocols::xdg::decoration::zv1::client::zxdg_toplevel_decoration_v1::{
    self, ZxdgToplevelDecorationV1,
};
use wayland_protocols::{
    wp::{
        fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        linux_dmabuf::zv1::{client as c_dmabuf, server as s_dmabuf},
        pointer_constraints::zv1::server::zwp_pointer_constraints_v1::ZwpPointerConstraintsV1,
        relative_pointer::zv1::server::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
        tablet::zv2::server::zwp_tablet_manager_v2::ZwpTabletManagerV2,
        viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
    },
    xdg::{
        shell::client::{
            xdg_popup::XdgPopup,
            xdg_positioner::{Anchor, Gravity, XdgPositioner},
            xdg_surface::XdgSurface,
            xdg_toplevel::XdgToplevel,
            xdg_wm_base::XdgWmBase,
        },
        xdg_output::zv1::server::zxdg_output_manager_v1::ZxdgOutputManagerV1,
    },
    xwayland::shell::v1::server::{
        xwayland_shell_v1::XwaylandShellV1, xwayland_surface_v1::XwaylandSurfaceV1,
    },
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
    window: x::Window,
    surface_serial: Option<[u32; 2]>,
    surface_key: Option<ObjectKey>,
    mapped: bool,
    attrs: WindowAttributes,
    output_offset: WindowOutputOffset,
    output_key: Option<ObjectKey>,
    activation_token: Option<String>,
}

impl WindowData {
    fn new(
        window: x::Window,
        override_redirect: bool,
        dims: WindowDims,
        activation_token: Option<String>,
    ) -> Self {
        Self {
            window,
            surface_key: None,
            surface_serial: None,
            mapped: false,
            attrs: WindowAttributes {
                is_popup: override_redirect,
                dims,
                ..Default::default()
            },
            output_offset: WindowOutputOffset::default(),
            output_key: None,
            activation_token,
        }
    }

    fn update_output_offset<C: XConnection>(
        &mut self,
        output_key: ObjectKey,
        offset: WindowOutputOffset,
        connection: &mut C,
    ) {
        log::trace!("offset: {offset:?}");
        if self.output_key != Some(output_key) {
            self.output_key = Some(output_key);
        }

        if offset == self.output_offset {
            return;
        }

        let dims = &mut self.attrs.dims;
        dims.x += (offset.x - self.output_offset.x) as i16;
        dims.y += (offset.y - self.output_offset.y) as i16;
        self.output_offset = offset;

        connection.set_window_dims(
            self.window,
            PendingSurfaceState {
                x: dims.x as i32,
                y: dims.y as i32,
                width: self.attrs.dims.width as _,
                height: self.attrs.dims.height as _,
            },
        );

        debug!("set {:?} offset to {:?}", self.window, self.output_offset);
    }
}

struct SurfaceAttach {
    buffer: Option<client::wl_buffer::WlBuffer>,
    x: i32,
    y: i32,
}

pub struct SurfaceData {
    client: client::wl_surface::WlSurface,
    server: WlSurface,
    key: ObjectKey,
    serial: Option<[u32; 2]>,
    frame_callback: Option<WlCallback>,
    attach: Option<SurfaceAttach>,
    role: Option<SurfaceRole>,
    xwl: Option<XwaylandSurfaceV1>,
    window: Option<x::Window>,
    output_key: Option<ObjectKey>,
    scale_factor: f64,
    viewport: WpViewport,
    fractional: Option<WpFractionalScaleV1>,
}

impl SurfaceData {
    fn xdg(&self) -> Option<&XdgSurfaceData> {
        match self
            .role
            .as_ref()
            .expect("Tried to get XdgSurface for surface without role")
        {
            SurfaceRole::Toplevel(ref t) => t.as_ref().map(|t| &t.xdg),
            SurfaceRole::Popup(ref p) => p.as_ref().map(|p| &p.xdg),
        }
    }

    fn xdg_mut(&mut self) -> Option<&mut XdgSurfaceData> {
        match self
            .role
            .as_mut()
            .expect("Tried to get XdgSurface for surface without role")
        {
            SurfaceRole::Toplevel(ref mut t) => t.as_mut().map(|t| &mut t.xdg),
            SurfaceRole::Popup(ref mut p) => p.as_mut().map(|p| &mut p.xdg),
        }
    }

    fn destroy_role(&mut self) {
        if let Some(role) = self.role.take() {
            match role {
                SurfaceRole::Toplevel(Some(mut t)) => {
                    if let Some(decoration) = t.decoration.take() {
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
}

#[derive(Debug)]
enum SurfaceRole {
    Toplevel(Option<ToplevelData>),
    Popup(Option<PopupData>),
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
    decoration: Option<ZxdgToplevelDecorationV1>,
}

#[derive(Debug)]
struct PopupData {
    popup: XdgPopup,
    positioner: XdgPositioner,
    xdg: XdgSurfaceData,
}

pub(crate) trait HandleEvent {
    type Event;
    fn handle_event<C: XConnection>(&mut self, event: Self::Event, state: &mut ServerState<C>);
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

/// Implement HandleEvent for our enum
macro_rules! handle_event_enum {
    (
        $(#[$meta:meta])*
        $pub:vis enum $name:ident {
            $( $variant:ident($ty:ty) ),+
        } => $name_event:ident
    ) => {
        enum_try_from! {
            $(#[$meta])*
            $pub enum $name {
                $( $variant($ty) ),+
            }
        }

        enum_try_from! {
            #[derive(Debug)]
            $pub enum $name_event {
                $( $variant(<$ty as HandleEvent>::Event) ),+
            }
        }

        impl HandleEvent for $name {
            type Event = $name_event;

            fn handle_event<C: XConnection>(&mut self, event: Self::Event, state: &mut ServerState<C>) {
                match self {
                    $(
                        Self::$variant(v) => {
                            let Self::Event::$variant(event) = event else {
                                unreachable!();
                            };
                            v.handle_event(event, state)
                        }
                    ),+
                }
            }
        }
    }
}

handle_event_enum! {

/// Objects that generate client side events that we will have to process.
pub(crate) enum Object {
    Surface(SurfaceData),
    Buffer(Buffer),
    Seat(Seat),
    Pointer(Pointer),
    Keyboard(Keyboard),
    Output(Output),
    RelativePointer(RelativePointer),
    DmabufFeedback(DmabufFeedback),
    Drm(Drm),
    Touch(Touch),
    ConfinedPointer(ConfinedPointer),
    LockedPointer(LockedPointer),
    TabletSeat(TabletSeat),
    Tablet(Tablet),
    TabletTool(TabletTool),
    TabletPad(TabletPad),
    TabletPadGroup(TabletPadGroup),
    TabletPadRing(TabletPadRing),
    TabletPadStrip(TabletPadStrip)
} => ObjectEvent

}

#[derive(Default)]
pub(crate) struct WrappedObject(Option<Object>);

impl<T> From<T> for WrappedObject
where
    T: Into<Object>,
{
    fn from(value: T) -> Self {
        Self(Some(value.into()))
    }
}

impl<T> AsRef<T> for WrappedObject
where
    for<'a> &'a T: TryFrom<&'a Object, Error = String>,
{
    fn as_ref(&self) -> &T {
        <&T>::try_from(self.0.as_ref().unwrap()).unwrap()
    }
}

impl<T> AsMut<T> for WrappedObject
where
    for<'a> &'a mut T: TryFrom<&'a mut Object, Error = String>,
{
    fn as_mut(&mut self) -> &mut T {
        <&mut T>::try_from(self.0.as_mut().unwrap()).unwrap()
    }
}

type ObjectMap = HopSlotMap<ObjectKey, WrappedObject>;
trait ObjectMapExt {
    fn insert_from_other_objects<F, const N: usize>(&mut self, keys: [ObjectKey; N], insert_fn: F)
    where
        F: FnOnce([&Object; N], ObjectKey) -> Object;
}

impl ObjectMapExt for ObjectMap {
    /// Insert an object into our map that needs some other values from our map as well
    fn insert_from_other_objects<F, const N: usize>(&mut self, keys: [ObjectKey; N], insert_fn: F)
    where
        F: FnOnce([&Object; N], ObjectKey) -> Object,
    {
        let objects = keys.each_ref().map(|key| self[*key].0.take().unwrap());
        let key = self.insert(WrappedObject(None));
        let obj = insert_fn(objects.each_ref(), key);
        let ret = self[key].0.replace(obj);
        debug_assert!(ret.is_none());
        for (object, key) in objects.into_iter().zip(keys.into_iter()) {
            let ret = self[key].0.replace(object);
            debug_assert!(ret.is_none());
        }
    }
}

fn handle_globals<'a, C: XConnection>(
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
                                dh.create_global::<ServerState<C>, $global, Global>(version, global.clone());
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

new_key_type! {
    pub struct ObjectKey;
}

struct FocusData {
    window: x::Window,
    output_name: Option<String>,
}

#[derive(Copy, Clone, Default)]
struct GlobalOutputOffsetDimension {
    owner: Option<ObjectKey>,
    value: i32,
}

#[derive(Copy, Clone)]
struct GlobalOutputOffset {
    x: GlobalOutputOffsetDimension,
    y: GlobalOutputOffsetDimension,
}

pub struct ServerState<C: XConnection> {
    dh: DisplayHandle,
    clientside: ClientState,
    objects: ObjectMap,
    associated_windows: SparseSecondaryMap<ObjectKey, x::Window>,
    output_keys: SparseSecondaryMap<ObjectKey, ()>,
    windows: HashMap<x::Window, WindowData>,
    pids: HashSet<u32>,

    qh: ClientQueueHandle,
    client: Option<Client>,
    to_focus: Option<FocusData>,
    unfocus: bool,
    last_focused_toplevel: Option<x::Window>,
    last_hovered: Option<x::Window>,
    pub connection: Option<C>,

    xdg_wm_base: XdgWmBase,
    viewporter: WpViewporter,
    fractional_scale: Option<WpFractionalScaleManagerV1>,
    clipboard_data: Option<ClipboardData<C::X11Selection>>,
    last_kb_serial: Option<(client::wl_seat::WlSeat, u32)>,
    activation_state: Option<ActivationState>,
    global_output_offset: GlobalOutputOffset,
    global_offset_updated: bool,
    output_scales_updated: bool,
    new_scale: Option<i32>,
    decoration_manager: Option<ZxdgDecorationManagerV1>,
}

impl<C: XConnection> ServerState<C> {
    pub fn new(dh: DisplayHandle, server_connection: Option<UnixStream>) -> Self {
        let clientside = ClientState::new(server_connection);
        let qh = clientside.qh.clone();

        let xdg_wm_base = clientside
            .global_list
            .bind::<XdgWmBase, _, _>(&qh, 2..=6, ())
            .expect("Could not bind xdg_wm_base");

        if xdg_wm_base.version() < 3 {
            warn!("xdg_wm_base version 2 detected. Popup repositioning will not work, and some popups may not work correctly.");
        }

        let viewporter = clientside
            .global_list
            .bind::<WpViewporter, _, _>(&qh, 1..=1, ())
            .expect("Could not bind wp_viewporter");

        let fractional_scale = clientside.global_list.bind::<WpFractionalScaleManagerV1, _, _>(&qh, 1..=1, ())
            .inspect_err(|e| warn!("Couldn't bind fractional scale manager: {e}. Fractional scaling will not work."))
            .ok();

        let manager = DataDeviceManagerState::bind(&clientside.global_list, &qh)
            .inspect_err(|e| {
                warn!("Could not bind data device manager ({e:?}). Clipboard will not work.")
            })
            .ok();
        let clipboard_data = manager.map(|manager| ClipboardData {
            manager,
            device: None,
            source: None::<CopyPasteData<C::X11Selection>>,
        });

        let activation_state = ActivationState::bind(&clientside.global_list, &qh)
            .inspect_err(|e| {
                warn!("Could not bind xdg activation ({e:?}). Windows might not receive focus depending on compositor focus stealing policy.")
            })
            .ok();

        let decoration_manager = clientside
            .global_list
            .bind::<ZxdgDecorationManagerV1, _, _>(&qh, 1..=1, ())
            .inspect_err(|e| {
                warn!("Could not bind xdg decoration ({e:?}). Windows might not have decorations.")
            })
            .ok();

        dh.create_global::<Self, XwaylandShellV1, _>(1, ());
        clientside
            .global_list
            .contents()
            .with_list(|globals| handle_globals::<C>(&dh, globals));

        Self {
            windows: HashMap::new(),
            pids: HashSet::new(),
            clientside,
            client: None,
            qh,
            dh,
            to_focus: None,
            unfocus: false,
            last_focused_toplevel: None,
            last_hovered: None,
            connection: None,
            objects: Default::default(),
            output_keys: Default::default(),
            associated_windows: Default::default(),
            xdg_wm_base,
            viewporter,
            fractional_scale,
            clipboard_data,
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
            output_scales_updated: false,
            new_scale: None,
            decoration_manager,
        }
    }

    pub fn clientside_fd(&self) -> BorrowedFd<'_> {
        self.clientside.queue.as_fd()
    }

    pub fn connect(&mut self, connection: UnixStream) {
        self.client = Some(
            self.dh
                .insert_client(connection, std::sync::Arc::new(()))
                .unwrap(),
        );
    }

    pub fn set_x_connection(&mut self, connection: C) {
        self.connection = Some(connection);
    }

    fn handle_new_globals(&mut self) {
        let globals = std::mem::take(&mut self.clientside.globals.new_globals);
        handle_globals::<C>(&self.dh, globals.iter());
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
        self.windows.insert(
            window,
            WindowData::new(window, override_redirect, dims, activation_token),
        );
    }

    pub fn set_popup(&mut self, window: x::Window, is_popup: bool) {
        let Some(win) = self.windows.get_mut(&window) else {
            debug!("not setting popup for unknown window {window:?}");
            return;
        };

        win.attrs.is_popup = is_popup;
    }

    pub fn set_win_title(&mut self, window: x::Window, name: WmName) {
        let Some(win) = self.windows.get_mut(&window) else {
            debug!("not setting title for unknown window {window:?}");
            return;
        };

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
        if let Some(key) = win.surface_key {
            if let Some(object) = self.objects.get(key) {
                let surface: &SurfaceData = object.as_ref();
                if let Some(SurfaceRole::Toplevel(Some(data))) = &surface.role {
                    data.toplevel.set_title(title.name().to_string());
                }
            } else {
                warn!("could not set window title: stale surface");
            }
        }
    }

    pub fn set_win_class(&mut self, window: x::Window, class: String) {
        let Some(win) = self.windows.get_mut(&window) else {
            debug!("not setting class for unknown window {window:?}");
            return;
        };

        let class = win.attrs.class.insert(class);
        if let Some(key) = win.surface_key {
            if let Some(object) = self.objects.get(key) {
                let surface: &SurfaceData = object.as_ref();
                if let Some(SurfaceRole::Toplevel(Some(data))) = &surface.role {
                    data.toplevel.set_app_id(class.to_string());
                }
            } else {
                warn!("could not set window class: stale surface");
            }
        }
    }

    pub fn set_win_hints(&mut self, window: x::Window, hints: WmHints) {
        let Some(win) = self.windows.get_mut(&window) else {
            debug!("not setting hints for unknown window {window:?}");
            return;
        };
        win.attrs.group = hints.window_group;
    }

    pub fn set_size_hints(&mut self, window: x::Window, hints: WmNormalHints) {
        let Some(win) = self.windows.get_mut(&window) else {
            debug!("not setting size hints for unknown window {window:?}");
            return;
        };

        if win.attrs.size_hints.is_none() || *win.attrs.size_hints.as_ref().unwrap() != hints {
            debug!("setting {window:?} hints {hints:?}");
            if let Some(key) = win.surface_key {
                if let Some(object) = self.objects.get(key) {
                    let surface: &SurfaceData = object.as_ref();
                    if let Some(SurfaceRole::Toplevel(Some(data))) = &surface.role {
                        if let Some(min_size) = &hints.min_size {
                            data.toplevel.set_min_size(
                                (min_size.width as f64 / surface.scale_factor) as i32,
                                (min_size.height as f64 / surface.scale_factor) as i32,
                            );
                        }
                        if let Some(max_size) = &hints.max_size {
                            data.toplevel.set_max_size(
                                (max_size.width as f64 / surface.scale_factor) as i32,
                                (max_size.height as f64 / surface.scale_factor) as i32,
                            );
                        }
                    }
                } else {
                    warn!("could not set size hint on {window:?}: stale surface")
                }
            }
            win.attrs.size_hints = Some(hints);
        }
    }

    pub fn set_win_decorations(&mut self, window: x::Window, decorations: Decorations) {
        if self.decoration_manager.is_none() {
            return;
        };

        let Some(win) = self.windows.get_mut(&window) else {
            debug!("not setting decorations for unknown window {window:?}");
            return;
        };

        if win.attrs.decorations != Some(decorations) {
            debug!("setting {window:?} decorations {decorations:?}");
            if let Some(key) = win.surface_key {
                if let Some(object) = self.objects.get(key) {
                    let surface: &SurfaceData = object.as_ref();
                    if let Some(SurfaceRole::Toplevel(Some(data))) = &surface.role {
                        data.decoration
                            .as_ref()
                            .unwrap()
                            .set_mode(decorations.into());
                    }
                } else {
                    warn!("could not set decorations on {window:?}: stale surface")
                }
            }
            win.attrs.decorations = Some(decorations);
        }
    }

    pub fn set_window_serial(&mut self, window: x::Window, serial: [u32; 2]) {
        let Some(win) = self.windows.get_mut(&window) else {
            warn!("Tried to set serial for unknown window {window:?}");
            return;
        };
        win.surface_serial = Some(serial);
    }

    pub fn can_change_position(&self, window: x::Window) -> bool {
        let Some(win) = self.windows.get(&window) else {
            return true;
        };

        !win.mapped || win.attrs.is_popup
    }

    pub fn reconfigure_window(&mut self, event: x::ConfigureNotifyEvent) {
        let Some(win) = self.windows.get_mut(&event.window()) else {
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
        }
        debug!("Reconfiguring {win:?} {:?}", dims);
        if !win.mapped {
            win.attrs.dims = dims;
            return;
        }

        if self.xdg_wm_base.version() < 3 {
            return;
        }

        let Some(key) = win.surface_key else {
            return;
        };

        let Some(data): Option<&mut SurfaceData> = self.objects.get_mut(key).map(|o| o.as_mut())
        else {
            return;
        };

        match &data.role {
            Some(SurfaceRole::Popup(Some(popup))) => {
                popup.positioner.set_offset(
                    ((event.x() as i32 - win.output_offset.x) as f64 / data.scale_factor) as i32,
                    ((event.y() as i32 - win.output_offset.y) as f64 / data.scale_factor) as i32,
                );
                popup.positioner.set_size(
                    1.max((event.width() as f64 / data.scale_factor) as i32),
                    1.max((event.height() as f64 / data.scale_factor) as i32),
                );
                popup.popup.reposition(&popup.positioner, 0);
            }
            Some(SurfaceRole::Toplevel(Some(_))) => {
                win.attrs.dims.width = dims.width;
                win.attrs.dims.height = dims.height;
                data.update_viewport(win.attrs.dims, win.attrs.size_hints);
            }
            other => warn!("Non popup ({other:?}) being reconfigured, behavior may be off."),
        }
    }

    pub fn map_window(&mut self, window: x::Window) {
        debug!("mapping {window:?}");

        let Some(window) = self.windows.get_mut(&window) else {
            debug!("not mapping unknown window {window:?}");
            return;
        };
        window.mapped = true;
    }

    pub fn unmap_window(&mut self, window: x::Window) {
        let Some(win) = self.windows.get_mut(&window) else {
            return;
        };
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

        if let Some(key) = win.surface_key.take() {
            let Some(object) = self.objects.get_mut(key) else {
                warn!("could not unmap {window:?}: stale surface");
                return;
            };
            let surface: &mut SurfaceData = object.as_mut();
            surface.destroy_role();
        }
    }

    pub fn set_fullscreen(&mut self, window: x::Window, state: super::xstate::SetState) {
        let Some(win) = self.windows.get(&window) else {
            warn!("Tried to set unknown window {window:?} fullscreen");
            return;
        };
        let Some(key) = win.surface_key else {
            warn!("Tried to set window without surface fullscreen: {window:?}");
            return;
        };
        let Some(object) = self.objects.get_mut(key) else {
            warn!("Could not set fullscreen on {window:?}: stale surface");
            return;
        };
        let surface: &mut SurfaceData = object.as_mut();
        let Some(SurfaceRole::Toplevel(Some(ref toplevel))) = surface.role else {
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
        let Some(win) = self.windows.get_mut(&window) else {
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
        let Some(win) = self.windows.get(&last_focused_toplevel) else {
            warn!("Unknown last focused toplevel, cannot focus window {window:?}");
            return;
        };
        let Some(key) = win.surface_key else {
            warn!("Last focused toplevel has no surface, cannot focus window {window:?}");
            return;
        };
        let Some(object) = self.objects.get_mut(key) else {
            warn!("Last focused toplevel has stale reference, cannot focus window {window:?}");
            return;
        };
        let surface: &mut SurfaceData = object.as_mut();
        activation_state.request_token_with_data(
            &self.qh,
            xdg_activation::ActivationData::new(
                window,
                smithay_client_toolkit::activation::RequestData {
                    app_id: win.attrs.class.clone(),
                    seat_and_serial: self.last_kb_serial.clone(),
                    surface: Some(surface.client.clone()),
                },
            ),
        );
    }

    pub fn destroy_window(&mut self, window: x::Window) {
        let _ = self.windows.remove(&window);
    }

    pub(crate) fn set_copy_paste_source(&mut self, selection: &Rc<C::X11Selection>) {
        if let Some(d) = &mut self.clipboard_data {
            let src = d
                .manager
                .create_copy_paste_source(&self.qh, selection.mime_types());
            let data = CopyPasteData::X11 {
                inner: src,
                data: Rc::downgrade(selection),
            };
            let CopyPasteData::X11 { inner, .. } = d.source.insert(data) else {
                unreachable!();
            };
            if let Some(serial) = self
                .last_kb_serial
                .as_ref()
                .map(|(_seat, serial)| serial)
                .copied()
            {
                inner.set_selection(d.device.as_ref().unwrap(), serial);
            }
        }
    }

    pub fn run(&mut self) {
        if let Some(r) = self.clientside.queue.prepare_read() {
            let fd = r.connection_fd();
            let pollfd = PollFd::new(&fd, PollFlags::IN);
            if poll(&mut [pollfd], 0).unwrap() > 0 {
                let _ = r.read();
            }
        }
        self.clientside
            .queue
            .dispatch_pending(&mut self.clientside.globals)
            .expect("Failed dispatching client side Wayland events");
        self.handle_clientside_events();
    }

    pub fn handle_clientside_events(&mut self) {
        self.handle_new_globals();

        for (key, event) in self.clientside.read_events() {
            let Some(object) = &mut self.objects.get_mut(key) else {
                warn!("could not handle clientside event: stale object");
                continue;
            };
            let mut object = object.0.take().unwrap();
            object.handle_event(event, self);

            let ret = self.objects[key].0.replace(object); // safe indexed access?
            debug_assert!(ret.is_none());
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
            for (key, _) in self.output_keys.clone() {
                let Some(object) = &mut self.objects.get_mut(key) else {
                    continue;
                };
                let mut output: Output = object
                    .0
                    .take()
                    .expect("Output object missing?")
                    .try_into()
                    .expect("Not an output?");
                output.global_offset_updated(self);
                self.objects[key].0.replace(output.into());
            }
            self.global_offset_updated = false;
        }

        if self.output_scales_updated {
            let mut mixed_scale = false;
            let mut scale;

            'b: {
                let mut keys_iter = self.output_keys.iter();
                let (key, _) = keys_iter.next().unwrap();
                let Some::<&Output>(output) = &mut self.objects.get(key).map(AsRef::as_ref) else {
                    // This should never happen, but you never know...
                    break 'b;
                };

                scale = output.scale();

                for (key, _) in keys_iter {
                    let Some::<&Output>(output) = self.objects.get(key).map(AsRef::as_ref) else {
                        continue;
                    };

                    if output.scale() != scale {
                        mixed_scale = true;
                        scale = scale.min(output.scale());
                    }
                }

                if mixed_scale {
                    warn!("Mixed output scales detected, choosing to give apps the smallest detected scale ({scale}x)");
                }

                debug!("Using new scale {scale}");
                self.new_scale = Some(scale);
            }

            self.output_scales_updated = false;
        }

        {
            if let Some(FocusData {
                window,
                output_name,
            }) = self.to_focus.take()
            {
                let conn = self.connection.as_mut().unwrap();
                debug!("focusing window {window:?}");
                conn.focus_window(window, output_name);
                self.last_focused_toplevel = Some(window);
            } else if self.unfocus {
                let conn = self.connection.as_mut().unwrap();
                conn.focus_window(x::WINDOW_NONE, None);
            }
            self.unfocus = false;
        }

        self.handle_clipboard_events();
        self.handle_activations();
        self.clientside
            .queue
            .flush()
            .expect("Failed flushing clientside events");
    }

    pub fn new_global_scale(&mut self) -> Option<i32> {
        self.new_scale.take()
    }

    pub fn new_selection(&mut self) -> Option<ForeignSelection> {
        self.clipboard_data.as_mut().and_then(|c| {
            c.source.take().and_then(|s| match s {
                CopyPasteData::Foreign(f) => Some(f),
                CopyPasteData::X11 { .. } => {
                    c.source = Some(s);
                    None
                }
            })
        })
    }

    fn handle_clipboard_events(&mut self) {
        let globals = &mut self.clientside.globals;

        if let Some(clipboard) = self.clipboard_data.as_mut() {
            for (mime_type, fd) in std::mem::take(&mut globals.selection_requests) {
                let CopyPasteData::X11 { data, .. } = clipboard.source.as_ref().unwrap() else {
                    unreachable!("Got selection request without having set the selection?")
                };
                if let Some(data) = data.upgrade() {
                    data.write_to(&mime_type, fd);
                }
            }

            if clipboard.source.is_none() || globals.cancelled {
                if globals.selection.take().is_some() {
                    let device = clipboard.device.as_ref().unwrap();
                    let offer = device.data().selection_offer().unwrap();
                    let mime_types: Box<[String]> = offer.with_mime_types(|mimes| mimes.into());
                    let foreign = ForeignSelection {
                        mime_types,
                        inner: offer,
                    };
                    clipboard.source = Some(CopyPasteData::Foreign(foreign));
                }
                globals.cancelled = false;
            }
        }
    }

    fn handle_activations(&mut self) {
        let Some(activation_state) = self.activation_state.as_ref() else {
            return;
        };
        let globals = &mut self.clientside.globals;

        globals.pending_activations.retain(|(window, token)| {
            if let Some(surface) = self
                .windows
                .get(window)
                .and_then(|window| window.surface_key)
                .and_then(|key| self.objects.get(key))
                .map(AsRef::<SurfaceData>::as_ref)
            {
                activation_state.activate::<Self>(&surface.client, token.clone());
                return false;
            }
            true
        });
    }

    fn calc_global_output_offset(&mut self) {
        for (key, _) in &self.output_keys {
            let Some(object) = &self.objects.get(key) else {
                continue;
            };

            let output: &Output = object.as_ref();
            if output.dimensions.x < self.global_output_offset.x.value {
                self.global_output_offset.x = GlobalOutputOffsetDimension {
                    owner: Some(key),
                    value: output.dimensions.x,
                }
            }
            if output.dimensions.y < self.global_output_offset.y.value {
                self.global_output_offset.y = GlobalOutputOffsetDimension {
                    owner: Some(key),
                    value: output.dimensions.y,
                }
            }
        }
    }

    fn create_role_window(&mut self, window: x::Window, surface_key: ObjectKey) {
        let surface: &mut SurfaceData = self.objects.get_mut(surface_key).unwrap().as_mut();
        surface.window = Some(window);
        surface.client.attach(None, 0, 0);
        surface.client.commit();

        let xdg_surface = self
            .xdg_wm_base
            .get_xdg_surface(&surface.client, &self.qh, surface_key);

        // Temporarily remove to placate borrow checker
        let window_data = self.windows.remove(&window).unwrap();

        let mut popup_for = None;
        if window_data.attrs.is_popup {
            popup_for = self.last_hovered.or(self.last_focused_toplevel);
        }

        let mut fullscreen = false;
        let (width, height) = (window_data.attrs.dims.width, window_data.attrs.dims.height);
        for (key, _) in &self.output_keys {
            let output: &Output = self.objects[key].as_ref();
            if output.dimensions.width == width as i32 && output.dimensions.height == height as i32
            {
                fullscreen = true;
                popup_for = None;
            }
        }

        let initial_scale;
        let role = if let Some(parent) = popup_for {
            let data;
            (initial_scale, data) =
                self.create_popup(&window_data, surface_key, xdg_surface, parent);
            SurfaceRole::Popup(Some(data))
        } else {
            initial_scale = 1.0;
            let data = self.create_toplevel(&window_data, surface_key, xdg_surface, fullscreen);
            SurfaceRole::Toplevel(Some(data))
        };

        let surface: &mut SurfaceData = self.objects[surface_key].as_mut();
        surface.scale_factor = initial_scale;

        let new_role_type = std::mem::discriminant(&role);
        let prev = surface.role.replace(role);
        if let Some(role) = prev {
            let old_role_type = std::mem::discriminant(&role);
            assert_eq!(
                new_role_type, old_role_type,
                "Surface for {:?} already had a role: {:?}",
                window_data.window, role
            );
        }

        surface.client.commit();
        // Reinsert
        self.windows.insert(window, window_data);
    }

    fn create_toplevel(
        &mut self,
        window: &WindowData,
        surface_key: ObjectKey,
        xdg: XdgSurface,
        fullscreen: bool,
    ) -> ToplevelData {
        debug!("creating toplevel for {:?}", window.window);

        let toplevel = xdg.get_toplevel(&self.qh, surface_key);
        if let Some(hints) = &window.attrs.size_hints {
            if let Some(min) = &hints.min_size {
                toplevel.set_min_size(min.width, min.height);
            }
            if let Some(max) = &hints.max_size {
                toplevel.set_max_size(max.width, max.height);
            }
        }

        let group = window.attrs.group.and_then(|win| self.windows.get(&win));
        if let Some(class) = window
            .attrs
            .class
            .as_ref()
            .or(group.and_then(|g| g.attrs.class.as_ref()))
        {
            toplevel.set_app_id(class.to_string());
        }
        if let Some(title) = window
            .attrs
            .title
            .as_ref()
            .or(group.and_then(|g| g.attrs.title.as_ref()))
        {
            toplevel.set_title(title.name().to_string());
        }

        if fullscreen {
            toplevel.set_fullscreen(None);
        }

        let decoration = self.decoration_manager.as_ref().map(|decoration_manager| {
            let decoration = decoration_manager.get_toplevel_decoration(&toplevel, &self.qh, ());
            decoration.set_mode(
                window
                    .attrs
                    .decorations
                    .map_or(zxdg_toplevel_decoration_v1::Mode::ServerSide, From::from),
            );
            decoration
        });

        let surface: &SurfaceData = self.objects[surface_key].as_ref();
        if let (Some(activation_state), Some(token)) = (
            self.activation_state.as_ref(),
            window.activation_token.clone(),
        ) {
            activation_state.activate::<Self>(&surface.client, token);
        }

        if let Some(parent) = window.attrs.transient_for {
            // TODO: handle transient_for window not being mapped/not a toplevel
            'b: {
                let Some(parent_data) = self.windows.get_mut(&parent) else {
                    warn!(
                        "Window {:?} is marked transient for unknown window {:?}",
                        window.window, parent
                    );
                    break 'b;
                };

                let Some(key) = parent_data.surface_key else {
                    warn!("Parent window {parent:?} missing surface key.");
                    break 'b;
                };

                let Some::<&SurfaceData>(surface) = self.objects.get(key).map(|o| o.as_ref())
                else {
                    warn!("Parent window {parent:?} surface is stale");
                    break 'b;
                };

                let Some(SurfaceRole::Toplevel(Some(parent_toplevel))) = &surface.role else {
                    warn!("Surface {:?} (for window {parent:?}) was not an active toplevel, not setting as parent", surface.client.id());
                    break 'b;
                };

                toplevel.set_parent(Some(&parent_toplevel.toplevel));
            }
        }

        ToplevelData {
            xdg: XdgSurfaceData {
                surface: xdg,
                configured: false,
                pending: None,
            },
            toplevel,
            fullscreen: false,
            decoration,
        }
    }

    fn create_popup(
        &self,
        window: &WindowData,
        surface_key: ObjectKey,
        xdg: XdgSurface,
        parent: x::Window,
    ) -> (f64, PopupData) {
        let parent_window = self.windows.get(&parent).unwrap();
        let parent_surface: &SurfaceData =
            self.objects[parent_window.surface_key.unwrap()].as_ref();
        let parent_dims = parent_window.attrs.dims;
        let initial_scale = parent_surface.scale_factor;

        debug!(
            "creating popup ({:?}) {:?} {:?} {:?} {surface_key:?} (scale: {initial_scale})",
            window.window,
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
        let popup = xdg.get_popup(
            Some(&parent_surface.xdg().unwrap().surface),
            &positioner,
            &self.qh,
            surface_key,
        );

        (
            initial_scale,
            PopupData {
                popup,
                positioner,
                xdg: XdgSurfaceData {
                    surface: xdg,
                    configured: false,
                    pending: None,
                },
            },
        )
    }

    fn get_server_surface_from_client(
        &self,
        surface: client::wl_surface::WlSurface,
    ) -> Option<&WlSurface> {
        let key: &ObjectKey = surface.data().unwrap();
        let surface: &SurfaceData = self.objects.get(*key)?.as_ref();
        Some(&surface.server)
    }

    fn get_client_surface_from_server(
        &self,
        surface: WlSurface,
    ) -> Option<&client::wl_surface::WlSurface> {
        let key: &ObjectKey = surface.data().unwrap();
        let surface: &SurfaceData = self.objects.get(*key)?.as_ref();
        Some(&surface.client)
    }

    fn close_x_window(&mut self, window: x::Window) {
        debug!("sending close request to {window:?}");
        self.connection.as_mut().unwrap().close_window(window);
        if self.last_focused_toplevel == Some(window) {
            self.last_focused_toplevel.take();
        }
        if self.last_hovered == Some(window) {
            self.last_hovered.take();
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

struct ClipboardData<X: X11Selection> {
    manager: DataDeviceManagerState,
    device: Option<DataDevice>,
    source: Option<CopyPasteData<X>>,
}

pub struct ForeignSelection {
    pub mime_types: Box<[String]>,
    inner: SelectionOffer,
}

impl ForeignSelection {
    pub(crate) fn receive(
        &self,
        mime_type: String,
        state: &ServerState<impl XConnection>,
    ) -> Vec<u8> {
        let mut pipe = self.inner.receive(mime_type).unwrap();
        state.clientside.queue.flush().unwrap();
        let mut data = Vec::new();
        pipe.read_to_end(&mut data).unwrap();
        data
    }
}

impl Drop for ForeignSelection {
    fn drop(&mut self) {
        self.inner.destroy();
    }
}

enum CopyPasteData<X: X11Selection> {
    X11 {
        inner: CopyPasteSource,
        data: Weak<X>,
    },
    Foreign(ForeignSelection),
}
