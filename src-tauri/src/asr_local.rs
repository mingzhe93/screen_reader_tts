//! Speech to text with speaker labels: the multitalker Parakeet model driven by the
//! Nemotron-3 diarizer, through the `parakeet-rs` crate on the shared ONNX Runtime.
//!
//! English only. The model is loaded for one transcription and dropped afterwards, so
//! its memory (about 1.3 GB) is only held while a recording is being transcribed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use parakeet_rs::multitalker::UNASSIGNED_SPEAKER_ID;
use parakeet_rs::{ExecutionConfig, MultitalkerASR, Transcriber};

use crate::audio8_local::ensure_onnxruntime;
use crate::audio_capture::MicrophoneSource;
use crate::audio8_model::load_wav_mono;
use crate::audio_decode::AudioFileReader;

pub(crate) const ASR_REPO: &str = "Recogment/parakeet-multitalker-int8-onnx";
/// Smallest files first, so a wrong repo or a network problem shows up early.
pub(crate) const ASR_MODEL_FILES: [&str; 5] = [
    "LICENSE.txt",
    "NOTICE.txt",
    "tokenizer.model",
    "decoder_joint.int8.onnx",
    "encoder.int8.onnx",
];
pub(crate) const DIARIZER_REPO: &str = "altunenes/parakeet-rs";
pub(crate) const DIARIZER_MODEL_FILES: [&str; 2] = [
    "nemotron-3-diarization/LICENSE",
    "nemotron-3-diarization/nemotron3_diar_v3.onnx",
];
const DIARIZER_MODEL_PATH: &str = "nemotron-3-diarization/nemotron3_diar_v3.onnx";

const SAMPLE_RATE: u32 = 16_000;
/// The diarizer has eight speaker slots; the speech model was trained on up to four.
pub(crate) const MAX_SPEAKERS_LIMIT: usize = 8;
/// All eight by default, so that as many people as possible get their own label.
/// Unused slots cost nothing, because only speakers who are talking are processed.
pub(crate) const DEFAULT_MAX_SPEAKERS: usize = 8;
/// The speaker number of turns spoken by people beyond the limit. They are transcribed
/// together and shown as "Unknown speaker" rather than being left out.
pub(crate) const UNKNOWN_SPEAKER: usize = UNASSIGNED_SPEAKER_ID;
const THREADS_ENV: &str = "VOICEREADER_ASR_THREADS";

/// How long a speaker stays "on" for the model after the diarizer says they stopped,
/// and how long they stay visible to the other speakers as background. Without the
/// hold, the last words before every pause or speaker change are lost, because the
/// model emits a word slightly after it was spoken. Chosen by test: shorter speaker
/// holds still lost words, and a background hold as long as the speaker hold made the
/// next speaker lose their first words.
const SPEAKER_HOLD_SECS: f32 = 0.8;
const BACKGROUND_HOLD_SECS: f32 = 0.6;

/// A pause this long inside one speaker's speech starts a new turn.
const TURN_PAUSE_SECS: f32 = 1.5;
/// A turn longer than this is closed at the next sentence end, to keep paragraphs readable.
const TURN_SOFT_MAX_CHARS: usize = 360;
/// Text that continues a word joins the earlier turn, unless that turn is this stale.
const TURN_STALE_SECS: f32 = 5.0;

fn repo_dir(models_dir: &Path, repo: &str) -> PathBuf {
    repo.split('/').fold(models_dir.to_path_buf(), |path, part| path.join(part))
}

pub(crate) fn asr_model_dir(models_dir: &Path) -> PathBuf {
    repo_dir(models_dir, ASR_REPO)
}

pub(crate) fn diarizer_model_dir(models_dir: &Path) -> PathBuf {
    repo_dir(models_dir, DIARIZER_REPO)
}

fn repo_file(model_dir: &Path, file: &str) -> PathBuf {
    file.split('/').fold(model_dir.to_path_buf(), |path, part| path.join(part))
}

/// True when every file of both models is present under `models_dir`.
pub(crate) fn asr_models_downloaded(models_dir: &Path) -> bool {
    let asr_dir = asr_model_dir(models_dir);
    let diarizer_dir = diarizer_model_dir(models_dir);
    ASR_MODEL_FILES.iter().all(|file| repo_file(&asr_dir, file).is_file())
        && DIARIZER_MODEL_FILES.iter().all(|file| repo_file(&diarizer_dir, file).is_file())
}

/// One stretch of speech by one speaker.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TranscriptTurn {
    pub id: usize,
    /// Zero-based, in order of first appearance in the recording, or [`UNKNOWN_SPEAKER`].
    pub speaker: usize,
    pub start_secs: f32,
    pub end_secs: f32,
    pub text: String,
}

/// Groups the text the model emits chunk by chunk into turns.
#[derive(Default)]
pub(crate) struct TurnBuilder {
    turns: Vec<TranscriptTurn>,
    open: HashMap<usize, usize>,
}

impl TurnBuilder {
    /// Adds text for `speaker` and returns the turn it went into. `delta` is the text
    /// as the model produced it: it starts with a space when it begins a new word.
    pub(crate) fn push(
        &mut self,
        speaker: usize,
        delta: &str,
        start_secs: f32,
        end_secs: f32,
    ) -> Option<&TranscriptTurn> {
        if delta.trim().is_empty() {
            return None;
        }
        let starts_word = delta.starts_with(char::is_whitespace);
        let continues = self.open.get(&speaker).copied().filter(|&index| {
            let turn = &self.turns[index];
            if !starts_word {
                // The rest of a word, or punctuation, belongs to what came before.
                return start_secs - turn.end_secs <= TURN_STALE_SECS;
            }
            let paused = start_secs - turn.end_secs > TURN_PAUSE_SECS;
            // Someone else began speaking after this speaker stopped: the reply should
            // read as a new turn, not be glued onto the old one above the other speaker.
            let answered = self.turns[index + 1..]
                .iter()
                .any(|other| other.speaker != speaker && other.start_secs >= turn.end_secs);
            let long_enough = turn.text.len() >= TURN_SOFT_MAX_CHARS && turn.text.ends_with(['.', '?', '!']);
            !(paused || answered || long_enough)
        });

        let index = match continues {
            Some(index) => {
                let turn = &mut self.turns[index];
                turn.text.push_str(delta);
                turn.end_secs = turn.end_secs.max(end_secs);
                index
            }
            None => {
                // A new turn never opens with punctuation left over from an old one.
                let text = delta.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ',' | '.' | ';' | ':'));
                if text.is_empty() {
                    return None;
                }
                let index = self.turns.len();
                self.turns.push(TranscriptTurn {
                    id: index,
                    speaker,
                    start_secs,
                    end_secs,
                    text: text.to_string(),
                });
                self.open.insert(speaker, index);
                index
            }
        };
        self.turns.get(index)
    }

    /// The turns in the order they should be read.
    pub(crate) fn into_turns(self) -> Vec<TranscriptTurn> {
        let mut turns = self.turns;
        turns.sort_by(|a, b| a.start_secs.total_cmp(&b.start_secs).then(a.id.cmp(&b.id)));
        turns
    }
}

pub(crate) enum TranscribeEvent<'a> {
    /// The model is loaded and the recording is about to be processed.
    Started { total_secs: Option<f32> },
    /// A turn was added or extended.
    Turn(&'a TranscriptTurn),
    Progress { processed_secs: f32, total_secs: Option<f32> },
    /// Loudest sample (0 to 1) in the audio just captured. Live sources only.
    Level(f32),
}

pub(crate) struct TranscribeSummary {
    pub turns: Vec<TranscriptTurn>,
    pub audio_secs: f32,
    pub elapsed_secs: f32,
    pub cancelled: bool,
    /// True for a live recording, false for a file.
    pub live: bool,
    /// Seconds of speech from speakers beyond the limit, shown as the unknown speaker.
    pub unknown_speaker_secs: f32,
}

/// Where the audio to transcribe comes from.
enum AudioSource {
    File(AudioFileReader),
    Microphone(MicrophoneSource),
}

impl AudioSource {
    /// Appends the next piece of 16 kHz mono audio. Returns `false` at the end.
    fn read_into(&mut self, output: &mut Vec<f32>) -> Result<bool> {
        match self {
            Self::File(reader) => reader.read_into(output).context("Failed to read the recording"),
            Self::Microphone(microphone) => microphone.read_into(output),
        }
    }

    fn is_live(&self) -> bool {
        matches!(self, Self::Microphone(_))
    }
}

fn default_threads() -> usize {
    if let Some(threads) = std::env::var(THREADS_ENV).ok().and_then(|value| value.trim().parse::<usize>().ok()) {
        return threads.clamp(1, 64);
    }
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    // Measured on a 16-core / 32-thread CPU: 8 threads were the fastest (6.6x real
    // time), 16 were slower. `cores` counts hardware threads, hence the halving.
    (cores / 2).clamp(1, 8)
}

fn load_model(models_dir: &Path, max_speakers: usize) -> Result<MultitalkerASR> {
    if !asr_models_downloaded(models_dir) {
        return Err(anyhow!("The transcription model is not downloaded yet."));
    }
    ensure_onnxruntime()?;
    let diarizer_path = repo_file(&diarizer_model_dir(models_dir), DIARIZER_MODEL_PATH);
    // CPU on purpose: on an RTX 5090, DirectML was no faster than 4 CPU threads for this
    // pipeline (4.7x against 5.1x real time), because the speech encoder is int8.
    let config = ExecutionConfig::new().with_intra_threads(default_threads());
    let mut model = MultitalkerASR::from_pretrained(asr_model_dir(models_dir), &diarizer_path, Some(config))
        .map_err(|err| anyhow!("Failed to load the transcription model: {err}"))?;
    model.set_max_speakers(max_speakers.clamp(1, MAX_SPEAKERS_LIMIT));
    model.set_speaker_hold(SPEAKER_HOLD_SECS, BACKGROUND_HOLD_SECS);
    model.set_transcribe_beyond_limit(true);
    Ok(model)
}

/// Silence added after a clip, so the model gets to emit the last words.
const CLIP_TAIL_SECS: f32 = 1.5;

/// Transcribes a short WAV clip of one person, such as a voice-cloning reference, and
/// returns the text. Speaker labelling is skipped: the whole clip is treated as one
/// voice, which is the model at its most accurate.
pub(crate) fn transcribe_clip(models_dir: &Path, wav_bytes: &[u8]) -> Result<String> {
    // Decode first, so a bad file fails before the model loads.
    let mut audio = load_wav_mono(wav_bytes, SAMPLE_RATE)?;
    audio.extend(std::iter::repeat(0.0).take((CLIP_TAIL_SECS * SAMPLE_RATE as f32) as usize));
    let mut model = load_model(models_dir, 1)?;
    let result = model
        .transcribe_samples(audio, SAMPLE_RATE, 1, None)
        .map_err(|err| anyhow!("Transcription failed: {err}"))?;
    Ok(result.text.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Transcribes the audio file at `audio_path`. `on_event` receives turns as they grow,
/// so the caller can show the transcript while it is being produced. Setting `cancel`
/// abandons the rest of the file.
pub(crate) fn transcribe_file(
    models_dir: &Path,
    audio_path: &Path,
    max_speakers: usize,
    cancel: &AtomicBool,
    mut on_event: impl FnMut(TranscribeEvent<'_>),
) -> Result<TranscribeSummary> {
    let started = Instant::now();
    // The file is opened first, so an unreadable one fails before the model loads.
    let reader = AudioFileReader::open(audio_path, SAMPLE_RATE)?;
    let total_secs = reader.duration_secs();
    let model = load_model(models_dir, max_speakers)?;
    on_event(TranscribeEvent::Started { total_secs });
    run(model, AudioSource::File(reader), total_secs, cancel, started, on_event)
}

/// Transcribes what the microphone hears until `stop` is set, then finishes the words
/// still in progress. `device_name` picks an input device; `None` is the system default.
/// Must run on one thread from start to end, because the capture stream cannot move.
pub(crate) fn transcribe_microphone(
    models_dir: &Path,
    device_name: Option<&str>,
    max_speakers: usize,
    stop: Arc<AtomicBool>,
    mut on_event: impl FnMut(TranscribeEvent<'_>),
) -> Result<TranscribeSummary> {
    // The model loads first, so the first words are not captured while it is loading
    // and then transcribed late.
    let model = load_model(models_dir, max_speakers)?;
    let microphone = MicrophoneSource::open(device_name, SAMPLE_RATE, stop)?;
    let started = Instant::now();
    on_event(TranscribeEvent::Started { total_secs: None });
    // A recording ends through `stop`, which lets the last words through; it is never cut off.
    let never = AtomicBool::new(false);
    run(model, AudioSource::Microphone(microphone), None, &never, started, on_event)
}

fn run(
    mut model: MultitalkerASR,
    mut source: AudioSource,
    total_secs: Option<f32>,
    cancel: &AtomicBool,
    started: Instant,
    mut on_event: impl FnMut(TranscribeEvent<'_>),
) -> Result<TranscribeSummary> {
    let live = source.is_live();
    let chunk_samples = model.chunk_audio_samples();
    let mut turns = TurnBuilder::default();
    let mut pending: Vec<f32> = Vec::new();
    let mut processed_samples = 0usize;
    let mut cancelled = false;

    let mut more = true;
    while more && !cancelled {
        let before = pending.len();
        more = source.read_into(&mut pending)?;
        if live && pending.len() > before {
            let peak = pending[before..].iter().fold(0.0f32, |peak, sample| peak.max(sample.abs()));
            on_event(TranscribeEvent::Level(peak.min(1.0)));
        }
        let mut offset = 0usize;
        while pending.len() - offset >= chunk_samples {
            if cancel.load(Ordering::SeqCst) {
                cancelled = true;
                break;
            }
            run_chunk(&mut model, &pending[offset..offset + chunk_samples], &mut turns, &mut on_event)?;
            offset += chunk_samples;
            processed_samples += chunk_samples;
            on_event(TranscribeEvent::Progress {
                processed_secs: processed_samples as f32 / SAMPLE_RATE as f32,
                total_secs,
            });
        }
        pending.drain(..offset);
        cancelled = cancelled || cancel.load(Ordering::SeqCst);
    }

    if !cancelled {
        // The last partial chunk, then silence so the model emits the words it was
        // still holding back.
        processed_samples += pending.len();
        pending.resize(chunk_samples, 0.0);
        run_chunk(&mut model, &pending, &mut turns, &mut on_event)?;
        let silence = vec![0.0f32; chunk_samples];
        for _ in 0..3 {
            run_chunk(&mut model, &silence, &mut turns, &mut on_event)?;
        }
        on_event(TranscribeEvent::Progress {
            processed_secs: processed_samples as f32 / SAMPLE_RATE as f32,
            total_secs,
        });
    }

    Ok(TranscribeSummary {
        turns: turns.into_turns(),
        audio_secs: processed_samples as f32 / SAMPLE_RATE as f32,
        elapsed_secs: started.elapsed().as_secs_f32(),
        cancelled,
        live,
        unknown_speaker_secs: model.speech_beyond_limit_secs(),
    })
}

fn run_chunk(
    model: &mut MultitalkerASR,
    chunk: &[f32],
    turns: &mut TurnBuilder,
    on_event: &mut impl FnMut(TranscribeEvent<'_>),
) -> Result<()> {
    let results = model
        .transcribe_chunk(chunk)
        .map_err(|err| anyhow!("Transcription failed: {err}"))?;
    for result in &results {
        let (Some(first), Some(last)) = (result.words.first(), result.words.last()) else {
            continue;
        };
        if let Some(turn) = turns.push(result.speaker_id, &result.text, first.start_secs, last.end_secs) {
            on_event(TranscribeEvent::Turn(turn));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deltas_from_one_speaker_join_into_one_turn() {
        let mut turns = TurnBuilder::default();
        turns.push(0, " If the red of the second bow f", 1.4, 3.3);
        turns.push(0, "alls upon the green", 3.4, 4.2);
        turns.push(0, ", the result", 5.6, 5.8);
        let turns = turns.into_turns();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].text, "If the red of the second bow falls upon the green, the result");
        assert_eq!(turns[0].start_secs, 1.4);
        assert_eq!(turns[0].end_secs, 5.8);
    }

    #[test]
    fn a_long_pause_starts_a_new_turn() {
        let mut turns = TurnBuilder::default();
        turns.push(0, " First thought.", 0.0, 1.0);
        turns.push(0, " Second thought.", 4.0, 5.0);
        // Punctuation that arrives late still belongs to the earlier words.
        turns.push(0, "!", 9.0, 9.1);
        let turns = turns.into_turns();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[1].text, "Second thought.!");
    }

    #[test]
    fn a_reply_from_someone_else_splits_the_turn() {
        let mut turns = TurnBuilder::default();
        turns.push(0, " Are we on track?", 0.0, 1.2);
        turns.push(1, " Yes.", 1.4, 1.8);
        turns.push(0, " Good.", 2.0, 2.4);
        let turns = turns.into_turns();
        let read: Vec<(usize, &str)> = turns.iter().map(|turn| (turn.speaker, turn.text.as_str())).collect();
        assert_eq!(read, vec![(0, "Are we on track?"), (1, "Yes."), (0, "Good.")]);
    }

    #[test]
    fn talking_over_someone_does_not_split_their_turn() {
        let mut turns = TurnBuilder::default();
        turns.push(0, " So the plan is to ship", 0.0, 2.0);
        turns.push(1, " Right.", 1.0, 1.4);
        turns.push(0, " on Friday.", 2.1, 2.8);
        let turns = turns.into_turns();
        let read: Vec<(usize, &str)> = turns.iter().map(|turn| (turn.speaker, turn.text.as_str())).collect();
        assert_eq!(read, vec![(0, "So the plan is to ship on Friday."), (1, "Right.")]);
    }

    #[test]
    fn long_turns_break_at_a_sentence_end() {
        let mut turns = TurnBuilder::default();
        let sentence = " This sentence is here to make the turn long enough to be split.";
        let mut time = 0.0;
        for _ in 0..8 {
            turns.push(0, sentence, time, time + 1.0);
            time += 1.1;
        }
        let turns = turns.into_turns();
        assert!(turns.len() >= 2);
        assert!(turns.iter().all(|turn| turn.text.len() < TURN_SOFT_MAX_CHARS + sentence.len()));
        assert!(turns.iter().all(|turn| turn.text.ends_with('.')));
    }

    #[test]
    fn late_punctuation_does_not_open_a_turn() {
        let mut turns = TurnBuilder::default();
        turns.push(0, " We stopped here", 0.0, 1.0);
        assert!(turns.push(0, ".", 20.0, 20.1).is_none());
        turns.push(0, ", and then carried on", 30.0, 31.0);
        let turns = turns.into_turns();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[1].text, "and then carried on");
    }

    #[test]
    fn empty_text_is_ignored() {
        let mut turns = TurnBuilder::default();
        assert!(turns.push(0, "  ", 0.0, 0.1).is_none());
        assert!(turns.into_turns().is_empty());
    }

    /// Transcribes a real recording. Run with:
    /// `VOICEREADER_ASR_TEST_MODELS_DIR=<models dir> VOICEREADER_ASR_TEST_AUDIO=<file>
    ///  cargo test --release --features build-base --lib -- --ignored --nocapture transcribes`
    #[test]
    #[ignore = "needs the downloaded transcription models and a recording"]
    fn transcribes_a_recording() {
        let models_dir = PathBuf::from(std::env::var("VOICEREADER_ASR_TEST_MODELS_DIR").expect("set VOICEREADER_ASR_TEST_MODELS_DIR"));
        let audio = PathBuf::from(std::env::var("VOICEREADER_ASR_TEST_AUDIO").expect("set VOICEREADER_ASR_TEST_AUDIO"));
        let cancel = AtomicBool::new(false);
        let mut turn_updates = 0usize;
        let mut last_progress = 0.0f32;
        // `VOICEREADER_ASR_TEST_MAX_SPEAKERS` tries a lower limit on a recording with many speakers.
        let max_speakers = std::env::var("VOICEREADER_ASR_TEST_MAX_SPEAKERS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_MAX_SPEAKERS);
        let summary = transcribe_file(&models_dir, &audio, max_speakers, &cancel, |event| match event {
            TranscribeEvent::Turn(_) => turn_updates += 1,
            TranscribeEvent::Progress { processed_secs, .. } => last_progress = processed_secs,
            TranscribeEvent::Started { .. } | TranscribeEvent::Level(_) => {}
        })
        .unwrap();
        for turn in &summary.turns {
            let speaker = if turn.speaker == UNKNOWN_SPEAKER {
                "unknown speaker".to_string()
            } else {
                format!("speaker {}", turn.speaker + 1)
            };
            println!("[{:6.1} - {:6.1}] {speaker}: {}", turn.start_secs, turn.end_secs, turn.text);
        }
        println!(
            "{:.1}s of audio in {:.1}s ({:.1}x real time), {:.1}s from speakers beyond the limit of {max_speakers}",
            summary.audio_secs,
            summary.elapsed_secs,
            summary.audio_secs / summary.elapsed_secs,
            summary.unknown_speaker_secs
        );
        // Nobody is left out: speech beyond the limit must come back as the unknown speaker.
        let has_unknown = summary.turns.iter().any(|turn| turn.speaker == UNKNOWN_SPEAKER);
        assert!(!has_unknown || summary.unknown_speaker_secs > 0.0, "unknown-speaker turns without any such speech");
        assert!(summary.turns.iter().all(|turn| turn.speaker < max_speakers || turn.speaker == UNKNOWN_SPEAKER));
        assert!(!summary.cancelled);
        assert!(!summary.turns.is_empty());
        assert!(turn_updates >= summary.turns.len());
        assert!(last_progress > 0.0);
        assert!(summary.turns.iter().all(|turn| !turn.text.trim().is_empty()));

        cancel.store(true, Ordering::SeqCst);
        let cancelled = transcribe_file(&models_dir, &audio, DEFAULT_MAX_SPEAKERS, &cancel, |_| {}).unwrap();
        assert!(cancelled.cancelled);
        assert!(cancelled.turns.is_empty());
    }

    /// Transcribes a WAV clip as one speaker. Run with:
    /// `VOICEREADER_ASR_TEST_MODELS_DIR=<models dir> VOICEREADER_ASR_TEST_AUDIO=<wav file>
    ///  cargo test --release --features build-base --lib -- --ignored --nocapture transcribes_a_clip`
    #[test]
    #[ignore = "needs the downloaded transcription models and a WAV recording"]
    fn transcribes_a_clip() {
        let models_dir = PathBuf::from(std::env::var("VOICEREADER_ASR_TEST_MODELS_DIR").expect("set VOICEREADER_ASR_TEST_MODELS_DIR"));
        let audio = std::fs::read(std::env::var("VOICEREADER_ASR_TEST_AUDIO").expect("set VOICEREADER_ASR_TEST_AUDIO")).unwrap();
        let text = transcribe_clip(&models_dir, &audio).unwrap();
        println!("clip: {text}");
        assert!(text.split(' ').count() >= 3, "too little text: {text:?}");
        assert!(!text.contains("  ") && text == text.trim());
        assert!(transcribe_clip(&models_dir, b"not a wav file").is_err());
    }

    /// Listens to the default microphone for a few seconds, then stops. Run with:
    /// `VOICEREADER_ASR_TEST_MODELS_DIR=<models dir>
    ///  cargo test --release --features build-base --lib -- --ignored --nocapture transcribes_the_microphone`
    #[test]
    #[ignore = "needs the downloaded transcription models and a microphone"]
    fn transcribes_the_microphone() {
        let models_dir = PathBuf::from(std::env::var("VOICEREADER_ASR_TEST_MODELS_DIR").expect("set VOICEREADER_ASR_TEST_MODELS_DIR"));
        let stop = Arc::new(AtomicBool::new(false));
        if crate::audio_capture::list_inputs().is_empty() {
            // The path the app takes with no microphone: the model loads, the capture
            // fails, and the caller gets an error to show.
            println!("no microphone connected; checking the error path only");
            let error = transcribe_microphone(&models_dir, None, DEFAULT_MAX_SPEAKERS, stop, |_| {})
                .err()
                .expect("recording without a microphone must fail");
            assert!(error.to_string().contains("No microphone"), "{error}");
            return;
        }
        let stopper = stop.clone();
        let mut levels = 0usize;
        let mut started_at = None;
        let summary = transcribe_microphone(&models_dir, None, DEFAULT_MAX_SPEAKERS, stop, |event| match event {
            TranscribeEvent::Started { total_secs } => {
                assert!(total_secs.is_none());
                started_at = Some(Instant::now());
            }
            TranscribeEvent::Level(level) => {
                assert!((0.0..=1.0).contains(&level));
                levels += 1;
                if started_at.is_some_and(|started| started.elapsed().as_secs_f32() > 5.0) {
                    stopper.store(true, Ordering::SeqCst);
                }
            }
            TranscribeEvent::Turn(turn) => println!("speaker {}: {}", turn.speaker + 1, turn.text),
            TranscribeEvent::Progress { .. } => {}
        })
        .unwrap();
        println!(
            "{:.1}s of audio in {:.1}s, {levels} level updates, {} turns",
            summary.audio_secs,
            summary.elapsed_secs,
            summary.turns.len()
        );
        assert!(summary.live);
        assert!(!summary.cancelled);
        assert!(levels > 10);
        assert!(summary.audio_secs > 2.0 && summary.audio_secs < 8.0, "{} s", summary.audio_secs);
    }
}
