//! The tray's own settings, what a person chose about how it shows things: `tray.json` in the user's
//! Cairn directory, beside the registry. Only the tray reads it; no daemon does. Read once at start and
//! kept, so the menu, the window and `--probe` format each time alike.

use cairn_protocol::local::user_dir;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

/// How a time of day is shown: `16:52`, or `4:52 PM`. 24 hours unless a person chose otherwise,
/// whatever the system's locale says, since the webview would follow the locale and not the system's
/// own clock setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Clock {
    #[default]
    #[serde(rename = "24h")]
    H24,
    #[serde(rename = "12h")]
    H12,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub clock: Clock,
    /// What a newer tray wrote and this one does not know, written back as it was.
    #[serde(flatten)]
    pub rest: serde_json::Map<String, serde_json::Value>,
}

/// The settings in use, and why the file could not be read if it could not.
static CURRENT: LazyLock<Mutex<(Settings, Option<String>)>> =
    LazyLock::new(|| Mutex::new((Settings::default(), None)));

fn current() -> std::sync::MutexGuard<'static, (Settings, Option<String>)> {
    CURRENT.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn path() -> Option<PathBuf> {
    user_dir().map(|d| d.join("tray.json"))
}

/// Reads `tray.json`. Missing, it is the defaults; unreadable, the defaults too, and the reason is
/// returned and kept for the window to show, so nobody is left wondering why a choice did not hold.
pub fn load() -> Option<String> {
    let (settings, error) = match path() {
        None => (Settings::default(), None),
        Some(p) => match std::fs::read(&p) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Settings::default(), None),
            Err(e) => (
                Settings::default(),
                Some(format!("cannot read {}: {e}", p.display())),
            ),
            Ok(bytes) => match parse(&bytes) {
                Ok(s) => (s, None),
                Err(e) => (
                    Settings::default(),
                    Some(format!("cannot read {}: {e}", p.display())),
                ),
            },
        },
    };
    *current() = (settings, error.clone());
    error
}

fn parse(bytes: &[u8]) -> Result<Settings, serde_json::Error> {
    serde_json::from_slice(bytes)
}

pub fn clock() -> Clock {
    current().0.clock
}

/// Why `tray.json` could not be read at start, until a choice wrote it anew.
pub fn error() -> Option<String> {
    current().1.clone()
}

/// Writes the choice, and only once it is on disk uses it, so what is shown is what the next start
/// reads.
pub fn set_clock(clock: Clock) -> Result<(), String> {
    let mut cur = current();
    let mut next = cur.0.clone();
    next.clock = clock;
    let p = path().ok_or("no home directory to keep the settings in")?;
    let dir = p.parent().expect("a file in a folder");
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
    let bytes = serde_json::to_vec_pretty(&next).map_err(|e| e.to_string())?;
    crate::registry::write_new(&p, &bytes)
        .map_err(|e| format!("cannot write {}: {e}", p.display()))?;
    *cur = (next, None);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_are_the_defaults_and_unknown_ones_are_kept() {
        assert_eq!(parse(b"{}").unwrap(), Settings::default());
        assert_eq!(Settings::default().clock, Clock::H24);
        let s = parse(br#"{"clock":"12h","later":{"a":1}}"#).unwrap();
        assert_eq!(s.clock, Clock::H12);
        let back = serde_json::to_value(&s).unwrap();
        assert_eq!(back, serde_json::json!({"clock": "12h", "later": {"a": 1}}));
        assert!(parse(br#"{"clock":"13h"}"#).is_err());
    }
}
