# Release notes guide

For each release, describe the changes since the previous tag and list the exact
download filenames, supported architectures and minimum OS versions. Include the
following notice in every release that provides an ad hoc signed, unnotarized
macOS app. Update it if the signing policy changes.

## First launch on macOS

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

## Other macOS notes to include

- Grant Accessibility access to the current app to read highlighted text. The
  default hotkey is Ctrl+Shift+S; keep the source app focused when pressing it.
- Grant Microphone access if you want to record for transcription.
- State the Audio8 compute support of the artifact being released. v0.2.3 uses
  CPU autoregressive generation plus native WebGPU/Metal FP32 decoding on Apple
  Silicon. Auto benchmarks it against CPU; GPU and CPU are also selectable. Intel
  remains CPU-only. v0.2.2 used CPU on macOS. See [METAL.md](METAL.md).

## Before uploading

The standard macOS portable command validates required assets and app/runtime
architectures, ad hoc signs the complete assembled app, verifies the signature,
then creates the ZIP. For Apple Silicon, it also requires the WebGPU plugin and
validated decoder graphs. No separate preview build or manual signing step is
needed:

```sh
npm run desktop:build:macos:portable
codesign --verify --deep --strict "src-tauri/target/release/bundle/macos/VoiceReader.app"
```

Verify the signature again on the extracted ZIP's `.app`, test launch, Kyutai and
Audio8 speech, selection capture, speed changes, microphone transcription and
recording/transcript export, and check the portable ZIP's architecture and bundled
runtime/assets. Keep the entire `.app` together.
Test a downloaded copy as well as the local build so first-launch behavior is
covered. An ad hoc signature does not provide an Apple-verified developer
identity or notarization, and does not suppress Gatekeeper's download warning.

For an Apple Silicon release that includes Metal, run the numerical,
streaming and cloning checks in [METAL.md](METAL.md), then compare
speech by ear. Test Auto/GPU/CPU and verify the reported device. Benchmark without
ONNX profiling; give the chip, OS, passage and scope alongside any performance
claim. Decoder speedup is not complete generation speedup, and the buffer graph
in [learnings section 15](learnings.md#15-native-webgpumetal-for-audio8-fp32-fixes-the-decoder-2026-10-10)
is a simulation, not measured uninterrupted 2x playback. Keep the measured scope and remaining follow-ups visible in release notes.
