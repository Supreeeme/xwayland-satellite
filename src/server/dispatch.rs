use super::*;
use hecs::{CommandBuffer, DynamicBundle};
use log::{debug, error, trace, warn};
use macros::simple_event_shunt;
use std::sync::{Arc, OnceLock};
use wayland_client::globals::Global;
use wayland_protocols::{
    wp::{
        fractional_scale::v1::client::wp_fractional_scale_v1::WpFractionalScaleV1,
        linux_dmabuf::zv1::{client as c_dmabuf, server as s_dmabuf},
        pointer_constraints::zv1::{
            client::zwp_confined_pointer_v1::ZwpConfinedPointerV1 as ConfinedPointerClient,
            client::zwp_locked_pointer_v1::ZwpLockedPointerV1 as LockedPointerClient,
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
            client::zwp_relative_pointer_v1::ZwpRelativePointerV1 as RelativePointerClient,
            server::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1 as RelativePointerManServer,
            server::zwp_relative_pointer_v1::ZwpRelativePointerV1 as RelativePointerServer,
        },
        tablet::zv2::{client as c_tablet, server as s_tablet},
        viewporter::client::wp_viewport::WpViewport,
    },
    xdg::xdg_output::zv1::{
        client::zxdg_output_manager_v1::ZxdgOutputManagerV1 as OutputManClient,
        client::zxdg_output_v1::ZxdgOutputV1 as XdgOutputClient,
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

// noop
impl<S: X11Selection> Dispatch<WlCallback, ()> for InnerServerState<S> {
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

impl<S: X11Selection> Dispatch<WlSurface, Entity> for InnerServerState<S> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlSurface,
        request: <WlSurface as Resource>::Request,
        entity: &Entity,
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let data = state.world.entity(*entity).unwrap();
        let mut role = data.get::<&mut SurfaceRole>();
        let xdg = role.as_ref().and_then(|role| role.xdg());
        let configured = xdg.is_none_or(|xdg| xdg.configured);
        let client = data.get::<&client::wl_surface::WlSurface>().unwrap();

        let mut cmd = CommandBuffer::new();

        match request {
            Request::<WlSurface>::Attach { buffer, x, y } => {
                if buffer.is_none() {
                    trace!("xwayland attached null buffer to {client:?}");
                }
                let buffer = buffer.as_ref().map(|b| {
                    let entity: Entity = b.data().copied().unwrap();
                    state
                        .world
                        .get::<&client::wl_buffer::WlBuffer>(entity)
                        .unwrap()
                });

                if configured {
                    client.attach(buffer.as_deref(), x, y);
                } else {
                    let buffer = buffer.as_deref().cloned();
                    cmd.insert(*entity, (SurfaceAttach { buffer, x, y },));
                }
            }
            Request::<WlSurface>::DamageBuffer {
                x,
                y,
                width,
                height,
            } => {
                if configured {
                    client.damage_buffer(x, y, width, height);
                }
            }
            Request::<WlSurface>::Frame { callback } => {
                let cb = data_init.init(callback, ());
                if configured {
                    client.frame(&state.qh, cb);
                } else {
                    cmd.insert(*entity, (cb,));
                }
            }
            Request::<WlSurface>::Commit => {
                if configured {
                    client.commit();
                }
            }
            Request::<WlSurface>::Destroy => {
                if !data.has::<x::Window>() {
                    cmd.despawn(*entity);
                }

                if let Some(role) = role.as_mut() {
                    role.destroy();
                }
                client.destroy();

                debug!(
                    "deleting (surface {:?})",
                    data.get::<&WlSurface>().unwrap().id().protocol_id()
                );

                let mut query = data.query::<(&WpViewport, Option<&WpFractionalScaleV1>)>();
                let (viewport, fractional) = query.get().unwrap();
                viewport.destroy();

                cmd.remove::<event::SurfaceBundle>(*entity);
                if let Some(f) = fractional {
                    f.destroy();
                    cmd.remove_one::<WpFractionalScaleV1>(*entity);
                }
            }
            Request::<WlSurface>::SetBufferScale { scale } => {
                client.set_buffer_scale(scale);
            }
            Request::<WlSurface>::SetInputRegion { region } => {
                let region = region.as_ref().map(|r| r.data().unwrap());
                client.set_input_region(region);
            }
            other => warn!("unhandled surface request: {other:?}"),
        }

        drop(client);
        drop(role);

        cmd.run_on(&mut state.world);
    }
}

impl<S: X11Selection> Dispatch<WlRegion, client::wl_region::WlRegion> for InnerServerState<S> {
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

impl<S: X11Selection>
    Dispatch<WlCompositor, ClientGlobalWrapper<client::wl_compositor::WlCompositor>>
    for InnerServerState<S>
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
                let entity = state.world.reserve_entity();
                let client = client.create_surface(&state.qh, entity);
                let server = data_init.init(id, entity);
                debug!("new surface ({})", server.id());
                let viewport = state.viewporter.get_viewport(&client, &state.qh, ());
                let fractional = state
                    .fractional_scale
                    .as_ref()
                    .map(|f| f.get_fractional_scale(&client, &state.qh, entity));

                state.world.spawn_at(
                    entity,
                    event::SurfaceBundle {
                        client,
                        server,
                        viewport,
                        scale: SurfaceScaleFactor(1.0),
                    },
                );
                if let Some(f) = fractional {
                    state.world.insert(entity, (f,)).unwrap();
                }
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

impl<S: X11Selection> Dispatch<WlBuffer, Entity> for InnerServerState<S> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlBuffer,
        request: <WlBuffer as Resource>::Request,
        entity: &Entity,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        assert!(matches!(request, Request::<WlBuffer>::Destroy));

        state
            .world
            .get::<&client::wl_buffer::WlBuffer>(*entity)
            .unwrap()
            .destroy();
        state.world.despawn(*entity).unwrap();
    }
}

impl<S: X11Selection> Dispatch<WlShmPool, client::wl_shm_pool::WlShmPool> for InnerServerState<S> {
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
                let entity = state.world.reserve_entity();
                let client = c_pool.create_buffer(
                    offset,
                    width,
                    height,
                    stride,
                    convert_wenum(format),
                    &state.qh,
                    entity,
                );
                let server = data_init.init(id, entity);
                state.world.spawn_at(entity, (client, server));
            }
            Request::<WlShmPool>::Resize { size } => {
                c_pool.resize(size);
            }
            Request::<WlShmPool>::Destroy => {
                c_pool.destroy();
                state.queue.flush().unwrap();
            }
            other => warn!("unhandled shmpool request: {other:?}"),
        }
    }
}

impl<S: X11Selection> Dispatch<WlShm, ClientGlobalWrapper<client::wl_shm::WlShm>>
    for InnerServerState<S>
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

impl<S: X11Selection> Dispatch<WlPointer, Entity> for InnerServerState<S> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlPointer,
        request: <WlPointer as Resource>::Request,
        entity: &Entity,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            Request::<WlPointer>::SetCursor {
                serial,
                hotspot_x,
                hotspot_y,
                surface,
            } => {
                let c_pointer = state
                    .world
                    .get::<&client::wl_pointer::WlPointer>(*entity)
                    .unwrap();

                let c_surface = surface.and_then(|s| {
                    let e = s.data().copied()?;
                    Some(
                        state
                            .world
                            .get::<&client::wl_surface::WlSurface>(e)
                            .unwrap(),
                    )
                });
                c_pointer.set_cursor(serial, c_surface.as_deref(), hotspot_x, hotspot_y);
            }
            Request::<WlPointer>::Release => {
                let (client, _) = state
                    .world
                    .remove::<(client::wl_pointer::WlPointer, WlPointer)>(*entity)
                    .unwrap();
                client.release();
            }
            _ => warn!("unhandled cursor request: {request:?}"),
        }
    }
}

impl<S: X11Selection> Dispatch<WlKeyboard, Entity> for InnerServerState<S> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlKeyboard,
        request: <WlKeyboard as Resource>::Request,
        entity: &Entity,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            Request::<WlKeyboard>::Release => {
                let (client, _) = state
                    .world
                    .remove::<(client::wl_keyboard::WlKeyboard, WlKeyboard)>(*entity)
                    .unwrap();
                client.release();
            }
            _ => unreachable!(),
        }
    }
}

impl<S: X11Selection> Dispatch<WlTouch, Entity> for InnerServerState<S> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlTouch,
        request: <WlTouch as Resource>::Request,
        entity: &Entity,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            Request::<WlTouch>::Release => {
                state
                    .world
                    .get::<&client::wl_touch::WlTouch>(*entity)
                    .unwrap()
                    .release();
            }
            _ => unreachable!(),
        }
    }
}

impl<S: X11Selection> Dispatch<WlSeat, Entity> for InnerServerState<S> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlSeat,
        request: <WlSeat as Resource>::Request,
        entity: &Entity,
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            Request::<WlSeat>::GetPointer { id } => {
                let client = {
                    state
                        .world
                        .get::<&client::wl_seat::WlSeat>(*entity)
                        .unwrap()
                        .get_pointer(&state.qh, *entity)
                };
                let server = data_init.init(id, *entity);
                state.world.insert(*entity, (client, server)).unwrap();
            }
            Request::<WlSeat>::GetKeyboard { id } => {
                let client = {
                    state
                        .world
                        .get::<&client::wl_seat::WlSeat>(*entity)
                        .unwrap()
                        .get_keyboard(&state.qh, *entity)
                };
                let server = data_init.init(id, *entity);
                state.world.insert(*entity, (client, server)).unwrap();
            }
            Request::<WlSeat>::GetTouch { id } => {
                let client = {
                    state
                        .world
                        .get::<&client::wl_seat::WlSeat>(*entity)
                        .unwrap()
                        .get_touch(&state.qh, *entity)
                };
                let server = data_init.init(id, *entity);
                state.world.insert(*entity, (client, server)).unwrap();
            }
            other => warn!("unhandled seat request: {other:?}"),
        }
    }
}

macro_rules! only_destroy_request_impl {
    ($server:ty, $client:ty) => {
        impl<S: X11Selection> Dispatch<$server, Entity> for InnerServerState<S> {
            fn request(
                state: &mut Self,
                _: &Client,
                _: &$server,
                request: <$server as Resource>::Request,
                entity: &Entity,
                _: &DisplayHandle,
                _: &mut wayland_server::DataInit<'_, Self>,
            ) {
                if !matches!(request, <$server as Resource>::Request::Destroy) {
                    warn!(
                        "unrecognized {} request: {:?}",
                        stringify!($server),
                        request
                    );
                    return;
                }

                state.world.get::<&$client>(*entity).unwrap().destroy();
                state.world.despawn(*entity).unwrap();
            }
        }
    };
}

only_destroy_request_impl!(RelativePointerServer, RelativePointerClient);

impl<S: X11Selection>
    Dispatch<RelativePointerManServer, ClientGlobalWrapper<RelativePointerManClient>>
    for InnerServerState<S>
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
                let pointer_entity: Entity = pointer.data().copied().unwrap();
                let entity = state.world.reserve_entity();
                let client = {
                    let client_pointer = state
                        .world
                        .get::<&client::wl_pointer::WlPointer>(pointer_entity)
                        .unwrap();

                    client.get_relative_pointer(&client_pointer, &state.qh, entity)
                };
                let server = data_init.init(id, entity);
                state.world.spawn_at(entity, (server, client));
            }
            _ => warn!("unhandled relative pointer request: {request:?}"),
        }
    }
}

impl<S: X11Selection> Dispatch<WlOutput, Entity> for InnerServerState<S> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlOutput,
        request: <WlOutput as Resource>::Request,
        entity: &Entity,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            wayland_server::protocol::wl_output::Request::Release => {
                state
                    .world
                    .get::<&client::wl_output::WlOutput>(*entity)
                    .unwrap()
                    .release();
                todo!("handle wloutput destruction");
            }
            _ => warn!("unhandled output request {request:?}"),
        }
    }
}

impl<S: X11Selection>
    Dispatch<s_dmabuf::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1, Entity>
    for InnerServerState<S>
{
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &s_dmabuf::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
        request: <s_dmabuf::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1 as Resource>::Request,
        entity: &Entity,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        use s_dmabuf::zwp_linux_dmabuf_feedback_v1::Request::*;
        match request {
            Destroy => {
                state
                    .world
                    .get::<&c_dmabuf::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1>(
                        *entity,
                    )
                    .unwrap()
                    .destroy();
                state.world.despawn(*entity).unwrap();
            }
            _ => unreachable!(),
        }
    }
}

impl<S: X11Selection>
    Dispatch<
        s_dmabuf::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        c_dmabuf::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    > for InnerServerState<S>
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
                let entity = state.world.reserve_entity();
                let client = c_params.create_immed(
                    width,
                    height,
                    format,
                    convert_wenum(flags),
                    &state.qh,
                    entity,
                );
                let server = data_init.init(buffer_id, entity);
                state.world.spawn_at(entity, (client, server));
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

impl<S: X11Selection>
    Dispatch<
        s_dmabuf::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        ClientGlobalWrapper<c_dmabuf::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    > for InnerServerState<S>
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
                let entity = state.world.reserve_entity();
                let client = client.get_default_feedback(&state.qh, entity);
                let server = data_init.init(id, entity);
                state.world.spawn_at(entity, (client, server));
            }
            GetSurfaceFeedback { id, surface } => {
                let entity = state.world.reserve_entity();
                let surface_entity: Entity = surface.data().copied().unwrap();
                let client = {
                    let c_surface = state
                        .world
                        .get::<&client::wl_surface::WlSurface>(surface_entity)
                        .unwrap();
                    client.get_surface_feedback(&c_surface, &state.qh, entity)
                };
                let server = data_init.init(id, entity);

                state.world.spawn_at(entity, (client, server));
            }
            _ => warn!("unhandled dmabuf request: {request:?}"),
        }
    }
}

impl<S: X11Selection> Dispatch<WlDrmServer, Entity> for InnerServerState<S> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &WlDrmServer,
        request: <WlDrmServer as Resource>::Request,
        entity: &Entity,
        _: &DisplayHandle,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        use wl_drm::server::wl_drm::Request::*;

        type DrmFn = dyn FnOnce(
            &wl_drm::client::wl_drm::WlDrm,
            Entity,
            &QueueHandle<MyWorld>,
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
                state
                    .world
                    .get::<&wl_drm::client::wl_drm::WlDrm>(*entity)
                    .unwrap()
                    .authenticate(id);
            }
            _ => unreachable!(),
        }

        if let Some((buf_create, id)) = bufs {
            let new_entity = state.world.reserve_entity();
            let client = {
                let drm_client = state
                    .world
                    .get::<&wl_drm::client::wl_drm::WlDrm>(*entity)
                    .unwrap();
                buf_create(&drm_client, new_entity, &state.qh)
            };
            let server = data_init.init(id, new_entity);
            state.world.spawn_at(new_entity, (client, server));
        }
    }
}

impl<S: X11Selection> Dispatch<XdgOutputServer, Entity> for InnerServerState<S> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &XdgOutputServer,
        request: <XdgOutputServer as Resource>::Request,
        entity: &Entity,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let s_xdgo::Request::Destroy = request else {
            unreachable!();
        };

        let (client, _) = state
            .world
            .query_one_mut::<(&XdgOutputClient, &XdgOutputServer)>(*entity)
            .unwrap();
        client.destroy();
    }
}

impl<S: X11Selection> Dispatch<OutputManServer, ClientGlobalWrapper<OutputManClient>>
    for InnerServerState<S>
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
                let entity: Entity = output.data().copied().unwrap();
                let client = {
                    let c_output = state
                        .world
                        .get::<&client::wl_output::WlOutput>(entity)
                        .unwrap();
                    client.get_xdg_output(&c_output, &state.qh, entity)
                };
                let server = data_init.init(id, entity);
                state.world.insert(entity, (client, server)).unwrap();
            }
            s_output_man::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl<S: X11Selection> Dispatch<ConfinedPointerServer, Entity> for InnerServerState<S> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &ConfinedPointerServer,
        request: <ConfinedPointerServer as Resource>::Request,
        entity: &Entity,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let client = state.world.get::<&ConfinedPointerClient>(*entity).unwrap();
        simple_event_shunt! {
            client, request: cp::Request => [
                SetRegion {
                    |region| region.as_ref().map(|r| r.data().unwrap())
                },
                Destroy
            ]
        }
    }
}

impl<S: X11Selection> Dispatch<LockedPointerServer, Entity> for InnerServerState<S> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &LockedPointerServer,
        request: <LockedPointerServer as Resource>::Request,
        entity: &Entity,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            lp::Request::SetCursorPositionHint {
                surface_x,
                surface_y,
            } => {
                let (client, scale) = state
                    .world
                    .query_one_mut::<(&LockedPointerClient, &SurfaceScaleFactor)>(*entity)
                    .unwrap();

                // Xwayland believes that the surface is actually <surface scale factor> times bigger
                // than it currently is, and therefore that the cursor position is also scaled up by the same
                // amount. So we need to divide the cursor position from Xwayland by the surface scale
                // to get where the cursor should actually be positioned.

                client.set_cursor_position_hint(surface_x / scale.0, surface_y / scale.0);
            }
            lp::Request::Destroy => {
                {
                    let client = state.world.get::<&LockedPointerClient>(*entity).unwrap();
                    client.destroy();
                }
                state.world.despawn(*entity).unwrap();
            }
            _ => warn!("unhandled locked pointer request: {request:?}"),
        }
    }
}

impl<S: X11Selection>
    Dispatch<PointerConstraintsServer, ClientGlobalWrapper<PointerConstraintsClient>>
    for InnerServerState<S>
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
                let surf_key: Entity = surface.data().copied().unwrap();
                let ptr_key: Entity = pointer.data().copied().unwrap();

                let entity = state.world.reserve_entity();
                let client = {
                    let c_surface = state
                        .world
                        .get::<&client::wl_surface::WlSurface>(surf_key)
                        .unwrap();
                    let c_ptr = state
                        .world
                        .get::<&client::wl_pointer::WlPointer>(ptr_key)
                        .unwrap();
                    client.confine_pointer(
                        &c_surface,
                        &c_ptr,
                        region.as_ref().map(|r| r.data().unwrap()),
                        convert_wenum(lifetime),
                        &state.qh,
                        entity,
                    )
                };
                let server = data_init.init(id, entity);

                state.world.spawn_at(entity, (client, server));
            }
            Request::LockPointer {
                id,
                surface,
                pointer,
                region,
                lifetime,
            } => {
                let surf_key: Entity = surface.data().copied().unwrap();
                let ptr_key: Entity = pointer.data().copied().unwrap();
                let entity = state.world.reserve_entity();

                let client = {
                    let c_surface = state
                        .world
                        .get::<&client::wl_surface::WlSurface>(surf_key)
                        .unwrap();
                    let c_ptr = state
                        .world
                        .get::<&client::wl_pointer::WlPointer>(ptr_key)
                        .unwrap();
                    client.lock_pointer(
                        &c_surface,
                        &c_ptr,
                        region.as_ref().map(|r| r.data().unwrap()),
                        convert_wenum(lifetime),
                        &state.qh,
                        entity,
                    )
                };
                let server = data_init.init(id, entity);
                let surface_scale = state
                    .world
                    .get::<&SurfaceScaleFactor>(surf_key)
                    .as_deref()
                    .copied()
                    .unwrap();

                state
                    .world
                    .spawn_at(entity, (client, server, surface_scale));
            }
            Request::Destroy => {
                client.destroy();
            }
            _ => unreachable!("unhandled pointer constraints request"),
        }
    }
}

impl<S: X11Selection>
    Dispatch<
        s_tablet::zwp_tablet_manager_v2::ZwpTabletManagerV2,
        ClientGlobalWrapper<c_tablet::zwp_tablet_manager_v2::ZwpTabletManagerV2>,
    > for InnerServerState<S>
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
                let seat_key: Entity = seat.data().copied().unwrap();

                let entity = state.world.reserve_entity();
                let client = {
                    let c_seat = state
                        .world
                        .get::<&client::wl_seat::WlSeat>(seat_key)
                        .unwrap();
                    client.get_tablet_seat(&c_seat, &state.qh, entity)
                };
                let server = data_init.init(tablet_seat, entity);

                state.world.spawn_at(entity, (client, server));
            }
            other => {
                warn!("unhandled tablet request: {other:?}");
            }
        }
    }
}

only_destroy_request_impl!(
    s_tablet::zwp_tablet_seat_v2::ZwpTabletSeatV2,
    c_tablet::zwp_tablet_seat_v2::ZwpTabletSeatV2
);
only_destroy_request_impl!(
    s_tablet::zwp_tablet_v2::ZwpTabletV2,
    c_tablet::zwp_tablet_v2::ZwpTabletV2
);
only_destroy_request_impl!(
    s_tablet::zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2,
    c_tablet::zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2
);

impl<S: X11Selection> Dispatch<s_tablet::zwp_tablet_pad_v2::ZwpTabletPadV2, Entity>
    for InnerServerState<S>
{
    fn request(
        state: &mut Self,
        _: &Client,
        _: &s_tablet::zwp_tablet_pad_v2::ZwpTabletPadV2,
        request: <s_tablet::zwp_tablet_pad_v2::ZwpTabletPadV2 as Resource>::Request,
        entity: &Entity,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let client = state
            .world
            .get::<&c_tablet::zwp_tablet_pad_v2::ZwpTabletPadV2>(*entity)
            .unwrap();
        match request {
            s_tablet::zwp_tablet_pad_v2::Request::SetFeedback {
                button,
                description,
                serial,
            } => {
                client.set_feedback(button, description, serial);
            }
            s_tablet::zwp_tablet_pad_v2::Request::Destroy => {
                client.destroy();
                drop(client);
                state.world.despawn(*entity).unwrap();
            }
            other => warn!("unhandled tablet pad request: {other:?}"),
        }
    }
}

impl<S: X11Selection> Dispatch<s_tablet::zwp_tablet_tool_v2::ZwpTabletToolV2, Entity>
    for InnerServerState<S>
{
    fn request(
        state: &mut Self,
        _: &Client,
        _: &s_tablet::zwp_tablet_tool_v2::ZwpTabletToolV2,
        request: <s_tablet::zwp_tablet_tool_v2::ZwpTabletToolV2 as Resource>::Request,
        entity: &Entity,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let client = state
            .world
            .get::<&c_tablet::zwp_tablet_tool_v2::ZwpTabletToolV2>(*entity)
            .unwrap();
        match request {
            s_tablet::zwp_tablet_tool_v2::Request::SetCursor {
                serial,
                surface,
                hotspot_x,
                hotspot_y,
            } => {
                let surf_key: Option<Entity> = surface.map(|s| s.data().copied().unwrap());
                let c_surface = surf_key.map(|key| {
                    state
                        .world
                        .get::<&client::wl_surface::WlSurface>(key)
                        .unwrap()
                });
                client.set_cursor(serial, c_surface.as_deref(), hotspot_x, hotspot_y);
            }
            s_tablet::zwp_tablet_tool_v2::Request::Destroy => {
                client.destroy();
                drop(client);
                state.world.despawn(*entity).unwrap();
            }
            other => warn!("unhandled tablet tool request: {other:?}"),
        }
    }
}

impl<S: X11Selection> Dispatch<s_tablet::zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2, Entity>
    for InnerServerState<S>
{
    fn request(
        state: &mut Self,
        _: &Client,
        _: &s_tablet::zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2,
        request: <s_tablet::zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2 as Resource>::Request,
        entity: &Entity,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let client = state
            .world
            .get::<&c_tablet::zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2>(*entity)
            .unwrap();
        match request {
            s_tablet::zwp_tablet_pad_ring_v2::Request::SetFeedback {
                description,
                serial,
            } => {
                client.set_feedback(description, serial);
            }
            s_tablet::zwp_tablet_pad_ring_v2::Request::Destroy => {
                client.destroy();
                drop(client);
                state.world.despawn(*entity).unwrap();
            }
            other => warn!("unhandled tablet pad ring request: {other:?}"),
        }
    }
}

impl<S: X11Selection> Dispatch<s_tablet::zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2, Entity>
    for InnerServerState<S>
{
    fn request(
        state: &mut Self,
        _: &Client,
        _: &s_tablet::zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2,
        request: <s_tablet::zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2 as Resource>::Request,
        entity: &Entity,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let client = state
            .world
            .get::<&c_tablet::zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2>(*entity)
            .unwrap();

        match request {
            s_tablet::zwp_tablet_pad_strip_v2::Request::SetFeedback {
                description,
                serial,
            } => {
                client.set_feedback(description, serial);
            }
            s_tablet::zwp_tablet_pad_strip_v2::Request::Destroy => {
                client.destroy();
                drop(client);
                state.world.despawn(*entity).unwrap();
            }
            other => warn!("unhandled tablet pad strip request: {other:?}"),
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
        impl<S: X11Selection> GlobalDispatch<$server, Global> for InnerServerState<S>
        where
            InnerServerState<S>: Dispatch<$server, ClientGlobalWrapper<$client>>,
            MyWorld: wayland_client::Dispatch<$client, ()>,
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
                    .set(state.world.global_list.registry().bind::<$client, _, _>(
                        data.name,
                        server.version(),
                        &state.qh,
                        (),
                    ))
                    .unwrap();
            }
        }
    };
}

macro_rules! global_dispatch_with_events {
    ($server:ty, $client:ty) => {
        impl<S: X11Selection> GlobalDispatch<$server, Global> for InnerServerState<S>
        where
            $server: Resource,
            $client: Proxy,
            InnerServerState<S>: Dispatch<$server, Entity>,
            MyWorld: wayland_client::Dispatch<$client, Entity>,
        {
            fn bind(
                state: &mut Self,
                _: &DisplayHandle,
                _: &wayland_server::Client,
                resource: wayland_server::New<$server>,
                data: &Global,
                data_init: &mut wayland_server::DataInit<'_, Self>,
            ) {
                let entity = state.world.reserve_entity();
                let server = data_init.init(resource, entity);
                let client = state.world.global_list.registry().bind::<$client, _, _>(
                    data.name,
                    server.version(),
                    &state.qh,
                    entity,
                );
                state.world.spawn_at(entity, (server, client));
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
global_dispatch_no_events!(PointerConstraintsServer, PointerConstraintsClient);
global_dispatch_no_events!(
    s_tablet::zwp_tablet_manager_v2::ZwpTabletManagerV2,
    c_tablet::zwp_tablet_manager_v2::ZwpTabletManagerV2
);

impl<S: X11Selection> GlobalDispatch<WlSeat, Global> for InnerServerState<S> {
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        _: &wayland_server::Client,
        resource: wayland_server::New<WlSeat>,
        data: &Global,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let entity = state.world.reserve_entity();
        let server = data_init.init(resource, entity);
        let client = state
            .world
            .global_list
            .registry()
            .bind::<client::wl_seat::WlSeat, _, _>(data.name, server.version(), &state.qh, entity);

        state.selection_states.seat_created(&state.qh, &client);
        state.world.spawn_at(entity, (server, client));
    }
}

impl<S: X11Selection> GlobalDispatch<WlOutput, Global> for InnerServerState<S> {
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        _: &wayland_server::Client,
        resource: wayland_server::New<WlOutput>,
        data: &Global,
        data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        let entity = state.world.reserve_entity();
        let server = data_init.init(resource, entity);
        let client = state
            .world
            .global_list
            .registry()
            .bind::<client::wl_output::WlOutput, _, _>(
                data.name,
                server.version(),
                &state.qh,
                entity,
            );
        state.world.spawn_at(
            entity,
            (
                server,
                client,
                event::OutputScaleFactor::Output(1),
                event::OutputDimensions::default(),
            ),
        );
        state.updated_outputs.push(entity);
    }
}
global_dispatch_with_events!(WlDrmServer, WlDrmClient);

impl<S: X11Selection> GlobalDispatch<XwaylandShellV1, ()> for InnerServerState<S> {
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

impl<S: X11Selection> Dispatch<XwaylandShellV1, ()> for InnerServerState<S> {
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
                let e: Entity = surface.data().copied().unwrap();
                if state.world.entity(e).unwrap().has::<XwaylandSurfaceV1>() {
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

                data_init.init(id, e);
            }
            Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl<S: X11Selection> Dispatch<XwaylandSurfaceV1, Entity> for InnerServerState<S> {
    fn request(
        state: &mut Self,
        _: &wayland_server::Client,
        _: &XwaylandSurfaceV1,
        request: <XwaylandSurfaceV1 as Resource>::Request,
        entity: &Entity,
        _: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, Self>,
    ) {
        use xwayland_surface_v1::Request;
        match request {
            Request::SetSerial {
                serial_lo,
                serial_hi,
            } => {
                let surface_id = state.world.get::<&WlSurface>(*entity).unwrap().id();
                let serial = SurfaceSerial([serial_lo, serial_hi]);
                let win_entity = state
                    .world
                    .query_mut::<&SurfaceSerial>()
                    .without::<&WlSurface>()
                    .into_iter()
                    .find(|(_, surface_serial)| **surface_serial == serial)
                    .map(|i| i.0);

                if let Some(win_entity) = win_entity {
                    let bundle = state.world.take(win_entity).unwrap();
                    if !bundle.has::<x::Window>() {
                        warn!("Window with same serial ({serial:?}) as {surface_id} has been destroyed?");
                        return;
                    }
                    let mut builder = hecs::EntityBuilder::new();
                    builder.add_bundle(bundle);
                    state.world.insert(*entity, builder.build()).unwrap();
                    state.world.remove_one::<SurfaceSerial>(*entity).unwrap();
                    let data = state.world.entity(*entity).unwrap();
                    let win = data.get::<&x::Window>().as_deref().copied().unwrap();
                    state.windows.insert(win, *entity);
                    debug!("associate {surface_id} with {win:?} (serial {serial:?})");
                    if data.get::<&WindowData>().unwrap().mapped
                        && state.create_role_window(win, *entity)
                    {
                        state.activate_window(win);
                    }
                } else {
                    state.world.insert(*entity, (serial,)).unwrap();
                }
            }
            Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}
