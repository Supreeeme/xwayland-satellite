use log::debug;
use std::env;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedValue, Value};

const PORTAL_DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const PORTAL_SETTINGS_INTERFACE: &str = "org.freedesktop.portal.Settings";
const APPEARANCE_NAMESPACE: &str = "org.freedesktop.appearance";
const COLOR_SCHEME_KEY: &str = "color-scheme";

static CURRENT_COLOR_SCHEME: AtomicU8 = AtomicU8::new(ColorScheme::Light as u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColorScheme {
    Light = 0,
    Dark = 1,
}

impl ColorScheme {
    fn from_env() -> Self {
        match env::var("COLOR_SCHEME") {
            Ok(value) => Self::from_env_value(Some(&value)),
            Err(_) => Self::from_env_value(None),
        }
    }

    fn from_env_value(value: Option<&str>) -> Self {
        match value {
            Some(value) if contains_dark_case_insensitive(value) => Self::Dark,
            _ => Self::Light,
        }
    }

    fn from_portal_value(value: &OwnedValue) -> Result<Self, String> {
        let value = Value::try_from(value)
            .and_then(Value::downcast::<u32>)
            .map_err(|err| format!("invalid color-scheme portal value {value:?}: {err}"))?;
        Ok(match value {
            1 => Self::Dark,
            _ => Self::Light,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

pub(crate) fn current_color_scheme() -> ColorScheme {
    match CURRENT_COLOR_SCHEME.load(Ordering::Relaxed) {
        value if value == ColorScheme::Dark as u8 => ColorScheme::Dark,
        _ => ColorScheme::Light,
    }
}

fn set_current_color_scheme(color_scheme: ColorScheme) -> bool {
    CURRENT_COLOR_SCHEME.swap(color_scheme as u8, Ordering::Relaxed) != color_scheme as u8
}

pub(crate) struct ColorSchemeMonitor {
    wake_rx: UnixStream,
    changes_rx: mpsc::Receiver<ColorScheme>,
}

impl ColorSchemeMonitor {
    pub(crate) fn as_raw_fd(&self) -> RawFd {
        self.wake_rx.as_raw_fd()
    }

    pub(crate) fn drain_changes(&mut self) -> bool {
        self.drain_wake_rx();

        let mut changed = false;
        while self.changes_rx.try_recv().is_ok() {
            changed = true;
        }
        changed
    }

    fn drain_wake_rx(&mut self) {
        let mut buf = [0; 64];
        loop {
            match self.wake_rx.read(&mut buf) {
                Ok(0) => return,
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return,
                Err(err) => {
                    debug!("Failed to drain color-scheme wake fd: {err}");
                    return;
                }
            }
        }
    }
}

pub(crate) fn initialize() -> Option<ColorSchemeMonitor> {
    match portal_color_scheme() {
        Ok((connection, color_scheme)) => {
            set_current_color_scheme(color_scheme);
            debug!("Using D-Bus portal color-scheme: {}", color_scheme.as_str());
            match ColorSchemeMonitor::new(connection) {
                Ok(monitor) => Some(monitor),
                Err(err) => {
                    debug!("D-Bus portal color-scheme listener unavailable: {err}");
                    None
                }
            }
        }
        Err(err) => {
            let color_scheme = ColorScheme::from_env();
            set_current_color_scheme(color_scheme);
            debug!(
                "D-Bus portal color-scheme unavailable ({err}); COLOR_SCHEME={:?}, using {}",
                env::var("COLOR_SCHEME").ok(),
                color_scheme.as_str()
            );
            None
        }
    }
}

impl ColorSchemeMonitor {
    fn new(connection: Connection) -> Result<Self, String> {
        let proxy = settings_proxy(&connection)?;
        let mut signals = proxy
            .receive_signal_with_args(
                "SettingChanged",
                &[(0, APPEARANCE_NAMESPACE), (1, COLOR_SCHEME_KEY)],
            )
            .map_err(|err| format!("failed to listen for SettingChanged: {err}"))?;

        let (wake_rx, mut wake_tx) =
            UnixStream::pair().map_err(|err| format!("failed to create wake fd: {err}"))?;
        wake_rx
            .set_nonblocking(true)
            .map_err(|err| format!("failed to set wake fd nonblocking: {err}"))?;
        wake_tx
            .set_nonblocking(true)
            .map_err(|err| format!("failed to set wake writer nonblocking: {err}"))?;

        let (changes_tx, changes_rx) = mpsc::channel();
        std::thread::spawn(move || {
            for message in &mut signals {
                match parse_setting_changed(message.body().deserialize()) {
                    Ok(Some(color_scheme)) => {
                        debug!(
                            "D-Bus portal color-scheme update received: {}",
                            color_scheme.as_str()
                        );
                        if set_current_color_scheme(color_scheme) {
                            if changes_tx.send(color_scheme).is_err() {
                                return;
                            }
                            let _ = wake_tx.write_all(&[1]);
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        debug!("Failed to parse D-Bus portal color-scheme update: {err}");
                    }
                }
            }
            debug!("D-Bus portal color-scheme listener stopped");
        });

        Ok(Self {
            wake_rx,
            changes_rx,
        })
    }
}

fn portal_color_scheme() -> Result<(Connection, ColorScheme), String> {
    let connection =
        Connection::session().map_err(|err| format!("failed to connect to session bus: {err}"))?;
    let proxy = settings_proxy(&connection)?;
    let value: OwnedValue = proxy
        .call("Read", &(APPEARANCE_NAMESPACE, COLOR_SCHEME_KEY))
        .map_err(|err| format!("failed to read portal setting: {err}"))?;
    let color_scheme = ColorScheme::from_portal_value(&value)?;
    Ok((connection, color_scheme))
}

fn settings_proxy(connection: &Connection) -> Result<Proxy<'static>, String> {
    Proxy::new(
        connection,
        PORTAL_DESTINATION,
        PORTAL_PATH,
        PORTAL_SETTINGS_INTERFACE,
    )
    .map_err(|err| format!("failed to create portal settings proxy: {err}"))
}

fn parse_setting_changed(
    body: zbus::Result<(String, String, OwnedValue)>,
) -> Result<Option<ColorScheme>, String> {
    let (namespace, key, value) =
        body.map_err(|err| format!("invalid SettingChanged signal body: {err}"))?;
    if namespace != APPEARANCE_NAMESPACE || key != COLOR_SCHEME_KEY {
        return Ok(None);
    }
    ColorScheme::from_portal_value(&value).map(Some)
}

fn contains_dark_case_insensitive(value: &str) -> bool {
    value
        .as_bytes()
        .windows(4)
        .any(|window| window.eq_ignore_ascii_case(b"dark"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_value_uses_dark_only_when_value_contains_dark() {
        assert_eq!(
            ColorScheme::from_env_value(Some("prefer-dark")),
            ColorScheme::Dark
        );
        assert_eq!(ColorScheme::from_env_value(Some("DARK")), ColorScheme::Dark);
        assert_eq!(
            ColorScheme::from_env_value(Some("light")),
            ColorScheme::Light
        );
        assert_eq!(ColorScheme::from_env_value(None), ColorScheme::Light);
    }

    #[test]
    fn portal_value_treats_no_preference_and_default_as_light() {
        assert_eq!(
            ColorScheme::from_portal_value(&OwnedValue::from(0_u32)).unwrap(),
            ColorScheme::Light
        );
        assert_eq!(
            ColorScheme::from_portal_value(&OwnedValue::from(1_u32)).unwrap(),
            ColorScheme::Dark
        );
        assert_eq!(
            ColorScheme::from_portal_value(&OwnedValue::from(2_u32)).unwrap(),
            ColorScheme::Light
        );
        assert_eq!(
            ColorScheme::from_portal_value(&OwnedValue::from(99_u32)).unwrap(),
            ColorScheme::Light
        );
    }

    #[test]
    fn portal_value_accepts_nested_variant_u32() {
        let no_preference = OwnedValue::try_from(Value::new(Value::U32(0))).unwrap();
        let prefer_dark = OwnedValue::try_from(Value::new(Value::U32(1))).unwrap();

        assert_eq!(
            ColorScheme::from_portal_value(&no_preference).unwrap(),
            ColorScheme::Light
        );
        assert_eq!(
            ColorScheme::from_portal_value(&prefer_dark).unwrap(),
            ColorScheme::Dark
        );
    }

    #[test]
    fn setting_changed_parser_filters_unrelated_settings() {
        let value = OwnedValue::from(1_u32);
        assert_eq!(
            parse_setting_changed(Ok((
                APPEARANCE_NAMESPACE.into(),
                COLOR_SCHEME_KEY.into(),
                value,
            )))
            .unwrap(),
            Some(ColorScheme::Dark)
        );

        assert_eq!(
            parse_setting_changed(Ok((
                "org.freedesktop.other".into(),
                COLOR_SCHEME_KEY.into(),
                OwnedValue::from(1_u32),
            )))
            .unwrap(),
            None
        );
        assert_eq!(
            parse_setting_changed(Ok((
                APPEARANCE_NAMESPACE.into(),
                "other".into(),
                OwnedValue::from(1_u32),
            )))
            .unwrap(),
            None
        );
    }
}
