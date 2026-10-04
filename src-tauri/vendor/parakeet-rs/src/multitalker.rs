//! Multi-talker streaming ASR pipeline.
//!
//! Combines Sortformer speaker diarisation with the multitalker encoder
//! (speaker kernel injection) to produce per-speaker transcriptions from
//! mixed audio. Each active speaker gets an independent encoder cache and
//! decoder state.
//!
//! Architecture:
//! ```text
//! Audio -> [Mel] -> [Sortformer raw preds] -> per-speaker masks
//!                   -> [ASR Encoder(mel, cache_k, spk_k, bg_k)] -> [RNNT Decode] -> text_k
//! ```

use crate::decoder::{TimedToken, TranscriptionResult};
use crate::error::{Error, Result};
use crate::execution::ModelConfig as ExecutionConfig;
use crate::model_multitalker::{MultitalkerEncoderCache, MultitalkerModel};
use crate::nemotron::SentencePieceVocab;
use crate::sortformer::{Sortformer, NUM_SPEAKERS};
use crate::timestamps::{self, TimestampMode};
use crate::transcriber::Transcriber;
use ndarray::{s, Array2, Array3};
use std::path::Path;

// Reuse the same audio constants as Nemotron (same encoder architecture)
const SAMPLE_RATE: usize = 16000;
const N_FFT: usize = 512;
const WIN_LENGTH: usize = 400;
const HOP_LENGTH: usize = 160;
const N_MELS: usize = 128;
const PREEMPH: f32 = 0.97;
const LOG_ZERO_GUARD: f32 = 5.960_464_5e-8;

// Encoder arch (same as Nemotron 0.6B)
const NUM_ENCODER_LAYERS: usize = 24;
const HIDDEN_DIM: usize = 1024;
const LEFT_CONTEXT: usize = 70;
const CONV_CONTEXT: usize = 8;

// Decoder
const VOCAB_SIZE: usize = 1024;
const BLANK_ID: usize = 1024;
const DECODER_LSTM_DIM: usize = 640;
const MAX_SYMBOLS_PER_STEP: usize = 10;

// Pre-encode cache frames (fixed, independent of latency mode)
const PRE_ENCODE_CACHE: usize = 9;

// Each encoded frame spans 8 mel frames at 10ms hop = 80ms
const SECONDS_PER_ENCODED_FRAME: f32 = 0.08;

/// Activity threshold: a speaker is considered active if any frame in the
/// chunk exceeds this probability.
const SPEAKER_ACTIVITY_THRESHOLD: f32 = 0.3;

/// Word-level timestamp for a single word in a speaker's transcript.
#[derive(Debug, Clone)]
pub struct WordTimestamp {
    pub word: String,
    pub start_secs: f32,
    pub end_secs: f32,
}

/// Per-speaker state for the multi-instance architecture.
struct SpeakerInstance {
    encoder_cache: MultitalkerEncoderCache,
    state_1: Array3<f32>,
    state_2: Array3<f32>,
    last_token: i32,
    /// Each entry is (token_id, absolute_encoder_frame).
    accumulated_tokens: Vec<(usize, usize)>,
    speaker_id: usize,
}

impl SpeakerInstance {
    fn new(speaker_id: usize) -> Self {
        Self {
            encoder_cache: MultitalkerEncoderCache::new(
                NUM_ENCODER_LAYERS,
                LEFT_CONTEXT,
                HIDDEN_DIM,
                CONV_CONTEXT,
            ),
            state_1: Array3::zeros((2, 1, DECODER_LSTM_DIM)),
            state_2: Array3::zeros((2, 1, DECODER_LSTM_DIM)),
            last_token: BLANK_ID as i32,
            accumulated_tokens: Vec::new(),
            speaker_id,
        }
    }
}

/// Per-speaker transcription output.
#[derive(Debug, Clone)]
pub struct SpeakerTranscript {
    pub speaker_id: usize,
    pub text: String,
    pub words: Vec<WordTimestamp>,
}

/// Streaming latency mode controlling the encoder chunk size.
///
/// The multitalker encoder was trained with multi-latency masking, so it can
/// operate at different chunk sizes at inference time. Smaller chunks give
/// lower latency but reduce accuracy because fewer future frames are available
/// to the attention layers.
///
/// Each mode corresponds to an `att_context_size` configuration in the model:
/// the second value is the number of future encoded frames the first layer
/// group can attend to.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LatencyMode {
    /// `[70, 13]` -- 14 encoded frames, 112 mel frames, 1.12s latency.
    /// Highest accuracy. This is the default.
    #[default]
    Normal,
    /// `[70, 6]` -- 7 encoded frames, 56 mel frames, 0.56s latency.
    Low,
    /// `[70, 1]` -- 2 encoded frames, 16 mel frames, 0.16s latency.
    VeryLow,
    /// `[70, 0]` -- 1 encoded frame, 8 mel frames, 0.08s latency.
    /// Lowest accuracy.
    Ultra,
}

impl LatencyMode {
    /// Number of mel spectrogram frames per encoder chunk.
    pub const fn chunk_mel_frames(self) -> usize {
        match self {
            Self::Normal => 112,  // 14 * 8
            Self::Low => 56,      //  7 * 8
            Self::VeryLow => 16,  //  2 * 8
            Self::Ultra => 8,     //  1 * 8
        }
    }

    /// Number of encoded frames per chunk (after 8x subsampling).
    pub const fn encoded_frames(self) -> usize {
        match self {
            Self::Normal => 14,
            Self::Low => 7,
            Self::VeryLow => 2,
            Self::Ultra => 1,
        }
    }

    /// Approximate latency in seconds.
    pub const fn latency_secs(self) -> f32 {
        match self {
            Self::Normal => 1.12,
            Self::Low => 0.56,
            Self::VeryLow => 0.16,
            Self::Ultra => 0.08,
        }
    }
}

/// Runtime configuration for the multitalker pipeline.
///
/// These settings can be changed between calls to `transcribe_chunk()` via
/// the setter methods on [`MultitalkerASR`]. Changing `latency_mode` requires
/// calling [`MultitalkerASR::reset()`] first (the setter does this automatically).
#[derive(Debug, Clone)]
pub struct MultitalkerConfig {
    /// Maximum number of concurrent speakers to track (1..=8).
    /// The Nemotron-3 (Sortformer v3) model supports up to 8 speaker slots.
    /// Setting this lower reduces compute by skipping inactive slots.
    pub max_speakers: usize,

    /// Minimum speaker activity probability to consider a speaker active
    /// in a given chunk. Higher values require stronger evidence of speech
    /// before creating a speaker instance. Range: 0.0..=1.0.
    pub activity_threshold: f32,

    /// Streaming latency mode. Controls the encoder chunk size and
    /// therefore the latency-accuracy tradeoff.
    pub latency_mode: LatencyMode,

    // VoiceReader patch.
    /// How long a speaker's activity stays switched on after the diariser says they
    /// stopped. The model emits a word slightly after it was spoken; without a hold,
    /// the last words before a pause or a speaker change are lost. 0 disables it.
    pub speaker_hold_secs: f32,

    // VoiceReader patch.
    /// The same hold, applied to the activity of the *other* speakers that each
    /// speaker's encoder sees as background. It keeps a new speaker from picking up
    /// the previous speaker's last word. Keep it shorter than `speaker_hold_secs`,
    /// or the start of the new speaker's first sentence is treated as background.
    pub background_hold_secs: f32,

    // VoiceReader patch.
    /// Transcribe speakers beyond `max_speakers` together, as one extra "speaker"
    /// with the id [`UNASSIGNED_SPEAKER_ID`], instead of skipping them.
    pub transcribe_beyond_limit: bool,
}

impl Default for MultitalkerConfig {
    fn default() -> Self {
        Self {
            max_speakers: NUM_SPEAKERS,
            activity_threshold: SPEAKER_ACTIVITY_THRESHOLD,
            latency_mode: LatencyMode::default(),
            speaker_hold_secs: 0.0,
            background_hold_secs: 0.0,
            transcribe_beyond_limit: false,
        }
    }
}

impl MultitalkerConfig {
    /// The mel-frame chunk size for the current latency mode.
    pub fn chunk_size(&self) -> usize {
        self.latency_mode.chunk_mel_frames()
    }
}

/// Multi-talker streaming ASR combining Sortformer diarisation with
/// speaker-kernel-injected ASR encoding.
pub struct MultitalkerASR {
    model: MultitalkerModel,
    sortformer: Sortformer,
    vocab: SentencePieceVocab,
    speakers: Vec<SpeakerInstance>,
    config: MultitalkerConfig,
    mel_basis: Array2<f32>,
    audio_buffer: Vec<f32>,
    audio_processed: usize,
    chunk_idx: usize,
    // VoiceReader patch: frames of hold left per speaker, carried across chunks.
    speaker_hold_left: Vec<usize>,
    background_hold_left: Vec<usize>,
    // VoiceReader patch: seconds of speech heard from speakers beyond `max_speakers`.
    beyond_limit_secs: f32,
}

impl MultitalkerASR {
    /// Load the multitalker ASR pipeline.
    ///
    /// # Arguments
    /// * `asr_model_dir` - Directory containing encoder.onnx, decoder_joint.onnx, tokenizer.model
    /// * `sortformer_model_path` - Path to Sortformer ONNX model
    /// * `exec_config` - ONNX Runtime execution config (optional)
    pub fn from_pretrained<P: AsRef<Path>, Q: AsRef<Path>>(
        asr_model_dir: P,
        sortformer_model_path: Q,
        exec_config: Option<ExecutionConfig>,
    ) -> Result<Self> {
        let asr_dir = asr_model_dir.as_ref();
        let exec = exec_config.unwrap_or_default();

        let vocab = SentencePieceVocab::from_file(asr_dir.join("tokenizer.model"))?;

        let model = MultitalkerModel::from_pretrained(asr_dir, exec.clone())?;
        let sortformer = Sortformer::with_config(
            sortformer_model_path,
            Some(exec),
            crate::sortformer::DiarizationConfig::default(),
        )?;

        let mel_basis = crate::audio::create_mel_filterbank(N_FFT, N_MELS, SAMPLE_RATE);

        Ok(Self {
            model,
            sortformer,
            vocab,
            speakers: Vec::new(),
            config: MultitalkerConfig::default(),
            mel_basis,
            audio_buffer: Vec::new(),
            audio_processed: 0,
            chunk_idx: 0,
            speaker_hold_left: Vec::new(),
            background_hold_left: Vec::new(),
            beyond_limit_secs: 0.0,
        })
    }

    /// Reset all state for a new utterance.
    pub fn reset(&mut self) {
        self.speakers.clear();
        self.sortformer.reset_state();
        self.audio_buffer.clear();
        self.audio_processed = 0;
        self.chunk_idx = 0;
        self.speaker_hold_left.clear();
        self.background_hold_left.clear();
        self.beyond_limit_secs = 0.0;
    }

    /// Returns the current multitalker configuration.
    pub fn multitalker_config(&self) -> &MultitalkerConfig {
        &self.config
    }

    /// Set the maximum number of speakers to track (1..=4).
    ///
    /// Can be called between chunks to adjust mid-session. Existing speaker
    /// instances above the new limit will still produce output for any
    /// already-accumulated tokens, but won't receive new audio.
    pub fn set_max_speakers(&mut self, max_speakers: usize) {
        self.config.max_speakers = max_speakers.clamp(1, NUM_SPEAKERS);
    }

    /// Set the speaker activity threshold (0.0..=1.0).
    ///
    /// A speaker is considered active in a chunk if any frame's probability
    /// exceeds this value. Lower values are more sensitive (detect quieter
    /// speakers sooner), higher values require stronger evidence.
    pub fn set_activity_threshold(&mut self, threshold: f32) {
        self.config.activity_threshold = threshold.clamp(0.0, 1.0);
    }

    // VoiceReader patch.
    /// Set how long a speaker's activity is held after they stop, and how long the
    /// other speakers' activity is held as background. See [`MultitalkerConfig`].
    pub fn set_speaker_hold(&mut self, speaker_secs: f32, background_secs: f32) {
        self.config.speaker_hold_secs = speaker_secs.max(0.0);
        self.config.background_hold_secs = background_secs.max(0.0);
    }

    // VoiceReader patch.
    /// Transcribe speakers beyond `max_speakers` as one extra speaker instead of
    /// skipping them. See [`MultitalkerConfig::transcribe_beyond_limit`].
    pub fn set_transcribe_beyond_limit(&mut self, enabled: bool) {
        self.config.transcribe_beyond_limit = enabled;
    }

    /// Set the streaming latency mode.
    ///
    /// This changes the encoder chunk size, trading latency for accuracy.
    /// Because encoder caches are tied to the chunk size, this automatically
    /// calls [`reset()`](Self::reset) to clear all state.
    pub fn set_latency_mode(&mut self, mode: LatencyMode) {
        if self.config.latency_mode != mode {
            self.config.latency_mode = mode;
            self.reset();
        }
    }

    /// Returns the number of audio samples the caller should provide per
    /// chunk for the current latency mode. This is `chunk_mel_frames * HOP_LENGTH`.
    pub fn chunk_audio_samples(&self) -> usize {
        self.config.chunk_size() * HOP_LENGTH
    }

    // VoiceReader patch.
    /// Seconds of speech the diariser attributed to speakers beyond `max_speakers`
    /// since the last reset. Those speakers are not transcribed, so anything above a
    /// second or two means part of the recording is missing from the transcript.
    pub fn speech_beyond_limit_secs(&self) -> f32 {
        self.beyond_limit_secs
    }

    /// Get accumulated per-speaker transcripts.
    pub fn get_transcripts(&self) -> Vec<SpeakerTranscript> {
        self.speakers
            .iter()
            .map(|spk| {
                let valid_ids: Vec<usize> = spk
                    .accumulated_tokens
                    .iter()
                    .filter(|&&(t, _)| t < VOCAB_SIZE)
                    .map(|&(t, _)| t)
                    .collect();
                let words = self.tokens_to_words(&spk.accumulated_tokens);
                SpeakerTranscript {
                    speaker_id: spk.speaker_id,
                    text: self.vocab.decode(&valid_ids),
                    words,
                }
            })
            .collect()
    }

    /// Process one audio chunk in streaming mode.
    ///
    /// Returns per-speaker text deltas for this chunk. Speakers are created
    /// automatically when first detected.
    pub fn transcribe_chunk(&mut self, audio_chunk: &[f32]) -> Result<Vec<SpeakerTranscript>> {
        self.audio_buffer.extend_from_slice(audio_chunk);

        let total_audio = self.audio_buffer.len();
        if total_audio < WIN_LENGTH {
            return Ok(vec![]);
        }

        // Compute mel over full buffer
        let full_mel = self.compute_mel_spectrogram(&self.audio_buffer)?;
        let total_mel_frames = full_mel.shape()[1];

        let processed_mel_frames = self.audio_processed / HOP_LENGTH;
        let chunk_size = self.config.chunk_size();
        let available_new_frames = total_mel_frames.saturating_sub(processed_mel_frames);
        if available_new_frames < chunk_size {
            return Ok(vec![]);
        }

        // Get raw diarisation predictions from Sortformer.
        // NOTE: The ASR chunk (~1.12s in Normal mode) is smaller than Sortformer's
        // internal stride (~10s). Sortformer pads the short input internally. A future
        // improvement would decouple the two chunk rates: buffer audio for Sortformer
        // and run ASR sub-chunks against the resulting predictions.
        let raw_preds = self.sortformer.diarize_chunk_raw(audio_chunk)?;
        // VoiceReader patch: hold each speaker's activity for a moment after they stop.
        // `target_preds` decides who is active and drives each speaker's own mask;
        // `background_preds` drives what each speaker sees of the others.
        let chunk_secs = audio_chunk.len() as f32 / SAMPLE_RATE as f32;
        let frames_per_sec = if chunk_secs > 0.0 {
            raw_preds.predictions.nrows() as f32 / chunk_secs
        } else {
            0.0
        };
        let target_preds = hold_activity(
            &raw_preds.predictions,
            (self.config.speaker_hold_secs * frames_per_sec).round() as usize,
            &mut self.speaker_hold_left,
        );
        let background_preds = hold_activity(
            &raw_preds.predictions,
            (self.config.background_hold_secs * frames_per_sec).round() as usize,
            &mut self.background_hold_left,
        );
        let diar_preds = &target_preds;

        // VoiceReader patch: count the frames in which someone beyond the limit spoke.
        if frames_per_sec > 0.0 {
            let raw = &raw_preds.predictions;
            let frames = (0..raw.nrows())
                .filter(|&t| (self.config.max_speakers..raw.ncols()).any(|spk| raw[[t, spk]] >= HOLD_ACTIVITY_LEVEL))
                .count();
            self.beyond_limit_secs += frames as f32 / frames_per_sec;
        }

        // Determine active speakers
        let mut active_speakers = Vec::new();
        for spk_id in 0..self.config.max_speakers {
            if spk_id >= diar_preds.ncols() {
                break;
            }
            let max_activity = (0..diar_preds.nrows())
                .map(|t| diar_preds[[t, spk_id]])
                .fold(0.0f32, f32::max);
            if max_activity > self.config.activity_threshold {
                active_speakers.push(spk_id);
            }
        }

        // VoiceReader patch: one extra instance for the speakers beyond the limit.
        let unassigned = self.unassigned_activity(&target_preds, &background_preds, frames_per_sec);
        if unassigned.is_some() {
            active_speakers.push(UNASSIGNED_SPEAKER_ID);
        }

        // Ensure speaker instances exist
        for &spk_id in &active_speakers {
            if !self.speakers.iter().any(|s| s.speaker_id == spk_id) {
                self.speakers.push(SpeakerInstance::new(spk_id));
            }
        }

        // Build encoder input chunk
        let expected_size = PRE_ENCODE_CACHE + chunk_size;
        let is_first_chunk = self.chunk_idx == 0;
        let main_start = processed_mel_frames;

        let mel_chunk = self.build_mel_chunk(&full_mel, main_start, is_first_chunk, expected_size)?;
        let chunk_length = expected_size;

        let chunk_frame_offset = self.chunk_idx * self.config.latency_mode.encoded_frames();
        let mut results = Vec::new();

        // For each active speaker, run encoder with speaker-specific masks
        for &spk_id in &active_speakers {
            // Derive spk_targets and bg_spk_targets from raw predictions
            // VoiceReader patch: the two masks come from differently held predictions,
            // and the unassigned instance has its own pair.
            let (spk_targets, bg_spk_targets) = match (&unassigned, spk_id) {
                (Some(activity), UNASSIGNED_SPEAKER_ID) => {
                    self.derive_speaker_targets(activity, 0, chunk_length)?
                }
                _ => {
                    let (spk_targets, _) =
                        self.derive_speaker_targets(diar_preds, spk_id, chunk_length)?;
                    let (_, bg_spk_targets) =
                        self.derive_speaker_targets(&background_preds, spk_id, chunk_length)?;
                    (spk_targets, bg_spk_targets)
                }
            };

            let spk_idx = self
                .speakers
                .iter()
                .position(|s| s.speaker_id == spk_id)
                .unwrap();

            // Run encoder with this speaker's targets and cache
            let (encoded, enc_len, new_cache) = self.model.run_encoder(
                &mel_chunk,
                chunk_length as i64,
                &self.speakers[spk_idx].encoder_cache,
                &spk_targets,
                &bg_spk_targets,
            )?;
            self.speakers[spk_idx].encoder_cache = new_cache;

            // Decode tokens for this speaker
            let tokens = self.decode_chunk_for_speaker(
                spk_idx,
                &encoded,
                enc_len as usize,
                chunk_frame_offset,
            )?;
            self.speakers[spk_idx].accumulated_tokens.extend(&tokens);

            // Build text delta and word timestamps for this chunk's tokens
            let mut text = String::new();
            for &(t, _) in &tokens {
                if t < VOCAB_SIZE {
                    text.push_str(&self.vocab.decode_single(t));
                }
            }

            if !text.is_empty() {
                let words = self.tokens_to_words(&tokens);
                results.push(SpeakerTranscript {
                    speaker_id: spk_id,
                    text,
                    words,
                });
            }
        }

        // Advance processed position
        self.audio_processed += chunk_size * HOP_LENGTH;
        self.chunk_idx += 1;

        // Trim audio buffer
        let keep_samples = (PRE_ENCODE_CACHE + chunk_size) * HOP_LENGTH + WIN_LENGTH;
        if self.audio_buffer.len() > keep_samples * 2 {
            let remove = self.audio_buffer.len() - keep_samples;
            let actual_remove = remove.min(self.audio_processed);
            self.audio_buffer.drain(0..actual_remove);
            self.audio_processed -= actual_remove;
        }

        Ok(results)
    }

    /// Non-streaming transcription of an audio file.
    pub fn transcribe_file_multitalker<P: AsRef<Path>>(
        &mut self,
        audio_path: P,
    ) -> Result<Vec<SpeakerTranscript>> {
        let (audio, spec) = crate::audio::load_audio(audio_path)?;

        if spec.sample_rate != SAMPLE_RATE as u32 {
            return Err(Error::Audio(format!(
                "Expected {} Hz, got {} Hz",
                SAMPLE_RATE, spec.sample_rate
            )));
        }

        let audio = if spec.channels > 1 {
            audio
                .chunks(spec.channels as usize)
                .map(|c| c.iter().sum::<f32>() / spec.channels as f32)
                .collect()
        } else {
            audio
        };

        self.transcribe_audio_multitalker(&audio)
    }

    /// Non-streaming transcription of raw audio samples.
    pub fn transcribe_audio_multitalker(
        &mut self,
        audio: &[f32],
    ) -> Result<Vec<SpeakerTranscript>> {
        self.reset();

        let audio_chunk_size = self.chunk_audio_samples();
        for chunk in audio.chunks(audio_chunk_size) {
            let chunk_vec = if chunk.len() < audio_chunk_size {
                let mut p = chunk.to_vec();
                p.resize(audio_chunk_size, 0.0);
                p
            } else {
                chunk.to_vec()
            };
            self.transcribe_chunk(&chunk_vec)?;
        }

        // Flush with silence
        let flush_chunk = vec![0.0f32; audio_chunk_size];
        for _ in 0..3 {
            self.transcribe_chunk(&flush_chunk)?;
        }

        Ok(self.get_transcripts())
    }

    // VoiceReader patch.
    /// Activity for the unassigned instance, as two columns: column 0 is its own
    /// activity, column 1 what the tracked speakers are doing (its background).
    /// `None` when the feature is off or nobody beyond the limit is speaking.
    fn unassigned_activity(
        &self,
        target_preds: &Array2<f32>,
        background_preds: &Array2<f32>,
        frames_per_sec: f32,
    ) -> Option<Array2<f32>> {
        if !self.config.transcribe_beyond_limit {
            return None;
        }
        let frames = target_preds.nrows();
        let tracked = self.config.max_speakers.min(target_preds.ncols());
        let mut activity = Array2::<f32>::zeros((frames, 2));
        let mut active_frames = 0usize;
        for t in 0..frames {
            let own = (tracked..target_preds.ncols())
                .map(|k| target_preds[[t, k]])
                .fold(0.0f32, f32::max);
            activity[[t, 0]] = own;
            activity[[t, 1]] = (0..tracked).map(|k| background_preds[[t, k]]).fold(0.0f32, f32::max);
            if own >= HOLD_ACTIVITY_LEVEL {
                active_frames += 1;
            }
        }
        let needed = (UNASSIGNED_MIN_SECS * frames_per_sec).round() as usize;
        (active_frames > 0 && active_frames >= needed).then_some(activity)
    }

    /// Derive per-speaker target masks from raw Sortformer predictions.
    ///
    /// For the target speaker k:
    /// - `spk_targets[t] = raw_preds[t, k]`
    /// - `bg_spk_targets[t] = max(raw_preds[t, j]) for j != k`
    ///
    /// The masks are resized/interpolated to match the encoder's time dimension.
    fn derive_speaker_targets(
        &self,
        diar_preds: &Array2<f32>,
        speaker_id: usize,
        encoder_time: usize,
    ) -> Result<(Array2<f32>, Array2<f32>)> {
        let diar_frames = diar_preds.nrows();

        let mut spk_vals = Vec::with_capacity(encoder_time);
        let mut bg_vals = Vec::with_capacity(encoder_time);

        for enc_t in 0..encoder_time {
            // Map encoder time to diarisation time (nearest-neighbour)
            let diar_t = if diar_frames > 0 && encoder_time > 0 {
                (enc_t * diar_frames / encoder_time).min(diar_frames - 1)
            } else {
                0
            };

            if diar_t < diar_frames && speaker_id < diar_preds.ncols() {
                let spk_val = diar_preds[[diar_t, speaker_id]];
                let bg_val = (0..diar_preds.ncols())
                    .filter(|&j| j != speaker_id)
                    .map(|j| diar_preds[[diar_t, j]])
                    .fold(0.0f32, f32::max);
                spk_vals.push(spk_val);
                bg_vals.push(bg_val);
            } else {
                // No diarisation data: assume single speaker
                spk_vals.push(1.0);
                bg_vals.push(0.0);
            }
        }

        let spk_targets = Array2::from_shape_vec((1, encoder_time), spk_vals)
            .map_err(|e| Error::Model(format!("spk_targets shape mismatch: {e}")))?;
        let bg_spk_targets = Array2::from_shape_vec((1, encoder_time), bg_vals)
            .map_err(|e| Error::Model(format!("bg_spk_targets shape mismatch: {e}")))?;

        Ok((spk_targets, bg_spk_targets))
    }

    fn build_mel_chunk(
        &self,
        full_mel: &Array2<f32>,
        main_start: usize,
        is_first_chunk: bool,
        expected_size: usize,
    ) -> Result<Array3<f32>> {
        let total_mel_frames = full_mel.shape()[1];
        let chunk_size = self.config.chunk_size();
        let mut chunk_data = vec![0.0f32; N_MELS * expected_size];

        if is_first_chunk {
            for f in 0..chunk_size.min(total_mel_frames) {
                for m in 0..N_MELS {
                    chunk_data[m * expected_size + PRE_ENCODE_CACHE + f] = full_mel[[m, f]];
                }
            }
        } else {
            let cache_start = main_start.saturating_sub(PRE_ENCODE_CACHE);
            let cache_frames = main_start - cache_start;
            let cache_offset = PRE_ENCODE_CACHE - cache_frames;

            for f in 0..cache_frames {
                for m in 0..N_MELS {
                    chunk_data[m * expected_size + cache_offset + f] =
                        full_mel[[m, cache_start + f]];
                }
            }

            for f in 0..chunk_size.min(total_mel_frames - main_start) {
                for m in 0..N_MELS {
                    chunk_data[m * expected_size + PRE_ENCODE_CACHE + f] =
                        full_mel[[m, main_start + f]];
                }
            }
        }

        Array3::from_shape_vec((1, N_MELS, expected_size), chunk_data)
            .map_err(|e| Error::Model(format!("Failed to create mel chunk: {e}")))
    }

    fn decode_chunk_for_speaker(
        &mut self,
        spk_idx: usize,
        encoder_out: &Array3<f32>,
        enc_frames: usize,
        chunk_frame_offset: usize,
    ) -> Result<Vec<(usize, usize)>> {
        let mut tokens = Vec::new();
        let hidden_dim = encoder_out.shape()[1];

        for t in 0..enc_frames {
            let frame = encoder_out.slice(s![0, .., t]).to_owned();
            let frame = frame
                .to_shape((1, 1, hidden_dim))
                .map_err(|e| Error::Model(format!("Failed to reshape frame: {e}")))?
                .to_owned();

            let absolute_frame = chunk_frame_offset + t;

            for _ in 0..MAX_SYMBOLS_PER_STEP {
                let (logits, new_state_1, new_state_2) = self.model.run_decoder(
                    &frame,
                    self.speakers[spk_idx].last_token,
                    &self.speakers[spk_idx].state_1,
                    &self.speakers[spk_idx].state_2,
                )?;

                let (max_idx, _) = crate::tensor_utils::argmax_f32(logits.iter().copied());

                if max_idx == BLANK_ID {
                    break;
                }

                tokens.push((max_idx, absolute_frame));
                self.speakers[spk_idx].last_token = max_idx as i32;
                self.speakers[spk_idx].state_1 = new_state_1;
                self.speakers[spk_idx].state_2 = new_state_2;
            }
        }

        Ok(tokens)
    }

    /// Convert (token_id, absolute_frame) pairs into word-level timestamps.
    fn tokens_to_words(&self, tokens: &[(usize, usize)]) -> Vec<WordTimestamp> {
        let timed: Vec<TimedToken> = tokens
            .iter()
            .filter(|(id, _)| *id < VOCAB_SIZE)
            .map(|&(id, frame)| TimedToken {
                text: self.vocab.decode_single(id),
                start: frame as f32 * SECONDS_PER_ENCODED_FRAME,
                end: (frame + 1) as f32 * SECONDS_PER_ENCODED_FRAME,
            })
            .collect();

        timestamps::group_by_words(&timed)
            .into_iter()
            .map(|t| WordTimestamp {
                word: t.text,
                start_secs: t.start,
                end_secs: t.end,
            })
            .collect()
    }

    /// Compute mel spectrogram using shared audio utilities.
    fn compute_mel_spectrogram(&self, audio: &[f32]) -> Result<Array2<f32>> {
        if audio.is_empty() {
            return Ok(Array2::zeros((N_MELS, 0)));
        }

        let preemph = crate::audio::apply_preemphasis(audio, PREEMPH);
        let spec = crate::audio::stft(&preemph, N_FFT, HOP_LENGTH, WIN_LENGTH)?;
        let mel = self.mel_basis.dot(&spec);

        Ok(mel.mapv(|x| (x.max(0.0) + LOG_ZERO_GUARD).ln()))
    }
}

// VoiceReader patch.
/// The speaker id of the extra instance that transcribes the speakers beyond
/// `max_speakers` together. It is one past the diariser's last slot.
pub const UNASSIGNED_SPEAKER_ID: usize = NUM_SPEAKERS;

// VoiceReader patch.
/// The unassigned instance only runs on a chunk with at least this much for it to hear.
const UNASSIGNED_MIN_SECS: f32 = 0.3;

// VoiceReader patch.
/// Activity at or above this counts as "speaking" for the hold.
const HOLD_ACTIVITY_LEVEL: f32 = 0.5;

// VoiceReader patch.
/// Returns `preds` with each speaker's activity kept at 1.0 for `hold_frames` frames
/// after it last reached [`HOLD_ACTIVITY_LEVEL`]. `left` carries the frames of hold
/// still owed to each speaker from one chunk to the next.
fn hold_activity(preds: &Array2<f32>, hold_frames: usize, left: &mut Vec<usize>) -> Array2<f32> {
    let mut held = preds.clone();
    if hold_frames == 0 {
        left.clear();
        return held;
    }
    if left.len() < held.ncols() {
        left.resize(held.ncols(), 0);
    }
    for spk in 0..held.ncols() {
        let mut remaining = left[spk];
        for t in 0..held.nrows() {
            if held[[t, spk]] >= HOLD_ACTIVITY_LEVEL {
                remaining = hold_frames;
            } else if remaining > 0 {
                remaining -= 1;
                held[[t, spk]] = 1.0;
            }
        }
        left[spk] = remaining;
    }
    held
}

#[cfg(test)]
mod hold_tests {
    use super::*;

    fn column(values: &[f32]) -> Array2<f32> {
        Array2::from_shape_vec((values.len(), 1), values.to_vec()).unwrap()
    }

    #[test]
    fn activity_is_held_after_the_speaker_stops() {
        let mut left = Vec::new();
        let held = hold_activity(&column(&[0.0, 0.9, 0.9, 0.1, 0.0, 0.0, 0.0]), 2, &mut left);
        assert_eq!(held.column(0).to_vec(), vec![0.0, 0.9, 0.9, 1.0, 1.0, 0.0, 0.0]);
        assert_eq!(left, vec![0]);
    }

    #[test]
    fn the_hold_carries_into_the_next_chunk() {
        let mut left = Vec::new();
        hold_activity(&column(&[0.9, 0.9, 0.0]), 3, &mut left);
        assert_eq!(left, vec![2]);
        let next = hold_activity(&column(&[0.0, 0.0, 0.0, 0.0]), 3, &mut left);
        assert_eq!(next.column(0).to_vec(), vec![1.0, 1.0, 0.0, 0.0]);
        assert_eq!(left, vec![0]);
    }

    #[test]
    fn no_hold_leaves_predictions_alone() {
        let mut left = vec![5];
        let preds = column(&[0.9, 0.2, 0.0]);
        assert_eq!(hold_activity(&preds, 0, &mut left), preds);
        assert!(left.is_empty());
    }
}

/// Implement the Transcriber trait for single-speaker fallback.
/// Runs with spk_targets=1.0 and bg_spk_targets=0.0 (no diarisation),
/// treating the multitalker encoder as a standard streaming ASR encoder.
impl Transcriber for MultitalkerASR {
    fn transcribe_samples(
        &mut self,
        audio: Vec<f32>,
        sample_rate: u32,
        channels: u16,
        _mode: Option<TimestampMode>,
    ) -> Result<TranscriptionResult> {
        if sample_rate != SAMPLE_RATE as u32 {
            return Err(Error::Audio(format!(
                "Expected {} Hz, got {} Hz",
                SAMPLE_RATE, sample_rate
            )));
        }

        let audio = if channels > 1 {
            audio
                .chunks(channels as usize)
                .map(|c| c.iter().sum::<f32>() / channels as f32)
                .collect()
        } else {
            audio
        };

        // Single-speaker mode: run encoder with full speaker activity
        self.reset();

        let mel = self.compute_mel_spectrogram(&audio)?;
        let total_frames = mel.shape()[1];

        if total_frames == 0 {
            return Ok(TranscriptionResult {
                text: String::new(),
                tokens: Vec::new(),
            });
        }

        // Create a single speaker instance
        self.speakers.push(SpeakerInstance::new(0));

        let chunk_size = self.config.chunk_size();
        let mut buffer_idx = 0;
        let mut chunk_idx = 0;

        while buffer_idx < total_frames {
            let expected_size = PRE_ENCODE_CACHE + chunk_size;

            let is_first = chunk_idx == 0;
            let mel_chunk = self.build_mel_chunk(&mel, buffer_idx, is_first, expected_size)?;
            // Use expected_size consistently (matches transcribe_chunk path)
            let chunk_length = expected_size;

            // Single-speaker: full activity, no background
            let spk_targets = Array2::from_elem((1, chunk_length), 1.0f32);
            let bg_spk_targets = Array2::from_elem((1, chunk_length), 0.0f32);

            let (encoded, enc_len, new_cache) = self.model.run_encoder(
                &mel_chunk,
                chunk_length as i64,
                &self.speakers[0].encoder_cache,
                &spk_targets,
                &bg_spk_targets,
            )?;
            self.speakers[0].encoder_cache = new_cache;

            let chunk_frame_offset =
                chunk_idx * self.config.latency_mode.encoded_frames();
            let tokens =
                self.decode_chunk_for_speaker(0, &encoded, enc_len as usize, chunk_frame_offset)?;
            self.speakers[0].accumulated_tokens.extend(tokens);

            buffer_idx += chunk_size;
            chunk_idx += 1;
        }

        let valid_ids: Vec<usize> = self.speakers[0]
            .accumulated_tokens
            .iter()
            .filter(|&&(t, _)| t < VOCAB_SIZE)
            .map(|&(t, _)| t)
            .collect();

        let text = self.vocab.decode(&valid_ids);

        Ok(TranscriptionResult {
            text,
            tokens: Vec::new(),
        })
    }
}
