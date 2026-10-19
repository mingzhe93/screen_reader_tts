//! One cancellation/lifecycle owner for an entire model → tools → model turn.
//! The public chat bridge retains its legacy path for old callers and regression tests.
use crate::assistant_chat::{
    self as chat, ChatMessage, ChatRequest, ChatSink, ChatStart, ChatTarget, Slot,
};
use crate::assistant_tools::{self as tools, ToolOutcome, ToolSelection, ToolServices, ToolSource, TurnOptions};
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const CANCELLED: &str = "Assistant turn cancelled.";
const MAX_ARGUMENT_BYTES: usize = 65_536;
const MAX_CALLS: usize = 16;
const MAX_EVENT_BYTES: usize = 1_048_576;
const MAX_ROUND_TEXT_BYTES: usize = 4_194_304;
const TRUNCATED_RESULT: &str =
    "[Tool result truncated to fit the context. Use the available information.]";

fn speeds() -> &'static Mutex<HashMap<String, f64>> {
    static SPEEDS: OnceLock<Mutex<HashMap<String, f64>>> = OnceLock::new();
    SPEEDS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn note_speed(model: &str, timings: &Value) {
    if let Some(speed) = timings
        .get("predicted_per_second")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.)
    {
        let mut speeds = speeds().lock().unwrap_or_else(|e| e.into_inner());
        let previous = speeds.get(model).copied().unwrap_or(speed);
        speeds.insert(model.to_string(), previous * 0.5 + speed * 0.5);
    }
}
// A private boundary permits recorded-tool tests without external services or model startup.
trait Services: Send + Sync {
    fn execute<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        cancel: Arc<AtomicBool>,
    ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>>;
    fn sources(&self) -> Vec<ToolSource>;
    fn history_note(&self) -> String;
}
impl Services for ToolServices {
    fn execute<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        cancel: Arc<AtomicBool>,
    ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
        Box::pin(ToolServices::execute(self, name, args, cancel))
    }
    fn sources(&self) -> Vec<ToolSource> {
        ToolServices::sources(self)
    }
    fn history_note(&self) -> String {
        ToolServices::history_note(self)
    }
}
struct Runtime {
    root: PathBuf,
    model_id: String,
    max_rounds: u32,
    enabled_tools: ToolSelection,
    ready: Box<dyn Fn() -> bool + Send + Sync>,
    services: Arc<dyn Services>,
    #[cfg(test)]
    seed: Option<u32>,
}
fn cancelled(options: &Runtime, cancel: &AtomicBool) -> bool {
    if cancel.load(Ordering::SeqCst) {
        return true;
    }
    if !(options.ready)() {
        cancel.store(true, Ordering::SeqCst);
        return true;
    }
    false
}
/// Dropping fitting/completion HTTP futures closes the socket, including between rounds.
async fn checked<F, T>(future: F, options: &Runtime, cancel: &AtomicBool) -> Result<T, String>
where
    F: Future<Output = Result<T, String>>,
{
    tokio::pin!(future);
    loop {
        if cancelled(options, cancel) {
            return Err(CANCELLED.into());
        }
        tokio::select! {
            result = &mut future => return result,
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    }
}

#[derive(Default, Debug)]
struct Call {
    index: usize,
    id: String,
    name: String,
    arguments: String,
    invalid: Option<String>,
}
impl Call {
    /// The call as replayed in the next request's history. Arguments that are not a JSON
    /// object are replaced by `{}`: templates that render arguments as fields (the Qwen/MiMo
    /// XML format) make llama.cpp parse them, and an unparsable string would fail the request
    /// meant to let the model retry. The model still sees the error in the tool result.
    fn wire(&self, round: u32) -> Value {
        let arguments = match serde_json::from_str::<Value>(&self.arguments) {
            Ok(Value::Object(_)) => self.arguments.clone(),
            _ => "{}".to_string(),
        };
        json!({"id": self.stable_id(round), "type":"function", "function":{"name":self.name,"arguments":arguments}})
    }
    fn stable_id(&self, round: u32) -> String {
        if self.id.is_empty() {
            format!("vr_{round}_{}", self.index)
        } else {
            self.id.clone()
        }
    }
    fn validated(&self) -> Result<Value, String> {
        if let Some(error) = &self.invalid {
            return Err(error.clone());
        }
        let args: Value = serde_json::from_str(&self.arguments)
            .map_err(|_| "Tool arguments are not complete JSON.".to_string())?;
        tools::validate_arguments(&self.name, &args)?;
        Ok(args)
    }
}

#[derive(Default)]
struct Completion {
    content: String,
    reasoning: String,
    calls: BTreeMap<usize, Call>,
    finish: Option<String>,
    timings: Value,
    completion_tokens: Option<u64>,
}
impl Completion {
    fn assistant_message(&self, round: u32) -> Value {
        let mut message = json!({"role":"assistant","content":self.content});
        if !self.reasoning.is_empty() {
            message["reasoning_content"] = json!(self.reasoning);
        }
        if !self.calls.is_empty() {
            message["tool_calls"] =
                Value::Array(self.calls.values().map(|call| call.wire(round)).collect());
        }
        message
    }
}

/// Returns JSON with stable key ordering for repeat detection, including nested objects.
fn canonical(value: &Value) -> String {
    match value {
        Value::Object(object) => {
            let sorted: BTreeMap<_, _> = object
                .iter()
                .map(|(key, value)| (key, canonical(value)))
                .collect();
            format!(
                "{{{}}}",
                sorted
                    .into_iter()
                    .map(|(k, v)| format!("{}:{v}", serde_json::to_string(k).unwrap()))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        Value::Array(array) => format!(
            "[{}]",
            array.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        _ => value.to_string(),
    }
}

fn call_event(slot: &Slot, round: u32, call: &Call, delta: &str, phase: &str, done: bool) {
    let mut payload = json!({"request_id":slot.request_id,"round":round,"call_index":call.index,"delta":delta,"phase":phase,"done":done});
    if !call.id.is_empty() {
        payload["call_id"] = json!(call.id);
    }
    if !call.name.is_empty() {
        payload["name"] = json!(call.name);
    }
    if done {
        payload["call_id"] = json!(call.stable_id(round));
        payload["arguments"] = json!(call.arguments);
    }
    slot.event("assistant:tool-call", payload);
}
fn phase(slot: &Slot, round: u32, phase: &str) {
    slot.event(
        "assistant:turn-phase",
        json!({"request_id":slot.request_id,"round":round,"phase":phase}),
    );
}
fn result_event(slot: &Slot, round: u32, call: &Call, outcome: &ToolOutcome) {
    let mut payload = serde_json::to_value(outcome).unwrap_or_else(|_| json!({}));
    payload["request_id"] = json!(slot.request_id);
    payload["round"] = json!(round);
    payload["call_id"] = json!(call.stable_id(round));
    payload["name"] = json!(call.name);
    slot.event("assistant:tool-result", payload);
}
fn error_outcome(status: &str, text: &str) -> ToolOutcome {
    ToolOutcome {
        status: status.into(),
        summary: text.into(),
        sources: vec![],
        output: None,
        content: json!({"error":text}).to_string(),
    }
}

/// Raw SSE lines are retained until a complete event; tools are never parsed from prose/XML.
#[derive(Default)]
struct RawSse {
    buffer: Vec<u8>,
    lines: Vec<String>,
    event_bytes: usize,
}
impl RawSse {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, String> {
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() > MAX_EVENT_BYTES {
            return Err("The assistant stream event was too large.".into());
        }
        let mut out = vec![];
        while let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<_> = self.buffer.drain(..=end).collect();
            let line = std::str::from_utf8(&line[..line.len() - 1])
                .map_err(|_| "The assistant stream contained invalid text.".to_string())?;
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.is_empty() {
                if !self.lines.is_empty() {
                    out.push(self.lines.join("\n"));
                    self.lines.clear();
                    self.event_bytes = 0;
                }
            } else if let Some(data) = line.strip_prefix("data:") {
                self.event_bytes += data.len();
                if self.event_bytes > MAX_EVENT_BYTES {
                    return Err("The assistant stream event was too large.".into());
                }
                self.lines
                    .push(data.strip_prefix(' ').unwrap_or(data).to_string());
            }
        }
        Ok(out)
    }
}

fn accumulate(
    completion: &mut Completion,
    value: &Value,
    slot: &Slot,
    round: u32,
) -> Result<(), String> {
    if let Some(error) = value.get("error") {
        return Err(error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("The assistant server reported an error.")
            .to_string());
    }
    if let Some(timings) = value.get("timings") {
        completion.timings = timings.clone();
    }
    if let Some(tokens) = value
        .pointer("/usage/completion_tokens")
        .and_then(Value::as_u64)
    {
        completion.completion_tokens = Some(tokens);
    }
    let Some(choice) = value.pointer("/choices/0") else {
        return Ok(());
    };
    if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
        completion.finish = Some(finish.into());
    }
    let delta = &choice["delta"];
    let content = delta
        .get("content")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let reasoning = delta
        .get("reasoning_content")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    if let Some(text) = content {
        completion.content.push_str(text);
    }
    if let Some(text) = reasoning {
        completion.reasoning.push_str(text);
    }
    if completion.content.len() + completion.reasoning.len() > MAX_ROUND_TEXT_BYTES {
        return Err("The assistant response was too large.".into());
    }
    if content.is_some() || reasoning.is_some() {
        // Main sinks track the phase's round; this private event supports consumers needing
        // an explicit boundary before forwarding the unchanged legacy delta callback.
        slot.event(
            "assistant:delta-round",
            json!({"request_id":slot.request_id,"round":round}),
        );
        slot.delta(content, reasoning);
    }
    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for fragment in calls {
            let index = fragment
                .get("index")
                .and_then(Value::as_u64)
                .ok_or("A tool call had no valid index.")? as usize;
            if index >= MAX_CALLS {
                return Err("The assistant requested too many tools at once.".into());
            }
            let call = completion.calls.entry(index).or_insert_with(|| Call {
                index,
                ..Call::default()
            });
            if let Some(id) = fragment.get("id").and_then(Value::as_str) {
                if call.id.len() + id.len() <= 256 {
                    call.id.push_str(id);
                } else {
                    call.invalid = Some("The tool call id is too long.".into());
                }
            }
            if let Some(name) = fragment.pointer("/function/name").and_then(Value::as_str) {
                if call.name.len() + name.len() <= 128 {
                    call.name.push_str(name);
                } else {
                    call.invalid = Some("The tool name is too long.".into());
                }
            }
            let args = fragment
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("");
            if call.arguments.len() + args.len() <= MAX_ARGUMENT_BYTES {
                call.arguments.push_str(args);
            } else {
                call.invalid = Some("Tool arguments exceeded the size limit.".into());
            }
            call_event(
                slot,
                round,
                call,
                if call.invalid.is_none() { args } else { "" },
                "streaming",
                false,
            );
        }
    }
    Ok(())
}

async fn completion(
    client: &Client,
    target: &ChatTarget,
    body: &Value,
    slot: &Slot,
    round: u32,
    state: &mut Completion,
) -> Result<(), String> {
    let mut response = client
        .post(format!(
            "{}/v1/chat/completions",
            target.base_url.trim_end_matches('/')
        ))
        .bearer_auth(&target.api_key)
        .json(body)
        .send()
        .await
        .map_err(chat::connection_error)?;
    if !response.status().is_success() {
        return Err(chat::http_error(response).await);
    }
    let mut parser = RawSse::default();
    while let Some(bytes) = response
        .chunk()
        .await
        .map_err(|_| "The assistant stopped unexpectedly.".to_string())?
    {
        for event in parser.push(&bytes)? {
            if event.trim() == "[DONE]" {
                return if state.finish.is_some() {
                    Ok(())
                } else {
                    Err("The assistant stopped before finishing its response.".into())
                };
            }
            let value: Value = serde_json::from_str(&event)
                .map_err(|_| "The assistant sent an unreadable stream event.".to_string())?;
            accumulate(state, &value, slot, round)?;
        }
    }
    Err("The assistant stopped unexpectedly.".into())
}

struct Context {
    system: Value,
    older: Vec<Vec<Value>>,
    current: Vec<Value>,
    dropped: u32,
}
impl Context {
    fn force_final(&mut self, reason: &str) {
        // A trailing assistant message is a llama.cpp prefill, so put recovery policy
        // in the system message and leave the real user/tool history intact.
        let system = self.system["content"].as_str().unwrap_or("");
        self.system["content"] = json!(format!("{system}\nApp recovery: {reason} Answer using only the information already available; do not call tools."));
    }
    fn new(system: &str, messages: &[ChatMessage]) -> Self {
        let mut older: Vec<Vec<Value>> = vec![];
        let last = messages.len() - 1;
        for message in &messages[..last] {
            if message.role == "user" || older.is_empty() {
                older.push(vec![]);
            }
            older.last_mut().unwrap().push(chat::wire_message(message));
        }
        Self {
            system: json!({"role":"system","content":system}),
            older,
            current: vec![chat::wire_message(&messages[last])],
            dropped: 0,
        }
    }
    fn messages(&self) -> Vec<Value> {
        std::iter::once(self.system.clone())
            .chain(self.older.iter().flatten().cloned())
            .chain(self.current.iter().cloned())
            .collect()
    }
    fn truncate_oldest_result(&mut self) -> bool {
        for message in &mut self.current {
            if message["role"] != "tool" {
                continue;
            }
            let Some(content) = message["content"].as_str() else {
                continue;
            };
            if content == TRUNCATED_RESULT {
                continue;
            }
            // Halve oldest first; preserve newest information for longest. The call/result
            // pair stays intact even when no result text can be retained.
            let chars = content.chars().count();
            message["content"] = json!(if chars > 256 {
                format!(
                    "{}\n{TRUNCATED_RESULT}",
                    content.chars().take(chars / 2).collect::<String>()
                )
            } else {
                TRUNCATED_RESULT.to_string()
            });
            return true;
        }
        false
    }
}

fn text_only(messages: &[Value]) -> (Vec<Value>, u32) {
    let mut images = 0;
    let messages = messages
        .iter()
        .cloned()
        .map(|mut message| {
            if let Some(parts) = message.get("content").and_then(Value::as_array) {
                let mut text = vec![];
                for part in parts {
                    if let Some(value) = part.get("text").and_then(Value::as_str) {
                        text.push(value);
                    }
                    if part["type"] == "image_url" {
                        images += 1;
                    }
                }
                message["content"] = json!(text.join("\n"));
            }
            message
        })
        .collect();
    (messages, images)
}
async fn count(
    client: &Client,
    target: &ChatTarget,
    messages: &[Value],
    definitions: Option<&Value>,
    think: bool,
) -> Result<u32, String> {
    let (messages, images) = text_only(messages);
    let mut body = json!({"messages":messages,"add_generation_prompt":true,"chat_template_kwargs":{"enable_thinking":think}});
    if let Some(definitions) = definitions {
        body["tools"] = definitions.clone();
    }
    let template = chat::post_json(client, target, "/apply-template", &body).await?;
    let prompt = template
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or("The assistant server did not return a prompt to count.")?;
    let tokens = chat::post_json(
        client,
        target,
        "/tokenize",
        &json!({"content":prompt,"add_special":false}),
    )
    .await?;
    let tokens = tokens
        .get("tokens")
        .and_then(Value::as_array)
        .ok_or("The assistant server did not return a token count.")?
        .len() as u32;
    Ok(tokens.saturating_add(images.saturating_mul(target.image_token_estimate)))
}

/// Eviction always consumes whole old turns. Tool results are shortened only after those
/// turns are exhausted; a current user and a call/result pair are never split or evicted.
async fn fit(
    client: &Client,
    target: &ChatTarget,
    context: &mut Context,
    definitions: &Value,
    tools_on: &mut bool,
    think: bool,
    reserve: u32,
) -> Result<u32, String> {
    loop {
        let messages = context.messages();
        let tokens = count(
            client,
            target,
            &messages,
            tools_on.then_some(definitions),
            think,
        )
        .await?;
        if tokens.saturating_add(reserve) <= target.ctx_size {
            return Ok(tokens);
        }
        if !context.older.is_empty() {
            context.dropped += context.older.remove(0).len() as u32;
            continue;
        }
        if context.truncate_oldest_result() {
            *tools_on = false;
            continue;
        }
        if *tools_on {
            *tools_on = false;
            continue;
        }
        return Err(format!("This message is too long for the current context (about {tokens} tokens). Shorten it or raise the context length in Assistant settings."));
    }
}

fn body(
    context: &Context,
    tools_on: bool,
    definitions: &Value,
    request: &ChatRequest,
    max_tokens: u32,
    budget: Option<u32>,
) -> Value {
    let mut body = json!({"model":"local","messages":context.messages(),"stream":true,"stream_options":{"include_usage":true},"chat_template_kwargs":{"enable_thinking":request.think},"max_tokens":max_tokens});
    if tools_on {
        body["tools"] = definitions.clone();
    }
    if request.think {
        if let Some(budget) = budget {
            body["thinking_budget_tokens"] = json!(budget);
        }
    } else {
        body["reasoning_effort"] = json!("none");
    }
    if let Some(sampling) = request.sampling {
        body["temperature"] = json!(sampling.temperature);
        body["top_p"] = json!(sampling.top_p);
        body["top_k"] = json!(sampling.top_k);
        body["min_p"] = json!(sampling.min_p);
        body["repeat_penalty"] = json!(sampling.repeat_penalty);
    }
    body
}
fn max_tokens(request: &ChatRequest, budget: Option<u32>, room: u32) -> u32 {
    let user = request.sampling.and_then(|s| s.max_tokens).unwrap_or(room);
    let cap = if request.think {
        budget.map_or(user, |b| user.max(b.saturating_add(1024)))
    } else {
        user
    };
    cap.min(room).max(1)
}
fn metadata(options: &Runtime, state: &Completion, round: u32, budget: Option<u32>) -> Value {
    json!({"final_text":if state.calls.is_empty(){state.content.as_str()}else{""},"final_round":if state.calls.is_empty(){round}else{round+1},"history_note":options.services.history_note(),"sources":options.services.sources(),"timings":state.timings,"thinking_budget_tokens":budget,"completion_tokens":state.completion_tokens})
}

pub(crate) async fn start(
    target: ChatTarget,
    request: ChatRequest,
    sink: Arc<dyn ChatSink>,
    options: TurnOptions,
) -> Result<ChatStart, String> {
    let mut messages = chat::validate_request(&request)?;
    if let Some(sampling) = request.sampling {
        sampling.validate()?;
    }
    options.limits.validate()?;
    let speed = speeds()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&options.model_id)
        .copied();
    let budget = if request.think {
        options.thinking_budget.resolve(options.voice, speed)?
    } else {
        None
    };
    let mut system = request
        .system_prompt
        .clone()
        .unwrap_or_else(|| chat::DEFAULT_CHAT_PROMPT.into());
    if system.chars().count() > chat::MAX_SYSTEM_PROMPT_CHARS {
        return Err("The system prompt is too long.".into());
    }
    options.services.allow_user_urls(&messages);
    add_turn_context(&mut system, &mut messages, &options.enabled_tools, options.voice, &chrono::Local::now());
    let mut tools_on = !options.enabled_tools.is_empty();
    let definitions = tools::definitions_for(&options.enabled_tools);
    let runtime = Runtime {
        root: options.backend.root_dir(),
        model_id: options.model_id,
        max_rounds: options.limits.max_rounds,
        enabled_tools: options.enabled_tools,
        ready: Box::new(move || options.backend.is_generation_ready(options.generation)),
        services: options.services,
        #[cfg(test)]
        seed: None,
    };
    let options = runtime;
    let cancel = Arc::new(AtomicBool::new(false));
    let Some(slot) = chat::register_turn(
        &request.request_id,
        sink.clone(),
        Some(cancel.clone()),
        Some(options.root.clone()),
    )?
    else {
        sink.event(
            &request.request_id,
            "assistant:turn-metadata",
            metadata(&options, &Completion::default(), 0, budget),
        );
        sink.done(&request.request_id, None, true);
        return Ok(ChatStart {
            request_id: request.request_id,
            prompt_tokens: 0,
            dropped_messages: 0,
        });
    };
    let client = match chat::http_client() {
        Ok(client) => client,
        Err(error) => {
            slot.finish_turn(
                metadata(&options, &Completion::default(), 0, budget),
                None,
                false,
                Some(&error),
            );
            return Err(error);
        }
    };
    let mut context = Context::new(&system, &messages);
    // Keep answer room to make a fully formed call/answer likely; an explicit thinking
    // budget affects the completion cap, while fitting never deletes the current user.
    let reserve = (if request.think { 8192 } else { 2048 }).min(target.ctx_size / 4);
    let first_fit = checked(
        fit(
            &client,
            &target,
            &mut context,
            &definitions,
            &mut tools_on,
            request.think,
            reserve,
        ),
        &options,
        &cancel,
    )
    .await;
    let prompt_tokens = match first_fit {
        Ok(tokens) => tokens,
        Err(error) => {
            let stopped = cancelled(&options, &cancel);
            slot.finish_turn(
                metadata(&options, &Completion::default(), 0, budget),
                None,
                stopped,
                (!stopped).then_some(error.as_str()),
            );
            return if stopped {
                Ok(ChatStart {
                    request_id: request.request_id,
                    prompt_tokens: 0,
                    dropped_messages: 0,
                })
            } else {
                Err(error)
            };
        }
    };
    let start = ChatStart {
        request_id: request.request_id.clone(),
        prompt_tokens,
        dropped_messages: context.dropped,
    };
    let finish = FinishOnDrop(slot.clone());
    tokio::spawn(async move {
        let _finish = finish;
        run(
            client,
            target,
            request,
            options,
            slot,
            cancel,
            context,
            definitions,
            tools_on,
            budget,
            reserve,
            prompt_tokens,
        )
        .await;
    });
    Ok(start)
}

#[allow(clippy::too_many_arguments)]
/// Every Chat and Voice system message gets today's date, so "today" and "latest" questions
/// are answered for the right day. With tools on it also gets the app's tools policy, and the
/// newest user message gets the time of day. The time stays out of the system message, the
/// start of every prompt: it would change each turn and stop the server reusing any cached
/// history. The date changes once a day; the newest user message is new each turn anyway.
fn add_turn_context<Tz: chrono::TimeZone>(
    system: &mut String,
    messages: &mut [ChatMessage],
    enabled: &ToolSelection,
    voice: bool,
    now: &chrono::DateTime<Tz>,
) where
    Tz::Offset: std::fmt::Display,
{
    system.push_str(&format!("\n\nToday's date: {}.", now.format("%A, %-d %B %Y")));
    if enabled.is_empty() {
        return;
    }
    system.push_str(&tools::prompt_at(voice, &tools::policy_now(now)));
    system.push_str(&enabled.policy());
    if let Some(last) = messages.last_mut() {
        last.text.push_str(&tools::time_note(now));
    }
}

/// Finishes the turn when its task ends without doing so itself: a panic inside a tool or
/// the loop, or the runtime dropping the task. Without it the registry slot and the model's
/// use guard would stay forever, and unload, model switch and app exit would wait on them.
/// `finish_turn` is idempotent, so after a normal completion this does nothing.
struct FinishOnDrop(Arc<Slot>);

impl Drop for FinishOnDrop {
    fn drop(&mut self) {
        self.0.finish_turn(json!({}), None, false, Some(TURN_ABORTED));
    }
}

const TURN_ABORTED: &str = "The Assistant stopped unexpectedly. Try again.";

async fn run(
    client: Client,
    target: ChatTarget,
    request: ChatRequest,
    options: Runtime,
    slot: Arc<Slot>,
    cancel: Arc<AtomicBool>,
    mut context: Context,
    definitions: Value,
    mut tools_on: bool,
    budget: Option<u32>,
    reserve: u32,
    mut prompt_tokens: u32,
) {
    let mut round = 0u32;
    let mut repeats = HashSet::new();
    let mut malformed = 0u32;
    let mut state = Completion::default();
    let mut error: Option<String> = None;
    let mut last_budget = budget;
    loop {
        if cancelled(&options, &cancel) {
            break;
        }
        phase(
            &slot,
            round,
            if request.think {
                "thinking"
            } else {
                "answering"
            },
        );
        if round > 0 {
            match checked(
                fit(
                    &client,
                    &target,
                    &mut context,
                    &definitions,
                    &mut tools_on,
                    request.think,
                    reserve,
                ),
                &options,
                &cancel,
            )
            .await
            {
                Ok(tokens) => prompt_tokens = tokens,
                Err(message) => {
                    error = Some(message);
                    break;
                }
            }
        }
        let room = target.ctx_size.saturating_sub(prompt_tokens);
        let effective_budget = budget.map(|tokens| tokens.min(room.saturating_sub(1024)).max(1));
        last_budget = effective_budget;
        let cap = max_tokens(&request, effective_budget, room);
        #[allow(unused_mut)] // seed is added only by the qualification harness.
        let mut body = body(
            &context,
            tools_on,
            &definitions,
            &request,
            cap,
            effective_budget,
        );
        #[cfg(test)]
        if let Some(seed) = options.seed {
            body["seed"] = json!(seed);
        }
        state = Completion::default();
        if let Err(message) = checked(
            completion(&client, &target, &body, &slot, round, &mut state),
            &options,
            &cancel,
        )
        .await
        {
            error = Some(message);
            break;
        }
        note_speed(&options.model_id, &state.timings);
        // Each call/result pair needs an unambiguous id even if the model duplicated one.
        let mut ids = HashSet::new();
        for call in state.calls.values_mut() {
            if !ids.insert(call.stable_id(round)) {
                call.id = format!("vr_{round}_{}", call.index);
                call.invalid = Some("The model repeated a tool call id.".into());
            }
        }
        let has_tools = !state.calls.is_empty();
        slot.event(
            "assistant:round-ended",
            json!({"round":round,"has_tools":has_tools}),
        );
        // A partial call is never a tool call: ignore all fragments on a length finish.
        if state.finish.as_deref() != Some("tool_calls") && has_tools {
            for call in state.calls.values() {
                call_event(&slot, round, call, "", "done", true);
                result_event(
                    &slot,
                    round,
                    call,
                    &error_outcome("skipped", "The tool call was cut off and was not executed."),
                );
            }
            if !tools_on {
                break;
            }
            context.force_final("A tool call was cut off and was not executed.");
            tools_on = false;
            state = Completion {
                timings: state.timings.clone(),
                completion_tokens: state.completion_tokens,
                ..Completion::default()
            };
            round += 1;
            continue;
        }
        if !has_tools {
            if state.finish.as_deref() == Some("tool_calls") && tools_on {
                context.force_final("The tool request was empty.");
                tools_on = false;
                state = Completion::default();
                round += 1;
                continue;
            }
            break;
        }
        if !tools_on {
            // A template/model ignoring tool omission cannot cause additional execution.
            for call in state.calls.values() {
                result_event(
                    &slot,
                    round,
                    call,
                    &error_outcome("skipped", "Tools are disabled for this final answer."),
                );
            }
            break;
        }
        // Pair every streamed call with a result, including malformed/repeated/skipped ones.
        context.current.push(state.assistant_message(round));
        phase(&slot, round, "tool");
        for call in state.calls.values() {
            call_event(&slot, round, call, "", "queued", true);
            let outcome = if cancelled(&options, &cancel) {
                error_outcome("cancelled", "Cancelled before this tool ran.")
            } else if !options.enabled_tools.contains(&call.name) {
                // Schemas are guidance, not authorization: a model may invent an omitted call.
                // Use the existing retry budget so repeated denied calls force finalization.
                malformed += 1;
                error_outcome("skipped", "This tool is disabled for this turn. Use only an available tool or answer without tools.")
            } else {
                match call.validated() {
                    Err(message) => {
                        malformed += 1;
                        error_outcome("error", &message)
                    }
                    Ok(args) => {
                        let identity = format!("{}:{}", call.name, canonical(&args));
                        if !repeats.insert(identity) {
                            error_outcome(
                                "skipped",
                                "Already done in this turn; use the earlier result.",
                            )
                        } else if malformed > 1 {
                            error_outcome(
                                "skipped",
                                "Too many malformed calls. Answer without tools.",
                            )
                        } else {
                            call_event(&slot, round, call, "", "running", true);
                            // Keep awaiting the service after signalling cancellation: a run
                            // must release its sandbox lease before the terminal event.
                            let future =
                                options.services.execute(&call.name, &args, cancel.clone());
                            tokio::pin!(future);
                            loop {
                                cancelled(&options, &cancel);
                                tokio::select! {
                                    outcome = &mut future => break outcome,
                                    _ = tokio::time::sleep(Duration::from_millis(20)) => {}
                                }
                            }
                        }
                    }
                }
            };
            result_event(&slot, round, call, &outcome);
            call_event(&slot, round, call, "", "done", true);
            context.current.push(json!({"role":"tool","tool_call_id":call.stable_id(round),"name":call.name,"content":outcome.content}));
        }
        if cancelled(&options, &cancel) {
            break;
        }
        state = Completion {
            timings: state.timings.clone(),
            completion_tokens: state.completion_tokens,
            ..Completion::default()
        };
        round += 1;
        if round >= options.max_rounds || malformed > 1 {
            tools_on = false;
        }
    }
    let stopped = cancelled(&options, &cancel);
    slot.finish_turn(
        metadata(&options, &state, round, last_budget),
        state.finish.as_deref(),
        stopped,
        if stopped { None } else { error.as_deref() },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    #[derive(Default)]
    struct Recording {
        events: Mutex<Vec<(String, Value)>>,
    }
    impl Recording {
        fn push(&self, name: &str, value: Value) {
            self.events.lock().unwrap().push((name.into(), value));
        }
        async fn terminal(&self) -> Vec<(String, Value)> {
            for _ in 0..400 {
                let events = self.events.lock().unwrap().clone();
                if events
                    .iter()
                    .any(|(name, _)| name == "done" || name == "error")
                {
                    return events;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("Timed out waiting for a terminal event");
        }
    }
    impl ChatSink for Recording {
        fn event(&self, _: &str, name: &str, payload: Value) {
            self.push(name, payload);
        }
        fn delta(&self, _: &str, content: Option<&str>, reasoning: Option<&str>) {
            self.push("delta", json!({"content":content,"reasoning":reasoning}));
        }
        fn done(&self, _: &str, finish: Option<&str>, stopped: bool) {
            self.push("done", json!({"finish":finish,"stopped":stopped}));
        }
        fn error(&self, _: &str, message: &str) {
            self.push("error", json!(message));
        }
    }
    #[derive(Default)]
    struct Fixtures {
        calls: Mutex<Vec<String>>,
        wait: bool,
        started: AtomicBool,
        ended: AtomicBool,
    }
    impl Services for Fixtures {
        fn execute<'a>(
            &'a self,
            name: &'a str,
            args: &'a Value,
            cancel: Arc<AtomicBool>,
        ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("{name}:{}", canonical(args)));
                self.started.store(true, Ordering::SeqCst);
                if self.wait {
                    while !cancel.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    // Represents a lease/worker being torn down after Stop was signalled.
                    tokio::time::sleep(Duration::from_millis(40)).await;
                }
                self.ended.store(true, Ordering::SeqCst);
                let mut outcome = error_outcome(
                    if cancel.load(Ordering::SeqCst) {
                        "cancelled"
                    } else {
                        "ok"
                    },
                    "Recorded fixture result.",
                );
                outcome.content=json!({"untrusted_tool_data":{"temperature":29,"city":"Singapore","result":391}}).to_string();
                outcome
            })
        }
        fn sources(&self) -> Vec<ToolSource> {
            vec![ToolSource {
                n: 1,
                title: "Fixture".into(),
                url: "https://example.com/fixture".into(),
            }]
        }
        fn history_note(&self) -> String {
            "(recorded fixtures)".into()
        }
    }
    fn runtime(root: &str, max_rounds: u32, fixtures: Arc<Fixtures>) -> Runtime {
        Runtime {
            root: PathBuf::from(root),
            model_id: "fixture-model".into(),
            max_rounds,
            enabled_tools: ToolSelection::all(),
            ready: Box::new(|| true),
            services: fixtures,
            seed: None,
        }
    }
    fn request(id: &str) -> ChatRequest {
        ChatRequest {
            request_id: id.into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                text: "Weather in Singapore?".into(),
                images: vec![],
            }],
            think: false,
            system_prompt: None,
            sampling: None,
        }
    }
    fn target(url: String) -> ChatTarget {
        ChatTarget {
            base_url: url,
            api_key: "test-secret".into(),
            ctx_size: 8192,
            image_token_estimate: 256,
        }
    }
    fn tool_chunk(index: usize, name: &str, args: &str) -> Value {
        json!({"choices":[{"delta":{"reasoning_content":"Current-turn plan.","tool_calls":[{"index":index,"id":format!("call_{index}"),"type":"function","function":{"name":name,"arguments":args}}]},"finish_reason":"tool_calls"}],"timings":{"predicted_per_second":40.},"usage":{"completion_tokens":25}})
    }
    fn answer_chunk() -> Value {
        json!({"choices":[{"delta":{"content":"It is 29 degrees.","reasoning_content":"Answer plan."},"finish_reason":"stop"}],"timings":{"predicted_per_second":40.},"usage":{"completion_tokens":10}})
    }
    async fn read(socket: &mut TcpStream) -> (String, Value) {
        let mut data = vec![];
        let mut buf = [0u8; 4096];
        let end = loop {
            let n = socket.read(&mut buf).await.unwrap();
            if n == 0 {
                return (String::new(), Value::Null);
            }
            data.extend_from_slice(&buf[..n]);
            if let Some(i) = data.windows(4).position(|s| s == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let head = String::from_utf8_lossy(&data[..end]).to_string();
        let length = head
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                    .map(|(_, v)| v.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        while data.len() < end + length {
            let n = socket.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            data.extend_from_slice(&buf[..n]);
        }
        (
            head.split_whitespace().nth(1).unwrap_or("").into(),
            serde_json::from_slice(&data[end..end + length]).unwrap(),
        )
    }
    type Replies = Arc<dyn Fn(usize, &Value) -> Vec<Value> + Send + Sync>;
    struct Mock {
        url: String,
        requests: Arc<Mutex<Vec<(String, Value)>>>,
        count: Arc<AtomicUsize>,
        closed: Arc<AtomicBool>,
    }
    async fn mock(replies: Replies, hang_template: bool) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(vec![]));
        let count = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let (requests_task, count_task, closed_task) =
            (requests.clone(), count.clone(), closed.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let (requests, count, replies, closed) = (
                    requests_task.clone(),
                    count_task.clone(),
                    replies.clone(),
                    closed_task.clone(),
                );
                tokio::spawn(async move {
                    let (path, body) = read(&mut socket).await;
                    requests.lock().unwrap().push((path.clone(), body.clone()));
                    if hang_template && path == "/apply-template" {
                        let mut buf = [0u8; 16];
                        while matches!(socket.read(&mut buf).await,Ok(n) if n>0) {}
                        closed.store(true, Ordering::SeqCst);
                        return;
                    }
                    let (content_type, result) = match path.as_str() {
                        "/apply-template" => (
                            "application/json",
                            json!({"prompt":body.to_string()}).to_string(),
                        ),
                        "/tokenize" => {
                            let n = body["content"].as_str().unwrap().len() / 4 + 1;
                            ("application/json", json!({"tokens":vec![0;n]}).to_string())
                        }
                        "/v1/chat/completions" => {
                            let n = count.fetch_add(1, Ordering::SeqCst);
                            let mut result = String::new();
                            for item in replies(n, &body) {
                                result.push_str(&format!("data: {}\n\n", item));
                            }
                            result.push_str("data: [DONE]\n\n");
                            ("text/event-stream", result)
                        }
                        _ => ("application/json", "{}".into()),
                    };
                    let response=format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{result}",result.len());
                    socket.write_all(response.as_bytes()).await.ok();
                });
            }
        });
        Mock {
            url,
            requests,
            count,
            closed,
        }
    }
    async fn drive(
        id: &str,
        max_rounds: u32,
        mock: &Mock,
        fixtures: Arc<Fixtures>,
    ) -> (Arc<Recording>, Arc<AtomicBool>) {
        drive_selected(id, max_rounds, mock, fixtures, ToolSelection::all()).await
    }
    async fn drive_selected(
        id: &str, max_rounds: u32, mock: &Mock, fixtures: Arc<Fixtures>, selection: ToolSelection,
    ) -> (Arc<Recording>, Arc<AtomicBool>) {
        let sink = Arc::new(Recording::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let mut options = runtime(id, max_rounds, fixtures);
        let definitions = tools::definitions_for(&selection);
        let tools_on = !selection.is_empty();
        options.enabled_tools = selection;
        let slot = chat::register_turn(
            id,
            sink.clone(),
            Some(cancel.clone()),
            Some(options.root.clone()),
        )
        .unwrap()
        .unwrap();
        let request = request(id);
        let context = Context::new("System", &request.messages);
        let target = target(mock.url.clone());
        let client = chat::http_client().unwrap();
        let cancel_task = cancel.clone();
        tokio::spawn(run(
            client,
            target,
            request,
            options,
            slot,
            cancel_task,
            context,
            definitions,
            tools_on,
            None,
            512,
            10,
        ));
        (sink, cancel)
    }
    fn assert_terminal_order(events: &[(String, Value)]) {
        assert_eq!(
            events
                .iter()
                .filter(|(name, _)| name == "done" || name == "error")
                .count(),
            1
        );
        assert!(matches!(
            events.last().unwrap().0.as_str(),
            "done" | "error"
        ));
        assert_eq!(events[events.len() - 2].0, "assistant:turn-metadata");
    }
    #[tokio::test]
    async fn selected_schemas_and_loop_authorization_exclude_disabled_calls() {
        let mock = mock(Arc::new(|n, _| if n == 0 {
            vec![tool_chunk(0,"run_python",r#"{"code":"print(391)"}"#), tool_chunk(1,"get_weather",r#"{"city":"Singapore"}"#)]
        } else { vec![answer_chunk()] }), false).await;
        let fixtures = Arc::new(Fixtures::default());
        let selection = ToolSelection::resolve(Some(true), Some(vec!["get_weather".into()])).unwrap();
        let (sink, _) = drive_selected("selected-tools-mixed", 5, &mock, fixtures.clone(), selection).await;
        let events = sink.terminal().await;
        assert_terminal_order(&events);
        let calls = fixtures.calls.lock().unwrap();
        assert_eq!(calls.len(), 1); assert!(calls[0].starts_with("get_weather:"));
        assert!(events.iter().any(|(n,p)| n == "assistant:tool-result" && p["name"] == "run_python" && p["status"] == "skipped"));
        assert!(!events.iter().any(|(n,p)| n == "assistant:tool-call" && p["name"] == "run_python" && p["phase"] == "running"));
        let requests = mock.requests.lock().unwrap();
        assert!(requests.iter().any(|(path,_)|path == "/apply-template"));
        for (path,body) in requests.iter().filter(|(path,_)|path == "/apply-template" || path == "/v1/chat/completions") {
            let names: Vec<_> = body["tools"].as_array().unwrap().iter().map(|d|d["function"]["name"].as_str().unwrap()).collect();
            assert_eq!(names, vec!["get_weather"], "{path}");
        }
        let last = &requests.iter().filter(|(path,_)|path == "/v1/chat/completions").last().unwrap().1;
        assert_eq!(last["messages"].as_array().unwrap().iter().filter(|m|m["role"]=="tool").count(), 2);
    }
    #[tokio::test]
    async fn empty_selection_omits_schemas_and_never_executes_invented_calls() {
        let mock = mock(Arc::new(|_,_|vec![tool_chunk(0,"run_python",r#"{"code":"print(391)"}"#)]),false).await;
        let fixtures = Arc::new(Fixtures::default());
        let (sink,_) = drive_selected("empty-selection-tools",5,&mock,fixtures.clone(),ToolSelection::default()).await;
        let events = sink.terminal().await;
        assert_terminal_order(&events);
        assert!(fixtures.calls.lock().unwrap().is_empty());
        assert_eq!(mock.count.load(Ordering::SeqCst),1);
        assert!(mock.requests.lock().unwrap().iter().all(|(_,body)|body.get("tools").is_none()));
        assert!(events.iter().any(|(n,p)|n=="assistant:tool-result" && p["status"]=="skipped"));
    }
    #[tokio::test]
    async fn disabled_tool_stays_denied_across_rounds_and_uses_one_retry() {
        let mock = mock(Arc::new(|_,body|if body.get("tools").is_some() {
            vec![tool_chunk(0,"run_python",r#"{"code":"print(391)"}"#)]
        } else { vec![answer_chunk()] }),false).await;
        let fixtures = Arc::new(Fixtures::default());
        let selection = ToolSelection::resolve(Some(true), Some(vec!["get_weather".into()])).unwrap();
        let (sink,_) = drive_selected("disabled-selection-retry",5,&mock,fixtures.clone(),selection).await;
        let events = sink.terminal().await;
        assert_terminal_order(&events);
        assert!(fixtures.calls.lock().unwrap().is_empty());
        assert_eq!(mock.count.load(Ordering::SeqCst),3);
        assert_eq!(events.iter().filter(|(n,p)|n=="assistant:tool-result" && p["status"]=="skipped").count(),2);
        assert!(mock.requests.lock().unwrap().iter().filter(|(path,_)|path=="/v1/chat/completions").last().unwrap().1.get("tools").is_none());
    }
    #[tokio::test]
    async fn round_limits_one_and_five_count_execution_batches_plus_final_request() {
        for limit in [1, 5] {
            let mock = mock(
                Arc::new(|n, body| {
                    if body.get("tools").is_some() {
                        vec![tool_chunk(
                            0,
                            "get_weather",
                            &json!({"city":format!("City {n}")}).to_string(),
                        )]
                    } else {
                        vec![answer_chunk()]
                    }
                }),
                false,
            )
            .await;
            let fixtures = Arc::new(Fixtures::default());
            let id = format!("round-limit-{limit}");
            let (sink, _) = drive(&id, limit, &mock, fixtures.clone()).await;
            let events = sink.terminal().await;
            assert_terminal_order(&events);
            assert_eq!(mock.count.load(Ordering::SeqCst), limit as usize + 1);
            assert_eq!(fixtures.calls.lock().unwrap().len(), limit as usize);
            let requests = mock.requests.lock().unwrap();
            let bodies: Vec<_> = requests
                .iter()
                .filter(|(p, _)| p == "/v1/chat/completions")
                .map(|(_, b)| b)
                .collect();
            assert!(bodies.last().unwrap().get("tools").is_none());
            let messages = bodies.last().unwrap()["messages"].as_array().unwrap();
            assert_eq!(
                messages.iter().filter(|m| m["role"] == "tool").count(),
                limit as usize
            );
            assert!(messages
                .iter()
                .any(|m| m.get("reasoning_content").is_some()));
            assert!(
                requests
                    .iter()
                    .filter(|(p, _)| p == "/apply-template")
                    .any(|(_, b)| b.get("tools").is_some())
                    || limit == 1
            );
        }
    }
    #[tokio::test]
    async fn length_finished_partial_call_is_never_executed_and_forces_finalisation() {
        let mock = mock(
            Arc::new(|n, _| {
                if n == 0 {
                    let mut v = tool_chunk(0, "get_weather", "{\"city\":");
                    v["choices"][0]["finish_reason"] = json!("length");
                    vec![v]
                } else {
                    vec![answer_chunk()]
                }
            }),
            false,
        )
        .await;
        let fixtures = Arc::new(Fixtures::default());
        let (sink, _) = drive("truncated-call", 5, &mock, fixtures.clone()).await;
        let events = sink.terminal().await;
        assert_terminal_order(&events);
        assert!(fixtures.calls.lock().unwrap().is_empty());
        assert_eq!(mock.count.load(Ordering::SeqCst), 2);
        assert!(events
            .iter()
            .any(|(n, p)| n == "assistant:tool-result" && p["status"] == "skipped"));
        let bodies = mock.requests.lock().unwrap();
        assert!(bodies
            .iter()
            .filter(|(p, _)| p == "/v1/chat/completions")
            .last()
            .unwrap()
            .1
            .get("tools")
            .is_none());
        let final_body = &bodies
            .iter()
            .filter(|(p, _)| p == "/v1/chat/completions")
            .last()
            .unwrap()
            .1;
        assert_eq!(
            final_body["messages"].as_array().unwrap().last().unwrap()["role"],
            "user"
        );
    }
    #[tokio::test]
    async fn empty_tool_request_forces_a_new_answer_without_assistant_prefill() {
        let mock = mock(
            Arc::new(|n, body| {
                assert_ne!(
                    body["messages"].as_array().unwrap().last().unwrap()["role"],
                    "assistant"
                );
                if n == 0 {
                    vec![json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})]
                } else {
                    vec![answer_chunk()]
                }
            }),
            false,
        )
        .await;
        let fixtures = Arc::new(Fixtures::default());
        let (sink, _) = drive("empty-tool-request", 5, &mock, fixtures.clone()).await;
        assert_terminal_order(&sink.terminal().await);
        assert_eq!(mock.count.load(Ordering::SeqCst), 2);
        assert!(fixtures.calls.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn malformed_has_one_retry_then_tool_free_answer() {
        let mock = mock(
            Arc::new(|_, body| {
                if body.get("tools").is_some() {
                    vec![tool_chunk(0, "get_weather", "{\"city\":null}")]
                } else {
                    vec![answer_chunk()]
                }
            }),
            false,
        )
        .await;
        let fixtures = Arc::new(Fixtures::default());
        let (sink, _) = drive("malformed-retry", 5, &mock, fixtures.clone()).await;
        let events = sink.terminal().await;
        assert_terminal_order(&events);
        assert_eq!(mock.count.load(Ordering::SeqCst), 3);
        assert!(fixtures.calls.lock().unwrap().is_empty());
        assert_eq!(
            events
                .iter()
                .filter(|(n, p)| n == "assistant:tool-result" && p["status"] == "error")
                .count(),
            2
        );
    }
    #[tokio::test]
    async fn repeated_calls_do_not_execute_twice() {
        let mock = mock(
            Arc::new(|_, body| {
                if body.get("tools").is_some() {
                    vec![tool_chunk(0, "get_weather", "{\"city\":\"Singapore\"}")]
                } else {
                    vec![answer_chunk()]
                }
            }),
            false,
        )
        .await;
        let fixtures = Arc::new(Fixtures::default());
        let (sink, _) = drive("repeat-call", 3, &mock, fixtures.clone()).await;
        let events = sink.terminal().await;
        assert_terminal_order(&events);
        assert_eq!(fixtures.calls.lock().unwrap().len(), 1);
        assert_eq!(
            events
                .iter()
                .filter(|(n, p)| n == "assistant:tool-result" && p["status"] == "skipped")
                .count(),
            2
        );
    }
    #[tokio::test]
    async fn stop_waits_for_tool_teardown_then_emits_metadata_and_one_terminal() {
        let mock = mock(
            Arc::new(|_, _| vec![tool_chunk(0, "get_weather", "{\"city\":\"Singapore\"}")]),
            false,
        )
        .await;
        let fixtures = Arc::new(Fixtures {
            wait: true,
            ..Fixtures::default()
        });
        let (sink, _) = drive("cancel-running-tool", 5, &mock, fixtures.clone()).await;
        while !fixtures.started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        chat::cancel_chat("cancel-running-tool");
        chat::cancel_chat("cancel-running-tool");
        assert!(!fixtures.ended.load(Ordering::SeqCst));
        assert!(!sink.events.lock().unwrap().iter().any(|(n, _)| n == "done"));
        let events = sink.terminal().await;
        assert_terminal_order(&events);
        assert!(fixtures.ended.load(Ordering::SeqCst));
        assert_eq!(events.last().unwrap().1["stopped"], true);
        assert_eq!(mock.count.load(Ordering::SeqCst), 1);
        let metadata = &events[events.len() - 2].1;
        assert_eq!(metadata["final_text"], "");
        assert_eq!(metadata["final_round"], 1);
        assert!(chat::wait_for_backend_turns(
            std::path::Path::new("cancel-running-tool"),
            Duration::ZERO
        ));
    }
    #[tokio::test]
    async fn backend_signal_cancels_fitting_and_never_calls_sink_synchronously() {
        let mock = mock(
            Arc::new(|_, _| vec![tool_chunk(0, "get_weather", "{\"city\":\"Singapore\"}")]),
            true,
        )
        .await;
        let (sink, _) = drive(
            "cancel-fitting-tools",
            5,
            &mock,
            Arc::new(Fixtures::default()),
        )
        .await;
        while !mock
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|(p, _)| p == "/apply-template")
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        chat::cancel_backend_turns(std::path::Path::new("cancel-fitting-tools"));
        let events = sink.terminal().await;
        assert_terminal_order(&events);
        assert_eq!(events.last().unwrap().1["stopped"], true);
        for _ in 0..40 {
            if mock.closed.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(mock.closed.load(Ordering::SeqCst));
        assert_eq!(mock.count.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn context_fitting_evicts_whole_turns_and_never_splits_call_result_pairs() {
        let mock = mock(Arc::new(|_, _| vec![]), false).await;
        let client = chat::http_client().unwrap();
        let mut target = target(mock.url.clone());
        target.ctx_size = 600;
        let messages = vec![
            ChatMessage {
                role: "user".into(),
                text: "older ".repeat(300),
                images: vec![],
            },
            ChatMessage {
                role: "assistant".into(),
                text: "older reply".into(),
                images: vec![],
            },
            ChatMessage {
                role: "user".into(),
                text: "Keep current question".into(),
                images: vec![],
            },
        ];
        let mut context = Context::new("System", &messages);
        context.current.push(json!({"role":"assistant","tool_calls":[{"id":"a","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Singapore\"}"}}],"reasoning_content":"Current plan"}));
        context
            .current
            .push(json!({"role":"tool","tool_call_id":"a","content":"result ".repeat(1000)}));
        let mut enabled = true;
        let count = fit(
            &client,
            &target,
            &mut context,
            &tools::definitions(),
            &mut enabled,
            false,
            64,
        )
        .await
        .unwrap();
        assert!(count + 64 <= 600);
        assert_eq!(context.dropped, 2);
        assert!(context.older.is_empty());
        assert_eq!(context.current.len(), 3);
        assert_eq!(context.current[0]["content"], "Keep current question");
        assert_eq!(
            context.current[1]["tool_calls"][0]["id"],
            context.current[2]["tool_call_id"]
        );
        assert!(!enabled);
    }
    #[test]
    fn every_turn_gets_the_date_and_tool_turns_get_the_time_on_the_user_message() {
        use chrono::TimeZone;
        let tz = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let message = |role: &str, text: &str| ChatMessage { role: role.into(), text: text.into(), images: vec![] };
        let prepare = |hour: u32, minute: u32| {
            let mut system = "You help.".to_string();
            let mut messages = vec![message("user", "Hi"), message("assistant", "Hello"), message("user", "Weather?")];
            let selection = ToolSelection::resolve(Some(true), Some(vec!["get_weather".into()])).unwrap();
            add_turn_context(&mut system, &mut messages, &selection, false, &tz.with_ymd_and_hms(2026, 10, 20, hour, minute, 0).unwrap());
            (system, messages)
        };
        let (morning, messages) = prepare(8, 5);
        let (evening, _) = prepare(21, 40);
        assert_eq!(morning, evening, "the system message is the same all day");
        assert!(morning.contains("2026-10-20"));
        assert!(morning.starts_with("You help.\n\nToday's date: Tuesday, 20 October 2026."));
        assert_eq!(messages[0].text, "Hi", "earlier user messages are unchanged");
        assert_eq!(messages[2].text, "Weather?\n\n(Local time when sent: 08:05)");
        let mut system = "You help.".to_string();
        let mut plain = vec![message("user", "Hi")];
        add_turn_context(&mut system, &mut plain, &ToolSelection::resolve(Some(false), None).unwrap(), true, &tz.with_ymd_and_hms(2026, 10, 20, 8, 5, 0).unwrap());
        assert_eq!(system, "You help.\n\nToday's date: Tuesday, 20 October 2026.", "without tools only the date is added");
        assert_eq!(plain[0].text, "Hi", "and the user message is unchanged");
    }
    #[test]
    fn malformed_arguments_are_replayed_as_an_empty_object() {
        let call = |arguments: &str| Call { name: "get_weather".into(), arguments: arguments.into(), ..Call::default() };
        assert_eq!(call("{\"city\":").wire(0)["function"]["arguments"], "{}");
        assert_eq!(call("").wire(0)["function"]["arguments"], "{}");
        assert_eq!(call("[1]").wire(0)["function"]["arguments"], "{}");
        assert_eq!(call("{\"city\":\"Paris\"}").wire(0)["function"]["arguments"], "{\"city\":\"Paris\"}");
        // Valid JSON that fails the schema is still an object and is kept as written.
        assert_eq!(call("{\"town\":1}").wire(0)["function"]["arguments"], "{\"town\":1}");
    }
    #[tokio::test]
    async fn a_panicking_turn_still_sends_one_terminal_error_and_releases_its_slot() {
        let root = std::env::temp_dir().join(format!("vr-turn-panic-{}", std::process::id()));
        let sink = Arc::new(Recording::default());
        let slot = chat::register_turn("panic-turn", sink.clone(), None, Some(root.clone())).unwrap().unwrap();
        assert!(!chat::wait_for_backend_turns(&root, Duration::from_millis(20)), "the turn is registered");
        let finish = FinishOnDrop(slot.clone());
        let task = tokio::spawn(async move {
            let _finish = finish;
            panic!("a tool panicked");
        });
        assert!(task.await.unwrap_err().is_panic());
        assert!(chat::wait_for_backend_turns(&root, Duration::from_secs(1)), "unload no longer waits on it");
        let events = sink.terminal().await;
        let terminals: Vec<_> = events.iter().filter(|(name, _)| name == "done" || name == "error").collect();
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0], &("error".to_string(), json!(TURN_ABORTED)));
        // A normal finish first makes the guard a no-op.
        let sink = Arc::new(Recording::default());
        let slot = chat::register_turn("normal-turn", sink.clone(), None, None).unwrap().unwrap();
        slot.finish_turn(json!({}), Some("stop"), false, None);
        drop(FinishOnDrop(slot));
        assert_eq!(sink.events.lock().unwrap().iter().filter(|(name, _)| name == "done" || name == "error").count(), 1);
    }
    #[test]
    fn streamed_indices_bounds_and_canonical_arguments_are_preserved() {
        let sink = Arc::new(Recording::default());
        let slot = chat::register_turn("assembly-pure", sink, None, None)
            .unwrap()
            .unwrap();
        let mut state = Completion::default();
        accumulate(&mut state,&json!({"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"city\":"}}]}}]}),&slot,0).unwrap();
        accumulate(&mut state,&json!({"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"get_weather","arguments":"\"Singapore\"}"}},{"index":0,"id":"a","function":{"name":"web_search","arguments":"{\"query\":\"news\"}"}}]}}]}),&slot,0).unwrap();
        assert_eq!(state.calls[&1].validated().unwrap()["city"], "Singapore");
        assert_eq!(
            state.calls.values().map(|c| c.index).collect::<Vec<_>>(),
            vec![0, 1]
        );
        let mut huge = Call {
            arguments: "x".repeat(MAX_ARGUMENT_BYTES + 1),
            ..Call::default()
        };
        huge.invalid = Some("too large".into());
        assert!(huge.validated().is_err());
        assert_eq!(
            canonical(&json!({"b":2,"a":{"z":0,"y":1}})),
            canonical(&json!({"a":{"y":1,"z":0},"b":2}))
        );
        slot.finish_turn(json!({}), None, false, None);
    }
    #[test]
    fn thinking_budget_raises_cap_then_respects_context_and_resets_in_body() {
        let mut request = request("budget");
        request.think = true;
        request.sampling = Some(chat::Sampling {
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            min_p: 0.0,
            repeat_penalty: 1.0,
            max_tokens: Some(128),
        });
        assert_eq!(max_tokens(&request, Some(512), 4096), 1536);
        assert_eq!(max_tokens(&request, Some(512), 1000), 1000);
        let context = Context::new("System", &request.messages);
        let first = body(
            &context,
            true,
            &tools::definitions(),
            &request,
            1536,
            Some(512),
        );
        let next = body(
            &context,
            false,
            &tools::definitions(),
            &request,
            1536,
            Some(512),
        );
        assert_eq!(first["thinking_budget_tokens"], 512);
        assert_eq!(next["thinking_budget_tokens"], 512);
    }
    #[test]
    fn sse_handles_split_unicode_and_rejects_unbounded_events() {
        let mut parser = RawSse::default();
        let bytes = "data: café\n\n".as_bytes();
        let cut = bytes.iter().position(|b| *b == 0xc3).unwrap() + 1;
        assert!(parser.push(&bytes[..cut]).unwrap().is_empty());
        assert_eq!(parser.push(&bytes[cut..]).unwrap(), vec!["café"]);
        assert!(RawSse::default()
            .push(&vec![b'x'; MAX_EVENT_BYTES + 1])
            .is_err());
    }
    /// Run manually for each catalogue model; never grants the tools capability itself.
    /// Required environment: VR_TOOLS_MODEL_ID, VR_TOOLS_MODEL_PATH, VR_TOOLS_SERVER_BIN,
    /// VR_TOOLS_SERVER_SHA256, VR_TOOLS_TEMPLATE_SHA256, VR_TOOLS_SANDBOX_ROOT.
    /// Starts its own loopback-only pinned server. Optional VR_TOOLS_REPORT_PATH saves JSON.
    #[cfg(feature = "build-base")]
    #[tokio::test]
    #[ignore = "requires pinned model/backend files and installed isolated Python runtime; no network tools are contacted"]
    async fn pinned_models_twelve_cases_three_seeds() {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        use std::process::{Child, Command, Stdio};
        fn env(name: &str) -> String {
            std::env::var(name)
                .unwrap_or_else(|_| panic!("Set {name} to run the pinned model evaluation"))
        }
        fn hash(path: &std::path::Path) -> String {
            let mut file = std::fs::File::open(path).unwrap();
            let mut sha = Sha256::new();
            let mut buf = vec![0u8; 1024 * 1024];
            loop {
                let n = file.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                sha.update(&buf[..n]);
            }
            format!("{:x}", sha.finalize())
        }
        struct Server(Child);
        impl Drop for Server {
            fn drop(&mut self) {
                self.0.kill().ok();
                self.0.wait().ok();
            }
        }
        struct Recorded {
            data: Value,
            calls: Mutex<Vec<(String, String)>>,
            sandbox: crate::assistant_sandbox::SandboxState,
        }
        impl Services for Recorded {
            fn execute<'a>(
                &'a self,
                name: &'a str,
                args: &'a Value,
                cancel: Arc<AtomicBool>,
            ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
                Box::pin(async move {
                    self.calls
                        .lock()
                        .unwrap()
                        .push((name.into(), args.to_string()));
                    if name == "run_python" {
                        let result = self
                            .sandbox
                            .run(
                                args["code"].as_str().unwrap_or("").into(),
                                Duration::from_secs(60),
                                cancel,
                            )
                            .await;
                        return match result {
                            Ok(value) => {
                                let data = serde_json::to_value(value).unwrap();
                                let stdout = data["stdout"].as_str().unwrap_or("");
                                let stderr = data["stderr"].as_str().unwrap_or("");
                                let ok = stderr.is_empty()
                                    && ["timed_out", "out_of_memory", "cancelled"]
                                        .iter()
                                        .all(|key| data[*key] != true);
                                ToolOutcome {
                                    status: if ok { "ok" } else { "error" }.into(),
                                    summary: "Isolated Python evaluation".into(),
                                    sources: vec![],
                                    output: Some(format!("{stdout}{stderr}")),
                                    content: json!({"untrusted_tool_data":data}).to_string(),
                                }
                            }
                            Err(error) => error_outcome("error", &format!("{error:#}")),
                        };
                    }
                    ToolOutcome {
                        status: "ok".into(),
                        summary: "Recorded network fixture".into(),
                        sources: self.sources(),
                        output: None,
                        content: self.data["results"][name].to_string(),
                    }
                })
            }
            fn sources(&self) -> Vec<ToolSource> {
                vec![ToolSource {
                    n: 1,
                    title: "Recorded fixture".into(),
                    url: "https://example.com/article".into(),
                }]
            }
            fn history_note(&self) -> String {
                "(recorded network fixtures)".into()
            }
        }
        let fixture: Value =
            serde_json::from_str(include_str!("../tests-data/tools/model-cases.json")).unwrap();
        let model_id = env("VR_TOOLS_MODEL_ID");
        let model = crate::assistant_catalogue::model_by_id(&model_id)
            .expect("Use a pinned catalogue model id");
        let model_path = PathBuf::from(env("VR_TOOLS_MODEL_PATH"));
        assert_eq!(hash(&model_path), model.weights.sha256, "model pin differs");
        let server_bin = PathBuf::from(env("VR_TOOLS_SERVER_BIN"));
        let server_hash = hash(&server_bin);
        assert_eq!(
            server_hash,
            env("VR_TOOLS_SERVER_SHA256").to_lowercase(),
            "server binary pin differs"
        );
        let version = Command::new(&server_bin).arg("--version").output().unwrap();
        let version = format!(
            "{}{}",
            String::from_utf8_lossy(&version.stdout),
            String::from_utf8_lossy(&version.stderr)
        );
        assert!(
            version.contains("11718"),
            "the evaluation requires llama-server b11718"
        );
        assert_eq!(
            fixture["server_build"],
            crate::assistant_catalogue::BACKEND_VERSION
        );
        let sandbox_root = PathBuf::from(env("VR_TOOLS_SANDBOX_ROOT"));
        assert!(
            crate::assistant_sandbox::SandboxState::new(sandbox_root.clone())
                .status()
                .installed,
            "Install the isolated code runner before evaluating Python"
        );
        let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = socket.local_addr().unwrap().port();
        drop(socket);
        let log_path =
            std::env::temp_dir().join(format!("voicereader-tools-eval-{}.log", std::process::id()));
        let log = std::fs::File::create(&log_path).unwrap();
        let mut server = Server(
            Command::new(&server_bin)
                .args([
                    "--model",
                    model_path.to_str().unwrap(),
                    "--jinja",
                    "--reasoning-format",
                    "deepseek",
                    "--parallel",
                    "1",
                    "--ctx-size",
                    "32768",
                    "--host",
                    "127.0.0.1",
                    "--port",
                    &port.to_string(),
                    "--api-key",
                    "voicereader-evaluation-only",
                    "--no-reasoning-preserve",
                ])
                .stdout(Stdio::from(log.try_clone().unwrap()))
                .stderr(Stdio::from(log))
                .spawn()
                .unwrap(),
        );
        let client = chat::http_client().unwrap();
        let base_url = format!("http://127.0.0.1:{port}");
        let deadline = std::time::Instant::now() + Duration::from_secs(180);
        loop {
            if let Ok(reply) = client
                .get(format!("{base_url}/health"))
                .bearer_auth("voicereader-evaluation-only")
                .send()
                .await
            {
                if reply.status().is_success() {
                    break;
                }
            }
            assert!(
                server.0.try_wait().unwrap().is_none(),
                "Pinned server exited; inspect {}",
                log_path.display()
            );
            assert!(
                std::time::Instant::now() < deadline,
                "Pinned server never became ready; inspect {}",
                log_path.display()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let props: Value = client
            .get(format!("{base_url}/props"))
            .bearer_auth("voicereader-evaluation-only")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let template = props["chat_template"]
            .as_str()
            .expect("server /props must expose its chat_template");
        let template_hash = format!("{:x}", Sha256::digest(template.as_bytes()));
        assert_eq!(
            template_hash,
            env("VR_TOOLS_TEMPLATE_SHA256").to_lowercase(),
            "template pin differs"
        );
        let mut records = vec![];
        let mut all_pass = true;
        for voice in [false, true] {
            let mut passes = 0;
            let mut hard_pass = true;
            for case in fixture["cases"].as_array().unwrap() {
                let mut seeds_pass = 0;
                for seed in [1, 2, 3] {
                    let id = format!(
                        "real-{}-{}-{voice}-{seed}",
                        model.id,
                        case["id"].as_str().unwrap()
                    );
                    let services = Arc::new(Recorded {
                        data: fixture.clone(),
                        calls: Mutex::new(vec![]),
                        sandbox: crate::assistant_sandbox::SandboxState::new(sandbox_root.clone()),
                    });
                    let mut options = Runtime {
                        root: std::env::temp_dir().join(&id),
                        model_id: model.id.into(),
                        max_rounds: 5,
                        enabled_tools: ToolSelection::all(),
                        ready: Box::new(|| true),
                        services: services.clone(),
                        seed: Some(seed),
                    };
                    let mut request = request(&id);
                    request.messages[0].text = if voice {
                        case.get("voice_prompt").unwrap_or(&case["prompt"])
                    } else {
                        &case["prompt"]
                    }
                    .as_str()
                    .unwrap()
                    .into();
                    request.sampling = Some(chat::Sampling {
                        temperature: model.sampling.temperature,
                        top_p: model.sampling.top_p,
                        top_k: model.sampling.top_k,
                        min_p: model.sampling.min_p,
                        repeat_penalty: model.sampling.repeat_penalty,
                        max_tokens: Some(4096),
                    });
                    let system = format!(
                        "{}{}",
                        if voice {
                            chat::DEFAULT_VOICE_PROMPT
                        } else {
                            chat::DEFAULT_CHAT_PROMPT
                        },
                        tools::prompt_at(voice, fixture["policy_time"].as_str().unwrap())
                    );
                    let mut context = Context::new(&system, &request.messages);
                    let target = target(base_url.clone());
                    let target = ChatTarget {
                        ctx_size: 32768,
                        api_key: "voicereader-evaluation-only".into(),
                        ..target
                    };
                    let definitions = tools::definitions();
                    let mut tools_on = true;
                    let prompt_tokens = fit(
                        &client,
                        &target,
                        &mut context,
                        &definitions,
                        &mut tools_on,
                        false,
                        2048,
                    )
                    .await
                    .unwrap();
                    let sink = Arc::new(Recording::default());
                    let cancel = Arc::new(AtomicBool::new(false));
                    let slot = chat::register_turn(
                        &id,
                        sink.clone(),
                        Some(cancel.clone()),
                        Some(options.root.clone()),
                    )
                    .unwrap()
                    .unwrap();
                    options.max_rounds = 5;
                    tokio::time::timeout(
                        Duration::from_secs(180),
                        run(
                            client.clone(),
                            target,
                            request,
                            options,
                            slot,
                            cancel,
                            context,
                            definitions,
                            tools_on,
                            None,
                            2048,
                            prompt_tokens,
                        ),
                    )
                    .await
                    .expect("model evaluation turn timed out");
                    let events = sink.terminal().await;
                    assert_terminal_order(&events);
                    let metadata = &events[events.len() - 2].1;
                    let answer = metadata["final_text"].as_str().unwrap_or("");
                    let calls = services.calls.lock().unwrap().clone();
                    let expected_tool = if voice {
                        case.get("voice_tool").unwrap_or(&case["tool"])
                    } else {
                        &case["tool"]
                    }
                    .as_str()
                    .unwrap_or("");
                    let successful = events.iter().any(|(name, p)| {
                        name == "assistant:tool-result"
                            && p["name"] == expected_tool
                            && p["status"] == "ok"
                    });
                    let lower = answer.to_lowercase();
                    let passed =
                        events.last().unwrap().0 == "done"
                            && !answer.trim().is_empty()
                            && match case["expect"].as_str().unwrap() {
                                "clarify_city" => {
                                    calls.is_empty()
                                        && ["city", "location", "where", "place"]
                                            .iter()
                                            .any(|word| lower.contains(word))
                                }
                                "no_tool" => calls.is_empty(),
                                "delayed_stock" => {
                                    successful
                                        && (lower.contains("delay")
                                            || lower.contains("real-time")
                                            || lower.contains("real time"))
                                }
                                _ => successful,
                            }
                            && (case["id"] != "python"
                                || events.iter().any(|(name, p)| {
                                    name == "assistant:tool-result"
                                        && p["name"] == "run_python"
                                        && p["status"] == "ok"
                                        && p["output"].as_str().unwrap_or("").lines().any(|line| {
                                            line.trim().replace(',', "") == "333383335000"
                                        })
                                }));
                    // Hard structural invariants hold on every seed, independently of score.
                    let executed = events
                        .iter()
                        .filter(|(name, p)| name == "assistant:tool-result" && p["status"] == "ok")
                        .collect::<Vec<_>>();
                    assert!(executed.len() <= 5 * MAX_CALLS);
                    for (_, result) in executed {
                        assert!(
                            events.iter().any(|(name, p)| name == "assistant:tool-call"
                                && p["call_id"] == result["call_id"]
                                && p["round"] == result["round"]
                                && p["phase"] == "running"),
                            "A partial/unvalidated call reached execution"
                        );
                    }
                    if passed {
                        seeds_pass += 1;
                    }
                    records.push(json!({"model":model.id,"voice":voice,"case":case["id"],"seed":seed,"passed":passed,"calls":calls,"tool_results":events.iter().filter(|(name,_)|name=="assistant:tool-result").map(|(_,payload)|payload).collect::<Vec<_>>(),"final_text":answer}));
                }
                let passed = seeds_pass >= 2;
                if passed {
                    passes += 1;
                }
                if case["hard"] == true && !passed {
                    hard_pass = false;
                }
            }
            all_pass &= passes >= 11 && hard_pass;
            eprintln!(
                "{} {}: {passes}/12, hard invariants {hard_pass}",
                model.id,
                if voice { "voice" } else { "chat" }
            );
        }
        let policy_time = fixture["policy_time"].as_str().unwrap();
        let report = json!({"protocol":fixture["protocol"],"model":model.id,"model_sha256":model.weights.sha256,"model_revision":model.revision,"server_build":"b11718","server_sha256":server_hash,"template_sha256":template_hash,"fixture_sha256":format!("{:x}",Sha256::digest(include_bytes!("../tests-data/tools/model-cases.json"))),"tools_sha256":format!("{:x}",Sha256::digest(tools::definitions().to_string().as_bytes())),"chat_prompt_sha256":format!("{:x}",Sha256::digest(format!("{}{}",chat::DEFAULT_CHAT_PROMPT,tools::prompt_at(false,policy_time)).as_bytes())),"voice_prompt_sha256":format!("{:x}",Sha256::digest(format!("{}{}",chat::DEFAULT_VOICE_PROMPT,tools::prompt_at(true,policy_time)).as_bytes())),"sampling":model.sampling,"thinking":false,"qualified":all_pass,"records":records});
        if let Ok(path) = std::env::var("VR_TOOLS_REPORT_PATH") {
            std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        } else {
            eprintln!("{}", serde_json::to_string_pretty(&report).unwrap());
        }
        assert!(
            all_pass,
            "Model did not meet 11/12 plus the hard invariants. Capability remains unverified."
        );
    }
}
