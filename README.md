# VoiceReader

VoiceReader reads highlighted text aloud. Highlight text in any app, press a hotkey, and listen. The speech comes from open-weight text-to-speech models that run on your own machine. Nothing is sent to a cloud service while you use it.

It is a Tauri 1.x desktop app: a Vite/TypeScript frontend and a Rust backend.

## What it can do

- Read the text selected in the active app when you press a global hotkey. The app copies the selection through the clipboard and restores your previous clipboard text afterwards.
- Show a small always-on-top toolbar in its own window while audio plays. It has rate, pause, stop and skip-forward controls, shows the source app, and remembers where you dragged it.
- Change the playback rate from `0.25x` to `4.0x` while audio is playing. Pitch is preserved when SoX is available.
- Clone a voice from a WAV clip, save it, and reuse it. Saved voices can be renamed, annotated and deleted in the Voices & Clone tab.
- Choose from 21 preset voices with Kyutai Pocket TTS.
- Run a model on the CPU, and move part of Audio8 to a GPU when that is faster (see Compute Device below).

## Models

| Model | Where it comes from | Languages | Notes |
|---|---|---|---|
| Kyutai Pocket TTS | Bundled with the app (`Verylicious/pocket-tts-ungated`) | English | Default. CPU. 21 preset voices: 8 are built in and 13 are cloned on first use from reference clips fetched by `scripts/fetch-kyutai-voices.js`. Voice cloning needs only an audio clip. |
| Audio8 TTS 0.1B | Optional in-app download from the Engine tab (`Edge0/audio8-TTS-0.1B-ONNX-INT8`, about 860 MB, of which about 400 MB is the encoder used for cloning) | English, Chinese | ONNX Runtime. Built-in voice plus cloning. Cloning needs the clip and its exact transcript; the clip must be 0.5 to 30 seconds. |
| Qwen3-TTS 0.6B (CustomVoice, Base) | Download in the Full build only | Several | Not part of the Base build. See Full build below. |

Speed, memory and measurements for Audio8 are in `docs/learnings.md` section 7. Kyutai is much faster than Audio8 for English. Audio8 is usable up to about 1.5x playback on the machine it was measured on.

## Builds

Two builds exist. They are chosen by Cargo feature.

- **Base** (`build-base`) is the product and the default. Everything runs inside the Rust process. There is no Python at run time.
- **Full** (`build-full`) starts a Python sidecar (`tts-engine/`) and adds the Qwen models. It is kept for future heavier models and is not actively used.

## Platform status

| Platform | Status |
|---|---|
| Windows | Primary platform. Selection capture sends Ctrl+C with `SendInput`. The source label comes from the foreground window title. Audio8 can decode on any DirectX 12 GPU through DirectML. |
| macOS | Implemented in code: selection capture sends Cmd+C through CGEvent, and the source label is the frontmost application's name. Core ML decoding for Audio8 is wired in but untested, so it is opt-in. |
| Linux | The hotkey cannot read a selection: simulated copy and source-window lookup are not implemented. `scripts/fetch-onnxruntime.js` has a Linux x64 download, and SoX is looked up on `PATH`, but nothing else about Linux is covered. |

## Known limitations

- Kyutai is English only. Use Audio8 for Chinese.
- Audio8 runs generation on the CPU and is close to real time. Playback above about 1.5x can stutter.
- Without SoX, rate changes use plain resampling, which also changes pitch. Windows builds bundle SoX under `src-tauri/binaries/sox`.
- Rate changes are applied to the next piece of audio the runtime produces, not to audio that is already queued in the player.
- The skip-back button does nothing yet.
- The hotkey is ignored while the VoiceReader window itself is focused. Use Read Selection Now there.
- Closing the main window quits the app. There is no tray icon.
- Text is spoken as written. URLs, markdown symbols, citation markers and emoji are not cleaned up.
- `qwen_base_clone` appears in the Full build model list, but read-aloud with it is not enabled.

## Quick start (Base build)

### Prerequisites

- Node.js and npm.
- Rust (`cargo` and `rustc` on `PATH`).
- CMake, to build native Rust dependencies.
- Python 3.10 or newer on `PATH`, only for the helper behind `npm run models:bundle:kyutai`. It copies the bundled Kyutai model into `src-tauri/binaries/models`, and downloads the model first if it is missing. The download needs `huggingface_hub`; the helper uses `tts-engine/.venv` if that exists, otherwise the Python on `PATH`.
- Internet access the first time you run a Base dev or build script. `npm run assets:fetch` runs automatically and downloads the ONNX Runtime library and the 13 extra Kyutai voice clips. It skips files that are already present.

On Windows:

```powershell
winget install --id Rustlang.Rustup -e
winget install --id Kitware.CMake -e
```

### Run

```powershell
npm install
npm run models:bundle:kyutai   # once: puts the Kyutai model under src-tauri/binaries/models
npm run desktop:dev
```

A debug build (`tauri dev`) keeps its data in `tts-engine/.data`, so the `tts-engine/` folder must exist even though the Base build does not run it. Release builds keep data under the app's local data directory instead. Set `VOICEREADER_DATA_DIR` to put it somewhere else.

To check that it works:

1. Open the Engine tab and confirm the Activity panel reports that the engine is ready.
2. On the Reader tab, press Speak Text and listen.
3. Highlight text in another app and press the hotkey shown on the Reader tab.

If there is no audio, check the OS output device, then use Refresh Health and Restart Engine on the Engine tab. The Activity panel lists the events the app received.

### Build

| Command | Result |
|---|---|
| `npm run desktop:dev` | Run the Base build in dev mode. Same as `desktop:dev:base`. |
| `npm run desktop:build` | Release executable without an installer. Same as `desktop:build:base`. |
| `npm run desktop:build:standalone` | Installer from the Tauri bundler. Same as `desktop:build:base:installer`. |
| `npm run desktop:build:portable` | Portable zip: `src-tauri/target/release/bundle/portable/VoiceReader_<version>_x64_portable.zip`. Same as `desktop:build:base:portable`. |
| `npm run assets:fetch` | Fetch the ONNX Runtime library and the extra Kyutai voice clips. Hooked into the Base dev and build scripts. |
| `npm run onnxruntime:fetch`, `npm run kyutai-voices:fetch` | The two halves of `assets:fetch`. |
| `npm run models:bundle:kyutai` | Ensure the Kyutai model (and SoX, if found on the build machine) is under `src-tauri/binaries`. Runs inside every Base build. |

The Base scripts also run `scripts/sync-version.js`, which copies the version from `package.json` into `src-tauri/Cargo.toml` and `src-tauri/tauri.conf.json`.

What belongs under `src-tauri/binaries` for each build is described in `src-tauri/binaries/README.txt`.

## Settings and environment variables

### In the app

| Setting | Where | Default | Saved |
|---|---|---|---|
| Hotkey | Reader tab | `Alt+S` on Windows, `Ctrl+Shift+S` on macOS and elsewhere | Yes, in `settings.json` in the app config directory |
| Compute Device (Auto, GPU, CPU) | Engine tab | Auto | Yes, in `settings.json` |
| Model mode and voice | Reader tab | Kyutai, built-in voice with the `alba` preset | No. Kyutai is selected on every start. |
| Rate | Reader tab, toolbar | 1.5, range 0.25 to 4.0 | No. The toolbar rate button steps by 0.25 and wraps from 4.0 to 0.25. |
| Volume | Reader tab | 1.0, range 0 to 2 | No. Applied when a job starts. |
| Chunk Max Chars | Reader tab | 200, range 100 to 200 in the UI | No |
| Theme, toolbar position | App and toolbar windows | Dark/light toggle; bottom-left of the monitor | Yes, in each window's local storage |

If the saved hotkey cannot be registered, the app falls back to the platform default above and saves that. `Alt+Space` and `Cmd+Space` are refused as OS-reserved.

Compute Device controls where Audio8's audio decoder runs. Auto uses the GPU only when a one-time benchmark shows it is at least 1.5 times faster than the CPU. GPU uses it whenever the provider loads. CPU never touches the GPU. Details are in `docs/learnings.md` section 11.

### Environment variables

All optional. The Base build reads these:

| Variable | Effect |
|---|---|
| `VOICEREADER_DATA_DIR` | Data directory for models, voices and caches. |
| `VOICEREADER_BUNDLED_KYUTAI_MODEL_DIR` | Use this folder as the bundled Kyutai model. It must contain the files listed in `src-tauri/binaries/README.txt`. |
| `VOICEREADER_SOX_PATH` | Path to the SoX executable. Otherwise SoX is looked up next to the app, then on `PATH`, then in the Windows winget packages folder. |
| `VOICEREADER_ONNXRUNTIME_PATH` | Path to the ONNX Runtime library file. Otherwise it is looked up under `binaries/onnxruntime` next to the app. There is no fallback to the system library. |
| `VOICEREADER_AUDIO8_DECODER_DEVICE` | `auto`, `gpu` or `cpu`. Overrides the Compute Device setting. |
| `VOICEREADER_AUDIO8_PARALLEL_CHUNKS` | Audio8 chunks generated at once, 1 to 8. Default 2 (1 on machines with fewer than 4 cores). |
| `VOICEREADER_AUDIO8_DECODERS` | Audio8 decoder sessions, 1 to 4. Default 1. |
| `VOICEREADER_AUDIO8_DECODER_THREADS` | Threads for the CPU decoder, 1 up to the core count. Default is half the cores, between 1 and 8. |
| `VOICEREADER_ENGINE_ROOT` | Location of the `tts-engine` folder. Debug builds use `<root>/.data` as the data directory. |
| `VOICEREADER_AUDIO8_TEST_MODEL_DIR`, `KYUTAI_TEST_MODEL_DIR`, `KYUTAI_TEST_OUT_DIR` | Used only by the ignored Rust tests. |

The Full build also reads `VOICEREADER_ENGINE_EXECUTABLE` (release builds: path to the sidecar executable) and sets the sidecar's own variables, which are listed in `tts-engine/README.md`.

## Full build

Use this only to work on the Python sidecar or the Qwen models.

- Needs everything from the Base prerequisites, plus Python 3.10 or newer, a virtual environment in `tts-engine/.venv`, and `pyinstaller` for packaging.
- Models are Kyutai plus Qwen3-TTS 0.6B CustomVoice and Base. Qwen models are downloaded from the Engine tab and run through PyTorch.
- The app starts the sidecar as a child process on a loopback port with a bearer token, talks to it over HTTP and a WebSocket, and stops it on exit. The protocol is in `docs/IPC_API.md`.

```powershell
cd tts-engine
py -3 -m venv .venv
.\.venv\Scripts\Activate.ps1
python -m pip install -r requirements.txt
python -m pip install -e .
python -m pip install pyinstaller
cd ..
npm run desktop:dev:full
```

| Command | Result |
|---|---|
| `npm run desktop:dev:full` | Dev mode with the sidecar. |
| `npm run desktop:build:full` | Release build; builds the sidecar first. |
| `npm run desktop:build:full:portable` | Portable zip that includes the sidecar. |
| `npm run sidecar:build` | Rebuild only the sidecar. |

To test the sidecar alone, see `tts-engine/README.md`.

## Project layout and docs

| Path | Contents |
|---|---|
| `src/` | Frontend: main window (`main.ts`), floating toolbar (`toolbar.ts`) and shared helpers. |
| `src-tauri/src/` | Rust backend. `voicereader_core.rs` has the Tauri commands, engine lifecycle and job streaming. The Base runtimes are `kyutai_local.rs` and `audio8_local.rs` (with `audio8_model.rs` for ONNX inference), sharing `text_chunking.rs` and `audio_pipeline.rs`. |
| `src-tauri/binaries/` | Files bundled with the app. See `src-tauri/binaries/README.txt`. |
| `tts-engine/` | Python sidecar for the Full build. |
| `scripts/` | Version sync, asset fetch scripts and portable packaging. |

| Document | Contents |
|---|---|
| `docs/DESIGN_SPEC.md` | Architecture: builds, runtimes, event flow, chunking, playback, storage. |
| `docs/DECISIONS.md` | Agreed direction, model choices, order of work. |
| `docs/learnings.md` | Measurements and reasoning behind the playback pipeline, Audio8, chunking and GPU use. |
| `docs/IPC_API.md` | The sidecar's HTTP and WebSocket API (Full build only). |

## Roadmap

Direction and order of work are in `docs/DECISIONS.md`. In short:

1. Add an ASR tab (speech to text), including meetings with speaker labels. English only at first. This is next.
2. Later: the Audio8 0.6B INT4 model as an on-demand multilingual download. Not scheduled yet.

Kyutai Pocket TTS stays the default model; Audio8 stays optional.

Not planned right now: running Audio8's generation step on a GPU (it measured slower there) and other languages for Kyutai.

## License

Project license: MIT. Third-party components and models keep their own licenses. The extra Kyutai voice clips are credited in `src-tauri/binaries/kyutai-voices/ATTRIBUTION.txt`, which the fetch script writes.
