//! Playback-rate handling shared by the in-process TTS runtimes: PCM is time-stretched
//! by SoX (pitch preserved) or, without SoX, resampled, then handed to the caller.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::OnceLock;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use crate::bundled_paths::{find_bundled_file, search_roots};

/// How long a piece may wait for SoX to hand back audio before moving on. The first
/// piece gets longer because it decides when playback starts.
const FIRST_AUDIO_SOX_WAIT: Duration = Duration::from_millis(300);
const SOX_OUTPUT_WAIT: Duration = Duration::from_millis(120);
/// SoX output is considered complete for a piece once nothing new arrives for this long.
const SOX_OUTPUT_QUIET: Duration = Duration::from_millis(20);
/// SoX's default 8192-sample buffer would hold back up to 0.19 s of audio per piece.
const SOX_BUFFER_BYTES: usize = 4096;

/// The playback rate selected in the app, in quarter steps (4 is 1.0x, 6 is 1.5x).
pub(crate) fn current_rate(active_rate_steps: &AtomicU32) -> f32 {
    (active_rate_steps.load(Ordering::SeqCst).clamp(1, 16) as f32) / 4.0
}

/// Applies the live playback rate to PCM (SoX tempo, pitch preserved) and forwards it.
pub(crate) struct RateEmitter<'a, F> {
    sample_rate: u32,
    active_rate_steps: &'a AtomicU32,
    rate: f32,
    sox: Option<SoxTempoStream>,
    on_chunk: F,
    chunk_index: usize,
    had_audio: bool,
}

impl<'a, F> RateEmitter<'a, F>
where
    F: Fn(usize, &[i16], u32) -> Result<()>,
{
    pub(crate) fn new(sample_rate: u32, active_rate_steps: &'a AtomicU32, on_chunk: F) -> Self {
        let rate = current_rate(active_rate_steps);
        Self {
            sample_rate,
            active_rate_steps,
            rate,
            sox: Self::open_sox(rate, sample_rate),
            on_chunk,
            chunk_index: 0,
            had_audio: false,
        }
    }

    /// The rate in effect for the audio pushed next.
    pub(crate) fn rate(&self) -> f32 {
        self.rate
    }

    /// Whether at least one non-empty chunk has been forwarded.
    pub(crate) fn had_audio(&self) -> bool {
        self.had_audio
    }

    fn open_sox(rate: f32, sample_rate: u32) -> Option<SoxTempoStream> {
        if (rate - 1.0).abs() > f32::EPSILON {
            SoxTempoStream::with_buffer(rate, sample_rate, SOX_BUFFER_BYTES)
        } else {
            None
        }
    }

    fn emit(&mut self, pcm: &[i16]) -> Result<()> {
        if pcm.is_empty() {
            return Ok(());
        }
        self.had_audio = true;
        (self.on_chunk)(self.chunk_index, pcm, self.sample_rate)?;
        self.chunk_index += 1;
        Ok(())
    }

    pub(crate) fn push(&mut self, pcm: &[i16]) -> Result<()> {
        // Pieces arrive every few hundred milliseconds, so checking the live rate once per
        // piece keeps rate changes responsive without slicing the audio further.
        let desired = current_rate(self.active_rate_steps);
        if (desired - self.rate).abs() > f32::EPSILON {
            self.flush_sox()?;
            self.rate = desired;
            self.sox = Self::open_sox(desired, self.sample_rate);
        }
        self.feed(pcm)
    }

    fn feed(&mut self, segment: &[i16]) -> Result<()> {
        let Some(sox) = self.sox.as_mut() else {
            if (self.rate - 1.0).abs() <= f32::EPSILON {
                return self.emit(segment);
            }
            // SoX is unavailable: fall back to plain resampling (changes pitch).
            let resampled = resample_pcm_by_rate(segment, self.rate);
            return self.emit(&resampled);
        };
        sox.push_samples(segment)?;
        // SoX runs in another process and hands audio back a moment after it is fed. Wait
        // until its output stops growing, so this piece's audio goes out now instead of
        // with the next piece several hundred milliseconds later.
        let mut adjusted = sox.drain_all_available();
        let deadline = Instant::now() + if self.had_audio { SOX_OUTPUT_WAIT } else { FIRST_AUDIO_SOX_WAIT };
        let mut quiet_since = Instant::now();
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(3));
            let more = sox.drain_all_available();
            if !more.is_empty() {
                adjusted.extend_from_slice(&more);
                quiet_since = Instant::now();
            } else if !adjusted.is_empty() && quiet_since.elapsed() >= SOX_OUTPUT_QUIET {
                break;
            }
        }
        self.emit(&adjusted)
    }

    fn flush_sox(&mut self) -> Result<()> {
        if let Some(mut sox) = self.sox.take() {
            let mut combined: Vec<i16> = Vec::new();
            for frame in sox.finish_and_drain() {
                combined.extend_from_slice(&frame);
            }
            self.emit(&combined)?;
        }
        Ok(())
    }

    /// Forwards whatever SoX still holds back. Call once after the last `push`.
    pub(crate) fn finish(&mut self) -> Result<()> {
        self.flush_sox()
    }

    /// Stops SoX without forwarding its remaining output.
    pub(crate) fn abort(&mut self) {
        if let Some(mut sox) = self.sox.take() {
            sox.abort();
        }
    }
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
    /// `buffer_bytes` sets SoX's processing buffer. SoX hands audio back one buffer at a
    /// time, so a small buffer lowers latency for callers that stream short pieces.
    fn with_buffer(rate: f32, sample_rate: u32, buffer_bytes: usize) -> Option<Self> {
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
            .arg("--buffer")
            .arg(buffer_bytes.to_string())
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

    /// Returns everything SoX has produced so far, without waiting for a full frame.
    fn drain_all_available(&mut self) -> Vec<i16> {
        while let Ok(bytes) = self.stdout_rx.try_recv() {
            self.pending.extend_from_slice(&bytes);
        }
        let whole = self.pending.len() - self.pending.len() % 2;
        let raw: Vec<u8> = self.pending.drain(..whole).collect();
        bytes_to_pcm_i16(&raw)
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

/// Converts an uploaded reference clip to the 24 kHz mono 16-bit WAV the voice-cloning
/// models read. Without SoX the bytes are stored as they are.
pub(crate) fn write_normalized_reference_wav(ref_wav_path: &Path, wav_bytes: &[u8]) -> Result<()> {
    if wav_bytes.is_empty() {
        return Err(anyhow!("Reference audio payload is empty"));
    }

    if let Some(sox_path) = resolve_sox_path_cached() {
        let mut command = Command::new(sox_path);
        command
            .arg("-q")
            .arg("-t")
            .arg("wav")
            .arg("-")
            .arg("-r")
            .arg("24000")
            .arg("-e")
            .arg("signed-integer")
            .arg("-b")
            .arg("16")
            .arg("-c")
            .arg("1")
            .arg("-L")
            .arg(ref_wav_path);

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        command.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("Failed to start SoX for reference-audio normalization: {}", ref_wav_path.display()))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(wav_bytes)
                .context("Failed writing clone reference audio to SoX stdin")?;
        }

        let output = child
            .wait_with_output()
            .context("Failed while waiting for SoX reference-audio normalization")?;
        if output.status.success() && ref_wav_path.exists() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow!(
            "Failed to normalize clone reference audio with SoX: {}",
            if stderr.is_empty() {
                "unknown SoX error".to_string()
            } else {
                stderr
            }
        ));
    }

    std::fs::write(ref_wav_path, wav_bytes)
        .with_context(|| format!("Failed to write {}", ref_wav_path.display()))?;
    Ok(())
}

pub(crate) fn resolve_sox_path_cached() -> Option<PathBuf> {
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
    // Layouts where the binary sits directly in a binaries folder or the app root.
    let extra = [
        format!("binaries/{sox_name}"),
        format!("resources/binaries/{sox_name}"),
        format!("resources/sox/{sox_name}"),
        sox_name.to_string(),
    ];
    find_bundled_file(&search_roots(), "sox", sox_name, &extra)
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

pub(crate) fn pcm_i16_to_le_bytes(samples: &[i16]) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn tempo_factors_multiply_to_the_rate_in_gentle_steps() {
        assert!(decompose_tempo_factors(0.0).is_empty());
        assert!(decompose_tempo_factors(-1.0).is_empty());
        assert_eq!(decompose_tempo_factors(1.0), vec![1.0]);

        for rate in [0.5_f32, 0.75, 1.25, 1.5, 2.0, 3.0, 4.0] {
            let factors = decompose_tempo_factors(rate);
            let product: f32 = factors.iter().product();
            assert!((product - rate).abs() < rate * 1e-3, "rate {rate}: {factors:?} multiply to {product}");
            for factor in &factors {
                assert!((0.5..=2.0).contains(factor), "rate {rate}: factor {factor}");
                // Each step stays small so the voice keeps its timbre.
                let step = if rate > 1.0 { *factor } else { 1.0 / factor };
                assert!(step <= 1.35 + 1e-3, "rate {rate}: step {step}");
                assert_eq!(*factor > 1.0, rate > 1.0);
            }
        }
        // A modest speedup is a single step; a large one is split.
        assert_eq!(decompose_tempo_factors(1.25).len(), 1);
        assert_eq!(decompose_tempo_factors(4.0).len(), 5);
    }

    #[test]
    fn resampling_scales_length_and_interpolates() {
        assert!(resample_pcm_by_rate(&[], 2.0).is_empty());
        let input: Vec<i16> = (0..100).collect();
        assert_eq!(resample_pcm_by_rate(&input, 1.0), input);

        let faster = resample_pcm_by_rate(&input, 2.0);
        assert_eq!(faster.len(), 50);
        assert_eq!(faster[10], 20);
        let slower = resample_pcm_by_rate(&input, 0.5);
        assert_eq!(slower.len(), 200);
        assert_eq!(slower[1], 1);

        // Positions between samples are interpolated; the last sample is held, not overrun.
        assert_eq!(resample_pcm_by_rate(&[0, 100], 0.5), vec![0, 50, 100, 100]);
        assert_eq!(resample_pcm_by_rate(&[7], 3.0), vec![7]);
    }

    #[test]
    fn pcm_bytes_round_trip_and_drop_a_trailing_half_sample() {
        let samples = [0_i16, 1, -1, i16::MAX, i16::MIN];
        let bytes = pcm_i16_to_le_bytes(&samples);
        assert_eq!(bytes.len(), 10);
        assert_eq!(bytes_to_pcm_i16(&bytes), samples);

        let mut odd = bytes.clone();
        odd.push(0x55);
        assert_eq!(bytes_to_pcm_i16(&odd), samples);
        assert_eq!(bytes_to_pcm_i16_drain_all(&mut odd), samples);
        assert_eq!(odd, vec![0x55]);
        assert!(bytes_to_pcm_i16_drain_all(&mut odd).is_empty());
    }

    #[test]
    fn current_rate_is_quarter_steps_within_bounds() {
        assert_eq!(current_rate(&AtomicU32::new(4)), 1.0);
        assert_eq!(current_rate(&AtomicU32::new(6)), 1.5);
        assert_eq!(current_rate(&AtomicU32::new(0)), 0.25);
        assert_eq!(current_rate(&AtomicU32::new(99)), 4.0);
    }

    #[test]
    fn emitter_passes_audio_through_at_normal_rate() {
        let rate = AtomicU32::new(4);
        let seen: Arc<Mutex<Vec<(usize, usize, u32)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let mut emitter = RateEmitter::new(24_000, &rate, move |index, pcm, sample_rate| {
            sink.lock().unwrap().push((index, pcm.len(), sample_rate));
            Ok(())
        });
        assert_eq!(emitter.rate(), 1.0);
        assert!(!emitter.had_audio());

        emitter.push(&[]).unwrap();
        assert!(!emitter.had_audio());
        emitter.push(&[1, 2, 3]).unwrap();
        emitter.push(&[4, 5]).unwrap();
        emitter.finish().unwrap();
        assert!(emitter.had_audio());
        assert_eq!(*seen.lock().unwrap(), vec![(0, 3, 24_000), (1, 2, 24_000)]);
    }

    #[test]
    fn emitter_stops_on_a_callback_error() {
        let rate = AtomicU32::new(4);
        let mut emitter = RateEmitter::new(24_000, &rate, |_, _, _| Err(anyhow!("closed")));
        assert!(emitter.push(&[1, 2, 3]).is_err());
    }
}
