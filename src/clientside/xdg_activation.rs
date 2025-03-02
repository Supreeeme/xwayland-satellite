use smithay_client_toolkit::{
    activation::{ActivationHandler, RequestData, RequestDataExt},
    delegate_activation,
};
use xcb::x;

use crate::clientside::Globals;

delegate_activation!(Globals, ActivationData);

pub struct ActivationData {
    window: x::Window,
    data: RequestData,
}

impl ActivationData {
    pub fn new(window: x::Window, data: RequestData) -> Self {
        Self { window, data }
    }
}

impl RequestDataExt for ActivationData {
    fn app_id(&self) -> Option<&str> {
        self.data.app_id()
    }

    fn seat_and_serial(&self) -> Option<(&wayland_client::protocol::wl_seat::WlSeat, u32)> {
        self.data.seat_and_serial()
    }

    fn surface(&self) -> Option<&wayland_client::protocol::wl_surface::WlSurface> {
        self.data.surface()
    }
}

impl ActivationHandler for Globals {
    type RequestData = ActivationData;

    fn new_token(&mut self, token: String, data: &Self::RequestData) {
        self.pending_activations.push((data.window, token));
    }
}
