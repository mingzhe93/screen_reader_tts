//! The small settings file kept in the app config directory, and the validation of the
//! values stored in it.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tauri::AppHandle;

const SETTINGS_FILE_NAME: &str = "settings.json";

/// Compute Device setting values. Parsed by `ComputePreference` in the Base build.
pub(crate) const COMPUTE_DEVICE_AUTO: &str = "auto";
pub(crate) const COMPUTE_DEVICE_VALUES: [&str; 3] = [COMPUTE_DEVICE_AUTO, "gpu", "cpu"];

/// The hotkey used when nothing is saved, and the one registered when the saved hotkey is
/// taken by another app.
#[cfg(target_os = "windows")]
pub(crate) const DEFAULT_FALLBACK_HOTKEY: &str = "Alt+S";

// Not Option+S on macOS: Option plus a letter types a character there, and a global
// hotkey would take that character away from every other app.
#[cfg(not(target_os = "windows"))]
pub(crate) const DEFAULT_FALLBACK_HOTKEY: &str = "Ctrl+Shift+S";

#[derive(Default, Serialize, Deserialize)]
struct AppSettingsFile {
    hotkey: Option<String>,
    compute_device: Option<String>,
}

pub(crate) fn normalize_hotkey(value: &str) -> Result<String> {
    let normalized = value.trim().replace(' ', "");
    if normalized.is_empty() {
        return Err(anyhow!("Hotkey cannot be empty"));
    }
    Ok(normalized)
}

pub(crate) fn is_hotkey_os_reserved(hotkey: &str) -> bool {
    let normalized = hotkey.trim().to_lowercase().replace(' ', "");
    matches!(
        normalized.as_str(),
        "alt+space" | "cmd+space" | "command+space" | "meta+space" | "super+space"
    )
}

fn load_settings_file(app: &AppHandle) -> AppSettingsFile {
    app_settings_path(app)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|body| serde_json::from_str(&body).ok())
        .unwrap_or_default()
}

/// Changes one or more settings while keeping the rest of the file.
fn update_settings_file(app: &AppHandle, change: impl FnOnce(&mut AppSettingsFile)) -> Result<()> {
    let path = app_settings_path(app).ok_or_else(|| anyhow!("Unable to resolve app settings path"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("Failed to create app settings directory {}", parent.display())
        })?;
    }

    let mut settings = load_settings_file(app);
    change(&mut settings);
    let serialized = serde_json::to_string_pretty(&settings)?;
    std::fs::write(&path, serialized)
        .with_context(|| format!("Failed to write app settings file {}", path.display()))?;
    Ok(())
}

pub(crate) fn load_saved_hotkey(app: &AppHandle) -> Option<String> {
    let candidate = load_settings_file(app).hotkey?;
    let normalized = normalize_hotkey(&candidate).ok()?;
    if is_hotkey_os_reserved(&normalized) {
        return None;
    }
    Some(normalized)
}

pub(crate) fn persist_hotkey(app: &AppHandle, hotkey: &str) -> Result<()> {
    update_settings_file(app, |settings| settings.hotkey = Some(hotkey.to_string()))
}

pub(crate) fn load_saved_compute_device(app: &AppHandle) -> Option<String> {
    let candidate = load_settings_file(app).compute_device?.trim().to_lowercase();
    COMPUTE_DEVICE_VALUES.contains(&candidate.as_str()).then_some(candidate)
}

#[cfg(feature = "build-base")]
pub(crate) fn persist_compute_device(app: &AppHandle, preference: &str) -> Result<()> {
    update_settings_file(app, |settings| settings.compute_device = Some(preference.to_string()))
}

fn app_settings_path(app: &AppHandle) -> Option<PathBuf> {
    app.path_resolver()
        .app_config_dir()
        .map(|path| path.join(SETTINGS_FILE_NAME))
}
