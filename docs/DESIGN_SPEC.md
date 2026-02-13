# DESIGN_SPEC.md - Speak Selection (Phase 1)

## 1. Scope
### Phase 1 Goals
- Background desktop app (Tauri) for Windows and macOS
- Global hotkey: "Read Selection"
- Capture selected text from the active application:
  1) Accessibility APIs (primary)
  2) Clipboard copy-and-restore (fallback)
- Local TTS inference with **Qwen3-TTS-12Hz-0.6B-Base** bundled by default
- Phase 1 runtime profile: CUDA + BF16 (`torch.bfloat16`)
- Preferred attention backend: FlashAttention 2
- Windows fallback backend (if FlashAttention 2 is unavailable): PyTorch SDPA
- Phase 1 engine validation target is NVIDIA CUDA; cross-device parity is deferred
- "Clone once -> save -> reuse" voice cloning using reusable voice prompt artifacts
- Chunked synthesis + immediate playback (streaming UX without true streaming inference)
- Model-swappable engine architecture (future models as plugins/backends)

### Phase 1 Non-goals
- Quantization / ONNX Runtime / INT8 runtime variants / DirectML
- True streaming token-by-token synthesis
- Mobile implementation (Android/iOS)
- Cloud services / accounts / sync
- Production CPU/MPS fallback paths

---

## 2. User Stories

### 2.1 Read selection
As a user, I can highlight text in any app and press a hotkey to hear it read aloud immediately.

Acceptance criteria:
- Hotkey triggers within 200ms perceived response (UI shows "reading...")
- First audio chunk plays within a reasonable time on supported CUDA hardware (target depends on model/device; focus on responsiveness via chunking)
- Stop/Pause/Resume work reliably

### 2.2 Clone and save a voice
As a user, I can create a custom voice from a short sample once, name it, and reuse it later.

Acceptance criteria:
- Voice creation works offline
- Voice is persisted to disk
- Reusing a saved voice does not require re-processing the reference audio

---

## 3. System Architecture

### 3.1 Components
1) **Tauri App (Rust)**
- UI: voice manager, settings
- System tray/menu bar
- Global hotkeys
- Selection capture (OS adapters)
- Audio playback

2) **Engine Service (Local Daemon)**
- Phase 1: Python process
- Loads TTS model once and stays warm
- Provides IPC API (HTTP + WebSocket)
- Handles voice cloning + persistence
- Handles synthesis jobs + chunking + cancellation

3) **Storage**
- Per-user app data directory contains:
  - models/
  - voices/
  - settings.json
  - logs/

### 3.2 Rationale: separate Engine service
- Keeps model warm
- Enables cancellation and job management
- Makes model swapping easy
- Keeps UI stable while allowing backend changes

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

### 5.3 Device selection
- Phase 1 supported runtime:
  - CUDA device path
  - `torch_dtype=bfloat16`
  - `attn_implementation="flash_attention_2"` preferred
  - `attn_implementation="sdpa"` fallback on Windows when FlashAttention 2 is unavailable
- Non-CUDA paths (CPU/MPS/ONNX/INT8) are deferred to Phase 2.

---

## 6. Voice Cloning & Persistence

### 6.1 Voice profile concept
A "voice" is stored as a reusable prompt artifact derived from reference audio (+ transcript).

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

### 6.2 Persisted format
Directory per voice:
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
- Engine saves voice profile and returns voice_id

---

## 7. Packaging & Models

### 7.1 Default bundled model
- Bundle Qwen3-TTS-12Hz-0.6B-Base as a compressed model pack in app resources.

First-run behavior:
- Extract to per-user `models/qwen3-tts-12hz-0.6b-base/`
- Verify checksum
- Mark as installed

### 7.2 On-demand model downloads
- Maintain `model_registry.json` shipped with app:
  - model_id
  - version
  - size
  - source URLs (mirrors)
  - sha256
  - license metadata
- Download to temp directory, verify sha256, move into place
- Note: Phase 1 keeps only the bundled default model active. Additional model variants become active scope in Phase 2.

---

## 8. Storage Layout

Per-user app data directory:
- Windows: `%LOCALAPPDATA%/SpeakSelection/`
- macOS: `~/Library/Application Support/SpeakSelection/`

```
models/  
qwen3-tts-12hz-0.6b-base/
qwen3-asr-0.6b/ (optional)
voices/
<voice_id>/
meta.json
prompt.safetensors
settings.json
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
- Load bundled model
- Validate CUDA + BF16 runtime path with FlashAttention 2 preferred and SDPA fallback
- `/health`
- `/speak` + audio chunks via WS
- `/cancel`

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
