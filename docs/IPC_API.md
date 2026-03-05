VoiceReader Engine API (Current Implementation, Full Build Sidecar)

## 1. Overview
This document describes the Python sidecar HTTP and WebSocket API used by the
`build-full` profile.

`build-base` does not use this localhost API. It runs Kyutai in-process in
Rust.

- Transport:
  - HTTP for control requests
  - WebSocket for streamed job events and audio chunks
- Bind:
  - Loopback only (`127.0.0.1`)
- Auth:
  - Bearer token required for HTTP and WS

## 2. Auth Policy

### 2.1 HTTP
All HTTP requests require:

```text
Authorization: Bearer <token>
```

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

## 4. HTTP API (`/v1`)

### 4.1 `GET /health`
Returns runtime health and capabilities.

### 4.2 `GET /voices`
Lists built-in and cloned voices.

### 4.3 `POST /voices/clone`
Creates a reusable cloned voice profile.

### 4.4 `DELETE /voices/{voice_id}`
Deletes a cloned voice. `voice_id="0"` cannot be deleted.

### 4.5 `POST /speak`
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
    "chunking": { "max_chars": 500 }
  }
}
```

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

### 4.6 `POST /cancel`
Cancels a job.

Request:

```json
{ "job_id": "uuid" }
```

Response:

```json
{ "canceled": true }
```

### 4.7 `POST /jobs/{job_id}/playback`
Updates playback controls for a running job.

Request:

```json
{
  "rate": 1.5,
  "pitch": 1.0,
  "volume": 1.0
}
```

Fields are optional, but at least one of `rate`, `pitch`, `volume` is required.

Response:

```json
{ "updated": true }
```

Behavior:
- Update is applied to the job state immediately.
- Effective audio change happens on the next chunk processing cycle in the
  sidecar job loop.
- `pitch` is accepted by schema but currently reserved (no active pitch DSP in
  the sidecar controls path).

Errors:
- `404 JOB_NOT_FOUND` if job is missing or already completed
- `422` validation error if payload has no playback fields

### 4.8 `POST /models/activate`
Reloads model/runtime configuration and triggers warmup.

### 4.9 `POST /models/prefetch`
Downloads model repositories into local model storage.

Request:

```json
{ "mode": "qwen_all" }
```

Allowed `mode` values:
- `qwen_custom`
- `qwen_base`
- `qwen_all`
- `all`

### 4.10 `POST /warmup`
Triggers warmup inference.

### 4.11 `POST /quit`
Requests graceful sidecar shutdown.

Response:

```json
{ "quitting": true }
```

## 5. WebSocket API

### 5.1 `WS /v1/stream/{job_id}`
Streams JSON events.

Event types:
- `JOB_STARTED`
- `AUDIO_CHUNK`
- `JOB_DONE`
- `JOB_CANCELED`
- `JOB_ERROR`

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

## 6. Playback Control Semantics

Validation ranges:
- `rate`: `0.25` to `4.0`
- `pitch`: `0.5` to `2.0`
- `volume`: `0.0` to `2.0`
- `chunking.max_chars`: `100` to `2000`

Current sidecar implementation:
- `rate`: time-stretch with pitch-preserving preference:
  1. SoX (`tempo`)
  2. `librosa.effects.time_stretch`
  3. linear resample fallback
- `volume`: applied by PCM amplitude scaling
- `pitch`: accepted and stored, currently reserved/no-op

## 7. Process Lifecycle Notes
- App should keep sidecar as a child process.
- On app shutdown:
  1. call `POST /v1/quit`
  2. wait briefly for exit
  3. force-kill only if still running
