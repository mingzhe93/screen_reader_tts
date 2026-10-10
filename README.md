# VoiceReader

Version 0.2.3. A desktop app that reads highlighted text aloud and turns speech into a transcript with speaker labels.

- **Read aloud.** Highlight text in any app, press a hotkey, and hear it spoken. The voices are open-weight text-to-speech models, with voice cloning from a short clip.
- **Transcribe.** Record from the microphone, or pick a recording, and get a transcript that labels who spoke. It tells up to 8 speakers apart. English only.
- **Local.** The models run on your own computer. After the one-time model downloads, nothing is sent to a cloud service while you use it.
- **Platforms.** Windows and macOS (Apple Silicon, macOS 14 or newer) work. Audio8 GPU decoding uses DirectML on Windows and native WebGPU/Metal on Apple Silicon macOS. The FP32 Metal decoder has passed listening and waveform checks on an M2 Max. On Linux the hotkey flow does not work (see [Platform status](#platform-status)).
- **Stack.** A Tauri 1.x desktop app: a Vite/TypeScript frontend and a Rust backend.

## How to use

1. Go to the [latest release](https://github.com/mingzhe93/screen_reader_tts/releases/latest) and download the portable ZIP for your computer:
   - **macOS (Apple Silicon):** `VoiceReader_macos_arm64_portable.zip`
   - **Windows (x64):** `VoiceReader-portable-win-x64.zip`
2. Extract the ZIP and open VoiceReader. On macOS, open `VoiceReader.app` and allow **Accessibility** access in System Settings > Privacy & Security so it can read highlighted text. Allow **Microphone** access when prompted if you want to record.
3. To read text aloud, **highlight text in another app and press the hotkey** shown on the Read aloud page. The defaults are **Ctrl+Shift+S on macOS** and **Alt+S on Windows**. Keep the source app focused when you press it. Kyutai's English voices are included, and a floating toolbar appears while reading.
4. For **Chinese text**, first open **Models** and download **Audio8 TTS**, then select Audio8 on the Read aloud page. On Apple Silicon macOS, Auto benchmarks CPU against Metal decoding; you can also select GPU or CPU in Settings.
5. For **transcription**, first open **Models** and download **Transcription**. Then open **Transcribe** to record and transcribe your microphone in real time, or choose an existing recording. Transcription currently supports English only.
6. After transcribing, choose **Copy**, **Transcription** (text, Markdown or SRT), or **Recording & transcription** (a ZIP containing the audio and a text transcript). Stop a live recording before exporting it, and save it before starting another transcription or closing the app.

To build from source, see [Development setup](#development-setup-base-build).

### First launch on macOS

VoiceReader is free to use. The macOS app is ad hoc signed, but is not Developer ID
signed or notarized by Apple, so macOS may show **“VoiceReader.app Not Opened”** or
**“Apple could not verify VoiceReader.app is free of malware.”**

If you downloaded VoiceReader from this repository's GitHub release and want to open it:

1. Click **Done** in the warning.
2. Open **System Settings → Privacy & Security**.
3. Scroll to the **Security** section and click **Open Anyway** for VoiceReader.
4. Authenticate if prompted, then confirm **Open**.

This creates an exception for this copy of VoiceReader. A later downloaded version
may require the same steps again. Keep Gatekeeper enabled; there is no need to
disable macOS security protections globally.

[Apple's instructions for opening an app from an unidentified developer](https://support.apple.com/en-us/102445).

## What it can do

- Read the text selected in the active app when you press a global hotkey. The app copies the selection through the clipboard and restores your previous clipboard text afterwards.
- Show a small always-on-top toolbar in its own window while audio plays. It has rate, pause, stop and skip-forward controls, shows the source app, and remembers where you dragged it.
- Change the playback speed from `0.25x` to `4.0x` while audio is playing. The change is heard at once, and the pitch stays the same.
- Clone a voice from a WAV clip, save it, and reuse it. The Transcribe audio button fills in the clip's transcript for you (English only; it uses the transcription model and offers to download it if it is missing). Saved voices can be renamed, annotated and deleted on the Voices page.
- Choose from 21 preset voices with Kyutai Pocket TTS.
- Run models on the CPU, and move Audio8's codec decoder to a GPU when that is faster: DirectML on Windows, or WebGPU/Metal on Apple Silicon macOS (see Compute device below).
- Transcribe speech on the Transcribe page, with a label per speaker and the transcript shown while it is produced. Press Record to transcribe the microphone live, or choose or drop a WAV, MP3, M4A, MP4, FLAC or OGG Vorbis file. Each of the 8 speakers has its own label colour. Speakers can be renamed, and the transcript can be copied or exported as text, Markdown or SRT subtitles. English only.

The main window has a sidebar with five pages: Read aloud, Voices, Transcribe, Models and Settings.

## Models

Every model is an open-weight model from Hugging Face. The table lists the repository the app downloads, the original model it comes from, and the licence stated on the repository. Check each model card before you reuse a model in your own project; the licences differ.

| Used for | Repository the app uses | Original model | Licence | Size | Delivery |
|---|---|---|---|---|---|
| Text to speech (default) | [Verylicious/pocket-tts-ungated](https://huggingface.co/Verylicious/pocket-tts-ungated), an ungated copy of Kyutai Pocket TTS | [kyutai/pocket-tts](https://huggingface.co/kyutai/pocket-tts) | CC BY 4.0 | About 230 MB | Bundled with the app |
| Extra Kyutai preset voices (13 reference clips) | [kyutai/tts-voices](https://huggingface.co/kyutai/tts-voices) | The same | Per clip: CC BY 4.0 (VCTK) or CC0 (LibriVox) | Small WAV files | Fetched at build time by `scripts/fetch-kyutai-voices.js`, then bundled |
| Text to speech (optional): Audio8 TTS 0.1B | [Edge0/audio8-TTS-0.1B-ONNX-INT8](https://huggingface.co/Edge0/audio8-TTS-0.1B-ONNX-INT8), an ONNX INT8 export | [Audio8/Audio8-TTS-Preview-0.1b](https://huggingface.co/Audio8/Audio8-TTS-Preview-0.1b) | Apache-2.0 (the ONNX repository) | About 860 MB | Downloaded on demand from the Models page |
| Speech to text: Parakeet multitalker | [Recogment/parakeet-multitalker-int8-onnx](https://huggingface.co/Recogment/parakeet-multitalker-int8-onnx), an int8 ONNX export | [nvidia/multitalker-parakeet-streaming-0.6b-v1](https://huggingface.co/nvidia/multitalker-parakeet-streaming-0.6b-v1) | NVIDIA Open Model License | About 666 MB | Downloaded on demand from the Models or Transcribe page |
| Speaker diarizer: Nemotron-3 Diarization (Streaming Sortformer v3) | The `nemotron-3-diarization` folder of [altunenes/parakeet-rs](https://huggingface.co/altunenes/parakeet-rs/tree/main/nemotron-3-diarization) | [nvidia/Nemotron-3-Diarization](https://huggingface.co/nvidia/Nemotron-3-Diarization) | OpenMDW-1.1 | About 400 MB | Downloaded on demand, together with the speech model |
| Full build only: Qwen3-TTS 0.6B | [Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice](https://huggingface.co/Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice) and [Qwen/Qwen3-TTS-12Hz-0.6B-Base](https://huggingface.co/Qwen/Qwen3-TTS-12Hz-0.6B-Base) | The same | Apache-2.0 | Not measured | Downloaded on demand from the Models page (Full build only) |

Notes on the table:

- The two transcription models are one download of about 1.07 GB. Start it from the Models page or the Transcribe page. The download resumes where it stopped.
- The 13 extra Kyutai voices are reference clips from `kyutai/tts-voices`. They are credited in `src-tauri/binaries/kyutai-voices/ATTRIBUTION.txt`, which the fetch script writes (the folder is not committed).
- Kyutai Pocket TTS is English only, with 21 preset voices: 8 are built in and the 13 extra ones are cloned on first use from the reference clips. Voice cloning needs only an audio clip.
- Audio8 TTS handles English and Chinese. It has a built-in voice and cloning. Cloning needs the clip and its exact transcript, and the clip must be 0.5 to 30 seconds. About 400 MB of its download is the encoder used for cloning.
- Transcription is English only. It runs on the CPU and is loaded only while a recording is being transcribed (about 1.3 GB of memory).
- Qwen3-TTS is listed in the Full build only. It is not part of the Base build. See [Full build](#full-build).

Runtimes the models run on:

- [ONNX Runtime](https://github.com/microsoft/onnxruntime) runs Audio8 and the transcription models. The app loads its shared library at run time; `scripts/fetch-onnxruntime.js` downloads it. Apple Silicon builds also fetch a native WebGPU plugin with `scripts/fetch-webgpu-macos.js`.
- [`pocket-tts`](https://github.com/babybirdprd/pocket-tts) is the Rust crate that runs Kyutai Pocket TTS.
- [`parakeet-rs`](https://github.com/altunenes/parakeet-rs) is the Rust crate that runs the transcription models. A patched copy is vendored in `src-tauri/vendor/parakeet-rs` (see Licence).

Speed, memory and measurements are in [the learnings](docs/learnings.md): Audio8 CPU in section 7, Windows GPU in section 11, transcription in section 12, and macOS Metal in section 15. Kyutai is much faster than Audio8 for English. Audio8 CPU playback above about 1.5x can exhaust the buffer on the measured machines. On the M2 Max, Metal supplied about 2.58 seconds of speech per second at the 2x setting, but bursty delivery can still cause early refilling; continuous 2x playback in the packaged app has not been measured.

## Builds

Two builds exist. They are chosen by Cargo feature, and exactly one must be enabled.

- **Base** (`build-base`) is the product and the default for the npm scripts. Everything runs inside the Rust process. There is no Python at run time.
- **Full** (`build-full`) starts a Python sidecar (`tts-engine/`) and adds the Qwen models. It is kept for future heavier models and is not actively used.

The Base build includes native Metal decoding on Apple Silicon macOS from v0.2.3. It uses the same Rust/ONNX backend, with no Python or MLX runtime in the app.

## Platform status

| Platform | Status |
|---|---|
| Windows | Primary and tested platform. Selection capture sends Ctrl+C with `SendInput`. The source label comes from the foreground window title. Audio8 can decode on any DirectX 12 GPU through DirectML. |
| macOS | Working on Apple Silicon with macOS 14 or newer. The portable app, model downloads, highlighted-text hotkey and floating playback toolbar have been tested. Selection capture requires Accessibility permission; microphone recording requires Microphone permission. The player preserves pitch when changing speed; bundled SoX prepares cloning clips and supports the older backend speed path. Audio8 supports FP32 Metal GPU decoding from v0.2.3, validated on an M2 Max running macOS 27.0.1. Other Mac chips/OS versions, and Intel/universal releases, still need verification. Intel uses CPU. |
| Linux | Not supported for the hotkey flow: simulated copy and source-window lookup are not implemented. `scripts/fetch-onnxruntime.js` has a Linux x64 download, and SoX is looked up on `PATH`, but nothing else about Linux is covered. |

## Known limitations

- Kyutai is English only. Use Audio8 for Chinese.
- Audio8's autoregressive generation stays on the CPU. Faster playback consumes audio faster without increasing generation throughput; above about 1.5x, the measured CPU setups can stutter. GPU decoding improves overall supply, but does not guarantee continuous playback at every speed.
- macOS Metal decoding uses FP32 computation with the validated Audio8 export. Load/warm-up failures fall back to CPU. Other model exports and Mac hardware/OS combinations still need validation.
- SoX is no longer used to change the speed (the player does that itself). It is still bundled, for preparing voice-cloning clips and for the older speed path kept behind `VOICEREADER_RATE_IN_BACKEND`: Windows builds bundle it under `src-tauri/binaries/sox`, macOS builds under `src-tauri/binaries/sox-macos`.
- The skip-back button does nothing yet.
- The hotkey is ignored while the VoiceReader window itself is focused. Use Read selection there.
- Closing the main window quits the app. There is no tray icon.
- Text is spoken as written. URLs, markdown symbols, citation markers and emoji are not cleaned up.
- `qwen_base_clone` appears in the Full build model list, but read-aloud with it is not enabled.
- Live transcription hears the microphone only. System audio (the other side of a call) is not captured. After stopping, choose **Recording & transcription** to save a ZIP with the mono WAV recording (at the microphone’s native sample rate) and a text transcript. For uploaded files, the ZIP includes the original recording.
- Live text appears about one to two seconds behind the speaker.
- Transcription is English only. It labels up to eight speakers. The speech model was trained on up to four, so with more than four expect some sentences under the wrong speaker or under two speakers at once. Anyone beyond the Speakers setting is still transcribed, together, as "Unknown speaker". With more than eight voices in a recording, the extra voices cannot be told apart from the first eight and appear under their labels.
- When people talk over each other for long stretches, some words are dropped or given to the wrong speaker. When one person takes over from another with no gap, a word or two at the changeover can appear under both speakers. The first word or two of a new speaker is sometimes missing.
- Opus audio (most `.webm` and `.opus` files) cannot be read. Convert it to one of the supported formats first.
- The transcript and unsaved microphone recording are kept for the current session, until the next transcription starts or the app closes. **Copy** copies the transcript; **Transcription** saves TXT, Markdown or SRT; **Recording & transcription** saves both files in a ZIP after transcription finishes.
- Transcription speed has been measured on a desktop CPU only; a laptop CPU has not been measured.

## Development setup (Base build)

### Prerequisites

- Node.js and npm.
- Rust (`cargo` and `rustc` on `PATH`). On Windows this also needs the Tauri 1.x prerequisites (C++ build tools and WebView2); see the [Tauri prerequisites guide](https://tauri.app/v1/guides/getting-started/prerequisites).
- CMake, to build native Rust dependencies.
- Python 3.10 or newer on `PATH`, only for build helpers. `npm run models:bundle:kyutai` copies the bundled Kyutai model into `src-tauri/binaries/models`, and downloads it first if missing. The download needs `huggingface_hub`; the helper uses `tts-engine/.venv` if that exists, otherwise the Python running the helper. macOS builds also use Python to build/bundle native SoX and package the ZIP; see the macOS instructions below.
- Internet access the first time you run a Base dev or build script. `npm run assets:fetch` runs automatically and downloads the ONNX Runtime library, the native WebGPU plugin on Apple Silicon macOS, and the 13 extra Kyutai voice clips. It skips files that are already present.

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

1. Check the status at the bottom of the sidebar: it shows the model and device once the engine is ready.
2. On the Read aloud page, press Speak and listen.
3. Highlight text in another app and press the hotkey shown on the Read aloud page.
4. To try transcription, open the Transcribe page and download the model (about 1.07 GB). Then press Record, or choose a recording.

If there is no audio, check the OS output device, then use Refresh health and Restart engine under Diagnostics on the Settings page. The activity log there lists the events the app received.

### Build

| Command | Result |
|---|---|
| `npm run desktop:dev` | Run the Base build in dev mode. Same as `desktop:dev:base`. |
| `npm run desktop:build` | Release executable without an installer. Same as `desktop:build:base`. |
| `npm run desktop:build:standalone` | Installer from the Tauri bundler. Same as `desktop:build:base:installer`. |
| `npm run desktop:build:portable` | Portable zip: `src-tauri/target/release/bundle/portable/VoiceReader_<version>_x64_portable.zip`. Same as `desktop:build:base:portable`. Windows only. |
| `npm run desktop:build:macos:portable` | Base macOS `.app` and versioned portable ZIP, including Metal on Apple Silicon. The packager ad hoc signs and verifies the complete app. |
| `npm run assets:fetch` | Fetch ONNX Runtime, WebGPU/Metal on Apple Silicon, and extra Kyutai voice clips. Hooked into the Base dev and build scripts. |
| `npm run onnxruntime:fetch`, `node scripts/fetch-webgpu-macos.js`, `npm run kyutai-voices:fetch` | The individual steps of `assets:fetch`; the Metal plugin step skips other platforms. |
| `npm run models:bundle:kyutai` | Ensure the Kyutai model and platform SoX runtime are under `src-tauri/binaries`. On macOS, builds native SoX and fails if it cannot be bundled. Elsewhere, copies SoX when found and warns if missing. Runs inside every Base build. |

The Base scripts also run `scripts/sync-version.js`, which copies the version from `package.json` into the desktop and Python engine metadata and the app entries in both lockfiles.

The build scripts produce versioned ZIP filenames. When publishing a release, name the uploaded assets `VoiceReader-portable-win-x64.zip` and `VoiceReader_macos_arm64_portable.zip` to match the download instructions above.

What belongs under `src-tauri/binaries` for each build is described in `src-tauri/binaries/README.txt`.

## Settings and environment variables

### In the app

| Setting | Where | Default | Saved |
|---|---|---|---|
| Hotkey | Read aloud page | `Alt+S` on Windows, `Ctrl+Shift+S` on macOS and elsewhere | Yes, in `settings.json` in the app config directory |
| Compute device (Auto, GPU, CPU) | Settings page. Shown only where a GPU provider is available. | Auto | Yes, in `settings.json` |
| Model and voice | Read aloud page | Kyutai, built-in voice with the `alba` preset | No. Kyutai is selected on every start. |
| Speed | Read aloud page, toolbar | 1.5, range 0.25 to 4.0 | No. The toolbar rate button steps by 0.25 and wraps from 4.0 to 0.25. |
| Volume | Settings page | 1.0, range 0 to 2 | No. Applied when a job starts. |
| Chunk max chars | Settings page | 200, range 100 to 200 in the UI | No |
| Speakers (transcription) | Transcribe page | 8, range 1 to 8 | No |
| Microphone (transcription) | Transcribe page | System default input | No |
| Theme, toolbar position | Settings page and toolbar window | Dark, with a dark/light toggle; toolbar at the bottom-left of the monitor | Yes, in each window's local storage |

If the saved hotkey cannot be registered, the app falls back to the platform default above and saves that. `Alt+Space` and `Cmd+Space` are refused as OS-reserved.

Compute device controls where Audio8's codec decoder runs. It does not affect Kyutai, voice registration, Audio8's autoregressive generation or transcription. Transcription stays on CPU; the Windows DirectML test measured no improvement, and Mac GPU transcription has not been benchmarked.

On Windows and Apple Silicon macOS, Auto uses GPU only when a decoder benchmark shows more than a 1.5x speedup over CPU. GPU skips that benchmark; CPU never initializes a GPU decoder. Auto choices are cached for 7 days: `audio8-decoder-device.json` on Windows and `audio8-decoder-device-webgpu-fp32.json` on Apple Silicon. The new Mac cache avoids reusing preview/other-provider measurements. Intel macOS uses CPU. Load/warm-up failures fall back to CPU, but there is no automatic recovery from a GPU failure in the middle of a job. See [learnings](docs/learnings.md) sections 11 and 15.

### Environment variables

All optional. The Base build reads these:

| Variable | Effect |
|---|---|
| `VOICEREADER_DATA_DIR` | Data directory for models, voices and caches. |
| `VOICEREADER_BUNDLED_KYUTAI_MODEL_DIR` | Use this folder as the bundled Kyutai model. It must contain the files listed in `src-tauri/binaries/README.txt`. |
| `VOICEREADER_SOX_PATH` | Path to the SoX executable. Otherwise SoX is looked up in bundled `sox-macos`/`sox` folders, then on `PATH`, then in the Windows winget packages folder. Used for cloning clips and the older backend speed path. |
| `VOICEREADER_ONNXRUNTIME_PATH` | Path to the ONNX Runtime library file. Otherwise it is looked up under `binaries/onnxruntime` next to the app. There is no fallback to the system library. |
| `VOICEREADER_AUDIO8_DECODER_DEVICE` | `auto`, `gpu` or `cpu`. Overrides the Compute device setting. |
| `VOICEREADER_AUDIO8_PARALLEL_CHUNKS` | Audio8 chunks generated at once, 1 to 8. Default 2 (1 on machines with fewer than 4 cores). |
| `VOICEREADER_AUDIO8_DECODERS` | Audio8 decoder sessions, 1 to 4. Default 1. |
| `VOICEREADER_AUDIO8_DECODER_THREADS` | Threads for the CPU decoder, 1 up to the core count. Default is half the cores, between 1 and 8. |
| `VOICEREADER_ASR_THREADS` | Threads for transcription, 1 to 64. Default is half the cores, between 1 and 8. |
| `VOICEREADER_RATE_IN_BACKEND` | Set to `1` to apply the speed in the backend with SoX, as it was up to version 0.2.1, instead of in the player. Speed changes then take effect only on audio not yet generated. For comparing the two. |
| `VOICEREADER_ENGINE_ROOT` | Location of the `tts-engine` folder. Debug builds use `<root>/.data` as the data directory. |
| `VOICEREADER_AUDIO8_TEST_MODEL_DIR`, `KYUTAI_TEST_MODEL_DIR`, `KYUTAI_TEST_OUT_DIR`, `VOICEREADER_ASR_TEST_MODELS_DIR`, `VOICEREADER_ASR_TEST_AUDIO`, `VOICEREADER_ASR_TEST_MAX_SPEAKERS` | Used only by the ignored Rust tests. |
| `VOICEREADER_AUDIO8_TEST_OUTPUT_DIR` | Audio8 test WAVs and ONNX profiling output. Profiling affects timings; unset for throughput comparisons. |
| `VOICEREADER_AUDIO8_BENCHMARK_OUTPUT_DIR` | Output folder for the ignored CPU/Metal streaming benchmark. See [Metal checks](docs/METAL.md). |

The Full build also reads `VOICEREADER_ENGINE_EXECUTABLE` (release builds: path to the sidecar executable) and sets the sidecar's own variables, which are listed in `tts-engine/README.md`.

## Build a macOS portable release (Base)

On a Mac with Xcode Command Line Tools installed (`xcode-select --install`):

```sh
brew install node rust cmake
python3 -m venv tts-engine/.venv
tts-engine/.venv/bin/python -m pip install huggingface_hub
npm ci
npm run desktop:build:macos:portable
```

The first build downloads the Kyutai model, preset clips, native ONNX Runtime and
SoX source. It builds a native SoX executable with static libsox using the Xcode
Command Line Tools; subsequent builds reuse it. SoX needs no Homebrew libraries
on your Mac. Its minimum macOS version is also 14.0.
Python is used only while building; users do not need Python, Node or Rust installed.
The macOS configuration targets macOS 14 or newer. Build on Apple Silicon for an arm64
release; Intel and universal releases need separate build and runtime verification.

The output is `src-tauri/target/release/bundle/portable/VoiceReader_0.2.3_macos_arm64_portable.zip`
on Apple Silicon. Extract it and open `VoiceReader.app`; moving it to Applications is optional.
The entire `.app` must stay together. Settings, saved voices and downloaded models go
under your application data directory, so portable here means no installer,
rather than keeping user data beside the app.

Grant the current copy of VoiceReader Accessibility permission in System Settings >
Privacy & Security (called Device Control and Data Access on newer macOS) to capture
selected text from other apps, and Microphone permission to record. Settings > Read
highlighted text shows whether the running executable has access. An enabled entry
for an older unsigned build may not authorize a rebuilt copy: remove that stale entry,
add the current `.app`, then quit and reopen VoiceReader. Highlight text in the source
app and press the hotkey without switching focus to VoiceReader.
SoX is bundled for preparing voice-cloning clips (the player changes the speed itself
and no longer needs it). The Mac SoX build includes
raw PCM, WAV and tempo effects; external codecs and audio-device drivers are disabled
because VoiceReader handles decoding and playback itself. The existing Windows SoX
binaries are excluded from the Mac app. Missing SoX causes the Mac build to fail
rather than silently shipping without it.

To rebuild only the Mac SoX runtime, run `python3 scripts/bundle_sox_macos.py`.
The source download is checksum-verified and cached under `build/sox-macos`.
The bundle includes SoX's licence files, source archive, patched header and build
recipe under `Contents/Resources/binaries/sox-macos`.

The macOS release is ad hoc signed, but is not Developer ID signed or notarized by
Apple. Downloaded copies may be blocked by Gatekeeper on first launch. Follow the
first-launch instructions above. Verify launch, speech, selection capture and
recording on a Mac before uploading the ZIP as a release asset. Include the macOS
first-launch notice from [the release guide](docs/RELEASING.md) in every macOS release.

The packager validates bundled assets and architecture, signs and verifies the
complete app, then creates the ZIP. Apple Silicon builds require the WebGPU plugin
and validated FP32 decoder graphs. See [Metal support](docs/METAL.md) for runtime
checks and reproducible benchmarks. On the macOS 27 toolchain where stripping produces invalid
proc-macro dylibs, set `CARGO_PROFILE_RELEASE_STRIP=none` and
`CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_STRIP=none` before building.

## Full build

Use this only to work on the Python sidecar or the Qwen models.

- Needs everything from the Base prerequisites, plus Python 3.10 or newer, a virtual environment in `tts-engine/.venv`, and `pyinstaller` for packaging.
- Models are Kyutai plus Qwen3-TTS 0.6B CustomVoice and Base. Qwen models are downloaded from the Models page and run through PyTorch.
- The app starts the sidecar as a child process on a loopback port with a bearer token, talks to it over HTTP and a WebSocket, and stops it on exit. The protocol is in `docs/IPC_API.md`.
- Transcription and Audio8 are Base-build features and are not available in the Full build.

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
| `npm run desktop:build:full:portable` | Portable zip that includes the sidecar; builds the sidecar first. |
| `npm run sidecar:build` | Rebuild only the sidecar. |

To test the sidecar alone, see `tts-engine/README.md`.

## Project layout and docs

| Path | Contents |
|---|---|
| `src/` | Frontend: main window (`main.ts`), the Transcribe page (`transcribe.ts`), floating toolbar (`toolbar.ts`), styles, icons and shared helpers. |
| `src-tauri/src/` | Rust backend. `voicereader_core.rs` has the Tauri commands, engine lifecycle and job streaming. The Base runtimes are `kyutai_local.rs` and `audio8_local.rs` (with `audio8_model.rs` for ONNX inference), sharing `text_chunking.rs` and `audio_pipeline.rs`. Transcription is `asr_local.rs`, with `audio_decode.rs` reading audio files, `audio_capture.rs` recording the microphone, and `recording_export.rs` retaining/exporting the current session audio. `model_download.rs`, `settings.rs`, `selection.rs` and `bundled_paths.rs` hold the model downloads, saved settings, selection capture and bundled-file lookup. |
| `src-tauri/vendor/parakeet-rs/` | The `parakeet-rs` crate (0.3.8) with patches to `src/multitalker.rs`. See `VOICEREADER_PATCH.md` in that folder. |
| `src-tauri/binaries/` | Files bundled with the app. See `src-tauri/binaries/README.txt`. |
| `tts-engine/` | Python sidecar for the Full build. |
| `scripts/` | Version sync, asset fetch scripts and portable packaging. |

| Document | Contents |
|---|---|
| `docs/DESIGN_SPEC.md` | Architecture: builds, runtimes, event flow, chunking, playback, storage, transcription. |
| `docs/DECISIONS.md` | Agreed direction, model choices, order of work. |
| `docs/learnings.md` | Measurements and reasoning behind the playback pipeline, Audio8, chunking, GPU use and transcription. |
| `docs/IPC_API.md` | The sidecar's HTTP and WebSocket API (Full build only). |
| [docs/METAL.md](docs/METAL.md) | macOS GPU support, precision fix, listening result and reproducible checks. |
| [docs/RELEASING.md](docs/RELEASING.md) | Release notes, Mac signing/packaging checks and the first-launch notice. |
| `docs/benchmarks/`, `docs/images/` | Archived CPU/Metal measurements and the 2x streaming graph linked from the learnings. |

## Roadmap

Direction and order of work are in `docs/DECISIONS.md`. In short:

1. Transcription with speaker labels, from audio files and live from the microphone. Built.
2. Planned, not built: live transcription of system audio (the other side of a call), mixed with the microphone.
3. Planned, not built: word boosting for names and unusual terms in transcripts.
4. Later: the Audio8 0.6B INT4 model as an on-demand multilingual download. Not scheduled yet.

Kyutai Pocket TTS stays the default model; Audio8 stays optional.

Not planned right now: running Audio8's generation step on a GPU (it measured slower through Windows DirectML; Mac GPU generation has not been benchmarked) and other languages for Kyutai. macOS decoder support and its validation are documented in [Metal support](docs/METAL.md).

## Licence

VoiceReader is MIT-licensed. Models and third-party components keep their own licences.

- The licence of each model is listed in the [Models](#models) table.
- `src-tauri/vendor/parakeet-rs/` is a patched copy of the `parakeet-rs` crate 0.3.8, which is licensed MIT OR Apache-2.0. Its licence file and the list of changes are in that folder.
- The extra Kyutai voice clips are credited in `src-tauri/binaries/kyutai-voices/ATTRIBUTION.txt`, which the fetch script writes.
- SoX is bundled for Windows under `src-tauri/binaries/sox` with its own licence files, and for macOS under `src-tauri/binaries/sox-macos` with licence files and corresponding source/build recipe.
- Apple Silicon macOS builds bundle the WebGPU plugin's licence notices under `binaries/onnxruntime/webgpu-licenses`, and the derived Audio8 decoder graph's Apache-2.0 licence/notice under `binaries/audio8-metal`. It reuses the original downloaded weights.
