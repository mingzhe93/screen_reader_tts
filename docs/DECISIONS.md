# VoiceReader Decisions

I settled on this direction on 2026-10-03, after a seven-month gap in development. This document records my decisions through v0.3.0 release preparation (updated 2026-10-19). I have tested and accepted the current macOS app, and since 2026-10-20 the Windows app. Newest decisions go at the top of each section. Dated release entries describe those releases; Metal ships in the main app from v0.2.3; v0.2.2 remains CPU-only on macOS.

## v0.3.0 Assistant and tools (implemented; macOS accepted 2026-10-19, Windows accepted 2026-10-20)

- **I tested v0.3.0 on Windows and it works** (2026-10-20). The Assistant, tools and the rest of the app were built and run on a real Windows machine. The Mac and Windows results are separate sign-offs, so repeat each on the release artifact. I also tested web search with my own Brave API key, and it works.
- **I will not release a curated model that does not work** (2026-10-20). I asked for the raw tool-call answer to be fixed before 0.3.0 and for MiMo to pass the tools qualification. Both are done; MiniCPM is not verified and needs my decision (keep with the warning, block tools for it, or remove it). Evidence and causes are in [the findings](assistant/tools.md#live-test-findings-and-the-minicpm-and-mimo-work-2026-10-20).
- **Models may carry their own tools guidance** *(AI-recommended)*. A catalogue field appended after the shared tools policy, and pinned in the report's prompt hash. MiMo has four lines; MiniCPM has its own; the Gemma models have none, so their prompts and pins did not change. I chose this over editing the shared policy because that would have meant requalifying every model.
- **Qualification follows the app's defaults** *(AI-recommended)*. Chat thinks and Voice does not (`VR_TOOLS_THINKING=app-defaults`), and a model that needs thinking (`tools_need_thinking`) always thinks when tools are on, in the app as in the test. MiMo's flag therefore means: with its guidance and Chat's Think toggle on.
- **Raw tool-call text is never an answer** *(AI-recommended)*. It is held back from the page and speech, the model gets one more request with a notice next to its last tool result, and otherwise the user gets a plain apology. A failed search is not repeated in the same turn. `run_python` echoes a final bare expression like a REPL.
- **Flags rest on fresh seeds, not only the official three** *(AI-recommended)*. MiniCPM passed the official run but not a six-seed check, so it stays unverified.

- **v0.3.0 is feature-complete (2026-10-20).** I tested Python calculations and web search in Voice on macOS. As a last change, every Chat and Voice system prompt carries today's date (with tools on, the time of day goes on the newest user message, keeping the prompt cache reusable). Further features and refinements go to v0.3.1; candidates are in the [Assistant guide](assistant/README.md#next-steps).

Details, checked facts and the decisions I accepted on the reviewer's recommendation (marked AI-recommended) are in the [v0.3.0 Assistant plan](v0.3.0_assistant_plan.md).

- **I settled the open decisions in a grilling review on 2026-10-18.** The audience is friends and family, so the infrastructure is sized for a spare-time project. At that planning review no Assistant code or model run existed; implementation and testing followed. The base Assistant is committed in `5e62732`, and the tools phase now extends it.
- **Tools are part of v0.3.0.** I added search, public-page reading, weather and optional WASI Python in both Chat and Voice. Each mode starts with tools off and has a wrench menu with All tools, per-tool switches and settings. The host enforces a per-turn capability snapshot; network tools do not download Python. See [the implementation](assistant/tools.md).
- **Thinking in Voice is optional.** It starts off, has its own budget and a collapsed transcript accordion, and is never spoken or retained in later history. Tool fillers and answer phrases share one response queue and cancellation lifecycle.
- **I accepted the current macOS app and UI.** The tool controls, reasoning display and final spacing/alignment work as intended. The default Voice prompt asks for decimal “point” wording; custom prompts are preserved. Windows and a live Brave key were tested later (2026-10-20) and work.
- **Plugins are planning only.** I want to create tools through Chat without rebuilding the app. I chose to save [the runtime plugin plan](assistant/tool-plugins-plan.md) and implement it later; runtime, permissions and credential choices remain open.
- **Version 0.3.0 is synchronized from package.json.** This replaces the testing suffix across desktop, lockfiles and the optional Python engine. The version bump prepares the release; it does not publish it or imply Windows acceptance.
- **Base build only.** The Full build is deprecated but its code stays; it shows no Assistant page.
- **I am pinning llama.cpp to each VoiceReader release.** The build tag, upstream URL and SHA-256 are compiled into the app; downloads are staged, verified and renamed into place. Independent backend updates, the signed catalogue and rollback are deferred. This replaces my earlier wish for backend updates without an app release.
- **One backend package per OS.** Windows x64 uses the upstream Vulkan zip, which also contains the CPU backends; macOS arm64 uses the Metal build. CUDA is skipped as too large until larger models need the speed.
- **The Assistant page has a chat page and a conversation mode.** Chat takes typed or push-to-talk input, streams Markdown answers, has a Speak button per message, a Think toggle (on by default) and image attachments. Conversation mode is voice in and voice out with a transcript side pane, push-to-talk turns, and pressing the button to interrupt.
- **I want to try automatic turn detection in conversation mode.** VAD plus Smart Turn ONNX sits behind an experimental toggle: on in my builds, off for friends and family until it stops cutting people off.
- **The implemented catalogue has four Q4 models.** MiniCPM-V-4.7-1B and MiMo-V2.6 9B use Q4_K_M; Gemma 4 E4B and Gemma 4 12B use UD-Q4_K_XL, each with its vision projector. The app recommends a tier from detected hardware and never downloads one automatically. Model files are pinned by revision and SHA-256.
- **Context is 32K by default.** A gear setting changes it on the next load and warns when the setting may not fit memory. I intend to make 128K the default on most machines later; that is kept in view, not decided.
- **The model loads when the Assistant page opens.** Explicit Load/Eject controls, plus unloading after 10 minutes idle.
- **A read-aloud action ends an Assistant response.** It cancels the request, pending text, synthesis and audio, matching the v0.2.2 hotkey rule.
- **The Assistant and Transcribe are mutually exclusive.** A meeting recording is never stopped automatically.
- **Out of v0.3.0:** an external API screen, saved chat history, custom Hugging Face models, a global push-to-talk hotkey, voice barge-in and runtime-installed/chat-created plugins. Tool calling was subsequently included by the tools-phase decision above.
- **The chat page ships first.** Order of work: managed backend and chat page, push-to-talk input, conversation mode, the auto-send experiment, then tiers and measurements.
- **I am choosing a Rust conversation controller** (2026-10-14). It reuses the existing ASR, TTS and player. Pipecat is a reference for conversation behaviour rather than a runtime dependency.
- **I want a separate appendable speech mode for conversation answers** (2026-10-14). Streamed phrases queue under one response and cancellation lifecycle, sharing synthesis and playback components with normal TTS. Normal read-aloud keeps its replacement behaviour.
- **The plan includes the earlier Opus 5.5 review** (2026-10-18). Its comments on playback feedback, resource ownership, slow-stream buffering and package variants are now decisions or open implementation work. Its update-authenticity and ordering comments are superseded by pinning and the chat-first order.

## Release 0.2.3 (prepared 2026-10-10; subsequently published for macOS and Windows)

- **I am including the validated Metal decoder in the main app.** I confirmed stable playback and decided to include it in v0.2.3. The standard Apple Silicon Base build moves only Audio8's codec decoder to the GPU; the separate preview feature, app name and build configuration are removed. Core ONNX Runtime 1.24.4 plus WebGPU plugin 0.4.0 use Dawn's Metal backend. Audio8 generation, voice registration, Kyutai and transcription stay on CPU. MLX is not needed for this implementation. Windows retains DirectML and Intel macOS retains CPU.
- **Use FP32 computation as the validated baseline.** The first FP16 GPU decoder was fast but produced unusable speech. Identical-code comparisons showed 85.6% relative RMS waveform difference from CPU; FP32 reduced it to 0.0321%. The small derived graph casts the existing FP16 weights at load time, preserving the download and CPU graph. Any later precision optimization needs the same waveform, streaming and listening checks. The exact FP16 operator/kernel failure remains unlocalized.
- **I accepted the quality and stability.** I found Preview 2 indistinguishable from the original CPU run by ear and stable enough for the main app. v0.2.3 uses a fresh FP32 Auto cache (`audio8-decoder-device-webgpu-fp32.json`) and CPU fallback for load/warm-up failures. Other Mac chips/OS versions, peak memory and the plugin's teardown diagnostic remain follow-ups, without retaining preview branding.
- **Report complete generation speed separately from decoder speed.** On an M2 Max, warm decoding was roughly 4–5x faster, while 51.08 seconds of speech at the 2x setting took 29.00 seconds on CPU and 19.82 seconds with CPU plus Metal: about 1.46x overall. Average supply rose from 1.76x to 2.58x real time. The traced buffer simulation still had an early refill; actual continuous 2x playback has not been measured, and no buffer-policy change was made. The measurements, graph, reproducible checks and limits are in [learnings section 15](learnings.md#15-native-webgpumetal-for-audio8-fp32-fixes-the-decoder-2026-10-10) and [METAL.md](METAL.md).
- **Continue distributing ad hoc signed Mac apps with first-launch instructions.** I am keeping this release free and do not plan to pay for Developer ID signing/notarization. Every macOS release must include the notice in [RELEASING.md](RELEASING.md). The standard Mac packager now signs and verifies the complete app before creating its portable ZIP.

## Release 0.2.2 (2026-10-09)

- **The player applies the speed, not the backend** (2026-10-09, as I requested). Speed changes used to reach only audio not yet generated, and Kyutai generates most of a text within seconds. The backend now sends audio at normal speed and the player time-stretches it while it plays, so a change is heard at once and the slider's `0.05x` steps are honoured (the backend rounded to `0.25x`). The stretcher is written in the app (WSOLA, two stages) rather than taken from a library. `VOICEREADER_RATE_IN_BACKEND=1` brings back the SoX path for comparison; SoX stays bundled for voice-cloning clips. Measurements are in `docs/learnings.md` section 13.
- **Chunks generated in parallel each get their own copy of the voice state** (2026-10-09). Sharing it garbled 11 of the 21 Kyutai presets, and cloned voices at random; the cause and the measurements are in `docs/learnings.md` section 14. I had asked for those 11 voices to be removed because they read badly. They now read as well as the rest, so which presets to drop is a question of taste and is still open.
- **A new read-aloud job replaces the one that is playing** (2026-10-09). Pressing the hotkey during playback stops the audio and empties the queue before the new text starts. Late audio and end-of-job events from the replaced job are ignored.
- **One manifest for both platforms** (2026-10-09). `macOSPrivateApi` (needed for the transparent toolbar window on macOS) is set in the shared `tauri.conf.json`, not in the macOS-only config. The Tauri CLI rewrites the `tauri` features in `Cargo.toml` to match the config, and the build fails when they disagree, so a macOS-only setting made `Cargo.toml` differ between a Mac and a Windows machine and broke plain `cargo` builds on Windows. The setting has no effect on Windows.
- **The clone form says that transcription is English only.** The note is on the Transcribe audio button itself, not only in its tooltip.
- **Next: skip back and skip forward.** The player keeps the audio at normal speed, which makes moving back and forth in it possible.

## Release 0.2.1 (2026-10-05)

- **macOS portable support is working on Apple Silicon.** Highlighted-text hotkeys and the floating playback toolbar have been tested. SoX is bundled for pitch-preserving speed changes, and both platforms use the updated app icon.
- **Audio8 GPU acceleration on macOS is deferred for this release.** Core ML inference failed with the shipped decoder, so macOS uses the CPU. GPU acceleration remains available on Windows through DirectML. The subsequent native Metal experiment is recorded above; this describes the regular 0.2.1 release.
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
- **Audio8 thread layout stays at the measured default.** More threads or parallel sessions did not raise throughput on the 9950X (see `docs/learnings.md` section 7.6). CPU Audio8 was usable up to about 1.5x playback there; the newer M2 Max CPU/Metal comparison is in section 15 and does not change the default thread layout.
- **Audio8 TTS is the second model**, added as a Rust-native ONNX Runtime backend in the base build:
  - `Edge0/audio8-TTS-0.1B-ONNX-INT8` first (Apache-2.0, about 430 MB for synthesis, primarily English and Chinese).
  - `Edge0/Audio8-TTS-Preview-0.6B-ONNX-INT4` later, as the optional multilingual download. Not built yet.
  - Not `Edge0/Audio8-TTS-Preview-0.1b`: that repo is the PyTorch build under a revenue-capped licence.
- **Audio8 does not become the default** (2026-10-04). It was to be decided after testing; Kyutai is much faster for English and I found Audio8's built-in voice poor but acceptable, so Audio8 stays the optional model.
- **The clone form can transcribe the reference clip** (2026-10-04, as I requested). Audio8 needs the exact transcript, and typing it is the tedious part of cloning. A Transcribe audio button fills it in with the transcription model, for both Kyutai and Audio8, and offers the model download when it is missing. The reference text moved below the reference audio so the order matches. The transcriber is English only, so the text still needs checking for other languages.
- **Voice cloning must keep working** for every TTS model. Audio8 needs the reference audio plus its exact transcript, so the clone flow gains a transcript field.
- **I tested the implemented Audio8 0.1B support (2026-10-03).** Measured on a Ryzen 9 9950X: about 0.8 s to first audio at 1.0x, about 1.4 s at 1.5x, roughly 2x real time at best, about 1.5 GB of memory while speaking. Details are in `docs/learnings.md` section 7.
- **ONNX Runtime is loaded as a shared library, not linked statically.** Static linking clashes with the protobuf copy inside the Kyutai runtime's dependencies. The library is fetched at build time by `scripts/fetch-onnxruntime.js` (ONNX Runtime 1.24.4 at the time of writing, 1.23.2 for Intel macOS) and is not committed. Transcription uses this same library.
- **GPU acceleration goes through ONNX Runtime's native providers** (2026-10-04; macOS status updated 2026-10-10). Windows uses DirectML for Audio8's decoder, chosen by benchmark with CPU fallback. Apple Silicon macOS uses native WebGPU through Dawn/Metal in the Rust process from v0.2.3, replacing the failed Core ML route; Intel macOS stays on CPU. Only graphs that measure faster are moved; generation stays on CPU, and Mac GPU generation has not been benchmarked. Details are in [learnings](learnings.md) sections 11 and 15.
- **I can choose the compute device** (2026-10-04): Auto (default), GPU or CPU on the Settings page (the Engine tab at the time). Auto means "GPU when available and faster than the CPU", not "GPU whenever present", because a weak integrated GPU measured four times slower than the CPU. CPU exists so the GPU can be left free for other work.
- **Chatterbox was considered and set aside** (2026-10-04). It promises better quality and cloning without a transcript, but is realistically a GPU model; Audio8 stays for now.
- **The Qwen ONNX port plan is dropped.** `docs/onnxruntime_plan.md` planned a Rust ONNX Runtime port of Qwen, which is no longer the goal. Its approach (ONNX Runtime via the `ort` crate, tokenizer, cached decode loop) was reused for Audio8, and the file was deleted on 2026-10-04.

## ASR

- **Built in stages, files first** (2026-10-04). Stage 1 transcribes an audio file with speaker labels. I asked for live microphone transcription the same day to make testing easier, and it was added. System audio is the next stage. I test each stage before moving on to the next.
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
  - **Speakers beyond the limit are transcribed as "Unknown speaker"** (2026-10-04, as I requested), instead of being left out. They share one label. This cannot help with more than eight voices: the diarizer has eight slots and files further voices under the existing ones, so that speech is not missing, only mislabelled.
- **Word boosting is a follow-up.** `parakeet-rs` shows no sign of supporting it. It can be added at decode time (a prefix tree of tokenized custom words that adds a bonus to matching tokens), which means a fork or an upstream contribution.
- **Not chosen:** `Edge0/Audio8-ASR-0.1B-onnx-runtime` (CC BY-NC, no streaming, truncates at 30 seconds). Plain Nemotron 3.5 ASR plus diarization remains the fallback if multilingual transcription matters more than crosstalk handling.

## Order of work

1. Flip the build defaults to base. Done 2026-10-03.
2. Add Audio8 0.1B INT8 support, with cloning. Done 2026-10-03.
3. Test it; decide whether it becomes the default. Done 2026-10-04: Kyutai stays the default, Audio8 stays as the optional model.
4. Housekeeping before ASR. Done 2026-10-04.
5. Redesign the main window, add file transcription and live microphone transcription. Done 2026-10-04; I tried it on recordings and the microphone.
6. Release version 0.2.0 on GitHub. Published, followed by 0.2.1 (2026-10-05) and 0.2.2 (2026-10-09).
7. Release 0.2.3 with validated Metal decoding. Published for macOS and Windows.
8. Optional Assistant and tools. Implemented and accepted on macOS for v0.3.0; validate Windows next.
9. Runtime plugins/chat-created tools, system-audio transcription and word boosting remain planned follow-ups, not part of the completed Assistant scope.

The on-demand Audio8 0.6B INT4 model was originally step 4. It has not been dropped, but ASR was started ahead of it, so it has no slot at the moment. I found the 0.1B model's built-in voice poor but acceptable for now (2026-10-04).

## User interface (2026-10-04)

- **Sidebar layout.** The three tabs became a left sidebar with five pages: Read aloud, Voices, Transcribe, Models, Settings. Version 0.3.0 adds Assistant as a sixth page in the Base build. Engine health and the activity log moved under Settings as Diagnostics.
- **Dark, calm greens.** I like the green look; the palette stays dark green, toned down from the earlier bright teal. The light theme is kept.
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
