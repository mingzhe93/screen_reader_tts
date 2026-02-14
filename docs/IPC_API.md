VoiceReader Engine API (Phase 1)

## 1. Overview
The Engine runs as a localhost-only service that the desktop app communicates with.

- Transport:
  - HTTP for request/response control
  - WebSocket for audio chunk and job event streaming
- Security:
  - App generates a high-entropy session token before launching Engine
  - Token is passed via stdin bootstrap payload (preferred) or inherited env var
  - Engine binds loopback only (`127.0.0.1`) on an app-chosen port
  - Query-string auth tokens are disabled

### 1.1 Auth policy
- HTTP requests: `Authorization: Bearer <token>` (required)
- WebSocket requests (preferred): `Authorization: Bearer <token>`
- WebSocket fallback (for clients that cannot set custom headers):
  - `Sec-WebSocket-Protocol: auth.bearer.v1, <token>`
  - server validates the second protocol token as the auth token
  - server responds with `Sec-WebSocket-Protocol: auth.bearer.v1`

## 2. Conventions

### 2.1 Content types
- Requests/Responses: `application/json`
- Audio chunks: JSON events with base64 PCM in Phase 1

### 2.2 IDs
- `job_id`: UUID
- `voice_id`: string
  - reserved built-in default voice: `"0"`
  - cloned voice: UUID string
- `model_id`: string (example: `qwen3-tts-12hz-0.6b-base`)

### 2.3 Error format
All errors return:

```json
{
  "error": {
    "code": "STRING_CODE",
    "message": "Human readable message",
    "details": {}
  }
}
```

### 2.4 Session bootstrap (current status)
Bootstrap schema is intentionally deferred for now. Interim behavior for standalone engine testing:
1) set token via `SPEAK_SELECTION_ENGINE_TOKEN`
2) launch engine with `python -m tts_engine --server --port <port>`

A strict stdin bootstrap JSON schema will be finalized during Tauri integration.

Local storage defaults (standalone):
- engine data dir: `./.data` (or `--data-dir`)
- Hugging Face cache: `<data_dir>/hf-cache`
- local model mirrors (if prefetched): `<data_dir>/models/<org>/<repo>`

### 2.5 Error/status mapping
- `INVALID_AUDIO`, `EMPTY_TEXT`, `TRANSCRIPT_REQUIRED`: HTTP `400`
- `VOICE_NOT_FOUND`, `JOB_NOT_FOUND`: HTTP `404`
- `MODEL_NOT_READY`: HTTP `409`
- `JOB_IN_PROGRESS`: HTTP `409`
- `UNAUTHORIZED`: HTTP `401`
- `FORBIDDEN`: HTTP `403`
- `INFERENCE_FAILED`: HTTP `500`

### 2.6 Process lifecycle (recommended)
- App launches engine as a child process (non-detached) and keeps the process handle.
- On app shutdown:
  1) call `POST /v1/quit` with Bearer token
  2) wait for process exit for a short timeout (e.g., 2-5 seconds)
  3) if still alive, force-kill process

---

## 3. HTTP API

Base path: `/v1`

### 3.1 GET /health

Returns engine status and capabilities.

**Response 200**

```json
{
  "engine_version": "0.1.0",
  "active_model_id": "Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice",
  "device": "cuda",
  "capabilities": {
    "supports_voice_clone": false,
    "supports_audio_chunk_stream": true,
    "supports_true_streaming_inference": false,
    "languages": ["zh", "en", "ja", "ko", "de", "fr", "es", "pt", "ru", "it", "auto"]
  },
  "runtime": {
    "backend": "qwen_custom_voice",
    "model_loaded": true,
    "fallback_active": false,
    "detail": "model=Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice, device_map=cuda:0, dtype=bfloat16, attn=flash_attention_2",
    "supports_default_voice": true,
    "supports_cloned_voices": false,
    "warmup": {
      "status": "ready",
      "runs": 1,
      "last_reason": "startup",
      "last_started_at": "ISO8601",
      "last_completed_at": "ISO8601",
      "last_duration_ms": 1500,
      "last_error": null
    }
  }
}
```

Notes:
- In `VOICEREADER_SYNTH_BACKEND=auto`, engine may fall back to `backend=mock` if Qwen runtime cannot be loaded.
- Use `runtime` fields to confirm if real model inference is active.
- `capabilities.supports_voice_clone` is backend-dependent (e.g., false on `qwen_custom_voice`, true on current `mock` fallback).

---

### 3.2 GET /voices

List available voices.

**Response 200**

```json
{
  "voices": [
    {
      "voice_id": "0",
      "display_name": "Default Built-in Voice",
      "created_at": "1970-01-01T00:00:00Z",
      "tts_model_id": "Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice",
      "language_hint": "auto"
    },
    {
      "voice_id": "uuid",
      "display_name": "My Voice",
      "created_at": "ISO8601",
      "tts_model_id": "qwen3-tts-12hz-0.6b-base",
      "language_hint": "en"
    }
  ]
}
```

---

### 3.3 POST /voices/clone

Create a new reusable voice profile from a reference sample.

**Request**

```json
{
  "display_name": "My Voice",
  "ref_audio": {
    "path": "/path/to/sample.wav"
  },
  "ref_text": "Optional transcript (recommended)",
  "language": "en",
  "options": {
    "normalize_audio": true
  }
}
```

Alternative audio input (base64):

```json
{
  "display_name": "My Voice",
  "ref_audio": {
    "wav_base64": "..."
  },
  "ref_text": "..."
}
```

**Response 200**

```json
{
  "voice_id": "uuid",
  "display_name": "My Voice",
  "created_at": "ISO8601",
  "tts_model_id": "qwen3-tts-12hz-0.6b-base"
}
```

**Errors**

- `INVALID_AUDIO`
- `TRANSCRIPT_REQUIRED` (if ASR not enabled and `ref_text` missing)
- `MODEL_NOT_READY`

---

### 3.4 DELETE /voices/{voice_id}

Delete a cloned voice profile and its stored artifacts.

Notes:
- `voice_id="0"` is reserved and cannot be deleted.

**Response 200**

```json
{ "deleted": true }
```

**Errors**

- `VOICE_NOT_FOUND`

---

### 3.5 POST /speak

Start speaking text using a specified voice.

`voice_id` is optional. If omitted, default built-in voice `"0"` is used.

**Request**

```json
{
  "voice_id": "0",
  "text": "Hello world...",
  "language": "en",
  "settings": {
    "rate": 1.0,
    "pitch": 1.0,
    "volume": 1.0,
    "chunking": {
      "max_chars": 400
    }
  }
}
```

**Response 200**

```json
{
  "job_id": "uuid",
  "ws_url": "ws://127.0.0.1:<port>/v1/stream/<job_id>"
}
```

**Errors**

- `VOICE_NOT_FOUND` (for non-default unknown voice IDs)
- `EMPTY_TEXT`
- `MODEL_NOT_READY` (for known-but-unsupported voice modes on current backend, e.g., cloned voice on `qwen_custom_voice`)

Notes:
- `settings.rate` is applied engine-side by time-scaling each returned chunk.
- `settings.volume` is applied engine-side by scaling PCM amplitude per chunk.
- `settings.pitch` is accepted by schema but currently reserved (no-op in Phase 1 runtime).

---

### 3.6 POST /cancel

Cancel an active job.

**Request**

```json
{ "job_id": "uuid" }
```

**Response 200**

```json
{ "canceled": true }
```

**Errors**

- `JOB_NOT_FOUND`

Notes:
- Cancel is honored at chunk boundaries.
- If cancel arrives while a chunk is generating, that in-flight chunk is dropped and not emitted to WS.
- Terminal event after successful cancellation is `JOB_CANCELED`.

---

### 3.7 POST /quit

Request graceful engine shutdown.

**Request**

```json
{}
```

**Response 200**

```json
{ "quitting": true }
```

Notes:
- Protected by the same Bearer-token auth as all other endpoints.
- Intended for app/test cleanup. Callers should still apply a force-kill fallback if process does not exit in time.

---

### 3.8 POST /warmup

Trigger warmup inference for the currently loaded backend/model.

**Request**

```json
{
  "wait": true,
  "force": false,
  "reason": "app_startup"
}
```

Fields:
- `wait`: if `true`, response returns after warmup completes
- `force`: if `true`, run warmup even when status is already `ready`
- `reason`: optional tag for logs/diagnostics

**Response 200**

```json
{
  "accepted": true,
  "warmup": {
    "status": "ready",
    "runs": 2,
    "last_reason": "app_startup",
    "last_started_at": "ISO8601",
    "last_completed_at": "ISO8601",
    "last_duration_ms": 1020,
    "last_error": null
  }
}
```

Notes:
- Warmup can also run automatically on startup (`VOICEREADER_WARMUP_ON_STARTUP=true`).
- Any future model-switch flow should call `/v1/warmup` after activating a new model.

---

### 3.9 POST /models/activate

Activate/reload the runtime backend/model config and trigger warmup for the new runtime.

This endpoint is intended for app-side model switch flows and engine troubleshooting.

**Request**

```json
{
  "synth_backend": "qwen",
  "active_model_id": "qwen3-tts-12hz-0.6b-base",
  "qwen_model_name": "Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice",
  "qwen_device_map": "cuda:0",
  "qwen_dtype": "bfloat16",
  "qwen_attn_implementation": "flash_attention_2",
  "qwen_default_speaker": "Ryan",
  "warmup_wait": true,
  "warmup_force": true,
  "reason": "app_model_switch"
}
```

Fields are optional. Omitted fields keep their current runtime value.

**Response 200**

```json
{
  "reloaded": true,
  "warmup_accepted": true,
  "active_model_id": "Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice",
  "runtime": {
    "backend": "qwen_custom_voice",
    "model_loaded": true,
    "fallback_active": false,
    "detail": "model=..., device_map=cuda:0, dtype=bfloat16, attn=flash_attention_2",
    "supports_default_voice": true,
    "supports_cloned_voices": false,
    "warmup": {
      "status": "ready",
      "runs": 1,
      "last_reason": "app_model_switch",
      "last_started_at": "ISO8601",
      "last_completed_at": "ISO8601",
      "last_duration_ms": 1200,
      "last_error": null
    }
  }
}
```

**Errors**
- `JOB_IN_PROGRESS` (cannot switch while a speak job is active)
- `MODEL_NOT_READY` (new backend/model failed to initialize)

---

## 4. WebSocket Streaming API

### 4.1 WS /v1/stream/{job_id}

Events are JSON messages in Phase 1. Later versions may switch audio frames to binary.

Auth:
- preferred: `Authorization: Bearer <token>`
- fallback: `Sec-WebSocket-Protocol: auth.bearer.v1, <token>`

#### Event: JOB_STARTED

```json
{ "type": "JOB_STARTED", "job_id": "uuid" }
```

#### Event: AUDIO_CHUNK

Contains PCM audio (16-bit signed little-endian), base64 encoded.

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
    "end_char": 132
  }
}
```

#### Event: JOB_DONE

```json
{ "type": "JOB_DONE", "job_id": "uuid" }
```

#### Event: JOB_CANCELED

```json
{ "type": "JOB_CANCELED", "job_id": "uuid" }
```

#### Event: JOB_ERROR

```json
{
  "type": "JOB_ERROR",
  "job_id": "uuid",
  "error": {
    "code": "INFERENCE_FAILED",
    "message": "Details...",
    "details": {}
  }
}
```

---

## 5. Settings schema (engine-facing)

Suggested engine settings validation:

- `rate`: float (0.25-4.0)
- `pitch`: float (0.5-2.0)
- `volume`: float (0.0-2.0)
- `chunking.max_chars`: int (100-2000)

Current Phase 1 behavior:
- `rate`: implemented (engine post-processing, chunk time-scale).
- `volume`: implemented (engine post-processing, PCM gain).
- `pitch`: accepted but reserved/no-op for now.

---

## 6. Standalone engine validation

Recommended order before app integration:
1) run engine server with token env var
2) call `/v1/health`
3) call `/v1/voices` and verify default voice `"0"` is present
4) call `/v1/speak` using `voice_id: "0"`
5) connect to WS stream and verify `JOB_STARTED -> AUDIO_CHUNK* -> JOB_DONE`
6) call `/v1/quit` to ensure clean shutdown
7) optionally call `/v1/warmup` with `wait=true` before latency-sensitive tests

---

## 7. Future extensions (non-breaking plan)

- Add `/models` endpoints:
  - list installed
  - download/install
  - set active
- Add `/voices/clone/transcribe` flow:
  - run optional ASR and return transcript
- Add binary WS frames for audio chunks
- Support multi-job queueing
