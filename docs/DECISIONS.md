# VoiceReader Decisions

Direction agreed on 2026-10-03, after a seven-month gap in development. Newest decisions go at the top of each section.

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
- **Audio8 TTS is the next model**, added as a Rust-native ONNX Runtime backend in the base build:
  - `Edge0/audio8-TTS-0.1B-ONNX-INT8` first (Apache-2.0, about 430 MB for synthesis, primarily English and Chinese).
  - `Edge0/Audio8-TTS-Preview-0.6B-ONNX-INT4` later, as the optional multilingual download.
  - Not `Edge0/Audio8-TTS-Preview-0.1b`: that repo is the PyTorch build under a revenue-capped licence.
- **Audio8 may become the default** if it performs well in testing. That is decided after the model change is tested, not before.
- **Voice cloning must keep working** for every TTS model. Audio8 needs the reference audio plus its exact transcript, so the clone flow gains a transcript field.
- **Audio8 0.1B support is implemented (2026-10-03), pending the user's listening test.** Measured on a Ryzen 9 9950X: about 0.8 s to first audio at 1.0x, about 1.4 s at 1.5x, roughly 2x real time at best, about 1.5 GB of memory while speaking. Details are in `docs/learnings.md` section 7.
- **ONNX Runtime is loaded as a shared library, not linked statically.** Static linking clashes with the protobuf copy inside the Kyutai runtime's dependencies. The library is fetched at build time by `scripts/fetch-onnxruntime.js` (ONNX Runtime 1.24.4 at the time of writing, 1.23.2 for Intel macOS) and is not committed. The ASR work should reuse this same library.
- **GPU acceleration goes through ONNX Runtime's native providers, not WebGPU in the app window** (2026-10-04). DirectML on Windows is implemented for the Audio8 decoder and chosen by benchmark, with CPU fallback; Core ML on macOS is wired in but opt-in until tested on a Mac. Only graphs that measure faster on the GPU are moved; generation stays on the CPU. Details are in `docs/learnings.md` section 11.
- **The user can choose the compute device** (2026-10-04): Auto (default), GPU or CPU on the Engine tab. Auto means "GPU when available and faster than the CPU", not "GPU whenever present", because a weak integrated GPU measured four times slower than the CPU. CPU exists so the GPU can be left free for other work.
- **Chatterbox was considered and set aside** (2026-10-04). It promises better quality and cloning without a transcript, but is realistically a GPU model; Audio8 stays for now.
- **The Qwen ONNX port plan is dropped.** `docs/onnxruntime_plan.md` planned a Rust ONNX Runtime port of Qwen, which is no longer the goal. Its approach (ONNX Runtime via the `ort` crate, tokenizer, cached decode loop) was reused for Audio8, and the file was deleted on 2026-10-04.

## ASR (extended goal, starts after the TTS model change is tested)

- **A separate ASR tab** for speech to text, including meetings with speaker labels ("speaker 1 said this, speaker 2 said that").
- **Model choice: multitalker Parakeet plus the Nemotron-3 diarizer**, because crosstalk is common in meetings and this pairing is built for overlapping speech.
  - `Recogment/parakeet-multitalker-int8-onnx` (about 666 MB) and `onnx-community/Nemotron-3-Diarization-ONNX` (83 to 120 MB quantized).
  - Runtime: the `parakeet-rs` crate, which uses `ort`, so it shares the inference layer with Audio8.
  - The GPU-enabled ONNX Runtime library added for Audio8 is the one ASR should use. The same rule applies: benchmark each graph, and move it to the GPU only if it is faster there.
  - **English only for now.** The tab must say so.
  - Plan around 4 speakers until tested: the multitalker card describes 4 slots, the diarizer supports 8.
- **Word boosting is a follow-up.** `parakeet-rs` shows no sign of supporting it. It can be added at decode time (a prefix tree of tokenized custom words that adds a bonus to matching tokens), which means a fork or an upstream contribution.
- **Not chosen:** `Edge0/Audio8-ASR-0.1B-onnx-runtime` (CC BY-NC, no streaming, truncates at 30 seconds). Plain Nemotron 3.5 ASR plus diarization remains the fallback if multilingual transcription matters more than crosstalk handling.

## Order of work

1. Flip the build defaults to base. Done 2026-10-03.
2. Add Audio8 0.1B INT8 support, with cloning. Done 2026-10-03.
3. Test it; decide whether it becomes the default. Done 2026-10-04: Kyutai stays the default, Audio8 stays as the optional model.
4. Housekeeping before ASR. Done 2026-10-04.
5. Start the ASR tab. Next.

The on-demand Audio8 0.6B INT4 model was originally step 4. It has not been dropped, but ASR was started ahead of it, so it has no slot at the moment. The 0.1B model's built-in voice was judged poor but acceptable for now (2026-10-04).

## Housekeeping (2026-10-04)

Before starting ASR, unused code was removed and code shared by the two TTS runtimes was factored out into its own modules: settings, selection capture, model download, SoX and rate handling, bundled-file lookup, and text chunking. The unused `model_registry.json` was deleted. The docs (`README.md`, `docs/DESIGN_SPEC.md`, `docs/IPC_API.md`, `docs/learnings.md`) were brought up to date with the code. No product decision changed.
