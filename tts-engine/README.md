# tts-engine (Python sidecar for Full build)

This is the Phase 1 local engine service for VoiceReader.

## Build profile context

VoiceReader now ships in two desktop profiles:

- **Base build** (`build-base`): Rust-native Kyutai runtime only, no Python sidecar, no Qwen/GPU path, and English-only synthesis (`languages: ["en"]`).  
  This is the lightweight path (portable package is around ~200 MB).
- **Full build** (`build-full`): Python sidecar + Kyutai + Qwen model switching/downloads.

This `tts-engine` folder is used by the **Full build** profile.

Current scope:
- HTTP + WebSocket API contract from `docs/IPC_API.md`
- Bearer-token auth for HTTP and WS
- WS headerless fallback auth via `Sec-WebSocket-Protocol: auth.bearer.v1, <token>`
- Voice profile persistence (`voices/<voice_id>/meta.json` + `prompt.safetensors`)
- Speak job lifecycle (`/speak`, `/cancel`, WS events)
- Built-in first-run voice (`voice_id: "0"`) so `/speak` works without cloning
- Real Kyutai Pocket TTS inference path for `voice_id: "0"` and saved cloned voices
- Real Qwen 0.6B custom-voice inference path for `voice_id: "0"` (when runtime deps are available)
- Automatic fallback to mock audio backend when Kyutai/Qwen runtime is unavailable (in `auto` mode)
- Kyutai language support in current app flow is English-only

Not implemented yet:
- Qwen cloned-voice inference path
- Optional ASR transcription flow

## Run locally

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

If `pocket-tts` is unavailable on your package index:

```powershell
python -m pip install "git+https://github.com/kyutai-labs/pocket-tts.git"
```

Optional runtime controls:

```powershell
# backend selection: auto | kyutai | qwen | mock
$env:VOICEREADER_SYNTH_BACKEND = "auto"

# Kyutai runtime settings (used when backend is auto/kyutai)
$env:VOICEREADER_KYUTAI_MODEL = "Verylicious/pocket-tts-ungated"
$env:VOICEREADER_KYUTAI_VOICE_PROMPT = "alba"
$env:VOICEREADER_KYUTAI_SAMPLE_RATE = "24000"

# Qwen runtime settings (used when backend is auto/qwen)
$env:VOICEREADER_QWEN_MODEL = "Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice"
$env:VOICEREADER_QWEN_SPEAKER = "Ryan"
$env:VOICEREADER_QWEN_DEVICE_MAP = "cuda:0"
$env:VOICEREADER_QWEN_DTYPE = "bfloat16"
$env:VOICEREADER_QWEN_ATTN_IMPLEMENTATION = "flash_attention_2"

# Warmup behavior
$env:VOICEREADER_WARMUP_ON_STARTUP = "true"
$env:VOICEREADER_WARMUP_TEXT = "Engine warmup sentence."
$env:VOICEREADER_WARMUP_LANGUAGE = "auto"
```

## Own local model/cache directory (`./.data`)

By default, model/cache ownership now stays inside engine data dir:
- Hugging Face cache defaults to `./.data/hf-cache`
- Local model mirrors are under `./.data/models/<org>/<repo>`

Prefetch model repos (recommended once before app integration):

```powershell
cd tts-engine
python ./scripts/prefetch_models.py --data-dir ./.data
```

What gets downloaded:
- `Verylicious/pocket-tts-ungated`
- `Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice`
- `Qwen/Qwen3-TTS-12Hz-0.6B-Base`

Runtime behavior:
- Keep `VOICEREADER_KYUTAI_MODEL=Verylicious/pocket-tts-ungated` for fast default read-aloud.
- Engine will prefer local mirror path under `./.data/models/...` when present.
- For `Verylicious/pocket-tts-ungated`, engine materializes `voicereader-pocket-tts.yaml` inside that model folder and loads Pocket TTS from local files.
- Kyutai supports default `voice_id: "0"` and saved cloned voice UUIDs.

If `-SynthBackend qwen` fails with `Torch not compiled with CUDA enabled`, your env has a CPU-only torch build.

CPU-only test path:

```powershell
$env:VOICEREADER_QWEN_DEVICE_MAP = "cpu"
$env:VOICEREADER_QWEN_DTYPE = "float32"
```

GPU path:
- reinstall torch in this `.venv` using a CUDA wheel from the official PyTorch index that matches your platform.
- then re-run with:

```powershell
$env:VOICEREADER_QWEN_DEVICE_MAP = "cuda:0"
$env:VOICEREADER_QWEN_DTYPE = "bfloat16"
```

If startup exits with `No WebSocket runtime found`, install one:

```powershell
python -m pip install websockets
# or
python -m pip install wsproto
```

On Windows, for better `rate` quality (pitch-preserving tempo):

```powershell
winget install --id ChrisBagwell.SoX -e
sox --version
```

If `sox` is still not found, restart terminal. The engine also checks Winget SoX install location automatically.

## Quick API check

```powershell
Invoke-RestMethod `
  -Method GET `
  -Uri "http://127.0.0.1:8765/v1/health" `
  -Headers @{ Authorization = "Bearer dev-token" }

Invoke-RestMethod `
  -Method POST `
  -Uri "http://127.0.0.1:8765/v1/speak" `
  -Headers @{ Authorization = "Bearer dev-token" } `
  -ContentType "application/json" `
  -Body '{"voice_id":"0","text":"Hello from default voice zero."}'
```

Check `health.runtime` to confirm whether real Kyutai/Qwen backend is active or mock fallback is being used.
Use `POST /v1/warmup` for explicit warmup, or `POST /v1/models/activate` to switch/reload model settings and warm up in one call.

If `POST /v1/models/activate` returns `409`, inspect the JSON body:
- `code=JOB_IN_PROGRESS`: an active speak job is running; cancel/wait and retry.
- `code=MODEL_NOT_READY`: backend load failed; check `runtime.detail` in `/v1/health` and confirm dependencies/files.

Current real-inference limitation:
- `backend=kyutai_pocket_tts` supports `voice_id: "0"` and cloned voice UUIDs created via `POST /v1/voices/clone`.
- `backend=qwen_custom_voice` supports `voice_id: "0"` only (default speaker path).
- `POST /v1/voices/clone` is backend-gated; if unsupported by the active backend, the API returns `MODEL_NOT_READY`.
- Kyutai cloned voices persist prompt artifacts at `<data_dir>/voices/<voice_uuid>/prompt.safetensors`.

## Standalone smoke test (recommended)

Run the engine in one terminal:

```powershell
cd tts-engine
$env:SPEAK_SELECTION_ENGINE_TOKEN = "dev-token"
python -m tts_engine --server --port 8765
```

Run the smoke test in another terminal:

```powershell
cd tts-engine
python ./scripts/smoke_test.py --token dev-token
```

To test WS fallback auth via `Sec-WebSocket-Protocol`:

```powershell
cd tts-engine
python ./scripts/smoke_test.py --token dev-token --use-subprotocol-auth
```

One-command flow (auto start + auto stop engine):

```powershell
cd tts-engine
python ./scripts/run_smoke_with_engine.py --token dev-token
```

Force real backend for smoke test:

```powershell
cd tts-engine
python ./scripts/run_smoke_with_engine.py --token dev-token --synth-backend kyutai
```

`run_smoke_with_engine.py` starts engine on a free localhost port, waits for health, runs smoke checks, calls `/v1/quit`, and force-stops the process if needed.

## Stream + hear audio (no cloning)

With engine already running:

```powershell
cd tts-engine
python ./scripts/stream_play_queue_test.py --base-url http://127.0.0.1:8765 --token dev-token --voice-id 0 --chunk-max-chars 160 --prefetch-queue-size 5 --start-playback-after 2
```

This script:
- calls `/v1/speak` with a multi-sentence string
- streams WS `AUDIO_CHUNK` events
- plays each chunk immediately on Windows (no-op playback on non-Windows)
- prints terminal job event

Optional flags:

```powershell
# force smaller chunks to stress chunking behavior
python ./scripts/stream_play_queue_test.py --base-url http://127.0.0.1:8765 --token dev-token --voice-id 0 --chunk-max-chars 120 --prefetch-queue-size 5 --start-playback-after 2

# save the combined streamed audio
python ./scripts/stream_play_queue_test.py --base-url http://127.0.0.1:8765 --token dev-token --voice-id 0 --save-wav-path ./out_stream.wav --prefetch-queue-size 5 --start-playback-after 2

# quit engine when done
python ./scripts/stream_play_queue_test.py --base-url http://127.0.0.1:8765 --token dev-token --voice-id 0 --quit-on-done --prefetch-queue-size 5 --start-playback-after 2
```

When backend is `auto`:
- Engine tries Kyutai first, then Qwen, then mock fallback.
- If both real backends fail, engine falls back to mock audio and reports fallback details in `/v1/health`.

One-command variant (auto start engine, stream+play, auto shutdown):

```powershell
cd tts-engine
python ./scripts/run_stream_play_with_engine.py --token dev-token --voice-id 0
```

Defaults in this one-command flow:
- `ChunkMaxChars=160`
- `PrefetchQueueSize=5`
- `StartPlaybackAfter=2`
- warmup is triggered with `wait=true` before speak

Playback controls (engine-side):
- `Rate` (`0.25` to `4.0`) uses pitch-preserving time-stretch (faster/slower without raising/lowering voice pitch)
  - preferred path: SoX `tempo` effect when `sox` is available on PATH (better speech quality)
  - fallback path: librosa phase-vocoder (may introduce slight phasing/echo artifacts at high rates)
- `Volume` (`0.0` to `2.0`) applies gain to PCM
- `Pitch` is currently reserved (accepted but no-op)

Example:

```powershell
cd tts-engine
python ./scripts/run_stream_play_with_engine.py --token dev-token --voice-id 0 --rate 1.25 --volume 1.1
```

Force real backend (no mock fallback):

```powershell
cd tts-engine
python ./scripts/run_stream_play_with_engine.py --token dev-token --voice-id 0 --synth-backend qwen
```

Force CPU Qwen test (useful when CUDA torch is not installed yet):

```powershell
cd tts-engine
python ./scripts/run_stream_play_with_engine.py --token dev-token --voice-id 0 --synth-backend qwen --qwen-device-map cpu --qwen-dtype float32
```

## Performance notes (chunk pauses)

Long pauses between heard chunks can come from:
- CPU inference (`QwenDeviceMap=cpu`) which is much slower than CUDA on this model size.
- Sequential per-chunk generation: the engine generates chunk N+1 only after chunk N has completed.
- Synchronous playback in the Windows test client path to preserve chunk order.

How to diagnose quickly:
- In `stream_play_queue_test.py`, check printed timing lines:
  - `gap_since_prev` approximates model/generation delay between chunk arrivals.
  - `playback_wait` shows queue buffer wait before playback.
  - `playback_dur` is output audio duration for each chunk.
- Verify `/v1/health` reports `runtime.backend=qwen_custom_voice` and `fallback_active=false`.
- Verify `runtime.warmup.status=ready` before latency-sensitive speaks.

How to reduce pauses:
- Use CUDA-enabled torch (`QwenDeviceMap=cuda:0`, `QwenDtype=bfloat16`).
- Use queue buffering (`PrefetchQueueSize=5`, `StartPlaybackAfter=2`).
- Trigger warmup (`POST /v1/warmup` with `wait=true`) on startup and after model changes.
- Keep chunk size moderate (`ChunkMaxChars=140` to `180`) for earlier first chunk with fewer boundaries.

Cancel behavior details:
- `POST /v1/cancel` is honored at chunk boundaries.
- If cancel arrives while a chunk is generating, that in-flight chunk is dropped and not streamed.
- WS terminal event is `JOB_CANCELED`.

Example model switch + warmup call:

```powershell
Invoke-RestMethod `
  -Method POST `
  -Uri "http://127.0.0.1:8765/v1/models/activate" `
  -Headers @{ Authorization = "Bearer dev-token" } `
  -ContentType "application/json" `
  -Body '{"synth_backend":"qwen","qwen_device_map":"cuda:0","qwen_dtype":"bfloat16","warmup_wait":true,"warmup_force":true,"reason":"app_model_switch"}'
```

## Stopping the engine

- Manual run: press `Ctrl+C` in the engine terminal.
- Programmatic shutdown:

```powershell
Invoke-RestMethod `
  -Method POST `
  -Uri "http://127.0.0.1:8765/v1/quit" `
  -Headers @{ Authorization = "Bearer dev-token" }
```

## Tests

```powershell
cd tts-engine
.\.venv\Scripts\Activate.ps1
pytest -q
```

## Optional machine-level cleanup (Windows)

If you installed SoX only for local dev and want to remove it later:

```powershell
winget uninstall --id ChrisBagwell.SoX -e
```
