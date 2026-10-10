# VoiceReader Design Overview

This document describes how the app is put together today. Decisions and their reasons are in `docs/DECISIONS.md`. Measurements are in `docs/learnings.md`. The usage guide is in `README.md`.

## 1. Product

VoiceReader reads highlighted text aloud. A global hotkey captures the selection from the active app, a local text-to-speech model turns it into audio, and the audio plays while a small floating toolbar offers playback controls. Models run on your machine.

## 2. Builds

A Cargo feature picks the build. Exactly one must be enabled; the crate refuses to compile otherwise.

| | Base (`build-base`) | Full (`build-full`) |
|---|---|---|
| Status | The product and the default for `npm run desktop:dev` and `desktop:build` | Kept for future heavier models; not actively used |
| Inference | In the Rust process | Python sidecar (`tts-engine/`) over loopback HTTP and WebSocket |
| Models | Kyutai Pocket TTS (bundled), Audio8 TTS 0.1B (optional download) | Kyutai, Qwen3-TTS 0.6B CustomVoice and Base |
| Speech to text | Multitalker Parakeet with the Nemotron-3 diarizer (optional download; section 15) | Not available |
| Python at run time | No | Yes |

Code shared by both builds (commands, state, hotkey, selection capture, toolbar) lives in `voicereader_core.rs`. Code that exists only in one build is behind `#[cfg(feature = ...)]`. In the Base build `qwen_modes_enabled()` is false, so Qwen modes are hidden and their commands return an error. Audio8 and transcription exist only in the Base build. In the Full build `download_audio8_model`, `download_asr_model`, `transcribe_audio_file`, `transcribe_microphone_input` and `cancel_transcription` return an error, `audio8_model_status` and `asr_model_status` report `supported: false`, and `list_audio_inputs` returns an empty list.

The app version is 0.2.3. `package.json` is the source: `scripts/sync-version.js`, which runs before dev and build, copies it to `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json`, the app entries in both lockfiles, and the Full-build Python engine metadata (`tts-engine/pyproject.toml`, `tts_engine/__init__.py` and `tts_engine/config.py`). Dependency versions are left unchanged. The Base runtimes report it as `engine_version` in their health JSON (through `CARGO_PKG_VERSION`), and the model downloader sends it in its user agent. The sidecar reports the same release version.

From v0.2.3, the standard Base build includes native WebGPU/Metal decoding on Apple Silicon macOS. There is no preview feature or separate app/configuration. [METAL.md](METAL.md) describes its packaging and validation; Intel macOS stays on CPU.

## 3. Source layout

Rust backend (`src-tauri/src/`). `lib.rs` declares the modules; every module except `voicereader_core.rs`, `selection.rs` and `settings.rs` is compiled only in the Base build.

| File | Role |
|---|---|
| `main.rs` | Entry point; calls `run()` in `lib.rs`, which calls `run_app()` |
| `voicereader_core.rs` | Tauri commands, hotkey, toolbar window, engine lifecycle, job streaming, transcription jobs |
| `settings.rs` | Settings file (hotkey, compute device) and hotkey validation |
| `selection.rs` | Capturing the selected text and the source window title, per platform |
| `model_download.rs` | Resumable model download from Hugging Face |
| `kyutai_local.rs`, `audio8_local.rs` | The two Base-build TTS runtimes. `kyutai_local.rs` also stores the saved voices; `audio8_local.rs` also chooses the decoder device and loads the ONNX Runtime library (`ensure_onnxruntime`), which transcription uses too |
| `audio8_model.rs` | Audio8 ONNX inference; on Apple Silicon, native plugin registration and validated FP32 decoder graph preparation |
| `audio_pipeline.rs` | PCM emission shared by both runtimes, the switch for where the speed is applied, the older SoX tempo path, and reference-clip normalization |
| `bundled_paths.rs` | Locating files bundled next to the app |
| `text_chunking.rs` | Text normalization and chunking |
| `asr_local.rs` | Transcription: model files, the chunk loop, grouping text into speaker turns |
| `audio_decode.rs` | Reading audio files as 16 kHz mono in blocks; the resampler (also used for Audio8 reference clips) |
| `audio_capture.rs` | Capturing mono microphone samples, keeping a native-rate WAV for export and resampling to 16 kHz for transcription |
| `recording_export.rs` | Retaining the current session audio and atomically exporting recording/transcript ZIPs |

`src-tauri/vendor/parakeet-rs` is a vendored copy of the `parakeet-rs` crate (0.3.8) with two changes to `src/multitalker.rs`, marked `VoiceReader patch`: the speaker hold and the transcription of speakers beyond the limit (section 15). `src-tauri/vendor/parakeet-rs/VOICEREADER_PATCH.md` lists them and says how to move to a newer upstream version. `src-tauri/Info.plist` holds the macOS microphone permission text.

Frontend (`src/`):

| File | Role |
|---|---|
| `main.ts` | The main window: the HTML template for all pages, the handlers, and the playback queue |
| `transcribe.ts` | The Transcribe page and the transcription row on the Models page |
| `toolbar.ts` | The floating toolbar window |
| `playback.ts` | Prebuffer and rebuffer times, leading silence, PCM decoding |
| `player.ts` | The speech player: audio device, queueing, start and refill decisions, pause, stop |
| `tempo-worklet.ts`, `player-messages.ts` | The player's audio-thread half, and the messages between the two halves |
| `tempo-stretch.ts` | The time-stretcher (WSOLA) that changes speed without changing pitch |
| `shared.ts` | Toolbar event names and rate helpers used by both windows |
| `types.ts` | Payload types for the commands and events |
| `icons.ts` | The inline SVG icons and the brand mark |
| `styles.css`, `toolbar.css` | Styles of the main window and of the toolbar |

The two HTML entry points are `index.html` and `toolbar.html`.

## 4. Windows

- **Main window** (`index.html`, label `main`): opens at 1240 by 980 (minimum 980 by 840). A fixed sidebar on the left lists the pages in three groups, Speak (Read aloud, Voices), Listen (Transcribe) and App (Models, Settings); one page at a time shows on the right.
  - Read aloud: the hotkey with a Change button that captures a key combination, Model, Voice (the model's built-in voices, then saved voices), Speed (a slider from `0.25x` to `4x` in steps of `0.05`), and a text box with Speak, Read selection and Stop. Changing speed, volume or chunk size sends `set_speak_settings`; Speak and Read selection send it first.
  - Voices: a clone form (voice name, language hint, reference audio, reference text) and the voice library table. The form takes WAV files only. Transcribe audio fills the reference text from the chosen clip with `transcribe_reference_clip` (section 15); when the transcription model is missing, the form offers the download in place, shows its progress, and transcribes the clip once it finishes. The reference text is optional for Kyutai and required for Audio8, and the form's hint changes with the selected model. The table lists the model's built-in voices and the saved voices; a saved voice's name, language and description can be edited and saved, and it can be deleted. The description of a built-in voice can be edited in the table, but the change is kept only in memory until the app closes.
  - Transcribe: section 15.
  - Models: Kyutai Pocket TTS (bundled, always ready), Audio8 TTS with its download button and progress (Base build only), Transcription with its download button and progress (shown when the build supports it), Qwen3 TTS with three download buttons (Full build only), and a line with the data, models and Hugging Face cache folders.
  - Settings: Playback (volume `0` to `2`, chunk max chars `100` to `200`), Compute device (Auto, GPU or CPU; shown only where a GPU provider exists, which is Windows and Apple Silicon macOS in the Base build), Appearance (dark or light theme, dark by default), and Diagnostics (engine health JSON, Refresh health, Restart engine, and the activity log).
  - The bottom of the sidebar shows the engine status (model and device); clicking it opens Settings at Diagnostics.
- **Toolbar window** (`toolbar.html`, label `toolbar`), created at startup: 360 by 108, frameless, transparent, always on top, hidden from the taskbar, hidden until a job starts.
  - Controls: rate button (steps by `0.25x` and wraps from `4.0x` to `0.25x`), skip back (shows a short flash only; seeking is not implemented), pause or resume, stop, skip forward.
  - Pause suspends the Web Audio context. Skip forward jumps past the audio received so far.
  - The label is the last segment of the source window title split on ` - `, or "Reading aloud..." when there is none.
  - It opens at the bottom-left of the current monitor with a 20 px margin. Dragging saves the position to the toolbar window's local storage (`voicereader.toolbar.position.v1`) and it is restored on the next start.
- Closing the main window closes the toolbar and exits the app. Engine shutdown runs on exit.

## 5. From hotkey to audio

1. **Hotkey.** `register_hotkey` binds the saved hotkey, or the platform default, through Tauri's global shortcut API. If registration fails it falls back to the default (`Alt+S` on Windows, `Ctrl+Shift+S` on macOS and elsewhere), saves it and emits `voicereader:hotkey-updated`. `Alt+Space` and `Cmd+Space` are refused. The handler does nothing while the main window is focused.
2. **Source window.** The foreground window title (Windows) or frontmost application name (macOS) is read before anything else, because the simulated copy can change focus.
3. **Selection capture.** The current clipboard text is saved and replaced by a random probe string. The app waits up to 350 ms for the hotkey's modifier keys to be released, so the copy is not seen as Ctrl+Shift+C. It then sends the system copy shortcut and polls the clipboard every 25 ms for up to 500 ms for a value that differs from the probe. The previous clipboard text is restored. If nothing is captured, `voicereader:selection-empty` is emitted and the flow ends.
4. **Job start.** `speak_and_stream` cancels any previous job, creates a job ID, sets the shared rate state, emits `voicereader:job-started`, and runs the synthesis on a background task.
5. **Synthesis.** The selected runtime (section 7) chunks the text, generates audio, applies volume and calls back with 16-bit mono PCM. Base emits normal-speed audio for the player to stretch; Full and the opt-in legacy Base path apply speed before emitting it.
6. **Events to the frontend.** Each PCM piece is sent as a `voicereader:ws-event` with `type: "AUDIO_CHUNK"`. The job ends with `JOB_DONE` or `JOB_CANCELED`, or `JOB_ERROR` on failure. In the Base build these are emitted directly from Rust. In the Full build the backend relays them from the sidecar's WebSocket.
7. **Playback.** The frontend decodes each chunk and queues it (section 9). The toolbar is shown on `voicereader:job-started` and hidden when playback ends.

The Read aloud page's Read selection button calls the same flow through `trigger_read_selection`. Speak uses `speak_text`, which skips steps 1 to 3.

## 6. IPC surface

### 6.1 Commands

Registered in `run_app`:

| Group | Commands |
|---|---|
| Startup and status | `app_bootstrap`, `engine_health`, `engine_list_voices`, `engine_runtime_status`, `engine_storage_paths`, `restart_engine` |
| Models | `select_model`, `audio8_model_status`, `download_audio8_model`, `prefetch_models` (Full only) |
| Compute | `get_compute_device`, `set_compute_device` |
| Voices | `set_selected_voice`, `set_preset_speaker`, `clone_voice_from_audio`, `update_saved_voice`, `delete_saved_voice` |
| Playback settings | `set_speak_settings` (rate, volume, chunk size), `cycle_speak_rate` |
| Hotkey | `set_hotkey` |
| Jobs | `speak_text`, `trigger_read_selection`, `cancel_active_job` |
| Transcription | `asr_model_status`, `download_asr_model`, `transcribe_audio_file`, `transcribe_microphone_input`, `transcribe_reference_clip`, `list_audio_inputs`, `cancel_transcription`, `save_text_file`, `export_recording_transcript` |

`set_speak_settings` validates rate `0.25..4.0`, volume `0.0..2.0` and chunk size `100..2000`, and updates the running job's rate immediately. The Settings page limits the chunk size to `100..200`.

The transcription commands take these arguments: `transcribe_audio_file(path, max_speakers?)` and `transcribe_microphone_input(device?, max_speakers?)` return `{ job_id }`; `max_speakers` defaults to 8 and `device` is a name from `list_audio_inputs`, which returns `{ name, is_default }` entries with the default first. `asr_model_status` returns `supported`, `downloaded`, `model_dir`, `repo`, `diarizer_repo`, `download_size_bytes`, `default_max_speakers` and `max_speakers_limit`. `download_asr_model` and `download_audio8_model` return when the download has finished and report progress as events. `cancel_transcription` returns `ok: false` when nothing is running. `save_text_file(path, contents)` writes the text to the path selected in a save dialog.

`export_recording_transcript(job_id, path, contents, format)` is Base-only and writes a ZIP for the retained session matching `job_id`. Supported transcript formats are `txt`, `md` and `srt`; the current UI sends `txt`. The ZIP contains `recording.<original extension>` and `transcription.<format>`. It is written to a temporary file alongside the destination, then replaces the destination only after all writes succeed. A stale job ID or unavailable source recording returns an error.

The Tauri allowlist in `tauri.conf.json` enables only `shell` open, `clipboard` read text, `globalShortcut` (all), `window` (all), and `dialog` open and save. The Transcribe page uses the two dialogs, to choose a recording and to choose where to export a transcript or recording ZIP.

### 6.2 Events

Emitted by the backend to all windows:

| Event | Payload | When |
|---|---|---|
| `voicereader:engine-ready` | health JSON | The runtime finished initializing |
| `voicereader:job-started` | `job_id`, `ws_url`, `source` (`manual` or `hotkey_selection_capture`), `source_window`, `rate`, `rate_applied_by_player` | A job was accepted. In the Base build `ws_url` is `local://stream/<job_id>` and unused. `rate_applied_by_player` is true for the default Base path, false for the legacy Base path and Full. |
| `voicereader:ws-event` | `type` is `JOB_STARTED`, `AUDIO_CHUNK`, `JOB_DONE`, `JOB_CANCELED` or `JOB_ERROR`; always `job_id` | During a job. Base `AUDIO_CHUNK` carries `chunk_index` and `audio` (`format: pcm_s16le`, `sample_rate`, `channels: 1`, `data_base64`). Base `JOB_DONE` and `JOB_CANCELED` carry `had_audio`; Base `JOB_ERROR` carries `error` as a string. In the Full build the sidecar's events are passed on unchanged (`docs/IPC_API.md`). |
| `voicereader:job-cancel-requested` | `job_id` | `cancel_active_job` was called; the frontend stops playback at once |
| `voicereader:rate-updated` | `rate` | The rate changed from the main window or the toolbar |
| `voicereader:hotkey-updated` | `hotkey` | The hotkey was changed or fell back |
| `voicereader:selection-empty` | `reason` | The hotkey found no selected text |
| `voicereader:model-download` | `model`, `state` (`progress`, `done`, `error`), `file`, `file_index`, `file_count`, `downloaded_bytes`, `total_bytes`, `message` | During a model download (Base build). `model` is `audio8_tts_0_1b` or `parakeet_multitalker`. The main window shows the first, `transcribe.ts` the second. |
| `voicereader:transcript` | `job_id`, `kind` (`loading`, `started`, `turn`, `progress`, `level`, `done`, `cancelled`, `error`), `turn` (`id`, `speaker`, `unknown`, `start_secs`, `end_secs`, `text`; `null` except in `turn` events), `processed_secs`, `total_secs` (`null` when unknown), `level` (0 to 1, in `level` events), `message` | During a transcription (section 15) |
| `voicereader:error` | `message` | Any backend error worth showing in the Activity log |

Between the main window and the toolbar, over the same event bus: `voicereader:toolbar-show` (`job_id`, `source_window`, `rate`), `voicereader:toolbar-hide`, `voicereader:toolbar-paused` (`paused`), `voicereader:toolbar-action` (`pause-toggle`, `skip-back`, `skip-forward` or `stop`) and `voicereader:toolbar-skip-back-noop`. The main window owns the audio, so toolbar buttons only send actions to it. The Transcribe page also listens to Tauri's own `tauri://file-drop` event (payload: a list of paths).

The frontend also polls `engine_runtime_status` every 5 seconds to show the engine status in the sidebar.

## 7. Runtimes (Base build)

Both runtimes sit behind the same `stream_synthesize(...)` shape: voice, text, chunk size, volume, a cancel flag, the shared rate state, and a callback that receives PCM. Both return whether the job finished or was canceled, and whether any audio was produced. The Kyutai runtime always exists. Audio8 is loaded on first use, because its model is an optional download.

### 7.1 Kyutai Pocket TTS

- Model files are bundled (see `src-tauri/binaries/README.txt`). The runtime uses the `pocket-tts` crate with the January 2026 weights.
- 21 preset voices. Eight have precomputed embeddings in the model folder. The other 13 are cloned on first use from reference clips in `binaries/kyutai-voices` and cached under `<data dir>/preset-voices`.
- Cloning builds a voice state from a reference WAV. A transcript is stored if given but not needed.
- Generation runs a whole chunk at a time. Upcoming chunks are generated on background threads (up to the core count minus one, at most four), and the finished PCM is released in order.

### 7.2 Audio8 TTS 0.1B

- Three ONNX graphs run through ONNX Runtime, loaded at run time from a shared library (section 10): a slow autoregressive model, a fast autoregressive model, and a codec decoder. A codec encoder is used only to register a cloned voice.
- Each voice has a prompt (reference transcript plus reference codes). Its state after the prompt is cached per voice, so chunks do not repeat that work. Only the few most recently used voices stay cached.
- By default two chunks are generated at once (one on machines with fewer than four cores). One decode loop serves the chunk being played first, in small windows, and uses idle time for chunks generated ahead. The first chunks are short so playback can start early. See `docs/learnings.md` section 7.
- Generation always runs on the CPU. The decoder can run on a GPU (section 12).

### 7.3 Playback rate

The speed is applied by the player in the app window, not by the runtimes. The runtimes send audio at normal speed, and `voicereader:job-started` carries `rate_applied_by_player: true`. The player time-stretches the audio as it plays (section 9), so a change is heard within a fraction of a second, in steps as fine as the slider's `0.05x`.

The backend still keeps the rate: `set_speak_settings` and `cycle_speak_rate` store it and emit `voicereader:rate-updated`, which is how the toolbar and the slider stay in step, and Audio8 uses it to size its first piece of audio (faster playback needs a bigger reserve before starting). Volume is applied as PCM gain when the audio is generated and is fixed for the job.

The older path is kept for comparison behind `VOICEREADER_RATE_IN_BACKEND=1`: the emitter stretches each piece with SoX (`tempo`, run as a child process) in steps of `0.25x`, or resamples it (changing the pitch) when SoX is missing, and reports `rate_applied_by_player: false`. A change then affects only audio not yet generated. The sidecar of the Full build always works this way.

## 8. Chunking

`text_chunking.rs` serves both runtimes.

1. `normalize_for_speech` repairs line breaks from PDFs and web pages: wrapped lines and hyphenated words are re-joined, headings and list items become sentences of their own.
2. `chunk_text` cuts at sentence ends where it can, then at semicolons or colons, then commas or dashes, then before a connecting word, then at any space. Abbreviations, decimals, times, URLs and initials do not end a sentence. A chunk may run about a third past its budget to finish a sentence.
3. Budgets are in units: one per character, 3.5 per CJK character.

| | First chunk | Later chunks |
|---|---|---|
| Kyutai | 100 | The chunk size setting, clamped to 100 to 200 (and at most 50 tokens, halving a chunk that is over) |
| Audio8 | 80, then 120 | The chunk size setting, clamped to 100 to 160 |

Details and known gaps are in `docs/learnings.md` section 10. The Python sidecar has its own, simpler splitter (`tts-engine/src/tts_engine/chunking.py`).

## 9. Playback queue (frontend)

- **Two halves.** `player.ts` runs in the window and decides when to start, refill, pause and stop. `tempo-worklet.ts` runs on the audio thread (an AudioWorklet): it holds the audio not yet heard and produces the samples for the speakers. They exchange the messages in `player-messages.ts`, each stamped with a job number so that anything from a job that was stopped or replaced is ignored.
- **Audio device.** One Web Audio context, opened at the sample rate of the audio (24 kHz for Kyutai, 44.1 kHz for Audio8) so the browser does the resampling to the device; it is reopened when the rate changes, and opened once at startup so the first job does not wait for it. The first audio after opening gets 160 ms of silence in front, because the device wake-up can clip the start.
- **Speed.** The worklet passes the audio through `TempoStretcher` (`tempo-stretch.ts`), a WSOLA time-stretcher with speech-sized segments (40 ms segments, 15 ms search, 8 ms crossfade) in two stages, each applying the square root of the speed. Setting the speed only changes a number in the stretcher, so nothing is dropped or repeated, and at exactly 1.0x the audio passes through sample for sample. `npm run test:player` checks this.
- **Starting.** Playback holds until a reserve is waiting, measured in seconds of listening at the current speed: 0.24 s at 1.0x or below, rising to 0.85 s at 2.0x and up to 2.0 s above that. The end of the job starts whatever is waiting.
- **Running dry.** When the worklet runs out of audio before the job has ended, it stops pulling and reports it; the player waits for a refill before going on: 1 s of audio the first time, 2 s after that. Each refill is logged as `playback_rebuffer`. When the last audio of a finished job has been played, the worklet reports that and the toolbar closes.
- **Replacing and stopping.** Cancel, stop and a new job reset the player, which drops everything waiting. A new job always replaces the one playing; audio and end-of-job events that still arrive from the replaced job are ignored.

## 10. Files, storage and ONNX Runtime

Data directory: `VOICEREADER_DATA_DIR` if set. Otherwise debug builds use `tts-engine/.data` and release builds use `data` under the app's local data directory.

| Path under the data directory | Contents |
|---|---|
| `models/<org>/<repo>/` | Downloaded models. Audio8 is in `models/Edge0/audio8-TTS-0.1B-ONNX-INT8`. The transcription models are in `models/Recogment/parakeet-multitalker-int8-onnx` and `models/altunenes/parakeet-rs/nemotron-3-diarization`. |
| `voices/<voice_id>/` | One folder per saved voice: `meta.json` (name, language hint, description, transcript, creation time), `reference.wav`, and `audio8_codes.npy` once the voice has been encoded for Audio8 |
| `preset-voices/` | Normalized reference clips for the 13 clone-on-first-use Kyutai presets |
| `pocket-tts-runtime/` | Kyutai runtime config, rewritten at start to point at the model files |
| `audio8-decoder-device.json` | Cached result of the regular GPU benchmark |
| `audio8-decoder-device-webgpu-fp32.json` | Separate 7-day Auto benchmark cache for FP32 Metal |
| `hf-cache/` | Hugging Face cache folder (created at start) |

Voice ID `0` is the built-in voice: the selected Kyutai preset, or Audio8's own voice. Other IDs are UUIDs. Voices are stored and listed by the Kyutai runtime for both models. A voice cloned under Kyutai is encoded for Audio8 on first use, which works only if it has a transcript. Reference clips are normalized to 24 kHz mono 16-bit with SoX when SoX is available.

App settings are in `settings.json` in the app config directory: `hotkey` and `compute_device`. Other state (theme, toolbar position, voice numbering) lives in webview local storage.

ONNX Runtime is not linked statically, because its protobuf clashes with the one inside the Kyutai runtime's dependencies. `scripts/fetch-onnxruntime.js` downloads it into `src-tauri/binaries/onnxruntime` (version 1.24.4, or 1.23.2 on Intel macOS; on Windows the DirectML build with `DirectML.dll`). The runtime loads it by absolute path (see `bundled_paths.rs` and `VOICEREADER_ONNXRUNTIME_PATH`) and never from the system library path. Transcription uses the same library.

The Apple Silicon Base build adds WebGPU plugin 0.4.0 beside core ONNX Runtime 1.24.4. `scripts/fetch-webgpu-macos.js` verifies the archive checksum and bundles licences. Registration is lazy and native, through ONNX Runtime; no JavaScript inference, MLX or Python runtime is added. The app bundles a small FP32-compute graph under `binaries/audio8-metal`. It verifies the downloaded original graph against the validated source, then caches `codec_decoder_webgpu_fp32_v1.onnx` next to the existing FP16 external weights in the Audio8 model directory. The original graph/weights remain unchanged for CPU use. FP32 computation increases decoder memory; total peak memory for the Mac comparison has not been quantified.

## 11. Model download

Each model download is a Tauri command and runs once at a time. The transcription download runs the same routine twice, once per repo, and reports progress against the combined size. It asks Hugging Face for the size of each of the model's files, then downloads them in order into `*.part` files. An interrupted download resumes with an HTTP range request; a file whose final size already matches is skipped. Finished files are renamed into place. Progress goes out as `voicereader:model-download`, at most every 250 ms. The model counts as downloaded when every required file is present.

## 12. Compute device

The Compute Device setting (Auto, GPU, CPU) decides where Audio8's decoder runs. The two generation graphs always run on the CPU.

- Windows: the GPU provider is DirectML. Auto benchmarks the decoder on the GPU and on the CPU and keeps the GPU only if it is more than 1.5 times faster. The result is cached for 7 days in `audio8-decoder-device.json`.
- Apple Silicon macOS: Auto benchmarks the native WebGPU/Metal decoder against CPU with the same threshold, using `audio8-decoder-device-webgpu-fp32.json`. GPU requests WebGPU directly. FP32 computation passed waveform checks on the M2 Max, and I accepted its listening quality and stability for the main app in v0.2.3. The initial FP16 implementation corrupted speech; the failed Core ML route is no longer offered.
- Intel macOS: CPU only; no Metal plugin is bundled.
- Other platforms: CPU only.
- GPU load/warm-up, graph validation and graph-cache write failures fall back to CPU. A GPU failure during a running job is reported as a job error; there is no automatic mid-job CPU retry.
- Changing the setting while Audio8 is loaded stops any speech and reloads the model. `VOICEREADER_AUDIO8_DECODER_DEVICE` overrides the setting.

Measurements and the reasons for these choices are in [learnings](learnings.md) sections 11 and 15. On the M2 Max, the 2x-setting backend benchmark generated 51.08 seconds of speech in 29.00 seconds on CPU and 19.82 seconds with CPU generation plus Metal decoding: about 1.46x overall, despite roughly 4–5x warm decoder speedup. Average Metal supply exceeded 2x consumption, but the traced buffer simulation still had an early refill. These are backend/simulated buffer measurements, not measured speaker startup or uninterrupted 2x playback in the app.

## 13. Full build differences

- The app starts the sidecar as a child process on a free loopback port with a random bearer token and waits for `GET /v1/health`.
- `speak_text` and the hotkey flow call `POST /v1/speak`, then relay the WebSocket stream as `voicereader:ws-event`. Cancel calls `POST /v1/cancel`. Live rate changes call `POST /v1/jobs/{job_id}/playback`.
- Model switching calls the sidecar's model activation endpoint.
- The sidecar's rate handling tries SoX, then librosa, then linear resampling. It applies changes per chunk.
- The API is documented in `docs/IPC_API.md`.

## 14. Platform differences

- Windows: copy is sent with `SendInput`; modifier state comes from `GetAsyncKeyState`; the source label is the foreground window title; the Audio8 decoder can use DirectML.
- macOS: copy is sent with CGEvent; modifier state comes from the CGEvent source state; the source label is the frontmost application's name. Selection capture requires Accessibility access. The highlighted-text hotkey and floating toolbar have been tested on Apple Silicon. `src-tauri/Info.plist` carries the microphone permission text (`NSMicrophoneUsageDescription`) for live transcription.
- Other platforms: no copy simulation and no source label, so the hotkey flow reports an empty selection.

## 15. Transcription

English speech to text with speaker labels, Base build only. It works on a recording (a file) or live from a microphone. System audio (what the computer is playing) is not captured and word boosting does not exist. Transcripts and the current session's audio can be exported; they are not automatically saved as a session history. Measurements are in `docs/learnings.md` section 12.

- **Models.** The multitalker Parakeet model (int8 encoder and decoder, about 666 MB) and the Nemotron-3 diarizer (about 400 MB), both run by the `parakeet-rs` crate on the shared ONNX Runtime library (section 10). They run on the CPU on purpose: Windows DirectML measured no faster with the int8 speech encoder. Mac GPU transcription has not been benchmarked. The model uses half the hardware threads, between 1 and 8 (`VOICEREADER_ASR_THREADS` overrides it, from 1 to 64), and about 1.3 GB of memory while transcribing. The crate is vendored in `src-tauri/vendor/parakeet-rs` with two patches (speaker hold and unknown speaker, below). The diarizer says who is speaking in each chunk; the speech encoder then runs once per active speaker with that speaker's activity as an extra input, which is what lets it separate people talking at the same time.
- **Speaker hold.** The model emits a word slightly after it was spoken, and the diarizer's activity is what switches each speaker's encoder input on and off. Used as is, the input is switched off the moment a speaker stops and the words still on their way out are lost. The patch keeps a speaker switched on for 0.8 s after their activity last reached 0.5, and keeps them visible to the other speakers as background for 0.6 s (`SPEAKER_HOLD_SECS`, `BACKGROUND_HOLD_SECS` in `asr_local.rs`). The hold carries over from one chunk to the next. Measurements are in `docs/learnings.md` section 12.6.
- **Speaker limit and the unknown speaker.** The diarizer has eight speaker slots and gives each new voice the next free one. The Speakers setting on the Transcribe page (1 to 8, default 8; `max_speakers`) says how many slots get a label of their own. The slots beyond it are transcribed together by one extra model instance, which runs only on a chunk with at least 0.3 s of speech for it to hear. Their turns carry the speaker number 8 with `unknown: true`, and the page shows them as "Unknown speaker". With the default of 8 there are no slots beyond the limit, so this appears only when Speakers is set lower. When such speech adds up to 2 seconds or more, the final message says how much it was and suggests raising Speakers. Upstream skips those slots; this is the second change in the vendored crate.
- **Lifecycle.** `transcribe_audio_file` and `transcribe_microphone_input` start one job at a time on a blocking task and return its `job_id`; starting another while one runs is refused. The job sends `loading` at once, loads the model (about 3 s), then sends `started`. The model is dropped when the job ends, so its memory is held only while transcribing. `cancel_transcription` sets a flag. For a file it is checked between chunks and abandons the rest of the file (`cancelled`). For the microphone it ends the recording: the audio already captured is transcribed, and the job finishes normally (`done`). A guard releases the job and sends `error` if the task ends without a result. The `done` message gives the audio length, the number of labelled speakers and, for a file, the time it took; the unknown speaker is not counted as a speaker.
- **Audio input.** `audio_decode.rs` opens the file before the model loads, so an unreadable file fails at once. It decodes with `symphonia` packet by packet (a damaged packet is skipped), mixes to mono and resamples to 16 kHz in a streaming windowed-sinc resampler, so a long recording is never held in memory as a whole. The decoder is built for WAV, MP3, AAC in M4A or MP4, ALAC, FLAC, OGG Vorbis and MKV; Opus cannot be decoded. The file dialog offers `wav`, `mp3`, `m4a`, `mp4`, `aac`, `flac`, `ogg`, `oga`, `mkv` and `mov`, and a file the decoder cannot read fails with a message naming the supported formats. The total length is known only when the file states its frame count.
- **Microphone input.** `audio_capture.rs` opens the chosen input device (or the system default) with `cpal` at the device's own format (`f32`, `i16`, `u16` or `i32` samples), mixes it to mono in the audio callback and sends it over a channel. The transcription thread resamples it to 16 kHz with the same resampler as files. The capture stream is created and dropped on that thread, after the model has loaded, so nothing is recorded while the model is still loading. When recording for export, the mono samples are also written to a temporary 16-bit WAV at the microphone’s native sample rate, before resampling for the model. A `level` event carries the loudest sample (0 to 1) of each piece of captured audio for the level meter, and the first one tells the frontend that audio is arriving. If a device delivers nothing for 10 seconds (`SILENT_DEVICE_TIMEOUT`; a Bluetooth headset can need over a second to start) the job fails with an explanation. A thread named `audio-host` touches the audio system first and never exits: on Windows the audio library's device enumerator belongs to the first thread that uses it, and the app's background threads end when idle, which crashed the next use from another thread.
- **Reference clips.** `transcribe_reference_clip(wav_base64)` returns `{ text }` for the voice-cloning form. It decodes the WAV, adds 1.5 s of silence so the last words come out, and runs the speech model with every frame marked as one speaker: no diarizer gating, no turns, no events. It waits for the result instead of starting a job, but it takes the same single job slot, so it is refused while a transcription is running and the other way round.
- **Chunk loop.** Audio goes to the model in 1.12-second chunks (the model's normal latency mode). Each chunk returns, per speaker, the new text and word times, and a `progress` event follows. After the file ends, the last partial chunk is padded with silence and three chunks of silence are fed so the model emits the words it was holding back.
- **Turns.** `TurnBuilder` groups the text into turns. Text that starts with a space begins a new word and continues the speaker's open turn unless they paused for more than 1.5 s, another speaker started after they stopped (so a reply reads as a reply), or the turn already has 360 characters and ends with `.`, `?` or `!`. Text that does not start with a space (the rest of a word, or punctuation) joins the speaker's open turn unless that turn ended more than 5 s earlier, and a new turn never opens with leftover punctuation. Speech that overlaps another speaker does not split the turn.
- **Events.** Every change goes out as `voicereader:transcript`: `turn` events carry the whole current text of one turn, identified by `id`, so the frontend replaces the row rather than appending. `started` carries `total_secs` for a file; progress carries `processed_secs` and `total_secs`.
- **Frontend.** `transcribe.ts` builds the page. Until the models are downloaded it shows a setup card with a download button and progress bar; afterwards it shows the controls: Record, Choose recording, a Stop button (labelled "Stop recording" for the microphone) while a job runs, a record light with a level meter, the Microphone list (system default first; refreshed at start, when a job ends and when the window gains focus), the Speakers list, a progress bar for files and a status line. A file dropped on the window (`tauri://file-drop`) starts a transcription when the Transcribe page is open. Starting a job clears the previous transcript. Turns are kept in a map and shown ordered by start time, one row per turn with the time, a speaker label and the text. Speakers have eight label colours (the number is taken modulo 8); the unknown speaker has its own style. A field per speaker lets you name them, and the names replace the labels in the rows, in Copy and in Export. Copy puts plain text (`[m:ss] Name: text`) on the clipboard. **Transcription** opens a save dialog (default name: the recording name without its extension, plus `.txt`; a microphone recording is named `recording-<date>-<time>`) and picks the format from the chosen extension: `.md` gives Markdown, `.srt` gives subtitles (each cue lasts at least 0.5 s), anything else gives plain text. The file is written through `save_text_file`.

- **Recording export and retention.** After file transcription finishes or is cancelled, the current session keeps the source file path; the original file must remain available for export. After microphone recording stops and transcription finishes, the temporary WAV is kept until the next file/microphone transcription starts or the app exits. `recording_export.rs` owns that temporary directory and removes it when released. The transcript stays in the frontend for the session. **Recording & transcription** saves the audio and a text transcript as a ZIP; text-only **Transcription** offers TXT, Markdown and SRT. The combined export is disabled while a job runs, and the UI binds it to the completed job ID. There is no automatic archive or recovery of unsaved sessions.

## 16. Known gaps

- Skip back has no seek behind it.
- Text is read as written; URLs, markdown symbols, citation markers and emoji are not cleaned up.
- A chunk cut in the middle of a sentence is generated on its own, so intonation can dip at the cut.
- Core ML decoding for Audio8 failed with the shipped export and was replaced by native WebGPU/Metal FP32 decoding in v0.2.3; broader chip/OS validation, actual 2x playback measurements and investigation of the plugin teardown diagnostic remain (learnings section 15.8).
- The clone form takes WAV files only. The description of a built-in voice, edited on the Voices page, is lost when the app closes.
- Live transcription captures the microphone only. System audio is not captured. Audio can be kept through the optional ZIP export; unsaved microphone audio is temporary. End-to-end recording/export on macOS still needs verification, and live accuracy has not been measured since the speaker hold was added.
- Transcripts are not saved between runs (Copy or export is needed to keep one), and there is no word boosting.
- Transcription is English only and runs on the CPU. More than eight speakers cannot be told apart. The first word or two of a new speaker is sometimes missing, and when one speaker takes over with no gap the previous speaker can be given the newcomer's first words (`docs/learnings.md` sections 12.6 and 12.8).
- Opus audio cannot be decoded.
