//! Voice conversation answers (plan §6, §8). The model's answer streams to the page as
//! text (the same `assistant:chat-*` events as the chat page) and, phrase by phrase, to the
//! speakers through one [`AssistantSpeechJob`]. Turn-taking, the microphone and history
//! stay in the page, which hears exactly what was played.
//!
//! Per answer:
//! - the Voice system prompt, thinking off;
//! - deltas go through a [`PhraseBuffer`] (sentences or clauses, cleaned for speech), and each
//!   phrase is announced as `assistant:voice-phrase` and then synthesised;
//! - synthesis waits while more than [`MAX_BUFFERED_SECS`] of audio is queued but unheard,
//!   as reported by the page (`assistant:playback`), so a fast model cannot build an
//!   unbounded backlog;
//! - a read-aloud request cancels the speech job (plan §6.2); the answer's model request is
//!   then cancelled too, and `assistant:voice-ended` says so.
//! See `docs/assistant/conversation.md`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::assistant_backend::{AssistantState, UseGuard};
use crate::assistant_catalogue;
use crate::assistant_chat::{self, ChatMessage, ChatRequest, ChatSink, ChatStart, ChatTarget, Sampling, DEFAULT_VOICE_PROMPT};
use crate::assistant_phrases::{PhraseBuffer, PhraseConfig};
use crate::settings::load_assistant_prompts;
use crate::voicereader_core::{start_assistant_speech, AssistantSpeechJob};

/// Audio queued but not yet heard above which synthesis waits (plan §6.1).
const MAX_BUFFERED_SECS: f32 = 8.0;
/// How often the speech worker checks for stalled text, cancellation and backpressure.
const TICK: Duration = Duration::from_millis(100);
/// A playback report older than this no longer holds synthesis back.
const PLAYBACK_REPORT_TTL: Duration = Duration::from_secs(3);

/// What the page needs when an answer starts.
#[derive(Serialize)]
pub(crate) struct VoiceStart {
    #[serde(flatten)]
    pub chat: ChatStart,
    pub job_id: String,
}

#[derive(Clone, Serialize)]
struct VoicePhrasePayload<'a> {
    request_id: &'a str,
    job_id: &'a str,
    seq: u32,
    text: &'a str,
}

#[derive(Clone, Serialize)]
struct VoiceEndedPayload<'a> {
    request_id: &'a str,
    job_id: &'a str,
    /// "done": every phrase was handed to the player; "cancelled": stopped by the page;
    /// "preempted": read-aloud took over; "error": synthesis failed.
    reason: &'a str,
    message: Option<&'a str>,
}

/// `assistant:playback`, sent by the page about every 250 ms while a voice answer plays.
#[derive(Deserialize)]
struct PlaybackReport {
    job_id: String,
    buffered_secs: f32,
}

fn playback_reports() -> &'static Mutex<HashMap<String, (f32, Instant)>> {
    static REPORTS: OnceLock<Mutex<HashMap<String, (f32, Instant)>>> = OnceLock::new();
    REPORTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Records a playback report from the page. Registered as a global event listener at setup.
pub(crate) fn on_playback_event(payload: Option<&str>) {
    let Some(report) = payload.and_then(|text| serde_json::from_str::<PlaybackReport>(text).ok()) else { return };
    if let Ok(mut reports) = playback_reports().lock() {
        reports.insert(report.job_id, (report.buffered_secs.max(0.0), Instant::now()));
    }
}

fn buffered_secs(job_id: &str) -> f32 {
    playback_reports()
        .lock()
        .ok()
        .and_then(|reports| reports.get(job_id).copied())
        .filter(|(_, at)| at.elapsed() < PLAYBACK_REPORT_TTL)
        .map(|(secs, _)| secs)
        .unwrap_or(0.0)
}

/// A running voice answer: its speech job, and whether the page cancelled it (as opposed
/// to read-aloud preempting it).
struct RunningAnswer {
    speech: Arc<AssistantSpeechJob>,
    cancelled_by_page: Arc<AtomicBool>,
}

/// Running voice answers, by request id, so the page can cancel one.
fn running() -> &'static Mutex<HashMap<String, RunningAnswer>> {
    static RUNNING: OnceLock<Mutex<HashMap<String, RunningAnswer>>> = OnceLock::new();
    RUNNING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Answers whose synthesis has finished but whose audio may still be playing, as
/// `(request_id, job_id)`, newest last. A cancel then stops the playback (Codex F5).
fn finished_answers() -> &'static Mutex<std::collections::VecDeque<(String, String)>> {
    static FINISHED: OnceLock<Mutex<std::collections::VecDeque<(String, String)>>> = OnceLock::new();
    FINISHED.get_or_init(|| Mutex::new(std::collections::VecDeque::new()))
}
/// How many finished answers are remembered for that.
const FINISHED_ANSWERS_KEPT: usize = 8;

enum SpeechMessage {
    Delta(String),
    Done,
    Stopped,
}

/// Forwards the answer's text to the page and to the speech worker.
struct VoiceSink {
    app: AppHandle,
    guard: Mutex<Option<UseGuard>>,
    to_speech: Mutex<Sender<SpeechMessage>>,
}

impl VoiceSink {
    fn send(&self, message: SpeechMessage) {
        if let Ok(sender) = self.to_speech.lock() {
            let _ = sender.send(message);
        }
    }

    fn release(&self) {
        if let Ok(mut guard) = self.guard.lock() {
            guard.take();
        }
    }
}

impl ChatSink for VoiceSink {
    fn delta(&self, request_id: &str, content: Option<&str>, _reasoning: Option<&str>) {
        // Thinking is off for voice; any reasoning that still arrives is neither shown nor spoken.
        let Some(content) = content.filter(|text| !text.is_empty()) else { return };
        let _ = self.app.emit_all(
            "assistant:chat-delta",
            serde_json::json!({"request_id": request_id, "content": content, "reasoning": null}),
        );
        self.send(SpeechMessage::Delta(content.to_string()));
    }

    fn done(&self, request_id: &str, finish_reason: Option<&str>, stopped: bool) {
        let _ = self.app.emit_all(
            "assistant:chat-done",
            serde_json::json!({"request_id": request_id, "finish_reason": finish_reason, "stopped": stopped}),
        );
        self.send(if stopped { SpeechMessage::Stopped } else { SpeechMessage::Done });
        self.release();
    }

    fn error(&self, request_id: &str, message: &str) {
        let _ = self.app.emit_all("assistant:chat-error", serde_json::json!({"request_id": request_id, "message": message}));
        self.send(SpeechMessage::Stopped);
        self.release();
    }
}

/// Starts a spoken answer to `messages` (the page's history plus the new user turn).
#[tauri::command]
pub(crate) async fn assistant_voice_respond(
    app: AppHandle,
    state: State<'_, AssistantState>,
    request_id: String,
    messages: Vec<ChatMessage>,
    sampling: Option<Sampling>,
) -> Result<VoiceStart, String> {
    let system_prompt = load_assistant_prompts(&app).1.unwrap_or_else(|| DEFAULT_VOICE_PROMPT.to_string());
    let endpoint = state
        .endpoint()
        .ok_or_else(|| "The assistant model is not loaded. Load it and try again.".to_string())?;
    let model = assistant_catalogue::model_by_id(&endpoint.model_id)
        .ok_or_else(|| format!("Unknown assistant model \"{}\".", endpoint.model_id))?;
    let speech = Arc::new(start_assistant_speech(&app).await.map_err(|err| format!("{err:#}"))?);
    let job_id = speech.job_id().to_string();
    let cancelled_by_page = Arc::new(AtomicBool::new(false));
    if let Ok(mut map) = running().lock() {
        map.insert(request_id.clone(), RunningAnswer { speech: speech.clone(), cancelled_by_page: cancelled_by_page.clone() });
    }

    let (to_speech, from_model) = mpsc::channel();
    let sink = Arc::new(VoiceSink { app: app.clone(), guard: Mutex::new(Some(state.begin_use())), to_speech: Mutex::new(to_speech) });
    {
        let app = app.clone();
        let speech = speech.clone();
        let request_id = request_id.clone();
        let cancelled_by_page = cancelled_by_page.clone();
        std::thread::Builder::new()
            .name("assistant-voice".to_string())
            .spawn(move || speak_answer(&app, &request_id, &speech, &cancelled_by_page, from_model))
            .map_err(|err| format!("Could not start the voice answer: {err}"))?;
    }

    let target = ChatTarget {
        base_url: endpoint.base_url,
        api_key: endpoint.api_key,
        ctx_size: endpoint.ctx_size,
        image_token_estimate: model.image_token_estimate,
    };
    let request = ChatRequest { request_id: request_id.clone(), messages, think: false, system_prompt: Some(system_prompt), sampling };
    match assistant_chat::start_chat(target, request, sink.clone()).await {
        Ok(chat) => Ok(VoiceStart { chat, job_id }),
        Err(message) => {
            // Nothing was streamed: end the speech job and its worker.
            cancelled_by_page.store(true, Ordering::SeqCst);
            speech.cancel();
            sink.send(SpeechMessage::Stopped);
            Err(message)
        }
    }
}

/// Stops a voice answer: its model request and its speech. Unknown ids are a no-op.
#[tauri::command]
pub(crate) fn assistant_voice_cancel(app: AppHandle, request_id: String) {
    if let Some(answer) = running().lock().ok().and_then(|map| {
        map.get(&request_id).map(|answer| (answer.speech.clone(), answer.cancelled_by_page.clone()))
    }) {
        answer.1.store(true, Ordering::SeqCst);
        answer.0.cancel();
    } else if let Some(job_id) = finished_answers()
        .lock()
        .ok()
        .and_then(|finished| finished.iter().find(|(request, _)| *request == request_id).map(|(_, job)| job.clone()))
    {
        // Synthesis is done but its audio may still be queued: tell the player to drop it.
        // The player ignores this unless that job is the one playing.
        let _ = app.emit_all("voicereader:ws-event", serde_json::json!({"type": "JOB_CANCELED", "job_id": job_id}));
    }
    assistant_chat::cancel_chat(&request_id);
}

/// The speech worker for one answer: phrases in, audio out, in order.
fn speak_answer(
    app: &AppHandle,
    request_id: &str,
    speech: &Arc<AssistantSpeechJob>,
    cancelled_by_page: &AtomicBool,
    from_model: mpsc::Receiver<SpeechMessage>,
) {
    let started = Instant::now();
    let now_ms = || started.elapsed().as_millis() as u64;
    let mut buffer = PhraseBuffer::new(PhraseConfig::default());
    let mut text_done = false;
    let mut reason = "done";
    let mut error: Option<String> = None;
    'answer: loop {
        let phrases = match from_model.recv_timeout(TICK) {
            Ok(SpeechMessage::Delta(delta)) => buffer.push(&delta, now_ms()),
            Ok(SpeechMessage::Done) => {
                text_done = true;
                buffer.finish()
            }
            Ok(SpeechMessage::Stopped) | Err(RecvTimeoutError::Disconnected) => {
                reason = "cancelled";
                speech.cancel();
                break;
            }
            Err(RecvTimeoutError::Timeout) => buffer.tick(now_ms()),
        };
        for phrase in phrases {
            // Backpressure: wait while plenty of audio is already queued.
            while buffered_secs(speech.job_id()) > MAX_BUFFERED_SECS && !speech.is_cancelled() {
                std::thread::sleep(TICK);
            }
            if speech.is_cancelled() {
                break 'answer;
            }
            let _ = app.emit_all(
                "assistant:voice-phrase",
                VoicePhrasePayload { request_id, job_id: speech.job_id(), seq: phrase.seq, text: &phrase.text },
            );
            match speech.speak_phrase(phrase.seq, &phrase.text) {
                Ok(true) => {}
                Ok(false) => break 'answer,
                Err(err) => {
                    reason = "error";
                    error = Some(format!("Speaking the answer failed: {err:#}"));
                    speech.cancel();
                    break 'answer;
                }
            }
        }
        if text_done || speech.is_cancelled() {
            break;
        }
    }
    if cancelled_by_page.load(Ordering::SeqCst) {
        reason = "cancelled";
    } else if speech.is_cancelled() && reason == "done" {
        // Cancelled by something other than the page or a model error: read-aloud took over.
        reason = "preempted";
    }
    if reason != "done" {
        assistant_chat::cancel_chat(request_id);
    }
    speech.finish();
    let _ = app.emit_all(
        "assistant:voice-ended",
        VoiceEndedPayload { request_id, job_id: speech.job_id(), reason, message: error.as_deref() },
    );
    if let Ok(mut finished) = finished_answers().lock() {
        finished.push_back((request_id.to_string(), speech.job_id().to_string()));
        while finished.len() > FINISHED_ANSWERS_KEPT {
            finished.pop_front();
        }
    }
    if let Ok(mut map) = running().lock() {
        map.remove(request_id);
    }
    if let Ok(mut reports) = playback_reports().lock() {
        reports.remove(speech.job_id());
    }
}
