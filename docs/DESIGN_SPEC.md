# VoiceReader Design Spec (Current Implementation Snapshot)

## 1. Scope

### 1.1 Product goal
Desktop read-aloud app with global hotkey capture, local TTS inference, and
streamed playback controls.

### 1.2 Build profiles
- Base build (`build-base`)
  - In-process Rust Kyutai runtime
  - No Python sidecar API
  - English-focused flow
- Full build (`build-full`)
  - Python sidecar runtime
  - HTTP + WS control/stream API
  - Kyutai + optional Qwen runtime paths

### 1.3 Current platform status
- Windows: primary implementation path
- macOS: partial parity
  - toolbar window mechanics are cross-platform
  - selection capture hotkey injection and foreground window title capture are
    currently Windows-only in Rust backend code

## 2. Core UX

### 2.1 Read selection flow
1. Global hotkey fires.
2. App captures selection via clipboard copy/restore flow.
3. Speak job starts and audio chunks stream into playback queue.
4. Floating toolbar appears with source label and playback controls.

### 2.2 Floating toolbar window
- Separate Tauri window (`toolbar.html`), not embedded in main UI.
- Window properties:
  - frameless (`decorations=false`)
  - transparent background
  - always on top
  - hidden from taskbar
  - initially hidden, shown only during active playback
- Visual style:
  - pill shell with semi-transparent background
  - centered source title and controls

### 2.3 Toolbar controls
- Rate button:
  - click cycles by `+0.25x`
  - wraps `4.0x -> 0.25x`
- Pause/resume toggle
- Stop
- Skip forward
- Skip back currently no-op feedback flash

### 2.4 Toolbar placement behavior
- Default startup placement: bottom-left of current monitor, margin `20px`.
- Last dragged position is persisted in webview localStorage and restored on
  next launch.

## 3. Runtime Control Model

### 3.1 Shared settings
Speak settings include:
- `rate` (`0.25..4.0`)
- `pitch` (`0.5..2.0`)
- `volume` (`0.0..2.0`)
- `chunk_max_chars` (`100..2000`)

### 3.2 Live rate updates while streaming
Rate changes can be applied during an active job.

- Base build:
  - active rate shared through `AtomicU32` (`active_rate_steps`)
  - synthesis loop polls desired rate during output processing
  - rate transitions can occur inside a generated chunk (sub-chunk segments)
- Full build:
  - app posts playback updates to sidecar endpoint:
    `POST /v1/jobs/{job_id}/playback`
  - sidecar applies updated job playback values during chunk processing loop

### 3.3 App-wide rate sync
When rate changes, backend emits `voicereader:rate-updated` so:
- main window setting input stays synced
- toolbar rate display stays synced

## 4. Selection Capture and Source Context

### 4.1 Selection capture path (implemented)
- Clipboard probe and restore logic is used.
- Simulated copy keystroke is currently implemented only for Windows.

### 4.2 Source window label
- Foreground window title is captured on Windows and attached to job-start
  payload.
- Toolbar displays normalized app/source label from this title.
- Non-Windows fallback is empty title -> `"Reading aloud..."`.

## 5. Audio Pipeline

### 5.1 Streaming model
- Text is chunked by sentence/char constraints.
- Chunks are synthesized and emitted as PCM frames.
- Frontend queues chunks and schedules playback continuously.

### 5.2 Time-scale behavior
- Base build prefers SoX tempo stream when available for pitch-preserving rate
  control.
- Full build sidecar prefers SoX, then librosa, then linear resample fallback.
- Volume is PCM gain scaling.
- Pitch control is reserved in current sidecar playback path.

## 6. App Lifecycle and Window Management
- Toolbar window is created at startup.
- Main-window close event explicitly closes toolbar window and exits app to
  avoid orphaned processes or stuck file locks.
- Engine/runtime shutdown is still executed on exit events.

## 7. Storage and Persistence
- Engine settings/state persisted under app data directory.
- Toolbar position persisted in toolbar webview localStorage key:
  `voicereader.toolbar.position.v1`.

## 8. Known Gaps
- macOS parity for selection capture injection and source-window title is not
  yet implemented.
- Skip-back control has no seek implementation yet.
- Sidecar playback pitch parameter is accepted but not actively transformed.
