//! The desktop's light or dark preference on Linux and the BSDs.
//!
//! winit reports no theme there (X11 never does, and many Wayland compositors don't), so egui's
//! "System" choice fell back to dark whatever the desktop said. The freedesktop settings portal
//! (`org.freedesktop.appearance` `color-scheme`) knows, and signals when it changes; GNOME's
//! `gsettings` is asked once where there is no portal.

const PORTAL_NAMESPACE: &str = "org.freedesktop.appearance";
const PORTAL_KEY: &str = "color-scheme";

/// The portal's answer: 1 prefers dark, 2 prefers light, anything else has no preference.
fn decode(code: u32) -> Option<egui::Theme> {
    match code {
        1 => Some(egui::Theme::Dark),
        2 => Some(egui::Theme::Light),
        _ => None,
    }
}

fn code_of(theme: Option<egui::Theme>) -> u8 {
    match theme {
        Some(egui::Theme::Dark) => 1,
        Some(egui::Theme::Light) => 2,
        None => 0,
    }
}

/// `gsettings get org.gnome.desktop.interface color-scheme` output.
fn parse_gtk_scheme(s: &str) -> Option<egui::Theme> {
    match s.trim().trim_matches('\'') {
        "prefer-dark" => Some(egui::Theme::Dark),
        "prefer-light" => Some(egui::Theme::Light),
        _ => None,
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
mod portal {
    use super::{PORTAL_KEY, PORTAL_NAMESPACE};

    pub fn proxy() -> Option<zbus::blocking::Proxy<'static>> {
        let conn = zbus::blocking::connection::Builder::session().ok()?.method_timeout(std::time::Duration::from_secs(2)).build().ok()?;
        zbus::blocking::Proxy::new(&conn, "org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop", "org.freedesktop.portal.Settings").ok()
    }

    pub fn read(proxy: &zbus::blocking::Proxy<'_>) -> Option<egui::Theme> {
        let value: zbus::zvariant::OwnedValue = proxy.call("Read", &(PORTAL_NAMESPACE, PORTAL_KEY)).ok()?;
        super::decode(code(value)?)
    }

    /// `Settings.Read` answers with a variant, and some portals nest another one inside it.
    pub fn code(value: zbus::zvariant::OwnedValue) -> Option<u32> {
        let mut value: zbus::zvariant::Value<'_> = value.into();
        for _ in 0..4 {
            match value {
                zbus::zvariant::Value::Value(inner) => value = *inner,
                other => return u32::try_from(other).ok(),
            }
        }
        None
    }

    /// Once at start-up, by absolute path, for desktops without the portal.
    pub fn gtk() -> Option<egui::Theme> {
        let exe = ["/usr/bin/gsettings", "/bin/gsettings", "/usr/local/bin/gsettings"].into_iter().find(|p| std::path::Path::new(p).is_file())?;
        let output = std::process::Command::new(exe).args(["get", "org.gnome.desktop.interface", "color-scheme"]).output().ok()?;
        if !output.status.success() {
            return None;
        }
        super::parse_gtk_scheme(std::str::from_utf8(&output.stdout).ok()?)
    }
}

/// The desktop's current preference, kept up to date by a background thread.
pub struct SystemTheme {
    value: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

impl SystemTheme {
    /// Reads the preference, then follows the portal's `SettingChanged` signal, calling `wake`
    /// when it changes. No polling: the thread sleeps in the signal iterator. `None` where the
    /// platform reports its theme to egui itself.
    pub fn start(wake: impl Fn() + Send + 'static) -> Option<Self> {
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
        {
            use std::sync::Arc;
            use std::sync::atomic::{AtomicU8, Ordering};

            let value = Arc::new(AtomicU8::new(0));
            let worker = Arc::clone(&value);
            let (ready, wait) = std::sync::mpsc::channel();
            let spawned = std::thread::Builder::new().name("appearance-portal".into()).spawn(move || {
                let proxy = portal::proxy();
                let initial = proxy.as_ref().and_then(portal::read).or_else(portal::gtk);
                worker.store(code_of(initial), Ordering::Relaxed);
                let _ = ready.send(());
                let Some(proxy) = proxy else { return };
                let Ok(signals) = proxy.receive_signal("SettingChanged") else { return };
                for message in signals {
                    let Ok((namespace, key, changed)) = message.body().deserialize::<(String, String, zbus::zvariant::OwnedValue)>() else {
                        continue;
                    };
                    if namespace != PORTAL_NAMESPACE || key != PORTAL_KEY {
                        continue;
                    }
                    let code = code_of(portal::code(changed).and_then(decode));
                    if worker.swap(code, Ordering::Relaxed) != code {
                        wake();
                    }
                }
            });
            if let Err(e) = spawned {
                log::warn!("can't follow the desktop's light or dark setting: {e}");
                return None;
            }
            // The first frame should already have the right colours; a slow bus doesn't hold it.
            let _ = wait.recv_timeout(std::time::Duration::from_millis(250));
            Some(Self { value })
        }
        #[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly")))]
        {
            let _ = wake;
            None
        }
    }

    pub fn get(&self) -> Option<egui::Theme> {
        decode(u32::from(self.value.load(std::sync::atomic::Ordering::Relaxed)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_portal_and_gsettings_values() {
        assert_eq!(decode(0), None);
        assert_eq!(decode(1), Some(egui::Theme::Dark));
        assert_eq!(decode(2), Some(egui::Theme::Light));
        assert_eq!(decode(9), None);
        for theme in [None, Some(egui::Theme::Dark), Some(egui::Theme::Light)] {
            assert_eq!(decode(u32::from(code_of(theme))), theme);
        }
        assert_eq!(parse_gtk_scheme("'prefer-light'\n"), Some(egui::Theme::Light));
        assert_eq!(parse_gtk_scheme("'prefer-dark'\n"), Some(egui::Theme::Dark));
        assert_eq!(parse_gtk_scheme("'default'\n"), None);
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
    #[test]
    fn unwraps_nested_portal_variants() {
        use zbus::zvariant::{OwnedValue, Value};

        let nested = Value::Value(Box::new(Value::Value(Box::new(Value::U32(2)))));
        assert_eq!(portal::code(OwnedValue::try_from(nested).unwrap()), Some(2));
        assert_eq!(portal::code(OwnedValue::from(1u32)), Some(1));
        assert_eq!(portal::code(OwnedValue::try_from(Value::from("dark")).unwrap()), None);
    }
}
