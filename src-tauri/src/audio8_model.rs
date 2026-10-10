//! Rust-native ONNX Runtime inference for `Edge0/audio8-TTS-0.1B-ONNX-INT8`.
//!
//! This is a port of the upstream Python reference runtime
//! (`Audio8-AI/Audio8_TTS`, `onnx_runtime_0_1b_int8/arktts_runtime`). The model is
//! three graphs:
//!   • slow AR  — one `[1, 11, 1]` token column per call, recurrent state carried by the caller
//!   • fast AR  — ten calls per audio frame, producing the ten codec codebooks
//!   • codec decoder — codes `[1, 10, T]` to 44.1 kHz audio
//! plus an optional codec encoder used only to register (clone) a voice.
//!
//! This file has no Tauri or SoX dependencies so it can be exercised standalone.

use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

use anyhow::{anyhow, bail, Context, Result};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::TensorRef;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::Deserialize;
use tokenizers::Tokenizer;

use crate::audio_decode::resample;
use crate::text_chunking::{is_cjk, is_invisible, text_units};

const MIN_REFERENCE_SECONDS: f32 = 0.5;
const MAX_REFERENCE_SECONDS: f32 = 30.0;
/// A chunk that is currently being played gets a decode as soon as this many new frames exist.
const STREAM_MIN_NEW_FRAMES: usize = 8;
const GPU_WARMUP_FRAMES: usize = 16;
/// The same, when the decoder runs on a GPU and a decode costs almost nothing.
const GPU_STREAM_MIN_NEW_FRAMES: usize = 4;
/// Chunks generated ahead of playback are decoded in larger, less frequent windows.
const LOOKAHEAD_MIN_NEW_FRAMES: usize = 24;
/// Cap on one look-ahead decode, so the chunk being played is never starved for long.
const LOOKAHEAD_MAX_WINDOW_FRAMES: usize = 24;
/// Left context for every decode window after a chunk's first. The first window starts
/// at frame 0 and is bit-exact with a full decode because the codec decoder is causal;
/// later windows are close to, but not identical with, a full decode (docs/learnings.md).
const STREAM_CONTEXT_FRAMES: usize = 8;
/// Samples blended at a window boundary to hide phase differences between windows.
const STREAM_CROSSFADE_SAMPLES: usize = 512;
/// How many chunks past the one being played may be generated ahead.
const MAX_CHUNKS_AHEAD: usize = 3;

/// Where the codec decoder runs. The two AR graphs always run on the CPU: they are
/// called once per token with small inputs, and measured twice as slow on a GPU. The
/// decoder is the opposite: about 25 times faster on a discrete GPU than on 8 CPU threads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecoderDevice {
    Cpu,
    /// DirectML on Windows: any DirectX 12 GPU (NVIDIA, AMD, Intel, integrated or not).
    DirectMl,
    /// Native WebGPU through Dawn's Metal backend (Apple Silicon macOS).
    WebGpu,
}

impl DecoderDevice {
    pub fn label(self) -> &'static str {
        match self {
            DecoderDevice::Cpu => "cpu",
            DecoderDevice::DirectMl => "directml",
            DecoderDevice::WebGpu => "webgpu",
        }
    }
}

#[derive(Deserialize, Clone)]
pub struct Manifest {
    pub model_fingerprint: String,
    pub sample_rate: u32,
    pub codec_hop_length: usize,
    #[serde(default = "default_guard_frames")]
    pub stream_guard_frames: usize,
    pub num_codebooks: usize,
    pub codebook_size: usize,
    pub semantic_begin_id: i64,
    pub semantic_end_id: i64,
    pub im_end_id: i64,
    pub slow_logits_layout: String,
    pub max_seq_len: usize,
    pub num_layers: usize,
    pub n_local_heads: usize,
    pub head_dim: usize,
    pub num_fast_layers: usize,
    pub fast_n_local_heads: usize,
    pub fast_head_dim: usize,
    pub reference_codes: Option<String>,
    pub reference_text: Option<String>,
    pub slow_decode_model: Option<String>,
    pub fast_model: Option<String>,
}

fn default_guard_frames() -> usize {
    1
}

/// Codec codes for a reference voice, stored codebook-major: `data[codebook * frames + frame]`.
#[derive(Clone)]
pub struct VoiceCodes {
    pub frames: usize,
    pub data: Vec<i64>,
}

/// Slow-AR state after the part of the prompt that only depends on the voice.
/// Restoring it skips ~2.4 s of prefill for every chunk spoken with that voice.
pub struct VoicePrefix {
    len: usize,
    keys: Vec<f32>,
    values: Vec<f32>,
    conv: Vec<f32>,
    ssm: Vec<f32>,
}

#[derive(Clone)]
pub struct GenParams {
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: usize,
    pub max_new_frames: usize,
    pub seed: Option<u64>,
}

impl Default for GenParams {
    fn default() -> Self {
        Self {
            temperature: 0.7,
            top_p: 0.9,
            top_k: 50,
            max_new_frames: 1024,
            seed: None,
        }
    }
}

/// One independently generated piece of text.
pub struct ChunkPlan {
    pub text: String,
    pub params: GenParams,
}

struct Pool<T> {
    items: Mutex<Vec<T>>,
    available: Condvar,
}

struct PoolGuard<'a, T> {
    pool: &'a Pool<T>,
    item: Option<T>,
}

impl<T> Pool<T> {
    fn new(items: Vec<T>) -> Self {
        Self {
            items: Mutex::new(items),
            available: Condvar::new(),
        }
    }

    fn acquire(&self) -> PoolGuard<'_, T> {
        let mut items = self.items.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if let Some(item) = items.pop() {
                return PoolGuard {
                    pool: self,
                    item: Some(item),
                };
            }
            items = self
                .available
                .wait(items)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

impl<T> Deref for PoolGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.item.as_ref().expect("pool item present until drop")
    }
}

impl<T> DerefMut for PoolGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.item.as_mut().expect("pool item present until drop")
    }
}

impl<T> Drop for PoolGuard<'_, T> {
    fn drop(&mut self) {
        if let Some(item) = self.item.take() {
            let mut items = self.pool.items.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            items.push(item);
            self.pool.available.notify_one();
        }
    }
}

struct SlowState {
    keys: Vec<f32>,
    values: Vec<f32>,
    conv: Vec<f32>,
    ssm: Vec<f32>,
    /// Number of cache positions written since the buffers were last zeroed.
    used_len: usize,
}

struct ArSessions {
    slow: Session,
    fast: Session,
    state: SlowState,
    fast_caches: Vec<Vec<f32>>,
}

pub struct Audio8Model {
    manifest: Manifest,
    model_dir: PathBuf,
    tokenizer: Tokenizer,
    ar_pool: Pool<ArSessions>,
    ar_pool_size: usize,
    decoder_pool: Pool<Session>,
    decoder_pool_size: usize,
    decoder_device: DecoderDevice,
    cache_shape: Vec<i64>,
    conv_shape: Vec<i64>,
    ssm_shape: Vec<i64>,
    fast_cache_shape: Vec<i64>,
    fast_hidden_dim: usize,
    fast_cache_names: Vec<(String, String, String, String)>,
    suffix_tokens: Vec<i64>,
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
static WEBGPU_LIBRARY_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Loads the ONNX Runtime shared library. Must succeed once before any model is loaded;
/// later calls return the first result.
pub fn init_onnxruntime(library_path: &Path) -> Result<()> {
    static INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| {
        if !library_path.is_file() {
            return Err(format!(
                "ONNX Runtime library was not found at {}",
                library_path.display()
            ));
        }
        // Windows ships its own, usually older, DirectML.dll in System32. Load the bundled
        // one first so ONNX Runtime binds to the version it was built against.
        #[cfg(target_os = "windows")]
        if let Some(directml) = library_path.parent().map(|dir| dir.join("DirectML.dll")) {
            if directml.is_file() {
                // Safety: DirectML.dll is a plain system library with no unusual
                // initialization; it stays loaded for the life of the process.
                if let Ok(library) = unsafe { libloading::Library::new(&directml) } {
                    std::mem::forget(library);
                }
            }
        }
        match ort::init_from(library_path) {
            Ok(builder) => {
                builder.commit();
                #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
                if let Some(parent) = library_path.parent() {
                    let _ = WEBGPU_LIBRARY_PATH.set(parent.join("libonnxruntime_providers_webgpu.dylib"));
                }
                Ok(())
            }
            Err(err) => Err(format!(
                "Failed to load ONNX Runtime from {}: {err}",
                library_path.display()
            )),
        }
    })
    .clone()
    .map_err(|message| anyhow!(message))
}

fn ort_err<E: std::fmt::Display>(err: E) -> anyhow::Error {
    anyhow!("{err}")
}

/// Register lazily: a missing plugin must not prevent CPU TTS or transcription.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn register_webgpu() -> Result<()> {
    static REGISTRATION: OnceLock<std::result::Result<ort::ep::ExecutionProviderLibrary, String>> = OnceLock::new();
    REGISTRATION
        .get_or_init(|| {
            let path = WEBGPU_LIBRARY_PATH.get().ok_or("ONNX Runtime must be initialized before WebGPU")?;
            if !path.is_file() {
                return Err(format!("WebGPU plugin was not found at {}", path.display()));
            }
            ort::environment::Environment::current()
                .and_then(|env| env.register_ep_library("voicereader_webgpu", path))
                .map_err(|err| format!("Failed to register WebGPU: {err}"))
        })
        .as_ref()
        .map(|_| ())
        .map_err(|message| anyhow!("{message}"))
}

/// The CPU EP promotes this export's math to FP32 internally. Native WebGPU's
/// FP16 math corrupted speech, so use the validated FP32-compute graph there too.
/// It is small: weight casts reuse the existing, unchanged FP16 external data.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn webgpu_decoder_path(original: &Path) -> Result<PathBuf> {
    use crate::bundled_paths::{find_bundled_file, search_roots};
    static PREPARE: Mutex<()> = Mutex::new(());
    let _guard = PREPARE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let graph = find_bundled_file(
        &search_roots(), "audio8-metal", "codec_decoder_fp32.onnx",
        &["src-tauri/binaries/audio8-metal/codec_decoder_fp32.onnx".to_string()],
    ).ok_or_else(|| anyhow!("The validated FP32 Metal decoder graph is missing from this app"))?;
    let expected_source = graph.with_file_name("codec_decoder_fp16.source.onnx");
    if std::fs::read(original)? != std::fs::read(expected_source)? {
        bail!("The Audio8 export differs from the one validated for Metal; using CPU is required");
    }
    let destination = original.with_file_name("codec_decoder_webgpu_fp32_v1.onnx");
    let bytes = std::fs::read(&graph)?;
    if std::fs::read(&destination).ok().as_deref() != Some(bytes.as_slice()) {
        let temporary = destination.with_extension(format!("onnx.tmp-{}", std::process::id()));
        std::fs::write(&temporary, &bytes).context("Failed to cache the FP32 Metal decoder graph")?;
        std::fs::rename(&temporary, &destination).context("Failed to finish caching the FP32 Metal decoder graph")?;
    }
    Ok(destination)
}

fn build_session(path: &Path, intra_threads: usize) -> Result<Session> {
    if !path.is_file() {
        bail!("ONNX model was not found: {}", path.display());
    }
    let mut builder = Session::builder()
        .map_err(ort_err)?
        .with_optimization_level(GraphOptimizationLevel::All)
        .map_err(ort_err)?
        .with_intra_threads(usize::max(1, intra_threads))
        .map_err(ort_err)?
        .with_inter_threads(1)
        .map_err(ort_err)?;
    builder
        .commit_from_file(path)
        .map_err(ort_err)
        .with_context(|| format!("Failed to load ONNX model {}", path.display()))
}

fn build_decoder_session(path: &Path, device: DecoderDevice, cpu_threads: usize) -> Result<Session> {
    if !path.is_file() {
        bail!("ONNX model was not found: {}", path.display());
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    let prepared_path;
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    let path = if device == DecoderDevice::WebGpu {
        prepared_path = webgpu_decoder_path(path)?;
        prepared_path.as_path()
    } else {
        path
    };
    let builder = Session::builder()
        .map_err(ort_err)?
        .with_optimization_level(GraphOptimizationLevel::All)
        .map_err(ort_err)?
        .with_intra_threads(usize::max(1, cpu_threads))
        .map_err(ort_err)?
        .with_inter_threads(1)
        .map_err(ort_err)?;
    let mut builder = match device {
        DecoderDevice::Cpu => builder,
        #[cfg(target_os = "windows")]
        DecoderDevice::DirectMl => builder
            // DirectML requires memory patterns off and sequential execution (the default).
            .with_memory_pattern(false)
            .map_err(ort_err)?
            .with_execution_providers([ort::ep::DirectML::default()
                .with_performance_preference(ort::ep::directml::PerformancePreference::HighPerformance)
                .build()
                .error_on_failure()])
            .map_err(ort_err)?,
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        DecoderDevice::WebGpu => {
            register_webgpu()?;
            let env = ort::environment::Environment::current().map_err(ort_err)?;
            let device = env
                .devices()
                .find(|device| device.ep().ok() == Some("WebGpuExecutionProvider"))
                .ok_or_else(|| anyhow!("WebGPU did not discover a compatible Metal device"))?;
            // Plugin EPs use the device API, rather than the legacy built-in WebGPU API.
            builder.with_devices([device], None).map_err(ort_err)?
        }
        #[allow(unreachable_patterns)]
        other => bail!("The {} decoder device is not available on this platform", other.label()),
    };
    #[cfg(all(test, target_os = "macos", target_arch = "aarch64"))]
    if let Some(dir) = std::env::var_os("VOICEREADER_AUDIO8_TEST_OUTPUT_DIR") {
        builder = builder
            .with_profiling(PathBuf::from(dir).join(format!("decoder-{}", device.label())))
            .map_err(ort_err)?;
    }
    builder
        .commit_from_file(path)
        .map_err(ort_err)
        .with_context(|| format!("Failed to load {} on {}", path.display(), device.label()))
}

/// Loads the codec decoder on `device` and times one decode of a short, fixed input
/// (after a warm-up run). Used to decide whether a GPU is actually faster than the CPU
/// on this machine; an integrated GPU can be several times slower.
pub fn benchmark_decoder(model_dir: &Path, device: DecoderDevice, cpu_threads: usize) -> Result<std::time::Duration> {
    const FRAMES: usize = 24;
    const RUNS: usize = 2;
    let manifest: Manifest = serde_json::from_str(
        &std::fs::read_to_string(model_dir.join("runtime_manifest.json")).context("Failed to read runtime manifest")?,
    )
    .context("Failed to parse runtime manifest")?;
    let mut decoder = build_decoder_session(&model_dir.join("codec_decoder_fp16.onnx"), device, cpu_threads)?;
    let codebooks = manifest.num_codebooks;
    // Any valid codes will do; the cost does not depend on their values.
    let codes: Vec<i64> = (0..codebooks * FRAMES)
        .map(|index| (index * 37 % manifest.codebook_size) as i64)
        .collect();
    let mut best = std::time::Duration::MAX;
    // The first run includes one-time setup, so the faster of two is used.
    for _ in 0..RUNS {
        let started = std::time::Instant::now();
        decoder
            .run(ort::inputs![
                "codes" => TensorRef::from_array_view((vec![1i64, codebooks as i64, FRAMES as i64], &codes[..])).map_err(ort_err)?,
            ])
            .map_err(ort_err)
            .with_context(|| format!("Audio8 codec decode failed on {}", device.label()))?;
        best = best.min(started.elapsed());
    }
    Ok(best)
}

fn input_shape(session: &Session, name: &str) -> Result<Vec<i64>> {
    let outlet = session
        .inputs()
        .iter()
        .find(|outlet| outlet.name() == name)
        .ok_or_else(|| anyhow!("ONNX graph is missing expected input `{name}`"))?;
    let shape = outlet
        .dtype()
        .tensor_shape()
        .ok_or_else(|| anyhow!("ONNX input `{name}` is not a tensor"))?;
    let dims: Vec<i64> = shape.iter().copied().collect();
    if dims.iter().any(|dim| *dim <= 0) {
        bail!("ONNX input `{name}` has a dynamic shape {dims:?}; this runtime expects fixed state shapes");
    }
    Ok(dims)
}

fn element_count(shape: &[i64]) -> usize {
    shape.iter().map(|dim| *dim as usize).product()
}

impl Audio8Model {
    /// Loads the model. `ar_pool_size` AR session pairs allow that many chunks to be
    /// generated concurrently; `decoder_pool_size` codec decoders run in parallel.
    pub fn load(
        model_dir: &Path,
        ar_pool_size: usize,
        decoder_pool_size: usize,
        decoder_threads: usize,
        decoder_device: DecoderDevice,
    ) -> Result<Self> {
        let manifest_path = model_dir.join("runtime_manifest.json");
        let manifest_body = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("Failed to read {}", manifest_path.display()))?;
        let manifest: Manifest = serde_json::from_str(&manifest_body)
            .with_context(|| format!("Failed to parse {}", manifest_path.display()))?;
        if manifest.slow_logits_layout != "relative_semantic_then_eos" {
            bail!(
                "Unsupported Audio8 slow logits layout `{}`; this runtime only supports the 0.1B INT8 export",
                manifest.slow_logits_layout
            );
        }
        if manifest.num_fast_layers != 4 {
            bail!("The Audio8 0.1B fast graph must have four cache layers");
        }

        let slow_name = manifest
            .slow_decode_model
            .clone()
            .unwrap_or_else(|| "slow_ar_int8.onnx".to_string());
        let fast_name = manifest.fast_model.clone().unwrap_or_else(|| "fast_ar_int8.onnx".to_string());

        let tokenizer_path = model_dir.join("tokenizer").join("tokenizer.json");
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|err| anyhow!("Failed to load tokenizer {}: {err}", tokenizer_path.display()))?;

        let ar_pool_size = usize::max(1, ar_pool_size);
        let mut ar_items: Vec<ArSessions> = Vec::with_capacity(ar_pool_size);
        let mut shapes: Option<(Vec<i64>, Vec<i64>, Vec<i64>, Vec<i64>, usize)> = None;
        for _ in 0..ar_pool_size {
            // The AR graphs run one token at a time and are fastest single-threaded; parallelism
            // comes from generating several chunks at once on separate session pairs.
            let slow = build_session(&model_dir.join(&slow_name), 1)?;
            let fast = build_session(&model_dir.join(&fast_name), 1)?;
            if shapes.is_none() {
                let cache_shape = input_shape(&slow, "cache_keys")?;
                let conv_shape = input_shape(&slow, "conv_states")?;
                let ssm_shape = input_shape(&slow, "ssm_states")?;
                let fast_cache_shape = input_shape(&fast, "cache_key_0")?;
                let hidden_shape = input_shape(&fast, "slow_hidden")?;
                if cache_shape.len() != 5
                    || cache_shape[0] as usize != manifest.num_layers
                    || cache_shape[2] as usize != manifest.n_local_heads
                    || cache_shape[3] as usize != manifest.max_seq_len
                    || cache_shape[4] as usize != manifest.head_dim
                {
                    bail!("Audio8 slow cache shape {cache_shape:?} does not match runtime_manifest.json");
                }
                if fast_cache_shape.len() != 4
                    || fast_cache_shape[2] as usize != manifest.num_codebooks
                    || fast_cache_shape[3] as usize != manifest.fast_head_dim
                {
                    bail!("Audio8 fast cache shape {fast_cache_shape:?} does not match runtime_manifest.json");
                }
                let hidden_dim = *hidden_shape.last().unwrap_or(&0) as usize;
                shapes = Some((cache_shape, conv_shape, ssm_shape, fast_cache_shape, hidden_dim));
            }
            let (cache_shape, conv_shape, ssm_shape, fast_cache_shape, _) =
                shapes.as_ref().expect("shapes resolved on first session");
            ar_items.push(ArSessions {
                slow,
                fast,
                state: SlowState {
                    keys: vec![0.0; element_count(cache_shape)],
                    values: vec![0.0; element_count(cache_shape)],
                    conv: vec![0.0; element_count(conv_shape)],
                    ssm: vec![0.0; element_count(ssm_shape)],
                    used_len: 0,
                },
                fast_caches: vec![vec![0.0; element_count(fast_cache_shape)]; manifest.num_fast_layers * 2],
            });
        }
        let (cache_shape, conv_shape, ssm_shape, fast_cache_shape, fast_hidden_dim) =
            shapes.expect("at least one AR session pair");

        let decoder_path = model_dir.join("codec_decoder_fp16.onnx");
        let decoder_pool_size = usize::max(1, decoder_pool_size);
        let mut decoders = Vec::new();
        for _ in 0..decoder_pool_size {
            decoders.push(build_decoder_session(&decoder_path, decoder_device, decoder_threads)?);
        }

        let fast_cache_names = (0..manifest.num_fast_layers)
            .map(|index| {
                (
                    format!("cache_key_{index}"),
                    format!("cache_value_{index}"),
                    format!("key_delta_{index}"),
                    format!("value_delta_{index}"),
                )
            })
            .collect();

        let mut model = Self {
            manifest,
            model_dir: model_dir.to_path_buf(),
            tokenizer,
            ar_pool: Pool::new(ar_items),
            ar_pool_size,
            decoder_pool: Pool::new(decoders),
            decoder_pool_size,
            decoder_device,
            cache_shape,
            conv_shape,
            ssm_shape,
            fast_cache_shape,
            fast_hidden_dim,
            fast_cache_names,
            suffix_tokens: Vec::new(),
        };
        if decoder_device != DecoderDevice::Cpu {
            // The first GPU decode pays for shader and pipeline setup (over a second);
            // do it now so it is not added to the first sentence spoken.
            let warmup: Vec<i64> = vec![0; model.manifest.num_codebooks * GPU_WARMUP_FRAMES];
            model.decode(&warmup, 0, GPU_WARMUP_FRAMES)?;
        }
        let mut suffix = model.encode("<|im_end|>\n")?;
        suffix.extend(model.encode("<|im_start|>assistant\n<|voice|>")?);
        model.suffix_tokens = suffix;
        Ok(model)
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn sample_rate(&self) -> u32 {
        self.manifest.sample_rate
    }

    pub fn ar_pool_size(&self) -> usize {
        self.ar_pool_size
    }

    pub fn decoder_pool_size(&self) -> usize {
        self.decoder_pool_size
    }

    pub fn decoder_device(&self) -> DecoderDevice {
        self.decoder_device
    }

    pub fn registration_available(&self) -> bool {
        self.model_dir.join("registration").join("codec_encoder_fp16.onnx").is_file()
    }

    /// The voice that ships with the model (`reference_codes.npy` + manifest transcript).
    pub fn builtin_voice(&self) -> Result<(VoiceCodes, String)> {
        let codes_name = self
            .manifest
            .reference_codes
            .clone()
            .unwrap_or_else(|| "reference_codes.npy".to_string());
        let codes = read_codes_npy(&self.model_dir.join(codes_name), self.manifest.num_codebooks)?;
        let text = self
            .manifest
            .reference_text
            .clone()
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| anyhow!("runtime_manifest.json has no reference_text for the built-in voice"))?;
        Ok((codes, text))
    }

    fn encode(&self, text: &str) -> Result<Vec<i64>> {
        let encoding = self
            .tokenizer
            .encode(text, false)
            .map_err(|err| anyhow!("Tokenizer failed: {err}"))?;
        Ok(encoding.get_ids().iter().map(|id| *id as i64).collect())
    }

    fn validate_codes(&self, codes: &VoiceCodes) -> Result<()> {
        if codes.frames == 0 || codes.data.len() != codes.frames * self.manifest.num_codebooks {
            bail!("Reference codes must have shape [{}, T>0]", self.manifest.num_codebooks);
        }
        let limit = self.manifest.codebook_size as i64;
        if codes.data.iter().any(|code| *code < 0 || *code >= limit) {
            bail!("Reference codes contain values outside [0, {limit})");
        }
        Ok(())
    }

    /// Runs the voice-dependent part of the prompt once and snapshots the slow-AR state.
    pub fn build_voice_prefix(&self, reference_text: &str, codes: &VoiceCodes) -> Result<VoicePrefix> {
        self.validate_codes(codes)?;
        let width = self.manifest.num_codebooks + 1;
        let mut head: Vec<i64> = Vec::new();
        for part in [
            "<|im_start|>system\n",
            "convert the provided text to speech reference to the following:\n\nText:\n",
            format_reference_text(reference_text).as_str(),
            "\n\nSpeech:\n",
        ] {
            head.extend(self.encode(part)?);
        }
        let mut tail = self.encode("<|im_end|>\n")?;
        tail.extend(self.encode("<|im_start|>user\n")?);

        let total = head.len() + codes.frames + tail.len();
        // Leave room for the target text and at least a little audio.
        if total + 64 >= self.manifest.max_seq_len {
            bail!(
                "Reference voice prompt is too long ({total} positions); use a shorter reference clip or transcript"
            );
        }

        let mut sessions = self.ar_pool.acquire();
        let sessions = &mut *sessions;
        self.reset_state(&mut sessions.state, None);
        let mut column = vec![0i64; width];
        let mut position = 0usize;
        for token in &head {
            column.fill(0);
            column[0] = *token;
            self.slow_step(&mut sessions.slow, &mut sessions.state, &column, position, false)?;
            position += 1;
        }
        for frame in 0..codes.frames {
            column[0] = codes.data[frame] + self.manifest.semantic_begin_id;
            for codebook in 0..self.manifest.num_codebooks {
                column[1 + codebook] = codes.data[codebook * codes.frames + frame];
            }
            self.slow_step(&mut sessions.slow, &mut sessions.state, &column, position, false)?;
            position += 1;
        }
        for token in &tail {
            column.fill(0);
            column[0] = *token;
            self.slow_step(&mut sessions.slow, &mut sessions.state, &column, position, false)?;
            position += 1;
        }

        let rows = self.manifest.num_layers * self.manifest.n_local_heads;
        let dim = self.manifest.head_dim;
        let stride = self.manifest.max_seq_len * dim;
        let mut keys = Vec::with_capacity(rows * position * dim);
        let mut values = Vec::with_capacity(rows * position * dim);
        for row in 0..rows {
            keys.extend_from_slice(&sessions.state.keys[row * stride..row * stride + position * dim]);
            values.extend_from_slice(&sessions.state.values[row * stride..row * stride + position * dim]);
        }
        Ok(VoicePrefix {
            len: position,
            keys,
            values,
            conv: sessions.state.conv.clone(),
            ssm: sessions.state.ssm.clone(),
        })
    }

    /// Zeroes whatever the previous generation wrote and optionally restores a voice prefix.
    fn reset_state(&self, state: &mut SlowState, prefix: Option<&VoicePrefix>) {
        let rows = self.manifest.num_layers * self.manifest.n_local_heads;
        let dim = self.manifest.head_dim;
        let stride = self.manifest.max_seq_len * dim;
        let used = state.used_len * dim;
        for row in 0..rows {
            state.keys[row * stride..row * stride + used].fill(0.0);
            state.values[row * stride..row * stride + used].fill(0.0);
        }
        state.used_len = 0;
        match prefix {
            Some(prefix) => {
                let span = prefix.len * dim;
                for row in 0..rows {
                    state.keys[row * stride..row * stride + span]
                        .copy_from_slice(&prefix.keys[row * span..(row + 1) * span]);
                    state.values[row * stride..row * stride + span]
                        .copy_from_slice(&prefix.values[row * span..(row + 1) * span]);
                }
                state.conv.copy_from_slice(&prefix.conv);
                state.ssm.copy_from_slice(&prefix.ssm);
                state.used_len = prefix.len;
            }
            None => {
                state.conv.fill(0.0);
                state.ssm.fill(0.0);
            }
        }
    }

    fn slow_step(
        &self,
        slow: &mut Session,
        state: &mut SlowState,
        column: &[i64],
        position: usize,
        want_output: bool,
    ) -> Result<Option<(Vec<f32>, Vec<f32>)>> {
        if position >= self.manifest.max_seq_len {
            bail!("Audio8 slow position {position} exceeds the model context length");
        }
        let position_value = [position as i64];
        let outputs = slow
            .run(ort::inputs![
                "codes" => TensorRef::from_array_view((vec![1i64, column.len() as i64, 1], column)).map_err(ort_err)?,
                "position" => TensorRef::from_array_view((vec![1i64], &position_value[..])).map_err(ort_err)?,
                "cache_keys" => TensorRef::from_array_view((self.cache_shape.clone(), &state.keys[..])).map_err(ort_err)?,
                "cache_values" => TensorRef::from_array_view((self.cache_shape.clone(), &state.values[..])).map_err(ort_err)?,
                "conv_states" => TensorRef::from_array_view((self.conv_shape.clone(), &state.conv[..])).map_err(ort_err)?,
                "ssm_states" => TensorRef::from_array_view((self.ssm_shape.clone(), &state.ssm[..])).map_err(ort_err)?,
            ])
            .map_err(ort_err)
            .context("Audio8 slow AR step failed")?;

        let rows = self.manifest.num_layers * self.manifest.n_local_heads;
        let dim = self.manifest.head_dim;
        let stride = self.manifest.max_seq_len * dim;
        let (_, key_delta) = outputs["key_delta"].try_extract_tensor::<f32>().map_err(ort_err)?;
        let (_, value_delta) = outputs["value_delta"].try_extract_tensor::<f32>().map_err(ort_err)?;
        if key_delta.len() != rows * dim || value_delta.len() != rows * dim {
            bail!("Audio8 slow graph returned an unexpected cache delta size");
        }
        for row in 0..rows {
            let target = row * stride + position * dim;
            state.keys[target..target + dim].copy_from_slice(&key_delta[row * dim..(row + 1) * dim]);
            state.values[target..target + dim].copy_from_slice(&value_delta[row * dim..(row + 1) * dim]);
        }
        let (_, next_conv) = outputs["next_conv_states"].try_extract_tensor::<f32>().map_err(ort_err)?;
        let (_, next_ssm) = outputs["next_ssm_states"].try_extract_tensor::<f32>().map_err(ort_err)?;
        if next_conv.len() != state.conv.len() || next_ssm.len() != state.ssm.len() {
            bail!("Audio8 slow graph returned an unexpected recurrent state size");
        }
        state.conv.copy_from_slice(next_conv);
        state.ssm.copy_from_slice(next_ssm);
        state.used_len = usize::max(state.used_len, position + 1);

        if !want_output {
            return Ok(None);
        }
        let (_, logits) = outputs["logits"].try_extract_tensor::<f32>().map_err(ort_err)?;
        let (_, hidden) = outputs["hidden"].try_extract_tensor::<f32>().map_err(ort_err)?;
        let logits_size = self.manifest.codebook_size + 1;
        if logits.len() < logits_size || hidden.len() < self.fast_hidden_dim {
            bail!("Audio8 slow graph returned unexpected logits/hidden sizes");
        }
        Ok(Some((
            logits[logits.len() - logits_size..].to_vec(),
            hidden[hidden.len() - self.fast_hidden_dim..].to_vec(),
        )))
    }

    fn fast_step(
        &self,
        fast: &mut Session,
        caches: &mut [Vec<f32>],
        hidden: &[f32],
        token_id: i64,
        use_slow_hidden: bool,
        position: usize,
        want_logits: bool,
    ) -> Result<Option<Vec<f32>>> {
        let token_value = [token_id];
        let flag_value = [use_slow_hidden];
        let position_value = [position as i64];
        let mut inputs = ort::inputs![
            "slow_hidden" => TensorRef::from_array_view((vec![1i64, 1, hidden.len() as i64], hidden)).map_err(ort_err)?,
            "token_id" => TensorRef::from_array_view((vec![1i64, 1], &token_value[..])).map_err(ort_err)?,
            "use_slow_hidden" => TensorRef::from_array_view((vec![1i64], &flag_value[..])).map_err(ort_err)?,
            "input_pos" => TensorRef::from_array_view((vec![1i64], &position_value[..])).map_err(ort_err)?,
        ];
        for (index, (key_name, value_name, _, _)) in self.fast_cache_names.iter().enumerate() {
            inputs.push((
                key_name.as_str().into(),
                TensorRef::from_array_view((self.fast_cache_shape.clone(), &caches[index * 2][..]))
                    .map_err(ort_err)?
                    .into(),
            ));
            inputs.push((
                value_name.as_str().into(),
                TensorRef::from_array_view((self.fast_cache_shape.clone(), &caches[index * 2 + 1][..]))
                    .map_err(ort_err)?
                    .into(),
            ));
        }
        let outputs = fast.run(inputs).map_err(ort_err).context("Audio8 fast AR step failed")?;

        let heads = self.manifest.fast_n_local_heads;
        let dim = self.manifest.fast_head_dim;
        let slots = self.manifest.num_codebooks;
        for (index, (_, _, key_delta_name, value_delta_name)) in self.fast_cache_names.iter().enumerate() {
            for (offset, name) in [(0usize, key_delta_name), (1usize, value_delta_name)] {
                let (_, delta) = outputs[name.as_str()].try_extract_tensor::<f32>().map_err(ort_err)?;
                let cache = &mut caches[index * 2 + offset];
                if delta.len() == cache.len() {
                    cache.copy_from_slice(delta);
                } else if delta.len() == heads * dim {
                    for head in 0..heads {
                        let target = (head * slots + position) * dim;
                        cache[target..target + dim].copy_from_slice(&delta[head * dim..(head + 1) * dim]);
                    }
                } else {
                    bail!("Audio8 fast graph returned an unexpected cache delta size");
                }
            }
        }

        if !want_logits {
            return Ok(None);
        }
        let (_, logits) = outputs["logits"].try_extract_tensor::<f32>().map_err(ort_err)?;
        let size = self.manifest.codebook_size;
        if logits.len() < size {
            bail!("Audio8 fast graph returned unexpected logits size");
        }
        Ok(Some(logits[logits.len() - size..].to_vec()))
    }

    fn sample_semantic(&self, logits: &[f32], previous: &[i64], params: &GenParams, rng: &mut StdRng) -> i64 {
        let begin = self.manifest.semantic_begin_id;
        let end = self.manifest.semantic_end_id;
        let stop = self.manifest.im_end_id;
        let eos_index = logits.len() - 1;
        let mut values = logits.to_vec();
        if previous.is_empty() {
            // The first frame is always required; EOS stays available on every later step.
            values[eos_index] = f32::NEG_INFINITY;
        }
        let normal_index = sample_logits(&values, params.temperature, params.top_p, params.top_k, rng);
        let high_index = sample_logits(&values, 1.0, 0.9, params.top_k, rng);
        let normal = if normal_index == eos_index { stop } else { begin + normal_index as i64 };
        let high = if high_index == eos_index { stop } else { begin + high_index as i64 };
        if normal >= begin && normal <= end && previous.contains(&normal) {
            high
        } else {
            normal
        }
    }

    /// Generates codec frames for `text`. `on_frame` receives each frame's ten codes.
    /// Returns the number of frames generated; stops early when `cancel` is set.
    pub fn generate<F>(
        &self,
        prefix: &VoicePrefix,
        text: &str,
        params: &GenParams,
        cancel: &AtomicBool,
        mut on_frame: F,
    ) -> Result<usize>
    where
        F: FnMut(&[i64]) -> Result<()>,
    {
        let target = clean_text(text);
        if target.is_empty() {
            bail!("text must not be empty");
        }
        let mut tokens = self.encode(&target)?;
        tokens.extend_from_slice(&self.suffix_tokens);
        let prompt_len = prefix.len + tokens.len();
        if prompt_len >= self.manifest.max_seq_len {
            bail!(
                "Prompt length {prompt_len} exceeds the Audio8 context length {}",
                self.manifest.max_seq_len
            );
        }
        let max_new = usize::min(params.max_new_frames, self.manifest.max_seq_len - prompt_len);
        let codebooks = self.manifest.num_codebooks;
        let width = codebooks + 1;
        let begin = self.manifest.semantic_begin_id;
        let stop = self.manifest.im_end_id;
        let mut rng = match params.seed {
            Some(seed) => StdRng::seed_from_u64(seed),
            None => StdRng::from_entropy(),
        };

        let mut sessions = self.ar_pool.acquire();
        let sessions = &mut *sessions;
        self.reset_state(&mut sessions.state, Some(prefix));

        let mut column = vec![0i64; width];
        let mut output: Option<(Vec<f32>, Vec<f32>)> = None;
        for (index, token) in tokens.iter().enumerate() {
            if cancel.load(Ordering::SeqCst) {
                return Ok(0);
            }
            column.fill(0);
            column[0] = *token;
            let last = index + 1 == tokens.len();
            output = self.slow_step(&mut sessions.slow, &mut sessions.state, &column, prefix.len + index, last)?;
        }
        let (mut logits, mut hidden) = output.ok_or_else(|| anyhow!("Audio8 prompt produced no logits"))?;

        let mut previous: Vec<i64> = Vec::new();
        let mut frames = 0usize;
        for step in 0..max_new {
            if cancel.load(Ordering::SeqCst) {
                return Ok(frames);
            }
            let semantic = self.sample_semantic(&logits, &previous, params, &mut rng);
            if semantic == stop {
                break;
            }
            previous.push(semantic);
            if previous.len() > 10 {
                previous.remove(0);
            }
            let first_code = semantic - begin;
            if first_code < 0 || first_code >= self.manifest.codebook_size as i64 {
                bail!("Audio8 semantic token {semantic} is outside the codebook range");
            }

            for cache in sessions.fast_caches.iter_mut() {
                cache.fill(0.0);
            }
            self.fast_step(&mut sessions.fast, &mut sessions.fast_caches, &hidden, 0, true, 0, false)?;
            column[0] = semantic;
            column[1] = first_code;
            let mut token = first_code;
            for fast_position in 1..codebooks {
                let fast_logits = self
                    .fast_step(
                        &mut sessions.fast,
                        &mut sessions.fast_caches,
                        &hidden,
                        token,
                        false,
                        fast_position,
                        true,
                    )?
                    .ok_or_else(|| anyhow!("Audio8 fast graph returned no logits"))?;
                token = sample_logits(&fast_logits, params.temperature, params.top_p, params.top_k, &mut rng) as i64;
                column[1 + fast_position] = token;
            }
            on_frame(&column[1..])?;
            frames += 1;
            if step + 1 >= max_new {
                break;
            }
            let next = self
                .slow_step(&mut sessions.slow, &mut sessions.state, &column, prompt_len + step, true)?
                .ok_or_else(|| anyhow!("Audio8 slow graph returned no logits"))?;
            logits = next.0;
            hidden = next.1;
        }
        Ok(frames)
    }

    /// Decodes frames `[start, end)` of a frame-major code buffer to mono f32 audio.
    pub fn decode(&self, frame_major: &[i64], start: usize, end: usize) -> Result<Vec<f32>> {
        let codebooks = self.manifest.num_codebooks;
        if end <= start || end * codebooks > frame_major.len() {
            bail!("Invalid Audio8 decode range {start}..{end}");
        }
        let count = end - start;
        let mut codes = vec![0i64; codebooks * count];
        for frame in 0..count {
            for codebook in 0..codebooks {
                codes[codebook * count + frame] = frame_major[(start + frame) * codebooks + codebook];
            }
        }
        let mut decoder = self.decoder_pool.acquire();
        let outputs = decoder
            .run(ort::inputs![
                "codes" => TensorRef::from_array_view((vec![1i64, codebooks as i64, count as i64], &codes[..])).map_err(ort_err)?,
            ])
            .map_err(ort_err)
            .context("Audio8 codec decode failed")?;
        let (_, audio) = outputs[0].try_extract_tensor::<f32>().map_err(ort_err)?;
        Ok(audio.to_vec())
    }

    /// Generates `chunks` in order and emits their audio through `on_audio` as it becomes
    /// available. Returns `false` if canceled.
    ///
    /// Three kinds of threads cooperate:
    ///   • generators (`ar_pool_size`) — each produces the codec frames of one chunk
    ///   • decoders (`decoder_pool_size`) — turn frames into audio; the chunk being played
    ///     is always served first, in small windows, so the first audio arrives after
    ///     `first_frames` frames instead of after a whole chunk; idle decoders work on
    ///     chunks generated ahead
    ///   • the calling thread — releases decoded audio strictly in order via `on_audio`
    pub fn synthesize_chunks<F>(
        &self,
        prefix: &VoicePrefix,
        chunks: &[ChunkPlan],
        first_frames: usize,
        cancel: &AtomicBool,
        mut on_audio: F,
    ) -> Result<bool>
    where
        F: FnMut(&[f32]) -> Result<()>,
    {
        #[derive(Default)]
        struct ChunkState {
            frames: Vec<i64>,
            done: bool,
            /// A decoder is working on this chunk; windows of one chunk must stay in order.
            decoding: bool,
            decoded: usize,
            held_back: Vec<f32>,
            pending_audio: Vec<f32>,
        }
        struct Shared {
            chunks: Vec<ChunkState>,
            next_to_start: usize,
            head: usize,
            error: Option<anyhow::Error>,
        }
        struct Job {
            index: usize,
            window_start: usize,
            window_end: usize,
            decoded: usize,
            is_final: bool,
            codes: Vec<i64>,
        }

        let total_chunks = chunks.len();
        if total_chunks == 0 {
            return Ok(true);
        }
        let codebooks = self.manifest.num_codebooks;
        let hop = self.manifest.codec_hop_length;
        let guard_frames = self.manifest.stream_guard_frames;
        // On the CPU, re-decoding a chunk from its start for every window would cost more
        // than generation, so later windows only get a few frames of left context and are
        // an approximation. A GPU decoder is fast enough to decode from the start every
        // time, which is exact, and to do it for smaller steps.
        let gpu_decoder = self.decoder_device != DecoderDevice::Cpu;
        let (context_frames, head_min_new) = if gpu_decoder {
            (usize::MAX, GPU_STREAM_MIN_NEW_FRAMES)
        } else {
            (STREAM_CONTEXT_FRAMES, STREAM_MIN_NEW_FRAMES)
        };
        let shared = Mutex::new(Shared {
            chunks: (0..total_chunks).map(|_| ChunkState::default()).collect(),
            next_to_start: 0,
            head: 0,
            error: None,
        });
        let wake = Condvar::new();
        let abort = AtomicBool::new(false);
        let lock = || shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let stopping = || abort.load(Ordering::SeqCst) || cancel.load(Ordering::SeqCst);
        let fail = |err: anyhow::Error| {
            let mut guard = lock();
            if !abort.load(Ordering::SeqCst) && guard.error.is_none() {
                guard.error = Some(err);
            }
            wake.notify_all();
        };

        std::thread::scope(|scope| -> Result<bool> {
            for _ in 0..usize::min(self.ar_pool_size, total_chunks) {
                scope.spawn(|| loop {
                    let index = {
                        let mut guard = lock();
                        loop {
                            if stopping() || guard.next_to_start >= total_chunks {
                                return;
                            }
                            if guard.next_to_start <= guard.head + MAX_CHUNKS_AHEAD {
                                break;
                            }
                            guard = wake.wait(guard).unwrap_or_else(|poisoned| poisoned.into_inner());
                        }
                        guard.next_to_start += 1;
                        guard.next_to_start - 1
                    };
                    let plan = &chunks[index];
                    let result = self.generate(prefix, &plan.text, &plan.params, cancel, |frame| {
                        if abort.load(Ordering::SeqCst) {
                            bail!("Audio8 generation aborted");
                        }
                        let mut guard = lock();
                        guard.chunks[index].frames.extend_from_slice(frame);
                        wake.notify_all();
                        Ok(())
                    });
                    {
                        let mut guard = lock();
                        guard.chunks[index].done = true;
                        wake.notify_all();
                    }
                    if let Err(err) = result {
                        fail(err);
                    }
                });
            }

            for _ in 0..self.decoder_pool_size {
                scope.spawn(|| loop {
                    let job = {
                        let mut guard = lock();
                        loop {
                            if stopping() || guard.head >= total_chunks {
                                return;
                            }
                            let head = guard.head;
                            let last = usize::min(total_chunks, head + MAX_CHUNKS_AHEAD + 1);
                            let mut picked: Option<Job> = None;
                            for index in head..last {
                                let chunk = &guard.chunks[index];
                                if chunk.decoding {
                                    continue;
                                }
                                let available = chunk.frames.len() / codebooks;
                                let is_head = index == head;
                                let threshold = if !is_head {
                                    LOOKAHEAD_MIN_NEW_FRAMES
                                } else if index == 0 && chunk.decoded == 0 {
                                    usize::max(1, first_frames)
                                } else {
                                    head_min_new
                                };
                                let ready = if chunk.done {
                                    available > chunk.decoded
                                } else {
                                    available >= chunk.decoded + guard_frames + threshold
                                };
                                if !ready {
                                    continue;
                                }
                                let window_end = if is_head {
                                    available
                                } else {
                                    usize::min(available, chunk.decoded + guard_frames + LOOKAHEAD_MAX_WINDOW_FRAMES)
                                };
                                let window_start = chunk.decoded.saturating_sub(context_frames);
                                picked = Some(Job {
                                    index,
                                    window_start,
                                    window_end,
                                    decoded: chunk.decoded,
                                    is_final: chunk.done && window_end == available,
                                    codes: chunk.frames[window_start * codebooks..window_end * codebooks].to_vec(),
                                });
                                break;
                            }
                            if let Some(job) = picked {
                                guard.chunks[job.index].decoding = true;
                                break job;
                            }
                            guard = wake.wait(guard).unwrap_or_else(|poisoned| poisoned.into_inner());
                        }
                    };

                    let audio = match self.decode(&job.codes, 0, job.window_end - job.window_start) {
                        Ok(audio) => audio,
                        Err(err) => {
                            fail(err);
                            return;
                        }
                    };
                    let stable = if job.is_final {
                        job.window_end
                    } else {
                        job.window_end - guard_frames
                    };
                    let from = usize::min((job.decoded - job.window_start) * hop, audio.len());
                    let to = if job.is_final {
                        audio.len()
                    } else {
                        usize::min((stable - job.window_start) * hop, audio.len())
                    };
                    let mut piece = audio[from..to].to_vec();

                    let mut guard = lock();
                    let chunk = &mut guard.chunks[job.index];
                    // Blend with what the previous window predicted for the same samples.
                    let blend = usize::min(usize::min(chunk.held_back.len(), piece.len()), STREAM_CROSSFADE_SAMPLES);
                    for index in 0..blend {
                        let weight = (index as f32 + 1.0) / (blend as f32 + 1.0);
                        piece[index] = chunk.held_back[index] * (1.0 - weight) + piece[index] * weight;
                    }
                    chunk.held_back = audio[to..].to_vec();
                    chunk.decoded = stable;
                    chunk.pending_audio.extend_from_slice(&piece);
                    chunk.decoding = false;
                    wake.notify_all();
                });
            }

            let outcome: Result<bool> = (|| {
                loop {
                    let audio = {
                        let mut guard = lock();
                        loop {
                            if let Some(err) = guard.error.take() {
                                return Err(err);
                            }
                            if cancel.load(Ordering::SeqCst) {
                                return Ok(false);
                            }
                            if guard.head >= total_chunks {
                                return Ok(true);
                            }
                            let head = guard.head;
                            // Audio decoded for a chunk is released as soon as it is the head.
                            if !guard.chunks[head].pending_audio.is_empty() {
                                break std::mem::take(&mut guard.chunks[head].pending_audio);
                            }
                            let chunk = &mut guard.chunks[head];
                            if chunk.done && !chunk.decoding && chunk.decoded >= chunk.frames.len() / codebooks {
                                chunk.frames = Vec::new();
                                chunk.held_back = Vec::new();
                                guard.head += 1;
                                wake.notify_all();
                                continue;
                            }
                            guard = wake.wait(guard).unwrap_or_else(|poisoned| poisoned.into_inner());
                        }
                    };
                    on_audio(&audio)?;
                }
            })();

            // Whatever the outcome, stop the workers before the scope joins them.
            abort.store(true, Ordering::SeqCst);
            {
                let _guard = lock();
                wake.notify_all();
            }
            outcome
        })
    }

    /// Encodes reference audio (mono, model sample rate) to codec codes for voice cloning.
    /// The encoder is loaded only for the duration of this call.
    pub fn encode_reference(&self, samples: &[f32]) -> Result<VoiceCodes> {
        let registration_dir = self.model_dir.join("registration");
        let encoder_path = registration_dir.join("codec_encoder_fp16.onnx");
        if !encoder_path.is_file() {
            bail!(
                "Audio8 voice registration files are missing ({}). Re-download the model.",
                encoder_path.display()
            );
        }
        // Codes from an encoder built for another model version would be meaningless here.
        let registration_manifest = registration_dir.join("registration_manifest.json");
        if let Ok(body) = std::fs::read_to_string(&registration_manifest) {
            let fingerprint = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| value.get("model_fingerprint").and_then(|v| v.as_str()).map(str::to_string));
            if fingerprint.is_some_and(|fingerprint| fingerprint != self.manifest.model_fingerprint) {
                bail!("Audio8 voice registration files do not match the installed model. Re-download the model.");
            }
        }
        let seconds = samples.len() as f32 / self.manifest.sample_rate as f32;
        if !(MIN_REFERENCE_SECONDS..=MAX_REFERENCE_SECONDS).contains(&seconds) {
            bail!("Reference audio must be between 0.5 and 30 seconds (got {seconds:.1} s)");
        }
        if samples.iter().any(|sample| !sample.is_finite()) {
            bail!("Reference audio contains non-finite samples");
        }
        let frame = self.manifest.codec_hop_length;
        let padded_len = samples.len().div_ceil(frame) * frame;
        let mut audio: Vec<half::f16> = samples.iter().map(|sample| half::f16::from_f32(*sample)).collect();
        audio.resize(padded_len, half::f16::from_f32(0.0));

        let mut encoder = build_session(&encoder_path, 4)?;
        let input_name = encoder
            .inputs()
            .first()
            .map(|outlet| outlet.name().to_string())
            .ok_or_else(|| anyhow!("Audio8 codec encoder has no inputs"))?;
        let outputs = encoder
            .run(ort::inputs![
                input_name.as_str() => TensorRef::from_array_view((vec![1i64, 1, padded_len as i64], &audio[..])).map_err(ort_err)?,
            ])
            .map_err(ort_err)
            .context("Audio8 codec encode failed")?;
        let (shape, data) = outputs[0].try_extract_tensor::<i64>().map_err(ort_err)?;
        let dims: Vec<i64> = shape.iter().copied().collect();
        let codebooks = self.manifest.num_codebooks;
        let frames = match dims.as_slice() {
            [1, books, frames] if *books as usize == codebooks => *frames as usize,
            [books, frames] if *books as usize == codebooks => *frames as usize,
            _ => bail!("Audio8 codec encoder returned unexpected shape {dims:?}"),
        };
        let codes = VoiceCodes {
            frames,
            data: data.to_vec(),
        };
        self.validate_codes(&codes)?;
        Ok(codes)
    }
}

/// Port of the reference sampler: nucleus + top-k filtering on the untempered
/// distribution, then a temperature-scaled draw from what is left.
fn sample_logits(logits: &[f32], temperature: f32, top_p: f32, top_k: usize, rng: &mut StdRng) -> usize {
    let temperature = f64::max(temperature as f64, 1e-5);
    let top_p = (top_p as f64).clamp(1e-5, 1.0);
    let top_k = top_k.clamp(1, logits.len());

    let mut order: Vec<usize> = (0..logits.len()).collect();
    if top_k < order.len() {
        order.select_nth_unstable_by(top_k - 1, |a, b| logits[*b].total_cmp(&logits[*a]));
        order.truncate(top_k);
    }
    order.sort_unstable_by(|a, b| logits[*b].total_cmp(&logits[*a]));

    let max_logit = logits[order[0]] as f64;
    let total: f64 = logits.iter().map(|value| ((*value as f64) - max_logit).exp()).sum();
    let mut kept = 0usize;
    let mut cumulative = 0.0f64;
    for (rank, index) in order.iter().enumerate() {
        cumulative += ((logits[*index] as f64) - max_logit).exp() / total;
        if rank > 0 && cumulative > top_p {
            break;
        }
        kept = rank + 1;
    }

    let weights: Vec<f64> = order[..kept]
        .iter()
        .map(|index| (((logits[*index] as f64) - max_logit) / temperature).exp())
        .collect();
    let weight_sum: f64 = weights.iter().sum();
    let mut draw = rng.gen::<f64>() * weight_sum;
    for (rank, weight) in weights.iter().enumerate() {
        draw -= weight;
        if draw <= 0.0 {
            return order[rank];
        }
    }
    order[kept - 1]
}

fn is_line_break(ch: char) -> bool {
    matches!(ch, '\r' | '\n' | '\u{0B}' | '\u{0C}' | '\u{1C}'..='\u{1E}' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// Strips invisible characters and collapses whitespace. A whitespace run containing a
/// line break between two CJK characters is removed entirely instead of becoming a space.
pub fn clean_text(text: &str) -> String {
    let chars: Vec<char> = text.chars().filter(|ch| ch.is_whitespace() || !is_invisible(*ch)).collect();
    let mut output = String::with_capacity(text.len());
    let mut index = 0usize;
    while index < chars.len() {
        if !chars[index].is_whitespace() {
            output.push(chars[index]);
            index += 1;
            continue;
        }
        let start = index;
        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }
        let has_break = chars[start..index].iter().any(|ch| is_line_break(*ch));
        let left_cjk = start > 0 && is_cjk(chars[start - 1]);
        let right_cjk = index < chars.len() && is_cjk(chars[index]);
        if !(has_break && left_cjk && right_cjk) {
            output.push(' ');
        }
    }
    output.trim().to_string()
}

fn format_reference_text(text: &str) -> String {
    let cleaned = clean_text(text);
    let has_speaker_tag = cleaned.match_indices("<|speaker:").any(|(start, tag)| {
        let rest = &cleaned[start + tag.len()..];
        let digits = rest.chars().take_while(|ch| ch.is_ascii_digit()).count();
        digits > 0 && rest[digits..].starts_with("|>")
    });
    if has_speaker_tag {
        cleaned
    } else {
        format!("<|speaker:0|>{cleaned}")
    }
}

/// Upper bound on frames for a chunk, so a generation that never emits EOS cannot run away.
pub fn max_frames_for_text(text: &str, sample_rate: u32, hop: usize) -> usize {
    let frames_per_second = sample_rate as f32 / hop as f32;
    let estimated_seconds = text_units(text) / 15.0;
    (estimated_seconds * frames_per_second * 2.5) as usize + 48
}

/// Reads a `[codebooks, frames]` integer array saved by numpy (`.npy`, C order).
pub fn read_codes_npy(path: &Path, codebooks: usize) -> Result<VoiceCodes> {
    let bytes = std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
    if bytes.len() < 10 || &bytes[..6] != b"\x93NUMPY" {
        bail!("{} is not a .npy file", path.display());
    }
    let (header_len, header_start) = match bytes[6] {
        1 => (u16::from_le_bytes([bytes[8], bytes[9]]) as usize, 10usize),
        2 | 3 => {
            if bytes.len() < 12 {
                bail!("{} has a truncated .npy header", path.display());
            }
            (
                u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize,
                12usize,
            )
        }
        version => bail!("Unsupported .npy version {version} in {}", path.display()),
    };
    let data_start = header_start + header_len;
    if bytes.len() < data_start {
        bail!("{} has a truncated .npy header", path.display());
    }
    let header = String::from_utf8_lossy(&bytes[header_start..data_start]).to_string();
    if header.contains("'fortran_order': True") {
        bail!("{} uses Fortran order, which is not supported", path.display());
    }
    let descr = header
        .split("'descr':")
        .nth(1)
        .and_then(|rest| rest.split('\'').nth(1))
        .ok_or_else(|| anyhow!("{} has no dtype in its .npy header", path.display()))?
        .to_string();
    let shape: Vec<usize> = header
        .split("'shape':")
        .nth(1)
        .and_then(|rest| rest.split('(').nth(1))
        .and_then(|rest| rest.split(')').next())
        .map(|dims| dims.split(',').filter_map(|dim| dim.trim().parse::<usize>().ok()).collect())
        .unwrap_or_default();
    if shape.len() != 2 || shape[0] != codebooks || shape[1] == 0 {
        bail!("{} must have shape [{codebooks}, T>0], found {shape:?}", path.display());
    }
    let count = shape[0] * shape[1];
    let payload = &bytes[data_start..];
    let width = match descr.as_str() {
        "<u2" | "<i2" => 2,
        "<i4" | "<u4" => 4,
        "<i8" | "<u8" => 8,
        "|u1" | "|i1" => 1,
        other => bail!("Unsupported .npy dtype `{other}` in {}", path.display()),
    };
    if payload.len() < count * width {
        bail!("{} is truncated", path.display());
    }
    let data: Vec<i64> = (0..count)
        .map(|index| {
            let raw = &payload[index * width..(index + 1) * width];
            match width {
                1 => raw[0] as i64,
                2 => u16::from_le_bytes([raw[0], raw[1]]) as i64,
                4 => u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as i64,
                _ => i64::from_le_bytes([raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7]]),
            }
        })
        .collect();
    Ok(VoiceCodes { frames: shape[1], data })
}

/// Writes codes as a little-endian `uint16` `.npy`, the format the reference runtime uses.
pub fn write_codes_npy(path: &Path, codes: &VoiceCodes, codebooks: usize) -> Result<()> {
    let mut header = format!(
        "{{'descr': '<u2', 'fortran_order': False, 'shape': ({codebooks}, {}), }}",
        codes.frames
    );
    // Header (magic + version + length + dict) is padded to a multiple of 64 and ends in \n.
    let unpadded = 10 + header.len() + 1;
    header.push_str(&" ".repeat((64 - unpadded % 64) % 64));
    header.push('\n');
    let mut bytes: Vec<u8> = Vec::with_capacity(10 + header.len() + codes.data.len() * 2);
    bytes.extend_from_slice(b"\x93NUMPY\x01\x00");
    bytes.extend_from_slice(&(header.len() as u16).to_le_bytes());
    bytes.extend_from_slice(header.as_bytes());
    for code in &codes.data {
        bytes.extend_from_slice(&(*code as u16).to_le_bytes());
    }
    std::fs::write(path, bytes).with_context(|| format!("Failed to write {}", path.display()))
}

/// Decodes WAV bytes to mono f32 at `target_rate`.
pub fn load_wav_mono(wav_bytes: &[u8], target_rate: u32) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::new(std::io::Cursor::new(wav_bytes))
        .map_err(|err| anyhow!("Unsupported or invalid WAV file: {err}"))?;
    let spec = reader.spec();
    let channels = usize::max(1, spec.channels as usize);
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<std::result::Result<Vec<f32>, _>>()
            .map_err(|err| anyhow!("Failed to read WAV samples: {err}"))?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1i64 << (spec.bits_per_sample.max(1) - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|sample| sample.map(|value| value as f32 * scale))
                .collect::<std::result::Result<Vec<f32>, _>>()
                .map_err(|err| anyhow!("Failed to read WAV samples: {err}"))?
        }
    };
    let mono: Vec<f32> = interleaved
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect();
    if mono.is_empty() {
        bail!("WAV file contains no audio");
    }
    Ok(resample(&mono, spec.sample_rate, target_rate))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checks the native Rust/plugin path, including actual GPU kernel placement.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    #[ignore = "needs the Audio8 model, WebGPU plugin and a Metal GPU"]
    fn webgpu_decodes_reference_audio() {
        crate::audio8_local::ensure_onnxruntime().unwrap();
        let model_dir = PathBuf::from(std::env::var("VOICEREADER_AUDIO8_TEST_MODEL_DIR").unwrap());
        let output_dir = PathBuf::from(std::env::var("VOICEREADER_AUDIO8_TEST_OUTPUT_DIR").unwrap());
        std::fs::create_dir_all(&output_dir).unwrap();
        let manifest: Manifest = serde_json::from_str(
            &std::fs::read_to_string(model_dir.join("runtime_manifest.json")).unwrap(),
        ).unwrap();
        let codes = read_codes_npy(&model_dir.join("reference_codes.npy"), manifest.num_codebooks).unwrap();
        let mut outputs = Vec::new();
        for device in [DecoderDevice::Cpu, DecoderDevice::WebGpu] {
            let mut session = build_decoder_session(&model_dir.join("codec_decoder_fp16.onnx"), device, 8).unwrap();
            let mut pcm = Vec::new();
            // Warm-up first, then report a repeat of the same full reference clip.
            for _ in 0..2 {
                let started = std::time::Instant::now();
                let result = session.run(ort::inputs![
                    "codes" => TensorRef::from_array_view((
                        vec![1i64, manifest.num_codebooks as i64, codes.frames as i64], &codes.data[..]
                    )).unwrap(),
                ]).unwrap();
                pcm = result[0].try_extract_tensor::<f32>().unwrap().1.to_vec();
                println!("{}: {} frames decoded in {:.0} ms", device.label(), codes.frames, started.elapsed().as_secs_f64() * 1000.0);
            }
            assert_eq!(pcm.len(), codes.frames * manifest.codec_hop_length);
            assert!(pcm.iter().all(|sample| sample.is_finite()));
            assert!(pcm.iter().any(|sample| sample.abs() > 0.01), "silent output");
            let profile = session.end_profiling().unwrap();
            let profile: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(profile).unwrap()).unwrap();
            if device == DecoderDevice::WebGpu {
                let gpu_nodes = profile.as_array().unwrap().iter()
                    .filter(|event| event["args"]["provider"] == "WebGpuExecutionProvider").count();
                println!("WebGPU kernel events: {gpu_nodes}");
                assert!(gpu_nodes > 0, "decoder silently ran entirely on CPU");
            }
            let mut wav = hound::WavWriter::create(output_dir.join(format!("reference-{}.wav", device.label())), hound::WavSpec {
                channels: 1, sample_rate: manifest.sample_rate, bits_per_sample: 32, sample_format: hound::SampleFormat::Float,
            }).unwrap();
            for sample in &pcm { wav.write_sample(*sample).unwrap(); }
            wav.finalize().unwrap();
            outputs.push(pcm);
        }
        let signal: f64 = outputs[0].iter().map(|sample| (*sample as f64).powi(2)).sum();
        let difference: f64 = outputs[0].iter().zip(&outputs[1]).map(|(a, b)| (*a as f64 - *b as f64).powi(2)).sum();
        let relative_rms = (difference / signal).sqrt();
        println!("CPU/WebGPU relative RMS difference: {:.4}%", relative_rms * 100.0);
        assert!(relative_rms < 0.001, "Metal decoder differs from CPU by {:.2}%; precision regression", relative_rms * 100.0);
    }

    /// Same deterministic codes through full CPU decoding and actual GPU streaming.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    #[ignore = "needs the Audio8 model, FP32 decoder assets and a Metal GPU"]
    fn webgpu_streaming_matches_cpu_for_generated_speech() {
        crate::audio8_local::ensure_onnxruntime().unwrap();
        let model_dir = PathBuf::from(std::env::var("VOICEREADER_AUDIO8_TEST_MODEL_DIR").unwrap());
        let output_dir = PathBuf::from(std::env::var("VOICEREADER_AUDIO8_TEST_OUTPUT_DIR").unwrap());
        std::fs::create_dir_all(&output_dir).unwrap();
        let model = Audio8Model::load(&model_dir, 1, 1, 8, DecoderDevice::WebGpu).unwrap();
        let (reference_codes, reference_text) = model.builtin_voice().unwrap();
        let prefix = model.build_voice_prefix(&reference_text, &reference_codes).unwrap();
        let text = "VoiceReader reads highlighted text aloud. 今天天气很好，我们一起去公园散步吧。";
        let params = GenParams { seed: Some(42), max_new_frames: 256, ..GenParams::default() };
        let cancel = AtomicBool::new(false);
        let mut frame_codes = Vec::new();
        let frames = model.generate(&prefix, text, &params, &cancel, |codes| {
            frame_codes.extend_from_slice(codes);
            Ok(())
        }).unwrap();
        assert!(frames > 24);
        let codebooks = model.manifest.num_codebooks;
        let mut codes = vec![0i64; frame_codes.len()];
        for book in 0..codebooks {
            for frame in 0..frames {
                codes[book * frames + frame] = frame_codes[frame * codebooks + book];
            }
        }
        let mut cpu = build_decoder_session(&model_dir.join("codec_decoder_fp16.onnx"), DecoderDevice::Cpu, 8).unwrap();
        let result = cpu.run(ort::inputs![
            "codes" => TensorRef::from_array_view((vec![1i64, codebooks as i64, frames as i64], &codes[..])).unwrap(),
        ]).unwrap();
        let cpu_audio = result[0].try_extract_tensor::<f32>().unwrap().1.to_vec();
        let full_gpu_audio = model.decode(&frame_codes, 0, frames).unwrap();
        let mut streamed = Vec::new();
        let started = std::time::Instant::now();
        let mut first_audio = None;
        assert!(model.synthesize_chunks(&prefix, &[ChunkPlan { text: text.to_string(), params }], 12, &cancel, |audio| {
            first_audio.get_or_insert(started.elapsed());
            streamed.extend_from_slice(audio);
            Ok(())
        }).unwrap());
        println!("FP32 Metal stream: first audio {:.2}s, total {:.2}s, output {:.2}s", first_audio.unwrap().as_secs_f64(), started.elapsed().as_secs_f64(), streamed.len() as f64 / model.sample_rate() as f64);
        for (name, audio) in [("parity-speech-webgpu-full", &full_gpu_audio), ("parity-speech-webgpu", &streamed)] {
            assert_eq!(audio.len(), cpu_audio.len(), "samples lost in {name}");
            assert!(audio.iter().all(|sample| sample.is_finite()));
            let signal: f64 = cpu_audio.iter().map(|sample| (*sample as f64).powi(2)).sum();
            let difference: f64 = audio.iter().zip(&cpu_audio).map(|(a, b)| (*a as f64 - *b as f64).powi(2)).sum();
            let relative_rms = (difference / signal).sqrt();
            println!("{name} versus CPU: {:.4}% relative RMS difference", relative_rms * 100.0);
            assert!(relative_rms < 0.001, "{name} differs from CPU by {:.2}%", relative_rms * 100.0);
        }
        for (name, audio) in [("parity-speech-cpu", &cpu_audio), ("parity-speech-webgpu", &streamed)] {
            let mut wav = hound::WavWriter::create(output_dir.join(format!("{name}.wav")), hound::WavSpec {
                channels: 1, sample_rate: model.sample_rate(), bits_per_sample: 32, sample_format: hound::SampleFormat::Float,
            }).unwrap();
            for sample in audio { wav.write_sample(*sample).unwrap(); }
            wav.finalize().unwrap();
        }
    }

    #[test]
    fn clean_text_collapses_whitespace_and_cjk_line_breaks() {
        assert_eq!(clean_text("  Hello \n  world\u{200B}  "), "Hello world");
        assert_eq!(clean_text("你好\n世界"), "你好世界");
        assert_eq!(clean_text("你好 世界"), "你好 世界");
    }

    #[test]
    fn reference_text_gets_speaker_tag_once() {
        assert_eq!(format_reference_text("hello"), "<|speaker:0|>hello");
        assert_eq!(format_reference_text("<|speaker:1|>hello"), "<|speaker:1|>hello");
    }

    #[test]
    fn codes_npy_round_trip() {
        let dir = std::env::temp_dir().join(format!("a8-npy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("codes.npy");
        let codes = VoiceCodes {
            frames: 3,
            data: (0..30).map(|value| value * 100).collect(),
        };
        write_codes_npy(&path, &codes, 10).unwrap();
        let loaded = read_codes_npy(&path, 10).unwrap();
        assert_eq!(loaded.frames, 3);
        assert_eq!(loaded.data, codes.data);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sampler_is_greedy_at_tiny_top_p() {
        let mut rng = StdRng::seed_from_u64(1);
        let logits = [0.1f32, 3.0, 0.5, -1.0];
        for _ in 0..20 {
            assert_eq!(sample_logits(&logits, 0.7, 1e-5, 50, &mut rng), 1);
        }
    }
}
