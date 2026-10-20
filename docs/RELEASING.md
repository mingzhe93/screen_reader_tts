# Release notes guide

For each release, describe the changes since the previous tag and list the exact
download filenames, supported architectures and minimum OS versions. Include the
following notice in every release that provides an ad hoc signed, unnotarized
macOS app. Update it if the signing policy changes.

## v0.3.0 preparation status (2026-10-19)

The source version is `0.3.0`. I have tested and accepted the current macOS Assistant/tools, thinking transcript and UI. I have since tested these additions on Windows and they work (2026-10-20). The Mac and Windows results are separate sign-offs; repeat each on the release artifact. The version bump and these notes do not publish a GitHub release.

Release notes should cover the optional app-managed llama.cpp/model downloads; Chat, images and spoken conversations; experimental automatic turn detection; optional search/page/weather/Python tools; separate Chat/Voice switches and Assistant-page tool settings; thinking budgets/collapsed Voice reasoning; and decimal “point” wording in the default Voice prompt. Mention the privacy boundary: local inference, but enabled internet tools send their own requests to providers. Runtime plugins/chat-created tools are planning only.

Keep `package.json` as the version source and run `node scripts/sync-version.js` before building. Check the desktop, both lockfile app entries and Full engine metadata all show `0.3.0`; do not rewrite archived benchmark records or old testing ZIP names. Preserve generated versioned asset names and list their exact names in the release.

### Assistant checks before publishing each platform artifact

- Install/load/eject the pinned backend and a model; check the device, memory estimate and diagnostics. Test Chat, image input, dictation, Voice, automatic sending and interruption using the [Assistant checklist](assistant/README.md).
- Exercise each tool separately, All tools, partial selections and Chat/Voice independence. Weather/search alone must not install Python. Verify the Python download/delete/reinstall path, error recovery and Stop/New chat/Eject during tool work. Use [tools.md](assistant/tools.md) for the specific failure cases; macOS acceptance does not imply every edge case was checked individually.
- Check the compiled-in tool dependency notices and downloaded backend/model/runtime licences in the extracted artifact. The macOS bundle includes `notices/assistant-tools.txt`; the Windows portable packager now copies `notices/` next to the exe (2026-10-20; it copied only `binaries/` before, so the 0.3.0 Windows zip built earlier has no notices). Re-package it with `python scripts/package_portable.py --variant base`, or rebuild with `npm run desktop:build:base:portable`, and check that `notices/assistant-tools.txt` is in the zip.
- Windows was built and tested on a real Windows machine (2026-10-20). Keep this as a checklist for each release: the Job Object, process probes, Vulkan/CPU fallback, file locking, microphone/TTS and packaged runtime paths. A live Brave key test also passed (2026-10-20).
- Keep existing TTS/transcription/selection/toolbar regression checks. Retain the macOS first-launch notice below in every Mac release.

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
- State the Audio8 compute support of the artifact being released. From v0.2.3, the standard Base build uses
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
