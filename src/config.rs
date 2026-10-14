//! On-disk state, plain JSON under `~/.cache/leporello/`:
//!
//! - `store.json`: every record of the Reminders zone plus the sync token, so
//!   the window fills instantly, works offline, and syncs incrementally
//! - `state.json`: urgent flags by asset checksum, and which alerts already fired
//!
//! The iCloud session is a credential and goes to the keyring (`secrets.rs`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use gtk::glib;
use remindkit::Store;
use serde::{Deserialize, Serialize};

pub const APP_ID: &str = "io.github.mozartdiniz.Leporello";

fn cache_dir() -> PathBuf {
    glib::user_cache_dir().join("leporello")
}

pub fn load_store() -> Option<Store> {
    std::fs::read(cache_dir().join("store.json")).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

pub fn save_store(store: &Store) {
    write_json(&cache_dir().join("store.json"), store);
}

pub fn delete_store() {
    let _ = std::fs::remove_file(cache_dir().join("store.json"));
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// `UrgentPresentationAlarmsChecksum` → urgent toggle.
    #[serde(default)]
    pub urgent: HashMap<String, bool>,
    /// `<record name>@<unix seconds>` for alerts already shown.
    #[serde(default)]
    pub fired: HashSet<String>,
    /// Alerts up to this moment (unix seconds) have been handled.
    #[serde(default)]
    pub checked_until: i64,
}

impl State {
    fn path() -> PathBuf {
        cache_dir().join("state.json")
    }

    pub fn load() -> Self {
        std::fs::read(Self::path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        write_json(&Self::path(), self);
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) {
    let Ok(bytes) = serde_json::to_vec(value) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Write-then-rename so a crash never leaves a truncated file.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path)).is_err() {
        log::warn!("couldn't write {}", path.display());
    }
}
