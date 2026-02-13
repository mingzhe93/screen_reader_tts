# Speak Selection

Speak Selection is a lightweight, offline-first desktop app that reads aloud the text you highlight in any application.

It's designed to improve readability and accessibility (especially for people with dyslexia) by turning selected text into natural-sounding speech using local, open-weight TTS models - with **no cloud dependency**.

## What it does (Phase 1 target)
- Runs in the background (tray/menu bar)
- Reads out **highlighted text** from the active application via a hotkey
- Uses **accessibility APIs first** (Windows UIA, macOS AX), with a **clipboard fallback**
- Supports **voice cloning**:
  - Clone a voice once from a short audio sample
  - Save the cloned voice locally
  - Reuse it for all future speech generation
- Works fully offline by default:
  - Bundles **Qwen3-TTS-12Hz-0.6B-Base** in the installer/app
  - Additional models can be downloaded on-demand later

## Why this exists
Browser TTS extensions are often slow, inconsistent, and limited in voice quality. Meanwhile, modern TTS models can produce far more natural speech. Speak Selection aims to bring that quality to a simple "highlight -> hotkey -> listen" workflow, locally and privately.

## Core principles
- **Offline-first & private**: everything runs on-device
- **Accessibility-first**: selection capture via OS accessibility APIs before clipboard fallback
- **Model-swappable**: clean backend interface so we can add/replace models over time
- **Fast perceived latency**: chunked generation + immediate playback

---

## Current implementation (end of Phase 1 milestone)
At the end of Phase 1, the project will include:

### Desktop app (Tauri)
- Tray/menu bar app
- Global hotkeys:
  - Read selection
  - Pause/Resume
  - Stop
- Selection capture:
  - Windows UI Automation (UIA) primary
  - macOS Accessibility (AX) primary
  - Clipboard copy/restore fallback
- Audio playback of generated chunks

### Local engine service (Python)
- Local daemon process (kept warm)
- Loads the bundled TTS model once
- Phase 1 runtime target: CUDA + `torch.bfloat16` with `attn_implementation="flash_attention_2"` when available
- Windows fallback path: CUDA + BF16 + `attn_implementation="sdpa"` if FlashAttention 2 is unavailable
- Provides an IPC API for:
  - `speak` (chunked synthesis)
  - `cancel`
  - voice cloning + voice listing/deletion
- Voice cloning (Qwen Base model):
  - Create reusable voice prompt once
  - Persist it locally as a "Voice Profile"
  - Reuse by `voice_id` for later synthesis

### Bundling
- App bundles **Qwen3-TTS-12Hz-0.6B-Base** by default
- On first run, the model pack is extracted locally and verified
- Optional models are downloaded later via a model registry + checksum verification

---

## Default model sources
- Hugging Face model: [Qwen/Qwen3-TTS-12Hz-0.6B-Base](https://huggingface.co/Qwen/Qwen3-TTS-12Hz-0.6B-Base)
- GitHub repo: [QwenLM/Qwen3-TTS](https://github.com/QwenLM/Qwen3-TTS)

## Qwen runtime baseline (from upstream `pyproject.toml`)
- Recommended environment: isolated Python 3.12 env
- Version pins:
  - `transformers==4.57.3`
  - `accelerate==1.12.0`
- Additional runtime deps:
  - `gradio`, `librosa`, `torchaudio`, `soundfile`, `sox`, `onnxruntime`, `einops`

## Developer setup (minimal)

Use project-local dependencies only.

- Node packages: install into `./node_modules` with `npm install`
- Python packages: install into `./.venv` (never system Python)
- Avoid global installs like `npm install -g ...` or `pip install ...` outside `.venv`
- Phase 1 inference target is CUDA + BF16. Prefer FlashAttention 2; allow CUDA SDPA fallback on Windows if FlashAttention 2 cannot be installed.
- Phase 1 engine validation target is NVIDIA CUDA.

### 1) Install local dependencies

```powershell
npm install
py -3 -m venv .venv
.\.venv\Scripts\Activate.ps1
python -m pip install --upgrade pip
python -m pip install -r tts-engine/requirements.txt
python -m pip install qwen-tts pyinstaller
# Optional perf path (may fail on Windows toolchains):
python -m pip install -U flash-attn --no-build-isolation
```

### 2) Run engine + app in dev mode

Terminal A (engine, optional standalone debug mode):

```powershell
.\src-tauri\binaries\tts-engine-x86_64-pc-windows-msvc.exe --server
```

Terminal B (desktop app):

```powershell
npm run desktop:dev
```

If the app is configured to launch the sidecar automatically, you only need Terminal B.

### 3) Uninstall local dependencies

```powershell
deactivate 2>$null
Remove-Item -Recurse -Force .venv, node_modules -ErrorAction SilentlyContinue
```

This removes project-local dependencies without touching machine-wide toolchains.

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
- `model_registry.json` - draft registry format for on-demand model downloads (artifact URLs, checksums, runtime constraints)
- `docs/DESIGN_SPEC.md` - system design, components, storage, packaging, milestones
- `docs/IPC_API.md` - concrete API contract (HTTP/WS), schemas, errors, streaming events

---

## License
TBD (project). Bundled models retain their original licenses. The default bundled TTS model is Apache-2.0 licensed (Qwen3-TTS-12Hz-0.6B-Base).

