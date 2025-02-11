use super::*;
use log::{debug, error, trace, warn};
use macros::simple_event_shunt;
use std::sync::{Arc, OnceLock};
use wayland_client::globals::Global;
use wayland_protocols::{
    wp::{
        linux_dmabuf::zv1::{client as c_dmabuf, server as s_dmabuf},
        pointer_constraints::zv1::{
            client::zwp_pointer_constraints_v1::ZwpPointerConstraintsV1 as PointerConstraintsClient,
            server::{
                zwp_confined_pointer_v1::{
                    self as cp, ZwpConfinedPointerV1 as ConfinedPointerServer,
                },
                zwp_locked_pointer_v1::{self as lp, ZwpLockedPointerV1 as LockedPointerServer},
                zwp_pointer_constraints_v1::{
                    self as pc, ZwpPointerConstraintsV1 as PointerConstraintsServer,
                },
            },
        },
        relative_pointer::zv1::{
            client::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1 as RelativePointerManClient,
            server::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1 as RelativePointerManServer,
        },
        tablet::zv2::{client as c_tablet, server as s_tablet},
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
    xwayland::shell::v1::server::{
        xwayland_shell_v1::{self, XwaylandShellV1},
        xwayland_surface_v1::{self, XwaylandSurfaceV1},
    },
};
use wayland_server::{
    protocol::{
        wl_buffer::WlBuffer,
        wl_callback::WlCallback,
        wl_compositor::WlCompositor,
        wl_keyboard::WlKeyboard,
        wl_output::WlOutput,
        wl_pointer::WlPointer,
        wl_region::{self, WlRegion},
        wl_seat::WlSeat,
        wl_shm::WlShm,
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
        wl_touch::WlTouch,
    },
    Dispatch, DisplayHandle, GlobalDispatch, Resource,
};

macro_rules! only_destroy_request_impl {
    ($object_type:ty) => {
        impl<C: XConnection> Dispatch<<$object_type as GenericObjectExt>::Server, ObjectKey>
            for ServerState<C>
        {
            fn request(
                state: &mut Self,
                _: &Client,
                _: &<$object_type as GenericObjectExt>::Server,
                request: <<$object_type as GenericObjectExt>::Server as Resource>::Request,
                key: &ObjectKey,
                _: &DisplayHandle,
                _: &mut wayland_server::DataInit<'_, Self>,
            ) {
                if !matches!(
                    request,
                    <<$object_type as GenericObjectExt>::Server as Resource>::Request::Destroy
                ) {
                    warn!(
                        "unrecognized {} request: {:?}",
                        stringify!($object_type),
                        request
                    );
                    return;
                }

                let obj: &$object_type = state.objects[*key].as_ref();
                obj.client.destroy();
                state.objects.remove(*key);
            }
        }
    };
}

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
                if let Some(window_data) = surface.window.and_then(|w| state.windows.get_mut(&w)) {
                    window_data.surface_key.take();
                }
                surface.destroy_role();
                surface.client.destroy();
                debug!(
                    "deleting key: {key:?} (surface {:?})",
                    surface.server.id().protocol_id()
                );
            }
            Request::<WlSurface>::SetBufferScale { scale } => {
                surface.client.set_buffer_scale(scale);
            }
            Request::<WlSurface>::SetInputRegion { region } => {
                let region = region.as_ref().map(|r| r.data().unwrap());
                surface.client.set_input_region(region);
            }
            other => warn!("unhandled surface request: {other:?}"),
        }
    }
}

impl<C: XConnection> Dispatch<WlRegion, client::wl_region::WlRegion> for ServerState<C> {
    fn request(
        _: &mut Self,
        _: &wayland_server::Client,
        _: &WlRegion,
        request: <WlRegion as Resource>::Request,
        client: &client::wl_region::WlRegion,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        macros::simple_event_shunt! {
            client, request: wl_region::Request => [
                Add { x, y, width, height },
                Subtract { x, y, width, height },
                Destroy
            ]
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

                state.objects.insert_with_key(|key| {
                    let client = client.create_surface(&state.qh, key);
                    let server = data_init.init(id, key);
                    surface_id = Some(server.id().protocol_id());
                    debug!("new surface with key {key:?} ({surface_id:?})");

                    SurfaceData {
                        client,
                        server,
                        key,
                        serial: Default::default(),
                        attach: None,
                        frame_callback: None,
                        role: None,
                        xwl: None,
                        window: None,
                        output_key: None,
                    }
                    .into()
                });
            }
            Request::<WlCompositor>::CreateRegion { id } => {
                let c_region = client.create_region(&state.qh, ());
                data_init.init(id, c_region);
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

impl<C: XConnection> Dispatch<WlShmPool, client::wl_shm_pool::WlShmPool> for ServerState<C> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlShmPool,
        request: <WlShmPool as Resource>::Request,
        c_pool: &client::wl_shm_pool::WlShmPool,
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
                    let client = c_pool.create_buffer(
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
            Request::<WlShmPool>::Resize { size } => {
                c_pool.resize(size);
            }
            Request::<WlShmPool>::Destroy => {
                c_pool.destroy();
                state.clientside.queue.flush().unwrap();
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
                let c_surface = surface.and_then(|s| state.get_client_surface_from_server(s));
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

impl<C: XConnection> Dispatch<WlTouch, ObjectKey> for ServerState<C> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlTouch,
        request: <WlTouch as Resource>::Request,
        key: &ObjectKey,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            Request::<WlTouch>::Release => {
                let Touch { client, .. }: &_ = state.objects[*key].as_ref();
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
            Request::<WlSeat>::GetTouch { id } => {
                state
                    .objects
                    .insert_from_other_objects([*key], |[seat_obj], key| {
                        let Seat { client, .. }: &Seat = seat_obj.try_into().unwrap();
                        let client = client.get_touch(&state.qh, key);
                        let server = data_init.init(id, key);
                        Touch { client, server }.into()
                    });
            }
            other => warn!("unhandled seat request: {other:?}"),
        }
    }
}
only_destroy_request_impl!(RelativePointer);

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
            wp_viewporter::Request::GetViewport { id, surface } => 'get_viewport: {
                let Some(c_surface) = state.get_client_surface_from_server(surface) else {
                    break 'get_viewport;
                };
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

        let output: &mut Output = state.objects[*key].as_mut();
        let xdg_output = output.xdg.take().unwrap();
        xdg_output.client.destroy();
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
                let output: &mut Output = state.objects[output_key].as_mut();
                let client = client.get_xdg_output(&output.client, &state.qh, output_key);
                let server = data_init.init(id, output_key);
                output.xdg = Some(XdgOutput { client, server });
            }
            s_output_man::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl<C: XConnection> Dispatch<ConfinedPointerServer, ObjectKey> for ServerState<C> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &ConfinedPointerServer,
        request: <ConfinedPointerServer as Resource>::Request,
        key: &ObjectKey,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let confined_ptr: &ConfinedPointer = state.objects[*key].as_ref();
        simple_event_shunt! {
            confined_ptr.client, request: cp::Request => [
                SetRegion {
                    |region| region.as_ref().map(|r| r.data().unwrap())
                },
                Destroy
            ]
        }
    }
}

impl<C: XConnection> Dispatch<LockedPointerServer, ObjectKey> for ServerState<C> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &LockedPointerServer,
        request: <LockedPointerServer as Resource>::Request,
        key: &ObjectKey,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let locked_ptr: &LockedPointer = state.objects[*key].as_ref();
        simple_event_shunt! {
            locked_ptr.client, request: lp::Request => [
                SetCursorPositionHint { surface_x, surface_y },
                SetRegion {
                    |region| region.as_ref().map(|r| r.data().unwrap())
                },
                Destroy
            ]
        }
    }
}

impl<C: XConnection>
    Dispatch<PointerConstraintsServer, ClientGlobalWrapper<PointerConstraintsClient>>
    for ServerState<C>
{
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &PointerConstraintsServer,
        request: <PointerConstraintsServer as Resource>::Request,
        client: &ClientGlobalWrapper<PointerConstraintsClient>,
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        use pc::Request;

        match request {
            Request::ConfinePointer {
                id,
                surface,
                pointer,
                region,
                lifetime,
            } => {
                let surf_key: ObjectKey = surface.data().copied().unwrap();
                let ptr_key: ObjectKey = pointer.data().copied().unwrap();
                state.objects.insert_from_other_objects(
                    [surf_key, ptr_key],
                    |[surf_obj, ptr_obj], key| {
                        let SurfaceData {
                            client: c_surface, ..
                        }: &SurfaceData = surf_obj.try_into().unwrap();
                        let Pointer { client: c_ptr, .. }: &Pointer = ptr_obj.try_into().unwrap();

                        let client = client.confine_pointer(
                            c_surface,
                            c_ptr,
                            region.as_ref().map(|r| r.data().unwrap()),
                            convert_wenum(lifetime),
                            &state.qh,
                            key,
                        );
                        let server = data_init.init(id, key);

                        ConfinedPointer { client, server }.into()
                    },
                );
            }
            Request::LockPointer {
                id,
                surface,
                pointer,
                region,
                lifetime,
            } => {
                let surf_key: ObjectKey = surface.data().copied().unwrap();
                let ptr_key: ObjectKey = pointer.data().copied().unwrap();
                state.objects.insert_from_other_objects(
                    [surf_key, ptr_key],
                    |[surf_obj, ptr_obj], key| {
                        let SurfaceData {
                            client: c_surface, ..
                        }: &SurfaceData = surf_obj.try_into().unwrap();
                        let Pointer { client: c_ptr, .. }: &Pointer = ptr_obj.try_into().unwrap();
                        let client = client.lock_pointer(
                            c_surface,
                            c_ptr,
                            region.as_ref().map(|r| r.data().unwrap()),
                            convert_wenum(lifetime),
                            &state.qh,
                            key,
                        );
                        let server = data_init.init(id, key);
                        LockedPointer { client, server }.into()
                    },
                );
            }
            Request::Destroy => {
                client.destroy();
            }
            _ => unreachable!("unhandled pointer constraints request"),
        }
    }
}

impl<C: XConnection>
    Dispatch<
        s_tablet::zwp_tablet_manager_v2::ZwpTabletManagerV2,
        ClientGlobalWrapper<c_tablet::zwp_tablet_manager_v2::ZwpTabletManagerV2>,
    > for ServerState<C>
{
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &s_tablet::zwp_tablet_manager_v2::ZwpTabletManagerV2,
        request: <s_tablet::zwp_tablet_manager_v2::ZwpTabletManagerV2 as Resource>::Request,
        client: &ClientGlobalWrapper<c_tablet::zwp_tablet_manager_v2::ZwpTabletManagerV2>,
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        use s_tablet::zwp_tablet_manager_v2::Request::*;
        match request {
            GetTabletSeat { tablet_seat, seat } => {
                let seat_key: ObjectKey = seat.data().copied().unwrap();
                state
                    .objects
                    .insert_from_other_objects([seat_key], |[seat_obj], key| {
                        let Seat { client: c_seat, .. }: &Seat = seat_obj.try_into().unwrap();
                        let client = client.get_tablet_seat(c_seat, &state.qh, key);
                        let server = data_init.init(tablet_seat, key);
                        TabletSeat { client, server }.into()
                    });
            }
            other => {
                warn!("unhandled tablet request: {other:?}");
            }
        }
    }
}

only_destroy_request_impl!(TabletSeat);
only_destroy_request_impl!(Tablet);
only_destroy_request_impl!(TabletPadGroup);

impl<C: XConnection> Dispatch<s_tablet::zwp_tablet_pad_v2::ZwpTabletPadV2, ObjectKey>
    for ServerState<C>
{
    fn request(
        state: &mut Self,
        _: &Client,
        _: &s_tablet::zwp_tablet_pad_v2::ZwpTabletPadV2,
        request: <s_tablet::zwp_tablet_pad_v2::ZwpTabletPadV2 as Resource>::Request,
        key: &ObjectKey,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let pad: &TabletPad = state.objects[*key].as_ref();
        match request {
            s_tablet::zwp_tablet_pad_v2::Request::SetFeedback {
                button,
                description,
                serial,
            } => {
                pad.client.set_feedback(button, description, serial);
            }
            s_tablet::zwp_tablet_pad_v2::Request::Destroy => {
                pad.client.destroy();
                state.objects.remove(*key);
            }
            other => warn!("unhandled tablet pad request: {other:?}"),
        }
    }
}

impl<C: XConnection> Dispatch<s_tablet::zwp_tablet_tool_v2::ZwpTabletToolV2, ObjectKey>
    for ServerState<C>
{
    fn request(
        state: &mut Self,
        _: &Client,
        _: &s_tablet::zwp_tablet_tool_v2::ZwpTabletToolV2,
        request: <s_tablet::zwp_tablet_tool_v2::ZwpTabletToolV2 as Resource>::Request,
        key: &ObjectKey,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let tool: &TabletTool = state.objects[*key].as_ref();
        match request {
            s_tablet::zwp_tablet_tool_v2::Request::SetCursor {
                serial,
                surface,
                hotspot_x,
                hotspot_y,
            } => {
                let surf_key: Option<ObjectKey> = surface.map(|s| s.data().copied().unwrap());
                let c_surface = surf_key.map(|key| {
                    let d: &SurfaceData = state.objects[key].as_ref();
                    &d.client
                });
                tool.client
                    .set_cursor(serial, c_surface, hotspot_x, hotspot_y);
            }
            s_tablet::zwp_tablet_tool_v2::Request::Destroy => {
                tool.client.destroy();
                state.objects.remove(*key);
            }
            other => warn!("unhandled tablet tool request: {other:?}"),
        }
    }
}

impl<C: XConnection> Dispatch<s_tablet::zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2, ObjectKey>
    for ServerState<C>
{
    fn request(
        state: &mut Self,
        _: &Client,
        _: &s_tablet::zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2,
        request: <s_tablet::zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2 as Resource>::Request,
        key: &ObjectKey,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let ring: &TabletPadRing = state.objects[*key].as_ref();
        match request {
            s_tablet::zwp_tablet_pad_ring_v2::Request::SetFeedback {
                description,
                serial,
            } => {
                ring.client.set_feedback(description, serial);
            }
            s_tablet::zwp_tablet_pad_ring_v2::Request::Destroy => {
                ring.client.destroy();
                state.objects.remove(*key);
            }
            other => warn!("unhandled tablet pad ring requst: {other:?}"),
        }
    }
}

impl<C: XConnection> Dispatch<s_tablet::zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2, ObjectKey>
    for ServerState<C>
{
    fn request(
        state: &mut Self,
        _: &Client,
        _: &s_tablet::zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2,
        request: <s_tablet::zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2 as Resource>::Request,
        key: &ObjectKey,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let strip: &TabletPadStrip = state.objects[*key].as_ref();
        match request {
            s_tablet::zwp_tablet_pad_strip_v2::Request::SetFeedback {
                description,
                serial,
            } => {
                strip.client.set_feedback(description, serial);
            }
            s_tablet::zwp_tablet_pad_strip_v2::Request::Destroy => {
                strip.client.destroy();
                state.objects.remove(*key);
            }
            other => warn!("unhandled tablet pad strip requst: {other:?}"),
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
        impl<C: XConnection> GlobalDispatch<$server, Global> for ServerState<C>
        where
            ServerState<C>: Dispatch<$server, ClientGlobalWrapper<$client>>,
            Globals: wayland_client::Dispatch<$client, ()>,
        {
            fn bind(
                state: &mut Self,
                _: &DisplayHandle,
                _: &wayland_server::Client,
                resource: wayland_server::New<$server>,
                data: &Global,
                data_init: &mut wayland_server::DataInit<'_, Self>,
            ) {
                let client = ClientGlobalWrapper::<$client>::default();
                let server = data_init.init(resource, client.clone());
                client
                    .0
                    .set(
                        state
                            .clientside
                            .global_list
                            .registry()
                            .bind::<$client, _, _>(data.name, server.version(), &state.qh, ()),
                    )
                    .unwrap();
            }
        }
    };
}

macro_rules! global_dispatch_with_events {
    ($server:ty, $client:ty) => {
        impl<C: XConnection> GlobalDispatch<$server, Global> for ServerState<C>
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
                data: &Global,
                data_init: &mut wayland_server::DataInit<'_, Self>,
            ) {
                state.objects.insert_with_key(|key| {
                    let server = data_init.init(resource, key);
                    let client = state
                        .clientside
                        .global_list
                        .registry()
                        .bind::<$client, _, _>(data.name, server.version(), &state.qh, key);
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
global_dispatch_no_events!(PointerConstraintsServer, PointerConstraintsClient);
global_dispatch_no_events!(
    s_tablet::zwp_tablet_manager_v2::ZwpTabletManagerV2,
    c_tablet::zwp_tablet_manager_v2::ZwpTabletManagerV2
);

impl<C: XConnection> GlobalDispatch<WlSeat, Global> for ServerState<C>
where
    WlSeat: Resource,
    client::wl_seat::WlSeat: Proxy,
    ServerState<C>: Dispatch<WlSeat, ObjectKey>,
    Globals: wayland_client::Dispatch<client::wl_seat::WlSeat, ObjectKey>,
    GenericObject<WlSeat, client::wl_seat::WlSeat>: Into<Object>,
{
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        _: &wayland_server::Client,
        resource: wayland_server::New<WlSeat>,
        data: &Global,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        state.objects.insert_with_key(|key| {
            let server = data_init.init(resource, key);
            let client = state
                .clientside
                .global_list
                .registry()
                .bind::<client::wl_seat::WlSeat, _, _>(data.name, server.version(), &state.qh, key);
            if let Some(c) = &mut state.clipboard_data {
                c.device = Some(c.manager.get_data_device(&state.qh, &client));
            }
            GenericObject { server, client }.into()
        });
    }
}
impl<C: XConnection> GlobalDispatch<WlOutput, Global> for ServerState<C> {
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        _: &wayland_server::Client,
        resource: wayland_server::New<WlOutput>,
        data: &Global,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let key = state.objects.insert_with_key(|key| {
            let server = data_init.init(resource, key);
            let client = state
                .clientside
                .global_list
                .registry()
                .bind::<client::wl_output::WlOutput, _, _>(
                    data.name,
                    server.version(),
                    &state.qh,
                    key,
                );
            Output::new(client, server).into()
        });
        state.output_keys.insert(key, ());
    }
}
global_dispatch_with_events!(WlDrmServer, WlDrmClient);

impl<C: XConnection> GlobalDispatch<XwaylandShellV1, ()> for ServerState<C> {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &wayland_server::Client,
        resource: wayland_server::New<XwaylandShellV1>,
        _: &(),
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl<C: XConnection> Dispatch<XwaylandShellV1, ()> for ServerState<C> {
    fn request(
        state: &mut Self,
        client: &wayland_server::Client,
        _: &XwaylandShellV1,
        request: <XwaylandShellV1 as Resource>::Request,
        _: &(),
        dhandle: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        use xwayland_shell_v1::Request;
        match request {
            Request::GetXwaylandSurface { id, surface } => {
                let key: ObjectKey = surface.data().copied().unwrap();
                let data: &mut SurfaceData = state.objects[key].as_mut();
                if data.xwl.is_some() {
                    error!("Surface {surface:?} already has the xwayland surface role!");
                    client.kill(
                        dhandle,
                        wayland_client::backend::protocol::ProtocolError {
                            code: 0,
                            object_id: surface.id().protocol_id(),
                            object_interface: "wl_surface".to_string(),
                            message: "Surface already has role".to_string(),
                        },
                    );
                    return;
                }

                let xwl = data_init.init(id, key);
                data.xwl = Some(xwl);
            }
            Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl<C: XConnection> Dispatch<XwaylandSurfaceV1, ObjectKey> for ServerState<C> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &XwaylandSurfaceV1,
        request: <XwaylandSurfaceV1 as Resource>::Request,
        key: &ObjectKey,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        use xwayland_surface_v1::Request;
        let data: &mut SurfaceData = state.objects[*key].as_mut();
        match request {
            Request::SetSerial {
                serial_lo,
                serial_hi,
            } => {
                let serial = [serial_lo, serial_hi];
                data.serial = Some(serial);
                if let Some((win, window_data)) =
                    state.windows.iter_mut().find_map(|(win, data)| {
                        Some(*win)
                            .zip((data.surface_serial.is_some_and(|s| s == serial)).then_some(data))
                    })
                {
                    debug!(
                        "associate surface {} with {:?}",
                        data.server.id().protocol_id(),
                        win
                    );
                    window_data.surface_key = Some(*key);
                    state.associated_windows.insert(*key, win);
                    if window_data.mapped {
                        state.create_role_window(win, *key);
                    }
                }
            }
            Request::Destroy => {
                data.xwl.take();
            }
            _ => unreachable!(),
        }
    }
}
