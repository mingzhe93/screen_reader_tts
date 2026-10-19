//! Tauri commands for the Assistant chat page: the glue between the backend manager
//! (`assistant_backend`), which owns the running llama-server, and the chat bridge
//! (`assistant_chat`), which streams answers. Chat output reaches the page as events:
//! `assistant:chat-delta`, `assistant:chat-done` and `assistant:chat-error`.
//! See `docs/assistant/README.md`.

use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use base64::prelude::{Engine as _, BASE64_STANDARD};
use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use crate::assistant_backend::{AssistantState, UseGuard};
use crate::assistant_catalogue;
use crate::assistant_chat::{
    self, ChatMessage, ChatRequest, ChatSink, ChatStart, ChatTarget, Sampling, DEFAULT_CHAT_PROMPT, DEFAULT_VOICE_PROMPT,
    MAX_SYSTEM_PROMPT_CHARS,
};
use crate::settings::{load_assistant_prompts, persist_assistant_prompts};

/// Largest image file accepted from a desktop drop, before downscaling in the page.
const MAX_DROPPED_IMAGE_BYTES: u64 = 25 * 1024 * 1024;

/// Emits chat output as Tauri events. Holds the backend's use guard until the request ends,
/// so the idle timer never unloads the model mid-answer.
struct TauriChatSink {
    app: AppHandle,
    guard: Mutex<Option<UseGuard>>,
    metadata: Mutex<serde_json::Value>,
    round: Mutex<u32>,
}

impl TauriChatSink {
    fn release(&self) {
        if let Ok(mut guard) = self.guard.lock() {
            guard.take();
        }
    }
}

impl ChatSink for TauriChatSink {
    fn delta(&self, request_id: &str, content: Option<&str>, reasoning: Option<&str>) {
        let round = *self.round.lock().unwrap_or_else(|e|e.into_inner());
        let _ = self.app.emit_all("assistant:chat-delta", serde_json::json!({"request_id":request_id,"content":content,"reasoning":reasoning,"round":round}));
    }

    fn done(&self, request_id: &str, finish_reason: Option<&str>, stopped: bool) {
        let mut payload = self.metadata.lock().unwrap_or_else(|e|e.into_inner()).clone();
        payload["request_id"] = request_id.into();
        payload["finish_reason"] = serde_json::json!(finish_reason);
        payload["stopped"] = stopped.into();
        let _ = self.app.emit_all("assistant:chat-done", payload);
        self.release();
    }

    fn error(&self, request_id: &str, message: &str) {
        let mut payload = self.metadata.lock().unwrap_or_else(|e|e.into_inner()).clone();
        payload["request_id"] = request_id.into(); payload["message"] = message.into();
        let _ = self.app.emit_all("assistant:chat-error", payload);
        self.release();
    }

    fn event(&self, request_id: &str, event: &str, mut payload: serde_json::Value) {
        if event == "assistant:turn-metadata" {
            *self.metadata.lock().unwrap_or_else(|e|e.into_inner()) = payload;
            return;
        }
        if let Some(round) = payload.get("round").and_then(serde_json::Value::as_u64) {
            *self.round.lock().unwrap_or_else(|e|e.into_inner()) = round as u32;
        }
        payload["request_id"] = request_id.into();
        let _ = self.app.emit_all(event, payload);
    }
}

#[tauri::command]
pub(crate) async fn assistant_chat_start(
    app: AppHandle,
    state: State<'_, AssistantState>,
    request_id: String,
    messages: Vec<ChatMessage>,
    think: bool,
    sampling: Option<Sampling>,
    conversation_id: Option<String>,
    tools: Option<bool>,
    enabled_tools: Option<Vec<String>>,
    thinking_budget: Option<crate::assistant_tools::ThinkingBudget>,
    tool_limits: Option<crate::assistant_tools::ToolLimits>,
) -> Result<ChatStart, String> {
    let system_prompt = load_assistant_prompts(&app).0;
    let mut options = crate::assistant_tools::request_options(&app,&state,conversation_id,tools,enabled_tools,thinking_budget,tool_limits,false)?;
    let (endpoint, guard, generation) = state.admit_turn().map_err(|error|format!("{error:#}"))?;
    options.generation = generation;
    options.model_id = endpoint.model_id.clone();
    let model = assistant_catalogue::model_by_id(&endpoint.model_id)
        .ok_or_else(|| format!("Unknown assistant model \"{}\".", endpoint.model_id))?;
    let sink = Arc::new(TauriChatSink { app, guard: Mutex::new(Some(guard)), metadata: Mutex::new(serde_json::json!({})), round: Mutex::new(0) });
    let target = ChatTarget {
        base_url: endpoint.base_url,
        api_key: endpoint.api_key,
        ctx_size: endpoint.ctx_size,
        image_token_estimate: model.image_token_estimate,
    };
    let request = ChatRequest { request_id, messages, think, system_prompt, sampling };
    assistant_chat::start_chat_with_options(target, request, sink, options).await
}

/// The Assistant's two system prompts with their defaults, for the settings panel.
#[derive(Serialize)]
pub(crate) struct AssistantPrompts {
    chat: String,
    voice: String,
    chat_default: &'static str,
    voice_default: &'static str,
}

fn prompts_payload(app: &AppHandle) -> AssistantPrompts {
    let (chat, voice) = load_assistant_prompts(app);
    AssistantPrompts {
        chat: chat.unwrap_or_else(|| DEFAULT_CHAT_PROMPT.to_string()),
        voice: voice.unwrap_or_else(|| DEFAULT_VOICE_PROMPT.to_string()),
        chat_default: DEFAULT_CHAT_PROMPT,
        voice_default: DEFAULT_VOICE_PROMPT,
    }
}

#[tauri::command]
pub(crate) fn assistant_get_prompts(app: AppHandle) -> AssistantPrompts {
    prompts_payload(&app)
}

/// Saves both prompts. `None`, or text equal to the default, stores "use the default", so a
/// later improvement to a default reaches people who never changed it.
#[tauri::command]
pub(crate) fn assistant_set_prompts(
    app: AppHandle,
    chat: Option<String>,
    voice: Option<String>,
) -> Result<AssistantPrompts, String> {
    let normalise = |value: Option<String>, default: &str| -> Result<Option<String>, String> {
        let Some(text) = value else { return Ok(None) };
        if text.chars().count() > MAX_SYSTEM_PROMPT_CHARS {
            return Err(format!("A system prompt can be at most {MAX_SYSTEM_PROMPT_CHARS} characters."));
        }
        Ok((text.trim() != default.trim()).then_some(text))
    };
    let chat = normalise(chat, DEFAULT_CHAT_PROMPT)?;
    let voice = normalise(voice, DEFAULT_VOICE_PROMPT)?;
    persist_assistant_prompts(&app, chat, voice).map_err(|err| format!("Saving the system prompts failed: {err:#}"))?;
    Ok(prompts_payload(&app))
}

#[tauri::command]
pub(crate) fn assistant_chat_cancel(request_id: String) {
    assistant_chat::cancel_chat(&request_id);
}

/// Reads an image the user dropped onto the Assistant page and returns it as a data URL.
/// Desktop drops reach the app as paths (Tauri's `tauri://file-drop`), not as page files.
#[tauri::command]
pub(crate) fn assistant_read_dropped_image(path: String) -> Result<String, String> {
    let path = PathBuf::from(path.trim());
    let name = path.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default();
    // Only regular files: a device or pipe (for example /dev/zero) reports length 0 and
    // would otherwise be read without end. The read itself is capped as well.
    let metadata = std::fs::metadata(&path).map_err(|err| format!("Could not read {name}: {err}"))?;
    if !metadata.is_file() {
        return Err(format!("{name} is not a file."));
    }
    if metadata.len() > MAX_DROPPED_IMAGE_BYTES {
        return Err(format!("{name} is larger than 25 MB."));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .and_then(|file| file.take(MAX_DROPPED_IMAGE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|err| format!("Could not read {name}: {err}"))?;
    if bytes.len() as u64 > MAX_DROPPED_IMAGE_BYTES {
        return Err(format!("{name} is larger than 25 MB."));
    }
    let mime = image_mime(&bytes).ok_or_else(|| format!("{name} is not a PNG, JPEG or WebP image."))?;
    Ok(format!("data:{mime};base64,{}", BASE64_STANDARD.encode(&bytes)))
}

/// Identifies the accepted image types from their leading bytes, not the file name.
fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']) {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_mime_uses_file_signatures() {
        assert_eq!(image_mime(b"\x89PNG\r\n\x1a\n rest"), Some("image/png"));
        assert_eq!(image_mime(&[0xff, 0xd8, 0xff, 0xe0, 0, 0]), Some("image/jpeg"));
        assert_eq!(image_mime(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some("image/webp"));
        assert_eq!(image_mime(b"GIF89a"), None);
        assert_eq!(image_mime(b"<svg onload=alert(1)>"), None);
        assert_eq!(image_mime(b""), None);
    }

    #[test]
    fn dropped_image_rejects_non_images() {
        let path = std::env::temp_dir().join(format!("voicereader-drop-{}.png", std::process::id()));
        std::fs::write(&path, b"not really a png").unwrap();
        let result = assistant_read_dropped_image(path.to_string_lossy().to_string());
        std::fs::remove_file(&path).ok();
        assert!(result.unwrap_err().contains("is not a PNG, JPEG or WebP image"));
    }

    #[test]
    fn dropped_image_rejects_directories_and_devices() {
        let dir = std::env::temp_dir();
        assert!(assistant_read_dropped_image(dir.to_string_lossy().to_string()).unwrap_err().contains("is not a file"));
        #[cfg(unix)]
        assert!(assistant_read_dropped_image("/dev/zero".to_string()).unwrap_err().contains("is not a file"));
    }
}
