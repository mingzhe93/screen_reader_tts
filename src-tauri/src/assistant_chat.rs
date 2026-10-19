//! Streaming chat against a running llama-server (OpenAI-compatible API).
//!
//! The caller supplies the server target and a `ChatSink`. Validation, loopback HTTP
//! helpers and the whole-turn registry live here; `assistant_tool_loop` owns actual
//! model/tool rounds and context fitting. The old single-completion path is retained
//! only as a regression-test fixture. See `docs/assistant/chat.md` and `tools.md`.

use std::collections::{HashMap, VecDeque};
#[cfg(test)]
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use anyhow::Context;
use futures_util::future::AbortHandle;
#[cfg(test)]
use futures_util::future::Abortable;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The chat page's default system message; the user can replace it in the settings panel.
pub(crate) const DEFAULT_CHAT_PROMPT: &str = "You are a helpful assistant inside the VoiceReader desktop app. \
Answer accurately and concisely. Use Markdown where it helps reading. If you are not sure, say so.";
/// Voice conversation's default system message (plan §8.1): answers are spoken aloud.
pub(crate) const DEFAULT_VOICE_PROMPT: &str = "You are VoiceReader's voice assistant, and your replies are spoken aloud. \
Answer in short, natural sentences, usually one to three. Do not use Markdown, lists, tables, code or emoji. \
Say numbers, symbols and abbreviations the way you would read them out. Write decimal numbers with the word point instead of a decimal dot, for example 28 point 8 degrees Celsius, not 28.8 degrees Celsius. If you are not sure, say so briefly.";
/// Longest system prompt accepted, in characters.
pub(crate) const MAX_SYSTEM_PROMPT_CHARS: usize = 8000;
/// Answer room kept free in the context window, before the quarter-of-context cap.
#[cfg(test)]
const RESERVE_THINK_TOKENS: u32 = 8192;
#[cfg(test)]
const RESERVE_NO_THINK_TOKENS: u32 = 2048;
const MAX_IMAGES_PER_MESSAGE: usize = 4;
const IMAGE_DATA_URL_PREFIXES: [&str; 3] = [
    "data:image/png;base64,",
    "data:image/jpeg;base64,",
    "data:image/webp;base64,",
];
const UNEXPECTED_STOP: &str = "The assistant stopped unexpectedly.";

/// Where and how to reach the server. Built by the caller from the backend's endpoint.
/// Deliberately has no `Debug` so the API key cannot end up in a log line.
pub(crate) struct ChatTarget {
    pub base_url: String,
    /// Sent as `Authorization: Bearer <key>`.
    pub api_key: String,
    /// The server's `-c` value.
    pub ctx_size: u32,
    /// Tokens counted per attached image.
    pub image_token_estimate: u32,
}

/// One message from the frontend. Reasoning is never sent back (plan section 5.4).
#[derive(Deserialize, Clone)]
pub(crate) struct ChatMessage {
    /// "user" or "assistant"; any other role is rejected.
    pub role: String,
    pub text: String,
    /// Data URLs (`data:image/png;base64,...`) from the frontend.
    #[serde(default)]
    pub images: Vec<String>,
}

pub(crate) struct ChatRequest {
    pub request_id: String,
    /// Oldest first; the last one must have role "user".
    pub messages: Vec<ChatMessage>,
    pub think: bool,
    /// `None` uses [`DEFAULT_CHAT_PROMPT`].
    pub system_prompt: Option<String>,
    /// `None` leaves sampling to llama-server's defaults.
    pub sampling: Option<Sampling>,
}

/// Sampling settings for one request, from the settings panel.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub(crate) struct Sampling {
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: u32,
    pub min_p: f32,
    pub repeat_penalty: f32,
    /// Longest answer in tokens; `None` = as long as the context allows.
    pub max_tokens: Option<u32>,
}

impl Sampling {
    /// Refuses out-of-range values rather than sending them to the server.
    pub(crate) fn validate(&self) -> Result<(), String> {
        let ok = (0.0..=2.0).contains(&self.temperature)
            && (0.0..=1.0).contains(&self.top_p)
            && self.top_k <= 500
            && (0.0..=1.0).contains(&self.min_p)
            && (0.5..=2.0).contains(&self.repeat_penalty)
            && self.max_tokens.map_or(true, |tokens| (16..=262_144).contains(&tokens));
        if ok { Ok(()) } else { Err("A sampling setting is out of range. Reset sampling to the recommended values.".to_string()) }
    }
}

/// What the caller learns before streaming starts.
#[derive(Serialize)]
pub(crate) struct ChatStart {
    pub request_id: String,
    pub prompt_tokens: u32,
    /// How many older messages were evicted to fit.
    pub dropped_messages: u32,
}

/// Receives streamed output. Exactly one of `done` or `error` is called per request and
/// nothing is called after it. Calls are serialised under a lock, so an implementation
/// must not call [`cancel_chat`] or [`start_chat`] from inside these methods.
pub(crate) trait ChatSink: Send + Sync + 'static {
    /// App-owned lifecycle/tool metadata, serialised with deltas and terminal output.
    fn event(&self, _request_id: &str, _event: &str, _payload: Value) {}
    fn delta(&self, request_id: &str, content: Option<&str>, reasoning: Option<&str>);
    fn done(&self, request_id: &str, finish_reason: Option<&str>, stopped: bool);
    fn error(&self, request_id: &str, message: &str);
}

// ---------------------------------------------------------------------------------------
// Request registry and cancellation
// ---------------------------------------------------------------------------------------

pub(crate) struct SlotState {
    pub(crate) finished: bool,
    abort: Option<AbortHandle>,
}

/// One in-flight request. All sink output goes through the slot so that the "exactly one
/// terminal event, nothing after it" rule holds even when `cancel_chat` races the stream.
pub(crate) struct Slot {
    pub(crate) request_id: String,
    sink: Arc<dyn ChatSink>,
    backend_root: Option<PathBuf>,
    cancel: Option<Arc<AtomicBool>>,
    state: Mutex<SlotState>,
}

impl Slot {
    pub(crate) fn lock(&self) -> MutexGuard<'_, SlotState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn delta(&self, content: Option<&str>, reasoning: Option<&str>) {
        let state = self.lock();
        if !state.finished {
            self.sink.delta(&self.request_id, content, reasoning);
        }
    }

    pub(crate) fn event(&self, event: &str, payload: Value) {
        let state = self.lock();
        if !state.finished { self.sink.event(&self.request_id, event, payload); }
    }

    pub(crate) fn finish_turn(&self, metadata: Value, finish_reason: Option<&str>, stopped: bool, error: Option<&str>) {
        let mut state = self.lock();
        if !state.finished {
            // Terminal ownership is claimed here. Stop observed before this point wins;
            // a later Stop belongs to already-completed playback, not model generation.
            let stopped = stopped || self.cancel.as_ref().is_some_and(|flag|flag.load(Ordering::SeqCst));
            self.sink.event(&self.request_id, "assistant:turn-metadata", metadata);
            state.finished = true;
            if let Some(message) = error.filter(|_|!stopped) { self.sink.error(&self.request_id, message); }
            else { self.sink.done(&self.request_id, finish_reason, stopped); }
        }
        drop(state);
        unregister(self);
    }

    /// Emits `done` unless a terminal event was already sent.
    #[cfg(test)]
    fn finish_done(&self, finish_reason: Option<&str>) {
        let mut state = self.lock();
        if !state.finished {
            state.finished = true;
            self.sink.done(&self.request_id, finish_reason, false);
        }
        drop(state);
        unregister(self);
    }

    /// Emits `error` unless a terminal event was already sent.
    #[cfg(test)]
    fn finish_error(&self, message: &str) {
        let mut state = self.lock();
        if !state.finished {
            state.finished = true;
            self.sink.error(&self.request_id, message);
        }
        drop(state);
        unregister(self);
    }

    /// Marks the request finished without emitting anything. Returns false when a
    /// terminal event had already been sent (a cancel won the race).
    #[cfg(test)]
    fn abandon(&self) -> bool {
        let mut state = self.lock();
        let claimed = !state.finished;
        state.finished = true;
        drop(state);
        unregister(self);
        claimed
    }
}

/// How many cancels for not-yet-registered ids are remembered.
const MAX_EARLY_CANCELS: usize = 64;

#[derive(Default)]
struct Registry {
    running: HashMap<String, Arc<Slot>>,
    /// Ids cancelled before `start_chat` registered them. Tauri runs the async start command on
    /// a spawned task but the sync cancel command directly, so Stop can arrive first.
    cancelled_early: VecDeque<String>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

/// Registers a request, or returns `Ok(None)` when it was already cancelled before it got here.
#[cfg(test)]
fn register(request_id: &str, sink: Arc<dyn ChatSink>) -> Result<Option<Arc<Slot>>, String> {
    register_turn(request_id, sink, None, None)
}
pub(crate) fn register_turn(request_id: &str, sink: Arc<dyn ChatSink>, cancel: Option<Arc<AtomicBool>>, backend_root: Option<PathBuf>) -> Result<Option<Arc<Slot>>, String> {
    let mut registry = registry().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(index) = registry.cancelled_early.iter().position(|id| id == request_id) {
        registry.cancelled_early.remove(index);
        return Ok(None);
    }
    if registry.running.contains_key(request_id) {
        return Err("A request with this id is already running.".to_string());
    }
    let slot = Arc::new(Slot {
        request_id: request_id.to_string(),
        sink,
        backend_root,
        cancel,
        state: Mutex::new(SlotState { finished: false, abort: None }),
    });
    registry.running.insert(request_id.to_string(), slot.clone());
    Ok(Some(slot))
}

/// Removes the slot's registry entry, unless a newer request reused the id.
fn unregister(slot: &Slot) {
    let mut registry = registry().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if registry.running.get(&slot.request_id).map(|entry| std::ptr::eq(Arc::as_ptr(entry), slot)).unwrap_or(false) {
        registry.running.remove(&slot.request_id);
    }
}

/// Stops a running request: drops the HTTP response (llama-server stops generating when the
/// client disconnects) and calls `sink.done(id, None, true)` exactly once. An id that is not
/// registered yet is remembered, so a `start_chat` arriving later for it starts nothing.
pub(crate) fn cancel_chat(request_id: &str) {
    let slot = {
        let mut registry = registry().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match registry.running.get(request_id).cloned() {
            Some(slot) => slot,
            None => {
                if !registry.cancelled_early.iter().any(|id| id == request_id) {
                    if registry.cancelled_early.len() == MAX_EARLY_CANCELS {
                        registry.cancelled_early.pop_front();
                    }
                    registry.cancelled_early.push_back(request_id.to_string());
                }
                return;
            }
        }
    };
    if let Some(cancel) = &slot.cancel { cancel.store(true, Ordering::SeqCst); return; }
    let abort = {
        let mut state = slot.lock();
        if state.finished {
            return;
        }
        state.finished = true;
        slot.sink.done(&slot.request_id, None, true);
        state.abort.take()
    };
    if let Some(handle) = abort {
        handle.abort();
    }
    unregister(&slot);
}

/// Backend invalidation holds backend locks: this hook only signals, never calls a sink.
pub(crate) fn cancel_backend_turns(root: &Path) {
    let registry = registry().lock().unwrap_or_else(|e| e.into_inner());
    for slot in registry.running.values().filter(|slot| slot.backend_root.as_deref() == Some(root)) {
        if let Some(cancel) = &slot.cancel { cancel.store(true, Ordering::SeqCst); }
    }
}
/// Called outside backend locks, after cancellation was signalled.
pub(crate) fn wait_for_backend_turns(root: &Path, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !registry().lock().unwrap_or_else(|e| e.into_inner()).running.values().any(|slot| slot.backend_root.as_deref() == Some(root)) { return true; }
        if std::time::Instant::now() >= deadline { return false; }
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub(crate) async fn start_chat_with_options(target: ChatTarget, request: ChatRequest, sink: Arc<dyn ChatSink>, options: crate::assistant_tools::TurnOptions) -> Result<ChatStart, String> {
    crate::assistant_tool_loop::start(target, request, sink, options).await
}

// ---------------------------------------------------------------------------------------
// Validation and wire format
// ---------------------------------------------------------------------------------------

fn is_valid_image_url(url: &str) -> bool {
    IMAGE_DATA_URL_PREFIXES.iter().any(|prefix| {
        url.strip_prefix(prefix)
            .map(|payload| {
                !payload.is_empty()
                    && payload.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'-' | b'_'))
            })
            .unwrap_or(false)
    })
}

/// Checks a request and returns the messages to use. Messages with neither text nor
/// images are dropped (an empty final message is an error).
pub(crate) fn validate_request(request: &ChatRequest) -> Result<Vec<ChatMessage>, String> {
    if request.request_id.trim().is_empty() {
        return Err("The request has no id.".to_string());
    }
    let Some(last) = request.messages.last() else {
        return Err("There is no message to send.".to_string());
    };
    if last.role != "user" {
        return Err("The last message must be from the user.".to_string());
    }
    let mut cleaned = Vec::with_capacity(request.messages.len());
    for (index, message) in request.messages.iter().enumerate() {
        if message.role != "user" && message.role != "assistant" {
            return Err(format!("Unsupported message role \"{}\".", message.role));
        }
        if message.images.len() > MAX_IMAGES_PER_MESSAGE {
            return Err(format!("A message can have at most {MAX_IMAGES_PER_MESSAGE} images."));
        }
        if !message.images.is_empty() && message.role != "user" {
            return Err("Only user messages can carry images.".to_string());
        }
        if message.images.iter().any(|url| !is_valid_image_url(url)) {
            return Err("Only PNG, JPEG or WebP images can be attached.".to_string());
        }
        let is_final = index + 1 == request.messages.len();
        if message.text.trim().is_empty() && message.images.is_empty() {
            if is_final {
                return Err("Type a message before sending.".to_string());
            }
            continue;
        }
        cleaned.push(message.clone());
    }
    Ok(cleaned)
}

pub(crate) fn wire_message(message: &ChatMessage) -> Value {
    if message.images.is_empty() {
        return json!({"role": message.role, "content": message.text});
    }
    let mut parts = vec![json!({"type": "text", "text": message.text})];
    parts.extend(
        message
            .images
            .iter()
            .map(|url| json!({"type": "image_url", "image_url": {"url": url}})),
    );
    json!({"role": message.role, "content": parts})
}

/// The `/v1/chat/completions` body. `messages` already includes the system message.
#[cfg(test)]
fn build_request_body(messages: &[ChatMessage], think: bool, max_tokens: u32, sampling: Option<&Sampling>) -> Value {
    let mut body = json!({
        "model": "local",
        "messages": messages.iter().map(wire_message).collect::<Vec<_>>(),
        "stream": true,
        "chat_template_kwargs": {"enable_thinking": think},
        "max_tokens": max_tokens,
    });
    if !think {
        body["reasoning_effort"] = json!("none");
    }
    if let Some(sampling) = sampling {
        body["temperature"] = json!(sampling.temperature);
        body["top_p"] = json!(sampling.top_p);
        body["top_k"] = json!(sampling.top_k);
        body["min_p"] = json!(sampling.min_p);
        body["repeat_penalty"] = json!(sampling.repeat_penalty);
    }
    body
}

// ---------------------------------------------------------------------------------------
// Server-Sent Events
// ---------------------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
#[cfg(test)]
enum StreamEvent {
    Delta {
        content: Option<String>,
        reasoning: Option<String>,
        finish_reason: Option<String>,
    },
    Error(String),
    Done,
}

fn error_message(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| "The assistant server reported an error.".to_string())
}

#[cfg(test)]
fn parse_event_data(data: &str) -> StreamEvent {
    if data.trim() == "[DONE]" {
        return StreamEvent::Done;
    }
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return StreamEvent::Error("The assistant sent a response that could not be read.".to_string());
    };
    if let Some(error) = value.get("error") {
        return StreamEvent::Error(error_message(error));
    }
    let choice = value.pointer("/choices/0");
    let text = |pointer: &str| {
        choice
            .and_then(|choice| choice.pointer(pointer))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    StreamEvent::Delta {
        content: text("/delta/content"),
        reasoning: text("/delta/reasoning_content"),
        finish_reason: text("/finish_reason"),
    }
}

/// Incremental Server-Sent Events parser. Bytes are buffered until a full line arrives, so
/// JSON split across HTTP chunks (and multi-byte characters split mid-sequence) are safe.
#[derive(Default)]
#[cfg(test)]
struct SseParser {
    buffer: Vec<u8>,
    data_lines: Vec<String>,
}

#[cfg(test)]
impl SseParser {
    fn push(&mut self, chunk: &[u8]) -> Vec<StreamEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.buffer.drain(..=newline).collect();
            let line = String::from_utf8_lossy(&raw[..raw.len() - 1]);
            let line = line.strip_suffix('\r').unwrap_or(&line);
            if line.is_empty() {
                if !self.data_lines.is_empty() {
                    let data = self.data_lines.join("\n");
                    self.data_lines.clear();
                    events.push(parse_event_data(&data));
                }
            } else if line.starts_with(':') {
                // Comment or keep-alive.
            } else if let Some(rest) = line.strip_prefix("data:") {
                self.data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            }
            // `event:`, `id:` and `retry:` fields carry nothing llama-server needs.
        }
        events
    }
}

// ---------------------------------------------------------------------------------------
// Token counting and context fitting
// ---------------------------------------------------------------------------------------

/// Counts the tokens of the text-only prompt the server would build. Faked in tests.
#[cfg(test)]
trait TokenCounter {
    fn count_text_tokens<'a>(
        &'a self,
        messages: &'a [ChatMessage],
    ) -> impl Future<Output = Result<u32, String>> + Send + 'a;
}

#[cfg(test)]
struct HttpCounter<'a> {
    client: &'a Client,
    target: &'a ChatTarget,
}

#[cfg(test)]
impl TokenCounter for HttpCounter<'_> {
    async fn count_text_tokens(&self, messages: &[ChatMessage]) -> Result<u32, String> {
        let text_only: Vec<Value> = messages
            .iter()
            .map(|message| json!({"role": message.role, "content": message.text}))
            .collect();
        let template = post_json(self.client, self.target, "/apply-template", &json!({"messages": text_only})).await?;
        let prompt = template
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| "The assistant server did not return a prompt to count.".to_string())?;
        let tokens = post_json(self.client, self.target, "/tokenize", &json!({"content": prompt})).await?;
        let count = tokens
            .get("tokens")
            .and_then(Value::as_array)
            .map(Vec::len)
            .ok_or_else(|| "The assistant server did not return a token count.".to_string())?;
        Ok(count as u32)
    }
}

#[cfg(test)]
fn reserve_tokens(ctx_size: u32, think: bool) -> u32 {
    let wanted = if think { RESERVE_THINK_TOKENS } else { RESERVE_NO_THINK_TOKENS };
    wanted.min(ctx_size / 4)
}

#[cfg(test)]
struct Fitted {
    /// System message first, final user message last.
    messages: Vec<ChatMessage>,
    prompt_tokens: u32,
    dropped_messages: u32,
}

/// Evicts the oldest messages (a user message together with the assistant reply after it)
/// until the prompt plus the answer reserve fits in `ctx_size`. The system message and the
/// final user message are never dropped.
#[cfg(test)]
async fn fit_to_context<C: TokenCounter>(
    counter: &C,
    ctx_size: u32,
    image_token_estimate: u32,
    think: bool,
    system_prompt: &str,
    mut messages: Vec<ChatMessage>,
) -> Result<Fitted, String> {
    let reserve = reserve_tokens(ctx_size, think);
    messages.insert(
        0,
        ChatMessage { role: "system".to_string(), text: system_prompt.to_string(), images: Vec::new() },
    );
    let mut dropped_messages = 0u32;
    loop {
        let images: usize = messages.iter().map(|message| message.images.len()).sum();
        let prompt_tokens = counter
            .count_text_tokens(&messages)
            .await?
            .saturating_add(image_token_estimate.saturating_mul(images as u32));
        if prompt_tokens.saturating_add(reserve) <= ctx_size {
            return Ok(Fitted { messages, prompt_tokens, dropped_messages });
        }
        if messages.len() <= 2 {
            return Err(format!(
                "This message is too long for the current context (about {prompt_tokens} tokens; the limit for a message is {}). \
Shorten it or raise the context length in Assistant settings.",
                ctx_size.saturating_sub(reserve)
            ));
        }
        let removed = messages.remove(1);
        dropped_messages += 1;
        // The final message is a user message, so an assistant message at index 1 is never it.
        if removed.role == "user" && messages.len() > 2 && messages[1].role == "assistant" {
            messages.remove(1);
            dropped_messages += 1;
        }
    }
}

// ---------------------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------------------

pub(crate) fn http_client() -> Result<Client, String> {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = Client::builder()
        .user_agent(concat!("VoiceReader/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(5))
        // The server is on this machine; a system proxy must never see the API key.
        .no_proxy()
        .build()
        .context("Failed to create chat client")
        .map_err(|error| format!("{error:#}"))?;
    Ok(CLIENT.get_or_init(|| client).clone())
}

fn post(client: &Client, target: &ChatTarget, path: &str) -> reqwest::RequestBuilder {
    client
        .post(format!("{}{path}", target.base_url.trim_end_matches('/')))
        .bearer_auth(&target.api_key)
}

/// Turns a non-success response into a short sentence with the HTTP status.
pub(crate) async fn http_error(response: reqwest::Response) -> String {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let detail = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|value| value.get("error").map(error_message))
        .unwrap_or_else(|| body.trim().chars().take(200).collect());
    if detail.is_empty() {
        format!("The assistant server returned HTTP {status}.")
    } else {
        format!("The assistant server returned HTTP {status}: {detail}")
    }
}

pub(crate) fn connection_error(error: reqwest::Error) -> String {
    if error.is_connect() || error.is_timeout() {
        "Could not reach the assistant server. Check that it is running.".to_string()
    } else {
        UNEXPECTED_STOP.to_string()
    }
}

/// A short, non-streaming JSON request (the token-counting calls).
pub(crate) async fn post_json(client: &Client, target: &ChatTarget, path: &str, body: &Value) -> Result<Value, String> {
    let response = post(client, target, path)
        .timeout(Duration::from_secs(30))
        .json(body)
        .send()
        .await
        .map_err(connection_error)?;
    if !response.status().is_success() {
        return Err(http_error(response).await);
    }
    response
        .json::<Value>()
        .await
        .map_err(|_| "The assistant server sent a response that could not be read.".to_string())
}

/// Sends the chat request and forwards the stream to the slot's sink until it ends.
/// Runs inside an `Abortable`: dropping it closes the connection.
#[cfg(test)]
async fn run_stream(client: Client, url: String, api_key: String, body: Value, slot: Arc<Slot>) {
    match stream_answer(&client, &url, &api_key, &body, &slot).await {
        Ok(finish_reason) => slot.finish_done(finish_reason.as_deref()),
        Err(message) => slot.finish_error(&message),
    }
}

#[cfg(test)]
async fn stream_answer(
    client: &Client,
    url: &str,
    api_key: &str,
    body: &Value,
    slot: &Slot,
) -> Result<Option<String>, String> {
    let mut response = client
        .post(url)
        .bearer_auth(api_key)
        .json(body)
        .send()
        .await
        .map_err(connection_error)?;
    if !response.status().is_success() {
        return Err(http_error(response).await);
    }
    let mut parser = SseParser::default();
    let mut finish_reason = None;
    loop {
        let Some(bytes) = response.chunk().await.map_err(|_| UNEXPECTED_STOP.to_string())? else {
            return Err(UNEXPECTED_STOP.to_string());
        };
        for event in parser.push(&bytes) {
            match event {
                StreamEvent::Delta { content, reasoning, finish_reason: finished } => {
                    if content.is_some() || reasoning.is_some() {
                        slot.delta(content.as_deref(), reasoning.as_deref());
                    }
                    if finished.is_some() {
                        finish_reason = finished;
                    }
                }
                StreamEvent::Error(message) => return Err(message),
                StreamEvent::Done => return Ok(finish_reason),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------------------

/// Validates, counts tokens, evicts old messages and starts the HTTP stream on a spawned
/// tokio task. Returns once streaming has started, or an error the UI can show as-is
/// (including the "message too long" case). Registers the request for cancellation.
///
/// If `cancel_chat` runs before streaming starts, the sink has already received
/// `done(stopped = true)`; this function then returns `Ok` with zero counts and starts nothing.
#[cfg(test)]
pub(crate) async fn start_chat(
    target: ChatTarget,
    request: ChatRequest,
    sink: Arc<dyn ChatSink>,
) -> Result<ChatStart, String> {
    let messages = validate_request(&request)?;
    if let Some(sampling) = request.sampling.as_ref() {
        sampling.validate()?;
    }
    let system_prompt = request.system_prompt.clone().unwrap_or_else(|| DEFAULT_CHAT_PROMPT.to_string());
    if system_prompt.chars().count() > MAX_SYSTEM_PROMPT_CHARS {
        return Err(format!("The system prompt is longer than {MAX_SYSTEM_PROMPT_CHARS} characters."));
    }
    let Some(slot) = register(&request.request_id, sink.clone())? else {
        // Stop arrived before the start: report it like any other cancel and send nothing.
        sink.done(&request.request_id, None, true);
        return Ok(ChatStart { request_id: request.request_id, prompt_tokens: 0, dropped_messages: 0 });
    };
    match begin_stream(target, &slot, messages, request.think, &system_prompt, request.sampling).await {
        Ok(start) => Ok(start),
        Err(message) => {
            if slot.abandon() {
                Err(message)
            } else {
                Ok(ChatStart { request_id: request.request_id, prompt_tokens: 0, dropped_messages: 0 })
            }
        }
    }
}

#[cfg(test)]
async fn begin_stream(
    target: ChatTarget,
    slot: &Arc<Slot>,
    messages: Vec<ChatMessage>,
    think: bool,
    system_prompt: &str,
    sampling: Option<Sampling>,
) -> Result<ChatStart, String> {
    let client = http_client()?;
    let cancelled = || ChatStart { request_id: slot.request_id.clone(), prompt_tokens: 0, dropped_messages: 0 };
    // Cancellation covers the token-counting calls too: aborting drops the in-flight request.
    let (fit_handle, fit_registration) = AbortHandle::new_pair();
    {
        let mut state = slot.lock();
        if state.finished {
            return Ok(cancelled());
        }
        state.abort = Some(fit_handle);
    }
    let counter = HttpCounter { client: &client, target: &target };
    let fitting = fit_to_context(&counter, target.ctx_size, target.image_token_estimate, think, system_prompt, messages);
    let fitted = match Abortable::new(fitting, fit_registration).await {
        Ok(result) => result?,
        Err(_) => return Ok(cancelled()),
    };
    let start = ChatStart {
        request_id: slot.request_id.clone(),
        prompt_tokens: fitted.prompt_tokens,
        dropped_messages: fitted.dropped_messages,
    };
    let room = target.ctx_size.saturating_sub(fitted.prompt_tokens).max(1);
    let max_tokens = sampling.and_then(|sampling| sampling.max_tokens).map_or(room, |limit| limit.min(room));
    let body = build_request_body(&fitted.messages, think, max_tokens, sampling.as_ref());
    let url = format!("{}/v1/chat/completions", target.base_url.trim_end_matches('/'));

    let (handle, registration) = AbortHandle::new_pair();
    {
        let mut state = slot.lock();
        if state.finished {
            return Ok(cancelled());
        }
        state.abort = Some(handle);
    }
    let stream = run_stream(client, url, target.api_key, body, slot.clone());
    tokio::spawn(Abortable::new(stream, registration));
    Ok(start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    fn msg(role: &str, text: &str) -> ChatMessage {
        ChatMessage { role: role.to_string(), text: text.to_string(), images: Vec::new() }
    }

    fn msg_n(role: &str, len: usize) -> ChatMessage {
        msg(role, &"x".repeat(len))
    }

    const PNG: &str = "data:image/png;base64,iVBORw0KGgo=";

    // ---- SSE parser ----

    fn delta(content: Option<&str>, reasoning: Option<&str>, finish: Option<&str>) -> StreamEvent {
        StreamEvent::Delta {
            content: content.map(str::to_string),
            reasoning: reasoning.map(str::to_string),
            finish_reason: finish.map(str::to_string),
        }
    }

    #[test]
    fn sse_parses_content_and_reasoning_deltas() {
        let mut parser = SseParser::default();
        let events = parser.push(
            b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"hmm\"}}]}\n\n\
              data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
        );
        assert_eq!(events, vec![delta(None, Some("hmm"), None), delta(Some("Hi"), None, None)]);
    }

    #[test]
    fn sse_handles_json_split_across_chunks_and_crlf() {
        let mut parser = SseParser::default();
        assert!(parser.push(b"data: {\"choices\":[{\"delta\":{\"con").is_empty());
        assert!(parser.push(b"tent\":\"Hel").is_empty());
        let events = parser.push(b"lo\"}}]}\r\n\r\n");
        assert_eq!(events, vec![delta(Some("Hello"), None, None)]);
    }

    #[test]
    fn sse_handles_split_multibyte_characters() {
        let line = "data: {\"choices\":[{\"delta\":{\"content\":\"caf\u{e9}\"}}]}\n\n".as_bytes();
        let split = line.iter().position(|&b| b == 0xc3).unwrap() + 1;
        let mut parser = SseParser::default();
        assert!(parser.push(&line[..split]).is_empty());
        assert_eq!(parser.push(&line[split..]), vec![delta(Some("caf\u{e9}"), None, None)]);
    }

    #[test]
    fn sse_handles_multiple_events_comments_finish_reason_and_done() {
        let mut parser = SseParser::default();
        let events = parser.push(
            b": keep-alive\n\n\
              data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n\
              data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
              data: {\"choices\":[],\"usage\":{}}\n\n\
              data: [DONE]\n\n",
        );
        assert_eq!(
            events,
            vec![
                delta(Some("a"), None, None),
                delta(None, None, Some("stop")),
                delta(None, None, None),
                StreamEvent::Done,
            ]
        );
    }

    #[test]
    fn sse_ignores_null_and_empty_content() {
        let mut parser = SseParser::default();
        let events = parser.push(b"data: {\"choices\":[{\"delta\":{\"content\":null,\"reasoning_content\":\"\"}}]}\n\n");
        assert_eq!(events, vec![delta(None, None, None)]);
    }

    #[test]
    fn sse_error_event_uses_the_server_message() {
        let mut parser = SseParser::default();
        let events = parser.push(b"data: {\"error\":{\"message\":\"context exceeded\",\"code\":400}}\n\n");
        assert_eq!(events, vec![StreamEvent::Error("context exceeded".to_string())]);
        let events = parser.push(b"data: {\"error\":\"plain\"}\n\n");
        assert_eq!(events, vec![StreamEvent::Error("plain".to_string())]);
    }

    #[test]
    fn sse_reports_unreadable_json() {
        let mut parser = SseParser::default();
        let events = parser.push(b"data: {oops\n\n");
        assert!(matches!(events.as_slice(), [StreamEvent::Error(_)]));
    }

    // ---- request body ----

    #[test]
    fn body_for_text_only_with_thinking() {
        let body = build_request_body(&[msg("system", "sys"), msg("user", "hi")], true, 1234, None);
        assert_eq!(body["model"], "local");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 1234);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["messages"][0], json!({"role": "system", "content": "sys"}));
        assert_eq!(body["messages"][1], json!({"role": "user", "content": "hi"}));
    }

    #[test]
    fn body_carries_sampling_settings() {
        let sampling = Sampling { temperature: 0.7, top_p: 0.95, top_k: 100, min_p: 0.0, repeat_penalty: 1.03, max_tokens: None };
        let body = build_request_body(&[msg("user", "hi")], false, 10, Some(&sampling));
        assert!((body["temperature"].as_f64().unwrap() - 0.7).abs() < 1e-6);
        assert_eq!(body["top_k"], 100);
        assert!((body["repeat_penalty"].as_f64().unwrap() - 1.03).abs() < 1e-6);
        let plain = build_request_body(&[msg("user", "hi")], false, 10, None);
        assert!(plain.get("temperature").is_none() && plain.get("top_k").is_none());
    }

    #[test]
    fn sampling_out_of_range_is_refused() {
        let good = Sampling { temperature: 1.0, top_p: 0.95, top_k: 64, min_p: 0.0, repeat_penalty: 1.0, max_tokens: Some(512) };
        assert!(good.validate().is_ok());
        for bad in [
            Sampling { temperature: 2.5, ..good },
            Sampling { top_p: 1.5, ..good },
            Sampling { top_k: 9999, ..good },
            Sampling { repeat_penalty: 0.1, ..good },
            Sampling { max_tokens: Some(4), ..good },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn body_without_thinking_sets_reasoning_effort_none() {
        let body = build_request_body(&[msg("user", "hi")], false, 10, None);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(body["reasoning_effort"], "none");
    }

    #[test]
    fn body_with_images_uses_content_parts() {
        let mut message = msg("user", "what is this?");
        message.images = vec![PNG.to_string(), "data:image/webp;base64,AAAA".to_string()];
        let body = build_request_body(&[message], true, 10, None);
        assert_eq!(
            body["messages"][0]["content"],
            json!([
                {"type": "text", "text": "what is this?"},
                {"type": "image_url", "image_url": {"url": PNG}},
                {"type": "image_url", "image_url": {"url": "data:image/webp;base64,AAAA"}},
            ])
        );
    }

    // ---- validation ----

    fn request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest { request_id: "r".to_string(), messages, think: false, system_prompt: None, sampling: None }
    }

    #[test]
    fn image_data_urls_are_validated() {
        assert!(is_valid_image_url(PNG));
        assert!(is_valid_image_url("data:image/jpeg;base64,/9j/4AAQ+Zg=="));
        assert!(is_valid_image_url("data:image/webp;base64,UklGRg=="));
        assert!(!is_valid_image_url("data:image/gif;base64,R0lGOD=="));
        assert!(!is_valid_image_url("data:image/png;base64,"));
        assert!(!is_valid_image_url("data:image/png,rawbytes"));
        assert!(!is_valid_image_url("https://example.com/a.png"));
        assert!(!is_valid_image_url("data:image/png;base64,abc def"));
        assert!(!is_valid_image_url("file:///etc/passwd"));
    }

    #[test]
    fn validation_rejects_bad_requests() {
        let err = |messages: Vec<ChatMessage>| validate_request(&request(messages)).err().unwrap();
        assert!(err(vec![]).contains("no message"));
        assert!(err(vec![msg("user", "a"), msg("assistant", "b")]).contains("last message"));
        assert!(err(vec![msg("system", "a"), msg("user", "b")]).contains("role"));
        assert!(err(vec![msg("user", "  ")]).contains("Type a message"));
        let mut bad = msg("user", "a");
        bad.images = vec!["https://example.com/a.png".to_string()];
        assert!(err(vec![bad]).contains("PNG, JPEG or WebP"));
        let mut many = msg("user", "a");
        many.images = vec![PNG.to_string(); 5];
        assert!(err(vec![many]).contains("at most 4"));
        let mut assistant_image = msg("assistant", "a");
        assistant_image.images = vec![PNG.to_string()];
        assert!(err(vec![assistant_image, msg("user", "b")]).contains("Only user"));
        let mut no_id = request(vec![msg("user", "a")]);
        no_id.request_id = " ".to_string();
        assert!(validate_request(&no_id).is_err());
    }

    #[test]
    fn validation_drops_empty_earlier_messages_and_keeps_images_only_messages() {
        let mut image_only = msg("user", "");
        image_only.images = vec![PNG.to_string()];
        let cleaned =
            validate_request(&request(vec![msg("user", "a"), msg("assistant", ""), image_only, msg("assistant", "b"), msg("user", "c")]))
                .unwrap();
        assert_eq!(cleaned.len(), 4);
        assert_eq!(cleaned[1].images.len(), 1);
    }

    // ---- context fitting ----

    /// 10 tokens for the system message, one token per character for everything else.
    struct FakeCounter {
        calls: AtomicU32,
    }

    impl FakeCounter {
        fn new() -> Self {
            Self { calls: AtomicU32::new(0) }
        }
    }

    impl TokenCounter for FakeCounter {
        async fn count_text_tokens(&self, messages: &[ChatMessage]) -> Result<u32, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(messages
                .iter()
                .map(|m| if m.role == "system" { 10 } else { m.text.len() as u32 })
                .sum())
        }
    }

    // ctx 1000 with Think off: reserve = min(2048, 250) = 250, so the prompt may use 750.

    #[test]
    fn reserve_is_capped_at_a_quarter_of_the_context() {
        assert_eq!(reserve_tokens(1000, false), 250);
        assert_eq!(reserve_tokens(1000, true), 250);
        assert_eq!(reserve_tokens(16384, false), 2048);
        assert_eq!(reserve_tokens(16384, true), 4096);
        assert_eq!(reserve_tokens(65536, true), 8192);
    }

    #[tokio::test]
    async fn fitting_keeps_everything_when_it_fits() {
        let counter = FakeCounter::new();
        let messages = vec![msg_n("user", 100), msg_n("assistant", 100), msg_n("user", 100)];
        let fitted = fit_to_context(&counter, 1000, 0, false, DEFAULT_CHAT_PROMPT, messages).await.unwrap();
        assert_eq!(fitted.dropped_messages, 0);
        assert_eq!(fitted.prompt_tokens, 310);
        assert_eq!(fitted.messages.len(), 4);
        assert_eq!(fitted.messages[0].role, "system");
        assert_eq!(counter.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fitting_drops_the_oldest_pair_first() {
        let counter = FakeCounter::new();
        let messages = vec![
            msg_n("user", 300),
            msg_n("assistant", 300),
            msg_n("user", 101),
            msg_n("assistant", 102),
            msg_n("user", 103),
        ];
        let fitted = fit_to_context(&counter, 1000, 0, false, DEFAULT_CHAT_PROMPT, messages).await.unwrap();
        assert_eq!(fitted.dropped_messages, 2);
        assert_eq!(fitted.prompt_tokens, 10 + 101 + 102 + 103);
        let lengths: Vec<usize> = fitted.messages.iter().map(|m| m.text.len()).collect();
        assert_eq!(&lengths[1..], &[101, 102, 103]);
    }

    #[tokio::test]
    async fn fitting_keeps_the_final_user_message_and_drops_lone_assistant_messages() {
        let counter = FakeCounter::new();
        let messages = vec![msg_n("user", 100), msg_n("assistant", 500), msg_n("user", 300)];
        let fitted = fit_to_context(&counter, 1000, 0, false, DEFAULT_CHAT_PROMPT, messages).await.unwrap();
        assert_eq!(fitted.dropped_messages, 2);
        assert_eq!(fitted.messages.len(), 2);
        assert_eq!(fitted.messages[1].role, "user");
        assert_eq!(fitted.messages[1].text.len(), 300);
    }

    #[tokio::test]
    async fn fitting_does_not_pair_a_user_message_with_the_final_message() {
        let counter = FakeCounter::new();
        // Two user messages in a row: the older one is dropped alone, never with the final one.
        let messages = vec![msg_n("user", 400), msg_n("user", 400)];
        let fitted = fit_to_context(&counter, 1000, 0, false, DEFAULT_CHAT_PROMPT, messages).await.unwrap();
        assert_eq!(fitted.dropped_messages, 1);
        assert_eq!(fitted.messages.len(), 2);
    }

    #[tokio::test]
    async fn fitting_rejects_an_oversized_final_message_with_numbers() {
        let counter = FakeCounter::new();
        let messages = vec![msg_n("user", 100), msg_n("assistant", 100), msg_n("user", 800)];
        let error = fit_to_context(&counter, 1000, 0, false, DEFAULT_CHAT_PROMPT, messages).await.err().unwrap();
        assert_eq!(
            error,
            "This message is too long for the current context (about 810 tokens; the limit for a message is 750). \
Shorten it or raise the context length in Assistant settings."
        );
    }

    #[tokio::test]
    async fn fitting_counts_image_estimates_and_think_reserve() {
        let counter = FakeCounter::new();
        let mut last = msg_n("user", 100);
        last.images = vec![PNG.to_string(), PNG.to_string()];
        // 10 + 100 text + 2 * 300 images = 710 fits with reserve 250 (limit 750).
        let fitted = fit_to_context(&counter, 1000, 300, false, DEFAULT_CHAT_PROMPT, vec![last.clone()]).await.unwrap();
        assert_eq!(fitted.prompt_tokens, 710);
        // One more image tips it over, and there is nothing older to drop.
        last.images.push(PNG.to_string());
        let error = fit_to_context(&counter, 1000, 300, false, DEFAULT_CHAT_PROMPT, vec![last]).await.err().unwrap();
        assert!(error.contains("about 1010 tokens"), "{error}");
        assert!(error.contains("limit for a message is 750"), "{error}");
        // Think on with a large context reserves 8192 tokens.
        let error = fit_to_context(&counter, 10000, 0, true, DEFAULT_CHAT_PROMPT, vec![msg_n("user", 8000)]).await.err().unwrap();
        assert!(error.contains("limit for a message is 7500"), "{error}");
    }

    #[test]
    fn start_chat_future_is_send() {
        fn assert_send<T: Send>(_: &T) {}
        let target = ChatTarget { base_url: String::new(), api_key: String::new(), ctx_size: 0, image_token_estimate: 0 };
        let future = start_chat(target, request(vec![msg("user", "a")]), Arc::new(RecordingSink::default()));
        assert_send(&future);
    }

    // ---- mock llama-server ----

    #[derive(Debug, PartialEq, Clone)]
    enum Event {
        Delta(Option<String>, Option<String>),
        Done(Option<String>, bool),
        Error(String),
    }

    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<(String, Event)>>,
    }

    impl RecordingSink {
        fn events(&self) -> Vec<Event> {
            self.events.lock().unwrap().iter().map(|(_, event)| event.clone()).collect()
        }

        fn ids(&self) -> Vec<String> {
            self.events.lock().unwrap().iter().map(|(id, _)| id.clone()).collect()
        }

        fn push(&self, id: &str, event: Event) {
            self.events.lock().unwrap().push((id.to_string(), event));
        }

        async fn wait_for(&self, what: &str, check: impl Fn(&[Event]) -> bool) {
            for _ in 0..200 {
                if check(&self.events()) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("timed out waiting for {what}; events: {:?}", self.events());
        }
    }

    impl ChatSink for RecordingSink {
        fn delta(&self, id: &str, content: Option<&str>, reasoning: Option<&str>) {
            self.push(id, Event::Delta(content.map(str::to_string), reasoning.map(str::to_string)));
        }
        fn done(&self, id: &str, finish_reason: Option<&str>, stopped: bool) {
            self.push(id, Event::Done(finish_reason.map(str::to_string), stopped));
        }
        fn error(&self, id: &str, message: &str) {
            self.push(id, Event::Error(message.to_string()));
        }
    }

    #[test]
    fn stop_before_terminal_claim_wins_over_cached_completion_state() {
        let sink = Arc::new(RecordingSink::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let slot = register_turn("terminal-claim-race", sink.clone(), Some(cancel.clone()), None).unwrap().unwrap();
        // The caller computed stopped=false, then Stop arrived before finalization.
        cancel.store(true, Ordering::SeqCst);
        slot.finish_turn(json!({}), Some("stop"), false, Some("late socket failure"));
        slot.finish_turn(json!({}), Some("stop"), false, None);
        assert_eq!(sink.events(), vec![Event::Done(Some("stop".into()), true)]);
    }

    struct Recorded {
        path: String,
        authorization: String,
        body: Value,
    }

    #[derive(Clone)]
    enum ChatReply {
        /// Write these pieces (with a short pause between them), then close.
        Pieces(Vec<&'static str>),
        /// Write these pieces, then keep the connection open until the client hangs up.
        PiecesThenHang(Vec<&'static str>),
        /// Answer with this status and body instead of a stream.
        Status(u16, &'static str),
    }

    struct Mock {
        base_url: String,
        requests: Arc<Mutex<Vec<Recorded>>>,
        client_hung_up: Arc<AtomicBool>,
    }

    async fn read_request(socket: &mut TcpStream) -> Option<Recorded> {
        let mut data = Vec::new();
        let mut buf = [0u8; 4096];
        let header_end = loop {
            if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            let n = socket.read(&mut buf).await.ok()?;
            if n == 0 {
                return None;
            }
            data.extend_from_slice(&buf[..n]);
        };
        let head = String::from_utf8_lossy(&data[..header_end]).to_string();
        let header = |name: &str| {
            head.lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.trim().eq_ignore_ascii_case(name).then(|| value.trim().to_string())
                })
                .unwrap_or_default()
        };
        let length: usize = header("content-length").parse().unwrap_or(0);
        while data.len() < header_end + length {
            let n = socket.read(&mut buf).await.ok()?;
            if n == 0 {
                return None;
            }
            data.extend_from_slice(&buf[..n]);
        }
        let path = head.split_whitespace().nth(1)?.to_string();
        let body = serde_json::from_slice(&data[header_end..header_end + length]).unwrap_or(Value::Null);
        Some(Recorded { path, authorization: header("authorization"), body })
    }

    async fn write_json(socket: &mut TcpStream, status: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.ok();
    }

    async fn spawn_mock(reply: ChatReply) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let client_hung_up = Arc::new(AtomicBool::new(false));
        let (requests_task, hung_up_task) = (requests.clone(), client_hung_up.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let (requests, hung_up, reply) = (requests_task.clone(), hung_up_task.clone(), reply.clone());
                tokio::spawn(async move {
                    let Some(recorded) = read_request(&mut socket).await else { return };
                    let path = recorded.path.clone();
                    let body = recorded.body.clone();
                    requests.lock().unwrap().push(recorded);
                    match path.as_str() {
                        "/apply-template" => {
                            let joined: Vec<String> = body["messages"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|m| m["content"].as_str().unwrap().to_string())
                                .collect();
                            write_json(&mut socket, "200 OK", &json!({"prompt": joined.join(" ")}).to_string()).await;
                        }
                        "/tokenize" => {
                            // One token per whitespace-separated word.
                            let words = body["content"].as_str().unwrap().split_whitespace().count();
                            write_json(&mut socket, "200 OK", &json!({"tokens": vec![0; words]}).to_string()).await;
                        }
                        "/v1/chat/completions" => match &reply {
                            ChatReply::Status(code, text) => {
                                write_json(&mut socket, &format!("{code} Error"), text).await;
                            }
                            ChatReply::Pieces(pieces) | ChatReply::PiecesThenHang(pieces) => {
                                let hang = matches!(reply, ChatReply::PiecesThenHang(_));
                                socket
                                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                                    .await
                                    .ok();
                                for piece in pieces {
                                    socket.write_all(piece.as_bytes()).await.ok();
                                    socket.flush().await.ok();
                                    tokio::time::sleep(Duration::from_millis(30)).await;
                                }
                                if hang {
                                    let mut buf = [0u8; 64];
                                    loop {
                                        match socket.read(&mut buf).await {
                                            Ok(0) | Err(_) => break,
                                            Ok(_) => {}
                                        }
                                    }
                                    hung_up.store(true, Ordering::SeqCst);
                                }
                            }
                        },
                        _ => write_json(&mut socket, "404 Not Found", "{}").await,
                    }
                });
            }
        });
        Mock { base_url, requests, client_hung_up }
    }

    fn target(mock: &Mock) -> ChatTarget {
        ChatTarget {
            base_url: mock.base_url.clone(),
            api_key: "secret-key".to_string(),
            ctx_size: 4096,
            image_token_estimate: 256,
        }
    }

    fn user_request(id: &str, think: bool) -> ChatRequest {
        ChatRequest {
            request_id: id.to_string(),
            messages: vec![msg("user", "hello there"), msg("assistant", "hi"), msg("user", "tell me more")],
            think,
            system_prompt: None,
            sampling: None,
        }
    }

    #[tokio::test]
    async fn streams_ordered_deltas_then_one_done() {
        let mock = spawn_mock(ChatReply::Pieces(vec![
            ": keep-alive\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Hm\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"con",
            "tent\":\"Hello\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ]))
        .await;
        let sink = Arc::new(RecordingSink::default());
        let start = start_chat(target(&mock), user_request("mock-stream", false), sink.clone()).await.unwrap();
        assert_eq!(start.request_id, "mock-stream");
        assert_eq!(start.dropped_messages, 0);
        assert!(start.prompt_tokens > 0);

        sink.wait_for("done", |events| events.iter().any(|e| matches!(e, Event::Done(..)))).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            sink.events(),
            vec![
                Event::Delta(None, Some("Hm".to_string())),
                Event::Delta(Some("Hello".to_string()), None),
                Event::Delta(Some(" world".to_string()), None),
                Event::Done(Some("stop".to_string()), false),
            ]
        );
        assert!(sink.ids().iter().all(|id| id == "mock-stream"));

        let requests = mock.requests.lock().unwrap();
        let paths: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["/apply-template", "/tokenize", "/v1/chat/completions"]);
        assert!(requests.iter().all(|r| r.authorization == "Bearer secret-key"));
        let template_messages = requests[0].body["messages"].as_array().unwrap();
        assert_eq!(template_messages.len(), 4);
        assert_eq!(template_messages[0]["role"], "system");
        let chat = &requests[2].body;
        assert_eq!(chat["stream"], true);
        assert_eq!(chat["model"], "local");
        assert_eq!(chat["reasoning_effort"], "none");
        assert_eq!(chat["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(chat["max_tokens"], 4096 - start.prompt_tokens);
        assert_eq!(chat["messages"].as_array().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn a_stream_that_closes_without_done_is_an_error() {
        let mock = spawn_mock(ChatReply::Pieces(vec!["data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n"])).await;
        let sink = Arc::new(RecordingSink::default());
        start_chat(target(&mock), user_request("mock-truncated", true), sink.clone()).await.unwrap();
        sink.wait_for("error", |events| events.iter().any(|e| matches!(e, Event::Error(..)))).await;
        assert_eq!(
            sink.events(),
            vec![
                Event::Delta(Some("Hi".to_string()), None),
                Event::Error("The assistant stopped unexpectedly.".to_string()),
            ]
        );
        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests[2].body["chat_template_kwargs"]["enable_thinking"], true);
        assert!(requests[2].body.get("reasoning_effort").is_none());
    }

    #[tokio::test]
    async fn an_http_error_becomes_one_error_with_the_status_and_server_message() {
        let mock = spawn_mock(ChatReply::Status(503, "{\"error\":{\"message\":\"Loading model\",\"code\":503}}")).await;
        let sink = Arc::new(RecordingSink::default());
        start_chat(target(&mock), user_request("mock-503", false), sink.clone()).await.unwrap();
        sink.wait_for("error", |events| !events.is_empty()).await;
        let events = sink.events();
        assert_eq!(events.len(), 1);
        let Event::Error(message) = &events[0] else { panic!("expected an error, got {events:?}") };
        assert!(message.contains("503") && message.contains("Loading model"), "{message}");
        assert!(!message.contains("secret-key"));
    }

    #[tokio::test]
    async fn an_error_event_in_the_stream_becomes_one_error() {
        let mock = spawn_mock(ChatReply::Pieces(vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"A\"}}]}\n\n",
            "data: {\"error\":{\"message\":\"slot failed\"}}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"late\"}}]}\n\n",
        ]))
        .await;
        let sink = Arc::new(RecordingSink::default());
        start_chat(target(&mock), user_request("mock-error-event", false), sink.clone()).await.unwrap();
        sink.wait_for("error", |events| events.iter().any(|e| matches!(e, Event::Error(..)))).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            sink.events(),
            vec![Event::Delta(Some("A".to_string()), None), Event::Error("slot failed".to_string())]
        );
    }

    #[tokio::test]
    async fn cancel_mid_stream_emits_one_stopped_done_and_nothing_after() {
        let mock = spawn_mock(ChatReply::PiecesThenHang(vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
        ]))
        .await;
        let sink = Arc::new(RecordingSink::default());
        start_chat(target(&mock), user_request("mock-cancel", false), sink.clone()).await.unwrap();
        sink.wait_for("first delta", |events| !events.is_empty()).await;

        cancel_chat("mock-cancel");
        cancel_chat("mock-cancel");
        cancel_chat("unknown-id");

        for _ in 0..100 {
            if mock.client_hung_up.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(mock.client_hung_up.load(Ordering::SeqCst), "the HTTP connection was not closed");
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            sink.events(),
            vec![Event::Delta(Some("partial".to_string()), None), Event::Done(None, true)]
        );
        assert!(registry().lock().unwrap().running.get("mock-cancel").is_none());
    }

    /// Stop can reach Rust before the start command registers the request (Codex re-review R1).
    #[tokio::test]
    async fn cancel_before_registration_prevents_the_start() {
        let sink = Arc::new(RecordingSink::default());
        cancel_chat("early-stop");
        let target = ChatTarget {
            // Nothing listens here: the request must not even try to connect.
            base_url: "http://127.0.0.1:9".to_string(),
            api_key: "k".to_string(),
            ctx_size: 4096,
            image_token_estimate: 0,
        };
        let request = ChatRequest {
            request_id: "early-stop".to_string(),
            messages: vec![ChatMessage { role: "user".to_string(), text: "hi".to_string(), images: vec![] }],
            think: false,
            system_prompt: None,
            sampling: None,
        };
        let start = start_chat(target, request, sink.clone()).await.expect("a cancelled start is Ok");
        assert_eq!((start.prompt_tokens, start.dropped_messages), (0, 0));
        assert!(registry().lock().unwrap().running.get("early-stop").is_none());
        assert!(!registry().lock().unwrap().cancelled_early.iter().any(|id| id == "early-stop"));
        assert_eq!(sink.events(), vec![Event::Done(None, true)]);
    }

    /// `/apply-template` never answers, so the request is still being fitted when it is
    /// cancelled. Only `cancel_chat` is used: `start_chat` must resolve by itself.
    #[tokio::test]
    async fn cancel_during_context_fitting_resolves_start_chat_and_closes_the_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let accepted = Arc::new(AtomicU32::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let (accepted_task, closed_task) = (accepted.clone(), closed.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let (accepted, closed) = (accepted_task.clone(), closed_task.clone());
                tokio::spawn(async move {
                    if read_request(&mut socket).await.is_some() {
                        accepted.fetch_add(1, Ordering::SeqCst);
                    }
                    // Hold the connection open without answering until the client hangs up.
                    let mut buf = [0u8; 16];
                    while matches!(socket.read(&mut buf).await, Ok(n) if n > 0) {}
                    closed.store(true, Ordering::SeqCst);
                });
            }
        });
        let target = ChatTarget { base_url, api_key: "k".to_string(), ctx_size: 4096, image_token_estimate: 0 };
        let sink = Arc::new(RecordingSink::default());
        let starting = tokio::spawn(start_chat(target, user_request("mock-cancel-early", false), sink.clone()));
        while accepted.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cancel_chat("mock-cancel-early");
        let start = tokio::time::timeout(Duration::from_secs(1), starting)
            .await
            .expect("start_chat did not resolve after cancel")
            .unwrap()
            .expect("a cancelled start is Ok");
        assert_eq!((start.prompt_tokens, start.dropped_messages), (0, 0));
        for _ in 0..40 {
            if closed.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(closed.load(Ordering::SeqCst), "the counting connection was not closed");
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(accepted.load(Ordering::SeqCst), 1, "more requests were sent after the cancel");
        assert_eq!(sink.events(), vec![Event::Done(None, true)]);
        assert!(registry().lock().unwrap().running.get("mock-cancel-early").is_none());
    }

    #[tokio::test]
    async fn an_oversized_message_is_refused_before_any_chat_request() {
        let mock = spawn_mock(ChatReply::Pieces(vec![])).await;
        let sink = Arc::new(RecordingSink::default());
        let mut small = target(&mock);
        small.ctx_size = 20;
        let error = start_chat(small, user_request("mock-too-long", false), sink.clone()).await.err().unwrap();
        assert!(error.starts_with("This message is too long for the current context"), "{error}");
        assert!(sink.events().is_empty());
        assert!(mock.requests.lock().unwrap().iter().all(|r| r.path != "/v1/chat/completions"));
        assert!(registry().lock().unwrap().running.get("mock-too-long").is_none());
    }

    #[tokio::test]
    async fn duplicate_request_ids_are_rejected_while_running() {
        let mock = spawn_mock(ChatReply::PiecesThenHang(vec![])).await;
        let sink = Arc::new(RecordingSink::default());
        start_chat(target(&mock), user_request("mock-dup", false), sink.clone()).await.unwrap();
        let error = start_chat(target(&mock), user_request("mock-dup", false), sink.clone()).await.err().unwrap();
        assert!(error.contains("already running"));
        cancel_chat("mock-dup");
        assert_eq!(sink.events(), vec![Event::Done(None, true)]);
    }

    #[tokio::test]
    async fn an_unreachable_server_is_a_start_error_or_one_error_event() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let target = ChatTarget { base_url, api_key: "k".to_string(), ctx_size: 4096, image_token_estimate: 0 };
        let sink = Arc::new(RecordingSink::default());
        let error = start_chat(target, user_request("mock-down", false), sink.clone()).await.err().unwrap();
        assert!(error.contains("Could not reach the assistant server"), "{error}");
        assert!(sink.events().is_empty());
    }
}
