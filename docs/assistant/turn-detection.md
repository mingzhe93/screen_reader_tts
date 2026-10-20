# Assistant turn detection (`assistant_turn.rs`)

Status on 2026-10-19: implemented, tested on macOS and wired into automatic sending in the committed Assistant (`5e62732`). I have since tested it on Windows too (2026-10-20). See [conversation.md](conversation.md) for the frontend integration and experimental auto-send controls.

## What it does

`src-tauri/src/assistant_turn.rs` decides when the user has finished a spoken turn, so the Assistant can send it without a button press. It implements plan §7.4 (VAD + Smart Turn) and the §9.2 check "Smart Turn preprocessing and predictions match the upstream reference":

- **Silero VAD** labels every 32 ms frame as speech or not.
- After a pause that follows speech, **Smart Turn v3.2** (pipecat-ai, a Whisper-Tiny encoder with a classifier) judges from the last 8 s of the turn whether the utterance sounds complete. Silence alone never ends a turn early; a long pause still ends it (`max_pause_ms`).
- The Whisper log-mel preprocessing that Smart Turn needs is reimplemented in Rust and matches the upstream Python path to 4.5e-6 (see "Parity results").

The module is compiled only with the `build-base` feature. `lib.rs` declares `mod assistant_turn;` behind that feature. It uses the app's existing ONNX Runtime (`ort` with `load-dynamic`, loaded by `audio8_local::ensure_onnxruntime()`). The one new dependency is `realfft` 3 (already in `Cargo.lock` through `parakeet-rs`, now also a direct optional dependency in `build-base`).

## Interface (all `pub(crate)`)

```rust
enum TurnEvent {
    SpeechStarted,                   // the coordinator begins a turn
    EndOfTurn { probability: f32 },  // the coordinator ends the turn
}

struct TurnDetectorConfig {
    vad_threshold: f32,      // 0.5
    min_speech_ms: u32,      // 250
    pause_ms: u32,           // 300
    complete_threshold: f32, // 0.5
    max_pause_ms: u32,       // 2000
}  // impl Default with the values above

impl TurnDetector {
    fn new(smart_turn_onnx: &Path, vad_onnx: &Path, config: TurnDetectorConfig) -> Result<Self>;
    fn push(&mut self, samples: &[f32]) -> Result<Vec<TurnEvent>>; // 16 kHz mono, any length
    fn reset(&mut self);
}

fn smart_turn_features(samples: &[f32]) -> Vec<f32>; // 80 x 800, row-major by mel bin
const SAMPLE_RATE: usize = 16_000; const N_MELS: usize = 80; const N_FRAMES: usize = 800;
```

`new` returns an error if either model file is missing, if ONNX Runtime cannot be loaded, or if a threshold is outside 0..=1. `push` returns an error if an ONNX call fails; the samples it was given are consumed either way. `TurnDetector` is `Send` (both ONNX sessions are), so it can live on the capture or ASR worker thread. It is not `Sync` and needs no lock if one thread owns it.

`EndOfTurn.probability` is the Smart Turn answer that ended the turn, or, when `max_pause_ms` ended it, the last answer below the threshold (0 if Smart Turn was never asked).

Internally the VAD and the classifier sit behind two small private traits (`VoiceActivity`, `TurnClassifier`), so the state machine is tested with fakes.

## The pipeline

Audio is cut into 512-sample frames (32 ms); samples left over stay buffered until the next `push`.

**Silero VAD.** Each frame is sent as `input` [1, 576]: the last 64 samples of the previous input followed by the 512 new ones, as Silero's own Python wrapper (`OnnxWrapper`) does for 16 kHz. `state` [2, 1, 128] is carried from `stateN` of the previous call, and `sr` is the int64 scalar 16000. A frame is speech when `output` ≥ `vad_threshold`. One intra-op thread.

**State machine.**

| Phase | On a speech frame | On a silent frame |
|---|---|---|
| Idle | add 32 ms to the speech run; once it reaches `min_speech_ms`, emit `SpeechStarted` and go to Speech | reset the speech run |
| Speech | stay | go to Pause (pause = 32 ms) |
| Pause | back to Speech, no event (the pause timer restarts) | add 32 ms to the pause |

In Pause, Smart Turn is asked when the pause first reaches `pause_ms`, then every further 300 ms (`REASK_MS`). An answer ≥ `complete_threshold` emits `EndOfTurn`. When the pause reaches `max_pause_ms`, `EndOfTurn` is emitted anyway. After `EndOfTurn` the detector is Idle again with an empty turn buffer; the VAD keeps its recurrent state, because the audio stream continues.

Times are counted in whole frames, so they round up to a multiple of 32 ms: 250 ms is 8 frames (256 ms), 300 ms is 10 frames (320 ms), 2000 ms is 63 frames.

**Turn audio given to Smart Turn.** While Idle, only the last 500 ms (`PRE_SPEECH_MS`) plus the current speech run are kept, so the first syllable is included. From `SpeechStarted` on, everything is kept, capped at the last 8 s. This follows the upstream model card: give the model the user's current turn, not earlier turns, and re-run it on the whole turn when speech resumes.

**Smart Turn.** `smart_turn_features` (below) builds `input_features` [1, 80, 800]; the output `logits` [1, 1] is already a sigmoid probability. Two intra-op threads, one inter-op thread, full graph optimisation (upstream uses `ORT_SEQUENTIAL`, one inter-op thread and `ORT_ENABLE_ALL`).

## Exact preprocessing and its sources

Upstream (`pipecat-ai/smart-turn`, `inference.py` and `audio_utils.py`, copied into `scripts/smart_turn_reference.py`) calls `truncate_audio_to_last_n_seconds(audio, 8)` and then `WhisperFeatureExtractor(chunk_length=8)(audio, sampling_rate=16000, padding="max_length", max_length=128000, truncation=True, do_normalize=True)`. Each step below was confirmed in the installed `transformers` 4.57.6 source (`models/whisper/feature_extraction_whisper.py`, `audio_utils.py`) and numpy 2.0.2:

1. **Last 8 s, zero padding at the beginning** (`truncate_audio_to_last_n_seconds`): 128,000 samples, the audio at the end.
2. **Normalisation** (`zero_mean_unit_var_norm`): `(x - x.mean()) / np.sqrt(x.var() + 1e-7)`. `do_normalize=True` makes `pad()` return an attention mask, but the array is already 128,000 samples, so the mask is all ones and **the zero padding is included** in the mean and variance. The arithmetic is float32 in numpy; Rust sums in f64 and casts the mean and the scale to f32 before applying them. Pure silence gives variance 0 and stays 0. Note that quiet noise is scaled up to unit variance like speech.
3. **Centred STFT** (`spectrogram(center=True, pad_mode="reflect")`): `np.pad` reflect padding of 200 samples on each side (the edge sample is not repeated), then 801 frames of 400 samples every 160, promoted to float64.
4. **Window** (`window_function(400, "hann")`, periodic): `np.hanning(401)[:-1]`, using numpy's formula `0.5 + 0.5 cos(pi n / 400)` for `n = -400, -398, ...`.
5. **Power spectrum**: 400-point real FFT (201 bins). numpy stores the STFT as complex64 and squares the magnitude in float64; Rust computes in f64, rounds the real and imaginary parts to f32, then squares in f64.
6. **Mel filters** (`mel_filter_bank(201, 80, 0, 8000, 16000, norm="slaney", mel_scale="slaney")`): Slaney mel scale (linear `3f/200` below 1 kHz, logarithmic above with step `27/ln 6.4`), 82 points from `np.linspace` in mel converted back to Hz, triangles over the FFT bin frequencies `linspace(0, 8000, 201)`, multiplied by `2 / (f[j+2] - f[j])` (Slaney area normalisation). This is librosa's `mel(htk=False, norm="slaney")`.
7. **Log**: `log10(max(mel, 1e-10))`, cast to float32.
8. **Drop the last frame** (`log_spec[:, :-1]`): 800 frames.
9. **Dynamic range and scaling**: `max(x, x.max() - 8.0)` over the whole 80 x 800 tensor, then `(x + 4) / 4`, in float32.

`transformers` uses a torch STFT instead when torch is installed (`_torch_extract_fbank_features`). The reference venv has no torch, so the numpy path above is the reference. The torch path differs by about 1e-5 according to its own docstring.

## Parity results (macOS, Apple Silicon)

Reference: `scripts/smart_turn_reference.py` with `transformers` 4.57.6, numpy 2.0.2 and `onnxruntime` 1.19.2. Rust: the app's ONNX Runtime 1.24.4.

| Fixture | Length | Reference probability | Rust probability | Max abs feature diff |
|---|---|---|---|---|
| `question` ("Can you tell me what the weather is like tomorrow?") | 2.46 s | 0.79876 | 0.79876 | 4.5e-6 |
| `cut_off` ("I was thinking that maybe we could") | 1.90 s | 0.29598 | 0.29598 | 0 |
| `trailing_um` ("So I wanted to ask about, um") | 2.36 s | 0.00571 | 0.00571 | 0 |
| `silence` (exact zeros) | 1.00 s | 0.98704 | 0.98704 | 0 |
| `low_noise` (Gaussian, about -50 dBFS) | 1.50 s | 0.97771 | 0.97771 | 0 |
| `long` (a 9.4 s sentence; tests truncation) | 9.44 s | 0.67017 | 0.67017 | 4.5e-6 |

The required tolerances were 1e-3 for features and 0.02 for probabilities. The largest probability difference was 6e-8, and every complete/incomplete decision agrees. Silence and noise score as "complete", which is why the VAD gates Smart Turn: it is only asked after real speech.

The feature fixtures store, per clip, the per-frame mean, minimum and maximum of all 800 frames plus the full 80 values of 59 frames (0–2, every 15th from 5, and 797–799). So shape, frame count and the padding side are all covered without storing full tensors. A second test checks the padding side directly: leading zeros change nothing, trailing zeros do, and a clip over 8 s equals its last 8 s.

## Timings (M2 Max, macOS)

| Step | Release | Debug |
|---|---|---|
| Features only (`smart_turn_features`) | 1.8 ms | 139 ms |
| Smart Turn, features included, 1 thread | 38 ms | 199 ms |
| Smart Turn, features included, 2 threads (used) | 21 ms | 182 ms |
| Smart Turn, features included, 4 threads | 13 ms | 243 ms |
| Silero VAD, one 512-sample frame | 0.09 ms | 0.11 ms |

Smart Turn runs at most once every 300 ms and only during pauses; the VAD costs about 0.3% of one core. Debug builds (`npm run desktop:dev`) spend most of the time in the unoptimised FFT and mel loops; that is still well under the 300 ms re-ask interval. Extra memory was not measured (the two model files are 8.7 MB and 2.2 MB).

## Behaviour on the fixtures (false cut-offs)

Each fixture was played through the full detector with default settings: 0.5 s of silence, the clip, then 2.5 s of silence, in 100 ms pushes (times from the start of the audio):

| Fixture | Events | Verdict |
|---|---|---|
| `question` | SpeechStarted 0.8 s, EndOfTurn 3.2 s (p 0.976) | correct, about 0.3 s after the speech ends |
| `cut_off` | SpeechStarted 0.8 s, EndOfTurn 2.6 s (p 0.817) | **false cut-off** |
| `trailing_um` | SpeechStarted 0.8 s, EndOfTurn 4.8 s (p 0.006) | correct: kept listening, ended by `max_pause_ms` |
| `long` | SpeechStarted 0.8 s, EndOfTurn 10.1 s (p 0.890) | correct |
| `silence`, `low_noise` | none | correct: the VAD never triggers |

The `cut_off` result is the model's, not the port's: Rust and Python agree exactly. The clip is macOS text-to-speech, which reads the fragment with sentence-final intonation, and the model's answer depends strongly on how much pause follows it. Python reference on `cut_off.wav` plus trailing zeros: 0 ms 0.296, 100 ms 0.644, 200 ms 0.539, 300 ms 0.914, 500 ms 0.687, 1000 ms 0.280. On `trailing_um` the answer stays at 0.006 and on `question` at about 0.97 for every pause length. Synthetic voices are a weak test of a prosody model; judge it with real speech (manual checks below).

## Defaults and tuning

- `vad_threshold` 0.5: Silero's usual value. Raise it (0.6–0.7) if background noise or the assistant's own voice starts turns; lower it for quiet microphones.
- `min_speech_ms` 250: coughs, clicks and short noises are ignored. Higher values delay `SpeechStarted`, not the end of a turn.
- `pause_ms` 300: the first Smart Turn question. Shorter answers sooner but sees less pause, and the `cut_off` measurement shows the model's answer is unstable over the first 500 ms of pause.
- `complete_threshold` 0.5: upstream's decision point. Raise it (0.7–0.8) to reduce false cut-offs at the cost of more turns ending through `max_pause_ms`.
- `max_pause_ms` 2000: the longest wait after an unfinished-sounding pause. Lower makes hesitant speech end sooner.
- Not configurable: the 300 ms re-ask interval, the 500 ms pre-roll and the 8 s window (the model's input size).

A single speech frame during a pause cancels the pending end. If clicks or noise keep resetting pauses in practice, require two or three speech frames there.

## Model files to add to the catalogue

Both are pinned by revision and SHA-256 and must be verified with `download_verified_file` like the other Assistant downloads. The local copies used here were named by the coordinator; **the file paths inside the repositories below are assumptions** (not checked offline). The hashes will catch a wrong file.

| File | Repository and revision | Assumed path | Size | SHA-256 | Licence |
|---|---|---|---|---|---|
| Smart Turn v3.2, CPU int8 | `pipecat-ai/smart-turn-v3` @ `f766f81d3cfdf7737ac64aad813d91bbfd56bf93` | `smart-turn-v3.2-cpu.onnx` | 8,679,182 | `2bb026316b14a660486a75b1733cd3fbab8c2fd0314dc9af7be49f8cca967e4f` | BSD-2-Clause |
| Silero VAD | `onnx-community/silero-vad` @ `e71cae966052b992a7eca6b17738916ce0eca4ec` | `onnx/model.onnx` | 2,243,022 | `a4a068cd6cf1ea8355b84327595838ca748ec29a25bc91fc82e6c299ccdc5808` | MIT |

URL form: `https://huggingface.co/<repo>/resolve/<revision>/<path>`. Smart Turn's I/O: `input_features` [batch, 80, 800] float to `logits` [batch, 1]. Silero's: `input` [1, N] float, `state` [2, 1, 128] float and `sr` int64 scalar to `output` [1, 1] and `stateN`.

## Integration (completed in `5e62732`; plan §7.1, §7.2)

1. When auto-send is on, create one `TurnDetector` with the two model paths and `TurnDetectorConfig::default()` (or the user's settings). Load it once when the Assistant session starts, not per turn (load time was not measured).
2. Feed it the same 16 kHz mono blocks the capture fan-out sends to ASR, on the capture thread, in recording order.
3. `SpeechStarted`: begin a turn exactly as a push-to-talk press does.
4. `EndOfTurn`: end the turn exactly as a release does. The capture stage writes `EndTurn` into the ASR stream after every block captured so far (Codex F6), so producing the event on the capture thread keeps the order right.
5. Call `reset()` when the assistant starts speaking (so its voice and the pause before it do not count), and after a turn is sent if any audio was skipped. Feeding can resume when the microphone is open again.

The plan's other auto-send measurements (end-of-speech to final transcript latency, comparison with push-to-talk, memory) need this connection and are not done.

## Tests

Use the commands in [README.md](README.md) for the toolchain. From `src-tauri`:

```bash
cargo test --features build-base assistant_turn::
```

runs 11 tests that need no models: feature parity against the fixtures, the padding side, and eight state-machine tests (speech starts only after `min_speech_ms`, partial frames, no end before `pause_ms`, incomplete then re-ask then complete, the `max_pause_ms` fallback, speech resuming, `reset`, the 8 s cap and pre-roll, threshold validation).

The model tests are `#[ignore]` and need `VOICEREADER_SMART_TURN_DIR` pointing at a folder with both ONNX files under the names `smart-turn-v3.2-cpu.onnx` and `silero-vad.onnx`, plus the bundled ONNX Runtime (`npm run onnxruntime:fetch`, or `VOICEREADER_ONNXRUNTIME_PATH`):

```bash
VOICEREADER_SMART_TURN_DIR=/path/to/models cargo test --features build-base assistant_turn:: -- --ignored --nocapture --test-threads=1
# add --release for release timings
```

`probabilities_match_upstream_reference` asserts the 0.02 tolerance and equal decisions; `measure_timings_and_fixture_behaviour` prints the tables above.

On Windows (PowerShell): `$env:VOICEREADER_SMART_TURN_DIR = "C:\path\to\models"`, then the same `cargo test` line.

### Fixtures and the reference generator

`src-tauri/tests-data/smart_turn/` (780 KB) holds six 16-bit 16 kHz mono WAVs, one `.f32` summary per WAV and `reference.json` (probabilities, library versions, the `.f32` layout). The speech clips were made with the macOS Samantha voice:

```bash
say -v Samantha -o question.aiff "Can you tell me what the weather is like tomorrow?"
say -v Samantha -o cut_off.aiff "I was thinking that maybe we could"
say -v Samantha -o trailing_um.aiff "So I wanted to ask about, um"
say -v Samantha -o long.aiff "Yesterday I walked to the market near the river, bought some apples and fresh bread, talked with a friend about the weekend, and then I came home to cook dinner for the whole family."
afconvert -f WAVE -d LEI16@16000 -c 1 question.aiff question.wav   # and so on for each
```

`silence.wav` is 16,000 zero samples, and `low_noise.wav` is `numpy.random.default_rng(20261018).standard_normal(24000) * 0.003`, both written as 16-bit PCM with `soundfile`. (`say` silence is not digital zero, and normalisation would scale it to unit variance.) Regenerate the reference with a Python environment that has `numpy`, `soundfile`, `onnxruntime` and `transformers` (no torch, to stay on the numpy path):

```bash
python -I scripts/smart_turn_reference.py --model /path/to/smart-turn-v3.2-cpu.onnx \
    --out src-tauri/tests-data/smart_turn src-tauri/tests-data/smart_turn/*.wav
```

WAV samples are converted as `int16 / 32768` on both sides.

## Manual checks (Windows and macOS)

These are manual microphone checks through the wired automatic-send path, or a small harness that feeds microphone audio to `push` and prints the events.

1. **Windows build and parity.** Build with `--features build-base` and run the ignored tests above. Expected: all pass, with probability differences within the 0.02 tolerance (the macOS result was 6e-8), and timings printed.
2. **Real speech, complete question.** Ask a short question and stop. Expected: `EndOfTurn` about 0.3–0.6 s after you stop.
3. **Mid-sentence pause.** Say "I was thinking that maybe we could", pause 1 s, then finish the sentence. Expected: no `EndOfTurn` during the pause (the `cut_off` fixture failed this with a synthetic voice; record the probability printed for a real voice). If it cuts off often, try `complete_threshold` 0.7.
4. **Hesitation.** "So I wanted to ask about, um" and stop. Expected: no early end; `EndOfTurn` after about 2 s (`max_pause_ms`).
5. **Silence and background noise.** Leave the microphone open for 30 s in a quiet room, then with a fan or music. Expected: no `SpeechStarted` in the quiet room; with noise, note whether it triggers and at which `vad_threshold` it stops.
6. **Speech resuming.** Pause about 0.4 s and continue several times within a sentence. Expected: one turn, ended only after the final pause.
7. **Assistant voice.** With speakers, not headphones, let the assistant answer. Expected: `reset()` is called when it starts speaking; note whether its voice triggers `SpeechStarted` (echo is not handled here).
8. **CPU load.** With Task Manager (or Activity Monitor) open, talk for a minute. Expected: the detector adds only a few percent of one core.

## Known gaps and next steps

- Connected to the capture fan-out and ASR worker in the committed Assistant.
- The Silero 64-sample context follows the silero-vad Python wrapper as remembered, not re-read offline. Without it the model also runs (on 512 samples) but gives different probabilities.
- No echo handling: the detector must be reset or paused while the assistant speaks.
- Silence and noise score as complete; the design relies on the VAD gate. Smart Turn is only trained on 16 kHz input; other rates must be resampled first (the capture path already delivers 16 kHz).
- Only synthetic English speech was tested. The model supports 23 languages; the ASR is English-only.
- Extra memory and the latency from end of speech to final transcript are not measured.
