# VoiceReader TTS Pipeline Learnings

This file captures implementation-level lessons from the Rust (`build-base`) and
Python sidecar (`build-full`) playback pipelines. Sections 1 to 6 cover the shared
playback pipeline. Sections 7 to 11 cover Audio8, Kyutai voices, buffering, chunking
and GPU use, with measurements.

Where the code lives: `kyutai_local.rs` and `audio8_local.rs` are the Base runtimes.
`audio_pipeline.rs` holds the SoX tempo stream and the rate-controlled emitter that both
runtimes share. Sidecar code is under `tts-engine/src/tts_engine/`.

## 1. SoX starvation with tiny streaming tokens

When `rate != 1.0`, feeding SoX with very small token-sized buffers causes
starvation. The frontend drains output faster than SoX can produce consistent
frames from tiny inputs, creating audible discontinuities.

Key lesson:
- For rate-adjusted playback, batch-oriented chunk processing is more stable
  than token-by-token feeding.

How the current code applies it:
- Kyutai (Base) still generates a whole chunk with `model.generate()` and feeds SoX
  that PCM in one piece.
- Audio8 (Base) streams: it feeds SoX one decoded window at a time, runs SoX with a
  small `--buffer` and waits for SoX's output to settle after each piece
  (section 7.3.1).
- The Python sidecar processes one whole chunk per SoX call.

## 2. Parallel look-ahead reduces inter-chunk gaps

Rust base runtime, Kyutai:
- Up to `min(cores - 1, 4)` upcoming chunks are generated on background threads
  (`model.generate`), and their PCM is released in order.
- The calling thread pushes each finished chunk through SoX and emits it.

The next chunks are pre-generated while the current one is still being pushed through
SoX and emitted. This removes most dead-air between chunks on CPU workloads.

Rust base runtime, Audio8 uses a different layout (two chunks generated at once and a
single decode loop); see section 7.3.

Python sidecar loop uses a similar pattern:
- pre-submit chunk `N+1` synthesis before waiting on SoX processing for chunk
  `N`
- run playback control DSP in worker threads to keep the event loop responsive

## 3. Live rate control architecture

### 3.1 Base build (Rust)
- The active rate is shared through an `AtomicU32` (`active_rate_steps`) in steps of
  `0.25x`, from 1 (0.25x) to 16 (4.0x).
- Both runtimes send PCM through `RateEmitter` in `audio_pipeline.rs`. It reads the
  desired rate once for every piece of audio it is given.
- When the rate has changed, it flushes the current SoX stream and opens a new one for
  the new rate. If SoX is not available it resamples instead (see section 4).
- How often the rate is read therefore depends on the piece size: for Audio8 a piece
  is a decoded window, so a rate change lands within a few hundred milliseconds. For
  Kyutai a piece is a whole generated chunk, so the new rate applies from the next
  chunk. An earlier version polled every 960 samples inside a chunk
  (`RATE_CONTROL_POLL_SAMPLES`); that constant no longer exists.
- Audio the frontend has already queued still plays at the old rate.
- Volume is not live: it is applied as PCM gain when the audio is generated and stays
  fixed for the job.

### 3.2 Full build (Python sidecar)
- `POST /v1/jobs/{job_id}/playback` mutates active job playback settings. The app
  sends only `rate`.
- Updated values are consumed in the job loop during chunk processing.
- Effective audio change is chunk-cycle granularity.

## 4. Pitch-preserving rate control fallback chain

Sidecar fallback order (`tts-engine/src/tts_engine/jobs.py`):
1. SoX tempo
2. librosa time-stretch
3. linear resample

Base build (`audio_pipeline.rs`) has only two steps: SoX tempo, then linear resample.
SoX is looked up through `VOICEREADER_SOX_PATH`, then next to the app, then `PATH`, then
the Windows winget packages folder.

Implication:
- pitch preservation depends on SoX (and librosa in the sidecar)
- fallback linear resampling changes pitch

## 5. Syncing UI state with backend playback state

`voicereader:rate-updated` is emitted by `set_speak_settings` and `cycle_speak_rate`
after the rate changes, so both the main UI and floating toolbar stay consistent with
the effective backend rate value.

Without this event, UI elements drift and users see stale controls.

## 6. Window lifecycle coupling matters for packaging

A floating toolbar as a separate window can keep the process alive after main
window close if not explicitly closed.

Current fix (`handle_run_event` in `voicereader_core.rs`):
- on main-window close request or destroy, close the toolbar window and exit the app

This prevents stale process handles that can block portable packaging cleanup on
Windows.

## 7. Audio8 TTS 0.1B (ONNX, base build)

Measured on a Ryzen 9 9950X (16 cores) with `Edge0/audio8-TTS-0.1B-ONNX-INT8`.
One codec frame is 2048 samples at 44.1 kHz, about 46 ms of audio.

### 7.1 Where the time goes
- Slow AR step: about 12 to 14 ms per token, single-threaded. More threads do not
  help, and the cost grows slowly with sequence position.
- Fast AR: ten calls per frame, about 0.5 to 0.7 ms each.
- Together about 25 ms per frame, so generation alone runs at about 0.55x real time.
- Codec decode: about 14 to 15 ms per frame with 8 threads (0.3x real time). It does
  not scale past 8 threads.
- The voice prompt (reference transcript plus reference codes) costs about 2.3 s of
  prefill. It is identical for every chunk spoken with a voice, so the slow-AR state
  after it is snapshotted per voice (`VoicePrefix`) and restored for each chunk.
  Without that, every chunk would start about 2.3 s late.

### 7.2 The codec decoder is causal but needs left context
- Decoding the first N frames gives the same samples as a full decode (80 dB SNR or
  better), so a chunk's first decode window can start at frame 0 and be exact.
- Decoding from the middle with limited left context gives a different waveform
  (about 1 to 3 dB SNR) but nearly the same spectrum (about 0.5 to 0.8 dB mean
  log-spectral difference, against 3.2 dB for unrelated audio). Window boundaries do
  not click, and a short crossfade over the held-back guard frame hides the rest.
- Context of 8 frames measures the same as 16; only 32 or more is noticeably closer.

### 7.3 Streaming design
- Cumulative decoding (always from frame 0) cannot keep up with playback: the
  decoder would redo all earlier frames on every pass.
- `Audio8Model::synthesize_chunks` therefore runs one decode loop on the calling
  thread. It serves the chunk being played first, in small windows with 8 frames of
  left context, and uses idle time to decode chunks that are generated ahead.
- Two chunks are generated concurrently. Three or more made latency worse on a
  16-core CPU, because the single decoder is the bottleneck.
- The first chunks are kept short (80 and 120 units, then the user's chunk size up to
  160; see section 10.3) so the next chunk is ready when the first finishes playing.
- Result for a 40 s passage, model only: about 0.75 s to first audio and no underruns
  at 1.0x; occasional short underruns (0.1 to 0.4 s) at 1.5x. Overall the pipeline
  runs at about 0.5x real time, so about 2x is the ceiling on this CPU. Slower CPUs
  will not reach real time.
- Through the app runtime (SoX included): first audio after about 0.8 s at 1.0x and
  about 1.4 s at 1.5x.
- Peak memory is about 1.5 GB while speaking.

### 7.3.1 Why 1.5x starts later than 1.0x
- The frontend holds playback until a prebuffer is filled, and the prebuffer grows
  with the rate (`minPrebufferSeconds` in `src/playback.ts`: 0.24 s at 1.0x, 0.465 s
  at 1.5x). The first piece has to cover it, or playback starts with the second piece.
- SoX holds back 0.16 to 0.25 s of output while time-stretching, and with its default
  buffer it returns audio in 8192-sample blocks. The emitter runs SoX with
  `--buffer 4096` and waits briefly after each piece for the output to settle, so a
  piece's audio is sent with that piece and not with the next one. (This was first
  done in the Audio8 path; it now lives in `audio_pipeline.rs` and both runtimes use it.)
- `first_frames_for_rate` in `audio8_local.rs` sizes the first piece from both.

### 7.4 ONNX Runtime packaging
- Static linking fails: ONNX Runtime bundles protobuf, and so does `sentencepiece-sys`
  (pulled in by `pocket-tts`), which gives duplicate symbols at link time (LNK2005).
- The `ort` crate is therefore built with `load-dynamic`, and the shared library is
  fetched by `scripts/fetch-onnxruntime.js` into `src-tauri/binaries/onnxruntime`.
- The library is loaded by absolute path. Windows ships an old `onnxruntime.dll` in
  System32, so falling back to the system search path would load the wrong version.
- Output differs between ONNX Runtime versions: 1.23 and 1.30 pick different tokens
  for the same input with greedy sampling, because the INT8 kernels round
  differently. The Rust port matches the Python reference when both use the same
  version. The pinned version (1.23.2) is inside the range the reference runtime
  supports. (Note, 2026-10-04: `scripts/fetch-onnxruntime.js` now fetches 1.24.4, and
  1.23.2 only for Intel macOS.)

### 7.5 Voice cloning
- The model conditions on the reference transcript as well as the reference audio,
  so a clone without an exact transcript is not possible.
- Codes from the Rust encoder path match the Python reference (1599 of 1600 codes
  for a 7.4 s clip).
- Reference clips are stored as codes (`audio8_codes.npy`) next to the voice
  metadata. A voice cloned while Kyutai was active only has the 24 kHz normalized
  clip; it is encoded on first use with Audio8 if it has a transcript.
- Longer reference clips make every chunk slower, because the prompt is longer.
  5 to 10 seconds is a good length.

### 7.6 More threads do not make Audio8 faster
- Default layout: 2 chunks generated at once (1 thread each) plus 1 codec decoder with
  up to 8 threads, so about 10 busy threads on a 32-thread CPU.
- Tried at 2x playback on the 9950X: 2 to 6 parallel chunks, 1 to 4 decoders, 6 to 30
  decoder threads. Every layout landed at 0.51 to 0.61x real time, the same as the
  default within run-to-run noise, while memory rose from about 1.9 GB to 3 to 5.8 GB.
- Likely cause: each generated token reads the whole 133 MB slow-AR weight file plus
  about 50 MB of attention cache, and each decode pass reads about 500 MB of decoder
  weights. None of that fits in cache, so the streams compete for memory bandwidth
  and extra cores sit idle. This is an inference from the measurements, not a profile.
- The layout is still overridable for other machines with
  `VOICEREADER_AUDIO8_PARALLEL_CHUNKS` (1 to 8), `VOICEREADER_AUDIO8_DECODERS` (1 to 4)
  and `VOICEREADER_AUDIO8_DECODER_THREADS`. The active values show under Engine Health.
- ONNX Runtime 1.26 and 1.30 were not faster than 1.23.2 in a quick comparison.
- Practical ceiling stays near 1.5x playback on this CPU. Other load on the machine
  (a game was running during one test session) lowers it further.

## 8. Kyutai Pocket TTS preset voices (checked 2026-10-03)

- Upstream (`kyutai/pocket-tts`) now lists 26 voices and 7 languages, with newer
  model versions (`languages/English_2026-09` and others). That repo is gated, and
  its new voices ship as `embeddings_v3` states tied to the newer weights.
- The Rust `pocket-tts` crate (0.2.8 in use, 0.6.2 latest) still targets the January
  2026 weights (`b6369a24`) and lists the same eight predefined voices that are
  bundled. So the eight built-in presets are everything the bundled model has as
  precomputed embeddings.
- The source clips for upstream's newer English voices are public in
  `kyutai/tts-voices` (VCTK: CC BY 4.0, voice-zero: CC0). Thirteen of them are now
  presets too: the clip is cloned with the bundled weights on first use (about 1.5 to
  2 s once per session) and cached. `scripts/fetch-kyutai-voices.js` downloads them.
- Upstream's non-English voices (giovanni, lola, juergen, rafael, estelle) are left
  out because the bundled weights are English-only.
- Preset descriptions were checked by measuring the pitch of synthesized speech:
  alba, marius, javert, jean, charles, paul, george, michael, bill_boerst,
  peter_yearsley and stuart_bell are in the male range (78 to 135 Hz); fantine,
  cosette, eponine, azelma, anna, vera, mary, jane, eve and caro_davy are in the
  female range (150 to 220 Hz).

## 9. Refill the buffer when playback runs dry

- The frontend used to schedule every late chunk the moment it arrived. When a model
  produced audio slower than it was played (Audio8 above about 1.5x, or any model on
  a busy machine), that gave a short gap before almost every chunk: constant stutter.
- `flushQueuedPlayback` in `main.ts` now notices that everything scheduled has already
  finished playing when a new chunk arrives, holds playback, and resumes once enough
  audio is queued: 1 s after the first time a job runs dry, 2 s after later ones.
  The end of the job always flushes whatever is queued.
- Simulated with a producer at 75% of playback speed and 0.3 s pieces: about 95 gaps
  of 0.1 s become a handful of pauses of 1.5 to 3 s. Total waiting time is about the
  same, since the model is no faster; it is just grouped.
- A model that keeps up is unaffected, and so are sentence-sized bursts (Kyutai): one
  burst already exceeds the refill target, so playback resumes immediately.
- Each refill is logged in the Activity panel as `playback_rebuffer`.

## 10. Text chunking (shared by Kyutai and Audio8)

Both base-build runtimes now use `text_chunking.rs` instead of their own splitters.

### 10.1 What was wrong before
- Kyutai used the `pocket-tts` crate's `split_into_best_sentences`, which cuts at every
  `.`, `:`, `;`, `!` and `?` and re-joins the pieces with spaces. Spoken text was
  altered: "$3.50" became "$3. 50", "10:30" became "10: 30", "e.g." became "e. g.",
  "example.com" became "example. com" and "..." became ". . .".
- Sentences over 50 tokens were cut every 35 words, wherever that fell.
- Line breaks were turned into spaces by both paths, so a heading ran straight into
  the next sentence, list items ran together, and words hyphenated across lines
  ("con-" / "vert") stayed broken.
- Audio8's first splitter cut at any full stop followed by a space ("Jan." | "5") and
  used a 45-unit first chunk that split most opening sentences.

### 10.2 What the shared chunker does
- `normalize_for_speech` repairs line breaks. A break is layout (re-join) when the
  line ends in a hyphenated word, the next line starts in lower case, the line is
  nearly as long as the longest line of its paragraph, or it ends in a word such as
  "the" or "of". Otherwise it is structure: a short line before a capitalized one is
  a heading, and bullet or numbered lines are list items. Those get a full stop.
- `chunk_text` ranks possible cuts: sentence end, then semicolon or colon, then comma
  or dash, then just before a connecting word, then any space.
- A full stop does not end a sentence inside a word ("3.14", "example.com"), after
  a listed abbreviation or capitalized month ("Dr.", "Jan."), after initials
  ("J. K.", "U.S."), after a list number at the start of a sentence, or when the next
  word is lower case (`"What?" she asked.`).
- A sentence that does not fit is divided into the fewest pieces that do, of similar
  length, at the strongest boundary near the ideal length.
- Sizes are in "units": one per character, 3.5 per CJK character, because CJK text
  takes about that much longer to speak.

### 10.3 Model-specific limits
- Kyutai: first chunk 100 characters, later chunks up to 200, and never more than 50
  tokens (the model is trained on short prompts). A chunk over the token limit is
  halved again, which mostly happens with digit-heavy or non-English text.
- Audio8: first chunk 80 units, second 120, then up to 160. A shorter first chunk
  saves about 0.4 s before the first audio, because there are fewer prompt tokens to
  process. Going below about 80 gained nothing and split more opening sentences.

### 10.4 Known gaps
- The heading heuristic can misfire on a short wrapped line that happens to precede
  a capitalized word, which adds a full stop mid-sentence.
- "The U.S. Then..." is not split after "U.S."; the two sentences are spoken as one.
- Text is still read as written: URLs, markdown symbols, citation markers like "[12]"
  and emoji are not cleaned up or expanded.
- A chunk cut mid-sentence is still generated on its own, so intonation can dip at
  the cut. Kyutai also appends a full stop to such a piece.

## 11. GPU acceleration for Audio8 (and later ASR)

Measured on the 9950X with an RTX 5090 and the CPU's integrated Radeon graphics,
through ONNX Runtime's DirectML provider.

### 11.1 What is worth putting on a GPU
| Graph | CPU | RTX 5090 | Integrated Radeon |
|---|---|---|---|
| Codec decoder, per frame | 12 to 17 ms (8 threads) | about 0.5 ms | about 55 ms |
| Slow AR step | 13.5 ms (1 thread) | 28.5 ms | 167 ms |
| Fast AR step | 0.8 ms | 1.1 ms | 64 ms |

- The decoder is a large feed-forward network called a few times per chunk: about 25
  to 30 times faster on the discrete GPU.
- The AR graphs are called once per token with small inputs and INT8 weights. They
  are slower on any GPU, so they stay on the CPU.
- The integrated GPU in this desktop CPU is slower than the CPU for everything. A
  GPU being present is not a reason to use it; it has to be measured.

### 11.2 How the app decides
- On Windows the decoder is benchmarked once on the GPU DirectML ranks as highest
  performance and once on the CPU (24 frames each). The GPU is used only if it is at
  least 1.5 times faster. The result is cached in `audio8-decoder-device.json` in the
  data directory for 7 days; deleting the file forces a new benchmark.
- If a GPU session later fails to load, the model loads on the CPU and the cache is
  updated, so speech keeps working.
- The Compute Device setting on the Engine tab (Auto, GPU, CPU) is saved in the app
  settings file. Auto is the benchmark-based choice above. GPU skips the benchmark and
  uses the GPU provider if it loads. CPU never touches the GPU. Changing it reloads
  a loaded Audio8 model, which stops anything being spoken.
- `VOICEREADER_AUDIO8_DECODER_DEVICE=auto|gpu|cpu` overrides the setting for testing.
  The engine badge and Engine Health show the device in use.

### 11.3 What it changes
- A 40 s passage: the whole pipeline went from about 0.53x to about 0.38x to 0.45x of
  real time, and 2x playback ran with no gaps, or a few short ones, in the
  simulation. 3x is still out of reach, because generation (CPU) is now the limit.
- With decoding nearly free, every window is decoded from the start of its chunk,
  which is exact. The CPU path's limited-context approximation (section 7.2) is not
  used on a GPU.
- Process memory while speaking dropped from about 1.5 to 1.9 GB to about 1.1 GB.
- The first GPU decode after loading takes over a second (pipeline setup), so one
  warm-up decode runs at model load.

### 11.4 Packaging
- DirectML covers every DirectX 12 GPU on Windows (NVIDIA, AMD, Intel) with one
  provider and no vendor SDK. It needs the DirectML build of ONNX Runtime and a
  matching `DirectML.dll` (1.15.4); together about 36 MB against 14 MB for CPU only.
- Windows ships an older `DirectML.dll` in System32, so the bundled one is loaded by
  full path before ONNX Runtime.
- The same library also contains the CPU provider, so one file serves both.

### 11.5 Not done or not verified
- macOS: the Core ML provider is wired in but untested (no Mac available), so it is
  opt-in. Whether Core ML accepts this decoder, with its variable-length input, is
  unknown.
- Laptop integrated GPUs (Intel Iris Xe or Arc, Radeon 780M) are far stronger than
  the one measured here, and laptop CPUs are weaker, so the outcome there may differ.
  The benchmark handles either case.
- Linux GPUs are not covered. ONNX Runtime also has a native WebGPU provider that
  would cover Windows, macOS and Linux with one code path; official prebuilt
  libraries for it were not found, so it was not tried.
- WebGPU inside the app window works at the engine level on Windows (the Edge 154
  engine found the RTX 5090), but it would mean a second inference stack in
  JavaScript, so the native route was taken.
