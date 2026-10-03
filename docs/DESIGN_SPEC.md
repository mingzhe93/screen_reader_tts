# VoiceReader Design Overview

This document describes how the app is put together today. Decisions and their reasons are in `docs/DECISIONS.md`. Measurements are in `docs/learnings.md`. The user-facing summary is in `README.md`.

## 1. Product

VoiceReader reads highlighted text aloud. A global hotkey captures the selection from the active app, a local text-to-speech model turns it into audio, and the audio plays while a small floating toolbar offers playback controls. Models run on the user's machine.

## 2. Builds

A Cargo feature picks the build. Exactly one must be enabled; the crate refuses to compile otherwise.

| | Base (`build-base`) | Full (`build-full`) |
|---|---|---|
| Status | The product and the default for `npm run desktop:dev` and `desktop:build` | Kept for future heavier models; not actively used |
| Inference | In the Rust process | Python sidecar (`tts-engine/`) over loopback HTTP and WebSocket |
| Models | Kyutai Pocket TTS (bundled), Audio8 TTS 0.1B (optional download) | Kyutai, Qwen3-TTS 0.6B CustomVoice and Base |
| Python at run time | No | Yes |

Code shared by both builds (commands, state, hotkey, selection capture, toolbar) lives in `voicereader_core.rs`. Code that exists only in one build is behind `#[cfg(feature = ...)]`. In the Base build `qwen_modes_enabled()` is false, so Qwen modes are hidden and their commands return an error.

## 3. Source layout

Rust backend (`src-tauri/src/`):

| File | Role |
|---|---|
| `voicereader_core.rs` | Tauri commands, engine lifecycle, job streaming |
| `settings.rs` | Settings file (hotkey, compute device) |
| `selection.rs` | Capturing the selected text and the source window title, per platform |
| `model_download.rs` | Resumable model download from Hugging Face |
| `kyutai_local.rs`, `audio8_local.rs` | The two Base-build runtimes |
| `audio8_model.rs` | Audio8 ONNX inference |
| `audio_pipeline.rs` | SoX tempo stream and rate-controlled PCM emission, shared by both runtimes |
| `bundled_paths.rs` | Locating files bundled next to the app |
| `text_chunking.rs` | Text normalization and chunking |

Frontend (`src/`): `main.ts` is the main window and owns the playback queue. `toolbar.ts` is the floating toolbar window. The two HTML entry points are `index.html` and `toolbar.html`.

## 4. Windows

- **Main window** (`index.html`), with three tabs: Reader (hotkey, model, voice, rate, volume, chunk size, Speak Text), Voices & Clone (clone form and voice library), Engine (model downloads, Compute Device, health, activity log).
- **Toolbar window** (`toolbar.html`, label `toolbar`), created at startup: 360 by 108, frameless, transparent, always on top, hidden from the taskbar, hidden until a job starts.
  - Controls: rate button (steps by `0.25x` and wraps from `4.0x` to `0.25x`), skip back (shows a short flash only; seeking is not implemented), pause or resume, stop, skip forward.
  - Pause suspends the Web Audio context. Skip forward stops the audio already scheduled and plays what is queued next.
  - The label is the last segment of the source window title split on ` - `, or "Reading aloud..." when there is none.
  - It opens at the bottom-left of the current monitor with a 20 px margin. Dragging saves the position to the toolbar window's local storage (`voicereader.toolbar.position.v1`) and it is restored on the next start.
- Closing the main window closes the toolbar and exits the app. Engine shutdown runs on exit.

## 5. From hotkey to audio

1. **Hotkey.** `register_hotkey` binds the saved hotkey, or the platform default, through Tauri's global shortcut API. If registration fails it falls back to the default (`Alt+S` on Windows, `Ctrl+Shift+S` on macOS and elsewhere), saves it and emits `voicereader:hotkey-updated`. `Alt+Space` and `Cmd+Space` are refused. The handler does nothing while the main window is focused.
2. **Source window.** The foreground window title (Windows) or frontmost application name (macOS) is read before anything else, because the simulated copy can change focus.
3. **Selection capture.** The current clipboard text is saved and replaced by a random probe string. The app waits up to 350 ms for the hotkey's modifier keys to be released, so the copy is not seen as Ctrl+Shift+C. It then sends the system copy shortcut and polls the clipboard every 25 ms for up to 500 ms for a value that differs from the probe. The previous clipboard text is restored. If nothing is captured, `voicereader:selection-empty` is emitted and the flow ends.
4. **Job start.** `speak_and_stream` cancels any previous job, creates a job ID, sets the shared rate state, emits `voicereader:job-started`, and runs the synthesis on a background task.
5. **Synthesis.** The selected runtime (section 7) chunks the text, generates audio, applies the playback rate, and calls back with 16-bit mono PCM.
6. **Events to the frontend.** Each PCM piece is sent as a `voicereader:ws-event` with `type: "AUDIO_CHUNK"`. The job ends with `JOB_DONE` or `JOB_CANCELED`, or `JOB_ERROR` on failure. In the Base build these are emitted directly from Rust. In the Full build the backend relays them from the sidecar's WebSocket.
7. **Playback.** The frontend decodes each chunk and queues it (section 9). The toolbar is shown on `voicereader:job-started` and hidden when playback ends.

The Reader tab's Read Selection Now button calls the same flow through `trigger_read_selection`. Speak Text uses `speak_text`, which skips steps 1 to 3.

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

`set_speak_settings` validates rate `0.25..4.0`, volume `0.0..2.0` and chunk size `100..2000`, and updates the running job's rate immediately.

### 6.2 Events

Emitted by the backend to all windows:

| Event | Payload | When |
|---|---|---|
| `voicereader:engine-ready` | health JSON | The runtime finished initializing |
| `voicereader:job-started` | `job_id`, `ws_url`, `source` (`manual` or `hotkey_selection_capture`), `source_window`, `rate` | A job was accepted. In the Base build `ws_url` is `local://stream/<job_id>` and unused. |
| `voicereader:ws-event` | `type` is `JOB_STARTED`, `AUDIO_CHUNK`, `JOB_DONE`, `JOB_CANCELED` or `JOB_ERROR` | During a job. Base `AUDIO_CHUNK` carries `chunk_index` and `audio` (`format: pcm_s16le`, `sample_rate`, `channels: 1`, `data_base64`). Terminal events carry `had_audio` (Base) or the sidecar's fields (Full). |
| `voicereader:job-cancel-requested` | `job_id` | `cancel_active_job` was called; the frontend stops playback at once |
| `voicereader:rate-updated` | `rate` | The rate changed from the main window or the toolbar |
| `voicereader:hotkey-updated` | `hotkey` | The hotkey was changed or fell back |
| `voicereader:selection-empty` | `reason` | The hotkey found no selected text |
| `voicereader:model-download` | `model`, `state` (`progress`, `done`, `error`), `file`, `file_index`, `file_count`, `downloaded_bytes`, `total_bytes`, `message` | During the Audio8 download |
| `voicereader:error` | `message` | Any backend error worth showing in the Activity log |

Between the main window and the toolbar, over the same event bus: `voicereader:toolbar-show` (`job_id`, `source_window`, `rate`), `voicereader:toolbar-hide`, `voicereader:toolbar-paused` (`paused`), `voicereader:toolbar-action` (`pause-toggle`, `skip-back`, `skip-forward` or `stop`) and `voicereader:toolbar-skip-back-noop`. The main window owns the audio, so toolbar buttons only send actions to it.

The frontend also polls `engine_runtime_status` every 5 seconds to show the engine pill.

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
- Generation always runs on the CPU. The decoder can run on a GPU (section 11).

### 7.3 Playback rate

The frontend sends the rate to the backend as a number; the backend keeps it in a shared atomic counter in steps of `0.25x`. A running job reads it between pieces of audio. When it has changed, the emitter flushes its SoX stream and opens a new one for the new rate. SoX (`tempo`, run as a child process) keeps the pitch. If SoX cannot be found, the audio is resampled instead, which changes the pitch. Volume is applied as PCM gain when the audio is generated and is fixed for the job.

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

- Audio plays through one Web Audio context. Chunks are decoded to floating point and scheduled back to back after a cursor. The first chunk of the session gets 160 ms of silence in front, because the first device wake-up can clip the start.
- Chunks queue per job. Playback holds until a prebuffer is filled: 0.24 s at 1.0x or below, rising to 0.85 s at 2.0x and up to 2.0 s above that. The terminal event forces whatever is queued to play.
- If everything scheduled has already finished when a new chunk arrives, playback pauses and refills before resuming: 1 s of audio the first time a job runs dry, 2 s after that. Each refill is logged as `playback_rebuffer`.
- Cancel, stop and a new job clear the queue. Audio from a canceled job that is still arriving is dropped.

## 10. Files, storage and ONNX Runtime

Data directory: `VOICEREADER_DATA_DIR` if set. Otherwise debug builds use `tts-engine/.data` and release builds use `data` under the app's local data directory.

| Path under the data directory | Contents |
|---|---|
| `models/<org>/<repo>/` | Downloaded models. Audio8 is in `models/Edge0/audio8-TTS-0.1B-ONNX-INT8`. |
| `voices/<voice_id>/` | One folder per saved voice: `meta.json` (name, language hint, description, transcript, creation time), `reference.wav`, and `audio8_codes.npy` once the voice has been encoded for Audio8 |
| `preset-voices/` | Normalized reference clips for the 13 clone-on-first-use Kyutai presets |
| `pocket-tts-runtime/` | Kyutai runtime config, rewritten at start to point at the model files |
| `audio8-decoder-device.json` | Cached result of the GPU benchmark |
| `hf-cache/` | Hugging Face cache folder (created at start) |

Voice ID `0` is the built-in voice: the selected Kyutai preset, or Audio8's own voice. Other IDs are UUIDs. Voices are stored and listed by the Kyutai runtime for both models. A voice cloned under Kyutai is encoded for Audio8 on first use, which works only if it has a transcript. Reference clips are normalized to 24 kHz mono 16-bit with SoX when SoX is available.

App settings are in `settings.json` in the app config directory: `hotkey` and `compute_device`. Other state (theme, toolbar position, voice numbering) lives in webview local storage.

ONNX Runtime is not linked statically, because its protobuf clashes with the one inside the Kyutai runtime's dependencies. `scripts/fetch-onnxruntime.js` downloads it into `src-tauri/binaries/onnxruntime`. The runtime loads it by absolute path (see `bundled_paths.rs` and `VOICEREADER_ONNXRUNTIME_PATH`) and never from the system library path. The same library is meant to serve the planned ASR work.

## 11. Model download

The Audio8 download is a Tauri command and runs once at a time. It asks Hugging Face for the size of each of the model's files, then downloads them in order into `*.part` files. An interrupted download resumes with an HTTP range request; a file whose final size already matches is skipped. Finished files are renamed into place. Progress goes out as `voicereader:model-download`, at most every 250 ms. The model counts as downloaded when every required file is present.

## 12. Compute device

The Compute Device setting (Auto, GPU, CPU) decides where Audio8's decoder runs. The two generation graphs always run on the CPU.

- Windows: the GPU provider is DirectML. Auto benchmarks the decoder on the GPU and on the CPU once and keeps the GPU only if it is at least 1.5 times faster. The result is cached for 7 days in `audio8-decoder-device.json`. If a GPU session fails to load, the model loads on the CPU.
- macOS: the provider is Core ML. It is wired in but untested, so Auto picks the CPU and only GPU selects Core ML.
- Other platforms: CPU only.
- Changing the setting while Audio8 is loaded stops any speech and reloads the model. `VOICEREADER_AUDIO8_DECODER_DEVICE` overrides the setting.

Measurements and the reasons for these choices are in `docs/learnings.md` section 11.

## 13. Full build differences

- The app starts the sidecar as a child process on a free loopback port with a random bearer token and waits for `GET /v1/health`.
- `speak_text` and the hotkey flow call `POST /v1/speak`, then relay the WebSocket stream as `voicereader:ws-event`. Cancel calls `POST /v1/cancel`. Live rate changes call `POST /v1/jobs/{job_id}/playback`.
- Model switching calls the sidecar's model activation endpoint.
- The sidecar's rate handling tries SoX, then librosa, then linear resampling. It applies changes per chunk.
- The API is documented in `docs/IPC_API.md`.

## 14. Platform differences

- Windows: copy is sent with `SendInput`; modifier state comes from `GetAsyncKeyState`; the source label is the foreground window title; the Audio8 decoder can use DirectML.
- macOS: copy is sent with CGEvent; modifier state comes from the CGEvent source state; the source label is the frontmost application's name.
- Other platforms: no copy simulation and no source label, so the hotkey flow reports an empty selection.

## 15. Known gaps

- Skip back has no seek behind it.
- Text is read as written; URLs, markdown symbols, citation markers and emoji are not cleaned up.
- A chunk cut in the middle of a sentence is generated on its own, so intonation can dip at the cut.
- Core ML decoding for Audio8 has not been tried on a Mac.
