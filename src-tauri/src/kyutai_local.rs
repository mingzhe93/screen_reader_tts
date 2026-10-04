use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use pocket_tts::{ModelState, TTSModel};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::audio_pipeline::{resolve_sox_path_cached, write_normalized_reference_wav, RateEmitter};
use crate::bundled_paths::{find_bundled_file, search_roots};
use crate::text_chunking::{chunk_text, normalize_for_speech, text_units, SOFT_OVERFLOW};

const DEFAULT_VOICE_ID: &str = "0";
pub(crate) const META_FILE_NAME: &str = "meta.json";
pub(crate) const REF_AUDIO_FILE_NAME: &str = "reference.wav";
const LOCAL_CONFIG_VARIANT: &str = "voicereader-pocket-tts-local";
const RUNTIME_CONFIG_DIR_NAME: &str = "pocket-tts-runtime";
/// Bundled reference clips for presets that have no precomputed embedding
/// (see scripts/fetch-kyutai-voices.js).
const PRESET_CLIPS_DIR_NAME: &str = "kyutai-voices";
/// Preset clips normalized to the model's input format are kept here, under the data dir.
const PRESET_CLIP_CACHE_DIR_NAME: &str = "preset-voices";
/// Pocket TTS is trained on short prompts; its own splitter caps chunks at 50 tokens.
const MAX_TOKENS_PER_CHUNK: usize = 50;
/// Range of the user's chunk size setting that this model can use (about 50 tokens at most).
const MIN_CHUNK_CHARS: f32 = 100.0;
const MAX_CHUNK_CHARS: f32 = 200.0;
/// The first chunk is kept shorter because nothing plays until it is fully generated.
const FIRST_CHUNK_BUDGET: f32 = 100.0;

#[derive(Clone)]
pub enum LocalJobEndState {
    Done,
    Canceled,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SavedVoiceMeta {
    pub voice_id: String,
    pub display_name: String,
    pub created_at: String,
    pub tts_model_id: String,
    pub language_hint: String,
    pub description: Option<String>,
    pub ref_text: Option<String>,
}

pub struct LocalKyutaiRuntime {
    model: Arc<TTSModel>,
    sample_rate: u32,
    voices_dir: PathBuf,
    preset_clip_cache_dir: PathBuf,
    model_dir: PathBuf,
    model_id: String,
    state_cache: HashMap<String, ModelState>,
}

impl LocalKyutaiRuntime {
    pub fn new(model_dir: &Path, data_dir: &Path, model_id: &str, default_preset: &str) -> Result<Self> {
        let config_path = model_dir.join("voicereader-pocket-tts.yaml");
        let weights_path = model_dir.join("tts_b6369a24.safetensors");
        let tokenizer_path = model_dir.join("tokenizer.model");
        for required in [&config_path, &weights_path, &tokenizer_path] {
            if !required.exists() {
                return Err(anyhow!(
                    "Missing Kyutai model asset required by Rust runtime: {}",
                    required.display()
                ));
            }
        }

        let runtime_config_root = materialize_runtime_config(&config_path, model_dir, data_dir)
            .context("Failed to prepare runtime Kyutai config")?;

        let model = load_model_from_runtime_config(&runtime_config_root)
            .context("Failed to initialize Rust Pocket-TTS model from bundled files")?;
        let sample_rate = model.sample_rate as u32;

        let voices_dir = data_dir.join("voices");
        std::fs::create_dir_all(&voices_dir)
            .with_context(|| format!("Failed to create voices directory {}", voices_dir.display()))?;

        let mut runtime = Self {
            model: Arc::new(model),
            sample_rate,
            voices_dir,
            preset_clip_cache_dir: data_dir.join(PRESET_CLIP_CACHE_DIR_NAME),
            model_dir: model_dir.to_path_buf(),
            model_id: model_id.to_string(),
            state_cache: HashMap::new(),
        };

        // Prime voice state and first inference to reduce first-playback clipping on cold start.
        let warmup_state = runtime
            .load_preset_voice_state(default_preset)
            .with_context(|| format!("Failed to load default Kyutai preset voice: {default_preset}"))?;
        let _ = runtime.model.generate("Warmup.", &warmup_state);
        runtime
            .state_cache
            .insert(format!("preset:{default_preset}"), warmup_state);

        Ok(runtime)
    }

    pub fn health_payload(&self, selected_preset: &str) -> Value {
        let sox_detail = resolve_sox_path_cached()
            .map(|path| format!("sox={}", path.display()))
            .unwrap_or_else(|| "sox=unavailable(resample_fallback_pitch_shift)".to_string());
        json!({
            "engine_version": env!("CARGO_PKG_VERSION"),
            "active_model_id": self.model_id,
            "device": "cpu",
            "capabilities": {
                "supports_voice_clone": true,
                "supports_audio_chunk_stream": true,
                "supports_true_streaming_inference": false,
                "languages": ["en"]
            },
            "runtime": {
                "backend": "kyutai_pocket_tts_rust",
                "model_loaded": true,
                "fallback_active": false,
                "detail": format!(
                    "model={}, source={}, preset={}, {}",
                    self.model_id,
                    self.model_dir.display(),
                    selected_preset,
                    sox_detail
                ),
                "supports_default_voice": true,
                "supports_cloned_voices": true,
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

    pub fn list_voices_payload(&self) -> Result<Value> {
        let mut voices = vec![json!({
            "voice_id": DEFAULT_VOICE_ID,
            "display_name": "Default Built-in Voice",
            "created_at": "1970-01-01T00:00:00Z",
            "tts_model_id": self.model_id,
            "language_hint": "auto",
            "description": Value::Null,
        })];

        let mut saved = self.list_saved_voices()?;
        saved.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        for voice in saved {
            voices.push(json!({
                "voice_id": voice.voice_id,
                "display_name": voice.display_name,
                "created_at": voice.created_at,
                "tts_model_id": voice.tts_model_id,
                "language_hint": voice.language_hint,
                "description": voice.description,
            }));
        }
        Ok(json!({ "voices": voices }))
    }

    pub fn clone_voice(
        &mut self,
        display_name: &str,
        wav_bytes: &[u8],
        language: Option<String>,
        ref_text: Option<String>,
    ) -> Result<SavedVoiceMeta> {
        let voice_id = Uuid::new_v4().to_string();
        let voice_dir = self.voice_dir(&voice_id);
        std::fs::create_dir_all(&voice_dir)
            .with_context(|| format!("Failed to create voice directory {}", voice_dir.display()))?;

        let ref_wav_path = voice_dir.join(REF_AUDIO_FILE_NAME);
        write_normalized_reference_wav(&ref_wav_path, wav_bytes)?;

        let state = self
            .model
            .get_voice_state(&ref_wav_path)
            .with_context(|| format!("Failed to create cloned voice state from {}", ref_wav_path.display()))?;
        self.state_cache.insert(format!("voice:{voice_id}"), state);

        let meta = SavedVoiceMeta {
            voice_id: voice_id.clone(),
            display_name: display_name.to_string(),
            created_at: now_unix_timestamp_string(),
            tts_model_id: self.model_id.clone(),
            language_hint: language.unwrap_or_else(|| "en".to_string()),
            description: None,
            ref_text,
        };
        self.write_voice_meta(&meta)?;
        Ok(meta)
    }

    pub fn update_voice(
        &mut self,
        voice_id: &str,
        display_name: &str,
        language: Option<String>,
        description: Option<String>,
    ) -> Result<SavedVoiceMeta> {
        let mut meta = self.read_voice_meta(voice_id)?;
        meta.display_name = display_name.to_string();
        if let Some(lang) = language {
            meta.language_hint = lang;
        }
        meta.description = description;
        self.write_voice_meta(&meta)?;
        Ok(meta)
    }

    pub fn delete_voice(&mut self, voice_id: &str) -> Result<()> {
        if voice_id == DEFAULT_VOICE_ID {
            return Err(anyhow!("Built-in default voice cannot be deleted"));
        }
        let voice_dir = self.voice_dir(voice_id);
        if !voice_dir.exists() {
            return Err(anyhow!("VOICE_NOT_FOUND: {voice_id}"));
        }
        self.state_cache.remove(&format!("voice:{voice_id}"));
        std::fs::remove_dir_all(&voice_dir)
            .with_context(|| format!("Failed to remove {}", voice_dir.display()))?;
        Ok(())
    }

    /// Synthesizes `text` in chunks and streams PCM audio via `on_chunk`.
    ///
    /// Returns `(end_state, had_audio)`.  `had_audio` is `true` if at least one
    /// non-empty PCM chunk was emitted, which the caller needs to decide whether
    /// to broadcast a "had_audio" flag in the terminal event.
    ///
    /// # Parallelism
    /// Multiple CPU cores are used concurrently:
    ///   • Main thread  — SoX push/drain + emit for the current chunk
    ///   • Up to N background threads — pre-generating upcoming chunks
    ///   • SoX subprocess — tempo-stretches PCM in the background
    ///
    /// N = min(available_cores - 1, 4).  On an 8-core machine, up to 4 chunks
    /// are generated concurrently in a sliding window, so by the time the main
    /// thread finishes SoX + emit for chunk i, chunks i+1..i+4 are already
    /// generated (or nearly so) and ready to emit with no pause.
    ///
    /// # `on_chunk` requirements
    /// The bound `Fn + Send + 'static` (instead of `FnMut`) is intentional:
    ///   • It removes the need for the caller to capture mutable state
    ///     (`sent_any_chunk`) in the closure — that flag is now returned.
    ///   • It leaves the door open for a future worker-thread drain loop that
    ///     calls `on_chunk` from a dedicated thread, further overlapping SoX
    ///     processing with model generation.
    pub fn stream_synthesize<F>(
        &mut self,
        voice_id: &str,
        selected_preset: &str,
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
        let text_chunks = self.plan_chunks(text, chunk_max_chars);

        // Resolve the voice state once (cached after first call).
        let voice_state = self.resolve_voice_state(voice_id, selected_preset)?;
        let gain: f32 = volume.clamp(0.0, 2.0);

        // The emitter reads the live rate on every push, so rate changes (including
        // 1.0 -> != 1.0) apply from the next chunk without restarting the stream.
        let mut emitter = RateEmitter::new(self.sample_rate, active_rate_steps, on_chunk);

        // ------------------------------------------------------------------
        // Parallel look-ahead generation
        // ------------------------------------------------------------------
        // We use model.generate() (full batch) so SoX receives a contiguous
        // PCM batch.  Feeding SoX tiny per-token buffers via generate_stream
        // causes output starvation at the frontend (see docs/learnings.md §1).
        //
        // TTSModel::generate() takes &self (shared ref) and ModelState is
        // Clone, so we can run the *next* chunk's generation on a background
        // thread while the main thread pushes the current chunk to SoX,
        // drains, and emits.  This overlaps T_gen(N+1) with SoX(N) + emit(N),
        // eliminating most of the inter-chunk gap.
        //
        //   Main thread:   [gen C0] [sox+emit C0 | join C1] [sox+emit C1 | join C2] …
        //   Look-ahead:              [gen C1]                [gen C2]
        // ------------------------------------------------------------------

        type GenResult = Result<Vec<i16>>;
        type LookAhead = JoinHandle<GenResult>;

        /// Spawn a thread that runs model.generate() and returns PCM i16.
        fn spawn_generate(
            model: &Arc<TTSModel>,
            text: String,
            voice_state: ModelState,
            gain: f32,
        ) -> LookAhead {
            let model = Arc::clone(model);
            std::thread::spawn(move || -> GenResult {
                let tensor = model
                    .generate(&text, &voice_state)
                    .context("Pocket-TTS generation failed (look-ahead)")?;
                let values = tensor
                    .flatten_all()
                    .context("Failed to flatten look-ahead tensor")?
                    .to_vec1::<f32>()
                    .context("Failed to convert look-ahead tensor to f32")?;
                let mut pcm = Vec::with_capacity(values.len());
                for sample in values {
                    let scaled = (sample * gain).clamp(-1.0, 1.0);
                    pcm.push((scaled * 32767.0) as i16);
                }
                Ok(pcm)
            })
        }

        // Determine how many chunks to generate concurrently.
        // Each generate() call uses MKL/BLAS internally (multi-threaded),
        // so we cap concurrency to avoid thread contention.  Reserve 1
        // core for the main thread (SoX + emit) and split the rest among
        // concurrent generate() calls, with a ceiling of 4.
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let look_ahead_depth = cores.saturating_sub(1).max(1).min(4);

        // Pre-submit up to `look_ahead_depth` chunks.
        let mut queue: VecDeque<LookAhead> = VecDeque::new();
        let mut next_to_submit = 0usize;
        while next_to_submit < text_chunks.len() && queue.len() < look_ahead_depth {
            queue.push_back(spawn_generate(
                &self.model,
                text_chunks[next_to_submit].clone(),
                voice_state.clone(),
                gain,
            ));
            next_to_submit += 1;
        }

        for _i in 0..text_chunks.len() {
            if cancel.load(Ordering::SeqCst) {
                drop(queue);
                emitter.abort();
                return Ok((LocalJobEndState::Canceled, emitter.had_audio()));
            }

            // Await the PCM from the earliest queued generation thread.
            let pcm = match queue.pop_front() {
                Some(handle) => handle
                    .join()
                    .map_err(|_| anyhow!("Look-ahead generation thread panicked"))??,
                None => Vec::new(),
            };

            // Refill the queue: submit the next unstarted chunk so
            // look_ahead_depth threads stay in flight.
            if next_to_submit < text_chunks.len() {
                queue.push_back(spawn_generate(
                    &self.model,
                    text_chunks[next_to_submit].clone(),
                    voice_state.clone(),
                    gain,
                ));
                next_to_submit += 1;
            }

            if cancel.load(Ordering::SeqCst) {
                drop(queue);
                emitter.abort();
                return Ok((LocalJobEndState::Canceled, emitter.had_audio()));
            }

            if pcm.is_empty() {
                continue;
            }

            if let Err(err) = emitter.push(&pcm) {
                emitter.abort();
                return Err(err);
            }
        }

        // Flush remaining SoX output for the last chunk.
        emitter.finish()?;
        Ok((LocalJobEndState::Done, emitter.had_audio()))
    }

    /// Splits text into the chunks that are generated one at a time. The crate's own
    /// `split_into_best_sentences` is not used: it cuts at every full stop and colon, which
    /// turns "3.50" into "3. 50" and "10:30" into "10: 30".
    fn plan_chunks(&self, text: &str, chunk_max_chars: u32) -> Vec<String> {
        // chunk_text may run SOFT_OVERFLOW past a budget to finish a sentence, so the
        // budget is set below the hard limit by that factor.
        let budget = (chunk_max_chars as f32).clamp(MIN_CHUNK_CHARS, MAX_CHUNK_CHARS) / SOFT_OVERFLOW;
        let budgets = [f32::min(FIRST_CHUNK_BUDGET, budget), budget];
        let mut chunks = Vec::new();
        for chunk in chunk_text(&normalize_for_speech(text), &budgets) {
            self.push_within_token_limit(chunk, &mut chunks);
        }
        chunks
    }

    /// Characters are only a proxy for tokens (digits and non-English text tokenize far
    /// less compactly), so a chunk that still exceeds the token limit is halved again.
    fn push_within_token_limit(&self, chunk: String, output: &mut Vec<String>) {
        let tokens = self.model.conditioner.count_tokens(&chunk).unwrap_or(0);
        if tokens <= MAX_TOKENS_PER_CHUNK {
            output.push(chunk);
            return;
        }
        let pieces = chunk_text(&chunk, &[text_units(&chunk) / 2.0]);
        if pieces.len() < 2 {
            output.push(chunk);
            return;
        }
        for piece in pieces {
            self.push_within_token_limit(piece, output);
        }
    }

    fn resolve_voice_state(&mut self, voice_id: &str, selected_preset: &str) -> Result<ModelState> {
        let cache_key = if voice_id == DEFAULT_VOICE_ID {
            format!("preset:{selected_preset}")
        } else {
            format!("voice:{voice_id}")
        };

        if !self.state_cache.contains_key(&cache_key) {
            let state = if voice_id == DEFAULT_VOICE_ID {
                self.load_preset_voice_state(selected_preset)?
            } else {
                let voice_meta = self.read_voice_meta(voice_id)?;
                let ref_audio_path = self.voice_dir(&voice_meta.voice_id).join(REF_AUDIO_FILE_NAME);
                if !ref_audio_path.exists() {
                    return Err(anyhow!(
                        "Saved voice {} is missing reference audio at {}",
                        voice_id,
                        ref_audio_path.display()
                    ));
                }
                self.model
                    .get_voice_state(&ref_audio_path)
                    .with_context(|| format!("Failed to load saved voice from {}", ref_audio_path.display()))?
            };
            self.state_cache.insert(cache_key.clone(), state);
        }

        self.state_cache
            .get(&cache_key)
            .cloned()
            .ok_or_else(|| anyhow!("Failed to resolve voice state for {voice_id}"))
    }

    fn load_preset_voice_state(&self, selected_preset: &str) -> Result<ModelState> {
        let preset_path = self
            .model_dir
            .join("embeddings")
            .join(format!("{selected_preset}.safetensors"));
        if preset_path.exists() {
            return self
                .model
                .get_voice_state_from_prompt_file(&preset_path)
                .with_context(|| format!("Failed to load Kyutai preset prompt {}", preset_path.display()));
        }

        // Presets added upstream after the bundled weights were released only exist as
        // reference clips; clone them the same way a user's voice is cloned.
        let Some(clip_path) = find_bundled_preset_clip(selected_preset) else {
            return Err(anyhow!(
                "Unsupported Kyutai preset voice: {selected_preset} (missing {} and no bundled reference clip; run `npm run assets:fetch`)",
                preset_path.display()
            ));
        };
        let normalized_path = self.preset_clip_cache_dir.join(format!("{selected_preset}.wav"));
        if !normalized_path.exists() {
            std::fs::create_dir_all(&self.preset_clip_cache_dir)
                .with_context(|| format!("Failed to create {}", self.preset_clip_cache_dir.display()))?;
            let clip_bytes =
                std::fs::read(&clip_path).with_context(|| format!("Failed to read {}", clip_path.display()))?;
            write_normalized_reference_wav(&normalized_path, &clip_bytes)?;
        }
        self.model
            .get_voice_state(&normalized_path)
            .with_context(|| format!("Failed to load Kyutai preset voice from {}", clip_path.display()))
    }

    fn list_saved_voices(&self) -> Result<Vec<SavedVoiceMeta>> {
        if !self.voices_dir.exists() {
            return Ok(Vec::new());
        }

        let mut output = Vec::new();
        for entry in std::fs::read_dir(&self.voices_dir)
            .with_context(|| format!("Failed to read {}", self.voices_dir.display()))?
        {
            let entry = entry?;
            let voice_dir = entry.path();
            if !voice_dir.is_dir() {
                continue;
            }
            let meta_path = voice_dir.join(META_FILE_NAME);
            if !meta_path.exists() {
                continue;
            }
            let body = std::fs::read_to_string(&meta_path)
                .with_context(|| format!("Failed to read {}", meta_path.display()))?;
            let parsed: SavedVoiceMeta = serde_json::from_str(&body)
                .with_context(|| format!("Failed to parse {}", meta_path.display()))?;
            output.push(parsed);
        }
        Ok(output)
    }

    fn read_voice_meta(&self, voice_id: &str) -> Result<SavedVoiceMeta> {
        let meta_path = self.voice_dir(voice_id).join(META_FILE_NAME);
        if !meta_path.exists() {
            return Err(anyhow!("VOICE_NOT_FOUND: {voice_id}"));
        }
        let body = std::fs::read_to_string(&meta_path)
            .with_context(|| format!("Failed to read {}", meta_path.display()))?;
        let parsed: SavedVoiceMeta = serde_json::from_str(&body)
            .with_context(|| format!("Failed to parse {}", meta_path.display()))?;
        Ok(parsed)
    }

    fn write_voice_meta(&self, meta: &SavedVoiceMeta) -> Result<()> {
        let voice_dir = self.voice_dir(&meta.voice_id);
        std::fs::create_dir_all(&voice_dir)
            .with_context(|| format!("Failed to create {}", voice_dir.display()))?;
        let meta_path = voice_dir.join(META_FILE_NAME);
        let serialized = serde_json::to_string_pretty(meta)?;
        std::fs::write(&meta_path, serialized)
            .with_context(|| format!("Failed to write {}", meta_path.display()))?;
        Ok(())
    }

    fn voice_dir(&self, voice_id: &str) -> PathBuf {
        self.voices_dir.join(voice_id)
    }
}

fn find_bundled_preset_clip(preset: &str) -> Option<PathBuf> {
    find_bundled_file(&search_roots(), PRESET_CLIPS_DIR_NAME, &format!("{preset}.wav"), &[])
}

fn now_unix_timestamp_string() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    secs.to_string()
}

fn materialize_runtime_config(config_path: &Path, model_dir: &Path, data_dir: &Path) -> Result<PathBuf> {
    let template = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read {}", config_path.display()))?;
    let runtime_root = data_dir.join(RUNTIME_CONFIG_DIR_NAME);
    let runtime_config_dir = runtime_root.join("config");
    std::fs::create_dir_all(&runtime_config_dir)
        .with_context(|| format!("Failed to create {}", runtime_config_dir.display()))?;

    let weights_path = normalize_yaml_path(&model_dir.join("tts_b6369a24.safetensors"));
    let tokenizer_path = normalize_yaml_path(&model_dir.join("tokenizer.model"));
    let rewritten = rewrite_config_paths(&template, &weights_path, &tokenizer_path)?;

    let runtime_config_path = runtime_config_dir.join(format!("{LOCAL_CONFIG_VARIANT}.yaml"));
    std::fs::write(&runtime_config_path, rewritten)
        .with_context(|| format!("Failed to write {}", runtime_config_path.display()))?;
    Ok(runtime_root)
}

fn load_model_from_runtime_config(runtime_config_root: &Path) -> Result<TTSModel> {
    let previous_cwd = std::env::current_dir().context("Failed to read current working directory")?;
    std::env::set_current_dir(runtime_config_root)
        .with_context(|| format!("Failed to switch cwd to {}", runtime_config_root.display()))?;

    let load_result = TTSModel::load(LOCAL_CONFIG_VARIANT);
    let restore_result = std::env::set_current_dir(&previous_cwd)
        .with_context(|| format!("Failed to restore cwd to {}", previous_cwd.display()));

    match (load_result, restore_result) {
        (Ok(model), Ok(())) => Ok(model),
        (Err(load_err), Ok(())) => Err(load_err),
        (Ok(_), Err(restore_err)) => Err(restore_err),
        (Err(load_err), Err(restore_err)) => Err(anyhow!(
            "Model load failed ({load_err:#}); also failed to restore cwd ({restore_err:#})"
        )),
    }
}

fn rewrite_config_paths(template: &str, weights_path: &str, tokenizer_path: &str) -> Result<String> {
    let mut has_weights = false;
    let mut has_weights_no_clone = false;
    let mut has_tokenizer = false;

    let mut output = Vec::new();
    for line in template.lines() {
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];

        if trimmed.starts_with("weights_path:") {
            has_weights = true;
            output.push(format!("{indent}weights_path: {}", yaml_quote_path(weights_path)));
            continue;
        }
        if trimmed.starts_with("weights_path_without_voice_cloning:") {
            has_weights_no_clone = true;
            output.push(format!(
                "{indent}weights_path_without_voice_cloning: {}",
                yaml_quote_path(weights_path)
            ));
            continue;
        }
        if trimmed.starts_with("tokenizer_path:") {
            has_tokenizer = true;
            output.push(format!(
                "{indent}tokenizer_path: {}",
                yaml_quote_path(tokenizer_path)
            ));
            continue;
        }
        output.push(line.to_string());
    }

    if !has_weights || !has_weights_no_clone || !has_tokenizer {
        return Err(anyhow!(
            "Kyutai config template is missing required path keys (weights/tokenizer)"
        ));
    }

    Ok(output.join("\n"))
}

fn normalize_yaml_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn yaml_quote_path(path: &str) -> String {
    let escaped = path.replace('\'', "''");
    format!("'{escaped}'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_pipeline::pcm_i16_to_le_bytes;
    use std::sync::Mutex;

    /// Speaks one sentence with every preset and writes the audio as raw 16-bit PCM, so
    /// the voices can be checked by ear or by pitch. Run with:
    /// `KYUTAI_TEST_MODEL_DIR=<model dir> KYUTAI_TEST_OUT_DIR=<dir> cargo test --features build-base -- --ignored presets`
    #[test]
    #[ignore = "needs the bundled Kyutai model and the fetched preset clips"]
    fn every_preset_voice_speaks() {
        let model_dir = PathBuf::from(std::env::var("KYUTAI_TEST_MODEL_DIR").expect("set KYUTAI_TEST_MODEL_DIR"));
        let out_dir = PathBuf::from(std::env::var("KYUTAI_TEST_OUT_DIR").expect("set KYUTAI_TEST_OUT_DIR"));
        let mut runtime = LocalKyutaiRuntime::new(&model_dir, &out_dir.join("data"), "test", "alba").unwrap();
        let presets = [
            "alba", "marius", "javert", "jean", "fantine", "cosette", "eponine", "azelma", "anna", "vera", "mary",
            "jane", "eve", "caro_davy", "charles", "paul", "george", "michael", "bill_boerst", "peter_yearsley",
            "stuart_bell",
        ];
        for preset in presets {
            let collected: Arc<Mutex<Vec<i16>>> = Arc::new(Mutex::new(Vec::new()));
            let sink = collected.clone();
            let cancel = AtomicBool::new(false);
            let rate = AtomicU32::new(4);
            let started = std::time::Instant::now();
            let (_, had_audio) = runtime
                .stream_synthesize(
                    DEFAULT_VOICE_ID,
                    preset,
                    "The quick brown fox jumps over the lazy dog, and then it sleeps in the warm afternoon sun.",
                    200,
                    1.0,
                    &cancel,
                    &rate,
                    move |_, pcm, _| {
                        sink.lock().unwrap().extend_from_slice(pcm);
                        Ok(())
                    },
                )
                .unwrap_or_else(|err| panic!("preset {preset} failed: {err:#}"));
            assert!(had_audio, "preset {preset} produced no audio");
            let pcm = collected.lock().unwrap();
            let seconds = pcm.len() as f32 / runtime.sample_rate as f32;
            assert!(seconds > 2.0 && seconds < 20.0, "preset {preset} produced {seconds:.1}s");
            std::fs::write(out_dir.join(format!("{preset}.raw")), pcm_i16_to_le_bytes(&pcm)).unwrap();
            println!("{preset}: {seconds:.1}s of audio in {:.1}s", started.elapsed().as_secs_f32());
        }
        assert!(runtime.load_preset_voice_state("not-a-voice").is_err());
    }

    /// Checks that the playback rate shortens the audio. Run with:
    /// `KYUTAI_TEST_MODEL_DIR=<model dir> cargo test --features build-base -- --ignored --nocapture rate`
    #[test]
    #[ignore = "needs the bundled Kyutai model and SoX"]
    fn faster_rate_gives_shorter_audio() {
        let model_dir = PathBuf::from(std::env::var("KYUTAI_TEST_MODEL_DIR").expect("set KYUTAI_TEST_MODEL_DIR"));
        let data_dir = std::env::temp_dir().join(format!("voicereader-kyutai-rate-{}", std::process::id()));
        let mut runtime = LocalKyutaiRuntime::new(&model_dir, &data_dir, "test", "alba").unwrap();
        let text = "The quick brown fox jumps over the lazy dog, and then it sleeps in the warm afternoon sun.";

        let mut speak = |rate_steps: u32| {
            let collected: Arc<Mutex<Vec<i16>>> = Arc::new(Mutex::new(Vec::new()));
            let sink = collected.clone();
            let cancel = AtomicBool::new(false);
            let rate = AtomicU32::new(rate_steps);
            let started = std::time::Instant::now();
            let (end, had_audio) = runtime
                .stream_synthesize(DEFAULT_VOICE_ID, "alba", text, 200, 1.0, &cancel, &rate, move |_, pcm, _| {
                    sink.lock().unwrap().extend_from_slice(pcm);
                    Ok(())
                })
                .unwrap();
            assert!(matches!(end, LocalJobEndState::Done));
            assert!(had_audio);
            let pcm = collected.lock().unwrap();
            let peak = pcm.iter().map(|sample| sample.unsigned_abs()).max().unwrap_or(0);
            let seconds = pcm.len() as f32 / runtime.sample_rate as f32;
            println!(
                "rate={:.2}: {seconds:.2}s of audio, peak {peak}, took {:.1}s",
                rate_steps as f32 / 4.0,
                started.elapsed().as_secs_f32()
            );
            assert!(peak > 1000, "output is silent at {rate_steps}");
            seconds
        };

        let normal = speak(4);
        let fast = speak(6);
        assert!(fast < normal * 0.85, "tempo was not applied: {fast:.2}s vs {normal:.2}s");
        std::fs::remove_dir_all(&data_dir).ok();
    }

    /// Run with: `KYUTAI_TEST_MODEL_DIR=<model dir> cargo test --features build-base -- --ignored plans`
    #[test]
    #[ignore = "needs the bundled Kyutai model"]
    fn plans_keep_numbers_intact_and_stay_within_the_token_limit() {
        let model_dir = PathBuf::from(std::env::var("KYUTAI_TEST_MODEL_DIR").expect("set KYUTAI_TEST_MODEL_DIR"));
        let data_dir = std::env::temp_dir().join(format!("voicereader-kyutai-plan-{}", std::process::id()));
        let runtime = LocalKyutaiRuntime::new(&model_dir, &data_dir, "test", "alba").unwrap();

        let text = "Dr. Smith paid $3.50 at 10:30 a.m. on Jan. 5, e.g. for coffee. The U.S. economy grew 2.5% in Q3.                     Visit example.com/docs for details. When the committee finally met after several months of delays                     caused by scheduling conflicts and a series of unexpected resignations, it decided that the                     proposal, which had been revised four times and reviewed by three separate working groups, should                     be sent back once more for a detailed cost analysis before any vote could be taken.";
        let chunks = runtime.plan_chunks(text, 200);
        let squash = |value: &str| value.chars().filter(|ch| !ch.is_whitespace()).collect::<String>();
        assert_eq!(squash(&chunks.concat()), squash(text));
        assert_eq!(chunks[0], "Dr. Smith paid $3.50 at 10:30 a.m. on Jan. 5, e.g. for coffee.");
        for chunk in &chunks {
            let tokens = runtime.model.conditioner.count_tokens(chunk).unwrap();
            assert!(tokens <= MAX_TOKENS_PER_CHUNK, "{tokens} tokens: {chunk}");
        }

        // The chunk size setting is clamped to what the model can take.
        for setting in [100, 200, 2000] {
            for chunk in runtime.plan_chunks(text, setting) {
                assert!(chunk.chars().count() as f32 <= MAX_CHUNK_CHARS + 1.0, "setting {setting}: {chunk}");
            }
        }
        std::fs::remove_dir_all(&data_dir).ok();
    }
}
