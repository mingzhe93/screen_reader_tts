# Assistant chat and turn loop

Updated 2026-10-19. The base Assistant is committed in `5e62732` and I have tested it working on macOS. Chat and Voice now share the tools-phase turn loop, including when tools are disabled. I have tested and accepted Chat/Voice with tools on macOS; Windows validation remains outstanding. See [tools.md](tools.md) for tool policy, permissions, runtime limits and qualification.

## Code and responsibilities

- `assistant_chat.rs`: request validation, public message/sampling types, safe loopback HTTP helpers, `ChatSink` callbacks and the whole-turn registry. The old single-completion implementation is retained only under `cfg(test)` as a baseline regression fixture.
- `assistant_tool_loop.rs`: internal wire messages, context fitting, bounded SSE/tool-call assembly, thinking budgets and the model/tool loop.
- `assistant_commands.rs`: Tauri chat commands and event sink.
- `assistant_conversation.rs`: Voice sink and ordered speech worker, using that same loop.

All are Base-only. `ChatTarget` has no `Debug` so the server key cannot accidentally appear in logs. The HTTP client uses `no_proxy()` for authenticated loopback calls.

## Command and event contracts

`assistant_chat_start` accepts camelCase invoke arguments: `requestId`, `messages`, `think`, optional `sampling`, `conversationId`, `tools`, `enabledTools`, tagged `thinkingBudget`, and `toolLimits`. Voice adds the same options to `assistant_voice_respond`. `enabledTools` is the per-turn selected-name list: legacy `tools: true` without a list enables all built-ins, while an explicit empty list enables none. Both schema generation and execution enforce the snapshot. Messages are `{ role: "user"|"assistant", text, images: string[] }`, ending in the new user message. Rust applies the saved system prompt; the frontend cannot supply tool messages or a replacement system message.

The backend atomically admits the turn and holds a use guard until its terminal callback. `start_chat_with_options` validates, registers, fits the first prompt and starts the stream task. It returns `ChatStart { request_id, prompt_tokens, dropped_messages }`. Early validation failures reject the command. A registered fitting/connection failure sends terminal metadata/error as well as rejecting startup; Voice's sink claims terminal emission once, so its command wrapper cannot duplicate the error.

`ChatSink` supplies `delta`, `done`, `error` and `event`. Each model delta carries `round` in the Tauri event. Tool-call fragments/results and turn phases arrive through `event`. Before the terminal callback, private `assistant:turn-metadata` supplies `final_text`, `final_round`, `history_note`, `sources`, timings and the effective thinking budget; the Tauri sink merges these fields into `assistant:chat-done` or `assistant:chat-error`.

Tool call events carry a stable `call_index` before the model's id/name arrive; `delta` means append, while `arguments` on completed phases is the full string. Phases are `streaming`, `queued`, `running`, `done`. Results carry status, summary, sources and optional code output. See [tools.md](tools.md) and the types in `src/types.ts`.

## Validation and counting

Request ids cannot be empty. Only user/assistant roles are accepted; the last message must be a nonempty user message (text or image). Earlier empty messages are omitted. Only user messages can include images; at most four PNG/JPEG/WebP base64 data URLs per message. Sampling values and thinking/tool limits are validated in Rust.

An internal JSON wire-message representation includes current-turn assistant `tool_calls`/`reasoning_content` and matching `tool` results. Every round calls `/apply-template` with actual messages, tool definitions when enabled and thinking configuration, then `/tokenize` on the rendered prompt. Images add the catalogue's conservative per-image estimate rather than sending large data URLs through the text tokenizer.

The fitter evicts whole older turns first. It never drops the current user question or separates a call/result pair. If necessary it truncates the oldest current-turn tool results and forces a tool-free answer; if the protected conversation still cannot fit, it returns an actionable error. Answer reserve is 8,192 tokens with thinking or 2,048 without, capped at one quarter of context. Completion tokens are also bounded by actual remaining room. Finite thinking raises the answer cap to at least budget + 1,024 where context permits, resetting per HTTP round.

## Streaming and recovery

SSE is assembled from byte chunks without splitting UTF-8 text; event, per-round text and tool-argument sizes are bounded. The parser uses OpenAI-compatible structured `tool_calls` supplied by llama.cpp; it does not execute XML or JSON found in prose. Calls are assembled by index, strictly validated before execution and paired with a result even when skipped, malformed or cancelled.

N execution batches are followed by a final request without tool definitions. Repeated canonical calls execute once, malformed arguments get one retry, and a length-truncated call never executes. A cut-off/empty call's recovery instruction is added to the app's system policy, never as a trailing assistant prefill. A real b11718 probe confirmed that a trailing assistant recovery message can be echoed instead of answered.

Reasoning and raw results are retained between rounds of the current turn only. Earlier-turn history contains the final answer plus a short tools/source note. Model preambles are visible/spoken, but only the final round is stored as the answer.

## Cancellation and backend lifecycle

One registry slot owns the whole turn, not each HTTP request. Early Stop is remembered for up to 64 not-yet-registered ids. Stop during fitting/completion drops the HTTP future and closes its connection; during a tool it signals the shared cancellation flag and awaits teardown before terminal emission. Exactly one terminal callback is claimed under the slot lock. A cancellation observed before that claim overrides a previously computed completion state.

Turns are bound to backend root and generation. Admission snapshots endpoint, generation and use guard atomically. Unload, replacement, crash and exit signal registered turns; a stale turn cannot address a replacement server. Backend cancellation only signals flags while holding its own lock; draining happens outside that lock because terminal sinks release a use guard that takes it.

A sink must not recursively call chat registration/cancellation from a callback: callbacks run under a per-slot lock. Emitting Tauri events is safe. Tools must finish cleanup before their final result/terminal event. A first Python-module compilation cannot be interrupted; unload retains ownership until it drains rather than silently returning after a timeout.

## Verification

Unit/integration checks exercise validation, real template-count transport, bounded SSE assembly, whole-turn fitting, exact round limits, repeat/malformed/truncated guards, fitting/tool cancellation, terminal races and real backend generation transitions. Network-tool, sandbox, UI and voice checks are recorded in [tools.md](tools.md). The real-model harness pins weights/server/template/prompt/fixtures and tests both chat and spoken prompts; it is distinct from actual microphone/TTS playback testing.
