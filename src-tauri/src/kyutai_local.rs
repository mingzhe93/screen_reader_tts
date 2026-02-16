use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
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
const MAX_SENTENCES_PER_CHUNK: usize = 1;
const FIRST_CHUNK_MAX_SENTENCES: usize = 1;
const FIRST_CHUNK_MAX_CHARS: usize = 200;

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

struct SoxTempoStream {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout_rx: Receiver<Vec<u8>>,
    stdout_join: Option<JoinHandle<()>>,
    pending: Vec<u8>,
    frame_samples: usize,
}

impl SoxTempoStream {
    fn new(rate: f32, sample_rate: u32) -> Option<Self> {
        if sample_rate == 0 {
            return None;
        }
        let sox_path = resolve_sox_path_cached()?;
        let factors = decompose_tempo_factors(rate);
        if factors.is_empty() {
            return None;
        }

        let mut command = Command::new(sox_path);
        command
            .arg("-q")
            .arg("-t")
            .arg("raw")
            .arg("-r")
            .arg(sample_rate.to_string())
            .arg("-e")
            .arg("signed-integer")
            .arg("-b")
            .arg("16")
            .arg("-c")
            .arg("1")
            .arg("-L")
            .arg("-")
            .arg("-t")
            .arg("raw")
            .arg("-e")
            .arg("signed-integer")
            .arg("-b")
            .arg("16")
            .arg("-c")
            .arg("1")
            .arg("-L")
            .arg("-");

        for factor in factors {
            command.arg("tempo").arg(format!("{factor:.6}"));
        }

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let stdin = child.stdin.take()?;
        let mut stdout = child.stdout.take()?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let join = std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match stdout.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buffer[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let frame_samples = if rate >= 3.0 {
            24_576
        } else if rate >= 2.0 {
            16_384
        } else {
            8_192
        };

        Some(Self {
            child,
            stdin: Some(stdin),
            stdout_rx: rx,
            stdout_join: Some(join),
            pending: Vec::new(),
            frame_samples,
        })
    }

    fn push_samples(&mut self, samples: &[i16]) -> Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        let stdin = self.stdin.as_mut().ok_or_else(|| anyhow!("SoX stdin closed"))?;
        stdin
            .write_all(&pcm_i16_to_le_bytes(samples))
            .context("Failed writing PCM data to SoX stdin")?;
        let _ = stdin.flush();
        Ok(())
    }

    fn drain_available_frames(&mut self) -> Vec<Vec<i16>> {
        while let Ok(bytes) = self.stdout_rx.try_recv() {
            self.pending.extend_from_slice(&bytes);
        }
        self.take_ready_frames()
    }

    fn finish_and_drain(&mut self) -> Vec<Vec<i16>> {
        self.stdin.take();
        let _ = self.child.wait();
        if let Some(join) = self.stdout_join.take() {
            let _ = join.join();
        }
        while let Ok(bytes) = self.stdout_rx.try_recv() {
            self.pending.extend_from_slice(&bytes);
        }

        let mut frames = self.take_ready_frames();
        let trailing = bytes_to_pcm_i16_drain_all(&mut self.pending);
        if !trailing.is_empty() {
            frames.push(trailing);
        }
        frames
    }

    fn abort(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(join) = self.stdout_join.take() {
            let _ = join.join();
        }
        self.pending.clear();
    }

    fn take_ready_frames(&mut self) -> Vec<Vec<i16>> {
        let frame_bytes = self.frame_samples * 2;
        let mut frames: Vec<Vec<i16>> = Vec::new();
        while self.pending.len() >= frame_bytes {
            let raw: Vec<u8> = self.pending.drain(..frame_bytes).collect();
            let pcm = bytes_to_pcm_i16(&raw);
            if !pcm.is_empty() {
                frames.push(pcm);
            }
        }
        frames
    }
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
        let sox_detail = resolve_sox_path_cached()
            .map(|path| format!("sox={}", path.display()))
            .unwrap_or_else(|| "sox=unavailable(resample_fallback_pitch_shift)".to_string());
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
        rate: f32,
        volume: f32,
        cancel: &AtomicBool,
        mut on_chunk: F,
    ) -> Result<LocalJobEndState>
    where
        F: FnMut(usize, &[i16], u32) -> Result<()>,
    {
        let mut chunk_index: usize = 0;
        let rate_clamped = rate.clamp(0.25, 4.0);
        let rate_active = (rate_clamped - 1.0).abs() > f32::EPSILON;
        let mut sox_stream = if rate_active {
            SoxTempoStream::new(rate_clamped, self.sample_rate)
        } else {
            None
        };
        let chunk_size = usize::min(usize::max(chunk_max_chars as usize, 100), FIRST_CHUNK_MAX_CHARS);
        let split = self.model.split_into_best_sentences(text);
        let text_chunks = cap_chunks_by_chars(split, text, chunk_size, MAX_SENTENCES_PER_CHUNK);

        for text_chunk in text_chunks {
            if cancel.load(Ordering::SeqCst) {
                if let Some(stream) = sox_stream.as_mut() {
                    stream.abort();
                }
                return Ok(LocalJobEndState::Canceled);
            }

            let voice_state = self.resolve_voice_state(voice_id, selected_preset)?;
            if rate_active {
                let tensor = self
                    .model
                    .generate(&text_chunk, &voice_state)
                    .context("Pocket-TTS generation failed")?;
                if cancel.load(Ordering::SeqCst) {
                    if let Some(stream) = sox_stream.as_mut() {
                        stream.abort();
                    }
                    return Ok(LocalJobEndState::Canceled);
                }

                let gain: f32 = volume.clamp(0.0, 2.0);
                let values = tensor
                    .flatten_all()
                    .context("Failed to flatten Pocket-TTS tensor")?
                    .to_vec1::<f32>()
                    .context("Failed to convert Pocket-TTS tensor to f32 samples")?;
                let mut pcm = Vec::with_capacity(values.len());
                for sample in values {
                    let scaled = (sample * gain).clamp(-1.0, 1.0);
                    pcm.push((scaled * 32767.0) as i16);
                }
                if pcm.is_empty() {
                    continue;
                }

                if let Some(rate_stream) = sox_stream.as_mut() {
                    rate_stream.push_samples(&pcm)?;
                    let mut combined: Vec<i16> = Vec::new();
                    for adjusted in rate_stream.drain_available_frames() {
                        if adjusted.is_empty() {
                            continue;
                        }
                        combined.extend_from_slice(&adjusted);
                    }
                    if !combined.is_empty() {
                        on_chunk(chunk_index, &combined, self.sample_rate)?;
                        chunk_index += 1;
                    }
                } else {
                    pcm = resample_pcm_by_rate(&pcm, rate_clamped);
                    if pcm.is_empty() {
                        continue;
                    }
                    on_chunk(chunk_index, &pcm, self.sample_rate)?;
                    chunk_index += 1;
                }
                continue;
            }

            let stream = self.model.generate_stream(&text_chunk, &voice_state);
            for maybe_tensor in stream {
                if cancel.load(Ordering::SeqCst) {
                    if let Some(stream) = sox_stream.as_mut() {
                        stream.abort();
                    }
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
                if pcm.is_empty() {
                    continue;
                }

                if let Some(rate_stream) = sox_stream.as_mut() {
                    rate_stream.push_samples(&pcm)?;
                    let mut combined: Vec<i16> = Vec::new();
                    for adjusted in rate_stream.drain_available_frames() {
                        if adjusted.is_empty() {
                            continue;
                        }
                        combined.extend_from_slice(&adjusted);
                    }
                    if !combined.is_empty() {
                        on_chunk(chunk_index, &combined, self.sample_rate)?;
                        chunk_index += 1;
                    }
                    continue;
                }

                if rate_active {
                    pcm = resample_pcm_by_rate(&pcm, rate_clamped);
                    if pcm.is_empty() {
                        continue;
                    }
                }
                on_chunk(chunk_index, &pcm, self.sample_rate)?;
                chunk_index += 1;
            }
        }

        if let Some(rate_stream) = sox_stream.as_mut() {
            let mut combined: Vec<i16> = Vec::new();
            for adjusted in rate_stream.finish_and_drain() {
                if adjusted.is_empty() {
                    continue;
                }
                combined.extend_from_slice(&adjusted);
            }
            if !combined.is_empty() {
                on_chunk(chunk_index, &combined, self.sample_rate)?;
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

fn cap_chunks_by_chars(
    split: Vec<String>,
    original_text: &str,
    max_chars: usize,
    max_sentences_per_chunk: usize,
) -> Vec<String> {
    let mut output: Vec<String> = Vec::new();
    let source = if split.is_empty() {
        vec![original_text.to_string()]
    } else {
        split
    };
    let sentence_limit = usize::max(1, max_sentences_per_chunk);
    let first_sentence_limit = usize::max(1, usize::min(sentence_limit, FIRST_CHUNK_MAX_SENTENCES));
    let first_chunk_char_limit = usize::max(100, usize::min(max_chars, FIRST_CHUNK_MAX_CHARS));
    let mut grouped = String::new();
    let mut grouped_sentences = 0usize;

    let flush_group = |output: &mut Vec<String>, grouped: &mut String, grouped_sentences: &mut usize| {
        if grouped.trim().is_empty() {
            grouped.clear();
            *grouped_sentences = 0;
            return;
        }
        output.push(grouped.trim().to_string());
        grouped.clear();
        *grouped_sentences = 0;
    };

    for sentence in source {
        let trimmed = sentence.trim();
        if trimmed.is_empty() {
            continue;
        }

        let building_first_chunk = output.is_empty();
        let active_sentence_limit = if building_first_chunk {
            first_sentence_limit
        } else {
            sentence_limit
        };
        let active_char_limit = if building_first_chunk {
            first_chunk_char_limit
        } else {
            max_chars
        };

        let sentence_chars = trimmed.chars().count();
        if sentence_chars > active_char_limit {
            flush_group(&mut output, &mut grouped, &mut grouped_sentences);
            output.extend(split_long_segment_by_words(trimmed, active_char_limit));
            continue;
        }

        let next_len = if grouped.is_empty() {
            sentence_chars
        } else {
            grouped.chars().count() + 1 + sentence_chars
        };
        let reached_sentence_limit = grouped_sentences >= active_sentence_limit;
        let would_exceed_chars = !grouped.is_empty() && next_len > active_char_limit;
        if reached_sentence_limit || would_exceed_chars {
            flush_group(&mut output, &mut grouped, &mut grouped_sentences);
        }

        if !grouped.is_empty() {
            grouped.push(' ');
        }
        grouped.push_str(trimmed);
        grouped_sentences += 1;
    }

    if !grouped.is_empty() {
        output.push(grouped.trim().to_string());
    }

    if output.is_empty() {
        vec![original_text.trim().to_string()]
    } else {
        output
    }
}

fn split_long_segment_by_words(input: &str, max_chars: usize) -> Vec<String> {
    let mut output: Vec<String> = Vec::new();
    let mut current = String::new();

    for word in input.split_whitespace() {
        let word_chars = word.chars().count();

        if word_chars > max_chars {
            if !current.is_empty() {
                output.push(current);
                current = String::new();
            }
            let mut token = String::new();
            let mut token_chars = 0usize;
            for ch in word.chars() {
                if token_chars >= max_chars {
                    output.push(token);
                    token = String::new();
                    token_chars = 0;
                }
                token.push(ch);
                token_chars += 1;
            }
            if !token.is_empty() {
                output.push(token);
            }
            continue;
        }

        let next_len = if current.is_empty() {
            word_chars
        } else {
            current.chars().count() + 1 + word_chars
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
    output
}

fn resample_pcm_by_rate(input: &[i16], rate: f32) -> Vec<i16> {
    if input.is_empty() {
        return Vec::new();
    }
    if (rate - 1.0).abs() <= f32::EPSILON {
        return input.to_vec();
    }

    let input_len = input.len();
    let output_len = usize::max(1, ((input_len as f32) / rate).round() as usize);
    let mut output = Vec::with_capacity(output_len);

    for out_index in 0..output_len {
        let src_pos = (out_index as f32) * rate;
        let left_idx = usize::min(src_pos.floor() as usize, input_len.saturating_sub(1));
        let right_idx = usize::min(left_idx + 1, input_len.saturating_sub(1));
        let frac = (src_pos - (left_idx as f32)).clamp(0.0, 1.0);

        let left = input[left_idx] as f32;
        let right = input[right_idx] as f32;
        let interpolated = left + (right - left) * frac;
        output.push(interpolated.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16);
    }

    output
}

fn decompose_tempo_factors(rate: f32) -> Vec<f32> {
    if rate <= 0.0 {
        return Vec::new();
    }
    if (rate - 1.0).abs() <= f32::EPSILON {
        return vec![1.0];
    }

    // Prefer several smaller tempo steps over one large step; this
    // generally preserves speech timbre better at high speedups.
    let max_step = 1.35_f32;
    if rate > 1.0 {
        let mut steps = (rate.ln() / max_step.ln()).ceil() as usize;
        if steps == 0 {
            steps = 1;
        }
        let factor = rate.powf(1.0 / steps as f32);
        return vec![factor.clamp(0.5, 2.0); steps];
    }

    let mut steps = ((1.0 / rate).ln() / max_step.ln()).ceil() as usize;
    if steps == 0 {
        steps = 1;
    }
    let factor = rate.powf(1.0 / steps as f32);
    vec![factor.clamp(0.5, 2.0); steps]
}

fn resolve_sox_path_cached() -> Option<PathBuf> {
    static SOX_PATH_CACHE: OnceLock<Option<PathBuf>> = OnceLock::new();
    SOX_PATH_CACHE.get_or_init(resolve_sox_path).clone()
}

fn resolve_sox_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("VOICEREADER_SOX_PATH").map(PathBuf::from) {
        if path.exists() {
            return Some(path);
        }
    }
    if let Some(path) = find_bundled_sox_near_current_executable() {
        return Some(path);
    }
    if command_exists("sox") {
        return Some(PathBuf::from("sox"));
    }
    find_sox_in_windows_winget_location()
}

fn find_bundled_sox_near_current_executable() -> Option<PathBuf> {
    let sox_name = if cfg!(target_os = "windows") {
        "sox.exe"
    } else {
        "sox"
    };

    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            roots.push(parent.to_path_buf());
            if let Some(grand_parent) = parent.parent() {
                roots.push(grand_parent.to_path_buf());
                if let Some(great_grand_parent) = grand_parent.parent() {
                    roots.push(great_grand_parent.to_path_buf());
                }
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }

    let mut seen: HashSet<PathBuf> = HashSet::new();
    for root in roots {
        if !seen.insert(root.clone()) {
            continue;
        }
        let candidates = [
            root.join("binaries").join("sox").join(sox_name),
            root.join("resources").join("binaries").join("sox").join(sox_name),
            root.join("binaries").join(sox_name),
            root.join("resources").join("binaries").join(sox_name),
            root.join("sox").join(sox_name),
            root.join("resources").join("sox").join(sox_name),
            root.join(sox_name),
        ];
        for candidate in candidates {
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

fn command_exists(command: &str) -> bool {
    Command::new(command)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn find_sox_in_windows_winget_location() -> Option<PathBuf> {
    if !cfg!(target_os = "windows") {
        return None;
    }

    let local_app_data = std::env::var_os("LOCALAPPDATA")?;
    let root = PathBuf::from(local_app_data)
        .join("Microsoft")
        .join("WinGet")
        .join("Packages");
    if !root.exists() {
        return None;
    }

    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&root)
        .ok()?
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if !path.is_dir() {
                return None;
            }
            let name = path.file_name()?.to_string_lossy().to_string();
            if name.starts_with("ChrisBagwell.SoX_") {
                Some(path)
            } else {
                None
            }
        })
        .collect();
    candidates.sort();

    for candidate in candidates {
        if let Ok(entries) = std::fs::read_dir(&candidate) {
            let mut nested_bins: Vec<PathBuf> = entries
                .filter_map(|entry| {
                    let path = entry.ok()?.path();
                    if !path.is_dir() {
                        return None;
                    }
                    let name = path.file_name()?.to_string_lossy().to_string();
                    if name.starts_with("sox-") {
                        let binary = path.join("sox.exe");
                        if binary.exists() {
                            return Some(binary);
                        }
                    }
                    None
                })
                .collect();
            nested_bins.sort();
            if let Some(binary) = nested_bins.into_iter().next() {
                return Some(binary);
            }
        }

        let direct_binary = candidate.join("sox.exe");
        if direct_binary.exists() {
            return Some(direct_binary);
        }
    }

    None
}

fn pcm_i16_to_le_bytes(samples: &[i16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

fn bytes_to_pcm_i16(bytes: &[u8]) -> Vec<i16> {
    let even_len = bytes.len() - (bytes.len() % 2);
    let mut output = Vec::with_capacity(even_len / 2);
    for chunk in bytes[..even_len].chunks_exact(2) {
        output.push(i16::from_le_bytes([chunk[0], chunk[1]]));
    }
    output
}

fn bytes_to_pcm_i16_drain_all(buffer: &mut Vec<u8>) -> Vec<i16> {
    let even_len = buffer.len() - (buffer.len() % 2);
    if even_len == 0 {
        return Vec::new();
    }
    let drained: Vec<u8> = buffer.drain(..even_len).collect();
    bytes_to_pcm_i16(&drained)
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
