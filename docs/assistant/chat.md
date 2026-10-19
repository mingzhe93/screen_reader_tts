# Assistant chat bridge (`assistant_chat.rs`)

Status: Step 1 of v0.3.0, Rust side only. Written on macOS; nothing here has been run against a real llama-server or on Windows yet (see "Manual checks").

## What it does

`src-tauri/src/assistant_chat.rs` sends a chat conversation to a running llama-server (OpenAI-compatible API), fits it into the context window, streams the answer back through a callback trait and supports cancellation. It implements `docs/v0.3.0_assistant_plan.md`:

- section 3.2 (chat page: Stop keeps the text received so far, Think toggle, up to 4 images per message),
- section 5.4 (reasoning is never sent back; `reasoning_effort: none` when Think is off),
- section 6.3 (token counting with `/apply-template` and `/tokenize`, answer reserve, oldest-pair eviction, refusal of an oversized message; Codex finding F1).

The module does not depend on Tauri or on the backend (process manager) module. A caller builds a `ChatTarget` from the backend's endpoint and implements `ChatSink` with Tauri events. The two Tauri command wrappers and the event emission are not in this module.

It is compiled only with the `build-base` feature. The `mod assistant_chat;` line in `lib.rs` is added by the integrator. Without the Tauri commands in the build, the compiler reports `dead_code` warnings for it; they disappear once the commands call it.

No new dependencies: `reqwest` 0.11, `serde`, `serde_json`, `tokio`, `anyhow`, and `futures-util` (already in `Cargo.toml`) for `Abortable`.

## Public interface (all `pub(crate)`)

```rust
struct ChatTarget { base_url: String, api_key: String, ctx_size: u32, image_token_estimate: u32 }
struct ChatMessage { role: String, text: String, images: Vec<String> }   // Deserialize, Clone
struct ChatRequest { request_id: String, messages: Vec<ChatMessage>, think: bool }
struct ChatStart { request_id: String, prompt_tokens: u32, dropped_messages: u32 } // Serialize
trait ChatSink: Send + Sync + 'static {
    fn delta(&self, request_id: &str, content: Option<&str>, reasoning: Option<&str>);
    fn done(&self, request_id: &str, finish_reason: Option<&str>, stopped: bool);
    fn error(&self, request_id: &str, message: &str);
}
async fn start_chat(target: ChatTarget, request: ChatRequest, sink: Arc<dyn ChatSink>) -> Result<ChatStart, String>;
fn cancel_chat(request_id: &str);
```

`ChatTarget` has no `Debug` on purpose so the API key cannot be logged by accident. `ChatMessage.images` holds data URLs from the frontend. Serialized fields use snake_case (project convention).

### `start_chat`

1. Validates the request (below). Failures return `Err` with a sentence the UI can show as is; nothing is sent to the sink.
2. Registers the request id for cancellation. A second request with an id that is still running returns `Err("A request with this id is already running.")`.
3. Counts tokens and evicts old messages (context fitting, below). The server being down gives `Err("Could not reach the assistant server. Check that it is running.")`.
4. Spawns a tokio task that opens the stream and returns `Ok(ChatStart)`. The sink receives output only after this returns.

Cancellation covers the whole startup. The token-counting phase runs inside an `Abortable` whose handle is stored in the request slot before the first HTTP call. If `cancel_chat` runs while `/apply-template` or `/tokenize` is in flight, the sink receives `done(stopped = true)`, the counting request is dropped (its connection closes), no further counting requests are sent, and `start_chat` resolves promptly with `Ok` and `prompt_tokens: 0, dropped_messages: 0`, starting nothing. A second handle replaces the first once the stream task is spawned. The caller should not treat that as an error.

### `cancel_chat`

Marks the request finished, calls `sink.done(id, None, true)` and aborts the streaming task, which drops the HTTP response. llama-server stops generating when the client disconnects. An already finished id is a no-op. An id that is not registered yet is remembered (up to 64), because Tauri runs the async start command on a spawned task and the sync cancel command directly, so Stop can arrive first. A later `start_chat` for that id then sends `done(stopped = true)`, returns `Ok` with zero counts, and starts nothing (Codex re-review R1).

### Sink guarantees

Exactly one of `done` or `error` is emitted per request and nothing is emitted after it, even when a cancel races the stream. Every sink call is made while holding a per-request lock, so a sink implementation must not call `cancel_chat` or `start_chat` from inside `delta`, `done` or `error` (it would deadlock). Emitting a Tauri event from them is fine.

If the stream task panics, no terminal event is sent (not handled; see gaps).

## Validation

- `request_id` non-empty; at least one message; the last message has role `user`.
- Roles are `user` or `assistant`; anything else (including `system`) is rejected.
- At most 4 images per message; only user messages may carry images.
- Each image must be `data:image/png|jpeg|webp;base64,<payload>` with a non-empty base64 payload (standard or URL-safe alphabet). Anything else is rejected: "Only PNG, JPEG or WebP images can be attached."
- Earlier messages with neither text nor images are dropped silently. An empty final message is an error ("Type a message before sending.").

## Wire format

`POST {base_url}/v1/chat/completions`, header `Authorization: Bearer <api_key>`:

```json
{"model": "local", "messages": [...], "stream": true,
 "chat_template_kwargs": {"enable_thinking": true},
 "max_tokens": 3500}
```

- `reasoning_effort: "none"` is added only when `think` is false.
- `max_tokens = ctx_size - prompt_tokens` (at least 1). `prompt_tokens` includes the image estimates.
- The first message is a system message from the constant `SYSTEM_PROMPT` (short, neutral).
- A message without images has `"content": "<text>"`. With images it is `[{"type":"text","text":"..."}, {"type":"image_url","image_url":{"url":"<data url>"}}, ...]`.
- The HTTP client ignores system proxy settings (`no_proxy`) so the API key never goes through a proxy, and uses a 5 s connect timeout. The stream has no overall timeout; the two counting calls have 30 s.

### Streaming (Server-Sent Events)

`SseParser` buffers bytes until a full line arrives, so JSON split across HTTP chunks, several events in one chunk, `\r\n` line endings and multi-byte characters split between chunks are all handled.

- `data: {json}` lines end an event at a blank line; `data: [DONE]` ends the stream; lines starting with `:` are ignored; other fields (`event:`, `id:`, `retry:`) are ignored.
- From `choices[0].delta` the module forwards `content` and `reasoning_content` (llama-server puts reasoning there with `--reasoning-format deepseek`) in one `sink.delta` call per event; empty and `null` values are skipped. `choices[0].finish_reason` is remembered and passed to `done`.
- An `{"error": ...}` object in an event (string or `{message}`) becomes `sink.error` with the server's message as is.
- A non-2xx HTTP status becomes `sink.error("The assistant server returned HTTP <status>: <server message>")`.
- A connection that closes before `[DONE]` becomes `sink.error("The assistant stopped unexpectedly.")`. After a cancel nothing more is emitted.
- Unparseable JSON in an event becomes `sink.error("The assistant sent a response that could not be read.")`.

## Context fitting (plan section 6.3, Codex F1)

1. The prompt is built as text: `POST /apply-template` with `{"messages": [...]}` (system message plus the conversation, images stripped), then `POST /tokenize` with `{"content": "<prompt>"}`. The count is `tokens.len()` plus `image_token_estimate` for every attached image. Both calls send the bearer header.
2. `reserve = min(think ? 8192 : 2048, ctx_size / 4)`.
3. While `prompt_tokens + reserve > ctx_size`, the oldest non-system message is dropped. A user message is dropped together with the assistant reply that follows it. The final user message is never dropped. The prompt is recounted after each drop (two HTTP calls per drop; conversations are small).
4. If only the system message and the final message remain and they still do not fit, `start_chat` returns: "This message is too long for the current context (about N tokens; the limit for a message is M). Shorten it or raise the context length in Assistant settings." N is the token count including image estimates and M is `ctx_size - reserve`.
5. `ChatStart.dropped_messages` counts every evicted message. The UI should tell the user when it is above zero, because the model no longer sees those turns.

The eviction logic is generic over a small `TokenCounter` trait, so unit tests use a fake counter and the real one (`HttpCounter`) only talks HTTP.

Caveat: `/apply-template` renders with the server's default template settings, so the count assumes the same template as the real request. It is an estimate for thinking models, whose template may add tokens depending on `enable_thinking`; the reserve absorbs the difference.

## Tests

All in-module, no network and no real model:

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"   # macOS dev machine only
cd src-tauri
cargo test --features build-base assistant_chat::
```

On Windows use a normal Rust toolchain: `cd src-tauri; cargo test --features build-base assistant_chat::`.

Result when written: 30 passed, 0 ignored (about 0.3 s once built). Coverage:

- SSE parser: split JSON, several events per chunk, comments, `[DONE]`, reasoning and content, null and empty deltas, split UTF-8, CRLF, error events, bad JSON.
- Request body: text only, images, Think on and off.
- Validation and image data URLs.
- Eviction with a fake counter: fits, drops oldest pairs, lone assistant drops, final user message kept, oversized message error text with numbers, image estimates, Think reserve.
- Mock server (a `tokio::net::TcpListener` written in the test) answering `/apply-template`, `/tokenize` and a streamed `/v1/chat/completions`: ordered deltas then one `done`; stream closing without `[DONE]`; HTTP 503 body; `error` event; cancel mid-stream (one `done(stopped=true)`, nothing after, and the mock sees the client hang up); cancel while `/apply-template` never answers (uses only `cancel_chat`: `start_chat` resolves within 1 s, the mock sees the connection close, one `done(stopped=true)`, no further requests); oversized message refused before any chat request; duplicate id; unreachable server.

The tests share one global request registry, so every test uses its own request id.

## Manual checks

Run these against a real llama-server (any small GGUF). Start it with an API key, for example `llama-server -m model.gguf -c 8192 --port 8080 --api-key testkey --reasoning-format deepseek`.

The endpoints this module relies on are `/apply-template`, `/tokenize` and `/v1/chat/completions`. Confirm each with the commands below before building on them.

macOS (zsh):

```bash
H='Authorization: Bearer testkey'
curl -s localhost:8080/apply-template -H "$H" -H 'Content-Type: application/json' \
  -d '{"messages":[{"role":"system","content":"Be brief."},{"role":"user","content":"Hi"}]}'
# expect: {"prompt":"..."}
curl -s localhost:8080/tokenize -H "$H" -H 'Content-Type: application/json' -d '{"content":"Hello world"}'
# expect: {"tokens":[...]}
curl -N localhost:8080/v1/chat/completions -H "$H" -H 'Content-Type: application/json' \
  -d '{"model":"local","stream":true,"max_tokens":64,"reasoning_effort":"none","chat_template_kwargs":{"enable_thinking":false},"messages":[{"role":"user","content":"Say hi"}]}'
# expect: lines "data: {...}" with choices[0].delta.content, then "data: [DONE]"
```

Windows (PowerShell). Write the body to a file to avoid quoting problems, and use `curl.exe` (plain `curl` is an alias of `Invoke-WebRequest`):

```powershell
$h = 'Authorization: Bearer testkey'
'{"messages":[{"role":"system","content":"Be brief."},{"role":"user","content":"Hi"}]}' | Set-Content -NoNewline body1.json
curl.exe -s http://localhost:8080/apply-template -H $h -H "Content-Type: application/json" --data-binary "@body1.json"
'{"content":"Hello world"}' | Set-Content -NoNewline body2.json
curl.exe -s http://localhost:8080/tokenize -H $h -H "Content-Type: application/json" --data-binary "@body2.json"
'{"model":"local","stream":true,"max_tokens":64,"reasoning_effort":"none","chat_template_kwargs":{"enable_thinking":false},"messages":[{"role":"user","content":"Say hi"}]}' | Set-Content -NoNewline body3.json
curl.exe -N http://localhost:8080/v1/chat/completions -H $h -H "Content-Type: application/json" --data-binary "@body3.json"
```

Same expectations as macOS. Things to verify by hand once the Tauri commands exist (on both platforms):

1. Thinking on: reasoning arrives as `reasoning_content` deltas before `content` (needs `--reasoning-format deepseek`). Thinking off with `reasoning_effort: none`: no reasoning deltas.
2. Stop: press Stop mid-answer. The UI gets exactly one `done` with `stopped = true`, and the llama-server log shows the request ending (the slot is released) rather than generating to the end.
3. Long conversation: with `-c 2048`, paste long messages until `dropped_messages > 0`, then one message larger than the limit. Expect the "too long" sentence with plausible numbers.
4. Wrong or missing API key: expect `HTTP 401` in the message, with no key in it. Server stopped: expect "Could not reach the assistant server".
5. On Windows, check that a long answer (several thousand tokens) streams without stalls and that Stop works with the Windows firewall prompt dismissed (loopback only).
6. Images: attach a PNG to a vision-capable model (with its `--mmproj`) and confirm the answer refers to it; compare the real image token cost in the server log with `image_token_estimate`.

## Known gaps and next steps

- `image_token_estimate` is a placeholder supplied by the caller; the real per-model cost is not measured. Image size is not checked here (the frontend downscales, plan section 3.2).
- Token counting uses default template settings, not the request's `enable_thinking` value (see caveat above).
- Conversation mode (no reasoning, spoken style system prompt, `reasoning_effort: none` on every request, interruption markers) is Step 3. The system prompt is a single constant for the chat page.
- No retry and no automatic reconnect; a failed request is reported once and the user resends.
- A panic inside the stream task would leave the UI without a terminal event. The task body does no `unwrap`s, so this is unlikely, but there is no watchdog.
- No overall stall timeout while streaming: a server that stops sending without closing the connection hangs until the user presses Stop.
- The Tauri wrappers, the event names and the frontend page (`src/`) are outside this module.
