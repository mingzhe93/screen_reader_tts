# VoiceReader (Speak Selection Workflow)

VoiceReader is a lightweight, offline-first desktop app that reads aloud the text you highlight in any application.

It's designed to improve readability and accessibility (especially for people with dyslexia) by turning selected text into natural-sounding speech using local, open-weight TTS models - with **no cloud dependency**.

## What it does (Phase 1 completed)
Status: Phase 1 core goals are complete.

- Runs in the background (tray/menu bar)
- Reads out **highlighted text** from the active application via a hotkey
- Uses **accessibility APIs first** (Windows UIA, macOS AX), with a **clipboard fallback**
- Supports **voice cloning**:
  - Clone a voice once from a short audio sample
  - Save the cloned voice locally
  - Reuse it for all future speech generation
- Works fully offline by default:
  - Base build bundles **Kyutai Pocket TTS** with Rust-native runtime
  - Full build supports optional Qwen models (download on demand)

## Why this exists
Browser TTS extensions are often slow, inconsistent, and limited in voice quality. Meanwhile, modern TTS models can produce far more natural speech. VoiceReader brings that quality to a simple "highlight -> hotkey -> listen" workflow, locally and privately.

## Core principles
- **Offline-first & private**: everything runs on-device
- **Accessibility-first**: selection capture via OS accessibility APIs before clipboard fallback
- **Model-swappable**: clean backend interface so we can add/replace models over time
- **Fast perceived latency**: chunked generation + immediate playback

## Chunking & Playback Findings
- We observed audible breakup at high playback rates when chunks were too small, especially with pitch-preserving tempo processing.
- Root cause was real-time pressure mismatch: generation and post-processing produced bursty small packets, while playback drained continuously, causing underflow gaps.
- Current default policy in app/runtime is:
  - `chunk_max_chars = 500`
  - group up to **3 sentences per chunk**
  - apply playback prebuffering before first audible output
- Why this works better:
  - larger chunks reduce scheduling overhead and WS/UI event churn
  - 3-sentence grouping keeps better prosody continuity
  - GPU Qwen throughput tends to scale sub-linearly with chunk length, so larger chunks often improve smoothness without a proportional latency penalty

---

## Current implementation (Phase 1)
This is what is wired right now:

### Desktop app (Tauri)
- Windowed app with a simple "Reader" page
- Global hotkey: user-configurable (default: Windows `Alt+Shift+S`, macOS `Cmd+Shift+S`)
- End-to-end flow: hotkey/manual speak -> local runtime (Base) or `/v1/speak` + WS stream (Full) -> local playback
- Full build sidecar lifecycle from app:
  - launch on startup
  - health handshake
  - restart/cancel controls
  - shutdown on app exit
- UI for:
  - model mode selection (`kyutai_pocket_tts`, `qwen_custom_voice`, `qwen_base_clone`)
  - unified voice selection (preset + saved cloned voices)
  - clone/upload, voice edit/delete, and engine health/activity pages
  - model download actions for Qwen variants

### Engine runtime profiles
- **Base build (`build-base`)**:
  - Rust-native Pocket TTS runtime (no Python sidecar)
  - Kyutai bundled by default
  - Supports read + clone + saved voice reuse
  - English-only synthesis in current app flow
- **Full build (`build-full`)**:
  - Python sidecar daemon (kept warm)
  - Loads Kyutai/Qwen model(s) from local engine data dir
  - Optional Qwen runtime path: CUDA + `torch.bfloat16` with `attn_implementation="flash_attention_2"` when available
  - Windows Qwen fallback path: CUDA + BF16 + `attn_implementation="sdpa"` if FlashAttention 2 is unavailable
  - Provides IPC API endpoints for `speak`, `cancel`, and voice cloning/listing/deletion
  - Includes warmup support and model activation endpoint

### Known limitations in this slice
- Selection capture is currently clipboard-based only (UIA/AX capture not wired yet)
- Qwen base/custom flows are Full build only and not bundled by default (download on demand)
- Base build Kyutai runtime is English-only
- Portable mode still depends on system WebView2 runtime on Windows

---

## Default model sources
- Hugging Face model: [Verylicious/pocket-tts-ungated Meant for developers with no huggingface account](https://huggingface.co/Verylicious/pocket-tts-ungated)
- Hugging Face model: [Qwen/Qwen3-TTS-12Hz-0.6B-Base](https://huggingface.co/Qwen/Qwen3-TTS-12Hz-0.6B-Base)
- Full build optional no-clone runtime path: [Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice](https://huggingface.co/Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice)
- GitHub repo: [QwenLM/Qwen3-TTS](https://github.com/QwenLM/Qwen3-TTS)
- GitHub repo: [kyutai-labs/pocket-tts](https://github.com/kyutai-labs/pocket-tts)
- GitHub repo (Rust-native Pocket TTS used by Base build): [babybirdprd/pocket-tts](https://github.com/babybirdprd/pocket-tts)

## Qwen runtime baseline (from upstream `pyproject.toml`)
- Recommended environment: isolated Python 3.12 env
- Version pins:
  - `transformers==4.57.3`
  - `accelerate==1.12.0`
- Additional runtime deps:
  - `gradio`, `librosa`, `torchaudio`, `soundfile`, `sox`, `onnxruntime`, `einops`

## Developer setup (Base vs Full)

Use project-local dependencies only.

- Node packages: install into `./node_modules` with `npm install`
- Rust toolchain is required for Tauri (`cargo` + `rustc` on PATH)
- Avoid global installs like `npm install -g ...` or `pip install ...` outside project envs

### 0) Windows prerequisites (winget)

Install machine-level tools once:

```powershell
winget install --id Rustlang.Rustup -e
winget install --id Kitware.CMake -e
```

Verify:

```powershell
cargo --version
rustc --version
cmake --version
```

If `cmake` is installed but not found in PATH:

```powershell
$cmakeBin = "C:\Program Files\CMake\bin"
$env:Path = "$cmakeBin;$env:Path"
[Environment]::SetEnvironmentVariable("Path", "$cmakeBin;" + [Environment]::GetEnvironmentVariable("Path","User"), "User")
cmake --version
```

### 1) Base build dependencies and commands (`build-base`)

Base build uses Rust-native Pocket TTS only (no Python sidecar, no Qwen/GPU path).

Required:
- Node.js + npm
- Rust (`cargo`, `rustc`)
- CMake (required to build native Rust dependencies for Pocket TTS)

Install project deps:

```powershell
npm install
```

Run in dev mode:

```powershell
npm run desktop:dev:base
```

Build:

```powershell
npm run desktop:build:base
```

Portable build:

```powershell
npm run desktop:build:base:portable
```

Notes:
- Base runtime is English-only in current app flow.
- `desktop:build:base:portable` calls `models:bundle:kyutai`, which runs a Python helper to ensure bundled Kyutai model files exist.
- If that helper needs to prefetch missing model files, set up `tts-engine/.venv` (see Full build setup below).

### 2) Full build dependencies and commands (`build-full`)

Full build uses Python sidecar + Kyutai + optional Qwen model paths.

Required:
- Everything from Base build
- Python (recommended 3.11 or 3.12)
- Project Python venv under `tts-engine/.venv`
- `pyinstaller` for sidecar packaging

Set up Python env:

```powershell
cd tts-engine
py -3 -m venv .venv
.\.venv\Scripts\Activate.ps1
python -m pip install --upgrade pip
python -m pip install -r requirements.txt
python -m pip install -e .
python -m pip install pyinstaller
cd ..
```

If `pocket-tts` is unavailable on your package index:

```powershell
cd tts-engine
python -m pip install "git+https://github.com/kyutai-labs/pocket-tts.git"
cd ..
```

Optional Full/Qwen extras:

```powershell
winget install --id ChrisBagwell.SoX -e
```

Optional FlashAttention path (may fail depending on platform/toolchain):

```powershell
cd tts-engine
.\.venv\Scripts\Activate.ps1
python -m pip install -U flash-attn --no-build-isolation
cd ..
```

Run in dev mode:

```powershell
npm run desktop:dev:full
```

Build:

```powershell
npm run desktop:build:full
```

Portable build:

```powershell
npm run desktop:build:full:portable
```

### 3) Build command matrix

- Default dev (Full): `npm run desktop:dev`
- Base dev: `npm run desktop:dev:base`
- Full build: `npm run desktop:build:full`
- Base build: `npm run desktop:build:base`
- Full portable: `npm run desktop:build:full:portable`
- Base portable: `npm run desktop:build:base:portable`
- Full sidecar-only rebuild: `npm run sidecar:build`
- Bundle Kyutai models only: `npm run models:bundle:kyutai`

### 4) Validate desktop app end-to-end

After launching app dev mode:

1. Confirm Activity shows engine/runtime ready.
2. Keep model mode as `Kyutai Pocket TTS`.
3. Pick a Kyutai preset voice (for example `alba`) or a saved cloned voice.
4. Test **Speak Text**.
5. Test hotkey path:
   - highlight text in any app
   - press configured hotkey
6. Confirm events appear in Activity.

If no audio:
- Check OS output device and app volume.
- Check `Engine Health` for active backend/runtime.
- Use **Restart Engine** and retry.

### 5) Validate Python engine independently (Full build)

```powershell
cd tts-engine
.\.venv\Scripts\Activate.ps1
$env:SPEAK_SELECTION_ENGINE_TOKEN = "dev-token"
python -m tts_engine --server --port 8765
```

In another terminal:

```powershell
cd tts-engine
python ./scripts/smoke_test.py --token dev-token
```

One-command variant:

```powershell
cd tts-engine
python ./scripts/run_smoke_with_engine.py --token dev-token
```

### 6) Cleanup (local + optional machine tools)

Project-local cleanup:

```powershell
deactivate 2>$null
Remove-Item -Recurse -Force .\tts-engine\.venv, node_modules -ErrorAction SilentlyContinue
```

Optional machine-wide cleanup:

```powershell
winget uninstall --id ChrisBagwell.SoX -e
winget uninstall --id Kitware.CMake -e
winget uninstall --id Rustlang.Rustup -e
```

If Rustup was installed outside winget:

```powershell
rustup self uninstall -y
```

---

## Roadmap

### Phase 2 - performance & quality
- Better chunking (prosody-aware splitting)
- Robust cancellation + "skip sentence"
- Per-app capture improvements and fallbacks
- Multi-voice quick switching
- Optional model caching policies and cleanup UI

### Phase 3 - portability & runtimes
- Quantization support (optional)
- Alternative runtimes (e.g., ONNX Runtime / other native backends)
- Additional models (e.g., smaller CPU-first engines)
- GPU acceleration improvements (Windows + macOS)

### Phase 4 - mobile support (future)
- Android: Accessibility Service-based selection reading + offline TTS
- iOS: likely via Share Sheet / clipboard / in-app reader modes (OS limitations)
- Shared "Engine API" concepts across platforms

---

## Project docs
- `model_registry.json` - draft registry for bundled default + on-demand model metadata (source, distribution mode, runtime notes)
- `docs/DESIGN_SPEC.md` - system design, components, storage, packaging, milestones
- `docs/IPC_API.md` - concrete API contract (HTTP/WS), schemas, errors, streaming events

---

## License
TBD (project). Bundled/on-demand models retain their original licenses. The default bundled model is Kyutai Pocket TTS (`Verylicious/pocket-tts-ungated`, Apache-2.0). Qwen models are downloaded on demand.
