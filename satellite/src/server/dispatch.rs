use super::*;
use log::{debug, trace, warn};
use std::sync::{Arc, OnceLock};
use wayland_protocols::{
    wp::{
        linux_dmabuf::zv1::{client as c_dmabuf, server as s_dmabuf},
        relative_pointer::zv1::{
            client::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1 as RelativePointerManClient,
            server::{
                zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1 as RelativePointerManServer,
                zwp_relative_pointer_v1::ZwpRelativePointerV1 as RelativePointerServer,
            },
        },
        viewporter::{client as c_vp, server as s_vp},
    },
    xdg::xdg_output::zv1::{
        client::zxdg_output_manager_v1::ZxdgOutputManagerV1 as OutputManClient,
        server::{
            zxdg_output_manager_v1::{
                self as s_output_man, ZxdgOutputManagerV1 as OutputManServer,
            },
            zxdg_output_v1::{self as s_xdgo, ZxdgOutputV1 as XdgOutputServer},
        },
    },
};
use wayland_server::{
    protocol::{
        wl_buffer::WlBuffer, wl_callback::WlCallback, wl_compositor::WlCompositor,
        wl_keyboard::WlKeyboard, wl_output::WlOutput, wl_pointer::WlPointer, wl_seat::WlSeat,
        wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_surface::WlSurface,
    },
    Dispatch, DisplayHandle, GlobalDispatch, Resource,
};

// noop
impl<C: XConnection> Dispatch<WlCallback, ()> for ServerState<C> {
    fn request(
        _: &mut Self,
        _: &wayland_server::Client,
        _: &WlCallback,
        _: <WlCallback as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        unreachable!();
    }
}

impl<C: XConnection> Dispatch<WlSurface, ObjectKey> for ServerState<C> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlSurface,
        request: <WlSurface as Resource>::Request,
        key: &ObjectKey,
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let surface: &SurfaceData = state.objects[*key].as_ref();
        let configured =
            surface.role.is_none() || surface.xdg().is_none() || surface.xdg().unwrap().configured;

        match request {
            Request::<WlSurface>::Attach { buffer, x, y } => {
                if buffer.is_none() {
                    trace!("xwayland attached null buffer to {:?}", surface.client);
                }
                let buffer = buffer.as_ref().map(|b| {
                    let key: &ObjectKey = b.data().unwrap();
                    let data: &Buffer = state.objects[*key].as_ref();
                    &data.client
                });

                if configured {
                    surface.client.attach(buffer, x, y);
                } else {
                    let buffer = buffer.cloned();
                    let surface: &mut SurfaceData = state.objects[*key].as_mut();
                    surface.attach = Some(SurfaceAttach { buffer, x, y });
                }
            }
            Request::<WlSurface>::DamageBuffer {
                x,
                y,
                width,
                height,
            } => {
                if configured {
                    surface.client.damage_buffer(x, y, width, height);
                }
            }
            Request::<WlSurface>::Frame { callback } => {
                let cb = data_init.init(callback, ());
                if configured {
                    surface.client.frame(&state.qh, cb);
                } else {
                    let surface: &mut SurfaceData = state.objects[*key].as_mut();
                    surface.frame_callback = Some(cb);
                }
            }
            Request::<WlSurface>::Commit => {
                if configured {
                    surface.client.commit();
                }
            }
            Request::<WlSurface>::Destroy => {
                let mut object = state.objects.remove(*key).unwrap();
                let surface: &mut SurfaceData = object.as_mut();
                surface.destroy_role();
                surface.client.destroy();
                debug!("deleting key: {:?}", key);
            }
            Request::<WlSurface>::SetBufferScale { scale } => {
                surface.client.set_buffer_scale(scale);
            }
            other => warn!("unhandled surface request: {other:?}"),
        }
    }
}

impl<C: XConnection>
    Dispatch<WlCompositor, ClientGlobalWrapper<client::wl_compositor::WlCompositor>>
    for ServerState<C>
{
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlCompositor,
        request: <WlCompositor as wayland_server::Resource>::Request,
        client: &ClientGlobalWrapper<client::wl_compositor::WlCompositor>,
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            Request::<WlCompositor>::CreateSurface { id } => {
                let mut surface_id = None;

                let key = state.objects.insert_with_key(|key| {
                    debug!("new surface with key {key:?}");
                    let client = client.create_surface(&state.qh, key);
                    let server = data_init.init(id, key);
                    surface_id = Some(server.id().protocol_id());

                    SurfaceData {
                        client,
                        server,
                        key,
                        attach: None,
                        frame_callback: None,
                        role: None,
                    }
                    .into()
                });

                let surface_id = surface_id.unwrap();

                if let Some((win, window_data)) =
                    state.windows.iter_mut().find_map(|(win, data)| {
                        Some(*win).zip((data.surface_id == surface_id).then_some(data))
                    })
                {
                    window_data.surface_key = Some(key);
                    state.associated_windows.insert(key, win);
                    debug!("associate surface {surface_id} with window {win:?}");
                    if window_data.mapped {
                        state.create_role_window(win, key);
                    }
                }
            }
            other => {
                warn!("unhandled wlcompositor request: {other:?}");
            }
        }
    }
}

impl<C: XConnection> Dispatch<WlBuffer, ObjectKey> for ServerState<C> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlBuffer,
        request: <WlBuffer as Resource>::Request,
        key: &ObjectKey,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        assert!(matches!(request, Request::<WlBuffer>::Destroy));

        let buf: &Buffer = state.objects[*key].as_ref();
        buf.client.destroy();
        state.objects.remove(*key);
    }
}

impl<C: XConnection> Dispatch<WlShmPool, ClientShmPool> for ServerState<C> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlShmPool,
        request: <WlShmPool as Resource>::Request,
        c_pool: &ClientShmPool,
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            Request::<WlShmPool>::CreateBuffer {
                id,
                offset,
                width,
                height,
                stride,
                format,
            } => {
                state.objects.insert_with_key(|key| {
                    let client = c_pool.pool.create_buffer(
                        offset,
                        width,
                        height,
                        stride,
                        convert_wenum(format),
                        &state.qh,
                        key,
                    );
                    let server = data_init.init(id, key);
                    Buffer { server, client }.into()
                });
            }
            Request::<WlShmPool>::Destroy => {
                c_pool.pool.destroy();
            }
            other => warn!("unhandled shmpool request: {other:?}"),
        }
    }
}

impl<C: XConnection> Dispatch<WlShm, ClientGlobalWrapper<client::wl_shm::WlShm>>
    for ServerState<C>
{
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlShm,
        request: <WlShm as Resource>::Request,
        client: &ClientGlobalWrapper<client::wl_shm::WlShm>,
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            Request::<WlShm>::CreatePool { id, fd, size } => {
                let c_pool = client.create_pool(fd.as_fd(), size, &state.qh, ());
                let c_pool = ClientShmPool { pool: c_pool, fd };
                data_init.init(id, c_pool);
            }
            other => {
                warn!("unhandled shm pool  request: {other:?}");
            }
        }
    }
}

impl<C: XConnection> Dispatch<WlPointer, ObjectKey> for ServerState<C> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlPointer,
        request: <WlPointer as Resource>::Request,
        key: &ObjectKey,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let Pointer {
            client: c_pointer, ..
        }: &Pointer = state.objects[*key].as_ref();

        match request {
            Request::<WlPointer>::SetCursor {
                serial,
                hotspot_x,
                hotspot_y,
                surface,
            } => {
                let c_surface = surface.map(|s| state.get_client_surface_from_server(s));
                c_pointer.set_cursor(serial, c_surface, hotspot_x, hotspot_y);
            }
            Request::<WlPointer>::Release => {
                c_pointer.release();
                state.objects.remove(*key);
            }
            _ => warn!("unhandled cursor request: {request:?}"),
        }
    }
}

impl<C: XConnection> Dispatch<WlKeyboard, ObjectKey> for ServerState<C> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlKeyboard,
        request: <WlKeyboard as Resource>::Request,
        key: &ObjectKey,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            Request::<WlKeyboard>::Release => {
                let Keyboard { client, .. }: &_ = state.objects[*key].as_ref();
                client.release();
                state.objects.remove(*key);
            }
            _ => unreachable!(),
        }
    }
}

impl<C: XConnection> Dispatch<WlSeat, ObjectKey> for ServerState<C> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlSeat,
        request: <WlSeat as Resource>::Request,
        key: &ObjectKey,
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            Request::<WlSeat>::GetPointer { id } => {
                state
                    .objects
                    .insert_from_other_objects([*key], |[seat_obj], key| {
                        let Seat { client, .. }: &Seat = seat_obj.try_into().unwrap();
                        let client = client.get_pointer(&state.qh, key);
                        let server = data_init.init(id, key);
                        trace!("new pointer: {server:?}");
                        Pointer::new(server, client).into()
                    });
            }
            Request::<WlSeat>::GetKeyboard { id } => {
                state
                    .objects
                    .insert_from_other_objects([*key], |[seat_obj], key| {
                        let Seat { client, .. }: &Seat = seat_obj.try_into().unwrap();
                        let client = client.get_keyboard(&state.qh, key);
                        let server = data_init.init(id, key);
                        Keyboard { client, server }.into()
                    });
            }
            other => warn!("unhandled seat request: {other:?}"),
        }
    }
}

impl<C: XConnection> Dispatch<RelativePointerServer, ObjectKey> for ServerState<C> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &RelativePointerServer,
        request: <RelativePointerServer as Resource>::Request,
        key: &ObjectKey,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        if let Request::<RelativePointerServer>::Destroy = request {
            let obj: &RelativePointer = state.objects[*key].as_ref();
            obj.client.destroy();
            state.objects.remove(*key);
        }
    }
}

impl<C: XConnection>
    Dispatch<RelativePointerManServer, ClientGlobalWrapper<RelativePointerManClient>>
    for ServerState<C>
{
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &RelativePointerManServer,
        request: <RelativePointerManServer as Resource>::Request,
        client: &ClientGlobalWrapper<RelativePointerManClient>,
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            Request::<RelativePointerManServer>::GetRelativePointer { id, pointer } => {
                let p_key: ObjectKey = pointer.data().copied().unwrap();
                state
                    .objects
                    .insert_from_other_objects([p_key], |[pointer_obj], key| {
                        let pointer: &Pointer = pointer_obj.try_into().unwrap();
                        let client = client.get_relative_pointer(&pointer.client, &state.qh, key);
                        let server = data_init.init(id, key);
                        RelativePointer { client, server }.into()
                    });
            }
            _ => warn!("unhandled relative pointer request: {request:?}"),
        }
    }
}

impl<C: XConnection> Dispatch<WlOutput, ObjectKey> for ServerState<C> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlOutput,
        request: <WlOutput as Resource>::Request,
        key: &ObjectKey,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            wayland_server::protocol::wl_output::Request::Release => {
                let Output { client, .. }: &_ = state.objects[*key].as_ref();
                client.release();
                todo!("handle wloutput destruction");
            }
            _ => warn!("unhandled output request {request:?}"),
        }
    }
}

impl<C: XConnection>
    Dispatch<s_dmabuf::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1, ObjectKey>
    for ServerState<C>
{
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &s_dmabuf::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
        request: <s_dmabuf::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1 as Resource>::Request,
        key: &ObjectKey,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        use s_dmabuf::zwp_linux_dmabuf_feedback_v1::Request::*;
        match request {
            Destroy => {
                let dmabuf: &DmabufFeedback = state.objects[*key].as_ref();
                dmabuf.client.destroy();
                state.objects.remove(*key);
            }
            _ => unreachable!(),
        }
    }
}

impl<C: XConnection>
    Dispatch<
        s_dmabuf::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        c_dmabuf::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    > for ServerState<C>
{
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &s_dmabuf::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        request: <s_dmabuf::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1 as Resource>::Request,
        c_params: &c_dmabuf::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        use s_dmabuf::zwp_linux_buffer_params_v1::Request::*;
        match request {
            // TODO: Xwayland doesn't actually seem to use the Create request, and I don't feel like implementing it...
            Create { .. } => todo!(),
            CreateImmed {
                buffer_id,
                width,
                height,
                format,
                flags,
            } => {
                state.objects.insert_with_key(|key| {
                    let client = c_params.create_immed(
                        width,
                        height,
                        format,
                        convert_wenum(flags),
                        &state.qh,
                        key,
                    );
                    let server = data_init.init(buffer_id, key);
                    Buffer { server, client }.into()
                });
            }
            Add {
                fd,
                plane_idx,
                offset,
                stride,
                modifier_hi,
                modifier_lo,
            } => {
                c_params.add(
                    fd.as_fd(),
                    plane_idx,
                    offset,
                    stride,
                    modifier_hi,
                    modifier_lo,
                );
            }
            Destroy => {
                c_params.destroy();
            }
            _ => warn!("unhandled params request: {request:?}"),
        }
    }
}

impl<C: XConnection>
    Dispatch<
        s_dmabuf::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        ClientGlobalWrapper<c_dmabuf::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    > for ServerState<C>
{
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &s_dmabuf::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        request: <s_dmabuf::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1 as Resource>::Request,
        client: &ClientGlobalWrapper<c_dmabuf::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        use s_dmabuf::zwp_linux_dmabuf_v1::Request::*;
        match request {
            Destroy => {
                client.destroy();
            }
            CreateParams { params_id } => {
                let c_params = client.create_params(&state.qh, ());
                data_init.init(params_id, c_params);
            }
            GetDefaultFeedback { id } => {
                state.objects.insert_with_key(|key| {
                    let client = client.get_default_feedback(&state.qh, key);
                    let server = data_init.init(id, key);
                    DmabufFeedback { client, server }.into()
                });
            }
            GetSurfaceFeedback { id, surface } => {
                let surf_key: ObjectKey = surface.data().copied().unwrap();
                state
                    .objects
                    .insert_from_other_objects([surf_key], |[surface_obj], key| {
                        let SurfaceData {
                            client: c_surface, ..
                        }: &SurfaceData = surface_obj.try_into().unwrap();
                        let client = client.get_surface_feedback(c_surface, &state.qh, key);
                        let server = data_init.init(id, key);
                        DmabufFeedback { client, server }.into()
                    });
            }
            _ => warn!("unhandled dmabuf request: {request:?}"),
        }
    }
}

impl<C: XConnection> Dispatch<WlDrmServer, ObjectKey> for ServerState<C> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlDrmServer,
        request: <WlDrmServer as Resource>::Request,
        key: &ObjectKey,
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        use wl_drm::server::wl_drm::Request::*;

        type DrmFn = dyn FnOnce(
            &wl_drm::client::wl_drm::WlDrm,
            ObjectKey,
            &ClientQueueHandle,
        ) -> client::wl_buffer::WlBuffer;

        let mut bufs: Option<(Box<DrmFn>, wayland_server::New<WlBuffer>)> = None;
        match request {
            CreateBuffer {
                id,
                name,
                width,
                height,
                stride,
                format,
            } => {
                bufs = Some((
                    Box::new(move |drm, key, qh| {
                        drm.create_buffer(name, width, height, stride, format, qh, key)
                    }),
                    id,
                ));
            }
            CreatePlanarBuffer {
                id,
                name,
                width,
                height,
                format,
                offset0,
                stride0,
                offset1,
                stride1,
                offset2,
                stride2,
            } => {
                bufs = Some((
                    Box::new(move |drm, key, qh| {
                        drm.create_planar_buffer(
                            name, width, height, format, offset0, stride0, offset1, stride1,
                            offset2, stride2, qh, key,
                        )
                    }),
                    id,
                ));
            }
            CreatePrimeBuffer {
                id,
                name,
                width,
                height,
                format,
                offset0,
                stride0,
                offset1,
                stride1,
                offset2,
                stride2,
            } => {
                bufs = Some((
                    Box::new(move |drm, key, qh| {
                        drm.create_prime_buffer(
                            name.as_fd(),
                            width,
                            height,
                            format,
                            offset0,
                            stride0,
                            offset1,
                            stride1,
                            offset2,
                            stride2,
                            qh,
                            key,
                        )
                    }),
                    id,
                ));
            }
            Authenticate { id } => {
                let drm: &Drm = state.objects[*key].as_ref();
                drm.client.authenticate(id);
            }
            _ => unreachable!(),
        }

        if let Some((buf_create, id)) = bufs {
            state
                .objects
                .insert_from_other_objects([*key], |[drm_obj], key| {
                    let drm: &Drm = drm_obj.try_into().unwrap();
                    let client = buf_create(&drm.client, key, &state.qh);
                    let server = data_init.init(id, key);
                    Buffer { client, server }.into()
                });
        }
    }
}

impl<C: XConnection> Dispatch<s_vp::wp_viewport::WpViewport, c_vp::wp_viewport::WpViewport>
    for ServerState<C>
{
    fn request(
        _: &mut Self,
        _: &wayland_server::Client,
        _: &s_vp::wp_viewport::WpViewport,
        request: <s_vp::wp_viewport::WpViewport as Resource>::Request,
        c_viewport: &c_vp::wp_viewport::WpViewport,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        simple_event_shunt! {
            c_viewport, request: s_vp::wp_viewport::Request => [
                SetSource { x, y, width, height },
                SetDestination { width, height },
                Destroy
            ]
        }
    }
}

impl<C: XConnection>
    Dispatch<
        s_vp::wp_viewporter::WpViewporter,
        ClientGlobalWrapper<c_vp::wp_viewporter::WpViewporter>,
    > for ServerState<C>
{
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &s_vp::wp_viewporter::WpViewporter,
        request: <s_vp::wp_viewporter::WpViewporter as Resource>::Request,
        client: &ClientGlobalWrapper<c_vp::wp_viewporter::WpViewporter>,
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        use s_vp::wp_viewporter;
        match request {
            wp_viewporter::Request::GetViewport { id, surface } => {
                let c_surface = state.get_client_surface_from_server(surface);
                let c_viewport = client.get_viewport(c_surface, &state.qh, ());
                data_init.init(id, c_viewport);
            }
            wp_viewporter::Request::Destroy => {
                client.destroy();
            }
            _ => unreachable!(),
        }
    }
}

impl<C: XConnection> Dispatch<XdgOutputServer, ObjectKey> for ServerState<C> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &XdgOutputServer,
        request: <XdgOutputServer as Resource>::Request,
        key: &ObjectKey,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let s_xdgo::Request::Destroy = request else {
            unreachable!();
        };

        let output: &XdgOutput = state.objects[*key].as_ref();
        output.client.destroy();
        state.objects.remove(*key);
    }
}

impl<C: XConnection> Dispatch<OutputManServer, ClientGlobalWrapper<OutputManClient>>
    for ServerState<C>
{
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &OutputManServer,
        request: <OutputManServer as Resource>::Request,
        client: &ClientGlobalWrapper<OutputManClient>,
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            s_output_man::Request::GetXdgOutput { id, output } => {
                let output_key: ObjectKey = output.data().copied().unwrap();
                state
                    .objects
                    .insert_from_other_objects([output_key], |[output_obj], key| {
                        let output: &Output = output_obj.try_into().unwrap();
                        let client = client.get_xdg_output(&output.client, &state.qh, key);
                        let server = data_init.init(id, key);
                        XdgOutput { server, client }.into()
                    });
            }
            s_output_man::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct ClientGlobalWrapper<T: Proxy>(Arc<OnceLock<T>>);
impl<T: Proxy> std::ops::Deref for ClientGlobalWrapper<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        self.0.get().unwrap()
    }
}

impl<T: Proxy> Default for ClientGlobalWrapper<T> {
    fn default() -> Self {
        Self(Arc::default())
    }
}

macro_rules! global_dispatch_no_events {
    ($server:ty, $client:ty) => {
        impl<C: XConnection> GlobalDispatch<$server, GlobalData> for ServerState<C>
        where
            ServerState<C>: Dispatch<$server, ClientGlobalWrapper<$client>>,
            Globals: wayland_client::Dispatch<$client, ()>,
        {
            fn bind(
                state: &mut Self,
                _: &DisplayHandle,
                _: &wayland_server::Client,
                resource: wayland_server::New<$server>,
                data: &GlobalData,
                data_init: &mut wayland_server::DataInit<'_, Self>,
            ) {
                let client = ClientGlobalWrapper::<$client>::default();
                let server = data_init.init(resource, client.clone());
                client
                    .0
                    .set(
                        state
                            .clientside
                            .registry
                            .bind(data.name, server.version(), &state.qh, ()),
                    )
                    .unwrap();
            }
        }
    };
}

macro_rules! global_dispatch_with_events {
    ($server:ty, $client:ty) => {
        impl<C: XConnection> GlobalDispatch<$server, GlobalData> for ServerState<C>
        where
            $server: Resource,
            $client: Proxy,
            ServerState<C>: Dispatch<$server, ObjectKey>,
            Globals: wayland_client::Dispatch<$client, ObjectKey>,
            GenericObject<$server, $client>: Into<Object>,
        {
            fn bind(
                state: &mut Self,
                _: &DisplayHandle,
                _: &wayland_server::Client,
                resource: wayland_server::New<$server>,
                data: &GlobalData,
                data_init: &mut wayland_server::DataInit<'_, Self>,
            ) {
                state.objects.insert_with_key(|key| {
                    let server = data_init.init(resource, key);
                    let client = state.clientside.registry.bind::<$client, _, _>(
                        data.name,
                        server.version(),
                        &state.qh,
                        key,
                    );
                    GenericObject { server, client }.into()
                });
            }
        }
    };
}

global_dispatch_no_events!(WlShm, client::wl_shm::WlShm);
global_dispatch_no_events!(WlCompositor, client::wl_compositor::WlCompositor);
global_dispatch_no_events!(RelativePointerManServer, RelativePointerManClient);
global_dispatch_no_events!(
    s_dmabuf::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    c_dmabuf::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1
);
global_dispatch_no_events!(OutputManServer, OutputManClient);
global_dispatch_no_events!(
    s_vp::wp_viewporter::WpViewporter,
    c_vp::wp_viewporter::WpViewporter
);

global_dispatch_with_events!(WlSeat, client::wl_seat::WlSeat);
global_dispatch_with_events!(WlOutput, client::wl_output::WlOutput);
global_dispatch_with_events!(WlDrmServer, WlDrmClient);
