# Audio8 Metal support (v0.2.3)

The standard Apple Silicon Base build runs the Audio8 codec decoder through ONNX Runtime's native WebGPU plugin, using Dawn's Metal backend. The slow/fast autoregressive graphs, voice registration, transcription and Kyutai stay on their existing CPU paths. It uses FP32 decoder computation and reuses the existing FP16 weight download; no MLX or Python runtime is required in the app.

I found Preview 2 to be indistinguishable from the original CPU run by ear. Numerical and streaming checks passed on an M2 Max running macOS 27.0.1. The build targets macOS 14+, but other chips and OS versions have not been validated. After confirming stable app playback, I decided to include this implementation in the main app for v0.2.3. v0.2.2 remains CPU-only on macOS. Detailed evidence and the 2x streaming graph are in [learnings section 15](learnings.md#15-native-webgpumetal-for-audio8-fp32-fixes-the-decoder-2026-10-10).

Install the [macOS build prerequisites](../README.md#build-a-macos-portable-release-base), then build on an Apple Silicon Mac (macOS 14+):

```sh
npm ci
npm run desktop:build:macos:portable
```

The build pins core ONNX Runtime 1.24.4 and WebGPU plugin 0.4.0, verifies the plugin archive checksum and bundles its licenses. It produces `VoiceReader.app` and a portable ZIP at `src-tauri/target/release/bundle/portable/VoiceReader_0.2.3_macos_arm64_portable.zip`. The app has an ad-hoc signature; downloaded copies still require the macOS first-launch steps in [RELEASING.md](RELEASING.md).

Quit other VoiceReader copies before opening the new build. It reuses the existing model downloads, saved voices and preferences. Select **Audio8 TTS**, then **Auto** or **GPU** under **Compute Device**. The status should say `webgpu`. Select **CPU** to compare with the previous decoder. Auto benchmarks both providers and keeps the faster one only when it beats CPU by more than 1.5×. Its 7-day benchmark cache is `audio8-decoder-device-webgpu-fp32.json`, separate from the preview caches and Windows cache. If WebGPU cannot load or warm up, the existing CPU fallback applies and its reason appears in the status.

The first preview used native FP16 math and produced unusable speech. Controlled comparisons showed 85.6% relative RMS waveform difference from CPU on the 110-frame reference clip. Disabling graph optimizations or changing layout did not fix it. Promoting computation to FP32 reduced the difference to 0.032%, while retaining roughly 4× faster warm decoding on the M2 Max. This isolates the failure to the FP16 computation path; the exact sensitive operators have not yet been localized.

The app bundles a small derived graph under `src-tauri/binaries/audio8-metal/`. It casts the original FP16 weights to FP32 at load time and promotes floating-point computation, without duplicating or modifying the downloaded weight file. The app checks that the downloaded original graph exactly matches the validated export, then atomically caches the derived graph next to its original weight file. A missing asset, mismatched export or write/load failure uses the existing CPU fallback. FP32 increases decoder memory usage; speed figures apply to decoding, not the CPU autoregressive generation.

The generated graph, original graph and upstream Apache-2.0 license/notice are included for reproducibility. To regenerate these small assets after installing build-only dependencies `numpy` and `onnx`:

```sh
python3 scripts/prepare_audio8_metal_decoder.py --source-decoder /path/to/codec_decoder_fp16.onnx
```

The recipe rejects an unreviewed source graph checksum. Updating the supported export requires updating the pin and rerunning numerical and listening checks. The accepted listening result applies to the validated FP32 export; a new precision mode or export needs a new comparison.

With profiling disabled, the full backend pipeline generated the same 51.08-second English passage in 29.00 seconds on CPU and 19.82 seconds with Metal decoding at the 2x setting: about **1.46x faster overall**, or 32% less generation time. Average supply rose from 1.76 to 2.58 seconds of source speech per second. That exceeds 2x playback's average demand, but arrival bursts still produced one early refill in the buffer simulation. These figures exclude model loading, IPC, player scheduling and device startup; they do not establish uninterrupted 2x playback in the app. FP32 decoder peak memory has not yet been measured.

Native checks with downloaded model files (run in addition to the usual unit tests). Run them on the host with Metal access; a restricted runner could not discover a GPU adapter and the native provider aborted. That native abort cannot be recovered by the Rust CPU fallback:

```sh
export VOICEREADER_AUDIO8_TEST_MODEL_DIR="/path/to/Edge0/audio8-TTS-0.1B-ONNX-INT8"
export VOICEREADER_AUDIO8_TEST_OUTPUT_DIR="/tmp/voicereader-metal-test"
mkdir -p "$VOICEREADER_AUDIO8_TEST_OUTPUT_DIR"
cargo test --manifest-path src-tauri/Cargo.toml --release --features build-base webgpu_decodes_reference_audio -- --ignored --nocapture
cargo test --manifest-path src-tauri/Cargo.toml --release --features build-base webgpu_streaming_matches_cpu_for_generated_speech -- --ignored --nocapture
VOICEREADER_AUDIO8_DECODER_DEVICE=gpu cargo test --manifest-path src-tauri/Cargo.toml --release --features build-base speaks_and_clones_with_the_real_model -- --ignored --nocapture
```

The v0.2.3 Base build passed 48 unit tests and all three model-dependent checks below using its packaged ONNX Runtime/plugin. Fresh Auto selected WebGPU (551 ms CPU versus 98 ms Metal for the short decoder benchmark). Streaming, cloning and saved-voice re-encoding passed. Reference waveform error was 0.0321%; deterministic streamed speech differed from CPU by 0.0257% relative RMS.

The decoder check asserts GPU kernel execution, output length and finite/non-silent samples, and requires under 0.1% relative RMS waveform difference from CPU. It reports warm timings and writes `reference-cpu.wav` / `reference-webgpu.wav` from identical codes. The generated-speech check compares deterministic English/Chinese codes through full CPU decoding and the actual Metal streaming path, verifies no samples are lost, applies the same numerical limit and writes `parity-speech-cpu.wav` / `parity-speech-webgpu.wav`. The separate local-runtime streaming test exercises rate control and cloning and writes `speech-webgpu.wav`.

`VOICEREADER_AUDIO8_TEST_OUTPUT_DIR` also enables ONNX profiling, so use a separate run for fair throughput timing:

```sh
unset VOICEREADER_AUDIO8_TEST_OUTPUT_DIR
unset VOICEREADER_AUDIO8_DECODER_DEVICE
unset VOICEREADER_AUDIO8_PARALLEL_CHUNKS
unset VOICEREADER_AUDIO8_DECODERS
unset VOICEREADER_AUDIO8_DECODER_THREADS
export VOICEREADER_AUDIO8_BENCHMARK_OUTPUT_DIR="/tmp/voicereader-cpu-metal-benchmark"
cargo test --manifest-path src-tauri/Cargo.toml --release --features build-base measure_cpu_and_metal_streaming_throughput -- --ignored --nocapture
```

This writes `results.json` with PCM arrival timestamps and timings. The archived [benchmark data](benchmarks/audio8-cpu-metal-m2-max-2026-10-10.json) includes the supply simulation used for the graph.

Load/warm-up failures use CPU fallback; a failure during an active GPU job does not trigger an automatic CPU retry. The plugin currently logs `ReleaseEpFactory failed ... Unknown exception` at native test process teardown despite successful inference and test exit. That shutdown diagnostic, broader device/voice testing and actual app playback measurements remain open follow-ups, recorded in learnings section 15.8. I accepted the current implementation for the v0.2.3 main release.

For the macOS 27 toolchain where LLVM stripping produces invalid proc-macro dylibs, set `CARGO_PROFILE_RELEASE_STRIP=none` and `CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_STRIP=none` before building/testing.

Provider documentation: <https://onnxruntime.ai/docs/execution-providers/WebGPU-ExecutionProvider.html>.

Metal is supported only on Apple Silicon in this integration. Intel macOS stays on CPU; the plugin fetch skips it. Windows continues to use DirectML. The failed Core ML selection path and the preview-only Cargo feature/configuration have been removed.
