use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use pocket_tts::{ModelState, TTSModel};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

const DEFAULT_VOICE_ID: &str = "0";
const META_FILE_NAME: &str = "meta.json";
const REF_AUDIO_FILE_NAME: &str = "reference.wav";
const LOCAL_CONFIG_VARIANT: &str = "voicereader-pocket-tts-local";
const RUNTIME_CONFIG_DIR_NAME: &str = "pocket-tts-runtime";

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
    model: TTSModel,
    sample_rate: u32,
    voices_dir: PathBuf,
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
            model,
            sample_rate,
            voices_dir,
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
        json!({
            "engine_version": "0.1.0",
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
                    "model={}, source={}, preset={}",
                    self.model_id,
                    self.model_dir.display(),
                    selected_preset
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
        std::fs::write(&ref_wav_path, wav_bytes)
            .with_context(|| format!("Failed to write {}", ref_wav_path.display()))?;

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

    pub fn stream_synthesize<F>(
        &mut self,
        voice_id: &str,
        selected_preset: &str,
        text: &str,
        chunk_max_chars: u32,
        volume: f32,
        cancel: &AtomicBool,
        mut on_chunk: F,
    ) -> Result<LocalJobEndState>
    where
        F: FnMut(usize, &[i16], u32) -> Result<()>,
    {
        let mut chunk_index: usize = 0;
        let chunk_size = usize::max(chunk_max_chars as usize, 100);
        let split = self.model.split_into_best_sentences(text);
        let text_chunks = cap_chunks_by_chars(split, text, chunk_size);

        for text_chunk in text_chunks {
            if cancel.load(Ordering::SeqCst) {
                return Ok(LocalJobEndState::Canceled);
            }

            let voice_state = self.resolve_voice_state(voice_id, selected_preset)?;
            let stream = self.model.generate_stream(&text_chunk, &voice_state);
            for maybe_tensor in stream {
                if cancel.load(Ordering::SeqCst) {
                    return Ok(LocalJobEndState::Canceled);
                }
                let tensor = maybe_tensor.context("Pocket-TTS stream generation failed")?;
                let gain: f32 = volume.clamp(0.0, 2.0);
                let values = tensor
                    .flatten_all()
                    .context("Failed to flatten Pocket-TTS tensor chunk")?
                    .to_vec1::<f32>()
                    .context("Failed to convert Pocket-TTS tensor chunk to f32 samples")?;
                let mut pcm = Vec::with_capacity(values.len());
                for sample in values {
                    let scaled = (sample * gain).clamp(-1.0, 1.0);
                    pcm.push((scaled * 32767.0) as i16);
                }
                on_chunk(chunk_index, &pcm, self.sample_rate)?;
                chunk_index += 1;
            }
        }

        Ok(LocalJobEndState::Done)
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
        if !preset_path.exists() {
            return Err(anyhow!(
                "Unsupported Kyutai preset voice: {selected_preset} (missing {})",
                preset_path.display()
            ));
        }
        self.model
            .get_voice_state_from_prompt_file(&preset_path)
            .with_context(|| format!("Failed to load Kyutai preset prompt {}", preset_path.display()))
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

fn cap_chunks_by_chars(split: Vec<String>, original_text: &str, max_chars: usize) -> Vec<String> {
    let mut output: Vec<String> = Vec::new();
    let source = if split.is_empty() {
        vec![original_text.to_string()]
    } else {
        split
    };

    for chunk in source {
        let trimmed = chunk.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.chars().count() <= max_chars {
            output.push(trimmed.to_string());
            continue;
        }

        let mut current = String::new();
        for word in trimmed.split_whitespace() {
            let next_len = if current.is_empty() {
                word.chars().count()
            } else {
                current.chars().count() + 1 + word.chars().count()
            };
            if !current.is_empty() && next_len > max_chars {
                output.push(current);
                current = word.to_string();
            } else {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(word);
            }
        }
        if !current.is_empty() {
            output.push(current);
        }
    }

    if output.is_empty() {
        vec![original_text.trim().to_string()]
    } else {
        output
    }
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
