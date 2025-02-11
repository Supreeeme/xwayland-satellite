mod dispatch;
mod event;

#[cfg(test)]
mod tests;

use self::event::*;
use super::FromServerState;
use crate::clientside::*;
use crate::xstate::{Atoms, WindowDims, WmHints, WmName, WmNormalHints};
use crate::{X11Selection, XConnection};
use log::{debug, warn};
use rustix::event::{poll, PollFd, PollFlags};
use slotmap::{new_key_type, HopSlotMap, SparseSecondaryMap};
use smithay_client_toolkit::data_device_manager::{
    data_device::DataDevice, data_offer::SelectionOffer, data_source::CopyPasteSource,
    DataDeviceManagerState,
};
use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::rc::{Rc, Weak};
use wayland_client::{globals::Global, protocol as client, Proxy};
use wayland_protocols::{
    wp::{
        linux_dmabuf::zv1::{client as c_dmabuf, server as s_dmabuf},
        pointer_constraints::zv1::server::zwp_pointer_constraints_v1::ZwpPointerConstraintsV1,
        relative_pointer::zv1::server::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
        tablet::zv2::server::zwp_tablet_manager_v2::ZwpTabletManagerV2,
        viewporter::server as s_vp,
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
use wayland_server::{
    protocol::{
        wl_callback::WlCallback, wl_compositor::WlCompositor, wl_output::WlOutput, wl_seat::WlSeat,
        wl_shm::WlShm, wl_surface::WlSurface,
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
pub struct WindowAttributes {
    pub override_redirect: bool,
    pub popup_for: Option<x::Window>,
    pub dims: WindowDims,
    pub size_hints: Option<WmNormalHints>,
    pub title: Option<WmName>,
    pub class: Option<String>,
    pub group: Option<x::Window>,
}

#[derive(Debug, Default, PartialEq, Eq)]
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
}

impl WindowData {
    fn new(
        window: x::Window,
        override_redirect: bool,
        dims: WindowDims,
        parent: Option<x::Window>,
    ) -> Self {
        Self {
            window,
            surface_key: None,
            surface_serial: None,
            mapped: false,
            attrs: WindowAttributes {
                override_redirect,
                dims,
                popup_for: parent,
                ..Default::default()
            },
            output_offset: WindowOutputOffset::default(),
            output_key: None,
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
                SurfaceRole::Toplevel(Some(t)) => {
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
        }
    ) => {
        enum_try_from! {
            $(#[$meta])*
            $pub enum $name {
                $( $variant($ty) ),+
            }
        }

        paste::paste! {
            enum_try_from! {
                #[derive(Debug)]
                $pub enum [<$name Event>] {
                    $( $variant(<$ty as HandleEvent>::Event) ),+
                }
            }
        }

        impl HandleEvent for $name {
            paste::paste! {
                type Event = [<$name Event>];
            }

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
}

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
            s_vp::wp_viewporter::WpViewporter,
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

pub struct ServerState<C: XConnection> {
    pub atoms: Option<Atoms>,
    dh: DisplayHandle,
    clientside: ClientState,
    objects: ObjectMap,
    associated_windows: SparseSecondaryMap<ObjectKey, x::Window>,
    output_keys: SparseSecondaryMap<ObjectKey, ()>,
    windows: HashMap<x::Window, WindowData>,

    qh: ClientQueueHandle,
    client: Option<Client>,
    to_focus: Option<FocusData>,
    unfocus: bool,
    last_focused_toplevel: Option<x::Window>,
    last_hovered: Option<x::Window>,
    pub connection: Option<C>,

    xdg_wm_base: XdgWmBase,
    clipboard_data: Option<ClipboardData<C::X11Selection>>,
    last_kb_serial: Option<u32>,
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

        dh.create_global::<Self, XwaylandShellV1, _>(1, ());
        clientside
            .global_list
            .contents()
            .with_list(|globals| handle_globals::<C>(&dh, globals));

        Self {
            windows: HashMap::new(),
            clientside,
            client: None,
            atoms: None,
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
            clipboard_data,
            last_kb_serial: None,
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
        parent: Option<x::Window>,
    ) {
        self.windows.insert(
            window,
            WindowData::new(window, override_redirect, dims, parent),
        );
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
                            data.toplevel.set_min_size(min_size.width, min_size.height);
                        }
                        if let Some(max_size) = &hints.max_size {
                            data.toplevel.set_max_size(max_size.width, max_size.height);
                        }
                    }
                } else {
                    warn!("could not set size hint on {window:?}: stale surface")
                }
            }
            win.attrs.size_hints = Some(hints);
        }
    }

    pub fn set_window_serial(&mut self, window: x::Window, serial: [u32; 2]) {
        let Some(win) = self.windows.get_mut(&window) else {
            warn!("Tried to set serial for unknown window {window:?}");
            return;
        };
        win.surface_serial = Some(serial);
    }

    pub fn can_reconfigure_window(&mut self, window: x::Window) -> bool {
        let Some(win) = self.windows.get_mut(&window) else {
            return true;
        };

        !win.mapped || win.attrs.override_redirect
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
                    event.x() as i32 - win.output_offset.x,
                    event.y() as i32 - win.output_offset.y,
                );
                popup
                    .positioner
                    .set_size(event.width().into(), event.height().into());
                popup.popup.reposition(&popup.positioner, 0);
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
            if let Some(serial) = self.last_kb_serial.as_ref().copied() {
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
                warn!("could not handle clientside event: stale surface");
                continue;
            };
            let mut object = object.0.take().unwrap();
            object.handle_event(event, self);
            let ret = self.objects[key].0.replace(object); // safe indexed access?
            debug_assert!(ret.is_none());
        }

        {
            if let Some(FocusData {
                window,
                output_name,
            }) = self.to_focus.take()
            {
                let data = C::ExtraData::create(self);
                let conn = self.connection.as_mut().unwrap();
                debug!("focusing window {window:?}");
                conn.focus_window(window, output_name, data);
                self.last_focused_toplevel = Some(window);
            } else if self.unfocus {
                let data = C::ExtraData::create(self);
                let conn = self.connection.as_mut().unwrap();
                conn.focus_window(x::WINDOW_NONE, None, data);
            }
            self.unfocus = false;
        }

        self.handle_clipboard_events();
        self.clientside
            .queue
            .flush()
            .expect("Failed flushing clientside events");
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

    fn create_role_window(&mut self, window: x::Window, surface_key: ObjectKey) {
        // Temporarily remove surface to placate borrow checker
        let mut surface: SurfaceData = self.objects[surface_key]
            .0
            .take()
            .unwrap()
            .try_into()
            .unwrap();
        surface.window = Some(window);
        surface.client.attach(None, 0, 0);
        surface.client.commit();

        let xdg_surface = self
            .xdg_wm_base
            .get_xdg_surface(&surface.client, &self.qh, surface_key);

        let window_data = self.windows.get_mut(&window).unwrap();
        if window_data.attrs.override_redirect {
            // Override redirect is hard to convert to Wayland!
            if let Some(win) = self.last_hovered {
                window_data.attrs.popup_for = Some(win);
            } else if let Some(win) = self.last_focused_toplevel {
                window_data.attrs.popup_for = Some(win);
            }
        }

        let mut fullscreen = false;
        let (width, height) = (window_data.attrs.dims.width, window_data.attrs.dims.height);
        for (key, _) in &self.output_keys {
            let output: &Output = self.objects[key].as_ref();
            if output.dimensions.width == width as i32 && output.dimensions.height == height as i32
            {
                fullscreen = true;
                window_data.attrs.popup_for = None;
            }
        }

        let surface_id = surface.client.id();
        // Reinsert surface
        self.objects[surface_key].0 = Some(surface.into());
        let window = self.windows.get(&window).unwrap();

        let role = if let Some(parent) = window.attrs.popup_for {
            debug!(
                "creating popup ({:?}) {:?} {:?} {:?} {surface_key:?}",
                window.window, parent, window.attrs.dims, surface_id
            );

            let parent_window = self.windows.get(&parent).unwrap();
            let parent_surface: &SurfaceData =
                self.objects[parent_window.surface_key.unwrap()].as_ref();
            let parent_dims = parent_window.attrs.dims;

            let x = window.attrs.dims.x - parent_dims.x;
            let y = window.attrs.dims.y - parent_dims.y;

            let positioner = self.xdg_wm_base.create_positioner(&self.qh, ());
            positioner.set_size(window.attrs.dims.width as _, window.attrs.dims.height as _);
            positioner.set_offset(x as i32, y as i32);
            positioner.set_anchor(Anchor::TopLeft);
            positioner.set_gravity(Gravity::BottomRight);
            positioner.set_anchor_rect(
                0,
                0,
                parent_window.attrs.dims.width as _,
                parent_window.attrs.dims.height as _,
            );
            let popup = xdg_surface.get_popup(
                Some(&parent_surface.xdg().unwrap().surface),
                &positioner,
                &self.qh,
                surface_key,
            );
            let popup = PopupData {
                popup,
                positioner,
                xdg: XdgSurfaceData {
                    surface: xdg_surface,
                    configured: false,
                    pending: None,
                },
            };
            SurfaceRole::Popup(Some(popup))
        } else {
            let data = self.create_toplevel(window, surface_key, xdg_surface, fullscreen);
            SurfaceRole::Toplevel(Some(data))
        };

        let surface: &mut SurfaceData = self.objects[surface_key].as_mut();

        let new_role_type = std::mem::discriminant(&role);
        let prev = surface.role.replace(role);
        if let Some(role) = prev {
            let old_role_type = std::mem::discriminant(&role);
            assert_eq!(
                new_role_type, old_role_type,
                "Surface for {:?} already had a role: {:?}",
                window.window, role
            );
        }

        surface.client.commit();
    }

    fn create_toplevel(
        &self,
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

        ToplevelData {
            xdg: XdgSurfaceData {
                surface: xdg,
                configured: false,
                pending: None,
            },
            toplevel,
            fullscreen: false,
        }
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
        let data = C::ExtraData::create(self);
        self.connection.as_mut().unwrap().close_window(window, data);
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
