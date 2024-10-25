use std::collections::{hash_map, HashMap, HashSet};
use std::io::Read;
use std::io::Write;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use wayland_protocols::{
    wp::{
        linux_dmabuf::zv1::server::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        pointer_constraints::zv1::server::zwp_pointer_constraints_v1::ZwpPointerConstraintsV1,
        relative_pointer::zv1::server::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
        tablet::zv2::server::{
            zwp_tablet_manager_v2::ZwpTabletManagerV2,
            zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2,
            zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2,
            zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2,
            zwp_tablet_pad_v2::ZwpTabletPadV2,
            zwp_tablet_seat_v2::ZwpTabletSeatV2,
            zwp_tablet_tool_v2::{self, ZwpTabletToolV2},
            zwp_tablet_v2::ZwpTabletV2,
        },
        viewporter::server::wp_viewporter::WpViewporter,
    },
    xdg::{
        shell::server::{
            xdg_popup::{self, XdgPopup},
            xdg_positioner::{self, XdgPositioner},
            xdg_surface::XdgSurface,
            xdg_toplevel::{self, XdgToplevel},
            xdg_wm_base::{self, XdgWmBase},
        },
        xdg_output::zv1::server::{
            zxdg_output_manager_v1::{self, ZxdgOutputManagerV1},
            zxdg_output_v1::{self, ZxdgOutputV1},
        },
    },
};
use wayland_server::{
    backend::{
        protocol::{Interface, ProtocolError},
        GlobalHandler, ObjectData,
    },
    protocol::{
        self as proto,
        wl_buffer::WlBuffer,
        wl_callback::WlCallback,
        wl_compositor::WlCompositor,
        wl_data_device::{self, WlDataDevice},
        wl_data_device_manager::{self, WlDataDeviceManager},
        wl_data_offer::{self, WlDataOffer},
        wl_data_source::{self, WlDataSource},
        wl_keyboard::{self, WlKeyboard},
        wl_output::{self, WlOutput},
        wl_pointer::{self, WlPointer},
        wl_seat::{self, WlSeat},
        wl_shm::WlShm,
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
    Client, Dispatch, Display, DisplayHandle, GlobalDispatch, Resource,
};
use wl_drm::server::wl_drm::WlDrm;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BufferDamage {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SurfaceData {
    pub surface: WlSurface,
    pub buffer: Option<WlBuffer>,
    pub last_damage: Option<BufferDamage>,
    pub role: Option<SurfaceRole>,
    pub last_enter_serial: Option<u32>,
}

impl SurfaceData {
    pub fn xdg(&self) -> &XdgSurfaceData {
        match self.role.as_ref().expect("Surface missing role") {
            SurfaceRole::Toplevel(ref t) => &t.xdg,
            SurfaceRole::Popup(ref p) => &p.xdg,
            SurfaceRole::Cursor => panic!("cursor surface doesn't have an XdgSurface"),
        }
    }

    pub fn toplevel(&self) -> &Toplevel {
        match self.role.as_ref().expect("Surface missing role") {
            SurfaceRole::Toplevel(ref t) => t,
            other => panic!("Surface role was not toplevel: {other:?}"),
        }
    }
    pub fn popup(&self) -> &Popup {
        match self.role.as_ref().expect("Surface missing role") {
            SurfaceRole::Popup(ref p) => p,
            other => panic!("Surface role was not popup: {other:?}"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SurfaceRole {
    Toplevel(Toplevel),
    Popup(Popup),
    Cursor,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Toplevel {
    pub xdg: XdgSurfaceData,
    pub toplevel: XdgToplevel,
    pub min_size: Option<Vec2>,
    pub max_size: Option<Vec2>,
    pub states: Vec<xdg_toplevel::State>,
    pub closed: bool,
    pub title: Option<String>,
    pub app_id: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Popup {
    pub xdg: XdgSurfaceData,
    pub parent: XdgSurface,
    pub popup: XdgPopup,
    pub positioner_state: PositionerState,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct Vec2 {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, PartialEq, Eq)]
pub struct XdgSurfaceData {
    pub surface: XdgSurface,
    pub last_configure_serial: u32,
}

impl XdgSurfaceData {
    fn new(surface: XdgSurface) -> Self {
        Self {
            surface,
            last_configure_serial: 0,
        }
    }

    fn configure(&mut self, serial: u32) {
        self.surface.configure(serial);
        self.last_configure_serial = serial;
    }
}

#[derive(Debug, Hash, Clone, Copy, Eq, PartialEq)]
pub struct SurfaceId(u32);

#[derive(Hash, Clone, Copy, Eq, PartialEq)]
struct PositionerId(u32);

#[derive(Default)]
struct DataSourceData {
    mimes: Vec<String>,
}

struct Output {
    wl: WlOutput,
    xdg: Option<ZxdgOutputV1>,
}

struct KeyboardState {
    keyboard: WlKeyboard,
    current_focus: Option<SurfaceId>,
}

struct State {
    surfaces: HashMap<SurfaceId, SurfaceData>,
    outputs: HashMap<WlOutput, Output>,
    positioners: HashMap<PositionerId, PositionerState>,
    buffers: HashSet<WlBuffer>,
    begin: Instant,
    last_surface_id: Option<SurfaceId>,
    last_output: Option<WlOutput>,
    callbacks: Vec<WlCallback>,
    pointer: Option<WlPointer>,
    keyboard: Option<KeyboardState>,
    configure_serial: u32,
    selection: Option<WlDataSource>,
    data_device_man: Option<WlDataDeviceManager>,
    data_device: Option<WlDataDevice>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            surfaces: Default::default(),
            outputs: Default::default(),
            buffers: Default::default(),
            positioners: Default::default(),
            begin: Instant::now(),
            last_surface_id: None,
            last_output: None,
            callbacks: Vec::new(),
            pointer: None,
            keyboard: None,
            configure_serial: 0,
            selection: None,
            data_device_man: None,
            data_device: None,
        }
    }
}

impl State {
    #[track_caller]
    fn configure_toplevel(
        &mut self,
        surface_id: SurfaceId,
        width: i32,
        height: i32,
        states: Vec<xdg_toplevel::State>,
    ) {
        let last_serial = self.configure_serial;
        let toplevel = self.get_toplevel(surface_id);
        toplevel.states = states.clone();
        let states: Vec<u8> = states
            .into_iter()
            .map(|state| u32::from(state) as u8)
            .collect();
        toplevel.toplevel.configure(width, height, states);
        toplevel.xdg.configure(last_serial);
        self.configure_serial += 1;
    }

    #[track_caller]
    fn focus_toplevel(&mut self, surface_id: SurfaceId) {
        let KeyboardState {
            keyboard,
            current_focus,
        } = self.keyboard.as_mut().expect("Keyboard should be created");

        if let Some(id) = current_focus {
            keyboard.leave(self.configure_serial, &self.surfaces[id].surface);
        }

        keyboard.enter(
            self.configure_serial,
            &self.surfaces[&surface_id].surface,
            Vec::default(),
        );

        *current_focus = Some(surface_id);
    }

    #[track_caller]
    fn unfocus_toplevel(&mut self) {
        let KeyboardState {
            current_focus,
            keyboard,
        } = self.keyboard.as_mut().expect("Keyboard should be created");

        if let Some(id) = current_focus.take() {
            keyboard.leave(self.configure_serial, &self.surfaces[&id].surface);
        }
    }

    #[track_caller]
    pub fn configure_popup(&mut self, surface_id: SurfaceId) {
        let surface = self.surfaces.get_mut(&surface_id).unwrap();
        let Some(SurfaceRole::Popup(p)) = &mut surface.role else {
            panic!("Surface does not have popup role: {:?}", surface.role);
        };
        let PositionerState { size, offset, .. } = &p.positioner_state;
        let size = size.unwrap();
        p.popup.configure(offset.x, offset.y, size.x, size.y);
        p.xdg.configure(self.configure_serial);
        self.configure_serial += 1;
    }

    #[track_caller]
    fn get_toplevel(&mut self, surface_id: SurfaceId) -> &mut Toplevel {
        let surface = self
            .surfaces
            .get_mut(&surface_id)
            .expect("Surface does not exist");
        match &mut surface.role {
            Some(SurfaceRole::Toplevel(t)) => t,
            other => panic!("Surface does not have toplevel role: {:?}", other),
        }
    }
}

macro_rules! simple_global_dispatch {
    ($type:ty) => {
        impl GlobalDispatch<$type, ()> for State {
            fn bind(
                _: &mut Self,
                _: &DisplayHandle,
                _: &wayland_server::Client,
                resource: wayland_server::New<$type>,
                _: &(),
                data_init: &mut wayland_server::DataInit<'_, Self>,
            ) {
                data_init.init(resource, ());
            }
        }
    };
}

pub struct Server {
    display: Display<State>,
    dh: DisplayHandle,
    state: State,
    client: Option<Client>,
}

impl Server {
    pub fn new(noops: bool) -> Self {
        let display = Display::new().unwrap();
        let dh = display.handle();

        macro_rules! global_noop {
            ($type:ty) => {
                if noops {
                    dh.create_global::<State, $type, _>(1, ());
                }
                simple_global_dispatch!($type);
                impl Dispatch<$type, ()> for State {
                    fn request(
                        _: &mut Self,
                        _: &Client,
                        _: &$type,
                        _: <$type as Resource>::Request,
                        _: &(),
                        _: &DisplayHandle,
                        _: &mut wayland_server::DataInit<'_, Self>,
                    ) {
                        todo!("Dispatch for {} is no-op", stringify!($type));
                    }
                }
            };
        }
        dh.create_global::<State, WlCompositor, _>(6, ());
        dh.create_global::<State, WlShm, _>(1, ());
        dh.create_global::<State, XdgWmBase, _>(6, ());
        dh.create_global::<State, WlSeat, _>(5, ());
        dh.create_global::<State, WlDataDeviceManager, _>(3, ());
        dh.create_global::<State, ZwpTabletManagerV2, _>(1, ());
        global_noop!(ZwpLinuxDmabufV1);
        global_noop!(ZwpRelativePointerManagerV1);
        global_noop!(WpViewporter);
        global_noop!(ZwpPointerConstraintsV1);

        struct HandlerData;
        impl ObjectData<State> for HandlerData {
            fn request(
                self: Arc<Self>,
                _: &wayland_server::backend::Handle,
                _: &mut State,
                _: wayland_server::backend::ClientId,
                _: wayland_server::backend::protocol::Message<
                    wayland_server::backend::ObjectId,
                    OwnedFd,
                >,
            ) -> Option<Arc<dyn ObjectData<State>>> {
                None
            }
            fn destroyed(
                self: Arc<Self>,
                _: &wayland_server::backend::Handle,
                _: &mut State,
                _: wayland_server::backend::ClientId,
                _: wayland_server::backend::ObjectId,
            ) {
            }
        }
        struct Handler;
        impl GlobalHandler<State> for Handler {
            fn bind(
                self: Arc<Self>,
                _: &wayland_server::backend::Handle,
                _: &mut State,
                _: wayland_server::backend::ClientId,
                _: wayland_server::backend::GlobalId,
                _: wayland_server::backend::ObjectId,
            ) -> Arc<dyn wayland_server::backend::ObjectData<State>> {
                Arc::new(HandlerData)
            }
        }

        // Simulate interface with higher interface than supported client side
        static IF: OnceLock<Interface> = OnceLock::new();
        let interface = IF.get_or_init(|| Interface {
            version: WlDrm::interface().version + 1,
            ..*WlDrm::interface()
        });

        if noops {
            dh.backend_handle()
                .create_global(&interface, interface.version, Arc::new(Handler));
        }

        Self {
            display,
            dh,
            state: State::default(),
            client: None,
        }
    }

    pub fn poll_fd(&mut self) -> BorrowedFd<'_> {
        self.display.backend().poll_fd()
    }

    pub fn connect(&mut self, stream: UnixStream) {
        let client = self
            .dh
            .insert_client(stream, std::sync::Arc::new(()))
            .unwrap();
        assert!(
            self.client.replace(client).is_none(),
            "Client already connected to test server"
        );
    }

    pub fn dispatch(&mut self) {
        self.display.dispatch_clients(&mut self.state).unwrap();
        for callback in std::mem::take(&mut self.state.callbacks) {
            callback.done(self.state.begin.elapsed().as_millis().try_into().unwrap());
        }
        self.display.flush_clients().unwrap();
    }

    pub fn get_surface_data(&self, surface_id: SurfaceId) -> Option<&SurfaceData> {
        self.state.surfaces.get(&surface_id)
    }

    pub fn last_created_surface_id(&self) -> Option<SurfaceId> {
        self.state.last_surface_id
    }

    #[track_caller]
    pub fn last_created_output(&self) -> WlOutput {
        self.state
            .last_output
            .as_ref()
            .expect("No outputs created!")
            .clone()
    }

    pub fn get_object<T: Resource + 'static>(
        &self,
        id: SurfaceId,
    ) -> Result<T, wayland_server::backend::InvalidId> {
        let client = self.client.as_ref().unwrap();
        client.object_from_protocol_id::<T>(&self.display.handle(), id.0)
    }

    #[track_caller]
    pub fn configure_toplevel(
        &mut self,
        surface_id: SurfaceId,
        width: i32,
        height: i32,
        states: Vec<xdg_toplevel::State>,
    ) {
        self.state
            .configure_toplevel(surface_id, width, height, states);
        self.display.flush_clients().unwrap();
    }

    #[track_caller]
    pub fn focus_toplevel(&mut self, surface_id: SurfaceId) {
        self.state.focus_toplevel(surface_id);
        self.display.flush_clients().unwrap();
    }

    #[track_caller]
    pub fn unfocus_toplevel(&mut self) {
        self.state.unfocus_toplevel();
        self.display.flush_clients().unwrap();
    }

    #[track_caller]
    pub fn configure_popup(&mut self, surface_id: SurfaceId) {
        self.state.configure_popup(surface_id);
        self.display.flush_clients().unwrap();
    }

    #[track_caller]
    pub fn close_toplevel(&mut self, surface_id: SurfaceId) {
        let toplevel = self.state.get_toplevel(surface_id);
        toplevel.toplevel.close();
        self.dispatch();
    }

    #[track_caller]
    pub fn pointer(&self) -> &WlPointer {
        self.state.pointer.as_ref().unwrap()
    }

    pub fn data_source_mimes(&self) -> Vec<String> {
        let Some(selection) = &self.state.selection else {
            panic!("No selection set on data device");
        };

        let data: &Mutex<DataSourceData> = selection.data().unwrap();
        let data = data.lock().unwrap();
        data.mimes.to_vec()
    }

    pub fn paste_data(&mut self) -> PasteDataResolver {
        let Some(selection) = &self.state.selection else {
            panic!("No selection set on data device");
        };

        let ret = PasteDataResolver::new(&selection);
        self.display.flush_clients().unwrap();
        ret
    }

    pub fn data_source_exists(&self) -> bool {
        self.state.selection.is_none()
    }

    pub fn create_data_offer(&mut self, data: Vec<PasteData>) {
        let Some(dev) = &self.state.data_device else {
            panic!("No data device created");
        };

        if let Some(selection) = self.state.selection.take() {
            selection.cancelled();
        }

        let mimes: Vec<_> = data.iter().map(|m| m.mime_type.clone()).collect();
        let offer = self
            .client
            .as_ref()
            .unwrap()
            .create_resource::<_, _, State>(&self.dh, 3, data)
            .unwrap();
        dev.data_offer(&offer);
        for mime in mimes {
            offer.offer(mime);
        }
        dev.selection(Some(&offer));
        self.display.flush_clients().unwrap();
    }

    #[track_caller]
    pub fn move_pointer_to(&mut self, surface: SurfaceId, x: f64, y: f64) {
        let pointer = self.state.pointer.as_ref().expect("No pointer created");
        let data = self.state.surfaces.get(&surface).expect("No such surface");

        pointer.enter(24, &data.surface, x, y);
        pointer.frame();
        self.display.flush_clients().unwrap();
    }

    pub fn new_output(&mut self, x: i32, y: i32) {
        self.dh.create_global::<State, WlOutput, _>(4, (x, y));
        self.display.flush_clients().unwrap();
    }

    pub fn move_output(&mut self, output: &WlOutput, x: i32, y: i32) {
        output.geometry(
            x,
            y,
            0,
            0,
            wl_output::Subpixel::None,
            "".into(),
            "".into(),
            wl_output::Transform::Normal,
        );
        output.done();
        self.display.flush_clients().unwrap();
    }

    pub fn move_surface_to_output(&mut self, surface: SurfaceId, output: &WlOutput) {
        let data = self.state.surfaces.get(&surface).expect("No such surface");
        data.surface.enter(output);
        self.display.flush_clients().unwrap();
    }

    pub fn enable_xdg_output_manager(&mut self) {
        self.dh
            .create_global::<State, ZxdgOutputManagerV1, _>(3, ());
        self.display.flush_clients().unwrap();
    }

    pub fn move_xdg_output(&mut self, output: &WlOutput, x: i32, y: i32) {
        let xdg = self.state.outputs[output]
            .xdg
            .as_ref()
            .expect("Output doesn't have an xdg output");
        xdg.logical_position(x, y);
        xdg.done();
        self.display.flush_clients().unwrap();
    }
}

pub struct PasteDataResolver {
    fds: Vec<(String, OwnedFd, OwnedFd)>,
}

impl PasteDataResolver {
    fn new(source: &WlDataSource) -> Self {
        let data: &Mutex<DataSourceData> = source.data().unwrap();
        let data = data.lock().unwrap();
        let mimes = &data.mimes;

        let fds = mimes
            .iter()
            .map(|mime| {
                let (rx, tx) = rustix::pipe::pipe().unwrap();
                source.send(mime.clone(), tx.as_fd());
                (mime.clone(), tx, rx)
            })
            .collect();

        PasteDataResolver { fds }
    }

    pub fn resolve(self) -> Vec<PasteData> {
        self.fds
            .into_iter()
            .map(|(mime, tx, rx)| {
                drop(tx);
                let mut data = Vec::new();
                let mut file = std::fs::File::from(rx);
                file.read_to_end(&mut data).unwrap();
                PasteData {
                    mime_type: mime,
                    data,
                }
            })
            .collect()
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct PasteData {
    pub mime_type: String,
    pub data: Vec<u8>,
}

simple_global_dispatch!(WlShm);
simple_global_dispatch!(WlCompositor);
simple_global_dispatch!(XdgWmBase);
simple_global_dispatch!(ZxdgOutputManagerV1);
simple_global_dispatch!(ZwpTabletManagerV2);

impl Dispatch<ZwpTabletManagerV2, ()> for State {
    fn request(
        _: &mut Self,
        client: &Client,
        _: &ZwpTabletManagerV2,
        request: <ZwpTabletManagerV2 as Resource>::Request,
        _: &(),
        dhandle: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            wayland_protocols::wp::tablet::zv2::server::zwp_tablet_manager_v2::Request::GetTabletSeat { tablet_seat, seat: _ } => {
                let seat = data_init.init(tablet_seat, ());
                let tablet = client.create_resource::<_, _, State>(dhandle, 1, ()).unwrap();
                seat.tablet_added(&tablet);
                tablet.name("tabby".to_owned());
                tablet.done();

                let tool = client.create_resource::<_, _, State>(dhandle, 1, ()).unwrap();
                seat.tool_added(&tool);
                tool._type(zwp_tablet_tool_v2::Type::Finger);
                tool.done();

                let pad = client.create_resource::<_, _, State>(dhandle, 1, ()).unwrap();
                let group = client.create_resource::<_, _, State>(dhandle, 1, ()).unwrap();
                let ring = client.create_resource::<_, _, State>(dhandle, 1, ()).unwrap();
                let strip = client.create_resource::<_, _, State>(dhandle, 1, ()).unwrap();

                seat.pad_added(&pad);
                pad.buttons(5);
                pad.group(&group);
                pad.done();

                group.buttons(vec![]);
                group.ring(&ring);
                group.strip(&strip);
                group.done();
            }
            wayland_protocols::wp::tablet::zv2::server::zwp_tablet_manager_v2::Request::Destroy => {}
            other => todo!("unhandled tablet manager request: {other:?}")
        }
    }
}

macro_rules! unhandled {
    ($type:ty) => {
        impl Dispatch<$type, ()> for State {
            fn request(
                _: &mut Self,
                _: &Client,
                _: &$type,
                _: <$type as Resource>::Request,
                _: &(),
                _: &DisplayHandle,
                _: &mut wayland_server::DataInit<'_, Self>,
            ) {
                todo!(concat!(stringify!($type), " unhandled"));
            }
        }
    };
}

unhandled!(ZwpTabletSeatV2);
unhandled!(ZwpTabletV2);
unhandled!(ZwpTabletToolV2);
unhandled!(ZwpTabletPadV2);
unhandled!(ZwpTabletPadGroupV2);
unhandled!(ZwpTabletPadRingV2);
unhandled!(ZwpTabletPadStripV2);

impl Dispatch<ZxdgOutputManagerV1, ()> for State {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZxdgOutputManagerV1,
        request: <ZxdgOutputManagerV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            zxdg_output_manager_v1::Request::GetXdgOutput { id, output } => {
                let xdg = data_init.init(id, output.clone());
                xdg.logical_position(0, 0);
                xdg.logical_size(1000, 1000);
                xdg.done();
                state.outputs.get_mut(&output).unwrap().xdg = Some(xdg);
            }
            other => todo!("unhandled request: {other:?}"),
        }
    }
}

impl Dispatch<ZxdgOutputV1, WlOutput> for State {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZxdgOutputV1,
        request: <ZxdgOutputV1 as Resource>::Request,
        output: &WlOutput,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            zxdg_output_v1::Request::Destroy => {
                state.outputs.get_mut(output).unwrap().xdg = None;
            }
            other => todo!("unhandled request: {other:?}"),
        }
    }
}

impl GlobalDispatch<WlOutput, (i32, i32)> for State {
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: wayland_server::New<WlOutput>,
        &(x, y): &(i32, i32),
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let output = data_init.init(resource, ());
        output.geometry(
            x,
            y,
            0,
            0,
            wl_output::Subpixel::None,
            "xwls".to_string(),
            "fake monitor".to_string(),
            wl_output::Transform::Normal,
        );
        output.mode(wl_output::Mode::Current, 1000, 1000, 0);
        output.done();
        state.outputs.insert(
            output.clone(),
            Output {
                wl: output.clone(),
                xdg: None,
            },
        );
        state.last_output = Some(output);
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlOutput,
        _: <WlOutput as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        unreachable!();
    }
}

impl GlobalDispatch<WlDataDeviceManager, ()> for State {
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: wayland_server::New<WlDataDeviceManager>,
        _: &(),
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        state.data_device_man = Some(data_init.init(resource, ()));
    }
}

impl Dispatch<WlDataOffer, Vec<PasteData>> for State {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlDataOffer,
        request: <WlDataOffer as Resource>::Request,
        data: &Vec<PasteData>,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            wl_data_offer::Request::Receive { mime_type, fd } => {
                let pos = data
                    .iter()
                    .position(|data| data.mime_type == mime_type)
                    .expect("Invalid mime type: {mime_type}");

                let mut stream = UnixStream::from(fd);
                stream.write_all(&data[pos].data).unwrap();
            }
            other => todo!("unhandled request: {other:?}"),
        }
    }
}

impl Dispatch<WlDataSource, Mutex<DataSourceData>> for State {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WlDataSource,
        request: <WlDataSource as Resource>::Request,
        data: &Mutex<DataSourceData>,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let mut data = data.lock().unwrap();
        match request {
            wl_data_source::Request::Offer { mime_type } => {
                data.mimes.push(mime_type);
            }
            wl_data_source::Request::Destroy => {
                state.selection = None;
            }
            other => todo!("unhandled request {other:?}"),
        }
    }
}

impl Dispatch<WlDataDevice, WlSeat> for State {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WlDataDevice,
        request: <WlDataDevice as Resource>::Request,
        _: &WlSeat,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            wl_data_device::Request::SetSelection { source, .. } => {
                state.selection = source;
            }
            wl_data_device::Request::Release => {
                state.data_device = None;
            }
            other => todo!("unhandled request {other:?}"),
        }
    }
}

impl Dispatch<WlDataDeviceManager, ()> for State {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WlDataDeviceManager,
        request: <WlDataDeviceManager as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            wl_data_device_manager::Request::CreateDataSource { id } => {
                data_init.init(id, DataSourceData::default().into());
            }
            wl_data_device_manager::Request::GetDataDevice { id, seat } => {
                state.data_device = Some(data_init.init(id, seat));
            }
            other => todo!("unhandled request: {other:?}"),
        }
    }
}

impl GlobalDispatch<WlSeat, ()> for State {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: wayland_server::New<WlSeat>,
        _: &(),
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let seat = data_init.init(resource, ());
        seat.capabilities(wl_seat::Capability::Pointer | wl_seat::Capability::Keyboard);
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WlSeat,
        request: <WlSeat as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            wl_seat::Request::GetPointer { id } => {
                state.pointer = Some(data_init.init(id, ()));
            }
            wl_seat::Request::GetKeyboard { id } => {
                state.keyboard = Some(KeyboardState {
                    keyboard: data_init.init(id, ()),
                    current_focus: None,
                });
            }
            wl_seat::Request::Release => {}
            other => todo!("unhandled request {other:?}"),
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WlPointer,
        request: <WlPointer as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            wl_pointer::Request::SetCursor { surface, .. } => {
                if let Some(surface) = surface {
                    let data = state
                        .surfaces
                        .get_mut(&SurfaceId(surface.id().protocol_id()))
                        .unwrap();

                    assert!(
                        matches!(
                            data.role.replace(SurfaceRole::Cursor),
                            None | Some(SurfaceRole::Cursor)
                        ),
                        "Surface already had a non cursor role!"
                    );
                }
            }
            wl_pointer::Request::Release => {
                state.pointer.take();
            }
            other => todo!("unhandled pointer request: {other:?}"),
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlKeyboard,
        request: <WlKeyboard as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            wl_keyboard::Request::Release => {}
            other => todo!("unhandled request {other:?}"),
        }
    }
}

impl Dispatch<XdgPopup, SurfaceId> for State {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &XdgPopup,
        request: <XdgPopup as Resource>::Request,
        surface_id: &SurfaceId,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            xdg_popup::Request::Destroy => {}
            xdg_popup::Request::Reposition { positioner, token } => {
                let data = state.surfaces.get_mut(surface_id).unwrap();
                let Some(SurfaceRole::Popup(p)) = &mut data.role else {
                    unreachable!();
                };
                let positioner_data =
                    &state.positioners[&PositionerId(positioner.id().protocol_id())];
                p.positioner_state = positioner_data.clone();
                p.popup.repositioned(token);
                state.configure_popup(*surface_id);
            }
            other => todo!("unhandled request {other:?}"),
        }
    }
}

impl Dispatch<XdgToplevel, SurfaceId> for State {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &XdgToplevel,
        request: <XdgToplevel as Resource>::Request,
        surface_id: &SurfaceId,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            xdg_toplevel::Request::SetMinSize { width, height } => {
                let data = state.surfaces.get_mut(surface_id).unwrap();
                let Some(SurfaceRole::Toplevel(toplevel)) = &mut data.role else {
                    unreachable!();
                };
                toplevel.min_size = Some(Vec2 {
                    x: width,
                    y: height,
                });
            }
            xdg_toplevel::Request::SetMaxSize { width, height } => {
                let data = state.surfaces.get_mut(surface_id).unwrap();
                let Some(SurfaceRole::Toplevel(toplevel)) = &mut data.role else {
                    unreachable!();
                };
                toplevel.max_size = Some(Vec2 {
                    x: width,
                    y: height,
                });
            }
            xdg_toplevel::Request::SetFullscreen { .. } => {
                let data = state.surfaces.get_mut(surface_id).unwrap();
                let Some(SurfaceRole::Toplevel(toplevel)) = &mut data.role else {
                    unreachable!();
                };
                toplevel.states.push(xdg_toplevel::State::Fullscreen);
                let states = toplevel.states.clone();
                state.configure_toplevel(*surface_id, 100, 100, states);
            }
            xdg_toplevel::Request::UnsetFullscreen { .. } => {
                let data = state.surfaces.get_mut(surface_id).unwrap();
                let Some(SurfaceRole::Toplevel(toplevel)) = &mut data.role else {
                    unreachable!();
                };
                let Some(pos) = toplevel
                    .states
                    .iter()
                    .copied()
                    .position(|p| p == xdg_toplevel::State::Fullscreen)
                else {
                    return;
                };
                toplevel.states.swap_remove(pos);
                let states = toplevel.states.clone();
                state.configure_toplevel(*surface_id, 100, 100, states);
            }
            xdg_toplevel::Request::Destroy => {}
            xdg_toplevel::Request::SetTitle { title } => {
                let data = state.surfaces.get_mut(surface_id).unwrap();
                let Some(SurfaceRole::Toplevel(toplevel)) = &mut data.role else {
                    unreachable!();
                };
                toplevel.title = title.into();
            }
            xdg_toplevel::Request::SetAppId { app_id } => {
                let data = state.surfaces.get_mut(surface_id).unwrap();
                let Some(SurfaceRole::Toplevel(toplevel)) = &mut data.role else {
                    unreachable!();
                };
                toplevel.app_id = app_id.into();
            }
            other => todo!("unhandled request {other:?}"),
        }
    }
}

impl Dispatch<XdgSurface, SurfaceId> for State {
    fn request(
        state: &mut Self,
        client: &wayland_server::Client,
        resource: &XdgSurface,
        request: <XdgSurface as Resource>::Request,
        surface_id: &SurfaceId,
        dh: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        use wayland_protocols::xdg::shell::server::xdg_surface;

        match request {
            xdg_surface::Request::GetToplevel { id } => {
                let toplevel = data_init.init(id, *surface_id);
                let t = Toplevel {
                    xdg: XdgSurfaceData::new(resource.clone()),
                    toplevel,
                    min_size: None,
                    max_size: None,
                    states: Vec::new(),
                    closed: false,
                    title: None,
                    app_id: None,
                };
                let data = state.surfaces.get_mut(surface_id).unwrap();
                data.role = Some(SurfaceRole::Toplevel(t));
            }
            xdg_surface::Request::GetPopup {
                id,
                parent,
                positioner,
            } => {
                let popup = data_init.init(id, *surface_id);
                let p = Popup {
                    xdg: XdgSurfaceData::new(resource.clone()),
                    popup,
                    parent: parent.unwrap(),
                    positioner_state: state.positioners
                        [&PositionerId(positioner.id().protocol_id())]
                        .clone(),
                };
                let data = state.surfaces.get_mut(surface_id).unwrap();
                data.role = Some(SurfaceRole::Popup(p));
            }
            xdg_surface::Request::AckConfigure { serial } => {
                let data = state.surfaces.get_mut(surface_id).unwrap();
                assert_eq!(data.xdg().last_configure_serial, serial);
            }
            xdg_surface::Request::Destroy => {
                let data = state.surfaces.get_mut(surface_id).unwrap();
                let role_alive = data.role.is_none()
                    || match data.role.as_ref().unwrap() {
                        SurfaceRole::Toplevel(t) => t.toplevel.is_alive(),
                        SurfaceRole::Popup(p) => p.popup.is_alive(),
                        SurfaceRole::Cursor => false,
                    };
                if role_alive {
                    client.kill(
                        dh,
                        ProtocolError {
                            code: xdg_surface::Error::DefunctRoleObject.into(),
                            object_id: resource.id().protocol_id(),
                            object_interface: XdgSurface::interface().name.to_string(),
                            message: "destroyed xdg surface before role".to_string(),
                        },
                    );
                }
            }
            other => todo!("unhandled request {other:?}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rect {
    pub size: Vec2,
    pub offset: Vec2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionerState {
    pub size: Option<Vec2>,
    pub anchor_rect: Option<Rect>,
    pub offset: Vec2,
    pub anchor: xdg_positioner::Anchor,
    pub gravity: xdg_positioner::Gravity,
}

impl Default for PositionerState {
    fn default() -> Self {
        Self {
            size: None,
            anchor_rect: None,
            offset: Vec2 { x: 0, y: 0 },
            anchor: xdg_positioner::Anchor::None,
            gravity: xdg_positioner::Gravity::None,
        }
    }
}

impl Dispatch<XdgPositioner, ()> for State {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &XdgPositioner,
        request: <XdgPositioner as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let hash_map::Entry::Occupied(mut data) = state
            .positioners
            .entry(PositionerId(resource.id().protocol_id()))
        else {
            unreachable!();
        };
        match request {
            xdg_positioner::Request::SetSize { width, height } => {
                data.get_mut().size = Some(Vec2 {
                    x: width,
                    y: height,
                });
            }
            xdg_positioner::Request::SetAnchorRect {
                x,
                y,
                width,
                height,
            } => {
                data.get_mut().anchor_rect = Some(Rect {
                    size: Vec2 {
                        x: width,
                        y: height,
                    },
                    offset: Vec2 { x, y },
                });
            }
            xdg_positioner::Request::SetOffset { x, y } => {
                data.get_mut().offset = Vec2 { x, y };
            }
            xdg_positioner::Request::SetAnchor { anchor } => {
                data.get_mut().anchor = anchor.into_result().unwrap();
            }
            xdg_positioner::Request::SetGravity { gravity } => {
                data.get_mut().gravity = gravity.into_result().unwrap();
            }
            xdg_positioner::Request::Destroy => {
                data.remove();
            }
            other => todo!("unhandled positioner request {other:?}"),
        }
    }
}

impl Dispatch<XdgWmBase, ()> for State {
    fn request(
        state: &mut Self,
        client: &wayland_server::Client,
        _: &XdgWmBase,
        request: <XdgWmBase as Resource>::Request,
        _: &(),
        dhandle: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            xdg_wm_base::Request::GetXdgSurface { id, surface } => {
                let surface_id = SurfaceId(surface.id().protocol_id());
                let data = state.surfaces.get(&surface_id).unwrap();
                if data.buffer.is_some() {
                    client.kill(
                        dhandle,
                        ProtocolError {
                            code: xdg_wm_base::Error::InvalidSurfaceState.into(),
                            object_id: surface_id.0,
                            object_interface: XdgWmBase::interface().name.to_string(),
                            message: "Buffer already attached to surface".to_string(),
                        },
                    );
                    return;
                }
                data_init.init(id, surface_id);
            }
            xdg_wm_base::Request::CreatePositioner { id } => {
                let pos = data_init.init(id, ());
                state.positioners.insert(
                    PositionerId(pos.id().protocol_id()),
                    PositionerState::default(),
                );
            }
            other => todo!("unhandled request {other:?}"),
        }
    }
}

impl Dispatch<WlShm, ()> for State {
    fn request(
        _: &mut Self,
        _: &wayland_server::Client,
        _: &WlShm,
        request: <WlShm as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            proto::wl_shm::Request::CreatePool { id, .. } => {
                data_init.init(id, ());
            }
            _ => unreachable!(),
        }
    }
}

impl Dispatch<WlShmPool, ()> for State {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlShmPool,
        request: <WlShmPool as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        use proto::wl_shm_pool::Request::*;
        match request {
            CreateBuffer { id, .. } => {
                let buf = data_init.init(id, ());
                state.buffers.insert(buf);
            }
            Destroy => {}
            other => todo!("unhandled request {other:?}"),
        }
    }
}

impl Dispatch<WlBuffer, ()> for State {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        resource: &WlBuffer,
        request: <WlBuffer as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            proto::wl_buffer::Request::Destroy => {
                state.buffers.remove(resource);
            }
            _ => unreachable!(),
        }
    }
}

impl Dispatch<WlCompositor, ()> for State {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlCompositor,
        request: <WlCompositor as wayland_server::Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            proto::wl_compositor::Request::CreateSurface { id } => {
                let surface = data_init.init(id, ());
                let id = surface.id().protocol_id();
                state.surfaces.insert(
                    SurfaceId(id),
                    SurfaceData {
                        surface,
                        buffer: None,
                        last_damage: None,
                        role: None,
                        last_enter_serial: None,
                    },
                );
                state.last_surface_id = Some(SurfaceId(id));
            }
            _ => unreachable!(),
        }
    }
}

impl Dispatch<WlSurface, ()> for State {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        resource: &WlSurface,
        request: <WlSurface as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        use proto::wl_surface::Request::*;

        let data = state
            .surfaces
            .get_mut(&SurfaceId(resource.id().protocol_id()))
            .unwrap_or_else(|| panic!("{:?} missing from surface map", resource));

        match request {
            Attach { buffer, .. } => {
                data.buffer = buffer;
            }
            Frame { callback } => {
                // XXX: calling done immediately will cause wayland_backend to panic,
                // report upstream
                state.callbacks.push(data_init.init(callback, ()));
            }
            DamageBuffer {
                x,
                y,
                width,
                height,
            } => {
                data.last_damage = Some(BufferDamage {
                    x,
                    y,
                    width,
                    height,
                });
            }
            Commit => {}
            Destroy => {
                let id = SurfaceId(resource.id().protocol_id());
                if let Some(kb) = state
                    .keyboard
                    .as_mut()
                    .filter(|kb| kb.current_focus == Some(id))
                {
                    kb.keyboard.leave(state.configure_serial, resource);
                    kb.current_focus.take();
                }
                state.surfaces.remove(&id);
            }
            SetInputRegion { .. } => {}
            SetBufferScale { .. } => {}
            other => todo!("unhandled request {other:?}"),
        }
    }
}

impl Dispatch<WlCallback, ()> for State {
    fn request(
        _: &mut Self,
        _: &wayland_server::Client,
        _: &WlCallback,
        _: <WlCallback as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        unreachable!()
    }
}
