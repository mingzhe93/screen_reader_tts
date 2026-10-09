# VoiceReader Decisions

Direction agreed on 2026-10-03, after a seven-month gap in development, and the decisions made through version 0.2.2 (2026-10-09). Newest decisions go at the top of each section.

## Release 0.2.2 (2026-10-09)

- **The player applies the speed, not the backend** (2026-10-09, the user's wish). Speed changes used to reach only audio not yet generated, and Kyutai generates most of a text within seconds. The backend now sends audio at normal speed and the player time-stretches it while it plays, so a change is heard at once and the slider's `0.05x` steps are honoured (the backend rounded to `0.25x`). The stretcher is written in the app (WSOLA, two stages) rather than taken from a library. `VOICEREADER_RATE_IN_BACKEND=1` brings back the SoX path for comparison; SoX stays bundled for voice-cloning clips. Measurements are in `docs/learnings.md` section 13.
- **Chunks generated in parallel each get their own copy of the voice state** (2026-10-09). Sharing it garbled 11 of the 21 Kyutai presets, and cloned voices at random; the cause and the measurements are in `docs/learnings.md` section 14. The user had asked for those 11 voices to be removed because they read badly. They now read as well as the rest, so which presets to drop is a question of taste and is still open.
- **A new read-aloud job replaces the one that is playing** (2026-10-09). Pressing the hotkey during playback stops the audio and empties the queue before the new text starts. Late audio and end-of-job events from the replaced job are ignored.
- **One manifest for both platforms** (2026-10-09). `macOSPrivateApi` (needed for the transparent toolbar window on macOS) is set in the shared `tauri.conf.json`, not in the macOS-only config. The Tauri CLI rewrites the `tauri` features in `Cargo.toml` to match the config, and the build fails when they disagree, so a macOS-only setting made `Cargo.toml` differ between a Mac and a Windows machine and broke plain `cargo` builds on Windows. The setting has no effect on Windows.
- **The clone form says that transcription is English only.** The note is on the Transcribe audio button itself, not only in its tooltip.
- **Next: skip back and skip forward.** The player keeps the audio at normal speed, which makes moving back and forth in it possible.

## Release 0.2.1 (2026-10-05)

- **macOS portable support is working on Apple Silicon.** Highlighted-text hotkeys and the floating playback toolbar have been tested. SoX is bundled for pitch-preserving speed changes, and both platforms use the updated app icon.
- **Audio8 GPU acceleration on macOS is deferred.** Core ML inference failed with the shipped decoder, so macOS uses the CPU. GPU acceleration remains available on Windows through DirectML.
- **Recording export is optional.** The Transcribe page offers Copy, Transcription, and Recording & transcription. The combined export saves a ZIP containing the original uploaded audio or the microphone's mono WAV recording, plus a text transcript.
- **Release versions stay aligned.** The desktop, lockfile app entries and Python engine metadata use the release version and are synchronized from `package.json`.

## Product direction

- **CPU first.** The app must run well on CPU; GPU is optional. No PyTorch in the default build, because it blows up the app size.
- **Base build is the product.** `npm run desktop:dev`, `desktop:build`, `desktop:build:portable` and `desktop:build:standalone` all point at the Rust-only base build. The full (sidecar) build is reached through the explicit `:full` scripts.
- **Python sidecar stays.** It is kept for heavier models in the future, but is not actively used and is not part of the default build.
- **Qwen stays in the code, hidden from the base build.** It remains available in the full build for future exploration. The base build already hides it (`qwen_modes_enabled()`), so no removal work is planned.
- **Single user, on device.** No scaling concerns; keep it hardware friendly.

## TTS

- **Kyutai Pocket TTS stays the bundled default.** Other models are downloaded on demand. Reconfirmed on 2026-10-03 after testing Audio8: Kyutai is much faster for English.
- **Kyutai stays on the January 2026 weights for now.** Upstream has newer multilingual versions, but they are gated and the Rust runtime does not support them; other languages are not a goal for Kyutai at this point. The preset list was extended from 8 to 21 English voices using upstream's public reference clips.
- **Audio8 thread layout stays at the measured default.** More threads or parallel sessions did not raise throughput on the 9950X (see `docs/learnings.md` section 7.6). Audio8 is usable up to about 1.5x playback; above that it breaks up.
- **Audio8 TTS is the second model**, added as a Rust-native ONNX Runtime backend in the base build:
  - `Edge0/audio8-TTS-0.1B-ONNX-INT8` first (Apache-2.0, about 430 MB for synthesis, primarily English and Chinese).
  - `Edge0/Audio8-TTS-Preview-0.6B-ONNX-INT4` later, as the optional multilingual download. Not built yet.
  - Not `Edge0/Audio8-TTS-Preview-0.1b`: that repo is the PyTorch build under a revenue-capped licence.
- **Audio8 does not become the default** (2026-10-04). It was to be decided after testing; Kyutai is much faster for English and Audio8's built-in voice was judged poor but acceptable, so Audio8 stays the optional model.
- **The clone form can transcribe the reference clip** (2026-10-04, the user's request). Audio8 needs the exact transcript, and typing it is the tedious part of cloning. A Transcribe audio button fills it in with the transcription model, for both Kyutai and Audio8, and offers the model download when it is missing. The reference text moved below the reference audio so the order matches. The transcriber is English only, so the text still needs checking for other languages.
- **Voice cloning must keep working** for every TTS model. Audio8 needs the reference audio plus its exact transcript, so the clone flow gains a transcript field.
- **Audio8 0.1B support is implemented (2026-10-03) and tested by the user.** Measured on a Ryzen 9 9950X: about 0.8 s to first audio at 1.0x, about 1.4 s at 1.5x, roughly 2x real time at best, about 1.5 GB of memory while speaking. Details are in `docs/learnings.md` section 7.
- **ONNX Runtime is loaded as a shared library, not linked statically.** Static linking clashes with the protobuf copy inside the Kyutai runtime's dependencies. The library is fetched at build time by `scripts/fetch-onnxruntime.js` (ONNX Runtime 1.24.4 at the time of writing, 1.23.2 for Intel macOS) and is not committed. Transcription uses this same library.
- **GPU acceleration goes through ONNX Runtime's native providers, not WebGPU in the app window** (2026-10-04; macOS status updated 2026-10-05). DirectML on Windows is implemented for the Audio8 decoder and chosen by benchmark, with CPU fallback. Core ML on macOS failed inference with the shipped decoder, so macOS Audio8 GPU support is deferred and the app uses the CPU. Only graphs that measure faster on the GPU are moved; generation stays on the CPU. Details are in `docs/learnings.md` section 11.
- **The user can choose the compute device** (2026-10-04): Auto (default), GPU or CPU on the Settings page (the Engine tab at the time). Auto means "GPU when available and faster than the CPU", not "GPU whenever present", because a weak integrated GPU measured four times slower than the CPU. CPU exists so the GPU can be left free for other work.
- **Chatterbox was considered and set aside** (2026-10-04). It promises better quality and cloning without a transcript, but is realistically a GPU model; Audio8 stays for now.
- **The Qwen ONNX port plan is dropped.** `docs/onnxruntime_plan.md` planned a Rust ONNX Runtime port of Qwen, which is no longer the goal. Its approach (ONNX Runtime via the `ort` crate, tokenizer, cached decode loop) was reused for Audio8, and the file was deleted on 2026-10-04.

## ASR

- **Built in stages, files first** (2026-10-04). Stage 1 transcribes an audio file with speaker labels. Live microphone transcription was added the same day at the user's request, to make testing easier. System audio is the next stage. Each stage is tested by the user before the next starts.
- **Audio capture is done in Rust with `cpal`, not in the app window** (2026-10-04). It avoids the browser permission prompt and sending audio across the window boundary, and the same crate can capture system audio on Windows (WASAPI loopback) for the next stage. Pinned to 0.15, the API it was written against; newer releases changed the API and were not tried.
- **Transcription runs on the CPU** (2026-10-04). On a Ryzen 9 9950X it runs at about 5 to 6.6 times real time, and DirectML on an RTX 5090 was no faster (4.7 times) because the speech encoder is int8. The Compute device setting does not apply to it. This replaces the earlier expectation that ASR would need a GPU; weaker CPUs are untested.
- **The diarizer is the `parakeet-rs` export, not the `onnx-community` one** (2026-10-04). The crate loads its own `nemotron3_diar_v3.onnx` (400 MB) from `altunenes/parakeet-rs`. The smaller quantized `onnx-community` files (83 to 120 MB) are a different export and have not been tried with the crate. Total download is about 1.07 GB.
- **`parakeet-rs` is vendored and patched** (2026-10-04). The crate dropped the last words before every pause and speaker change, and the fix has to be made inside its chunk loop, which it does not expose. `src-tauri/vendor/parakeet-rs` is version 0.3.8 with that change and the unknown-speaker change below, both described in `VOICEREADER_PATCH.md` there. Word boosting will also need a change inside the crate, so the copy is expected to stay. The patch is worth offering upstream.
- **The model is loaded per transcription and dropped afterwards**, so its 1.3 GB of memory is not held while the app is only reading aloud.
- **A separate Transcribe page** for speech to text, including meetings with speaker labels ("speaker 1 said this, speaker 2 said that").
- **Model choice: multitalker Parakeet plus the Nemotron-3 diarizer**, because crosstalk is common in meetings and this pairing is built for overlapping speech.
  - `Recogment/parakeet-multitalker-int8-onnx` (about 666 MB) and the Nemotron-3 diarizer (see above for which export).
  - Runtime: the `parakeet-rs` crate, which uses `ort`, so it shares the inference layer with Audio8.
  - It uses the ONNX Runtime library added for Audio8. The same rule was applied: benchmark each graph, and move it to the GPU only if it is faster there. It was not (see above).
  - **English only for now.** The page says so.
  - The speaker limit defaults to 8, the diarizer's maximum (2026-10-04). It was 4 at first, to match the speech model's card, but the library dropped every speaker beyond the limit, and a ten-speaker news report lost almost half its words that way.
  - **Speakers beyond the limit are transcribed as "Unknown speaker"** (2026-10-04, the user's request), instead of being left out. They share one label. This cannot help with more than eight voices: the diarizer has eight slots and files further voices under the existing ones, so that speech is not missing, only mislabelled.
- **Word boosting is a follow-up.** `parakeet-rs` shows no sign of supporting it. It can be added at decode time (a prefix tree of tokenized custom words that adds a bonus to matching tokens), which means a fork or an upstream contribution.
- **Not chosen:** `Edge0/Audio8-ASR-0.1B-onnx-runtime` (CC BY-NC, no streaming, truncates at 30 seconds). Plain Nemotron 3.5 ASR plus diarization remains the fallback if multilingual transcription matters more than crosstalk handling.

## Order of work

1. Flip the build defaults to base. Done 2026-10-03.
2. Add Audio8 0.1B INT8 support, with cloning. Done 2026-10-03.
3. Test it; decide whether it becomes the default. Done 2026-10-04: Kyutai stays the default, Audio8 stays as the optional model.
4. Housekeeping before ASR. Done 2026-10-04.
5. Redesign the main window, add file transcription and live microphone transcription. Done 2026-10-04 and tried by the user on recordings and the microphone.
6. Release version 0.2.0 on GitHub. Published, followed by 0.2.1 (2026-10-05) and 0.2.2 (2026-10-09).
7. Live transcription of system audio, mixed with the microphone. Next.
8. Word boosting.

The on-demand Audio8 0.6B INT4 model was originally step 4. It has not been dropped, but ASR was started ahead of it, so it has no slot at the moment. The 0.1B model's built-in voice was judged poor but acceptable for now (2026-10-04).

## User interface (2026-10-04)

- **Sidebar layout.** The three tabs became a left sidebar with five pages: Read aloud, Voices, Transcribe, Models, Settings. Engine health and the activity log moved under Settings as Diagnostics.
- **Dark, calm greens.** The user likes the green look; the palette stays dark green, toned down from the earlier bright teal. The light theme is kept.
- **System fonts and inline icons**, so the app needs no font or icon downloads.
- **One colour per speaker.** The transcript uses eight distinct label colours, one for each speaker the diarizer can tell apart, and a grey dashed label for the unknown speaker.

## Models and where they come from

Recorded for the 0.2.0 release (2026-10-04), so the choices can be reproduced. Licences are the ones stated on each repository on that date.

| Use | Repository the app uses | Upstream model | Licence |
|---|---|---|---|
| Default text to speech (bundled) | [Verylicious/pocket-tts-ungated](https://huggingface.co/Verylicious/pocket-tts-ungated) | [kyutai/pocket-tts](https://huggingface.co/kyutai/pocket-tts) | CC BY 4.0 |
| Extra Kyutai preset voices (reference clips) | [kyutai/tts-voices](https://huggingface.co/kyutai/tts-voices) | the same | CC BY 4.0 (VCTK clips) and CC0 (LibriVox clips); credited in `src-tauri/binaries/kyutai-voices/ATTRIBUTION.txt` |
| Optional text to speech | [Edge0/audio8-TTS-0.1B-ONNX-INT8](https://huggingface.co/Edge0/audio8-TTS-0.1B-ONNX-INT8) | [Audio8/Audio8-TTS-Preview-0.1b](https://huggingface.co/Audio8/Audio8-TTS-Preview-0.1b) | Apache-2.0 (the ONNX repository) |
| Speech to text | [Recogment/parakeet-multitalker-int8-onnx](https://huggingface.co/Recogment/parakeet-multitalker-int8-onnx) | [nvidia/multitalker-parakeet-streaming-0.6b-v1](https://huggingface.co/nvidia/multitalker-parakeet-streaming-0.6b-v1) | NVIDIA Open Model License |
| Speaker diarization | [altunenes/parakeet-rs, `nemotron-3-diarization`](https://huggingface.co/altunenes/parakeet-rs/tree/main/nemotron-3-diarization) | [nvidia/Nemotron-3-Diarization](https://huggingface.co/nvidia/Nemotron-3-Diarization) | OpenMDW-1.1 |
| Full build only | [Qwen3-TTS 0.6B CustomVoice](https://huggingface.co/Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice), [Qwen3-TTS 0.6B Base](https://huggingface.co/Qwen/Qwen3-TTS-12Hz-0.6B-Base) | the same | Apache-2.0 |

## Housekeeping (2026-10-04)

Before starting ASR, unused code was removed and code shared by the two TTS runtimes was factored out into its own modules: settings, selection capture, model download, SoX and rate handling, bundled-file lookup, and text chunking. The unused `model_registry.json` was deleted. The docs (`README.md`, `docs/DESIGN_SPEC.md`, `docs/IPC_API.md`, `docs/learnings.md`) were brought up to date with the code. No product decision changed.
