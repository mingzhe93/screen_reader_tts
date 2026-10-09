VoiceReader Engine API (Python sidecar, Full build only)

> **This API applies to the Full build (`build-full`) only.** The Base build (`build-base`, the default) has no sidecar and no localhost API. Kyutai Pocket TTS, Audio8 TTS and transcription run inside the Rust process, and the Rust code talks to the frontend through Tauri commands and events. The Base build's interface is those commands and events, described in `docs/DESIGN_SPEC.md` (section 6, and section 15 for transcription). The sidecar has no transcription endpoint. The Full build is kept for future heavier models and is not actively used.
>
> This document describes `tts-engine/src/tts_engine/` (`app.py`, `schemas.py`, `jobs.py`, `auth.py`, `errors.py`, `chunking.py`, `config.py`) for engine version 0.2.2. This release changes the version metadata; the sidecar API contract is unchanged.

## 1. Overview
The Python sidecar exposes an HTTP and WebSocket API. The desktop app starts it as a child process.

- Transport:
  - HTTP for control requests
  - WebSocket for streamed job events and audio chunks
- Bind:
  - Loopback only (`127.0.0.1` by default; the app passes a free port)
- Auth:
  - Bearer token required for HTTP and WS
- Start-up:
  - `python -m tts_engine --server` (or the bundled `tts-engine` executable) with `--port` (default 8765), `--host` (default `127.0.0.1`), `--data-dir`, and the token from `--token`, the `SPEAK_SELECTION_ENGINE_TOKEN` environment variable (`--token-env` names another variable) or a JSON object on stdin with `--bootstrap-stdin` (`token`, `port`, `data_dir`)
  - The desktop app passes `--server --port <free port> --data-dir <dir>` and sets the token in the environment

## 2. Auth Policy

### 2.1 HTTP
All HTTP requests require:

```text
Authorization: Bearer <token>
```

A missing or wrong token returns `401 UNAUTHORIZED`.

### 2.2 WebSocket
Preferred:

```text
Authorization: Bearer <token>
```

Fallback for clients that cannot set auth headers:

```text
Sec-WebSocket-Protocol: auth.bearer.v1, <token>
```

If valid, server accepts and returns:

```text
Sec-WebSocket-Protocol: auth.bearer.v1
```

If the token is invalid the server closes with code `4401`. If the job does not exist it closes with code `4404`. The desktop app uses the subprotocol form.

## 3. Conventions

### 3.1 Content types
- Requests and responses: `application/json`
- Audio stream frames: JSON with base64 PCM (`pcm_s16le`)

### 3.2 IDs
- `job_id`: UUID
- `voice_id`:
  - `"0"` reserved built-in voice
  - UUID for cloned voices

### 3.3 Error shape

```json
{
  "error": {
    "code": "STRING_CODE",
    "message": "Human readable message",
    "details": {}
  }
}
```

Codes the sidecar returns:

| Status | Code | When |
|---|---|---|
| 400 | `INVALID_REQUEST` | Request body failed validation (`details.errors` has the list) |
| 400 | `EMPTY_TEXT` | `POST /speak` with blank text |
| 400 | `INVALID_AUDIO` | Clone reference audio path or base64 payload is invalid or empty |
| 400 | `VOICE_CLONE_FAILED` | Cloning raised an error in the backend |
| 401 | `UNAUTHORIZED` | Missing or invalid bearer token |
| 403 | `FORBIDDEN` | Deleting or editing built-in voice `"0"` |
| 404 | `VOICE_NOT_FOUND` | Unknown voice |
| 404 | `JOB_NOT_FOUND` | Unknown job (cancel, playback update) |
| 409 | `MODEL_NOT_READY` | The backend cannot do this (for example cloning, or a non-default voice on a backend that supports only the default) or failed to activate |
| 409 | `JOB_IN_PROGRESS` | `POST /models/activate` while a job is running |

## 4. HTTP API (`/v1`)

### 4.1 `GET /health`
Returns runtime health and capabilities: `engine_version` (`"0.2.2"`), `active_model_id`, `device`, `capabilities` (`supports_voice_clone`, `supports_audio_chunk_stream`, `supports_true_streaming_inference`, `languages`) and `runtime` (`backend`, `model_loaded`, `fallback_active`, `detail`, `supports_default_voice`, `supports_cloned_voices`, `warmup`). The `warmup` object has `status`, `runs`, `last_reason`, `last_started_at`, `last_completed_at`, `last_duration_ms` and `last_error`.

`backend` is one of `kyutai_pocket_tts`, `qwen_custom_voice` or `mock`.

### 4.2 `GET /voices`
Lists built-in and cloned voices, oldest first. Each entry has `voice_id`, `display_name`, `created_at`, `tts_model_id`, `language_hint` and `description`.

### 4.3 `POST /voices/clone`
Creates a reusable cloned voice profile.

Request:

```json
{
  "display_name": "My Voice",
  "ref_audio": { "wav_base64": "..." },
  "ref_text": "optional transcript",
  "language": "en",
  "description": "optional",
  "options": { "normalize_audio": true }
}
```

- `display_name`: required, 1 to 80 characters.
- `ref_audio`: required; give either `path` (a file on the sidecar's machine) or `wav_base64`.
- `ref_text`, `language`, `description` (at most 240 characters), `options`: optional.

Response: the new voice, in the same shape as `GET /voices` entries.

Errors: `409 MODEL_NOT_READY` if the active backend cannot clone, `400 INVALID_AUDIO`, `400 VOICE_CLONE_FAILED`.

### 4.4 `DELETE /voices/{voice_id}`
Deletes a cloned voice. `voice_id="0"` cannot be deleted (`403 FORBIDDEN`). An unknown or non-UUID ID returns `404 VOICE_NOT_FOUND`.

Response:

```json
{ "deleted": true }
```

### 4.5 `PATCH /voices/{voice_id}`
Edits a cloned voice's metadata. Body fields, all optional but at least one required: `display_name` (1 to 80 characters), `language`, `description` (at most 240 characters). Returns the updated voice. `voice_id="0"` returns `403 FORBIDDEN`; an unknown voice returns `404 VOICE_NOT_FOUND`.

### 4.6 `POST /speak`
Starts a new job.

Request example:

```json
{
  "voice_id": "0",
  "text": "Hello world",
  "language": "en",
  "settings": {
    "rate": 1.0,
    "pitch": 1.0,
    "volume": 1.0,
    "chunking": { "max_chars": 200 }
  }
}
```

Defaults when omitted: `voice_id` `"0"`, `rate` 1.0, `pitch` 1.0, `volume` 1.0, `chunking.max_chars` 200. `text` is required.

Response:

```json
{
  "job_id": "uuid",
  "ws_url": "ws://127.0.0.1:<port>/v1/stream/<job_id>"
}
```

Notes:
- Starting a new job cancels any previous active job.
- Playback controls in `settings` are the initial values for the job.
- Errors: `400 EMPTY_TEXT`, `404 VOICE_NOT_FOUND`, `409 MODEL_NOT_READY`.

### 4.7 `POST /cancel`
Cancels a job.

Request:

```json
{ "job_id": "uuid" }
```

Response:

```json
{ "canceled": true }
```

`404 JOB_NOT_FOUND` if the job is unknown.

### 4.8 `POST /jobs/{job_id}/playback`
Updates playback controls for a running job.

Request:

```json
{
  "rate": 1.5,
  "pitch": 1.0,
  "volume": 1.0
}
```

Fields are optional, but at least one of `rate`, `pitch`, `volume` is required. The desktop app sends only `rate`.

Response:

```json
{ "updated": true }
```

Behavior:
- Update is applied to the job state immediately.
- Effective audio change happens on the next chunk processing cycle in the sidecar job loop.
- `pitch` is accepted by schema but currently reserved (no active pitch DSP in the sidecar controls path).

Errors:
- `404 JOB_NOT_FOUND` if job is missing or already completed
- `400 INVALID_REQUEST` if a value is out of range or the payload has no playback fields

### 4.9 `POST /models/activate`
Reloads model/runtime configuration and triggers warmup. The body is optional. Fields (all optional): `synth_backend` (`auto`, `qwen`, `kyutai` or `mock`), `active_model_id`, `qwen_model_name`, `qwen_device_map`, `qwen_dtype`, `qwen_attn_implementation`, `qwen_default_speaker`, `kyutai_model_name`, `kyutai_voice_prompt`, `kyutai_sample_rate`, `warmup_wait` (default true), `warmup_force` (default true), `reason`.

Response: `reloaded`, `warmup_accepted`, `active_model_id`, `runtime` (as in `/health`).

Errors: `409 JOB_IN_PROGRESS`, `409 MODEL_NOT_READY` if the new backend fails to start.

### 4.10 `POST /models/prefetch`
Downloads model repositories into local model storage. The body is optional and `mode` defaults to `qwen_all`.

Request:

```json
{ "mode": "qwen_all" }
```

Allowed `mode` values:
- `qwen_custom`
- `qwen_base`
- `qwen_all`
- `all` (also includes the Kyutai repo)

Response: `ok`, `mode`, `downloaded` (repo IDs), `saved_to` (repo ID to local folder), `data_dir`, `models_dir`, `hf_cache_dir`.

### 4.11 `POST /warmup`
Triggers warmup inference. The body is optional: `wait` (default false), `force` (default false), `reason`.

Response: `accepted` and the `warmup` status object. With `wait: false` it returns before warmup finishes. Warmup also runs on startup unless `VOICEREADER_WARMUP_ON_STARTUP` is false.

### 4.12 `POST /quit`
Requests graceful sidecar shutdown.

Response:

```json
{ "quitting": true }
```

## 5. WebSocket API

### 5.1 `WS /v1/stream/{job_id}`
Streams JSON events. A client that connects late first receives the job's events so far, in order, and then the live ones. The stream closes after a terminal event.

Event types:
- `JOB_STARTED`
- `AUDIO_CHUNK`
- `JOB_DONE`
- `JOB_CANCELED`
- `JOB_ERROR`

`JOB_STARTED`, `JOB_DONE` and `JOB_CANCELED` carry `type` and `job_id`.

`AUDIO_CHUNK` example:

```json
{
  "type": "AUDIO_CHUNK",
  "job_id": "uuid",
  "seq": 1,
  "audio": {
    "format": "pcm_s16le",
    "sample_rate": 24000,
    "channels": 1,
    "data_base64": "..."
  },
  "text_range": {
    "chunk_index": 0,
    "start_char": 0,
    "end_char": 120
  }
}
```

`JOB_ERROR` carries an error object:

```json
{
  "type": "JOB_ERROR",
  "job_id": "uuid",
  "error": { "code": "INFERENCE_FAILED", "message": "...", "details": {} }
}
```

The desktop app forwards these events unchanged to its frontend as `voicereader:ws-event`. The Base build emits events of the same names, but its `AUDIO_CHUNK` carries `chunk_index` instead of `seq` and `text_range`.

## 6. Playback Control Semantics

Validation ranges:
- `rate`: `0.25` to `4.0`
- `pitch`: `0.5` to `2.0`
- `volume`: `0.0` to `2.0`
- `chunking.max_chars`: `100` to `2000` is accepted, but the sidecar's splitter caps chunks at 200 characters (`FIRST_CHUNK_MAX_CHARS` in `chunking.py`) and uses one sentence per chunk. Sentences are cut at `. ! ? ; :` and line breaks; a sentence longer than the limit is cut at a space.

Current sidecar implementation:
- `rate`: time-stretch with pitch-preserving preference:
  1. SoX (`tempo`)
  2. `librosa.effects.time_stretch`
  3. linear resample fallback (changes pitch)
- `volume`: applied by PCM amplitude scaling
- `pitch`: accepted and stored, currently reserved/no-op
- Synthesis of the next chunk starts before the current chunk's rate processing finishes.

## 7. Process Lifecycle Notes
- App should keep sidecar as a child process.
- On app shutdown:
  1. call `POST /v1/quit`
  2. wait briefly for exit
  3. force-kill only if still running
