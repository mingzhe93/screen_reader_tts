# DESIGN_SPEC.md - VoiceReader (Phase 1)

## 1. Scope
### Phase 1 Goals
- Background desktop app (Tauri) for Windows and macOS
- Global hotkey: "Read Selection"
- Capture selected text from the active application:
  1) Accessibility APIs (primary)
  2) Clipboard copy-and-restore (fallback)
- Local TTS inference with **Kyutai Pocket TTS** bundled by default
- Two distribution profiles:
  - **Base build** (`build-base`): Rust-native Kyutai runtime only, no Python sidecar
  - **Full build** (`build-full`): Python sidecar runtime with Kyutai + optional Qwen
- Qwen models are available as on-demand downloads in **Full build** only
- First-run default voice path uses built-in voice `voice_id: "0"` (Kyutai preset prompt)
- Runtime profile supports:
  - Base build: Kyutai on CPU (bundled, in-process Rust runtime)
  - Full build: Kyutai via sidecar, plus Qwen on CUDA + BF16 when activated
- Preferred Qwen attention backend (Full build): FlashAttention 2
- Windows Qwen fallback backend (Full build, if FlashAttention 2 is unavailable): PyTorch SDPA
- First-run usable speech path with built-in default voice (`voice_id: "0"`) before any cloning
- "Clone once -> save -> reuse" voice cloning using reusable voice prompt artifacts (implemented for Kyutai backend)
- Kyutai output language in the current app flow is English-only
- Chunked synthesis + immediate playback (streaming UX without true streaming inference)
- Model-swappable engine architecture (future models as plugins/backends)

### Phase 1 Non-goals
- Quantization / ONNX Runtime / INT8 runtime variants / DirectML
- True streaming token-by-token synthesis
- Mobile implementation (Android/iOS)
- Cloud services / accounts / sync
- Qwen CPU/MPS production optimization paths

---

## 2. User Stories

### 2.1 Read selection
As a user, I can highlight text in any app and press a hotkey to hear it read aloud immediately.

Acceptance criteria:
- Hotkey triggers within 200ms perceived response (UI shows "reading...")
- First audio chunk plays within a reasonable time on bundled Kyutai CPU path (Base) and optional CUDA Qwen path (Full)
- App works on first run with default built-in voice (`voice_id: "0"`) without cloning
- Stop/Pause/Resume work reliably

### 2.2 Clone and save a voice
As a user, I can create a custom voice from a short sample once, name it, and reuse it later.

Acceptance criteria:
- Voice creation works offline
- Voice is persisted to disk
- Reusing a saved voice does not require re-processing the reference audio

### 2.3 Validate engine independently
As a developer, I can validate each runtime path independently so troubleshooting boundaries are clear.

Acceptance criteria:
- Full build: `/health`, `/voices`, `/speak`, `/cancel`, and WS stream can be tested with simple scripts/CLI
- Base build: Rust-native Kyutai runtime can be validated without launching Python sidecar
- Default voice `"0"` can produce audio before cloning is configured
- Auth behavior is testable without app bootstrapping code

---

## 3. System Architecture

### 3.1 Components
1) **Tauri App (Rust)**
- UI: voice manager, settings
- System tray/menu bar
- Global hotkeys
- Selection capture (OS adapters)
- Audio playback

2) **Engine Runtime**
- **Base build**: in-process Rust Kyutai runtime (no localhost sidecar process)
- **Full build**: Python sidecar daemon
  - Loads TTS model once and stays warm
  - Provides IPC API (HTTP + WebSocket)
  - Exposes a graceful shutdown endpoint (`POST /v1/quit`) for app/test teardown
  - Supports explicit warmup endpoint (`POST /v1/warmup`) and optional startup warmup
  - Handles built-in default voice (`"0"`) and cloned voice profiles
  - Handles voice cloning + persistence
  - Handles synthesis jobs + chunking + cancellation

3) **Storage**
- Per-user app data directory contains:
  - models/
  - voices/
  - settings.json
  - logs/

### 3.2 Rationale: dual runtime architecture
- Base build removes sidecar overhead and reduces package size substantially (portable target around ~200 MB)
- Full build keeps model/service boundaries for advanced model switching and troubleshooting
- Both paths preserve the same app-level UX (hotkey -> speak -> playback)

---

## 4. Selection Capture

### 4.1 Windows (primary: UI Automation)
- Use UIA to query focused element selection text
- Attempt patterns in order:
  1) Selection Pattern
  2) Text Pattern (if available)
  3) Value Pattern (as last resort)

### 4.2 macOS (primary: Accessibility AX APIs)
- Query focused element for selected text via AX attributes
- Fallback if selection attribute not exposed

### 4.3 Fallback: Clipboard copy-and-restore (both OS)
Algorithm:
1) Save current clipboard content (all formats where possible)
2) Send Ctrl/Cmd+C to active app
3) Read clipboard text
4) Restore previous clipboard

Requirements:
- Always restore clipboard (even on error)
- Debounce to avoid repeated triggers
- Detect "no selection" (clipboard unchanged or empty)

---

## 5. TTS Pipeline

### 5.1 Chunking ("streaming UX")
- Split text by sentence boundaries where possible
- Enforce chunk limits:
  - max chars per chunk (e.g., 300-500)
  - optional max duration estimate
- Queue chunks sequentially to engine
- Playback begins as soon as first chunk returns audio

### 5.2 Job model
- One active "Speak" job at a time in Phase 1
- New speak request cancels previous by default (configurable later)
- Cancel stops queued chunks and interrupts inference ASAP
- If cancel arrives while a chunk is generating, that in-flight chunk is dropped and not streamed

### 5.3 Voice selection behavior
- Default built-in voice path:
  - `voice_id: "0"` is always available
  - no cloning required
  - current default app runtime uses Kyutai preset voice prompt (default `alba`)
- Cloned voice path:
  - `voice_id: <uuid>` stored in local `voices/`
  - if unknown UUID is requested, return `VOICE_NOT_FOUND`
  - Kyutai backend supports real cloned-voice inference from saved `prompt.safetensors`

### 5.4 Device selection
- Phase 1 supported runtime:
  - Base build: Kyutai default path on CPU (English-only)
  - Full build: Kyutai sidecar path on CPU
  - Full build: Qwen optional CUDA device path
  - Full build: Qwen `torch_dtype=bfloat16`
  - Full build: Qwen `attn_implementation="flash_attention_2"` preferred
  - Full build: Qwen `attn_implementation="sdpa"` fallback on Windows when FlashAttention 2 is unavailable
- Qwen non-CUDA optimization paths (CPU/MPS/ONNX/INT8) are deferred to Phase 2.

### 5.5 Warmup behavior
- Engine performs optional startup warmup inference (`VOICEREADER_WARMUP_ON_STARTUP=true` by default).
- API clients can trigger warmup with `POST /v1/warmup` (optionally blocking with `wait=true`).
- Model activation/switch flows must trigger warmup immediately after model load (or use `POST /v1/models/activate`, which performs reload + warmup).

---

## 6. Voice Cloning & Persistence

### 6.1 Voice profile concept
A cloned voice is stored as a reusable prompt artifact derived from reference audio (+ transcript).

Data model:
- voice_id (UUID)
- display_name
- created_at
- tts_model_id (compatibility)
- prompt_artifacts (binary)
- optional metadata:
  - language_hint
  - ref_audio_hash
  - ref_text

Reserved ID:
- `voice_id: "0"` is the built-in model voice and is not persisted as a cloned profile

### 6.2 Persisted format
Directory per cloned voice:
- `voices/<voice_id>/meta.json`
- `voices/<voice_id>/prompt.safetensors` (preferred) or `.npz`

Constraints:
- No pickle persistence for security/compatibility
- Engine enforces compatibility:
  - If model_id differs, prompt may not be reusable -> require re-clone

### 6.3 Voice creation workflow
- User provides sample audio (recorded/imported)
- User provides transcript OR optional ASR module supplies it
- Engine creates reusable clone prompt once
- Engine saves voice profile and returns UUID `voice_id`
- Engine can immediately synthesize with that saved cloned voice (`voice_id=<uuid>`) on Kyutai backend

---

## 7. Packaging & Models

### 7.1 Default bundled model
- Bundle Kyutai Pocket TTS model repo (`Verylicious/pocket-tts-ungated`) in app resources.
- Base build does **not** bundle Python sidecar runtime.
- Full build bundles sidecar runtime (`tts-engine`) in app resources.

First-run behavior:
- Engine resolves bundled Kyutai model path directly from app resources.
- If bundled model is missing, engine falls back to repo-id based download flow into app data.

### 7.2 On-demand model downloads
- Maintain `model_registry.json` shipped with app:
  - model_id
  - version
  - size
  - source URLs (mirrors)
  - sha256
  - license metadata
- Download to temp directory, verify sha256, move into place
- Phase 1 app exposes on-demand downloads for Qwen CustomVoice + Qwen Base from the Engine tab in Full build.

### 7.3 Build matrix summary
- Base build:
  - Runtime: Rust-native Kyutai only
  - Sidecar: none
  - Qwen/GPU: not supported
  - Language coverage: English-only
  - Primary goal: small portable footprint
- Full build:
  - Runtime: Python sidecar
  - Sidecar: bundled
  - Qwen/GPU: supported when dependencies and hardware are available
  - Language coverage: Kyutai English + Qwen multilingual
  - Primary goal: full model flexibility

---

## 8. Storage Layout

Per-user app data directory:
- Windows: `%LOCALAPPDATA%/com.voicereader.desktop/data/`
- macOS: `~/Library/Application Support/com.voicereader.desktop/data/`

```
models/
  Verylicious/pocket-tts-ungated/ (bundled or mirrored)
  Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice/ (on-demand, Full build)
  Qwen/Qwen3-TTS-12Hz-0.6B-Base/ (on-demand, Full build)
voices/
  <voice_id>/
    meta.json
    prompt.safetensors
settings.json
hf-cache/
cache/
logs/
```

---

## 9. Logging & Telemetry
- Local-only logs in `logs/`
- No telemetry or network calls by default (offline-first principle)

---

## 10. Milestones / Deliverables

### M1 - Engine MVP
- Load bundled Kyutai model
- Validate Base runtime path (Rust Kyutai, no sidecar) and Full runtime path (Kyutai/Qwen via sidecar)
- Validate Qwen optional CUDA + BF16 path in Full build (FlashAttention 2 preferred, SDPA fallback on Windows)
- `/health`
- `/voices` includes built-in default voice `"0"`
- `/speak` works with default voice `"0"` before cloning
- `/speak` + audio chunks via WS
- `/cancel`
- `/quit` for graceful shutdown

### M2 - App MVP
- Tray app + hotkeys
- Clipboard fallback selection capture
- Playback + pause/resume/stop

### M3 - Accessibility selection capture
- Windows UIA integration
- macOS AX integration
- Reliable fallback to clipboard

### M4 - Voice cloning + persistence
- Record/import audio
- Transcript input
- Clone prompt creation + save + reuse

### M5 - Optional ASR module
- On-demand download of Qwen3-ASR 0.6B
- Auto-transcribe sample audio during voice creation
