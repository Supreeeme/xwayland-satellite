use crate::clientside::Globals;
use smithay_client_toolkit::{
    data_device_manager::{
        data_device::DataDeviceHandler, data_offer::DataOfferHandler,
        data_source::DataSourceHandler,
    },
    delegate_data_device,
};
delegate_data_device!(Globals);

impl DataDeviceHandler for Globals {
    fn selection(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        data_device: &wayland_client::protocol::wl_data_device::WlDataDevice,
    ) {
        self.selection = Some(data_device.clone());
    }

    fn drop_performed(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &wayland_client::protocol::wl_data_device::WlDataDevice,
    ) {
    }

    fn motion(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &wayland_client::protocol::wl_data_device::WlDataDevice,
        _: f64,
        _: f64,
    ) {
    }

    fn leave(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &wayland_client::protocol::wl_data_device::WlDataDevice,
    ) {
    }

    fn enter(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &wayland_client::protocol::wl_data_device::WlDataDevice,
        _: f64,
        _: f64,
        _: &wayland_client::protocol::wl_surface::WlSurface,
    ) {
    }
}

impl DataSourceHandler for Globals {
    fn send_request(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &wayland_client::protocol::wl_data_source::WlDataSource,
        mime: String,
        fd: smithay_client_toolkit::data_device_manager::WritePipe,
    ) {
        self.selection_requests.push((mime, fd));
    }

    fn cancelled(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &wayland_client::protocol::wl_data_source::WlDataSource,
    ) {
        self.cancelled = true;
    }

    fn action(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &wayland_client::protocol::wl_data_source::WlDataSource,
        _: wayland_client::protocol::wl_data_device_manager::DndAction,
    ) {
    }

    fn dnd_finished(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &wayland_client::protocol::wl_data_source::WlDataSource,
    ) {
    }

    fn dnd_dropped(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &wayland_client::protocol::wl_data_source::WlDataSource,
    ) {
    }

    fn accept_mime(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &wayland_client::protocol::wl_data_source::WlDataSource,
        _: Option<String>,
    ) {
    }
}

impl DataOfferHandler for Globals {
    fn selected_action(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &mut smithay_client_toolkit::data_device_manager::data_offer::DragOffer,
        _: wayland_client::protocol::wl_data_device_manager::DndAction,
    ) {
    }

    fn source_actions(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: &mut smithay_client_toolkit::data_device_manager::data_offer::DragOffer,
        _: wayland_client::protocol::wl_data_device_manager::DndAction,
    ) {
    }
}
