use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use rand::{distributions::Alphanumeric, Rng};
use reqwest::{Client, Method};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, ClipboardManager, GlobalShortcutManager, Manager, RunEvent, State};
use tokio::time::{sleep, Duration};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

const MODEL_CUSTOM: &str = "qwen_custom_voice";
const MODEL_BASE: &str = "qwen_base_clone";
const MODEL_KYUTAI: &str = "kyutai_pocket_tts";
const QWEN_CUSTOM_REPO: &str = "Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice";
const QWEN_BASE_REPO: &str = "Qwen/Qwen3-TTS-12Hz-0.6B-Base";
const KYUTAI_REPO: &str = "Verylicious/pocket-tts-ungated";
const TERMINAL_EVENTS: [&str; 3] = ["JOB_DONE", "JOB_CANCELED", "JOB_ERROR"];
const SELECTION_COPY_TIMEOUT_MS: u64 = 500;
const SELECTION_COPY_POLL_MS: u64 = 25;
const HOTKEY_MODIFIER_RELEASE_TIMEOUT_MS: u64 = 350;
const HOTKEY_MODIFIER_RELEASE_POLL_MS: u64 = 10;
const DEFAULT_FALLBACK_HOTKEY: &str = "CmdOrCtrl+Shift+S";
const SETTINGS_FILE_NAME: &str = "settings.json";

#[derive(Clone)]
struct SharedState {
    inner: Arc<Mutex<EngineState>>,
}

#[derive(Clone)]
struct SpeakSettingsState {
    rate: f32,
    pitch: f32,
    volume: f32,
    chunk_max_chars: u32,
}

struct EngineState {
    child: Option<Child>,
    token: String,
    port: u16,
    base_url: String,
    selected_voice_id: String,
    selected_model: String,
    selected_qwen_speaker: String,
    selected_kyutai_voice: String,
    hotkey: String,
    speak_settings: SpeakSettingsState,
    last_job_id: Option<String>,
    suppressed_job_ids: HashSet<String>,
    startup_error: Option<String>,
}

impl Default for EngineState {
    fn default() -> Self {
        Self {
            child: None,
            token: String::new(),
            port: 0,
            base_url: String::new(),
            selected_voice_id: "0".to_string(),
            selected_model: MODEL_KYUTAI.to_string(),
            selected_qwen_speaker: "Ryan".to_string(),
            selected_kyutai_voice: "alba".to_string(),
            hotkey: default_hotkey(),
            speak_settings: SpeakSettingsState {
                rate: 1.0,
                pitch: 1.0,
                volume: 1.0,
                chunk_max_chars: 160,
            },
            last_job_id: None,
            suppressed_job_ids: HashSet::new(),
            startup_error: None,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct AppSettingsFile {
    hotkey: Option<String>,
}

#[derive(Serialize)]
struct ModelOption {
    id: String,
    label: String,
    status: String,
    notes: String,
}

#[derive(Serialize)]
struct SpeakerPreset {
    id: String,
    description: String,
    native_language: String,
}

#[derive(Serialize)]
struct BootstrapPayload {
    hotkey: String,
    selected_voice_id: String,
    selected_model: String,
    selected_speaker: String,
    startup_error: Option<String>,
    models: Vec<ModelOption>,
    preset_speakers: Vec<SpeakerPreset>,
    health: Value,
    voices: Value,
}

#[derive(Serialize)]
struct EngineRuntimePayload {
    running: bool,
    pid: Option<u32>,
    base_url: String,
    selected_voice_id: String,
    selected_model: String,
    selected_speaker: String,
}

#[derive(Serialize)]
struct GenericResult {
    ok: bool,
    message: String,
}

#[derive(Serialize)]
struct HotkeyResult {
    ok: bool,
    message: String,
    hotkey: String,
}

#[derive(Serialize)]
struct SelectModelResult {
    selected_model: String,
    selected_speaker: String,
    preset_speakers: Vec<SpeakerPreset>,
    applied: bool,
    message: String,
    health: Value,
}

#[derive(Clone, Serialize)]
struct JobStartedPayload {
    job_id: String,
    ws_url: String,
    source: String,
}

#[derive(Clone, Serialize)]
struct JobCancelRequestedPayload {
    job_id: String,
}

#[derive(Clone, Serialize)]
struct HotkeyUpdatedPayload {
    hotkey: String,
}

#[derive(Clone, Serialize)]
struct ErrorPayload {
    message: String,
}

#[derive(Deserialize)]
struct SpeakHttpResponse {
    job_id: String,
    ws_url: String,
}

struct SpeakerPresetRow {
    id: &'static str,
    description: &'static str,
    native_language: &'static str,
}

const QWEN_SPEAKER_PRESETS: [SpeakerPresetRow; 9] = [
    SpeakerPresetRow {
        id: "Vivian",
        description: "Bright, slightly edgy young female voice.",
        native_language: "Chinese",
    },
    SpeakerPresetRow {
        id: "Serena",
        description: "Warm, gentle young female voice.",
        native_language: "Chinese",
    },
    SpeakerPresetRow {
        id: "Uncle_Fu",
        description: "Seasoned male voice with a low, mellow timbre.",
        native_language: "Chinese",
    },
    SpeakerPresetRow {
        id: "Dylan",
        description: "Youthful Beijing male voice with a clear, natural timbre.",
        native_language: "Chinese (Beijing Dialect)",
    },
    SpeakerPresetRow {
        id: "Eric",
        description: "Lively Chengdu male voice with a slightly husky brightness.",
        native_language: "Chinese (Sichuan Dialect)",
    },
    SpeakerPresetRow {
        id: "Ryan",
        description: "Dynamic male voice with strong rhythmic drive.",
        native_language: "English",
    },
    SpeakerPresetRow {
        id: "Aiden",
        description: "Sunny American male voice with a clear midrange.",
        native_language: "English",
    },
    SpeakerPresetRow {
        id: "Ono_Anna",
        description: "Playful Japanese female voice with a light, nimble timbre.",
        native_language: "Japanese",
    },
    SpeakerPresetRow {
        id: "Sohee",
        description: "Warm Korean female voice with rich emotion.",
        native_language: "Korean",
    },
];

const KYUTAI_VOICE_PRESETS: [SpeakerPresetRow; 8] = [
    SpeakerPresetRow {
        id: "alba",
        description: "Balanced English female voice (Pocket TTS preset).",
        native_language: "English",
    },
    SpeakerPresetRow {
        id: "marius",
        description: "Clear English male voice (Pocket TTS preset).",
        native_language: "English",
    },
    SpeakerPresetRow {
        id: "javert",
        description: "Deep male voice (Pocket TTS preset).",
        native_language: "English",
    },
    SpeakerPresetRow {
        id: "jean",
        description: "Warm male voice (Pocket TTS preset).",
        native_language: "English",
    },
    SpeakerPresetRow {
        id: "fantine",
        description: "Soft female voice (Pocket TTS preset).",
        native_language: "English",
    },
    SpeakerPresetRow {
        id: "cosette",
        description: "Bright female voice (Pocket TTS preset).",
        native_language: "English",
    },
    SpeakerPresetRow {
        id: "eponine",
        description: "Expressive female voice (Pocket TTS preset).",
        native_language: "English",
    },
    SpeakerPresetRow {
        id: "azelma",
        description: "Natural female voice (Pocket TTS preset).",
        native_language: "English",
    },
];

pub fn run_app() {
    let state = SharedState {
        inner: Arc::new(Mutex::new(EngineState::default())),
    };

    let app = tauri::Builder::default()
        .manage(state)
        .setup(|app| {
            let handle = app.handle();
            let state = app.state::<SharedState>();
            if let Some(saved_hotkey) = load_saved_hotkey(&handle) {
                if let Ok(mut guard) = state.inner.lock() {
                    guard.hotkey = saved_hotkey;
                }
            }
            let init_result = tauri::async_runtime::block_on(async {
                initialize_engine_if_needed(&handle, &state.inner).await
            });
            if let Err(err) = init_result {
                let msg = format!("Engine startup failed during setup: {err:#}");
                eprintln!("{msg}");
                if let Ok(mut guard) = state.inner.lock() {
                    guard.startup_error = Some(msg);
                }
            }

            if let Err(err) = register_hotkey(&handle, state.inner.clone()) {
                let msg = format!("Global hotkey registration failed: {err:#}");
                eprintln!("{msg}");
                if let Ok(mut guard) = state.inner.lock() {
                    guard.startup_error = match guard.startup_error.take() {
                        Some(existing) => Some(format!("{existing}\n{msg}")),
                        None => Some(msg),
                    };
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_bootstrap,
            engine_health,
            engine_list_voices,
            engine_runtime_status,
            restart_engine,
            select_model,
            set_selected_voice,
            set_preset_speaker,
            set_speak_settings,
            set_hotkey,
            speak_text,
            trigger_read_selection,
            cancel_active_job,
        ])
        .build(tauri::generate_context!())
        .unwrap_or_else(|err| panic!("Failed to build VoiceReader app: {err}"));

    app.run(|app_handle, event| {
        handle_run_event(app_handle, &event);
    });
}

#[tauri::command]
async fn app_bootstrap(app: AppHandle, state: State<'_, SharedState>) -> Result<BootstrapPayload, String> {
    let mut startup_error: Option<String> = None;
    if let Err(err) = ensure_engine_ready(&app, &state.inner).await {
        let msg = to_cmd_error(err);
        startup_error = Some(msg.clone());
        if let Ok(mut guard) = state.inner.lock() {
            guard.startup_error = Some(msg);
        }
    }

    let health = match engine_health_inner(&state.inner).await {
        Ok(payload) => payload,
        Err(err) => {
            let msg = to_cmd_error(err);
            if startup_error.is_none() {
                startup_error = Some(msg.clone());
            }
            json!({
                "status": "unavailable",
                "error": startup_error.clone().unwrap_or(msg),
            })
        }
    };

    let voices = match engine_list_voices_inner(&state.inner).await {
        Ok(payload) => payload,
        Err(err) => {
            let msg = to_cmd_error(err);
            if startup_error.is_none() {
                startup_error = Some(msg.clone());
            }
            json!({
                "voices": [],
                "error": startup_error.clone().unwrap_or(msg),
            })
        }
    };

    let snapshot = {
        let guard = state.inner.lock().map_err(|_| "State lock poisoned".to_string())?;
        (
            guard.hotkey.clone(),
            guard.selected_voice_id.clone(),
            guard.selected_model.clone(),
            active_speaker_for_model(&guard),
            guard.startup_error.clone(),
        )
    };
    let selected_model = snapshot.2.clone();

    Ok(BootstrapPayload {
        hotkey: snapshot.0,
        selected_voice_id: snapshot.1,
        selected_model,
        selected_speaker: snapshot.3,
        startup_error: snapshot.4.or(startup_error),
        models: model_options(),
        preset_speakers: speaker_presets(&snapshot.2),
        health,
        voices,
    })
}

#[tauri::command]
async fn engine_health(app: AppHandle, state: State<'_, SharedState>) -> Result<Value, String> {
    ensure_engine_ready(&app, &state.inner).await.map_err(to_cmd_error)?;
    engine_health_inner(&state.inner).await.map_err(to_cmd_error)
}

#[tauri::command]
async fn engine_list_voices(app: AppHandle, state: State<'_, SharedState>) -> Result<Value, String> {
    ensure_engine_ready(&app, &state.inner).await.map_err(to_cmd_error)?;
    engine_list_voices_inner(&state.inner).await.map_err(to_cmd_error)
}

#[tauri::command]
fn engine_runtime_status(state: State<'_, SharedState>) -> Result<EngineRuntimePayload, String> {
    let mut guard = state.inner.lock().map_err(|_| "State lock poisoned".to_string())?;
    let (running, pid) = child_runtime_snapshot(&mut guard);
    let active_speaker = active_speaker_for_model(&guard);
    Ok(EngineRuntimePayload {
        running,
        pid,
        base_url: guard.base_url.clone(),
        selected_voice_id: guard.selected_voice_id.clone(),
        selected_model: guard.selected_model.clone(),
        selected_speaker: active_speaker,
    })
}

#[tauri::command]
async fn restart_engine(app: AppHandle, state: State<'_, SharedState>) -> Result<GenericResult, String> {
    shutdown_engine(&state.inner).await;
    initialize_engine_if_needed(&app, &state.inner)
        .await
        .map_err(to_cmd_error)?;

    let selected_model = {
        let guard = state.inner.lock().map_err(|_| "State lock poisoned".to_string())?;
        guard.selected_model.clone()
    };

    if selected_model == MODEL_CUSTOM {
        let _ = apply_custom_model_activation(&state.inner).await;
    } else if selected_model == MODEL_KYUTAI {
        let _ = apply_kyutai_model_activation(&state.inner).await;
    }

    Ok(GenericResult {
        ok: true,
        message: "Engine sidecar restarted and handshake completed".to_string(),
    })
}

#[tauri::command]
async fn select_model(
    app: AppHandle,
    state: State<'_, SharedState>,
    model: String,
) -> Result<SelectModelResult, String> {
    ensure_engine_ready(&app, &state.inner).await.map_err(to_cmd_error)?;
    let normalized = model.trim().to_string();

    match normalized.as_str() {
        MODEL_CUSTOM => {
            {
                let mut guard = state.inner.lock().map_err(|_| "State lock poisoned".to_string())?;
                guard.selected_model = MODEL_CUSTOM.to_string();
            }
            let _ = apply_custom_model_activation(&state.inner)
                .await
                .map_err(to_cmd_error)?;
            let health = engine_health_inner(&state.inner).await.map_err(to_cmd_error)?;
            let selected_speaker = {
                let guard = state.inner.lock().map_err(|_| "State lock poisoned".to_string())?;
                guard.selected_qwen_speaker.clone()
            };
            Ok(SelectModelResult {
                selected_model: MODEL_CUSTOM.to_string(),
                selected_speaker: selected_speaker.clone(),
                preset_speakers: speaker_presets(MODEL_CUSTOM),
                applied: true,
                message: "CustomVoice model is active for read-aloud".to_string(),
                health,
            })
        }
        MODEL_BASE => {
            {
                let mut guard = state.inner.lock().map_err(|_| "State lock poisoned".to_string())?;
                guard.selected_model = MODEL_BASE.to_string();
            }
            let health = engine_health_inner(&state.inner).await.map_err(to_cmd_error)?;
            Ok(SelectModelResult {
                selected_model: MODEL_BASE.to_string(),
                selected_speaker: {
                    let guard = state.inner.lock().map_err(|_| "State lock poisoned".to_string())?;
                    guard.selected_qwen_speaker.clone()
                },
                preset_speakers: speaker_presets(MODEL_CUSTOM),
                applied: false,
                message: "Base model mode is reserved for upcoming cloning UI".to_string(),
                health,
            })
        }
        MODEL_KYUTAI => {
            {
                let mut guard = state.inner.lock().map_err(|_| "State lock poisoned".to_string())?;
                guard.selected_model = MODEL_KYUTAI.to_string();
            }
            let _ = apply_kyutai_model_activation(&state.inner)
                .await
                .map_err(to_cmd_error)?;
            let health = engine_health_inner(&state.inner).await.map_err(to_cmd_error)?;
            Ok(SelectModelResult {
                selected_model: MODEL_KYUTAI.to_string(),
                selected_speaker: {
                    let guard = state.inner.lock().map_err(|_| "State lock poisoned".to_string())?;
                    guard.selected_kyutai_voice.clone()
                },
                preset_speakers: speaker_presets(MODEL_KYUTAI),
                applied: true,
                message: "Kyutai Pocket TTS model is active for read-aloud".to_string(),
                health,
            })
        }
        _ => Err("Unknown model id".to_string()),
    }
}

#[tauri::command]
fn set_selected_voice(state: State<'_, SharedState>, voice_id: String) -> Result<GenericResult, String> {
    let normalized = voice_id.trim().to_string();
    if normalized.is_empty() {
        return Err("voice_id cannot be empty".to_string());
    }

    let mut guard = state.inner.lock().map_err(|_| "State lock poisoned".to_string())?;
    guard.selected_voice_id = normalized.clone();
    Ok(GenericResult {
        ok: true,
        message: format!("Selected voice set to {normalized}"),
    })
}

#[tauri::command]
async fn set_preset_speaker(
    app: AppHandle,
    state: State<'_, SharedState>,
    speaker_id: String,
) -> Result<SelectModelResult, String> {
    ensure_engine_ready(&app, &state.inner).await.map_err(to_cmd_error)?;

    let selected_model = {
        let guard = state.inner.lock().map_err(|_| "State lock poisoned".to_string())?;
        guard.selected_model.clone()
    };

    match selected_model.as_str() {
        MODEL_CUSTOM => {
            if !QWEN_SPEAKER_PRESETS.iter().any(|row| row.id == speaker_id) {
                return Err("Unsupported Qwen speaker id".to_string());
            }

            {
                let mut guard = state.inner.lock().map_err(|_| "State lock poisoned".to_string())?;
                guard.selected_qwen_speaker = speaker_id.clone();
            }

            let _ = apply_custom_model_activation(&state.inner)
                .await
                .map_err(to_cmd_error)?;
            let health = engine_health_inner(&state.inner).await.map_err(to_cmd_error)?;
            Ok(SelectModelResult {
                selected_model: MODEL_CUSTOM.to_string(),
                selected_speaker: speaker_id.clone(),
                preset_speakers: speaker_presets(MODEL_CUSTOM),
                applied: true,
                message: format!("Qwen preset speaker switched to {speaker_id}"),
                health,
            })
        }
        MODEL_KYUTAI => {
            if !KYUTAI_VOICE_PRESETS.iter().any(|row| row.id == speaker_id) {
                return Err("Unsupported Kyutai voice prompt".to_string());
            }

            {
                let mut guard = state.inner.lock().map_err(|_| "State lock poisoned".to_string())?;
                guard.selected_kyutai_voice = speaker_id.clone();
            }

            let _ = apply_kyutai_model_activation(&state.inner)
                .await
                .map_err(to_cmd_error)?;
            let health = engine_health_inner(&state.inner).await.map_err(to_cmd_error)?;
            Ok(SelectModelResult {
                selected_model: MODEL_KYUTAI.to_string(),
                selected_speaker: speaker_id.clone(),
                preset_speakers: speaker_presets(MODEL_KYUTAI),
                applied: true,
                message: format!("Kyutai voice prompt switched to {speaker_id}"),
                health,
            })
        }
        _ => {
            let health = engine_health_inner(&state.inner).await.map_err(to_cmd_error)?;
            Ok(SelectModelResult {
                selected_model,
                selected_speaker: speaker_id,
                preset_speakers: speaker_presets(MODEL_CUSTOM),
                applied: false,
                message: "Preset is ignored in base clone mode".to_string(),
                health,
            })
        }
    }
}

#[tauri::command]
fn set_speak_settings(
    state: State<'_, SharedState>,
    rate: f32,
    pitch: f32,
    volume: f32,
    chunk_max_chars: u32,
) -> Result<GenericResult, String> {
    if !(0.25..=4.0).contains(&rate) {
        return Err("rate must be in [0.25, 4.0]".to_string());
    }
    if !(0.5..=2.0).contains(&pitch) {
        return Err("pitch must be in [0.5, 2.0]".to_string());
    }
    if !(0.0..=2.0).contains(&volume) {
        return Err("volume must be in [0.0, 2.0]".to_string());
    }
    if !(100..=2000).contains(&chunk_max_chars) {
        return Err("chunk_max_chars must be in [100, 2000]".to_string());
    }

    let mut guard = state.inner.lock().map_err(|_| "State lock poisoned".to_string())?;
    guard.speak_settings = SpeakSettingsState {
        rate,
        pitch,
        volume,
        chunk_max_chars,
    };

    Ok(GenericResult {
        ok: true,
        message: "Playback settings updated".to_string(),
    })
}

#[tauri::command]
fn set_hotkey(
    app: AppHandle,
    state: State<'_, SharedState>,
    hotkey: String,
) -> Result<HotkeyResult, String> {
    let normalized = normalize_hotkey(&hotkey).map_err(to_cmd_error)?;
    if is_hotkey_os_reserved(&normalized) {
        return Err(
            "Alt+Space (Windows) and Cmd+Space (macOS) are OS-reserved. Use another hotkey."
                .to_string(),
        );
    }

    let previous = {
        let guard = state.inner.lock().map_err(|_| "State lock poisoned".to_string())?;
        guard.hotkey.clone()
    };

    if normalized == previous {
        return Ok(HotkeyResult {
            ok: true,
            message: "Hotkey unchanged".to_string(),
            hotkey: normalized,
        });
    }

    let mut manager = app.global_shortcut_manager();
    let _ = manager.unregister(&previous);

    if let Err(err) = register_hotkey_binding(&app, state.inner.clone(), &normalized) {
        let _ = register_hotkey_binding(&app, state.inner.clone(), &previous);
        return Err(to_cmd_error(err.context("Failed to register selected hotkey")));
    }

    {
        let mut guard = state.inner.lock().map_err(|_| "State lock poisoned".to_string())?;
        guard.hotkey = normalized.clone();
    }

    if let Err(err) = persist_hotkey(&app, &normalized) {
        let _ = app.emit_all(
            "voicereader:error",
            ErrorPayload {
                message: format!("Hotkey set but could not persist settings: {err:#}"),
            },
        );
    }

    let _ = app.emit_all(
        "voicereader:hotkey-updated",
        HotkeyUpdatedPayload {
            hotkey: normalized.clone(),
        },
    );

    Ok(HotkeyResult {
        ok: true,
        message: format!("Global hotkey updated to {normalized}"),
        hotkey: normalized,
    })
}

#[tauri::command]
async fn speak_text(
    app: AppHandle,
    state: State<'_, SharedState>,
    text: String,
) -> Result<GenericResult, String> {
    ensure_engine_ready(&app, &state.inner).await.map_err(to_cmd_error)?;
    let job_id = speak_and_stream(&app, &state.inner, text, "manual")
        .await
        .map_err(to_cmd_error)?;
    Ok(GenericResult {
        ok: true,
        message: format!("Speak job started: {job_id}"),
    })
}

#[tauri::command]
async fn trigger_read_selection(app: AppHandle, state: State<'_, SharedState>) -> Result<GenericResult, String> {
    ensure_engine_ready(&app, &state.inner).await.map_err(to_cmd_error)?;
    read_selection_and_speak_inner(&app, &state.inner)
        .await
        .map_err(to_cmd_error)?;
    Ok(GenericResult {
        ok: true,
        message: "Read-selection hotkey flow triggered".to_string(),
    })
}

#[tauri::command]
async fn cancel_active_job(app: AppHandle, state: State<'_, SharedState>) -> Result<GenericResult, String> {
    ensure_engine_ready(&app, &state.inner).await.map_err(to_cmd_error)?;

    let (job_id, base_url, token) = {
        let guard = state.inner.lock().map_err(|_| "State lock poisoned".to_string())?;
        (
            guard.last_job_id.clone(),
            guard.base_url.clone(),
            guard.token.clone(),
        )
    };

    let Some(job_id) = job_id else {
        return Ok(GenericResult {
            ok: true,
            message: "No active job to cancel".to_string(),
        });
    };

    {
        let mut guard = state.inner.lock().map_err(|_| "State lock poisoned".to_string())?;
        guard.suppressed_job_ids.insert(job_id.clone());
    }
    let _ = app.emit_all(
        "voicereader:job-cancel-requested",
        JobCancelRequestedPayload {
            job_id: job_id.clone(),
        },
    );

    let _ = request_json(
        Method::POST,
        &format!("{base_url}/v1/cancel"),
        &token,
        Some(json!({ "job_id": job_id })),
    )
    .await
    .map_err(to_cmd_error)?;

    {
        let mut guard = state.inner.lock().map_err(|_| "State lock poisoned".to_string())?;
        if guard.last_job_id.as_deref() == Some(job_id.as_str()) {
            guard.last_job_id = None;
        }
    }

    Ok(GenericResult {
        ok: true,
        message: format!("Cancel request sent for job {job_id}"),
    })
}

fn register_hotkey(app: &AppHandle, state: Arc<Mutex<EngineState>>) -> Result<()> {
    let hotkey = {
        let guard = state.lock().map_err(|_| anyhow!("State lock poisoned"))?;
        guard.hotkey.clone()
    };

    if is_hotkey_os_reserved(&hotkey) {
        return Err(anyhow!(
            "Selected hotkey {hotkey} is OS-reserved. Use a non-reserved combination."
        ));
    }

    if register_hotkey_binding(app, state.clone(), &hotkey).is_ok() {
        return Ok(());
    }

    if hotkey != DEFAULT_FALLBACK_HOTKEY {
        register_hotkey_binding(app, state.clone(), DEFAULT_FALLBACK_HOTKEY)
            .with_context(|| format!("Failed to register fallback hotkey {DEFAULT_FALLBACK_HOTKEY}"))?;
        if let Ok(mut guard) = state.lock() {
            guard.hotkey = DEFAULT_FALLBACK_HOTKEY.to_string();
        }
        let _ = persist_hotkey(app, DEFAULT_FALLBACK_HOTKEY);
        let _ = app.emit_all(
            "voicereader:hotkey-updated",
            HotkeyUpdatedPayload {
                hotkey: DEFAULT_FALLBACK_HOTKEY.to_string(),
            },
        );
        return Ok(());
    }

    Err(anyhow!("Failed to register global hotkey {hotkey}"))
}

fn register_hotkey_binding(app: &AppHandle, state: Arc<Mutex<EngineState>>, hotkey: &str) -> Result<()> {
    let hotkey = normalize_hotkey(hotkey)?;
    let app_handle = app.clone();
    app.global_shortcut_manager()
        .register(&hotkey, move || {
            let app_clone = app_handle.clone();
            let state_clone = state.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(err) = read_selection_and_speak_inner(&app_clone, &state_clone).await {
                    emit_error(&app_clone, &format!("Hotkey flow failed: {err:#}"));
                }
            });
        })
        .with_context(|| format!("Failed to register global hotkey {hotkey}"))?;

    Ok(())
}

async fn read_selection_and_speak_inner(app: &AppHandle, state: &Arc<Mutex<EngineState>>) -> Result<()> {
    ensure_engine_ready(app, state).await?;

    let text = capture_selected_text_from_active_app(app).await;
    let Some(text) = text else {
        let _ = app.emit_all(
            "voicereader:selection-empty",
            json!({ "reason": "no_selection_detected" }),
        );
        return Ok(());
    };

    let _ = speak_and_stream(app, state, text, "hotkey_selection_capture").await?;
    Ok(())
}

async fn capture_selected_text_from_active_app(app: &AppHandle) -> Option<String> {
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
        return None;
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

    captured
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

    #[cfg(not(target_os = "windows"))]
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

    #[cfg(not(target_os = "windows"))]
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

async fn speak_and_stream(
    app: &AppHandle,
    state: &Arc<Mutex<EngineState>>,
    text: String,
    source: &str,
) -> Result<String> {
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        return Err(anyhow!("Speak text cannot be empty"));
    }

    let (base_url, token, voice_id, selected_model, settings) = {
        let guard = state.lock().map_err(|_| anyhow!("State lock poisoned"))?;
        (
            guard.base_url.clone(),
            guard.token.clone(),
            guard.selected_voice_id.clone(),
            guard.selected_model.clone(),
            guard.speak_settings.clone(),
        )
    };

    if selected_model != MODEL_CUSTOM && selected_model != MODEL_KYUTAI {
        return Err(anyhow!(
            "Current model mode ({selected_model}) is not enabled for read-aloud yet. Switch to qwen_custom_voice or kyutai_pocket_tts."
        ));
    }

    let speak_body = json!({
        "voice_id": voice_id,
        "text": trimmed,
        "settings": {
            "rate": settings.rate,
            "pitch": settings.pitch,
            "volume": settings.volume,
            "chunking": {
                "max_chars": settings.chunk_max_chars,
            }
        }
    });

    let speak_payload = request_json(
        Method::POST,
        &format!("{base_url}/v1/speak"),
        &token,
        Some(speak_body),
    )
    .await?;

    let speak_response: SpeakHttpResponse = serde_json::from_value(speak_payload)
        .context("Invalid /v1/speak response shape")?;

    {
        let mut guard = state.lock().map_err(|_| anyhow!("State lock poisoned"))?;
        guard.last_job_id = Some(speak_response.job_id.clone());
        guard.suppressed_job_ids.remove(&speak_response.job_id);
        if guard.suppressed_job_ids.len() > 128 {
            guard.suppressed_job_ids.clear();
        }
    }

    let _ = app.emit_all(
        "voicereader:job-started",
        JobStartedPayload {
            job_id: speak_response.job_id.clone(),
            ws_url: speak_response.ws_url.clone(),
            source: source.to_string(),
        },
    );

    let app_clone = app.clone();
    let state_clone = state.clone();
    let token_clone = token.clone();
    let ws_url = speak_response.ws_url.clone();
    let job_id = speak_response.job_id.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(err) = relay_ws_events(&app_clone, &state_clone, &ws_url, &token_clone, &job_id).await {
            emit_error(&app_clone, &format!("WS relay failed: {err:#}"));
        }
    });

    Ok(speak_response.job_id)
}

async fn relay_ws_events(
    app: &AppHandle,
    state: &Arc<Mutex<EngineState>>,
    ws_url: &str,
    token: &str,
    job_id: &str,
) -> Result<()> {
    let protocol_header = format!("auth.bearer.v1, {token}");
    let mut request = ws_url
        .into_client_request()
        .context("Failed to construct WS request")?;
    request
        .headers_mut()
        .insert(SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_str(&protocol_header)?);

    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .context("Failed to connect WS stream")?;

    while let Some(message) = socket.next().await {
        if is_job_suppressed(state, job_id) {
            break;
        }
        match message {
            Ok(Message::Text(text)) => {
                let parsed: Value = serde_json::from_str(&text)
                    .unwrap_or_else(|_| json!({ "type": "RAW_TEXT", "raw": text }));

                if is_job_suppressed(state, job_id) {
                    break;
                }

                let _ = app.emit_all("voicereader:ws-event", parsed.clone());

                if let Some(kind) = parsed.get("type").and_then(Value::as_str) {
                    if TERMINAL_EVENTS.contains(&kind) {
                        break;
                    }
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(err) => {
                return Err(anyhow!("WS stream read error: {err}"));
            }
        }
    }

    let mut guard = state.lock().map_err(|_| anyhow!("State lock poisoned"))?;
    if guard.last_job_id.as_deref() == Some(job_id) {
        guard.last_job_id = None;
    }
    guard.suppressed_job_ids.remove(job_id);
    Ok(())
}

fn is_job_suppressed(state: &Arc<Mutex<EngineState>>, job_id: &str) -> bool {
    match state.lock() {
        Ok(guard) => guard.suppressed_job_ids.contains(job_id),
        Err(_) => false,
    }
}

async fn ensure_engine_ready(app: &AppHandle, state: &Arc<Mutex<EngineState>>) -> Result<()> {
    let running = {
        let mut guard = state.lock().map_err(|_| anyhow!("State lock poisoned"))?;
        child_runtime_snapshot(&mut guard).0
    };

    if running {
        return Ok(());
    }

    initialize_engine_if_needed(app, state).await
}

async fn initialize_engine_if_needed(app: &AppHandle, state: &Arc<Mutex<EngineState>>) -> Result<()> {
    let running = {
        let mut guard = state.lock().map_err(|_| anyhow!("State lock poisoned"))?;
        child_runtime_snapshot(&mut guard).0
    };
    if running {
        return Ok(());
    }

    let engine_root = find_engine_root()?;
    let python_executable = resolve_python_executable(&engine_root);

    let token = generate_token();
    let port = portpicker::pick_unused_port().ok_or_else(|| anyhow!("Failed to find a free localhost port"))?;
    let base_url = format!("http://127.0.0.1:{port}");
    let data_dir = engine_root.join(".data");
    std::fs::create_dir_all(&data_dir).context("Failed to create engine data dir")?;

    let mut command = Command::new(&python_executable);
    command
        .args([
            "-m",
            "tts_engine",
            "--server",
            "--port",
            &port.to_string(),
            "--data-dir",
            data_dir
                .to_str()
                .ok_or_else(|| anyhow!("Invalid engine data dir path"))?,
        ])
        .current_dir(&engine_root)
        .env("SPEAK_SELECTION_ENGINE_TOKEN", &token)
        .env("PYTHONPATH", engine_root.join("src"))
        .env("VOICEREADER_SYNTH_BACKEND", "auto")
        .env("VOICEREADER_KYUTAI_MODEL", KYUTAI_REPO)
        .env("VOICEREADER_KYUTAI_VOICE_PROMPT", "alba")
        .env("VOICEREADER_QWEN_MODEL", QWEN_CUSTOM_REPO)
        .env("VOICEREADER_QWEN_SPEAKER", "Ryan");

    if cfg!(target_os = "windows") {
        // FlashAttention2 is often unavailable on Windows; use SDPA directly for stable startup.
        command.env("VOICEREADER_QWEN_ATTN_IMPLEMENTATION", "sdpa");
    }

    if cfg!(debug_assertions) {
        command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    } else {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }

    let child = command
        .spawn()
        .with_context(|| format!("Failed to launch engine sidecar via {python_executable}"))?;

    {
        let mut guard = state.lock().map_err(|_| anyhow!("State lock poisoned"))?;
        guard.child = Some(child);
        guard.token = token;
        guard.port = port;
        guard.base_url = base_url;
        guard.last_job_id = None;
        guard.suppressed_job_ids.clear();
    }

    let health = wait_for_engine_health(state).await?;
    let _ = app.emit_all("voicereader:engine-ready", health);

    if let Ok(mut guard) = state.lock() {
        guard.startup_error = None;
    }

    let selected_model = {
        let guard = state.lock().map_err(|_| anyhow!("State lock poisoned"))?;
        guard.selected_model.clone()
    };
    if selected_model == MODEL_CUSTOM {
        let _ = apply_custom_model_activation(state).await;
    } else if selected_model == MODEL_KYUTAI {
        let _ = apply_kyutai_model_activation(state).await;
    }

    Ok(())
}

async fn shutdown_engine(state: &Arc<Mutex<EngineState>>) {
    let (base_url, token) = {
        let guard = match state.lock() {
            Ok(v) => v,
            Err(_) => return,
        };
        (guard.base_url.clone(), guard.token.clone())
    };

    if !base_url.is_empty() && !token.is_empty() {
        let _ = request_json(Method::POST, &format!("{base_url}/v1/quit"), &token, Some(json!({}))).await;
        sleep(Duration::from_millis(400)).await;
    }

    let mut guard = match state.lock() {
        Ok(v) => v,
        Err(_) => return,
    };

    if let Some(child) = guard.child.as_mut() {
        match child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => {
                let _ = child.kill();
            }
            Err(_) => {
                let _ = child.kill();
            }
        }
    }
    guard.child = None;
    guard.last_job_id = None;
    guard.suppressed_job_ids.clear();
}

async fn wait_for_engine_health(state: &Arc<Mutex<EngineState>>) -> Result<Value> {
    for _ in 0..100 {
        let (base_url, token, running) = {
            let mut guard = state.lock().map_err(|_| anyhow!("State lock poisoned"))?;
            let (running, _) = child_runtime_snapshot(&mut guard);
            (guard.base_url.clone(), guard.token.clone(), running)
        };

        if !running {
            return Err(anyhow!("Engine process exited during startup"));
        }

        match request_json(Method::GET, &format!("{base_url}/v1/health"), &token, None).await {
            Ok(payload) => return Ok(payload),
            Err(_) => sleep(Duration::from_millis(200)).await,
        }
    }

    Err(anyhow!("Engine did not become healthy within startup timeout"))
}

async fn apply_custom_model_activation(state: &Arc<Mutex<EngineState>>) -> Result<Value> {
    let (base_url, token, speaker) = {
        let guard = state.lock().map_err(|_| anyhow!("State lock poisoned"))?;
        (
            guard.base_url.clone(),
            guard.token.clone(),
            guard.selected_qwen_speaker.clone(),
        )
    };

    let payload = json!({
        "synth_backend": "qwen",
        "active_model_id": "qwen3-tts-12hz-0.6b-customvoice",
        "qwen_model_name": QWEN_CUSTOM_REPO,
        "qwen_default_speaker": speaker,
        "warmup_wait": true,
        "warmup_force": true,
        "reason": "app_custom_voice_activation",
    });

    request_json(
        Method::POST,
        &format!("{base_url}/v1/models/activate"),
        &token,
        Some(payload),
    )
    .await
}

async fn apply_kyutai_model_activation(state: &Arc<Mutex<EngineState>>) -> Result<Value> {
    let (base_url, token, voice_prompt) = {
        let guard = state.lock().map_err(|_| anyhow!("State lock poisoned"))?;
        (
            guard.base_url.clone(),
            guard.token.clone(),
            guard.selected_kyutai_voice.clone(),
        )
    };

    let payload = json!({
        "synth_backend": "kyutai",
        "active_model_id": "kyutai-pocket-tts-ungated",
        "kyutai_model_name": KYUTAI_REPO,
        "kyutai_voice_prompt": voice_prompt,
        "warmup_wait": true,
        "warmup_force": true,
        "reason": "app_kyutai_activation",
    });

    request_json(
        Method::POST,
        &format!("{base_url}/v1/models/activate"),
        &token,
        Some(payload),
    )
    .await
}

async fn engine_health_inner(state: &Arc<Mutex<EngineState>>) -> Result<Value> {
    let (base_url, token) = {
        let guard = state.lock().map_err(|_| anyhow!("State lock poisoned"))?;
        (guard.base_url.clone(), guard.token.clone())
    };

    request_json(Method::GET, &format!("{base_url}/v1/health"), &token, None).await
}

async fn engine_list_voices_inner(state: &Arc<Mutex<EngineState>>) -> Result<Value> {
    let (base_url, token) = {
        let guard = state.lock().map_err(|_| anyhow!("State lock poisoned"))?;
        (guard.base_url.clone(), guard.token.clone())
    };

    request_json(Method::GET, &format!("{base_url}/v1/voices"), &token, None).await
}

async fn request_json(method: Method, url: &str, token: &str, body: Option<Value>) -> Result<Value> {
    let client = Client::new();
    let mut request = client
        .request(method, url)
        .header("Authorization", format!("Bearer {token}"));

    if let Some(payload) = body {
        request = request.json(&payload);
    }

    let response = request.send().await.with_context(|| format!("Request failed for {url}"))?;
    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_else(|_| String::new());
        return Err(anyhow!("Request to {url} failed with status {status}: {body_text}"));
    }

    response
        .json::<Value>()
        .await
        .with_context(|| format!("Failed to decode JSON response for {url}"))
}

fn model_options() -> Vec<ModelOption> {
    vec![
        ModelOption {
            id: MODEL_KYUTAI.to_string(),
            label: "Kyutai Pocket TTS".to_string(),
            status: "ready".to_string(),
            notes: format!("Main read-aloud path ({KYUTAI_REPO})"),
        },
        ModelOption {
            id: MODEL_CUSTOM.to_string(),
            label: "Qwen CustomVoice (preset speakers)".to_string(),
            status: "ready".to_string(),
            notes: format!("Secondary path ({QWEN_CUSTOM_REPO})"),
        },
        ModelOption {
            id: MODEL_BASE.to_string(),
            label: "Qwen Base (clone path)".to_string(),
            status: "planned".to_string(),
            notes: format!("Model repo prefetched: {QWEN_BASE_REPO}"),
        },
    ]
}

fn speaker_presets(model: &str) -> Vec<SpeakerPreset> {
    let presets: &[SpeakerPresetRow] = match model {
        MODEL_KYUTAI => &KYUTAI_VOICE_PRESETS,
        _ => &QWEN_SPEAKER_PRESETS,
    };

    presets
        .iter()
        .map(|row| SpeakerPreset {
            id: row.id.to_string(),
            description: row.description.to_string(),
            native_language: row.native_language.to_string(),
        })
        .collect()
}

fn active_speaker_for_model(state: &EngineState) -> String {
    match state.selected_model.as_str() {
        MODEL_KYUTAI => state.selected_kyutai_voice.clone(),
        _ => state.selected_qwen_speaker.clone(),
    }
}

fn default_hotkey() -> String {
    #[cfg(target_os = "macos")]
    {
        return "Cmd+Shift+Space".to_string();
    }
    #[cfg(target_os = "windows")]
    {
        return "Alt+Shift+Space".to_string();
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        "Ctrl+Shift+S".to_string()
    }
}

fn normalize_hotkey(value: &str) -> Result<String> {
    let normalized = value.trim().replace(' ', "");
    if normalized.is_empty() {
        return Err(anyhow!("Hotkey cannot be empty"));
    }
    Ok(normalized)
}

fn is_hotkey_os_reserved(hotkey: &str) -> bool {
    let normalized = hotkey.trim().to_lowercase().replace(' ', "");
    matches!(
        normalized.as_str(),
        "alt+space" | "cmd+space" | "command+space" | "meta+space" | "super+space"
    )
}

fn load_saved_hotkey(app: &AppHandle) -> Option<String> {
    let path = app_settings_path(app)?;
    let body = std::fs::read_to_string(path).ok()?;
    let parsed: AppSettingsFile = serde_json::from_str(&body).ok()?;
    let candidate = parsed.hotkey?;
    let normalized = normalize_hotkey(&candidate).ok()?;
    if is_hotkey_os_reserved(&normalized) {
        return None;
    }
    Some(normalized)
}

fn persist_hotkey(app: &AppHandle, hotkey: &str) -> Result<()> {
    let path = app_settings_path(app).ok_or_else(|| anyhow!("Unable to resolve app settings path"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("Failed to create app settings directory {}", parent.display())
        })?;
    }

    let settings = AppSettingsFile {
        hotkey: Some(hotkey.to_string()),
    };
    let serialized = serde_json::to_string_pretty(&settings)?;
    std::fs::write(&path, serialized)
        .with_context(|| format!("Failed to write app settings file {}", path.display()))?;
    Ok(())
}

fn app_settings_path(app: &AppHandle) -> Option<PathBuf> {
    app.path_resolver()
        .app_config_dir()
        .map(|path| path.join(SETTINGS_FILE_NAME))
}

fn child_runtime_snapshot(state: &mut EngineState) -> (bool, Option<u32>) {
    if let Some(child) = state.child.as_mut() {
        match child.try_wait() {
            Ok(Some(_)) => {
                state.child = None;
                (false, None)
            }
            Ok(None) => (true, Some(child.id())),
            Err(_) => {
                state.child = None;
                (false, None)
            }
        }
    } else {
        (false, None)
    }
}

fn find_engine_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("Failed to resolve current directory")?;
    let candidates = [
        cwd.join("tts-engine"),
        cwd.join("../tts-engine"),
        cwd.join("../../tts-engine"),
    ];

    for candidate in candidates {
        let marker = candidate.join("src").join("tts_engine").join("main.py");
        if marker.exists() {
            let canonical = candidate.canonicalize().unwrap_or(candidate);
            return Ok(normalize_windows_extended_path(canonical));
        }
    }

    Err(anyhow!(
        "Unable to locate tts-engine directory. Expected one of ./tts-engine, ../tts-engine, ../../tts-engine"
    ))
}

fn resolve_python_executable(engine_root: &Path) -> String {
    #[cfg(target_os = "windows")]
    {
        let venv_python = engine_root.join(".venv").join("Scripts").join("python.exe");
        if venv_python.exists() {
            return venv_python.to_string_lossy().to_string();
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let venv_python = engine_root.join(".venv").join("bin").join("python");
        if venv_python.exists() {
            return venv_python.to_string_lossy().to_string();
        }
    }

    "python".to_string()
}

fn normalize_windows_extended_path(path: PathBuf) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let raw = path.to_string_lossy();
        if let Some(stripped) = raw.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{stripped}"));
        }
        if let Some(stripped) = raw.strip_prefix(r"\\?\") {
            return PathBuf::from(stripped);
        }
    }
    path
}

fn generate_token() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(48)
        .map(char::from)
        .collect()
}

fn emit_error(app: &AppHandle, message: &str) {
    let _ = app.emit_all(
        "voicereader:error",
        ErrorPayload {
            message: message.to_string(),
        },
    );
}

fn to_cmd_error(err: anyhow::Error) -> String {
    format!("{err:#}")
}

impl Drop for SharedState {
    fn drop(&mut self) {
        let state = self.inner.clone();
        tauri::async_runtime::block_on(async {
            shutdown_engine(&state).await;
        });
    }
}

#[allow(clippy::needless_pass_by_value)]
pub fn handle_run_event(app: &AppHandle, event: &RunEvent) {
    if matches!(event, RunEvent::ExitRequested { .. } | RunEvent::Exit) {
        if let Some(state) = app.try_state::<SharedState>() {
            let state = state.inner.clone();
            tauri::async_runtime::block_on(async {
                shutdown_engine(&state).await;
            });
        }
    }
}
