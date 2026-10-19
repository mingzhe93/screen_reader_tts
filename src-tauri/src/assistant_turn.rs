//! Assistant auto-send: decides when the user has finished speaking (plan §7.4).
//!
//! Silero VAD runs on every 32 ms of microphone audio. After a pause that follows speech,
//! Smart Turn v3 (a Whisper-Tiny classifier from pipecat-ai) judges from the last 8 s of the
//! turn whether the utterance sounds complete. `smart_turn_features` reproduces the
//! upstream preprocessing (`WhisperFeatureExtractor(chunk_length=8)` with `do_normalize`)
//! exactly; see docs/assistant/turn-detection.md for the derivation and parity results.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use anyhow::{anyhow, bail, Context, Result};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::TensorRef;
use realfft::{RealFftPlanner, RealToComplex};

use crate::audio8_local::ensure_onnxruntime;

pub(crate) const SAMPLE_RATE: usize = 16_000;
/// Silero VAD's chunk at 16 kHz.
const VAD_FRAME: usize = 512;
/// Samples of the previous chunk that Silero's own wrapper prepends to each chunk.
const VAD_CONTEXT: usize = 64;
const VAD_STATE_LEN: usize = 2 * 128;
const N_FFT: usize = 400;
const HOP: usize = 160;
const N_BINS: usize = N_FFT / 2 + 1;
pub(crate) const N_MELS: usize = 80;
pub(crate) const N_FRAMES: usize = 800;
/// Smart Turn sees at most the last 8 s.
const WINDOW_SAMPLES: usize = 8 * SAMPLE_RATE;
/// Audio kept from before speech starts, so the first syllable is in the turn.
const PRE_SPEECH_MS: u32 = 500;
/// Smart Turn is asked again this often while a pause continues.
const REASK_MS: u32 = 300;
/// Smart Turn inference threads. Measured on Apple Silicon (release, features included):
/// 38 ms with 1 thread, 21 ms with 2, 13 ms with 4. Two is well inside the 300 ms
/// re-ask interval and leaves cores to the speech recogniser and the LLM.
const SMART_TURN_THREADS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum TurnEvent {
    /// VAD heard at least `min_speech_ms` of speech: the coordinator begins a turn.
    SpeechStarted,
    /// A pause after speech was judged complete (or lasted `max_pause_ms`): the
    /// coordinator ends the turn. `probability` is the last Smart Turn answer, 0 if none.
    EndOfTurn { probability: f32 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TurnDetectorConfig {
    pub vad_threshold: f32,
    pub min_speech_ms: u32,
    pub pause_ms: u32,
    pub complete_threshold: f32,
    pub max_pause_ms: u32,
}

impl Default for TurnDetectorConfig {
    fn default() -> Self {
        Self { vad_threshold: 0.5, min_speech_ms: 250, pause_ms: 300, complete_threshold: 0.5, max_pause_ms: 2000 }
    }
}

/// Speech probability for one 512-sample frame (Silero VAD, or a fake in tests).
trait VoiceActivity: Send {
    fn speech_probability(&mut self, frame: &[f32]) -> Result<f32>;
    fn reset(&mut self);
}

/// Probability that the turn in `audio` (16 kHz, any length) is complete.
trait TurnClassifier: Send {
    fn complete_probability(&mut self, audio: &[f32]) -> Result<f32>;
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    Speech,
    Pause,
}

pub(crate) struct TurnDetector {
    vad: Box<dyn VoiceActivity>,
    classifier: Box<dyn TurnClassifier>,
    config: TurnDetectorConfig,
    /// Samples not yet making up a whole VAD frame.
    pending: Vec<f32>,
    /// The current turn plus a short pre-roll, at most 8 s.
    audio: VecDeque<f32>,
    phase: Phase,
    /// Consecutive speech samples while idle.
    speech_run: usize,
    /// Silence samples since the pause began.
    pause: usize,
    /// Pause length at which Smart Turn is asked next.
    next_ask: usize,
    last_probability: Option<f32>,
}

impl TurnDetector {
    pub(crate) fn new(smart_turn_onnx: &Path, vad_onnx: &Path, config: TurnDetectorConfig) -> Result<Self> {
        validate(&config)?;
        ensure_onnxruntime()?;
        let vad = SileroVad::load(vad_onnx)?;
        let classifier = SmartTurn::load(smart_turn_onnx, SMART_TURN_THREADS)?;
        Ok(Self::from_parts(Box::new(vad), Box::new(classifier), config))
    }

    fn from_parts(vad: Box<dyn VoiceActivity>, classifier: Box<dyn TurnClassifier>, config: TurnDetectorConfig) -> Self {
        Self {
            vad,
            classifier,
            config,
            pending: Vec::with_capacity(VAD_FRAME * 4),
            audio: VecDeque::with_capacity(WINDOW_SAMPLES),
            phase: Phase::Idle,
            speech_run: 0,
            pause: 0,
            next_ask: 0,
            last_probability: None,
        }
    }

    /// Feeds 16 kHz mono samples (any length). Returns events in order.
    pub(crate) fn push(&mut self, samples: &[f32]) -> Result<Vec<TurnEvent>> {
        self.pending.extend_from_slice(samples);
        let mut events = Vec::new();
        let mut frame = [0.0f32; VAD_FRAME];
        while self.pending.len() >= VAD_FRAME {
            frame.copy_from_slice(&self.pending[..VAD_FRAME]);
            self.pending.drain(..VAD_FRAME);
            self.step(&frame, &mut events)?;
        }
        Ok(events)
    }

    /// Forgets everything (after a turn is sent, or when the assistant starts speaking).
    pub(crate) fn reset(&mut self) {
        self.vad.reset();
        self.pending.clear();
        self.audio.clear();
        self.phase = Phase::Idle;
        self.speech_run = 0;
        self.pause = 0;
        self.next_ask = 0;
        self.last_probability = None;
    }

    fn step(&mut self, frame: &[f32], events: &mut Vec<TurnEvent>) -> Result<()> {
        let is_speech = self.vad.speech_probability(frame)? >= self.config.vad_threshold;
        self.audio.extend(frame);
        let excess = self.audio.len().saturating_sub(WINDOW_SAMPLES);
        self.audio.drain(..excess);

        match self.phase {
            Phase::Idle => {
                if is_speech {
                    self.speech_run += frame.len();
                    if self.speech_run >= ms_to_samples(self.config.min_speech_ms) {
                        self.phase = Phase::Speech;
                        events.push(TurnEvent::SpeechStarted);
                    }
                } else {
                    self.speech_run = 0;
                }
                if self.phase == Phase::Idle {
                    let keep = ms_to_samples(PRE_SPEECH_MS) + self.speech_run;
                    let excess = self.audio.len().saturating_sub(keep);
                    self.audio.drain(..excess);
                }
            }
            Phase::Speech => {
                if !is_speech {
                    self.phase = Phase::Pause;
                    self.pause = frame.len();
                    self.next_ask = ms_to_samples(self.config.pause_ms);
                    self.last_probability = None;
                    self.check_pause(events)?;
                }
            }
            Phase::Pause => {
                if is_speech {
                    self.phase = Phase::Speech;
                    self.pause = 0;
                } else {
                    self.pause += frame.len();
                    self.check_pause(events)?;
                }
            }
        }
        Ok(())
    }

    fn check_pause(&mut self, events: &mut Vec<TurnEvent>) -> Result<()> {
        if self.pause >= self.next_ask {
            let probability = self.classifier.complete_probability(self.audio.make_contiguous())?;
            self.last_probability = Some(probability);
            if probability >= self.config.complete_threshold {
                self.end_turn(probability, events);
                return Ok(());
            }
            self.next_ask += ms_to_samples(REASK_MS);
        }
        if self.pause >= ms_to_samples(self.config.max_pause_ms) {
            self.end_turn(self.last_probability.unwrap_or(0.0), events);
        }
        Ok(())
    }

    /// Back to idle with an empty turn. The VAD keeps its state: the stream continues.
    fn end_turn(&mut self, probability: f32, events: &mut Vec<TurnEvent>) {
        events.push(TurnEvent::EndOfTurn { probability });
        self.phase = Phase::Idle;
        self.audio.clear();
        self.speech_run = 0;
        self.pause = 0;
    }
}

fn validate(config: &TurnDetectorConfig) -> Result<()> {
    for (name, value) in [("vad_threshold", config.vad_threshold), ("complete_threshold", config.complete_threshold)] {
        if !(0.0..=1.0).contains(&value) {
            bail!("{name} must be between 0 and 1, got {value}");
        }
    }
    Ok(())
}

fn ms_to_samples(ms: u32) -> usize {
    ms as usize * SAMPLE_RATE / 1000
}

fn ort_err<E: std::fmt::Display>(err: E) -> anyhow::Error {
    anyhow!("{err}")
}

fn build_session(path: &Path, intra_threads: usize) -> Result<Session> {
    if !path.is_file() {
        bail!("ONNX model was not found: {}", path.display());
    }
    Session::builder()
        .map_err(ort_err)?
        .with_optimization_level(GraphOptimizationLevel::All)
        .map_err(ort_err)?
        .with_intra_threads(intra_threads.max(1))
        .map_err(ort_err)?
        .with_inter_threads(1)
        .map_err(ort_err)?
        .commit_from_file(path)
        .map_err(ort_err)
        .with_context(|| format!("Failed to load ONNX model {}", path.display()))
}

/// Silero VAD v5 (onnx-community export), stepped the way its Python wrapper does: the
/// recurrent state is carried between calls and the last 64 samples of the previous
/// input are prepended, so the model sees 576 samples per 512-sample frame.
struct SileroVad {
    session: Session,
    state: Vec<f32>,
    input: Vec<f32>,
}

impl SileroVad {
    fn load(path: &Path) -> Result<Self> {
        Ok(Self {
            session: build_session(path, 1).context("Failed to load the Silero VAD model")?,
            state: vec![0.0; VAD_STATE_LEN],
            input: vec![0.0; VAD_CONTEXT],
        })
    }
}

impl VoiceActivity for SileroVad {
    fn speech_probability(&mut self, frame: &[f32]) -> Result<f32> {
        let context_start = self.input.len() - VAD_CONTEXT;
        self.input.drain(..context_start);
        self.input.extend_from_slice(frame);
        let sample_rate = [SAMPLE_RATE as i64];
        let outputs = self
            .session
            .run(ort::inputs![
                "input" => TensorRef::from_array_view((vec![1i64, self.input.len() as i64], &self.input[..])).map_err(ort_err)?,
                "state" => TensorRef::from_array_view((vec![2i64, 1, 128], &self.state[..])).map_err(ort_err)?,
                "sr" => TensorRef::from_array_view((Vec::<i64>::new(), &sample_rate[..])).map_err(ort_err)?,
            ])
            .map_err(ort_err)
            .context("Silero VAD step failed")?;
        let (_, probability) = outputs["output"].try_extract_tensor::<f32>().map_err(ort_err)?;
        let probability = *probability.first().ok_or_else(|| anyhow!("Silero VAD returned no probability"))?;
        let (_, state) = outputs["stateN"].try_extract_tensor::<f32>().map_err(ort_err)?;
        if state.len() != VAD_STATE_LEN {
            bail!("Silero VAD returned a state of {} values, expected {VAD_STATE_LEN}", state.len());
        }
        self.state.copy_from_slice(state);
        Ok(probability)
    }

    fn reset(&mut self) {
        self.state.fill(0.0);
        self.input.clear();
        self.input.resize(VAD_CONTEXT, 0.0);
    }
}

/// Smart Turn v3.x. Its `logits` output is already a sigmoid probability.
struct SmartTurn {
    session: Session,
}

impl SmartTurn {
    fn load(path: &Path, threads: usize) -> Result<Self> {
        Ok(Self { session: build_session(path, threads).context("Failed to load the Smart Turn model")? })
    }
}

impl TurnClassifier for SmartTurn {
    fn complete_probability(&mut self, audio: &[f32]) -> Result<f32> {
        let features = smart_turn_features(audio);
        let outputs = self
            .session
            .run(ort::inputs![
                "input_features" => TensorRef::from_array_view((vec![1i64, N_MELS as i64, N_FRAMES as i64], &features[..])).map_err(ort_err)?,
            ])
            .map_err(ort_err)
            .context("Smart Turn inference failed")?;
        let (_, probability) = outputs["logits"].try_extract_tensor::<f32>().map_err(ort_err)?;
        probability.first().copied().ok_or_else(|| anyhow!("Smart Turn returned no probability"))
    }
}

struct MelTables {
    /// Periodic Hann window of `N_FFT` samples.
    window: Vec<f64>,
    /// Per mel filter: first non-zero FFT bin and the weights from there.
    filters: Vec<(usize, Vec<f64>)>,
    fft: Arc<dyn RealToComplex<f64>>,
}

/// Slaney mel scale, as `transformers.audio_utils.hertz_to_mel(mel_scale="slaney")`.
fn hertz_to_mel(freq: f64) -> f64 {
    if freq >= 1000.0 {
        15.0 + (freq / 1000.0).ln() * (27.0 / 6.4f64.ln())
    } else {
        3.0 * freq / 200.0
    }
}

fn mel_to_hertz(mel: f64) -> f64 {
    if mel >= 15.0 {
        1000.0 * ((6.4f64.ln() / 27.0) * (mel - 15.0)).exp()
    } else {
        200.0 * mel / 3.0
    }
}

/// `np.linspace(start, stop, num)`: `i * step + start`, with the last point exactly `stop`.
fn linspace(start: f64, stop: f64, num: usize) -> Vec<f64> {
    let step = (stop - start) / (num - 1) as f64;
    let mut values: Vec<f64> = (0..num).map(|i| i as f64 * step + start).collect();
    values[num - 1] = stop;
    values
}

fn mel_tables() -> &'static MelTables {
    static TABLES: OnceLock<MelTables> = OnceLock::new();
    TABLES.get_or_init(|| {
        // `np.hanning(N_FFT + 1)[:-1]`, using numpy's own formula.
        let m = (N_FFT + 1) as f64;
        let window = (0..N_FFT)
            .map(|i| {
                let n = 2.0 * i as f64 + 1.0 - m;
                0.5 + 0.5 * (std::f64::consts::PI * n / (m - 1.0)).cos()
            })
            .collect();

        // `mel_filter_bank(201, 80, 0, 8000, 16000, norm="slaney", mel_scale="slaney")`.
        let mel_points = linspace(hertz_to_mel(0.0), hertz_to_mel(8000.0), N_MELS + 2);
        let filter_freqs: Vec<f64> = mel_points.iter().map(|&mel| mel_to_hertz(mel)).collect();
        let fft_freqs = linspace(0.0, (SAMPLE_RATE / 2) as f64, N_BINS);
        let filters = (0..N_MELS)
            .map(|j| {
                let enorm = 2.0 / (filter_freqs[j + 2] - filter_freqs[j]);
                let weights: Vec<f64> = fft_freqs
                    .iter()
                    .map(|&freq| {
                        let down = -(filter_freqs[j] - freq) / (filter_freqs[j + 1] - filter_freqs[j]);
                        let up = (filter_freqs[j + 2] - freq) / (filter_freqs[j + 2] - filter_freqs[j + 1]);
                        0.0f64.max(down.min(up)) * enorm
                    })
                    .collect();
                let first = weights.iter().position(|&w| w != 0.0).unwrap_or(0);
                let last = weights.iter().rposition(|&w| w != 0.0).map_or(first, |i| i + 1);
                (first, weights[first..last].to_vec())
            })
            .collect();

        let fft = RealFftPlanner::<f64>::new().plan_fft_forward(N_FFT);
        MelTables { window, filters, fft }
    })
}

/// The model-independent part, public for testing: the exact Smart Turn input tensor for a
/// clip (80 x 800 log-mel, Whisper-compatible, row-major by mel bin), from up to the last
/// 8 s of `samples`.
pub(crate) fn smart_turn_features(samples: &[f32]) -> Vec<f32> {
    let tables = mel_tables();

    // The last 8 s, zero-padded at the beginning (upstream `truncate_audio_to_last_n_seconds`).
    let mut audio = vec![0.0f32; WINDOW_SAMPLES];
    let tail = &samples[samples.len().saturating_sub(WINDOW_SAMPLES)..];
    audio[WINDOW_SAMPLES - tail.len()..].copy_from_slice(tail);

    // `do_normalize`: zero mean, unit variance over the whole padded array (its attention
    // mask is all ones), in float32 like numpy: `(x - x.mean()) / np.sqrt(x.var() + 1e-7)`.
    let count = WINDOW_SAMPLES as f64;
    let mean = audio.iter().map(|&x| x as f64).sum::<f64>() / count;
    let variance = audio.iter().map(|&x| (x as f64 - mean).powi(2)).sum::<f64>() / count;
    let (mean, scale) = (mean as f32, (variance as f32 + 1e-7f32).sqrt());
    for x in &mut audio {
        *x = (*x - mean) / scale;
    }

    // Centred STFT: reflect padding of N_FFT / 2 on both sides, as `np.pad(mode="reflect")`.
    let half = N_FFT / 2;
    let mut padded = Vec::with_capacity(WINDOW_SAMPLES + N_FFT);
    padded.extend((1..=half).rev().map(|i| audio[i] as f64));
    padded.extend(audio.iter().map(|&x| x as f64));
    padded.extend((0..half).map(|k| audio[WINDOW_SAMPLES - 2 - k] as f64));

    // 801 frames come out of the STFT; the extractor drops the last one.
    let mut features = vec![0.0f32; N_MELS * N_FRAMES];
    let mut buffer = tables.fft.make_input_vec();
    let mut spectrum = tables.fft.make_output_vec();
    let mut scratch = tables.fft.make_scratch_vec();
    let mut power = vec![0.0f64; N_BINS];
    for frame in 0..N_FRAMES {
        let start = frame * HOP;
        for (k, value) in buffer.iter_mut().enumerate() {
            *value = padded[start + k] * tables.window[k];
        }
        tables
            .fft
            .process_with_scratch(&mut buffer, &mut spectrum, &mut scratch)
            .expect("FFT buffers have the planned lengths");
        // numpy keeps the STFT as complex64, then squares the magnitude in float64.
        for (bin, value) in spectrum.iter().enumerate() {
            let (re, im) = (value.re as f32 as f64, value.im as f32 as f64);
            power[bin] = re * re + im * im;
        }
        for (mel, (first, weights)) in tables.filters.iter().enumerate() {
            let energy: f64 = weights.iter().zip(&power[*first..]).map(|(w, p)| w * p).sum();
            features[mel * N_FRAMES + frame] = energy.max(1e-10).log10() as f32;
        }
    }

    let floor = features.iter().copied().fold(f32::NEG_INFINITY, f32::max) - 8.0;
    for x in &mut features {
        *x = (x.max(floor) + 4.0) / 4.0;
    }
    features
}

/// Lets the microphone session's capture thread run the detector on each block.
impl crate::assistant_voice::TurnDecider for TurnDetector {
    fn push(&mut self, samples: &[f32]) -> Result<Vec<crate::assistant_voice::AutoTurnEvent>> {
        use crate::assistant_voice::AutoTurnEvent;
        Ok(TurnDetector::push(self, samples)?
            .into_iter()
            .map(|event| match event {
                TurnEvent::SpeechStarted => AutoTurnEvent::SpeechStarted,
                TurnEvent::EndOfTurn { .. } => AutoTurnEvent::EndOfTurn,
            })
            .collect())
    }

    fn reset(&mut self) {
        TurnDetector::reset(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Instant;

    fn data_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests-data/smart_turn")
    }

    fn read_wav(path: &Path) -> Vec<f32> {
        let mut reader = hound::WavReader::open(path).unwrap();
        let spec = reader.spec();
        assert_eq!((spec.sample_rate, spec.channels, spec.bits_per_sample), (16_000, 1, 16), "{}", path.display());
        reader.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect()
    }

    fn reference() -> serde_json::Value {
        let text = std::fs::read_to_string(data_dir().join("reference.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn fixtures() -> Vec<(String, Vec<f32>, f64)> {
        let reference = reference();
        reference["fixtures"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                let samples = read_wav(&data_dir().join(entry["wav"].as_str().unwrap()));
                assert_eq!(samples.len() as u64, entry["samples"].as_u64().unwrap());
                (entry["name"].as_str().unwrap().to_string(), samples, entry["probability"].as_f64().unwrap())
            })
            .collect()
    }

    /// The layout written by scripts/smart_turn_reference.py.
    fn summarise(features: &[f32], sampled: &[usize]) -> Vec<f32> {
        let column = |frame: usize| (0..N_MELS).map(move |mel| features[mel * N_FRAMES + frame]);
        let mut out = Vec::new();
        out.extend((0..N_FRAMES).map(|f| column(f).sum::<f32>() / N_MELS as f32));
        out.extend((0..N_FRAMES).map(|f| column(f).fold(f32::INFINITY, f32::min)));
        out.extend((0..N_FRAMES).map(|f| column(f).fold(f32::NEG_INFINITY, f32::max)));
        for &frame in sampled {
            out.extend(column(frame));
        }
        out
    }

    #[test]
    fn features_match_upstream_reference() {
        let reference = reference();
        assert_eq!(reference["n_mels"].as_u64(), Some(N_MELS as u64));
        assert_eq!(reference["n_frames"].as_u64(), Some(N_FRAMES as u64));
        let sampled: Vec<usize> =
            reference["sampled_frames"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as usize).collect();
        let mut worst = 0.0f32;
        for (name, samples, _) in fixtures() {
            let features = smart_turn_features(&samples);
            assert_eq!(features.len(), N_MELS * N_FRAMES, "{name}");
            let bytes = std::fs::read(data_dir().join(format!("{name}.f32"))).unwrap();
            let expected: Vec<f32> =
                bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
            let actual = summarise(&features, &sampled);
            assert_eq!(actual.len(), expected.len(), "{name}");
            let diff = actual.iter().zip(&expected).map(|(a, e)| (a - e).abs()).fold(0.0f32, f32::max);
            println!("{name}: max abs diff {diff:.3e}");
            assert!(diff <= 1e-3, "{name}: max abs diff {diff}");
            worst = worst.max(diff);
        }
        println!("worst max abs diff {worst:.3e}");
    }

    #[test]
    fn features_pad_at_the_start_and_keep_the_last_8_s() {
        let fixtures = fixtures();
        let (_, question, _) = fixtures.iter().find(|(name, ..)| name == "question").unwrap();
        // Shorter clips are zero-padded at the beginning, so leading zeros change nothing.
        let mut with_lead = vec![0.0f32; 16_000];
        with_lead.extend_from_slice(question);
        assert_eq!(smart_turn_features(question), smart_turn_features(&with_lead));
        // Padding at the end instead would move the speech and change the tensor.
        let mut with_tail = question.clone();
        with_tail.extend(std::iter::repeat(0.0).take(16_000));
        assert_ne!(smart_turn_features(question), smart_turn_features(&with_tail));
        // Longer clips keep only their last 8 s.
        let (_, long, _) = fixtures.iter().find(|(name, ..)| name == "long").unwrap();
        assert!(long.len() > WINDOW_SAMPLES);
        assert_eq!(smart_turn_features(long), smart_turn_features(&long[long.len() - WINDOW_SAMPLES..]));
        // Pure silence: every value is the floor, (log10(1e-10) + 4) / 4.
        assert!(smart_turn_features(&[]).iter().all(|&x| x == -1.5));
    }

    fn models_dir() -> PathBuf {
        PathBuf::from(std::env::var("VOICEREADER_SMART_TURN_DIR").expect("set VOICEREADER_SMART_TURN_DIR to the models folder"))
    }

    #[test]
    #[ignore = "needs VOICEREADER_SMART_TURN_DIR with smart-turn-v3.2-cpu.onnx and the bundled ONNX Runtime"]
    fn probabilities_match_upstream_reference() {
        ensure_onnxruntime().unwrap();
        let mut model = SmartTurn::load(&models_dir().join("smart-turn-v3.2-cpu.onnx"), SMART_TURN_THREADS).unwrap();
        for (name, samples, expected) in fixtures() {
            let probability = model.complete_probability(&samples).unwrap() as f64;
            let diff = (probability - expected).abs();
            println!("{name}: rust {probability:.5}, reference {expected:.5}, diff {diff:.2e}");
            assert!(diff <= 0.02, "{name}: {probability} against {expected}");
            assert_eq!(probability > 0.5, expected > 0.5, "{name}: decision differs");
        }
    }

    #[test]
    #[ignore = "measurement; needs VOICEREADER_SMART_TURN_DIR and the bundled ONNX Runtime; run with --nocapture"]
    fn measure_timings_and_fixture_behaviour() {
        ensure_onnxruntime().unwrap();
        let dir = models_dir();
        let fixtures = fixtures();
        let (_, question, _) = fixtures.iter().find(|(name, ..)| name == "question").unwrap();
        let profile = if cfg!(debug_assertions) { "debug" } else { "release" };

        let runs = 20;
        let started = Instant::now();
        for _ in 0..runs {
            std::hint::black_box(smart_turn_features(question));
        }
        println!("[{profile}] features: {:.2} ms", started.elapsed().as_secs_f64() * 1000.0 / runs as f64);
        for threads in [1, 2, 4] {
            let mut model = SmartTurn::load(&dir.join("smart-turn-v3.2-cpu.onnx"), threads).unwrap();
            model.complete_probability(question).unwrap();
            let started = Instant::now();
            for _ in 0..runs {
                model.complete_probability(question).unwrap();
            }
            println!(
                "[{profile}] Smart Turn ({threads} threads), features included: {:.2} ms",
                started.elapsed().as_secs_f64() * 1000.0 / runs as f64
            );
        }

        let mut vad = SileroVad::load(&dir.join("silero-vad.onnx")).unwrap();
        let frames: Vec<&[f32]> = question.chunks_exact(VAD_FRAME).collect();
        let started = Instant::now();
        for frame in &frames {
            vad.speech_probability(frame).unwrap();
        }
        println!("[{profile}] VAD step: {:.3} ms", started.elapsed().as_secs_f64() * 1000.0 / frames.len() as f64);

        // Each fixture through the full detector: 0.5 s of silence, the clip, 2.5 s of silence.
        let mut detector = TurnDetector::new(
            &dir.join("smart-turn-v3.2-cpu.onnx"),
            &dir.join("silero-vad.onnx"),
            TurnDetectorConfig::default(),
        )
        .unwrap();
        for (name, samples, _) in &fixtures {
            detector.reset();
            let mut audio = vec![0.0f32; 8_000];
            audio.extend_from_slice(samples);
            audio.extend(std::iter::repeat(0.0).take(40_000));
            let mut timeline = Vec::new();
            for (index, chunk) in audio.chunks(1_600).enumerate() {
                for event in detector.push(chunk).unwrap() {
                    timeline.push(format!("{:.2}s {event:?}", (index + 1) as f64 * 0.1));
                }
            }
            println!("{name} ({:.2} s): {}", samples.len() as f64 / 16_000.0, if timeline.is_empty() { "no events".into() } else { timeline.join(", ") });
        }
    }

    /// Speech when a frame's first sample is above 0.5.
    struct FakeVad;

    impl VoiceActivity for FakeVad {
        fn speech_probability(&mut self, frame: &[f32]) -> Result<f32> {
            Ok(if frame[0] > 0.5 { 0.9 } else { 0.1 })
        }
        fn reset(&mut self) {}
    }

    /// Answers from a script (0 once it runs out) and records the audio length of each call.
    #[derive(Clone, Default)]
    struct FakeClassifier {
        answers: Arc<Mutex<VecDeque<f32>>>,
        calls: Arc<Mutex<Vec<usize>>>,
    }

    impl TurnClassifier for FakeClassifier {
        fn complete_probability(&mut self, audio: &[f32]) -> Result<f32> {
            self.calls.lock().unwrap().push(audio.len());
            Ok(self.answers.lock().unwrap().pop_front().unwrap_or(0.0))
        }
    }

    fn fake_detector(answers: &[f32]) -> (TurnDetector, FakeClassifier) {
        let classifier = FakeClassifier::default();
        classifier.answers.lock().unwrap().extend(answers);
        let detector =
            TurnDetector::from_parts(Box::new(FakeVad), Box::new(classifier.clone()), TurnDetectorConfig::default());
        (detector, classifier)
    }

    fn speech(frames: usize) -> Vec<f32> {
        vec![1.0; frames * VAD_FRAME]
    }

    fn silence(frames: usize) -> Vec<f32> {
        vec![0.0; frames * VAD_FRAME]
    }

    /// Frames (32 ms each) needed to reach `ms`.
    fn frames_for(ms: u32) -> usize {
        ms_to_samples(ms).div_ceil(VAD_FRAME)
    }

    #[test]
    fn speech_starts_only_after_min_speech() {
        let (mut detector, _) = fake_detector(&[]);
        let needed = frames_for(250);
        assert_eq!(needed, 8);
        // A short burst followed by silence is ignored.
        assert!(detector.push(&speech(needed - 1)).unwrap().is_empty());
        assert!(detector.push(&silence(3)).unwrap().is_empty());
        // The count restarts after the silence.
        assert!(detector.push(&speech(needed - 1)).unwrap().is_empty());
        assert_eq!(detector.push(&speech(1)).unwrap(), vec![TurnEvent::SpeechStarted]);
        assert!(detector.push(&speech(20)).unwrap().is_empty());
    }

    #[test]
    fn partial_frames_are_buffered() {
        let (mut detector, _) = fake_detector(&[]);
        let audio = speech(8);
        let mut events = Vec::new();
        for chunk in audio.chunks(100) {
            events.extend(detector.push(chunk).unwrap());
        }
        assert_eq!(events, vec![TurnEvent::SpeechStarted]);
    }

    #[test]
    fn complete_turn_ends_after_pause() {
        let (mut detector, classifier) = fake_detector(&[0.8]);
        detector.push(&speech(20)).unwrap();
        let pause_frames = frames_for(300);
        assert!(detector.push(&silence(pause_frames - 1)).unwrap().is_empty());
        assert!(classifier.calls.lock().unwrap().is_empty(), "asked before pause_ms");
        assert_eq!(detector.push(&silence(1)).unwrap(), vec![TurnEvent::EndOfTurn { probability: 0.8 }]);
        // The turn audio was the pre-roll window plus speech and pause, under 8 s.
        let calls = classifier.calls.lock().unwrap().clone();
        assert_eq!(calls, vec![(20 + pause_frames) * VAD_FRAME]);
        // Back to idle: silence brings nothing more.
        assert!(detector.push(&silence(100)).unwrap().is_empty());
    }

    #[test]
    fn incomplete_turn_keeps_listening_and_asks_again() {
        let (mut detector, classifier) = fake_detector(&[0.2, 0.3, 0.9]);
        detector.push(&speech(20)).unwrap();
        let first = frames_for(300);
        let second = frames_for(600);
        let third = frames_for(900);
        assert!(detector.push(&silence(first)).unwrap().is_empty());
        assert_eq!(classifier.calls.lock().unwrap().len(), 1);
        assert!(detector.push(&silence(second - first)).unwrap().is_empty());
        assert_eq!(classifier.calls.lock().unwrap().len(), 2);
        assert!(detector.push(&silence(third - second - 1)).unwrap().is_empty());
        assert_eq!(detector.push(&silence(1)).unwrap(), vec![TurnEvent::EndOfTurn { probability: 0.9 }]);
        assert_eq!(classifier.calls.lock().unwrap().len(), 3);
    }

    #[test]
    fn long_pause_ends_the_turn_anyway() {
        let (mut detector, classifier) = fake_detector(&[]);
        detector.push(&speech(20)).unwrap();
        let limit = frames_for(2000);
        assert!(detector.push(&silence(limit - 1)).unwrap().is_empty());
        assert_eq!(detector.push(&silence(1)).unwrap(), vec![TurnEvent::EndOfTurn { probability: 0.0 }]);
        // Asked at 300, 600, ... 1800 ms of pause: six times.
        assert_eq!(classifier.calls.lock().unwrap().len(), 6);
    }

    #[test]
    fn speech_resuming_cancels_the_pending_end() {
        let (mut detector, classifier) = fake_detector(&[0.1, 0.9]);
        detector.push(&speech(20)).unwrap();
        // First pause: asked once, incomplete.
        assert!(detector.push(&silence(frames_for(300) + 2)).unwrap().is_empty());
        // Speech resumes: no event, and the pause timer restarts.
        assert!(detector.push(&speech(5)).unwrap().is_empty());
        assert!(detector.push(&silence(frames_for(300) - 1)).unwrap().is_empty());
        assert_eq!(classifier.calls.lock().unwrap().len(), 1);
        assert_eq!(detector.push(&silence(1)).unwrap(), vec![TurnEvent::EndOfTurn { probability: 0.9 }]);
        // The second ask saw the whole turn, including the first pause and the new speech.
        let calls = classifier.calls.lock().unwrap().clone();
        assert!(calls[1] > calls[0]);
    }

    #[test]
    fn reset_clears_state() {
        let (mut detector, classifier) = fake_detector(&[0.9]);
        detector.push(&speech(20)).unwrap();
        detector.push(&silence(2)).unwrap();
        detector.push(&[1.0; 100]).unwrap();
        detector.reset();
        // No pending end: silence after a reset asks nothing.
        assert!(detector.push(&silence(200)).unwrap().is_empty());
        assert!(classifier.calls.lock().unwrap().is_empty());
        // Speech must again last min_speech_ms; the 100 buffered samples were dropped.
        assert!(detector.push(&speech(7)).unwrap().is_empty());
        assert_eq!(detector.push(&speech(1)).unwrap(), vec![TurnEvent::SpeechStarted]);
    }

    #[test]
    fn turn_audio_is_capped_at_8_s_with_short_pre_roll() {
        let (mut detector, classifier) = fake_detector(&[0.9]);
        detector.push(&silence(100)).unwrap();
        detector.push(&speech(400)).unwrap();
        detector.push(&silence(frames_for(300))).unwrap();
        assert_eq!(classifier.calls.lock().unwrap().clone(), vec![WINDOW_SAMPLES]);

        let (mut detector, classifier) = fake_detector(&[0.9]);
        detector.push(&silence(100)).unwrap();
        detector.push(&speech(10)).unwrap();
        detector.push(&silence(frames_for(300))).unwrap();
        // Pre-roll: the last 500 ms before speech, rounded to whole frames.
        let pre_roll = ms_to_samples(PRE_SPEECH_MS);
        assert_eq!(classifier.calls.lock().unwrap().clone(), vec![pre_roll + (10 + frames_for(300)) * VAD_FRAME]);
    }

    #[test]
    fn rejects_bad_thresholds() {
        let config = TurnDetectorConfig { vad_threshold: 1.5, ..TurnDetectorConfig::default() };
        assert!(validate(&config).is_err());
        assert!(validate(&TurnDetectorConfig::default()).is_ok());
    }
}
