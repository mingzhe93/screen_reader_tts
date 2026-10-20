# Assistant voice input (`assistant_voice.rs`)

Status on 2026-10-19: implemented and wired into the committed Assistant (`5e62732`). I have tested the Assistant working on macOS and, since 2026-10-20, on Windows. The original implementation notes below describe the capture/ASR boundaries; the Tauri command wiring now exists in `voicereader_core.rs`.

## What it does

A voice-input session keeps one microphone open and one ASR model loaded while the Assistant needs voice input. It turns the audio into one transcript per push-to-talk turn, with no audio lost or blended between turns. It implements `docs/v0.3.0_assistant_plan.md`:

- §7.2: control messages travel in order with the audio, and one ASR worker handles them strictly in order, resetting the model after each turn.
- §7.2, Codex F6: the **capture thread** produces the turn boundaries, not the button.
- §7.2: the existing recording-end flush (pad the last chunk, then three chunks of silence) releases trailing words on every turn.
- §7.1/§7.2: the model stays loaded for the session, not per turn.
- §7.3: the experiment (results below).

Not in this module: admitting a finished transcript while a newer turn is recording (Codex F7, the coordinator decides), auto-send (§7.4), and the mutual exclusion with Transcribe (§7.1, the coordinator holds the ASR slot).

## Files

| File | Change |
|---|---|
| `src-tauri/src/assistant_voice.rs` | New. The session, the capture stage, the ASR worker, the engine trait and the tests. |
| `src-tauri/src/audio_capture.rs` | New `InputStream`, which pushes raw mono blocks into a channel the caller owns. The device lookup and stream building moved from `MicrophoneSource::open_recorded` into a shared private `open_input_stream`, and `build_stream` is generic over the message type (`M: From<Vec<f32>>`). This is a behaviour-preserving extraction, not a purely additive change: `open_recorded` does the same steps in the same order with the same error messages. |
| `src-tauri/src/asr_local.rs` | `load_model` and `SAMPLE_RATE` are now `pub(crate)`. Nothing else changed. |

No new dependencies. `lib.rs` declares this module behind `build-base`; the microphone commands are wired in `voicereader_core.rs`.

## Design

### Threads and channels

```
cpal callback ──Block(raw, device rate)──┐
                                         ├─► capture queue (unbounded mpsc) ─► capture thread
session API ──Begin/End/Cancel/Close─────┘                                        │ resample to 16 kHz,
                                                                                  │ pre-roll, turn state
                                          ASR queue (sync_channel, 2400 msgs) ◄───┘
                                                   │  Audio{turn_id, samples}, EndTurn(id), CancelTurn(id)
                                                   ▼
                                              ASR worker ─► events: partial, final_text
```

- **Capture thread** (`assistant-voice-capture`). It opens the microphone itself, because a cpal stream cannot move between threads. It reads one queue that holds both the microphone's blocks and the session's turn messages, resamples to 16 kHz with the existing `StreamResampler`, and forwards a turn's audio in blocks of at least 100 ms.
- **ASR worker** (`assistant-voice-asr`). It owns the model and handles the ASR queue in order.

### Why the boundary is in-band

`begin_turn`, `end_turn` and `cancel_turn` do not touch the ASR queue. Each one puts a message into the **same queue as the microphone's blocks**. Every block the audio driver delivered before the call is therefore ahead of the boundary, and every later block is behind it. On `End(id)` the capture thread forwards what it holds for the turn, flushes the resampler's tail, and only then sends `EndTurn(id)`. Because the capture thread is the only writer of the ASR queue, FIFO order there equals recording order. If the button release went straight to the ASR queue, a block still waiting in the capture stage would arrive after `EndTurn` and count toward the next turn. That is Codex F6.

"Captured" means delivered by the audio driver. Audio still in the driver's own buffer at the moment of the call (one hardware buffer, normally 10 to 20 ms, more on Bluetooth) is not part of the turn.

### Message order in the worker

- `Audio { turn_id, samples }`: appended to the pending buffer; each full chunk (1.12 s, 17,920 samples) is fed to the model, and `partial(turn_id, text)` is sent when the text changed.
- `EndTurn(id)`: if the turn heard any audio, the last partial chunk is padded with silence and three silence chunks follow (`AsrEngine::flush`). Then `final_text(id, text)` is sent and the model is `reset()`. An empty turn (press and release with no audio) still gets exactly one `final_text(id, "")`, without running the model.
- `CancelTurn(id)`: the pending audio is dropped and the model is reset; there is no `final_text`.

Audio for the next turn that arrives during a flush waits in the queue and is processed after the reset. The unit tests check this ordering.

### Turn state in the capture thread

`Idle` → `Begin(id)` → `Open(id)` → `End(id)` or `Cancel(id)` → `Idle`. While `Idle`, audio is discarded, which is the "microphone closed while the assistant speaks" rule, except for the pre-roll.

### Pre-roll (300 ms)

The last 300 ms heard while no turn was open is sent at the start of the next turn, so a word begun just before the press is not clipped. After a turn ends the pre-roll starts empty, so no audio from the previous turn is repeated. The §7.3 experiment supports this: when turn 1 started exactly at the first speech sample with no audio before it, the model dropped the first word ("The"); with 0.5 s of quiet before the press, it kept it.

Risk: with loudspeakers, up to 300 ms of whatever played before the press (for example the end of a spoken answer) can reach the turn. If that shows up in testing, lower `PRE_ROLL` in `assistant_voice.rs`, or make the coordinator stop playback a moment before `begin_turn`.

### Long turns (120 s)

A turn that reaches 120 s of audio (pre-roll included) is ended automatically: the capture thread flushes and sends `EndTurn(id)`, so `final_text(id, ...)` arrives while the user may still hold the button. The rest of that turn's audio is discarded. The caller's later `end_turn(id)` or `cancel_turn(id)` succeeds and produces no further event; a cancel cannot take back the final text already sent. The audio is truncated rather than continued under a new turn id because the caller owns the id space, and an id invented by the session could clash with the caller's next one.

### Bounded memory

- ASR queue: `sync_channel(2400)`. Audio goes in messages of at least 100 ms, so this is about 240 s of audio (about 15 MB), twice the longest turn. The capture thread uses `try_send` and never blocks. A full queue means the model cannot keep up, and the session fails with "Speech recognition cannot keep up with the microphone."
- Capture queue: unbounded, because the cpal callback must never block. It stays short in practice because the capture thread only resamples and does a non-blocking send per block.
- Pre-roll: at most 4,800 samples. Turn text: at most 120 s of speech.

### Single speaker

The model is loaded with `load_model(models_dir, 1)`, one speaker as in `transcribe_clip`, with speech from other diarizer slots still transcribed as the "beyond the limit" speaker. The engine joins the text of all speaker instances, in order of first appearance, into one string (`get_transcripts()`, words joined with single spaces). Each instance keeps its own text, so one speaker's half word is never glued onto another's. If the diarizer moves the user to a second slot mid-turn, that slot's words come after the first slot's, not interleaved. The experiment gave identical transcripts with 1 and 8 speakers.

### Failure and shutdown

- A microphone error from cpal, no audio for 10 s (`SILENT_DEVICE_TIMEOUT`, as in Transcribe), a model error, a full ASR queue or a panic in either thread calls `error(message)` **once**. Both threads then stop, the microphone is closed, and later `begin_turn`/`end_turn`/`cancel_turn` calls return `Err("Voice input stopped: ...")`.
- `close()` and `Drop` set a stop flag and send `Close`. Both threads check the flag at least every 50 ms, and the worker also checks it between chunks. The session waits for both threads for **at most 3 s in total** and leaves a thread that is still busy (one model call) to finish by itself. Turns still queued are abandoned without `final_text`, and errors during close are not reported.
- If a thread is left to finish, the model is freed when it does, normally within one chunk of compute (about 100 ms here).

## Interface (all `pub(crate)`)

```rust
pub(crate) trait VoiceEvents: Send + Sync + 'static {
    fn level(&self, rms: f32);                    // RMS 0..1 of the last 50 ms, ~20/s, only while a turn is open
    fn partial(&self, turn_id: u64, text: &str);  // whole text so far, after each chunk that changed it
    fn final_text(&self, turn_id: u64, text: &str); // exactly once per ended turn (may be "")
    fn error(&self, message: &str);               // at most once; the session is dead after it
}

pub(crate) struct VoiceSession { .. }
impl VoiceSession {
    pub(crate) fn open(models_dir: &Path, device: Option<&str>, events: Arc<dyn VoiceEvents>) -> Result<Self>;
    pub(crate) fn begin_turn(&self, turn_id: u64) -> Result<()>;
    pub(crate) fn end_turn(&self, turn_id: u64) -> Result<()>;
    pub(crate) fn cancel_turn(&self, turn_id: u64) -> Result<()>;
    pub(crate) fn close(self);   // also on Drop
}

pub(crate) trait AsrEngine: Send {        // the model behind a trait, for tests
    fn chunk_samples(&self) -> usize;
    fn feed(&mut self, chunk: &[f32]) -> Result<String>;   // utterance text so far
    fn flush(&mut self, tail: &[f32]) -> Result<String>;   // default: pad + 3 silence chunks
    fn reset(&mut self);
}
```

- `open` is blocking: it loads the model first (about 2 s on an M2 Max), then opens the microphone on the capture thread and returns once the microphone is running. An unknown device returns `The microphone "<name>" is no longer available.`; no device returns `No microphone was found. ...`. `device` is a name from `audio_capture::list_inputs()`, the same list Transcribe uses.
- `begin_turn(id)` fails if a turn is open or `id` is not greater than the last one.
- `end_turn(id)` and `cancel_turn(id)` fail if `id` is not the open turn.
- The methods take `&self` and are safe to call from any thread. Events arrive on the session's own threads: `level` from the capture thread, and `partial`/`final_text` from the worker.
- `VoiceSession` is used by the `assistant_mic_*` Tauri commands; the current wire contract is in [conversation.md](conversation.md).

## Original integration checklist (completed in `5e62732`)

These were implementation notes. Actual command/event names are `assistant_mic_*` / `assistant:mic-*`, documented in [conversation.md](conversation.md).

1. **Commands** (suggested names, camelCase arguments in JS): `assistant_voice_open { device?: string }` (on `spawn_blocking`), `assistant_voice_begin { turnId }`, `assistant_voice_end { turnId }`, `assistant_voice_cancel { turnId }`, `assistant_voice_close`. Keep the session in app state, for example `Mutex<Option<VoiceSession>>`.
2. **The ASR slot.** Hold `ASR_JOB` in `voicereader_core.rs` for the session's whole lifetime: take it before `open`, release it after `close` returns, and if `open` fails. Per §7.1, starting Transcribe or reference-clip transcription while a session is open ends the session, and opening a session while Transcribe runs is refused with "Transcription in progress". If `close` timed out, the worker can still hold the model for up to one chunk of compute; that is acceptable for a slot measured in seconds.
3. **Events** (payload fields snake_case, as in `src/types.ts`):
   - `assistant-voice-level` `{ rms: number }`
   - `assistant-voice-partial` `{ turn_id: number, text: string }`
   - `assistant-voice-final` `{ turn_id: number, text: string }`
   - `assistant-voice-error` `{ message: string }`. After it, drop the session and release the ASR slot.
4. **Turn ids** come from the frontend or the controller and must increase for the life of a session. Use them to apply the F7 admission rule (§7.2): a `final` for an older turn that arrives while a newer turn is open is held and joined with the newer turn's text.
5. **Close** on Assistant page exit, model unload, app exit and when Transcribe takes the slot.

## Tests

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"   # macOS here
cd src-tauri
cargo test --features build-base --lib assistant_voice::
```

Fourteen unit tests with a fake engine and a fake microphone. The fake writes block number `n` as the sample value `n`, and the fake engine "hears" it as the word `bn`, emitting each chunk's words one chunk late like the real model, so a missing flush loses words. The tests cover:

- blocks queued in the capture stage before `end_turn` all belong to that turn (the capture thread is held at a gate while blocks and boundaries are queued);
- the resampler tail (48 kHz input) is in the turn before `EndTurn`;
- audio arriving during a flush goes to the next turn, after `reset`, and no model chunk mixes two turns;
- `cancel_turn` gives no `final_text`;
- audio with no open turn is discarded, and the pre-roll keeps only the audio just before the press;
- `final_text` arrives exactly once per turn and in order, including an empty turn;
- calls out of order are rejected;
- the 120 s limit (scaled down) ends the turn and a later release adds nothing;
- a model error, a microphone error and a silent microphone are each reported once, later calls fail, and `close`/`Drop` return within 1 s;
- a microphone that cannot open makes `open` fail;
- `close` does not wait for a busy model or report an error.

Existing tests for the touched files: `cargo test --features build-base --lib audio_capture::` and `asr_local::`.

### §7.3 experiment (real model, `#[ignore]`)

```bash
VOICEREADER_ASR_MODELS_DIR="$HOME/Library/Application Support/com.voicereader.desktop/data/models" \
  cargo test --release --features build-base --lib -- --ignored --nocapture assistant_voice::tests::asr_turn_experiment
```

Windows: set `VOICEREADER_ASR_MODELS_DIR` to `%LOCALAPPDATA%\com.voicereader.desktop\data\models`, and `VOICEREADER_ASR_CLIP_A` / `VOICEREADER_ASR_CLIP_B` to WAV recordings of the two sentences, because `say` does not exist there:

- A: "The quick brown fox jumps over the lazy dog beside the river bank."
- B: "Please remind me to call the dentist tomorrow morning at nine."

Optional: `VOICEREADER_ASR_THREADS` (as for Transcribe) and `VOICEREADER_ASR_MAX_SPEAKERS` (default 1).

The test makes the clips with `say` and `afconvert` (16 kHz WAV), loads one model, and plays the clips through a session in 20 ms blocks at real-time pace, as a microphone would. There is 0.5 s of quiet before each press:

- (a) A and B back to back as one turn (turn 1);
- (b) A, `end_turn`, 0.5 s quiet, then B as a new turn (turns 2, 3);
- (c) `end_turn` for A and `begin_turn` for B at once, with B playing while A is flushed (turns 4, 5).

It checks that the trailing words are kept, that each word appears at most once, that the first word of B is kept, and that no A words appear in B and no B words in A. Latency is measured from the `end_turn` call to the `final_text` event.

Results on an M2 Max (release build, 2026-10-18), clips of 3.83 s (A) and 3.45 s (B), model load 1.7 to 1.8 s:

| Setting | Turn 1 (a) | Turn 2 (b, A) | Turn 3 (b, B) | Turn 4 (c, A) | Turn 5 (c, B) |
|---|---|---|---|---|---|
| default threads (6), 1 speaker | 410 ms | 322 ms | 270 ms | 313 ms | 272 ms |
| 4 threads, 1 speaker | 506 ms | 388 ms | 325 ms | 379 ms | 337 ms |
| 4 threads, 8 speakers | 504 ms | 387 ms | 322 ms | 381 ms | 333 ms |

The transcripts were identical in every run:

1. "The quick brown fox jumps over the lazy dog beside the riverbank. Please remind me to call the dentist tomorrow morning at 9"
2. and 4. "The quick brown fox jumps over the lazy dog beside the river bank"
3. and 5. "Please remind me to call the dentist tomorrow morning at nine"

Findings:

- Trailing words were kept in every turn, and nothing was duplicated, lost or leaked between turns. Case (c) matched case (b): B was recorded while A was being flushed and lost nothing, which means the reset did not degrade resumed speech in this test. That was Opus's open hypothesis in §7.3.
- End-of-turn latency is about 0.3 s for a 4 s turn and about 0.4 to 0.5 s for a 7 s turn on 4 threads. It includes the four flush chunks (4.5 s of audio) plus the tail of the turn not yet processed. Because chunks are processed as they fill, it does not grow much with turn length.
- The same model in one session gave slightly different text for turn 1 ("riverbank", "9") because A and B were one utterance there.
- Without any audio before the press (an earlier run), turn 1 lost its first word "The". The pre-roll covers this with a real microphone.
- 1 and 8 speakers gave the same text and latency with one voice, so the session uses 1 (as `transcribe_clip` does).

This machine is not the constrained profile of §9.1. Run the experiment on Windows with `VOICEREADER_ASR_THREADS=4` and compare.

## Manual checks (Windows, then macOS)

These are manual hardware checks; the wiring is complete. I have since tested the base Assistant working on macOS.

1. **Build:** `cargo build --features build-base` shows no new warnings, and `cargo test --features build-base --lib assistant_voice::` passes.
2. **Experiment on Windows** (above) with `VOICEREADER_ASR_THREADS=4`. Expected: all assertions pass, and latency is under about 1 s. Record the figures here.
3. **Microphone permission.** Windows: Settings → Privacy & security → Microphone, turn off "Let desktop apps access your microphone", then open a session. Expected: either `open` fails, or no audio arrives and after 10 s the error "No audio is arriving from the microphone..." appears once. Windows may instead deliver silence, in which case turns transcribe as empty text; note which. macOS: deny VoiceReader in System Settings → Privacy & Security → Microphone. macOS usually delivers silence when permission is denied, so expect empty finals rather than an error, and note what happens.
4. **Device selection.** Open with a named device from the Transcribe device list; speak; expect text. Unplug a USB or Bluetooth microphone during a session. Expected: exactly one error event (a cpal stream error, or the 10 s silence error), later calls fail, and the app stays responsive. Open with a device name that no longer exists. Expected: `The microphone "<name>" is no longer available.`
5. **Latency by hand:** hold, say one sentence, release. Expected: final text within about 1 s of the release on the constrained profile, with the last word present.
6. **Quick re-press:** release and press again at once, then speak. Expected: two finals, the first with only the first utterance and the second with only the second.
7. **Pre-roll echo:** with loudspeakers, press while a spoken answer is still playing. Check whether its last word appears in the turn (see "Pre-roll").
8. **Close:** close the session in the middle of a turn and while a long turn is flushing. Expected: it returns within 3 s, and Transcribe can start straight afterwards.
9. **Long turn:** hold for more than 120 s. Expected: a final at 120 s; the release afterwards adds nothing.

## Known gaps and next steps

- Commands, events, ASR-slot exclusivity and the UI are wired in the committed Assistant.
- F7 admission (joining an older final into a newer turn) belongs in the conversation controller, not here.
- `level` is RMS, while Transcribe's level is peak. The UI may need a different scale (speech RMS is about 0.02 to 0.2).
- Auto-send (§7.4) can send the same `End` message from a VAD/Smart Turn stage. It should go through the capture thread too, for example by adding a fan-out of the resampled audio in `CaptureStage`.
- Optional follow-up: `audio_capture::MicrophoneSource` could be built on `InputStream`, removing its own channel handling.
