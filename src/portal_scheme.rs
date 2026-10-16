//! The desktop's color scheme, as the XDG Settings portal publishes it:
//! `org.freedesktop.appearance` `color-scheme`, which cce-desktop-portal
//! serves from config.kdl's `style { color_scheme }` (the system-interface
//! Appearance page). It is what [`ColorScheme::System`] reports to pages, so
//! the browser agrees with Chromium, Firefox and libadwaita apps, which all
//! read the same key.
//!
//! One thread on blocking zbus, so no runtime comes in: it subscribes to
//! `SettingChanged` FIRST and reads the current value second, so a change
//! landing between the two is not lost, then hands every value to the loop as
//! [`Message::SystemScheme`]. Without a portal (no session bus, a frontend
//! that never answers) it sends nothing, and the browser keeps
//! [`FALLBACK_DARK`].
//!
//! [`ColorScheme::System`]: crate::settings::ColorScheme::System

use zbus::zvariant::{OwnedValue, Value};

use crate::Message;

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.portal.Settings";
const NAMESPACE: &str = "org.freedesktop.appearance";
const KEY: &str = "color-scheme";

/// What "Follow System" means before the portal answers, or when there is
/// none: dark, as cce is, and as cce-desktop-portal reads an absent key.
pub const FALLBACK_DARK: bool = true;

/// Whether a `color-scheme` value asks pages for their dark theme: 1 is
/// "prefer dark". 0 ("no preference") reads as light, as Chromium and
/// Firefox read it, since a page with no preference renders its default
/// (light) styles.
pub fn prefers_dark(value: u32) -> bool {
    value == 1
}

/// The `u32` inside a portal value. `Read` (portal v1) wraps it in a
/// second variant; `ReadOne` and `SettingChanged` do not.
fn scheme_of(value: &Value) -> Option<u32> {
    match value {
        Value::U32(v) => Some(*v),
        Value::Value(inner) => scheme_of(inner),
        _ => None,
    }
}

/// Watch the portal's color scheme for the life of the process.
pub fn spawn(sender: calloop::channel::Sender<Message>) {
    let spawned = std::thread::Builder::new().name("portal-scheme".into()).spawn(move || {
        if let Err(e) = watch(&sender) {
            log::info!("portal color-scheme unavailable ({e}); \"Follow System\" stays dark");
        }
    });
    if let Err(e) = spawned {
        log::warn!("portal color-scheme thread: {e}");
    }
}

fn watch(sender: &calloop::channel::Sender<Message>) -> zbus::Result<()> {
    let conn = zbus::blocking::Connection::session()?;
    let proxy = zbus::blocking::Proxy::new(&conn, DEST, PATH, IFACE)?;
    let changes = proxy.receive_signal("SettingChanged")?;

    let current = proxy
        .call::<_, _, OwnedValue>("ReadOne", &(NAMESPACE, KEY))
        .or_else(|_| proxy.call::<_, _, OwnedValue>("Read", &(NAMESPACE, KEY)))?;
    if let Some(v) = scheme_of(&current) {
        log::info!("portal color-scheme {v}");
        if sender.send(Message::SystemScheme(prefers_dark(v))).is_err() {
            return Ok(());
        }
    }

    for msg in changes {
        let Ok((namespace, key, value)) = msg.body().deserialize::<(String, String, OwnedValue)>() else {
            continue;
        };
        if namespace != NAMESPACE || key != KEY {
            continue;
        }
        if let Some(v) = scheme_of(&value) {
            log::info!("portal color-scheme changed to {v}");
            if sender.send(Message::SystemScheme(prefers_dark(v))).is_err() {
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_prefer_dark_is_dark() {
        assert!(prefers_dark(1));
        assert!(!prefers_dark(2), "prefer light");
        assert!(!prefers_dark(0), "no preference renders a page's default, light styles");
    }

    #[test]
    fn read_v1_wraps_the_value_twice() {
        assert_eq!(scheme_of(&Value::U32(2)), Some(2));
        assert_eq!(scheme_of(&Value::Value(Box::new(Value::U32(1)))), Some(1));
        assert_eq!(scheme_of(&Value::from("dark")), None);
    }
}
