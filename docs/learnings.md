# VoiceReader TTS Pipeline Learnings

This file captures implementation-level lessons from the current Rust
(`build-base`) and Python sidecar (`build-full`) playback pipelines.

## 1. SoX starvation with tiny streaming tokens

When `rate != 1.0`, feeding SoX with very small token-sized buffers causes
starvation. The frontend drains output faster than SoX can produce consistent
frames from tiny inputs, creating audible discontinuities.

Key lesson:
- For rate-adjusted playback, batch-oriented chunk processing is more stable
  than token-by-token feeding.

## 2. Parallel look-ahead reduces inter-chunk gaps

Rust base runtime overlaps:
- chunk generation (`model.generate`) on background threads
- SoX processing + emission on main thread

The next chunk is pre-generated while the current chunk is still being pushed
through SoX and emitted. This removes most dead-air between chunks on CPU
workloads.

Python sidecar loop uses a similar pattern:
- pre-submit chunk `N+1` synthesis before waiting on SoX processing for chunk
  `N`
- run playback control DSP in worker threads to keep the event loop responsive

## 3. Live rate control architecture

### 3.1 Base build (Rust)
- Active rate is shared via `AtomicU32` step values (`0.25x` increments).
- Synthesis output is processed in segments.
- Runtime polls desired rate every `RATE_CONTROL_POLL_SAMPLES` (`960`) and can
  switch rate inside a generated chunk.
- Segment transitions flush current SoX output before creating a new SoX stream
  for the new rate.

Practical result:
- rate changes feel near-immediate during streaming, with sub-chunk granularity
  rather than whole-chunk delays.

### 3.2 Full build (Python sidecar)
- `POST /v1/jobs/{job_id}/playback` mutates active job playback settings.
- Updated values are consumed in the job loop during chunk processing.
- Effective audio change is chunk-cycle granularity.

## 4. Pitch-preserving rate control fallback chain

Current fallback order in sidecar:
1. SoX tempo
2. librosa time-stretch
3. linear resample

Implication:
- pitch preservation depends on SoX/librosa availability
- fallback linear resampling changes pitch

## 5. Syncing UI state with backend playback state

`voicereader:rate-updated` is emitted after rate changes so both the main UI and
floating toolbar stay consistent with the effective backend rate value.

Without this event, UI elements drift and users see stale controls.

## 6. Window lifecycle coupling matters for packaging

A floating toolbar as a separate window can keep the process alive after main
window close if not explicitly closed.

Current fix:
- on main-window close/destroy, close toolbar window and exit app

This prevents stale process handles that can block portable packaging cleanup on
Windows.
