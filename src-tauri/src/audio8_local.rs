//! App-side runtime for Audio8 TTS: voice storage, text chunking, and streaming PCM
//! through the same SoX tempo pipeline the Kyutai runtime uses.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::audio8_model::{
    benchmark_decoder, clean_text, init_onnxruntime, load_wav_mono, max_frames_for_text, read_codes_npy,
    write_codes_npy, Audio8Model, ChunkPlan, DecoderDevice, GenParams, VoicePrefix,
};
use crate::audio_pipeline::RateEmitter;
use crate::bundled_paths::{find_bundled_file, search_roots};
use crate::kyutai_local::{LocalJobEndState, SavedVoiceMeta, META_FILE_NAME, REF_AUDIO_FILE_NAME};
use crate::text_chunking::{chunk_text, normalize_for_speech};

pub const AUDIO8_REPO: &str = "Edge0/audio8-TTS-0.1B-ONNX-INT8";
/// Everything the runtime needs from the Hugging Face repo, including voice registration.
pub const AUDIO8_MODEL_FILES: [&str; 12] = [
    "runtime_manifest.json",
    "reference_codes.npy",
    "tokenizer/tokenizer.json",
    "slow_ar_int8.onnx",
    "slow_ar_int8.onnx.data",
    "fast_ar_int8.onnx",
    "fast_ar_int8.onnx.data",
    "codec_decoder_fp16.onnx",
    "codec_decoder_fp16.onnx.data",
    "registration/registration_manifest.json",
    "registration/codec_encoder_fp16.onnx",
    "registration/codec_encoder_fp16.onnx.data",
];

const BUILTIN_VOICE_ID: &str = "0";
const VOICE_CODES_FILE_NAME: &str = "audio8_codes.npy";
/// Voice prefixes are ~9 MB each; keep only the most recently used few.
const MAX_CACHED_PREFIXES: usize = 4;
/// Unit budgets (roughly English characters) for the first chunks; later chunks use the
/// user's chunk size setting. A shorter first chunk means fewer prompt tokens before the
/// first audio frame (about 0.4 s sooner than a full-size chunk), but a first budget
/// below ~80 cuts most opening sentences in two for no further gain.
const RAMP_CHUNK_BUDGETS: [f32; 2] = [80.0, 120.0];
const MIN_CHUNK_BUDGET: f32 = 100.0;
/// ~11 s of speech. Longer chunks slow every generated frame and delay cancellation.
const MAX_CHUNK_BUDGET: f32 = 160.0;
const MIN_FIRST_FRAMES: usize = 12;
/// Environment overrides for the Audio8 thread layout (see `LocalAudio8Runtime::new`).
const PARALLEL_CHUNKS_ENV: &str = "VOICEREADER_AUDIO8_PARALLEL_CHUNKS";
const DECODERS_ENV: &str = "VOICEREADER_AUDIO8_DECODERS";
const DECODER_THREADS_ENV: &str = "VOICEREADER_AUDIO8_DECODER_THREADS";
/// Overrides the Compute Device setting for testing: `auto`, `gpu` or `cpu`.
const DECODER_DEVICE_ENV: &str = "VOICEREADER_AUDIO8_DECODER_DEVICE";
const DECODER_DEVICE_CACHE_FILE: &str = "audio8-decoder-device.json";
/// Benchmarking takes a few seconds, so its result is reused for this long. Hardware
/// rarely changes; deleting the cache file forces a new benchmark.
const DECODER_DEVICE_CACHE_SECONDS: u64 = 7 * 24 * 60 * 60;
/// A GPU is only used if it beats the CPU by this factor. Small integrated GPUs can be
/// several times slower than the CPU for this decoder.
const GPU_MIN_SPEEDUP: f32 = 1.5;

/// The user's Compute Device setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComputePreference {
    /// Use the GPU when one is available and measures faster than the CPU.
    Auto,
    /// Use the GPU whenever its provider loads, even if it is the slower option.
    Gpu,
    /// Never use the GPU, for example to leave it free for other work.
    Cpu,
}

impl ComputePreference {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "gpu" => Some(Self::Gpu),
            "cpu" => Some(Self::Cpu),
            _ => None,
        }
    }
}

/// Whether this platform has a GPU provider at all (it may still fail to load).
pub fn gpu_provider_available() -> bool {
    forced_gpu_device().is_some()
}

/// The GPU provider benchmarked automatically on this platform, if any.
fn auto_gpu_device() -> Option<DecoderDevice> {
    if cfg!(target_os = "windows") {
        Some(DecoderDevice::DirectMl)
    } else {
        // Core ML is wired up but has not been tested on a Mac, so it is opt-in
        // (VOICEREADER_AUDIO8_DECODER_DEVICE=gpu) until it has.
        None
    }
}

fn forced_gpu_device() -> Option<DecoderDevice> {
    if cfg!(target_os = "windows") {
        Some(DecoderDevice::DirectMl)
    } else if cfg!(target_os = "macos") {
        Some(DecoderDevice::CoreMl)
    } else {
        None
    }
}

#[derive(Serialize, Deserialize)]
struct DecoderDeviceCache {
    device: String,
    note: String,
    checked_at: u64,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn device_from_label(label: &str) -> Option<DecoderDevice> {
    [DecoderDevice::Cpu, DecoderDevice::DirectMl, DecoderDevice::CoreMl]
        .into_iter()
        .find(|device| device.label() == label)
}

/// Picks between CPU and GPU from two decode times (milliseconds for the same input).
fn faster_device(cpu_ms: f32, gpu: DecoderDevice, gpu_ms: f32) -> DecoderDevice {
    if gpu_ms * GPU_MIN_SPEEDUP < cpu_ms {
        gpu
    } else {
        DecoderDevice::Cpu
    }
}

fn store_decoder_device(data_dir: &Path, device: DecoderDevice, note: &str) {
    let cache = DecoderDeviceCache {
        device: device.label().to_string(),
        note: note.to_string(),
        checked_at: unix_now(),
    };
    if let Ok(body) = serde_json::to_string_pretty(&cache) {
        let _ = std::fs::write(data_dir.join(DECODER_DEVICE_CACHE_FILE), body);
    }
}

/// Decides where the codec decoder runs. Returns the device, a note for Engine Health,
/// and whether the choice came from the automatic benchmark.
fn choose_decoder_device(
    model_dir: &Path,
    data_dir: &Path,
    cpu_threads: usize,
    preference: ComputePreference,
) -> (DecoderDevice, String, bool) {
    let (preference, source) = match std::env::var(DECODER_DEVICE_ENV)
        .ok()
        .and_then(|value| ComputePreference::parse(&value))
    {
        Some(overridden) => (overridden, format!("set by {DECODER_DEVICE_ENV}")),
        None => (preference, "chosen in Compute Device".to_string()),
    };
    match preference {
        ComputePreference::Cpu => return (DecoderDevice::Cpu, source, false),
        ComputePreference::Gpu => {
            return match forced_gpu_device() {
                Some(device) => (device, source, false),
                None => (DecoderDevice::Cpu, "no GPU provider on this platform".to_string(), false),
            };
        }
        ComputePreference::Auto => {}
    }
    let Some(gpu) = auto_gpu_device() else {
        return (
            DecoderDevice::Cpu,
            "auto: no automatic GPU provider on this platform".to_string(),
            false,
        );
    };

    let cached = std::fs::read_to_string(data_dir.join(DECODER_DEVICE_CACHE_FILE))
        .ok()
        .and_then(|body| serde_json::from_str::<DecoderDeviceCache>(&body).ok())
        .filter(|cache| unix_now().saturating_sub(cache.checked_at) < DECODER_DEVICE_CACHE_SECONDS);
    if let Some(cache) = cached {
        if let Some(device) = device_from_label(&cache.device).filter(|device| *device == DecoderDevice::Cpu || *device == gpu) {
            return (device, format!("auto: {} (cached)", cache.note), true);
        }
    }

    let (device, note) = match benchmark_decoder(model_dir, gpu, cpu_threads) {
        Err(err) => (DecoderDevice::Cpu, format!("{} unavailable: {err:#}", gpu.label())),
        Ok(gpu_time) => match benchmark_decoder(model_dir, DecoderDevice::Cpu, cpu_threads) {
            // Without a CPU figure to compare against, stay on the known-good path.
            Err(err) => (DecoderDevice::Cpu, format!("cpu benchmark failed: {err:#}")),
            Ok(cpu_time) => {
                let (cpu_ms, gpu_ms) = (cpu_time.as_secs_f32() * 1000.0, gpu_time.as_secs_f32() * 1000.0);
                (
                    faster_device(cpu_ms, gpu, gpu_ms),
                    format!("benchmark: cpu {cpu_ms:.0} ms, {} {gpu_ms:.0} ms", gpu.label()),
                )
            }
        },
    };
    store_decoder_device(data_dir, device, &note);
    (device, format!("auto: {note}"), true)
}

fn env_override(name: &str, min: usize, max: usize) -> Option<usize> {
    let value = std::env::var(name).ok()?.trim().parse::<usize>().ok()?;
    Some(value.clamp(min, usize::max(min, max)))
}

pub fn audio8_model_dir(models_dir: &Path) -> PathBuf {
    AUDIO8_REPO
        .split('/')
        .fold(models_dir.to_path_buf(), |path, part| path.join(part))
}

pub fn is_audio8_model_dir(path: &Path) -> bool {
    AUDIO8_MODEL_FILES.iter().all(|file| path.join(file).is_file())
}

/// Finds the ONNX Runtime shared library that ships next to the app (see
/// scripts/fetch-onnxruntime.js).
fn resolve_onnxruntime_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("VOICEREADER_ONNXRUNTIME_PATH").map(PathBuf::from) {
        if path.is_file() {
            return Ok(path);
        }
    }
    let library_name = if cfg!(target_os = "windows") {
        "onnxruntime.dll"
    } else if cfg!(target_os = "macos") {
        "libonnxruntime.dylib"
    } else {
        "libonnxruntime.so"
    };

    if let Some(path) = find_bundled_file(&search_roots(), "onnxruntime", library_name, &[]) {
        return Ok(path);
    }
    // Deliberately no fallback to the system search path: Windows ships an old
    // onnxruntime.dll in System32 that is too old for this model.
    Err(anyhow!(
        "ONNX Runtime library ({library_name}) was not found next to the app. Run `npm run onnxruntime:fetch` or set VOICEREADER_ONNXRUNTIME_PATH."
    ))
}

/// Loads the bundled ONNX Runtime library. Safe to call more than once; every ONNX
/// backend (Audio8, transcription) calls it before creating a session.
pub(crate) fn ensure_onnxruntime() -> Result<()> {
    init_onnxruntime(&resolve_onnxruntime_path()?)
}

pub struct LocalAudio8Runtime {
    model: Arc<Audio8Model>,
    model_dir: PathBuf,
    voices_dir: PathBuf,
    prefix_cache: HashMap<String, Arc<VoicePrefix>>,
    prefix_order: Vec<String>,
    decoder_threads: usize,
    decoder_note: String,
}

impl LocalAudio8Runtime {
    pub fn new(model_dir: &Path, data_dir: &Path, compute: ComputePreference) -> Result<Self> {
        if !is_audio8_model_dir(model_dir) {
            return Err(anyhow!(
                "Audio8 model files are missing or incomplete in {}",
                model_dir.display()
            ));
        }
        ensure_onnxruntime()?;
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        // Defaults: two chunks generated at once (one thread each) and one codec decoder
        // with up to 8 threads. On a 16-core / 32-thread CPU, more generators, more
        // decoders or more decoder threads did not raise throughput (each token reads
        // far more memory than fits in cache, so memory traffic is the limit, not cores)
        // and cost 1 to 4 GB more RAM. The overrides exist for testing on other machines.
        let parallel_chunks = env_override(PARALLEL_CHUNKS_ENV, 1, 8).unwrap_or(if cores >= 4 { 2 } else { 1 });
        let decoders = env_override(DECODERS_ENV, 1, 4).unwrap_or(1);
        let decoder_threads = env_override(DECODER_THREADS_ENV, 1, cores).unwrap_or((cores / 2).clamp(1, 8));
        let (decoder_device, mut decoder_note, from_benchmark) =
            choose_decoder_device(model_dir, data_dir, decoder_threads, compute);
        let model = match Audio8Model::load(model_dir, parallel_chunks, decoders, decoder_threads, decoder_device) {
            Ok(model) => model,
            Err(err) if decoder_device != DecoderDevice::Cpu => {
                // A GPU that benchmarked fine can still fail later (driver update, device
                // removed). Speech must keep working, so fall back and remember it.
                decoder_note = format!("{} failed, using cpu: {err:#}", decoder_device.label());
                if from_benchmark {
                    store_decoder_device(data_dir, DecoderDevice::Cpu, &decoder_note);
                }
                Audio8Model::load(model_dir, parallel_chunks, decoders, decoder_threads, DecoderDevice::Cpu)
                    .context("Failed to load Audio8 TTS model")?
            }
            Err(err) => return Err(err).context("Failed to load Audio8 TTS model"),
        };

        let voices_dir = data_dir.join("voices");
        std::fs::create_dir_all(&voices_dir)
            .with_context(|| format!("Failed to create voices directory {}", voices_dir.display()))?;

        let mut runtime = Self {
            model: Arc::new(model),
            model_dir: model_dir.to_path_buf(),
            voices_dir,
            prefix_cache: HashMap::new(),
            prefix_order: Vec::new(),
            decoder_threads,
            decoder_note,
        };
        // Building a voice prefix takes a couple of seconds; do it for the built-in voice
        // now so the first read-aloud does not pay for it.
        runtime
            .resolve_prefix(BUILTIN_VOICE_ID)
            .context("Failed to prepare the built-in Audio8 voice")?;
        Ok(runtime)
    }

    /// Where the codec decoder ended up running ("cpu", "directml", "coreml").
    pub fn decoder_device_label(&self) -> &'static str {
        self.model.decoder_device().label()
    }

    /// How that device was chosen, for display.
    pub fn decoder_note(&self) -> &str {
        &self.decoder_note
    }

    pub fn health_payload(&self) -> Value {
        json!({
            "engine_version": env!("CARGO_PKG_VERSION"),
            "active_model_id": AUDIO8_REPO,
            "device": if self.model.decoder_device() == DecoderDevice::Cpu { "cpu" } else { "cpu+gpu" },
            "capabilities": {
                "supports_voice_clone": self.model.registration_available(),
                "supports_audio_chunk_stream": true,
                "supports_true_streaming_inference": true,
                "languages": ["en", "zh"]
            },
            "runtime": {
                "backend": "audio8_tts_onnx_rust",
                "model_loaded": true,
                "fallback_active": false,
                "detail": format!(
                    "model={}, source={}, sample_rate={}, parallel_chunks={}, decoders={}, decoder_threads={}, decoder_device={} ({})",
                    AUDIO8_REPO,
                    self.model_dir.display(),
                    self.model.sample_rate(),
                    self.model.ar_pool_size(),
                    self.model.decoder_pool_size(),
                    self.decoder_threads,
                    self.model.decoder_device().label(),
                    self.decoder_note
                ),
                "supports_default_voice": true,
                "supports_cloned_voices": self.model.registration_available(),
                "warmup": {
                    "status": "ready",
                    "runs": 1,
                    "last_reason": "startup",
                    "last_started_at": null,
                    "last_completed_at": null,
                    "last_duration_ms": null,
                    "last_error": null
                }
            }
        })
    }

    fn voice_dir(&self, voice_id: &str) -> Result<PathBuf> {
        if voice_id.is_empty() || Path::new(voice_id).file_name().and_then(|name| name.to_str()) != Some(voice_id) {
            return Err(anyhow!("Invalid voice id"));
        }
        Ok(self.voices_dir.join(voice_id))
    }

    /// Encodes a new voice's reference audio and stores the codes next to its metadata.
    /// `wav_bytes` should be the original upload; it is resampled to the model rate.
    pub fn register_voice(&mut self, voice_id: &str, wav_bytes: &[u8], ref_text: &str) -> Result<()> {
        let transcript = clean_text(ref_text);
        if transcript.is_empty() {
            return Err(anyhow!("Audio8 voice cloning needs the exact transcript of the reference audio"));
        }
        let voice_dir = self.voice_dir(voice_id)?;
        let samples = load_wav_mono(wav_bytes, self.model.sample_rate())?;
        let codes = self.model.encode_reference(&samples)?;
        // Fail now, with a clear message, if the clip plus transcript is too long to use.
        let prefix = self.model.build_voice_prefix(&transcript, &codes)?;
        std::fs::create_dir_all(&voice_dir)
            .with_context(|| format!("Failed to create voice directory {}", voice_dir.display()))?;
        write_codes_npy(
            &voice_dir.join(VOICE_CODES_FILE_NAME),
            &codes,
            self.model.manifest().num_codebooks,
        )?;
        self.cache_prefix(voice_id, Arc::new(prefix));
        Ok(())
    }

    pub fn invalidate_voice(&mut self, voice_id: &str) {
        self.prefix_cache.remove(voice_id);
        self.prefix_order.retain(|cached| cached != voice_id);
    }

    fn cache_prefix(&mut self, voice_id: &str, prefix: Arc<VoicePrefix>) {
        self.invalidate_voice(voice_id);
        self.prefix_cache.insert(voice_id.to_string(), prefix);
        self.prefix_order.push(voice_id.to_string());
        while self.prefix_order.len() > MAX_CACHED_PREFIXES {
            let evicted = self.prefix_order.remove(0);
            self.prefix_cache.remove(&evicted);
        }
    }

    fn resolve_prefix(&mut self, voice_id: &str) -> Result<Arc<VoicePrefix>> {
        if let Some(prefix) = self.prefix_cache.get(voice_id).cloned() {
            self.prefix_order.retain(|cached| cached != voice_id);
            self.prefix_order.push(voice_id.to_string());
            return Ok(prefix);
        }

        let prefix = if voice_id == BUILTIN_VOICE_ID {
            let (codes, text) = self.model.builtin_voice()?;
            self.model.build_voice_prefix(&text, &codes)?
        } else {
            let voice_dir = self.voice_dir(voice_id)?;
            let meta_path = voice_dir.join(META_FILE_NAME);
            if !meta_path.exists() {
                return Err(anyhow!("VOICE_NOT_FOUND: {voice_id}"));
            }
            let body = std::fs::read_to_string(&meta_path)
                .with_context(|| format!("Failed to read {}", meta_path.display()))?;
            let meta: SavedVoiceMeta =
                serde_json::from_str(&body).with_context(|| format!("Failed to parse {}", meta_path.display()))?;
            let transcript = meta.ref_text.as_deref().map(clean_text).unwrap_or_default();
            if transcript.is_empty() {
                return Err(anyhow!(
                    "Saved voice \"{}\" has no transcript, which Audio8 needs. Clone it again with the exact transcript of the sample, or use it with Kyutai Pocket TTS.",
                    meta.display_name
                ));
            }
            let codes_path = voice_dir.join(VOICE_CODES_FILE_NAME);
            let codes = if codes_path.exists() {
                read_codes_npy(&codes_path, self.model.manifest().num_codebooks)?
            } else {
                // A voice cloned while another model was active only has the normalized
                // reference clip; encode it on first use.
                let ref_audio_path = voice_dir.join(REF_AUDIO_FILE_NAME);
                let wav_bytes = std::fs::read(&ref_audio_path).with_context(|| {
                    format!("Saved voice {voice_id} is missing reference audio at {}", ref_audio_path.display())
                })?;
                let samples = load_wav_mono(&wav_bytes, self.model.sample_rate())?;
                let codes = self.model.encode_reference(&samples)?;
                write_codes_npy(&codes_path, &codes, self.model.manifest().num_codebooks)?;
                codes
            };
            self.model.build_voice_prefix(&transcript, &codes)?
        };

        let prefix = Arc::new(prefix);
        self.cache_prefix(voice_id, prefix.clone());
        Ok(prefix)
    }

    /// Synthesizes `text` and streams PCM through `on_chunk(chunk_index, pcm, sample_rate)`.
    /// Returns `(end_state, had_audio)`, matching `LocalKyutaiRuntime::stream_synthesize`.
    pub fn stream_synthesize<F>(
        &mut self,
        voice_id: &str,
        text: &str,
        chunk_max_chars: u32,
        volume: f32,
        cancel: &AtomicBool,
        active_rate_steps: &AtomicU32,
        on_chunk: F,
    ) -> Result<(LocalJobEndState, bool)>
    where
        F: Fn(usize, &[i16], u32) -> Result<()> + Send + 'static,
    {
        let prefix = self.resolve_prefix(voice_id)?;
        let sample_rate = self.model.sample_rate();
        let hop = self.model.manifest().codec_hop_length;

        let mut budgets = RAMP_CHUNK_BUDGETS.to_vec();
        budgets.push((chunk_max_chars as f32).clamp(MIN_CHUNK_BUDGET, MAX_CHUNK_BUDGET));
        let plans: Vec<ChunkPlan> = chunk_text(&normalize_for_speech(text), &budgets)
            .into_iter()
            .map(|piece| ChunkPlan {
                params: GenParams {
                    max_new_frames: max_frames_for_text(&piece, sample_rate, hop),
                    ..GenParams::default()
                },
                text: piece,
            })
            .collect();
        if plans.is_empty() {
            return Ok((LocalJobEndState::Done, false));
        }

        let mut emitter = RateEmitter::new(sample_rate, active_rate_steps, on_chunk);
        let first_frames = first_frames_for_rate(emitter.rate(), hop as f32 / sample_rate as f32);
        let gain = volume.clamp(0.0, 2.0);
        let mut pcm: Vec<i16> = Vec::new();
        let outcome = self.model.synthesize_chunks(&prefix, &plans, first_frames, cancel, |audio| {
            pcm.clear();
            pcm.extend(
                audio
                    .iter()
                    .map(|sample| ((sample * gain).clamp(-1.0, 1.0) * 32767.0) as i16),
            );
            emitter.push(&pcm)
        });

        match outcome {
            Ok(true) => {
                emitter.finish()?;
                Ok((LocalJobEndState::Done, emitter.had_audio()))
            }
            Ok(false) => {
                emitter.abort();
                Ok((LocalJobEndState::Canceled, emitter.had_audio()))
            }
            Err(err) => {
                emitter.abort();
                Err(err)
            }
        }
    }
}

/// Frames to buffer before the first audio is sent. The frontend waits for a prebuffer
/// that grows with the playback rate (`minPrebufferSeconds` in src/playback.ts), so the
/// first piece has to cover it or playback would only start with the second piece.
fn first_frames_for_rate(rate: f32, frame_seconds: f32) -> usize {
    let prebuffer = if rate <= 1.0 {
        0.24
    } else if rate <= 2.0 {
        (0.24 + (rate - 1.0) * 0.45).min(0.85)
    } else {
        (0.85 + (rate - 2.0)).min(2.0)
    };
    // SoX holds back audio while time-stretching: 0.16 to 0.25 s of output measured at 1.5x.
    let sox_slack = if (rate - 1.0).abs() > f32::EPSILON { 0.3 } else { 0.0 };
    let source_seconds = (prebuffer + sox_slack) * rate;
    usize::max(MIN_FIRST_FRAMES, (source_seconds / frame_seconds).ceil() as usize + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    #[test]
    fn first_frames_grow_with_playback_rate() {
        let frame_seconds = 2048.0 / 44100.0;
        assert_eq!(first_frames_for_rate(1.0, frame_seconds), MIN_FIRST_FRAMES);
        assert!(first_frames_for_rate(1.5, frame_seconds) > first_frames_for_rate(1.0, frame_seconds));
        assert!(first_frames_for_rate(3.0, frame_seconds) > first_frames_for_rate(1.5, frame_seconds));
    }

    #[test]
    fn gpu_is_only_chosen_when_clearly_faster() {
        let gpu = DecoderDevice::DirectMl;
        // Discrete GPU: 280 ms on the CPU, 10 ms on the GPU.
        assert_eq!(faster_device(280.0, gpu, 10.0), gpu);
        // Small integrated GPU: slower than the CPU.
        assert_eq!(faster_device(280.0, gpu, 1350.0), DecoderDevice::Cpu);
        // Roughly equal: not worth the extra moving parts.
        assert_eq!(faster_device(280.0, gpu, 220.0), DecoderDevice::Cpu);
        assert_eq!(device_from_label("directml"), Some(DecoderDevice::DirectMl));
        assert_eq!(device_from_label("vulkan"), None);
    }

    #[test]
    fn compute_preference_parses_setting_values() {
        assert_eq!(ComputePreference::parse("auto"), Some(ComputePreference::Auto));
        assert_eq!(ComputePreference::parse("cpu"), Some(ComputePreference::Cpu));
        assert_eq!(ComputePreference::parse(" GPU "), Some(ComputePreference::Gpu));
        assert_eq!(ComputePreference::parse("fast"), None);
    }

    #[test]
    fn model_dir_follows_repo_layout() {
        let dir = audio8_model_dir(Path::new("models"));
        assert!(dir.ends_with(Path::new("Edge0").join("audio8-TTS-0.1B-ONNX-INT8")));
        assert!(!is_audio8_model_dir(&dir));
    }

    /// End-to-end check against the real model. Run with:
    /// `VOICEREADER_AUDIO8_TEST_MODEL_DIR=<model dir> cargo test --features build-base -- --ignored --nocapture`
    #[test]
    #[ignore = "needs the downloaded Audio8 model"]
    fn speaks_and_clones_with_the_real_model() {
        let model_dir = PathBuf::from(
            std::env::var("VOICEREADER_AUDIO8_TEST_MODEL_DIR").expect("set VOICEREADER_AUDIO8_TEST_MODEL_DIR"),
        );
        let data_dir = std::env::temp_dir().join(format!("voicereader-audio8-test-{}", std::process::id()));
        let mut runtime =
            LocalAudio8Runtime::new(&model_dir, &data_dir, ComputePreference::Auto).expect("runtime loads");
        println!(
            "decoder device: {} ({})",
            runtime.model.decoder_device().label(),
            runtime.decoder_note
        );
        let sample_rate = runtime.model.sample_rate();
        let text = "VoiceReader reads highlighted text aloud. 今天天气很好，我们一起去公园散步吧。";

        let speak = |runtime: &mut LocalAudio8Runtime, voice_id: &str, rate_steps: u32| {
            let collected: Arc<Mutex<(Vec<i16>, Option<Duration>)>> = Arc::new(Mutex::new((Vec::new(), None)));
            let sink = collected.clone();
            let started = Instant::now();
            let cancel = AtomicBool::new(false);
            let rate = AtomicU32::new(rate_steps);
            let (end, had_audio) = runtime
                .stream_synthesize(voice_id, text, 200, 1.0, &cancel, &rate, move |_, pcm, chunk_rate| {
                    assert_eq!(chunk_rate, sample_rate);
                    let mut guard = sink.lock().unwrap();
                    guard.1.get_or_insert(started.elapsed());
                    guard.0.extend_from_slice(pcm);
                    Ok(())
                })
                .expect("stream succeeds");
            assert!(matches!(end, LocalJobEndState::Done));
            assert!(had_audio);
            let guard = collected.lock().unwrap();
            let seconds = guard.0.len() as f32 / sample_rate as f32;
            let peak = guard.0.iter().map(|sample| sample.unsigned_abs()).max().unwrap_or(0);
            println!(
                "voice={voice_id} rate={:.2} first audio after {:.2}s, {seconds:.2}s of output, peak {peak}",
                rate_steps as f32 / 4.0,
                guard.1.unwrap().as_secs_f32()
            );
            assert!(peak > 1000, "output is silent");
            (guard.0.clone(), seconds)
        };

        let (normal_pcm, normal_seconds) = speak(&mut runtime, BUILTIN_VOICE_ID, 4);
        assert!(normal_seconds > 3.0 && normal_seconds < 30.0);
        // 1.5x through SoX should be clearly shorter than 1.0x (generation is sampled, so not exact).
        let (_, fast_seconds) = speak(&mut runtime, BUILTIN_VOICE_ID, 6);
        assert!(fast_seconds < normal_seconds * 0.9, "tempo was not applied: {fast_seconds} vs {normal_seconds}");

        // Clone a voice from the audio just produced, the way the app does: metadata is
        // written by the Kyutai runtime, codes by `register_voice`.
        let voice_id = "test-voice";
        let voice_dir = data_dir.join("voices").join(voice_id);
        std::fs::create_dir_all(&voice_dir).unwrap();
        let clip: Vec<i16> = normal_pcm.iter().copied().take(sample_rate as usize * 6).collect();
        let wav_path = voice_dir.join(REF_AUDIO_FILE_NAME);
        let mut writer = hound::WavWriter::create(
            &wav_path,
            hound::WavSpec {
                channels: 1,
                sample_rate,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for sample in &clip {
            writer.write_sample(*sample).unwrap();
        }
        writer.finalize().unwrap();
        let meta = SavedVoiceMeta {
            voice_id: voice_id.to_string(),
            display_name: "Test voice".to_string(),
            created_at: "0".to_string(),
            tts_model_id: AUDIO8_REPO.to_string(),
            language_hint: "en".to_string(),
            description: None,
            ref_text: Some("VoiceReader reads highlighted text aloud.".to_string()),
        };
        std::fs::write(voice_dir.join(META_FILE_NAME), serde_json::to_string(&meta).unwrap()).unwrap();

        // A missing transcript is rejected before any work is done.
        assert!(runtime.register_voice(voice_id, &std::fs::read(&wav_path).unwrap(), "  ").is_err());
        runtime
            .register_voice(voice_id, &std::fs::read(&wav_path).unwrap(), meta.ref_text.as_deref().unwrap())
            .expect("voice registers");
        assert!(voice_dir.join(VOICE_CODES_FILE_NAME).exists());
        speak(&mut runtime, voice_id, 4);

        // A voice saved without codes (cloned while Kyutai was active) is encoded on first use.
        std::fs::remove_file(voice_dir.join(VOICE_CODES_FILE_NAME)).unwrap();
        runtime.invalidate_voice(voice_id);
        speak(&mut runtime, voice_id, 4);
        assert!(voice_dir.join(VOICE_CODES_FILE_NAME).exists());

        // A voice without a transcript cannot be used and says why.
        let mut no_text = meta.clone();
        no_text.ref_text = None;
        std::fs::write(voice_dir.join(META_FILE_NAME), serde_json::to_string(&no_text).unwrap()).unwrap();
        runtime.invalidate_voice(voice_id);
        let cancel = AtomicBool::new(false);
        let rate = AtomicU32::new(4);
        let err = runtime
            .stream_synthesize(voice_id, text, 200, 1.0, &cancel, &rate, |_, _, _| Ok(()))
            .err()
            .expect("missing transcript is an error");
        assert!(err.to_string().contains("no transcript"), "{err}");

        // Cancellation ends the job without an error.
        let cancel = AtomicBool::new(true);
        let (end, _) = runtime
            .stream_synthesize(BUILTIN_VOICE_ID, text, 200, 1.0, &cancel, &rate, |_, _, _| Ok(()))
            .unwrap();
        assert!(matches!(end, LocalJobEndState::Canceled));

        std::fs::remove_dir_all(&data_dir).ok();
    }
}
