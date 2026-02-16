VoiceReader binaries/assets in this folder depend on build profile.

Build profiles:
- Base build (`build-base`):
  - Uses Rust-native Kyutai runtime only (no Python sidecar).
  - Requires bundled Kyutai model files under:
    - src-tauri/binaries/models/Verylicious/pocket-tts-ungated
  - Does not include Qwen runtime/models/GPU path.
  - Kyutai in current app flow is English-only.
- Full build (`build-full`):
  - Uses Python sidecar runtime plus Kyutai/Qwen model paths.
  - Sidecar expected naming:
    - Windows: tts-engine-x86_64-pc-windows-msvc/tts-engine.exe
    - macOS (Intel): tts-engine-x86_64-apple-darwin/tts-engine
    - macOS (Apple Silicon): tts-engine-aarch64-apple-darwin/tts-engine
    - Linux (x86_64): tts-engine-x86_64-unknown-linux-gnu/tts-engine

Build helpers:
- Full sidecar build: npm run sidecar:build
- Bundle Kyutai model assets only: npm run models:bundle:kyutai

Portable packaging:
- Base portable: npm run desktop:build:base:portable
- Full portable: npm run desktop:build:full:portable
