use crate::clientside::Globals;
use log::warn;
use smithay_client_toolkit::globals::GlobalData;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::WpCursorShapeDeviceV1,
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};

impl Dispatch<WpCursorShapeManagerV1, GlobalData> for Globals {
    fn event(
        _: &mut Self,
        _: &WpCursorShapeManagerV1,
        event: <WpCursorShapeManagerV1 as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        warn!("unhandled cursor shape manager event: {event:?}");
    }
}

impl Dispatch<WpCursorShapeDeviceV1, GlobalData> for Globals {
    fn event(
        _: &mut Self,
        _: &WpCursorShapeDeviceV1,
        event: <WpCursorShapeDeviceV1 as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        warn!("unhandled cursor shape device event: {event:?}");
    }
}
