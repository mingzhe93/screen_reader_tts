Place bundled sidecar binaries in this folder.

Expected naming:
- Windows: tts-engine-x86_64-pc-windows-msvc/tts-engine.exe
- macOS (Intel): tts-engine-x86_64-apple-darwin/tts-engine
- macOS (Apple Silicon): tts-engine-aarch64-apple-darwin/tts-engine
- Linux (x86_64): tts-engine-x86_64-unknown-linux-gnu/tts-engine

Use:
- npm run sidecar:build

to generate and copy the platform sidecar here before packaging.

Bundled model assets:
- src-tauri/binaries/models/Verylicious/pocket-tts-ungated

The packaged app will use this bundled Kyutai model path first (offline first-run),
then fall back to on-demand download only if the bundled model is missing.
