//! Capturing the text selected in the foreground app, and naming the window it came from.
//! Everything here is platform-specific: Windows and macOS each simulate the copy shortcut
//! and read the key state their own way, other platforms capture nothing.

use std::time::Instant;

use rand::{distributions::Alphanumeric, Rng};
use tauri::{AppHandle, ClipboardManager};
use tokio::time::{sleep, Duration};

const SELECTION_COPY_TIMEOUT_MS: u64 = 500;
const SELECTION_COPY_POLL_MS: u64 = 25;
const HOTKEY_MODIFIER_RELEASE_TIMEOUT_MS: u64 = 350;
const HOTKEY_MODIFIER_RELEASE_POLL_MS: u64 = 10;

pub(crate) async fn capture_selected_text_from_active_app(app: &AppHandle) -> anyhow::Result<Option<String>> {
    #[cfg(target_os = "macos")]
    if !selection_access_allowed(true) {
        anyhow::bail!("VoiceReader needs macOS Accessibility access to read highlighted text. Open System Settings > Privacy & Security > Accessibility (Device Control and Data Access on newer macOS), enable this copy of VoiceReader, then quit and reopen it. If VoiceReader is already enabled, remove the old entry and add the current app again.");
    }
    let previous_clipboard = app.clipboard_manager().read_text().ok().flatten();
    let probe_clipboard_value = build_selection_probe_value();
    let probe_set = app
        .clipboard_manager()
        .write_text(probe_clipboard_value.clone())
        .is_ok();

    // Hotkey callback can run while Ctrl/Shift is still physically down.
    // Wait briefly so simulated copy does not become Ctrl+Shift+C in target apps.
    wait_for_hotkey_modifiers_release().await;

    if !trigger_system_copy_shortcut() {
        restore_clipboard_text(app, previous_clipboard, probe_set);
        anyhow::bail!("Could not send the Copy shortcut to the selected application");
    }

    let previous_trimmed = previous_clipboard.as_deref().map(str::trim);
    let started = Instant::now();
    let mut captured: Option<String> = None;

    while started.elapsed() < Duration::from_millis(SELECTION_COPY_TIMEOUT_MS) {
        let current_clipboard = app.clipboard_manager().read_text().ok().flatten();
        let normalized_current = normalized_clipboard_text(current_clipboard);
        if let Some(current_text) = normalized_current {
            let changed = if probe_set {
                current_text != probe_clipboard_value
            } else {
                previous_trimmed.map_or(true, |prev| current_text != prev)
            };
            if changed {
                captured = Some(current_text);
                break;
            }
        }
        sleep(Duration::from_millis(SELECTION_COPY_POLL_MS)).await;
    }

    restore_clipboard_text(app, previous_clipboard, probe_set);

    Ok(captured)
}

/// macOS permissions belong to the running executable, not just its display name.
/// A rebuilt, unsigned app can have an enabled but stale entry in System Settings.
pub(crate) fn selection_access_allowed(prompt: bool) -> bool {
    #[cfg(target_os = "macos")]
    {
        use core_foundation::{base::TCFType, boolean::CFBoolean, dictionary::{CFDictionary, CFDictionaryRef}, string::{CFString, CFStringRef}};
        #[link(name = "ApplicationServices", kind = "framework")]
        extern "C" {
            static kAXTrustedCheckOptionPrompt: CFStringRef;
            fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
        }
        // The key is a process-lifetime framework constant; the dictionary retains
        // both it and the boolean until the trust check completes.
        unsafe {
            let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
            let options = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::from(prompt))]);
            AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef())
        }
    }
    #[cfg(not(target_os = "macos"))]
    { let _ = prompt; true }
}

#[cfg(target_os = "macos")]
pub(crate) fn own_app_is_frontmost() -> bool {
    use objc::{class, msg_send, sel, sel_impl, runtime::Object};
    unsafe {
        let workspace: *mut Object = msg_send![class!(NSWorkspace), sharedWorkspace];
        let active: *mut Object = msg_send![workspace, frontmostApplication];
        if active.is_null() { return false; }
        let pid: i32 = msg_send![active, processIdentifier];
        pid as u32 == std::process::id()
    }
}

fn normalized_clipboard_text(raw: Option<String>) -> Option<String> {
    raw.map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn restore_clipboard_text(app: &AppHandle, previous_clipboard: Option<String>, probe_set: bool) {
    if let Some(previous_text) = previous_clipboard {
        let _ = app.clipboard_manager().write_text(previous_text);
    } else if probe_set {
        // We temporarily set a probe value; clear it back to empty when clipboard had no text before.
        let _ = app.clipboard_manager().write_text(String::new());
    }
}

fn build_selection_probe_value() -> String {
    let suffix: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(12)
        .map(char::from)
        .collect();
    format!("__voicereader_selection_probe_{suffix}__")
}

async fn wait_for_hotkey_modifiers_release() {
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(HOTKEY_MODIFIER_RELEASE_TIMEOUT_MS) {
        if !hotkey_modifiers_pressed() {
            return;
        }
        sleep(Duration::from_millis(HOTKEY_MODIFIER_RELEASE_POLL_MS)).await;
    }
}

fn hotkey_modifiers_pressed() -> bool {
    #[cfg(target_os = "windows")]
    {
        return hotkey_modifiers_pressed_windows();
    }

    #[cfg(target_os = "macos")]
    {
        return hotkey_modifiers_pressed_macos();
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        false
    }
}

#[cfg(target_os = "windows")]
fn hotkey_modifiers_pressed_windows() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
    };

    fn is_pressed(vk: i32) -> bool {
        unsafe { (GetAsyncKeyState(vk) as u16 & 0x8000) != 0 }
    }

    is_pressed(VK_CONTROL as i32)
        || is_pressed(VK_SHIFT as i32)
        || is_pressed(VK_MENU as i32)
        || is_pressed(VK_LWIN as i32)
        || is_pressed(VK_RWIN as i32)
}

fn trigger_system_copy_shortcut() -> bool {
    #[cfg(target_os = "windows")]
    {
        return trigger_copy_shortcut_windows();
    }

    #[cfg(target_os = "macos")]
    {
        return trigger_copy_shortcut_macos();
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        false
    }
}

#[cfg(target_os = "windows")]
fn trigger_copy_shortcut_windows() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_CONTROL,
    };

    const KEY_C: u16 = 0x43;

    fn keyboard_input(vk: u16, flags: u32) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    let inputs = [
        keyboard_input(VK_CONTROL as u16, 0),
        keyboard_input(KEY_C, 0),
        keyboard_input(KEY_C, KEYEVENTF_KEYUP),
        keyboard_input(VK_CONTROL as u16, KEYEVENTF_KEYUP),
    ];

    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };

    sent == inputs.len() as u32
}

#[cfg(target_os = "macos")]
fn hotkey_modifiers_pressed_macos() -> bool {
    use core_graphics::event_source::CGEventSourceStateID;
    use core_graphics::event::CGEventFlags;

    // core-graphics 0.24 does not wrap this Quartz function.
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSourceFlagsState(state_id: CGEventSourceStateID) -> u64;
    }
    let source_state = CGEventSourceStateID::CombinedSessionState;
    // Safety: this queries the system's modifier state and takes no pointers.
    let flags = CGEventFlags::from_bits_truncate(unsafe { CGEventSourceFlagsState(source_state) });

    flags.contains(CGEventFlags::CGEventFlagCommand)
        || flags.contains(CGEventFlags::CGEventFlagShift)
        || flags.contains(CGEventFlags::CGEventFlagControl)
        || flags.contains(CGEventFlags::CGEventFlagAlternate)
}

#[cfg(target_os = "macos")]
fn trigger_copy_shortcut_macos() -> bool {
    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, CGKeyCode};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    // Virtual key code for 'C' on macOS.
    const VK_C: CGKeyCode = 0x08;

    let source = match CGEventSource::new(CGEventSourceStateID::HIDSystemState) {
        Ok(src) => src,
        Err(_) => return false,
    };

    let key_down = match CGEvent::new_keyboard_event(source.clone(), VK_C, true) {
        Ok(evt) => evt,
        Err(_) => return false,
    };
    let key_up = match CGEvent::new_keyboard_event(source, VK_C, false) {
        Ok(evt) => evt,
        Err(_) => return false,
    };

    // Hold Cmd while pressing C (simulates Cmd+C).
    key_down.set_flags(CGEventFlags::CGEventFlagCommand);
    key_up.set_flags(CGEventFlags::CGEventFlagCommand);

    key_down.post(CGEventTapLocation::HID);
    key_up.post(CGEventTapLocation::HID);

    true
}

#[cfg(target_os = "macos")]
fn get_frontmost_app_name_macos() -> Option<String> {
    use objc::{class, msg_send, sel, sel_impl};
    use objc::runtime::Object;
    use core_foundation::string::CFString;
    use core_foundation::base::TCFType;

    unsafe {
        let workspace: *mut Object = msg_send![class!(NSWorkspace), sharedWorkspace];
        if workspace.is_null() {
            return None;
        }
        let app: *mut Object = msg_send![workspace, frontmostApplication];
        if app.is_null() {
            return None;
        }
        let name: *mut Object = msg_send![app, localizedName];
        if name.is_null() {
            return None;
        }

        // NSString -> CFString -> Rust String
        let cf_str = CFString::wrap_under_get_rule(name as *const _);
        let result = cf_str.to_string();
        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn get_foreground_window_title() -> Option<String> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
    };

    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return None;
    }

    let title_len = unsafe { GetWindowTextLengthW(hwnd) };
    if title_len <= 0 {
        return None;
    }

    let mut title_buf = vec![0u16; title_len as usize + 1];
    let written = unsafe { GetWindowTextW(hwnd, title_buf.as_mut_ptr(), title_buf.len() as i32) };
    if written <= 0 {
        return None;
    }

    let title = String::from_utf16_lossy(&title_buf[..written as usize])
        .trim()
        .to_string();
    if title.is_empty() {
        return None;
    }
    Some(title)
}

#[cfg(target_os = "macos")]
pub(crate) fn get_foreground_window_title() -> Option<String> {
    get_frontmost_app_name_macos()
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub(crate) fn get_foreground_window_title() -> Option<String> {
    None
}
