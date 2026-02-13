Speak Selection Engine API (Phase 1)

## 1. Overview
The Engine runs as a localhost-only service that the Tauri app communicates with.

- Transport:
  - HTTP for request/response control
  - WebSocket for streaming audio chunks and job events
- Security:
  - App generates a high-entropy session token before launching Engine
  - Token is passed over process bootstrap (stdin or inherited env var), not command-line args
  - Engine binds loopback only (`127.0.0.1`), on an ephemeral port
  - All HTTP and WS requests require `Authorization: Bearer <token>`
  - Query-string tokens are disabled by default (header-based auth only)

## 2. Conventions

### 2.1 Content types
- Requests/Responses: `application/json`
- Audio chunks: binary frames over WebSocket OR base64 in JSON (Phase 1 choice: start with base64 JSON for simplicity, migrate to binary frames later)

### 2.2 IDs
- `job_id`: UUID
- `voice_id`: UUID
- `model_id`: string (e.g., `qwen3-tts-12hz-0.6b-base`)

### 2.3 Error format
All errors return:
```json
{
  "error": {
    "code": "STRING_CODE",
    "message": "Human readable message",
    "details": { }
  }
}
```

### 2.4 Session bootstrap (recommended)
1) Tauri app generates a 256-bit random token and ephemeral port.
2) Tauri launches Engine sidecar and passes token via stdin bootstrap payload (preferred) or inherited env var.
3) Engine binds loopback, starts API, and emits ready signal including bound port.
4) App stores token in-memory only and uses it in `Authorization` header for HTTP + WS.
5) Token is rotated every app launch and invalidated on engine shutdown.

### 2.5 Error/status mapping
- `INVALID_AUDIO`, `EMPTY_TEXT`, `TRANSCRIPT_REQUIRED`: HTTP `400`
- `VOICE_NOT_FOUND`, `JOB_NOT_FOUND`: HTTP `404`
- `MODEL_NOT_READY`: HTTP `409`
- `UNAUTHORIZED`: HTTP `401`
- `FORBIDDEN`: HTTP `403`
- `INFERENCE_FAILED`: HTTP `500`

---

## 3. HTTP API

Base path: `/v1`

### 3.1 GET /health

Returns engine status and capabilities.

**Response 200**

```json
{
  "engine_version": "0.1.0",
  "active_model_id": "qwen3-tts-12hz-0.6b-base",
  "device": "cuda",
  "capabilities": {
    "supports_voice_clone": true,
    "supports_audio_chunk_stream": true,
    "supports_true_streaming_inference": false,
    "languages": ["zh", "en", "ja", "ko", "de", "fr", "es", "pt", "ru", "id", "auto"]
  }
}
```

---

### 3.2 GET /voices

List saved voices.

**Response 200**

```json
{
  "voices": [
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

* `INVALID_AUDIO`
* `TRANSCRIPT_REQUIRED` (if ASR not enabled and ref_text missing)
* `MODEL_NOT_READY`

---

### 3.4 DELETE /voices/{voice_id}

Delete a voice profile and its stored artifacts.

**Response 200**

```json
{ "deleted": true }
```

**Errors**

* `VOICE_NOT_FOUND`

---

### 3.5 POST /speak

Start speaking text using a specified voice.

**Request**

```json
{
  "voice_id": "uuid",
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

* `VOICE_NOT_FOUND`
* `EMPTY_TEXT`
* `MODEL_NOT_READY`

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

* `JOB_NOT_FOUND`

---

## 4. WebSocket Streaming API

### 4.1 WS /v1/stream/{job_id}

Events are JSON messages (Phase 1). Later we may switch audio frames to binary.
WS requests must include `Authorization: Bearer <token>` header.
If the WS client cannot set custom headers, pass the token via `Sec-WebSocket-Protocol` instead of query params.

#### Event: JOB_STARTED

```json
{ "type": "JOB_STARTED", "job_id": "uuid" }
```

#### Event: AUDIO_CHUNK

Contains PCM audio (16-bit signed LE), base64 encoded.

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

* `rate`: float (0.5-2.0)
* `pitch`: float (0.5-2.0)
* `volume`: float (0.0-2.0)
* `chunking.max_chars`: int (100-2000)

---

## 6. Future Extensions (non-breaking plan)

* Add `/models` endpoints:

  * list installed
  * download/install
  * set active
* Add a `/voices/clone/transcribe` flow:

  * run optional ASR and return transcript
* Add binary WS frames for audio chunks
* Support multi-job queueing




