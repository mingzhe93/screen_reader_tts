# VoiceReader (Speak Selection Workflow)

VoiceReader is a lightweight, offline-first desktop app that reads aloud the text you highlight in any application.

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
  - Bundles **Kyutai Pocket TTS** in the installer/app runtime
  - Additional Qwen models can be downloaded on-demand later

## Why this exists
Browser TTS extensions are often slow, inconsistent, and limited in voice quality. Meanwhile, modern TTS models can produce far more natural speech. VoiceReader brings that quality to a simple "highlight -> hotkey -> listen" workflow, locally and privately.

## Core principles
- **Offline-first & private**: everything runs on-device
- **Accessibility-first**: selection capture via OS accessibility APIs before clipboard fallback
- **Model-swappable**: clean backend interface so we can add/replace models over time
- **Fast perceived latency**: chunked generation + immediate playback

---

## Current implementation (working dev slice)
This is what is wired right now:

### Desktop app (Tauri)
- Windowed app with a simple "Reader" page
- Global hotkey: user-configurable (default: Windows `Alt+Shift+Space`, macOS `Cmd+Shift+Space`)
- End-to-end flow: hotkey/manual speak -> `/v1/speak` -> WS stream -> local playback
- Sidecar lifecycle from app:
  - launch on startup
  - health handshake
  - restart/cancel controls
  - shutdown on app exit
- UI for:
  - model mode selection (`kyutai_pocket_tts`, `qwen_custom_voice`, `qwen_base_clone`)
  - unified voice selection (preset + saved cloned voices)
  - clone/upload, voice edit/delete, and engine health/activity pages
  - model download actions for Qwen variants

### Local engine service (Python)
- Local daemon process (kept warm)
- Loads Kyutai/Qwen model(s) from local engine data dir
- Default runtime path: Kyutai model on CPU (bundled for offline first run)
- Optional Qwen runtime path: CUDA + `torch.bfloat16` with `attn_implementation="flash_attention_2"` when available
- Windows Qwen fallback path: CUDA + BF16 + `attn_implementation="sdpa"` if FlashAttention 2 is unavailable
- Provides an IPC API for:
  - `speak` (chunked synthesis)
  - `cancel`
  - voice cloning + voice listing/deletion
- Includes a built-in default voice for first-run playback:
  - reserved `voice_id: "0"`
  - `/speak` can use this without cloning
- Warmup support and model activation endpoint are implemented

### Known limitations in this slice
- Selection capture is currently clipboard-based only (UIA/AX capture not wired yet)
- Qwen base/custom flows are available but not bundled by default (download on demand)
- Portable mode still depends on system WebView2 runtime on Windows

---

## Default model sources
- Hugging Face model: [Verylicious/pocket-tts-ungated](https://huggingface.co/Verylicious/pocket-tts-ungated)
- Hugging Face model: [Qwen/Qwen3-TTS-12Hz-0.6B-Base](https://huggingface.co/Qwen/Qwen3-TTS-12Hz-0.6B-Base)
- No-clone default voice runtime path: [Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice](https://huggingface.co/Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice)
- GitHub repo: [QwenLM/Qwen3-TTS](https://github.com/QwenLM/Qwen3-TTS)
- GitHub repo: [kyutai-labs/pocket-tts](https://github.com/kyutai-labs/pocket-tts)

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
- Python packages: install into `./tts-engine/.venv` (never system Python)
- Rust toolchain is required for Tauri (`cargo` + `rustc` on PATH)
- Avoid global installs like `npm install -g ...` or `pip install ...` outside `.venv`
- Phase 1 inference target is CUDA + BF16. Prefer FlashAttention 2; allow CUDA SDPA fallback on Windows if FlashAttention 2 cannot be installed.
- Phase 1 engine validation target is NVIDIA CUDA.

### 0) Windows toolchain prerequisites (winget)

Install machine-level tools once on Windows:

```powershell
winget install --id Rustlang.Rustup -e
winget install --id ChrisBagwell.SoX -e
```

Verify:

```powershell
cargo --version
rustc --version
sox --version
```

If `sox --version` fails after install, restart terminal first.  
If it still fails, locate and add SoX to current shell PATH:

```powershell
$allMatches = Get-ChildItem "$env:LOCALAPPDATA\Microsoft\WinGet\Packages" -Recurse -Filter sox.exe -File -ErrorAction SilentlyContinue | Select-Object -ExpandProperty FullName
$soxExe = $allMatches | Select-Object -First 1
if($soxExe){ $env:Path = "$(Split-Path $soxExe -Parent);$env:Path"; sox --version }
```

Note: the engine also auto-detects Winget SoX location on Windows, so global PATH is preferred but not strictly required.

### 1) Install project-local dependencies

```powershell
npm install
cd tts-engine
py -3 -m venv .venv
.\.venv\Scripts\Activate.ps1
python -m pip install --upgrade pip
python -m pip install -r requirements.txt
python -m pip install -e .
python -m pip install qwen-tts pocket-tts pyinstaller
# Optional perf path (may fail on Windows toolchains):
python -m pip install -U flash-attn --no-build-isolation
cd ..
```

If `pocket-tts` is unavailable on your package index, install directly from GitHub:

```powershell
cd tts-engine
python -m pip install "git+https://github.com/kyutai-labs/pocket-tts.git"
cd ..
```

If engine startup reports `No WebSocket runtime found`, run:

```powershell
python -m pip install websockets
# or
python -m pip install wsproto
```

If Qwen startup logs `'sox' is not recognized` on Windows, install SoX and reopen terminal:

```powershell
winget install --id ChrisBagwell.SoX -e
```

### 2) Run engine + app in dev mode

App-first path (recommended):

```powershell
npm run desktop:dev
```

The app launches `tts-engine` automatically and performs auth + health handshake before enabling actions:
- dev fallback: Python sidecar from `tts-engine/.venv` when needed
- packaged build: bundled sidecar runtime folder (no separate Python install required)

On Windows, sidecar startup is created with `CREATE_NO_WINDOW` so the engine does not open a floating terminal window.

Optional standalone engine debug mode:

```powershell
cd tts-engine
$env:SPEAK_SELECTION_ENGINE_TOKEN = "dev-token"
python -m tts_engine --server --port 8765
```

If you run standalone engine manually, use the Python test scripts in `tts-engine/scripts/` (see section 2.2) instead of the desktop app sidecar path.

### 2.0) Build standalone desktop installer/runtime (Windows)

This build path bundles the Python sidecar runtime and the Kyutai model files into the app package.

```powershell
npm run desktop:build:standalone
```

What this does:
- builds sidecar with PyInstaller via `tts-engine/scripts/build_sidecar.py`
- copies sidecar runtime folder to `src-tauri/binaries/tts-engine-x86_64-pc-windows-msvc/`
- ensures Kyutai model mirror exists at `src-tauri/binaries/models/Verylicious/pocket-tts-ungated`
- runs `tauri build` with bundled resources from `src-tauri/binaries/**/*`

Quick verify before packaging:

```powershell
Get-ChildItem -Recurse src-tauri\binaries\models\Verylicious\pocket-tts-ungated | Select-Object FullName
```

Runtime behavior in packaged app:
- if bundled Kyutai model exists, app sets `VOICEREADER_KYUTAI_MODEL` to that local bundled path
- otherwise it falls back to repo id (`Verylicious/pocket-tts-ungated`) and downloads into app data as needed
- sidecar uses PyInstaller `onedir` runtime layout (no onefile extraction step), which avoids temp-extraction failures on locked-down Windows machines

Output artifacts are under:
- `src-tauri/target/release/`
- `src-tauri/target/release/bundle/`

### 2.0.1) Build portable (no installer, Windows)

If some users cannot install programs, build a portable package:

```powershell
npm run desktop:build:portable
```

This creates:
- portable folder: `src-tauri/target/release/portable/VoiceReader-portable-win-x64`
- portable zip: `src-tauri/target/release/bundle/portable/VoiceReader_<version>_x64_portable.zip`

Portable contents:
- `VoiceReader.exe`
- `binaries/` (sidecar + bundled Kyutai model files)

Notes:
- no installer/admin rights required
- keep `binaries/` next to `VoiceReader.exe`
- app data/cache is still written to LocalAppData

### 2.1) Validate desktop app end-to-end flow

After `npm run desktop:dev` opens the app:

1. Confirm the Activity log shows sidecar startup and engine readiness.
2. Keep model mode as `Kyutai Pocket TTS`.
3. Pick a Kyutai preset voice (for example `alba`) and keep `voice_id=0`.
4. Test manual path with **Speak Text**.
5. Test hotkey path:
   - highlight text in any app
   - press the configured hotkey shown in the app
6. Confirm WS events (`JOB_STARTED`, `AUDIO_CHUNK`, `JOB_DONE`) appear in Activity.

If no audio plays:
- Check OS output device and app volume.
- Verify `Engine Health` shows backend `kyutai_pocket_tts` (not `mock`).
- Use **Restart Engine** and retry.

### 2.2) Validate the Python engine independently (recommended first)

Before wiring Tauri, verify the engine API behavior directly.

```powershell
cd tts-engine
py -3 -m venv .venv
.\.venv\Scripts\Activate.ps1
python -m pip install --upgrade pip
python -m pip install -r requirements.txt
python -m pip install -e .
$env:SPEAK_SELECTION_ENGINE_TOKEN = "dev-token"
python -m tts_engine --server --port 8765
```

Optional but recommended before this step (downloads Kyutai + both Qwen repos into local engine data dir):

```powershell
cd tts-engine
python ./scripts/prefetch_models.py --data-dir ./.data
```

This keeps model ownership under `tts-engine/.data`:
- model mirrors: `tts-engine/.data/models/...`
- HF cache: `tts-engine/.data/hf-cache/...`

In another terminal:

```powershell
$h = @{ Authorization = "Bearer dev-token" }
Invoke-RestMethod -Method GET -Uri "http://127.0.0.1:8765/v1/health" -Headers $h
Invoke-RestMethod -Method GET -Uri "http://127.0.0.1:8765/v1/voices" -Headers $h
Invoke-RestMethod `
  -Method POST `
  -Uri "http://127.0.0.1:8765/v1/speak" `
  -Headers $h `
  -ContentType "application/json" `
  -Body '{"voice_id":"0","text":"Engine first-run test with default voice zero."}'
```

Notes:
- For browser-like WS clients that cannot set HTTP headers, use `Sec-WebSocket-Protocol` fallback as defined in `docs/IPC_API.md`.
- Query-string auth tokens are intentionally not used.
- Check `/v1/health` -> `runtime.backend`:
  - `kyutai_pocket_tts` means Kyutai model inference is active
  - `qwen_custom_voice` means Qwen model inference is active
  - `mock` means fallback backend is active
- Current clone support: Kyutai backend supports cloned voices (`voice_id=<uuid>`). Qwen custom path remains preset/default-voice focused.
- Warmup endpoint exists: `POST /v1/warmup` (use `{"wait":true}` on startup). Model-switch flows can use `POST /v1/models/activate` to reload + warm up in one call.

One-command variant (starts engine on a free port, runs smoke checks, then shuts it down):

```powershell
cd tts-engine
python ./scripts/run_smoke_with_engine.py --token dev-token
```

Require real backend during smoke test:

```powershell
cd tts-engine
python ./scripts/run_smoke_with_engine.py --token dev-token --synth-backend kyutai
```

To test chunked streaming playback audibly (default voice, no cloning):

```powershell
cd tts-engine
python ./scripts/stream_play_queue_test.py --base-url http://127.0.0.1:8765 --token dev-token --voice-id 0 --chunk-max-chars 160 --prefetch-queue-size 5 --start-playback-after 2
```

One-command audible playback test (auto start and stop engine):

```powershell
cd tts-engine
python ./scripts/run_stream_play_with_engine.py --token dev-token --voice-id 0
```

This flow uses queue buffering + warmup by default:
- `ChunkMaxChars=160`
- `PrefetchQueueSize=5`
- `StartPlaybackAfter=2`
- warmup request (`/v1/warmup`, `wait=true`) before speak

Optional playback controls in this script:
- `--rate` (`0.25` to `4.0`) and `--volume` (`0.0` to `2.0`) are applied engine-side per chunk.
- `--pitch` is currently reserved (accepted but no-op in Phase 1 runtime).

To require real model inference (fail if Kyutai backend cannot load):

```powershell
cd tts-engine
python ./scripts/run_stream_play_with_engine.py --token dev-token --voice-id 0 --synth-backend kyutai
```

Performance note:
- Long pauses between chunks are expected in CPU mode and under sequential chunk generation.
- For best latency, run Qwen on CUDA (`QwenDeviceMap=cuda:0`, `QwenDtype=bfloat16`) and inspect timing logs from `stream_play_queue_test.py`.
- Queue buffering defaults in `run_stream_play_with_engine.py` are tuned for smoother playback (`PrefetchQueueSize=5`, `StartPlaybackAfter=2`).
- Cancel behavior: `POST /v1/cancel` is chunk-boundary based; if cancel arrives during chunk generation, that in-flight chunk is dropped and the stream ends with `JOB_CANCELED`.

### 3) Cleanup (local + optional machine tools)

Project-local cleanup:

```powershell
deactivate 2>$null
Remove-Item -Recurse -Force .\tts-engine\.venv, node_modules -ErrorAction SilentlyContinue
```

This removes project-local dependencies without touching machine-wide toolchains.

Optional machine-wide cleanup (if you no longer need these dev tools):

```powershell
winget uninstall --id ChrisBagwell.SoX -e
winget uninstall --id Rustlang.Rustup -e
```

If Rustup was installed outside winget, use:

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
