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

    pub(crate) fn update_global_scale(&mut self, scale: f64) {
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
const DEFAULT_CURSOR_SIZE: i32 = 24;
const DEFAULT_CURSOR_THEME: &str = "default";

const XFT_DPI: &str = "Xft/DPI";
const GDK_WINDOW_SCALE: &str = "Gdk/WindowScalingFactor";
const GDK_UNSCALED_DPI: &str = "Gdk/UnscaledDPI";
const GTK_CURSOR_THEME_SIZE: &str = "Gtk/CursorThemeSize";
const GTK_CURSOR_THEME_NAME: &str = "Gtk/CursorThemeName";

pub(super) struct Settings {
    window: x::Window,
    serial: u32,
    int_settings: HashMap<&'static str, IntSetting>,
    string_settings: HashMap<&'static str, StringSetting>,
}

#[derive(Copy, Clone)]
struct IntSetting {
    value: i32,
    last_change_serial: u32,
}

#[derive(Clone)]
struct StringSetting {
    value: String,
    last_change_serial: u32,
}

mod setting_type {
    pub const INTEGER: u8 = 0;
    pub const STRING: u8 = 1;
}

impl Settings {
    pub(super) fn new(connection: &xcb::Connection, atoms: &super::Atoms, root: x::Window) -> Self {
        // Is this a good place for reading this env var?
        let cursor_theme =
            std::env::var("XCURSOR_THEME").unwrap_or_else(|_| DEFAULT_CURSOR_THEME.to_string());
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
            int_settings: HashMap::from([
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
                (
                    GTK_CURSOR_THEME_SIZE,
                    IntSetting {
                        value: DEFAULT_CURSOR_SIZE,
                        last_change_serial: 0,
                    },
                ),
            ]),
            string_settings: HashMap::from([(
                GTK_CURSOR_THEME_NAME,
                StringSetting {
                    value: cursor_theme,
                    last_change_serial: 0,
                },
            )]),
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
        let num_settings = (self.int_settings.len() + self.string_settings.len()) as u32;
        data.extend_from_slice(&num_settings.to_le_bytes());

        fn insert_with_padding(data: &[u8], out: &mut Vec<u8>) {
            out.extend_from_slice(data);
            // See https://x.org/releases/X11R7.7/doc/xproto/x11protocol.html#Syntactic_Conventions_b
            let num_padding_bytes = (4 - (data.len() % 4)) % 4;
            out.extend(std::iter::repeat_n(0, num_padding_bytes));
        }

        fn insert_setting_header(
            name: &str,
            setting_type: u8,
            last_change_serial: u32,
            out: &mut Vec<u8>,
        ) {
            out.extend_from_slice(&[setting_type, 0]);
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            insert_with_padding(name.as_bytes(), out);
            out.extend_from_slice(&last_change_serial.to_le_bytes());
        }

        fn insert_integer_setting(name: &str, setting: &IntSetting, out: &mut Vec<u8>) {
            insert_setting_header(name, setting_type::INTEGER, setting.last_change_serial, out);
            out.extend_from_slice(&setting.value.to_le_bytes());
        }

        fn insert_string_setting(name: &str, setting: &StringSetting, out: &mut Vec<u8>) {
            insert_setting_header(name, setting_type::STRING, setting.last_change_serial, out);
            out.extend_from_slice(&(setting.value.len() as u32).to_le_bytes());
            insert_with_padding(setting.value.as_bytes(), out);
        }

        for (name, setting) in &self.int_settings {
            insert_integer_setting(name, setting, &mut data);
        }

        for (name, setting) in &self.string_settings {
            insert_string_setting(name, setting, &mut data);
        }

        data
    }

    fn set_scale(&mut self, scale: f64) {
        self.serial += 1;

        let scale = scale.max(1.0);
        let scaled_dpi = (scale * DEFAULT_DPI as f64 * DPI_SCALE_FACTOR as f64).round() as i32;
        let setting = IntSetting {
            value: scaled_dpi,
            last_change_serial: self.serial,
        };

        self.int_settings.entry(XFT_DPI).insert_entry(setting);
        // Gdk/WindowScalingFactor + Gdk/UnscaledDPI is identical to setting
        // GDK_SCALE = scale and then GDK_DPI_SCALE = 1 / scale.
        self.int_settings
            .entry(GDK_UNSCALED_DPI)
            .insert_entry(IntSetting {
                value: scaled_dpi / scale as i32,
                last_change_serial: self.serial,
            });
        self.int_settings
            .entry(GDK_WINDOW_SCALE)
            .insert_entry(IntSetting {
                value: scale as i32,
                last_change_serial: self.serial,
            });
        self.int_settings
            .entry(GTK_CURSOR_THEME_SIZE)
            .insert_entry(IntSetting {
                value: (DEFAULT_CURSOR_SIZE as f64 * scale) as i32,
                last_change_serial: self.serial,
            });
    }
}
