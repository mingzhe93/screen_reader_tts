//! The small settings file kept in the app config directory, and the validation of the
//! values stored in it.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use std::sync::Mutex;

static SETTINGS_WRITE_LOCK: Mutex<()> = Mutex::new(());

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
    /// The Assistant's system prompts; `None` means the built-in default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    assistant_chat_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    assistant_voice_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    assistant_brave_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    assistant_tool_limits: Option<serde_json::Value>,
}

#[cfg(feature = "build-base")]
pub(crate) fn load_assistant_tool_settings(app: &AppHandle) -> (Option<String>, crate::assistant_tools::ToolLimits) {
    let saved = load_settings_file(app);
    let limits = saved.assistant_tool_limits.and_then(|value| serde_json::from_value(value).ok())
        .filter(|limits: &crate::assistant_tools::ToolLimits| limits.validate().is_ok()).unwrap_or_default();
    (saved.assistant_brave_key.filter(|key| !key.is_empty()), limits)
}

#[cfg(feature = "build-base")]
pub(crate) fn persist_assistant_search_key(app: &AppHandle, key: Option<String>) -> Result<()> {
    update_settings_file(app, |settings| settings.assistant_brave_key = key)
}

#[cfg(feature = "build-base")]
pub(crate) fn persist_assistant_tool_limits(app: &AppHandle, limits: crate::assistant_tools::ToolLimits) -> Result<()> {
    let value = serde_json::to_value(limits)?;
    update_settings_file(app, |settings| settings.assistant_tool_limits = Some(value))
}

/// The Assistant system prompts saved by the user, `(chat, voice)`; `None` = the default.
#[cfg(feature = "build-base")]
pub(crate) fn load_assistant_prompts(app: &AppHandle) -> (Option<String>, Option<String>) {
    let settings = load_settings_file(app);
    (settings.assistant_chat_prompt, settings.assistant_voice_prompt)
}

#[cfg(feature = "build-base")]
pub(crate) fn persist_assistant_prompts(app: &AppHandle, chat: Option<String>, voice: Option<String>) -> Result<()> {
    update_settings_file(app, |settings| {
        settings.assistant_chat_prompt = chat;
        settings.assistant_voice_prompt = voice;
    })
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
    let _lock = SETTINGS_WRITE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    read_settings_file(app)
}

fn read_settings_file(app: &AppHandle) -> AppSettingsFile {
    app_settings_path(app)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|body| serde_json::from_str(&body).ok())
        .unwrap_or_default()
}

/// Changes one or more settings while keeping the rest of the file.
fn update_settings_file(app: &AppHandle, change: impl FnOnce(&mut AppSettingsFile)) -> Result<()> {
    let _lock = SETTINGS_WRITE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let path = app_settings_path(app).ok_or_else(|| anyhow!("Unable to resolve app settings path"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("Failed to create app settings directory {}", parent.display())
        })?;
    }

    let mut settings = read_settings_file(app);
    change(&mut settings);
    let serialized = serde_json::to_string_pretty(&settings)?;
    write_private_file_atomically(&path, serialized.as_bytes())
        .with_context(|| format!("Failed to write app settings file {}", path.display()))?;
    Ok(())
}

/// Writes through a temporary file in the same folder, then renames it over `path`, so a
/// crash mid-write never leaves a truncated file (which would read back as all defaults and
/// lose every setting, including the Brave key, on the next save). On Unix the file is
/// readable by its owner only, since it may hold the search key in plain text.
fn write_private_file_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("settings.json");
    let temp = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        // Replaces an existing file on both platforms (MoveFileEx with REPLACE_EXISTING on Windows).
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_replaced_atomically_and_kept_private() {
        let dir = std::env::temp_dir().join(format!("vr-settings-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, "old").unwrap();
        write_private_file_atomically(&path, b"{\"a\":1}").unwrap();
        write_private_file_atomically(&path, b"{\"a\":2}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":2}");
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("settings.json")], "no temporary file is left");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
