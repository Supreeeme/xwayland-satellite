mod dispatch;
mod event;

#[cfg(test)]
mod tests;

use self::event::*;
use super::FromServerState;
use crate::clientside::*;
use crate::xstate::{Atoms, WindowDims, WmNormalHints};
use crate::XConnection;
use log::{debug, warn};
use rustix::event::{poll, PollFd, PollFlags};
use slotmap::{new_key_type, HopSlotMap, SparseSecondaryMap};
use std::collections::HashMap;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use wayland_client::{protocol as client, Proxy};
use wayland_protocols::{
    wp::{
        linux_dmabuf::zv1::{client as c_dmabuf, server as s_dmabuf},
        relative_pointer::zv1::{
            self as rp, server::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
        },
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
};
use wayland_server::{
    protocol::{
        wl_callback::WlCallback, wl_compositor::WlCompositor, wl_output::WlOutput, wl_seat::WlSeat,
        wl_shm::WlShm, wl_surface::WlSurface,
    },
    DisplayHandle, Resource, WEnum,
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
struct WindowData {
    window: x::Window,
    surface_key: Option<ObjectKey>,
    mapped: bool,
    surface_id: u32,
    popup_for: Option<x::Window>,
    dims: WindowDims,
    hints: Option<WmNormalHints>,
    override_redirect: bool,
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
            mapped: false,
            popup_for: parent,
            surface_id: 0,
            dims,
            hints: None,
            override_redirect,
        }
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
    frame_callback: Option<WlCallback>,
    attach: Option<SurfaceAttach>,
    role: Option<SurfaceRole>,
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
    XdgOutput(XdgOutput),
    Touch(Touch)
}

}

struct WrappedObject(Option<Object>);

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

new_key_type! {
    pub struct ObjectKey;
}
pub struct ServerState<C: XConnection> {
    pub atoms: Option<Atoms>,
    dh: DisplayHandle,
    clientside: ClientState,
    objects: ObjectMap,
    associated_windows: SparseSecondaryMap<ObjectKey, x::Window>,
    windows: HashMap<x::Window, WindowData>,

    xdg_wm_base: XdgWmBase,
    qh: ClientQueueHandle,
    to_focus: Option<x::Window>,
    last_focused_toplevel: Option<x::Window>,
    connection: Option<C>,
}

const XDG_WM_BASE_VERSION: u32 = 2;

impl<C: XConnection> ServerState<C> {
    pub fn new(dh: DisplayHandle, server_connection: Option<UnixStream>) -> Self {
        let mut clientside = ClientState::new(server_connection);
        let qh = clientside.qh.clone();

        let xdg_pos = clientside
            .globals
            .new_globals
            .iter()
            .position(|g| g.interface == XdgWmBase::interface().name)
            .expect("Did not get an xdg_wm_base global");

        let data = clientside.globals.new_globals.swap_remove(xdg_pos);

        assert!(
            data.version >= XDG_WM_BASE_VERSION,
            "xdg_wm_base older than version {XDG_WM_BASE_VERSION}"
        );

        let xdg_wm_base =
            clientside
                .registry
                .bind::<XdgWmBase, _, _>(data.name, XDG_WM_BASE_VERSION, &qh, ());

        let mut ret = Self {
            windows: HashMap::new(),
            clientside,
            atoms: None,
            qh,
            dh,
            to_focus: None,
            last_focused_toplevel: None,
            connection: None,
            objects: Default::default(),
            associated_windows: Default::default(),
            xdg_wm_base,
        };
        ret.handle_new_globals();
        ret
    }

    pub fn clientside_fd(&self) -> BorrowedFd<'_> {
        self.clientside.queue.as_fd()
    }

    pub fn connect(&mut self, connection: UnixStream) {
        self.dh
            .insert_client(connection, std::sync::Arc::new(()))
            .unwrap();
    }

    pub fn set_x_connection(&mut self, connection: C) {
        self.connection = Some(connection);
    }

    fn handle_new_globals(&mut self) {
        let globals = std::mem::take(&mut self.clientside.globals.new_globals);
        for data in globals {
            macro_rules! server_global {
                ($($global:ty),+) => {
                    match data.interface {
                        $(
                            ref x if x == <$global>::interface().name => {
                                self.dh.create_global::<Self, $global, GlobalData>(data.version, data);
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
                s_vp::wp_viewporter::WpViewporter
            ];
        }
    }

    fn get_object_from_client_object<T, P: Proxy>(&self, proxy: &P) -> &T
    where
        for<'a> &'a T: TryFrom<&'a Object, Error = String>,
        Globals: wayland_client::Dispatch<P, ObjectKey>,
    {
        let key: ObjectKey = proxy.data().copied().unwrap();
        self.objects[key].as_ref()
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

    pub fn set_win_hints(&mut self, window: x::Window, hints: WmNormalHints) {
        let win = self.windows.get_mut(&window).unwrap();

        if win.hints.is_none() || *win.hints.as_ref().unwrap() != hints {
            debug!("setting {window:?} hints {hints:?}");
            if let Some(surface) = win.surface_key {
                let surface: &SurfaceData = self.objects[surface].as_ref();
                if let Some(SurfaceRole::Toplevel(Some(data))) = &surface.role {
                    if let Some(min_size) = &hints.min_size {
                        data.toplevel.set_min_size(min_size.width, min_size.height);
                    }
                    if let Some(max_size) = &hints.max_size {
                        data.toplevel.set_max_size(max_size.width, max_size.height);
                    }
                }
            }
            win.hints = Some(hints);
        }
    }

    pub fn associate_window(&mut self, window: x::Window, surface_id: u32) {
        let win = self.windows.get_mut(&window).unwrap();
        win.surface_id = surface_id;

        if let Some(key) = self
            .objects
            .iter_mut()
            .filter_map(|(key, obj)| {
                Some(key).zip(<&mut SurfaceData>::try_from(obj.0.as_mut().unwrap()).ok())
            })
            .find_map(|(key, surface)| {
                (surface_id == surface.server.id().protocol_id()).then_some(key)
            })
        {
            win.surface_key = Some(key);
            self.associated_windows.insert(key, window);
            debug!("associate {:?} with surface {surface_id}", window);
            if win.mapped {
                self.create_role_window(window, key);
            }
        }
    }

    pub fn reconfigure_window(&mut self, event: x::ConfigureNotifyEvent) {
        let win = self.windows.get_mut(&event.window()).unwrap();
        win.dims = WindowDims {
            x: event.x(),
            y: event.y(),
            width: event.width(),
            height: event.height(),
        };
    }

    pub fn map_window(&mut self, window: x::Window) {
        debug!("mapping {window:?}");

        let window = self.windows.get_mut(&window).unwrap();
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
        win.mapped = false;

        if let Some(key) = win.surface_key.take() {
            let surface: &mut SurfaceData = self.objects[key].as_mut();
            surface.destroy_role();
        }
    }

    pub fn set_fullscreen(&mut self, window: x::Window, state: super::xstate::SetState) {
        let win = self.windows.get(&window).unwrap();
        let Some(key) = win.surface_key else {
            warn!("Tried to set window without surface fullscreen: {window:?}");
            return;
        };
        let surface: &mut SurfaceData = self.objects[key].as_mut();
        let Some(SurfaceRole::Toplevel(Some(ref toplevel))) = surface.role else {
            warn!("Tried to set an unmapped toplevel or non toplevel fullscreen: {window:?}");
            return;
        };

        match state {
            crate::xstate::SetState::Add => toplevel.toplevel.set_fullscreen(None),
            crate::xstate::SetState::Remove => toplevel.toplevel.unset_fullscreen(),
            crate::xstate::SetState::Toggle => {
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
            .unwrap();
        self.handle_clientside_events();
        self.clientside.queue.flush().unwrap();
    }

    pub fn handle_clientside_events(&mut self) {
        self.handle_new_globals();

        let client_events = std::mem::take(&mut self.clientside.globals.events);
        for (key, event) in client_events {
            let object = &mut self.objects[key];
            let mut object = object.0.take().unwrap();
            object.handle_event(event, self);
            let ret = self.objects[key].0.replace(object);
            debug_assert!(ret.is_none());
        }

        {
            if let Some(win) = self.to_focus.take() {
                let data = C::ExtraData::create(self);
                let conn = self.connection.as_mut().unwrap();
                debug!("focusing window {win:?}");
                conn.focus_window(win, data);
                self.last_focused_toplevel = Some(win);
            }
        }

        self.clientside.queue.flush().unwrap();
    }

    fn create_role_window(&mut self, window: x::Window, surface_key: ObjectKey) {
        let surface: &SurfaceData = self.objects[surface_key].as_ref();
        let client = &surface.client;
        client.attach(None, 0, 0);
        client.commit();

        let xdg_surface = self
            .xdg_wm_base
            .get_xdg_surface(client, &self.qh, surface_key);

        let window_data = self.windows.get_mut(&window).unwrap();
        if window_data.override_redirect {
            // Override redirect is hard to convert to Wayland!
            // We will just make them be popups for the last focused toplevel.
            if let Some(win) = self.last_focused_toplevel {
                window_data.popup_for = Some(win)
            }
        }
        let window = self.windows.get(&window).unwrap();

        let role = if let Some(parent) = window.popup_for {
            debug!(
                "creating popup ({:?}) {:?} {:?} {:?} {surface_key:?}",
                window.window,
                parent,
                window.dims,
                surface.client.id()
            );

            let parent_window = self.windows.get(&parent).unwrap();
            let parent_surface: &SurfaceData =
                self.objects[parent_window.surface_key.unwrap()].as_ref();

            let positioner = self.xdg_wm_base.create_positioner(&self.qh, ());
            positioner.set_size(window.dims.width as _, window.dims.height as _);
            positioner.set_offset(window.dims.x as i32, window.dims.y as i32);
            positioner.set_anchor(Anchor::TopLeft);
            positioner.set_gravity(Gravity::BottomRight);
            positioner.set_anchor_rect(
                0,
                0,
                parent_window.dims.width as _,
                parent_window.dims.height as _,
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
            let data = self.create_toplevel(window, surface_key, xdg_surface);
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
    ) -> ToplevelData {
        debug!("creating toplevel for {:?}", window.window);
        let toplevel = xdg.get_toplevel(&self.qh, surface_key);
        if let Some(hints) = &window.hints {
            if let Some(min) = &hints.min_size {
                toplevel.set_min_size(min.width, min.height);
            }
            if let Some(max) = &hints.max_size {
                toplevel.set_max_size(max.width, max.height);
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
        }
    }

    fn get_server_surface_from_client(&self, surface: client::wl_surface::WlSurface) -> &WlSurface {
        let key: &ObjectKey = surface.data().unwrap();
        let surface: &SurfaceData = self.objects[*key].as_ref();
        &surface.server
    }

    fn get_client_surface_from_server(&self, surface: WlSurface) -> &client::wl_surface::WlSurface {
        let key: &ObjectKey = surface.data().unwrap();
        let surface: &SurfaceData = self.objects[*key].as_ref();
        &surface.client
    }

    fn close_x_window(&mut self, window: x::Window) {
        debug!("sending close request to {window:?}");
        let data = C::ExtraData::create(self);
        self.connection.as_mut().unwrap().close_window(window, data);
        if self.last_focused_toplevel == Some(window) {
            self.last_focused_toplevel.take();
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
