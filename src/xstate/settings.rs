use super::XState;
use log::warn;
use std::collections::HashMap;
use xcb::x;

impl XState {
    pub(crate) fn set_xsettings_owner(&self) {
        self.connection
            .send_and_check_request(&x::SetSelectionOwner {
                owner: self.settings.window,
                selection: self.atoms.xsettings,
                time: x::CURRENT_TIME,
            })
            .unwrap();
        let reply = self
            .connection
            .wait_for_reply(self.connection.send_request(&x::GetSelectionOwner {
                selection: self.atoms.xsettings,
            }))
            .unwrap();

        if reply.owner() != self.settings.window {
            warn!(
                "Could not get XSETTINGS selection (owned by {:?})",
                reply.owner()
            );
        }
    }

    pub(crate) fn update_global_scale(&mut self, scale: i32) {
        self.settings.set_scale(scale);
        self.connection
            .send_and_check_request(&x::ChangeProperty {
                window: self.settings.window,
                mode: x::PropMode::Replace,
                property: self.atoms.xsettings_settings,
                r#type: self.atoms.xsettings_settings,
                data: &self.settings.as_data(),
            })
            .unwrap();
    }
}

/// The DPI consider 1x scale by X11.
const DEFAULT_DPI: i32 = 96;
/// I don't know why, but the DPI related xsettings seem to
/// divide the DPI by 1024.
const DPI_SCALE_FACTOR: i32 = 1024;

const XFT_DPI: &str = "Xft/DPI";
const GDK_WINDOW_SCALE: &str = "Gdk/WindowScalingFactor";
const GDK_UNSCALED_DPI: &str = "Gdk/UnscaledDPI";

pub(super) struct Settings {
    window: x::Window,
    serial: u32,
    settings: HashMap<&'static str, IntSetting>,
}

struct IntSetting {
    value: i32,
    last_change_serial: u32,
}

mod setting_type {
    pub const INTEGER: u8 = 0;
}

impl Settings {
    pub(super) fn new(connection: &xcb::Connection, atoms: &super::Atoms, root: x::Window) -> Self {
        let window = connection.generate_id();
        connection
            .send_and_check_request(&x::CreateWindow {
                wid: window,
                width: 1,
                height: 1,
                depth: 0,
                parent: root,
                x: 0,
                y: 0,
                border_width: 0,
                class: x::WindowClass::InputOnly,
                visual: x::COPY_FROM_PARENT,
                value_list: &[],
            })
            .expect("Couldn't create window for settings");

        let s = Settings {
            window,
            serial: 0,
            settings: HashMap::from([
                (
                    XFT_DPI,
                    IntSetting {
                        value: DEFAULT_DPI * DPI_SCALE_FACTOR,
                        last_change_serial: 0,
                    },
                ),
                (
                    GDK_WINDOW_SCALE,
                    IntSetting {
                        value: 1,
                        last_change_serial: 0,
                    },
                ),
                (
                    GDK_UNSCALED_DPI,
                    IntSetting {
                        value: DEFAULT_DPI * DPI_SCALE_FACTOR,
                        last_change_serial: 0,
                    },
                ),
            ]),
        };

        connection
            .send_and_check_request(&x::ChangeProperty {
                window,
                mode: x::PropMode::Replace,
                property: atoms.xsettings_settings,
                r#type: atoms.xsettings_settings,
                data: &s.as_data(),
            })
            .unwrap();

        s
    }

    fn as_data(&self) -> Vec<u8> {
        // https://specifications.freedesktop.org/xsettings-spec/0.5/#format

        let mut data = vec![
            // GTK seems to use this value for byte order from the X.h header,
            // so I assume I can use it too.
            x::ImageOrder::LsbFirst as u8,
            // unused
            0,
            0,
            0,
        ];

        data.extend_from_slice(&self.serial.to_le_bytes());
        data.extend_from_slice(&(self.settings.len() as u32).to_le_bytes());

        fn insert_with_padding(data: &[u8], out: &mut Vec<u8>) {
            out.extend_from_slice(data);
            // See https://x.org/releases/X11R7.7/doc/xproto/x11protocol.html#Syntactic_Conventions_b
            let num_padding_bytes = (4 - (data.len() % 4)) % 4;
            out.extend(std::iter::repeat_n(0, num_padding_bytes));
        }

        for (name, setting) in &self.settings {
            data.extend_from_slice(&[setting_type::INTEGER, 0]);
            data.extend_from_slice(&(name.len() as u16).to_le_bytes());
            insert_with_padding(name.as_bytes(), &mut data);
            data.extend_from_slice(&setting.last_change_serial.to_le_bytes());
            data.extend_from_slice(&setting.value.to_le_bytes());
        }

        data
    }

    fn set_scale(&mut self, scale: i32) {
        self.serial += 1;

        self.settings.entry(XFT_DPI).insert_entry(IntSetting {
            value: scale * DEFAULT_DPI * DPI_SCALE_FACTOR,
            last_change_serial: self.serial,
        });
        self.settings
            .entry(GDK_WINDOW_SCALE)
            .insert_entry(IntSetting {
                value: scale,
                last_change_serial: self.serial,
            });
    }
}
