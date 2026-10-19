//! Voice conversation answers (plan §6, §8). The model's answer streams to the page as
//! text (the same `assistant:chat-*` events as the chat page) and, phrase by phrase, to the
//! speakers through one [`AssistantSpeechJob`]. Turn-taking, the microphone and history
//! stay in the page, which hears exactly what was played.
//!
//! Per answer:
//! - the Voice system prompt and optional thinking/tool loop;
//! - deltas go through a [`PhraseBuffer`] (sentences or clauses, cleaned for speech), and each
//!   phrase is announced as `assistant:voice-phrase` and then synthesised;
//! - synthesis waits while more than [`MAX_BUFFERED_SECS`] of audio is queued but unheard,
//!   as reported by the page (`assistant:playback`), so a fast model cannot build an
//!   unbounded backlog;
//! - a read-aloud request cancels the speech job (plan §6.2); the answer's model request is
//!   then cancelled too, and `assistant:voice-ended` says so.
//! See `docs/assistant/conversation.md`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager, State};

use crate::assistant_backend::{AssistantState, UseGuard};
use crate::assistant_catalogue;
use crate::assistant_chat::{self, ChatMessage, ChatRequest, ChatSink, ChatStart, ChatTarget, Sampling, DEFAULT_VOICE_PROMPT};
use crate::assistant_phrases::{PhraseBuffer, PhraseConfig};
use crate::assistant_tools::{self, ThinkingBudget, ToolLimits};
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
    kind: &'a str,
    round: u32,
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
    Delta { text: String, round: u32 },
    Event { name: String, payload: Value },
    Done,
    Stopped,
}

/// Ensures the page receives the model's terminal metadata before speech completion.
#[derive(Default)]
struct TerminalSignal { claimed: AtomicBool, ended: Mutex<bool>, changed: Condvar }
impl TerminalSignal {
    fn claim(&self) -> bool { !self.claimed.swap(true, Ordering::SeqCst) }
    fn finish(&self) {
        *self.ended.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.changed.notify_all();
    }
    fn wait(&self) {
        let mut ended = self.ended.lock().unwrap_or_else(|e| e.into_inner());
        while !*ended { ended = self.changed.wait(ended).unwrap_or_else(|e| e.into_inner()); }
    }
}

/// Forwards the answer's text to the page and to the speech worker.
struct VoiceSink {
    app: AppHandle,
    guard: Mutex<Option<UseGuard>>,
    to_speech: Mutex<Sender<SpeechMessage>>,
    round: AtomicU32,
    metadata: Mutex<Value>,
    terminal: Arc<TerminalSignal>,
    stopped: Arc<AtomicBool>,
    show_reasoning: bool,
}

/// Transcript and speech are separate outputs: reasoning can only enter the former.
fn voice_delta(request_id: &str, content: Option<&str>, reasoning: Option<&str>, round: u32, show_reasoning: bool)
    -> (Option<Value>, Option<SpeechMessage>) {
    let content = content.filter(|text| !text.is_empty());
    let reasoning = reasoning.filter(|text| show_reasoning && !text.is_empty());
    let event = if content.is_some() || reasoning.is_some() {
        Some(json!({"request_id":request_id,"content":content,"reasoning":reasoning,"round":round}))
    } else { None };
    let speech = content.map(|text| SpeechMessage::Delta { text: text.to_string(), round });
    (event, speech)
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
    fn delta(&self, request_id: &str, content: Option<&str>, reasoning: Option<&str>) {
        let round = self.round.load(Ordering::SeqCst);
        let (event, speech) = voice_delta(request_id, content, reasoning, round, self.show_reasoning);
        if let Some(payload) = event { let _ = self.app.emit_all("assistant:chat-delta", payload); }
        if let Some(message) = speech { self.send(message); }
    }

    fn done(&self, request_id: &str, finish_reason: Option<&str>, stopped: bool) {
        if !self.terminal.claim() { return; }
        if stopped { self.stopped.store(true, Ordering::SeqCst); }
        let metadata = self.metadata.lock().unwrap_or_else(|e| e.into_inner());
        let mut payload = metadata.as_object().cloned().unwrap_or_default();
        payload.insert("request_id".into(), json!(request_id));
        payload.insert("finish_reason".into(), json!(finish_reason));
        payload.insert("stopped".into(), json!(stopped));
        let _ = self.app.emit_all("assistant:chat-done", Value::Object(payload));
        drop(metadata);
        self.terminal.finish();
        self.send(if stopped { SpeechMessage::Stopped } else { SpeechMessage::Done });
        self.release();
    }

    fn error(&self, request_id: &str, message: &str) {
        if !self.terminal.claim() { return; }
        self.stopped.store(true, Ordering::SeqCst);
        let mut payload = self.metadata.lock().unwrap_or_else(|e|e.into_inner()).as_object().cloned().unwrap_or_default();
        payload.insert("request_id".into(), json!(request_id));
        payload.insert("message".into(), json!(message));
        let _ = self.app.emit_all("assistant:chat-error", Value::Object(payload));
        self.terminal.finish();
        self.send(SpeechMessage::Stopped);
        self.release();
    }

    fn event(&self, request_id: &str, event: &str, mut payload: Value) {
        if let Some(round) = payload.get("round").and_then(Value::as_u64) {
            self.round.store(round as u32, Ordering::SeqCst);
        }
        if event == "assistant:turn-metadata" {
            *self.metadata.lock().unwrap_or_else(|e| e.into_inner()) = payload;
            return;
        }
        self.send(SpeechMessage::Event { name: event.to_string(), payload: payload.clone() });
        if event != "assistant:round-ended" {
            if let Some(object) = payload.as_object_mut() { object.insert("request_id".into(), json!(request_id)); }
            let _ = self.app.emit_all(event, payload);
        }
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
    conversation_id: Option<String>,
    tools: Option<bool>,
    enabled_tools: Option<Vec<String>>,
    think: Option<bool>,
    thinking_budget: Option<ThinkingBudget>,
    tool_limits: Option<ToolLimits>,
) -> Result<VoiceStart, String> {
    let mut options = assistant_tools::request_options(&app, &state, conversation_id, tools, enabled_tools, thinking_budget, tool_limits, true)?;
    let think = think.unwrap_or(false);
    let system_prompt = load_assistant_prompts(&app).1.unwrap_or_else(|| DEFAULT_VOICE_PROMPT.to_string());
    let (endpoint, use_guard, generation) = state.admit_turn().map_err(|error|format!("{error:#}"))?;
    options.generation = generation;
    options.model_id = endpoint.model_id.clone();
    let model = assistant_catalogue::model_by_id(&endpoint.model_id)
        .ok_or_else(|| format!("Unknown assistant model \"{}\".", endpoint.model_id))?;
    let speech = Arc::new(start_assistant_speech(&app).await.map_err(|err| format!("{err:#}"))?);
    let job_id = speech.job_id().to_string();
    let cancelled_by_page = Arc::new(AtomicBool::new(false));
    if let Ok(mut map) = running().lock() {
        map.insert(request_id.clone(), RunningAnswer { speech: speech.clone(), cancelled_by_page: cancelled_by_page.clone() });
    }

    let (to_speech, from_model) = mpsc::channel();
    let terminal = Arc::new(TerminalSignal::default());
    let stopped = Arc::new(AtomicBool::new(false));
    let sink = Arc::new(VoiceSink { app: app.clone(), guard: Mutex::new(Some(use_guard)), to_speech: Mutex::new(to_speech),
        round: AtomicU32::new(0), metadata: Mutex::new(json!({})), terminal: terminal.clone(), stopped: stopped.clone(), show_reasoning: think });
    {
        let worker_app = app.clone();
        let worker_speech = speech.clone();
        let worker_request_id = request_id.clone();
        let worker_cancelled = cancelled_by_page.clone();
        let started = std::thread::Builder::new()
            .name("assistant-voice".to_string())
            .spawn(move || speak_answer(&worker_app, &worker_request_id, &worker_speech, &worker_cancelled, from_model, think, &terminal, &stopped));
        if let Err(error) = started {
            // No worker exists to clean up the speech lease or running-answer entry.
            sink.release();
            running().lock().ok().map(|mut map| map.remove(&request_id));
            speech.cancel();
            speech.finish();
            return Err(format!("Could not start the voice answer: {error}"));
        }
    }

    let target = ChatTarget {
        base_url: endpoint.base_url,
        api_key: endpoint.api_key,
        ctx_size: endpoint.ctx_size,
        image_token_estimate: model.image_token_estimate,
    };
    let request = ChatRequest { request_id: request_id.clone(), messages, think, system_prompt: Some(system_prompt), sampling };
    match assistant_chat::start_chat_with_options(target, request, sink.clone(), options).await {
        Ok(chat) => Ok(VoiceStart { chat, job_id }),
        Err(message) => {
            // Nothing was streamed: end the speech job and its worker.
            cancelled_by_page.store(true, Ordering::SeqCst);
            speech.cancel();
            sink.error(&request_id, &message);
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

/// Ends an answer's speech job and waits for the model turn's terminal event, in the order
/// that keeps Stop immediate: cancelled audio is stopped before waiting, completed audio
/// only after (its job-done must follow every phrase and the turn's metadata).
fn end_speech(cancelled: bool, finish: impl FnOnce(), wait_for_terminal: impl FnOnce()) {
    if cancelled {
        finish();
        wait_for_terminal();
    } else {
        wait_for_terminal();
        finish();
    }
}

/// The speech worker for one answer: phrases in, audio out, in order.
fn speak_answer(
    app: &AppHandle,
    request_id: &str,
    speech: &Arc<AssistantSpeechJob>,
    cancelled_by_page: &AtomicBool,
    from_model: mpsc::Receiver<SpeechMessage>,
    think: bool,
    terminal: &TerminalSignal,
    stopped: &AtomicBool,
) {
    let started = Instant::now();
    let now_ms = || started.elapsed().as_millis() as u64;
    let mut state = SpeechState::new(think);
    let mut text_done = false;
    let mut reason = "done";
    let mut error: Option<String> = None;
    'answer: loop {
        if stopped.load(Ordering::SeqCst) { reason = "cancelled"; speech.cancel(); break; }
        if speech.is_cancelled() { break; }
        let mut phrases = match from_model.recv_timeout(TICK) {
            Ok(SpeechMessage::Delta { text, round }) => state.delta(&text, round, now_ms()),
            Ok(SpeechMessage::Event { name, payload }) => state.event(&name, &payload, now_ms()),
            Ok(SpeechMessage::Done) => {
                text_done = true;
                state.finish()
            }
            Ok(SpeechMessage::Stopped) | Err(RecvTimeoutError::Disconnected) => {
                reason = "cancelled";
                speech.cancel();
                break;
            }
            Err(RecvTimeoutError::Timeout) => state.tick(now_ms()),
        };
        phrases.extend(state.tick_if_due(now_ms()));
        for phrase in phrases {
            // Backpressure: wait while plenty of audio is already queued.
            while buffered_secs(speech.job_id()) > MAX_BUFFERED_SECS && !speech.is_cancelled() && !stopped.load(Ordering::SeqCst) {
                std::thread::sleep(TICK);
            }
            if stopped.load(Ordering::SeqCst) { reason = "cancelled"; speech.cancel(); break 'answer; }
            if speech.is_cancelled() {
                break 'answer;
            }
            let _ = app.emit_all(
                "assistant:voice-phrase",
                VoicePhrasePayload { request_id, job_id: speech.job_id(), seq: phrase.seq, text: &phrase.text, kind: phrase.kind, round: phrase.round },
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
    // A cancelled tool may still be draining. Its result and chat terminal metadata must
    // reach the page before voice-ended lets the microphone resume. A cancelled answer's
    // audio is stopped first, though, so Stop silences queued speech at once instead of after
    // the tool has wound down.
    end_speech(speech.is_cancelled(), || speech.finish(), || terminal.wait());
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

#[derive(Default)]
struct PendingCall { name: String, arguments: String, filler_sent: bool }

/// Pure timing/selection policy. The speech thread is the only caller; callbacks never do TTS.
struct FillerPolicy {
    think: bool,
    round: u32,
    first_round: Option<u32>,
    phase: String,
    content_seen: bool,
    next_timer_ms: u64,
    timer_count: u32,
    calls: HashMap<u32, PendingCall>,
}

impl FillerPolicy {
    fn new(think: bool) -> Self {
        Self { think, round: 0, first_round: None, phase: if think { "thinking" } else { "answering" }.into(),
            content_seen: false, next_timer_ms: 1200, timer_count: 0, calls: HashMap::new() }
    }
    fn set_round(&mut self, round: u32, now: u64) {
        self.first_round.get_or_insert(round);
        if self.round != round {
            self.round = round;
            self.content_seen = false;
            self.calls.clear();
            self.timer_count = 0;
            self.next_timer_ms = now + 1200;
        }
    }
    fn phase(&mut self, phase: &str, now: u64) {
        if self.phase != phase {
            self.phase = phase.to_string();
            self.timer_count = 0;
            self.next_timer_ms = now + if phase == "thinking" { 1200 } else { 6000 };
        }
    }
    fn tick(&mut self, now: u64) -> Option<String> {
        if now < self.next_timer_ms { return None; }
        let text = match self.phase.as_str() {
            "thinking" if self.think && !self.content_seen => {
                if self.timer_count == 0 { "Give me a moment." }
                else if self.timer_count % 2 == 1 { "Still thinking." } else { "Nearly there." }
            }
            "tool" => if self.timer_count % 2 == 0 { "Still working on it." } else { "Just a moment longer." },
            _ => return None,
        };
        self.timer_count += 1;
        self.next_timer_ms = now + 6000;
        Some(text.to_string())
    }
    fn call(&mut self, payload: &Value) -> Option<String> {
        let index = payload.get("call_index").and_then(Value::as_u64).unwrap_or(0) as u32;
        let first_round = self.first_round == Some(self.round);
        let call = self.calls.entry(index).or_default();
        if let Some(name) = payload.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()) { call.name = name.into(); }
        if let Some(arguments) = payload.get("arguments").and_then(Value::as_str) { call.arguments = arguments.into(); }
        else if let Some(delta) = payload.get("delta").and_then(Value::as_str) { call.arguments.push_str(delta); }
        if call.filler_sent || self.content_seen { return None; }
        let text = match call.name.as_str() {
            "web_search" if !first_round => "Searching the web.".to_string(),
            "web_search" => format!("Searching for {}.", spoken_argument(&call.arguments, "query")?),
            // After the first round web content may be in context, so model-chosen text is
            // never spoken in the app's own filler voice (plan §15.2).
            "get_weather" if !first_round => "Checking the weather.".to_string(),
            "get_weather" => format!("Checking the weather in {}.", spoken_argument(&call.arguments, "city")?),
            "fetch_page" => "Opening that page.".to_string(),
            "run_python" => "Let me work that out.".to_string(),
            _ => return None,
        };
        call.filler_sent = true;
        Some(text)
    }
}

/// Read a completed first JSON string field even while the closing object is streaming.
/// Never speak incomplete escapes, code, or raw model-generated markup.
fn spoken_argument(arguments: &str, key: &str) -> Option<String> {
    fn string_end(text: &str) -> Option<usize> {
        if !text.starts_with('"') { return None; }
        let mut escape = false;
        for (index, ch) in text.char_indices().skip(1) {
            if escape { escape = false; }
            else if ch == '\\' { escape = true; }
            else if ch == '"' { return Some(index + 1); }
        }
        None
    }
    let fields = arguments.trim_start().strip_prefix('{')?.trim_start();
    let key_end = string_end(fields)?;
    if serde_json::from_str::<String>(&fields[..key_end]).ok()? != key { return None; }
    let value = fields[key_end..].trim_start().strip_prefix(':')?.trim_start();
    let end = string_end(value)?;
    let value: String = serde_json::from_str(&value[..end]).ok()?;
    let words = value.split_whitespace().take(8).collect::<Vec<_>>().join(" ");
    let words: String = words.chars().filter(|c| !c.is_control() && !matches!(c, '<' | '>' | '`' | '*' | '#')).take(160).collect();
    if words.trim().is_empty() { None } else { Some(words) }
}

#[derive(Debug, PartialEq)]
struct SpokenPhrase { seq: u32, text: String, kind: &'static str, round: u32 }

struct SpeechState { buffer: PhraseBuffer, fillers: FillerPolicy, next_seq: u32, last_tick: u64 }
impl SpeechState {
    fn new(think: bool) -> Self { Self { buffer: PhraseBuffer::new(PhraseConfig::default()), fillers: FillerPolicy::new(think), next_seq: 0, last_tick: 0 } }
    fn phrases(&mut self, texts: impl IntoIterator<Item=String>, kind: &'static str) -> Vec<SpokenPhrase> {
        texts.into_iter().map(|text| {
            let seq = self.next_seq; self.next_seq += 1;
            SpokenPhrase { seq, text, kind, round: self.fillers.round }
        }).collect()
    }
    fn flush(&mut self) -> Vec<SpokenPhrase> {
        let phrases = self.buffer.finish();
        self.buffer = PhraseBuffer::new(PhraseConfig::default());
        self.phrases(phrases.into_iter().map(|p| p.text), "answer")
    }
    fn round(&mut self, round: u32, now: u64) -> Vec<SpokenPhrase> {
        let previous = if self.fillers.round != round { self.flush() } else { Vec::new() };
        self.fillers.set_round(round, now);
        previous
    }
    fn delta(&mut self, text: &str, round: u32, now: u64) -> Vec<SpokenPhrase> {
        let mut phrases = self.round(round, now);
        // Whitespace alone (a newline before a tool call) is not something the user heard.
        if !text.trim().is_empty() { self.fillers.content_seen = true; self.fillers.phase("answering", now); }
        let pending = self.buffer.push(text, now);
        phrases.extend(self.phrases(pending.into_iter().map(|p| p.text), "answer"));
        phrases
    }
    fn event(&mut self, name: &str, payload: &Value, now: u64) -> Vec<SpokenPhrase> {
        let round = payload.get("round").and_then(Value::as_u64).unwrap_or(self.fillers.round as u64) as u32;
        let mut phrases = self.round(round, now);
        match name {
            "assistant:turn-phase" => if let Some(phase) = payload.get("phase").and_then(Value::as_str) { self.fillers.phase(phase, now); },
            "assistant:round-ended" => {
                phrases.extend(self.flush());
                self.fillers.phase(if payload.get("has_tools").and_then(Value::as_bool).unwrap_or(false) { "tool" } else { "answering" }, now);
            }
            "assistant:tool-call" => {
                if let Some(text) = self.fillers.call(payload) { phrases.extend(self.phrases([text], "filler")); }
            }
            _ => {}
        }
        phrases
    }
    /// Runs `tick` at most once per TICK whatever else arrives. A model streaming reasoning
    /// sends an event every few tens of milliseconds, so the receive timeout alone would never
    /// fire and the thinking fillers would never play.
    fn tick_if_due(&mut self, now: u64) -> Vec<SpokenPhrase> {
        if now.saturating_sub(self.last_tick) < TICK.as_millis() as u64 { return Vec::new(); }
        self.tick(now)
    }
    fn tick(&mut self, now: u64) -> Vec<SpokenPhrase> {
        self.last_tick = now;
        let pending = self.buffer.tick(now);
        let mut phrases = self.phrases(pending.into_iter().map(|p| p.text), "answer");
        if let Some(text) = self.fillers.tick(now) { phrases.extend(self.phrases([text], "filler")); }
        phrases
    }
    fn finish(&mut self) -> Vec<SpokenPhrase> { self.fillers.phase("done", 0); self.flush() }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn voice_reasoning_reaches_transcript_but_never_the_speech_queue() {
        let (event, speech) = voice_delta("thinking-voice", None, Some("Private model reasoning."), 2, true);
        let event = event.unwrap();
        assert_eq!(event["reasoning"], "Private model reasoning.");
        assert!(event["content"].is_null());
        assert_eq!(event["round"], 2);
        assert!(speech.is_none());
        let (event, speech) = voice_delta("thinking-voice", Some("The answer."), Some("Do not speak this."), 2, true);
        assert_eq!(event.unwrap()["reasoning"], "Do not speak this.");
        match speech.unwrap() { SpeechMessage::Delta { text, round } => { assert_eq!(text, "The answer."); assert_eq!(round, 2); }, _ => panic!("Expected answer text only") }
        let (event, speech) = voice_delta("thinking-off", None, Some("Unexpected reasoning."), 0, false);
        assert!(event.is_none()); assert!(speech.is_none());
        let (event, speech) = voice_delta("empty", Some(""), Some(""), 0, true);
        assert!(event.is_none()); assert!(speech.is_none());
    }
    #[test]
    fn startup_error_and_registered_terminal_can_only_claim_one_emission() {
        let signal = TerminalSignal::default();
        assert!(signal.claim());
        assert!(!signal.claim());
        signal.finish();
        signal.wait();
    }
    #[test]
    fn thinking_fillers_play_while_reasoning_events_stream_without_pause() {
        // Reasoning arrives as an event every 30 ms, so the receive timeout never fires;
        // the loop's tick_if_due must still run the filler timer.
        let mut state = SpeechState::new(true);
        let mut spoken = Vec::new();
        state.event("assistant:turn-phase", &json!({"round":0,"phase":"thinking"}), 0);
        for now in (0..=1500).step_by(30) {
            spoken.extend(state.event("assistant:delta-round", &json!({"round":0}), now));
            spoken.extend(state.tick_if_due(now));
        }
        let fillers: Vec<_> = spoken.iter().filter(|p| p.kind == "filler").map(|p| p.text.as_str()).collect();
        assert_eq!(fillers, ["Give me a moment."]);
    }
    #[test]
    fn stop_silences_audio_before_waiting_for_a_draining_tool() {
        let order = std::cell::RefCell::new(Vec::new());
        end_speech(true, || order.borrow_mut().push("finish"), || order.borrow_mut().push("wait"));
        assert_eq!(*order.borrow(), ["finish", "wait"]);
        order.borrow_mut().clear();
        end_speech(false, || order.borrow_mut().push("finish"), || order.borrow_mut().push("wait"));
        assert_eq!(*order.borrow(), ["wait", "finish"]);
    }
    #[test]
    fn model_chosen_text_is_spoken_only_in_the_first_round() {
        let mut state = SpeechState::new(false);
        let first = state.event("assistant:tool-call", &json!({"round":0,"call_index":0,"name":"get_weather","delta":"{\"city\":\"Paris\"}"}), 10);
        assert_eq!(first[0].text, "Checking the weather in Paris.");
        state.event("assistant:round-ended", &json!({"round":0,"has_tools":true}), 20);
        let later = state.event("assistant:tool-call", &json!({"round":1,"call_index":0,"name":"get_weather","delta":"{\"city\":\"Your account is locked call now\"}"}), 30);
        assert_eq!(later[0].text, "Checking the weather.");
        let search = state.event("assistant:tool-call", &json!({"round":1,"call_index":1,"name":"web_search","delta":"{\"query\":\"injected words\"}"}), 40);
        assert_eq!(search[0].text, "Searching the web.");
    }
    #[test]
    fn whitespace_before_a_tool_call_does_not_suppress_its_filler() {
        let mut state = SpeechState::new(false);
        assert!(state.delta("\n\n", 0, 10).is_empty());
        let phrases = state.event("assistant:tool-call", &json!({"round":0,"call_index":0,"name":"run_python","delta":"{"}), 20);
        assert_eq!(phrases.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), ["Let me work that out."]);
    }
    #[test]
    fn thinking_fillers_start_at_1200ms_repeat_at_6s_and_stop_on_content() {
        let mut state = SpeechState::new(true);
        state.event("assistant:turn-phase", &json!({"round":1,"phase":"thinking"}), 0);
        assert!(state.tick(1199).is_empty());
        let first = state.tick(1200); assert_eq!(first[0].text, "Give me a moment.");
        assert_eq!(first[0].kind, "filler");
        assert!(state.tick(7199).is_empty());
        assert_eq!(state.tick(7200)[0].text, "Still thinking.");
        state.delta("The answer is ready.", 1, 7300);
        assert!(state.tick(20000).iter().all(|p| p.kind != "filler"));
        assert!(SpeechState::new(false).tick(20000).is_empty());
    }
    #[test]
    fn streamed_arguments_wait_for_complete_string_and_only_fill_once() {
        let mut state = SpeechState::new(false);
        let call = |delta| json!({"round":1,"call_index":0,"name":"web_search","delta":delta});
        assert!(state.event("assistant:tool-call", &call("{\"query\":\"apple"), 0).is_empty());
        let out = state.event("assistant:tool-call", &call(" stock price\""), 100);
        assert_eq!(out[0].text, "Searching for apple stock price.");
        assert!(state.event("assistant:tool-call", &call("}"), 200).is_empty());
        assert!(state.event("assistant:tool-call", &json!({"round":1,"call_index":0,"name":"web_search","arguments":"{\"query\":\"apple stock price\"}","phase":"running"}), 300).is_empty());
    }
    #[test]
    fn later_search_never_speaks_query_and_preamble_suppresses_call_filler() {
        let mut state = SpeechState::new(false);
        state.event("assistant:turn-phase", &json!({"round":1,"phase":"answering"}), 0);
        state.delta("I will look it up.", 1, 10);
        assert!(state.event("assistant:tool-call", &json!({"round":1,"call_index":0,"name":"fetch_page"}), 20).is_empty());
        state.event("assistant:round-ended", &json!({"round":1,"has_tools":true}), 30);
        state.event("assistant:turn-phase", &json!({"round":2,"phase":"thinking"}), 40);
        let out = state.event("assistant:tool-call", &json!({"round":2,"call_index":0,"name":"web_search","delta":"{\"query\":\"injected page instructions\"}"}), 50);
        assert_eq!(out[0].text, "Searching the web.");
    }
    #[test]
    fn one_job_sequence_spans_fillers_round_flushes_and_final_answer() {
        let mut state = SpeechState::new(false);
        let mut out = state.event("assistant:tool-call", &json!({"round":0,"call_index":0,"name":"run_python"}), 0);
        state.delta("A preamble without punctuation", 0, 100);
        out.extend(state.event("assistant:round-ended", &json!({"round":0,"has_tools":true}), 200));
        out.extend(state.event("assistant:turn-phase", &json!({"round":1,"phase":"answering"}), 300));
        out.extend(state.delta("Forty two.", 1, 400));
        out.extend(state.finish());
        assert_eq!(out.iter().map(|p|p.seq).collect::<Vec<_>>(), vec![0,1,2]);
        assert_eq!(out.iter().map(|p|(p.kind,p.round)).collect::<Vec<_>>(), vec![("filler",0),("answer",0),("answer",1)]);
        assert_eq!(out[1].text, "A preamble without punctuation");
        assert_eq!(out[2].text, "Forty two.");
    }
    #[test]
    fn tool_wait_has_six_second_timer_and_answering_cancels_it() {
        let mut state = SpeechState::new(false);
        state.event("assistant:turn-phase", &json!({"round":1,"phase":"tool"}), 100);
        assert!(state.tick(6099).is_empty());
        assert_eq!(state.tick(6100)[0].text, "Still working on it.");
        state.delta("Done.", 1, 6200);
        assert!(state.tick(20000).iter().all(|p|p.kind != "filler"));
    }
    #[test]
    fn spoken_argument_handles_escapes_and_trims_query() {
        assert_eq!(spoken_argument(r#"{"query":"a \"quote\" and \u263a""#, "query"), Some("a \"quote\" and ☺".into()));
        assert_eq!(spoken_argument(r#"{"query":"one two three four five six seven eight nine ten"}"#, "query"), Some("one two three four five six seven eight".into()));
        assert!(spoken_argument(r#"{"query":"unfinished\u26"#, "query").is_none());
        assert!(spoken_argument(r#"{"code":"print('secret')"}"#, "query").is_none());
    }
}
