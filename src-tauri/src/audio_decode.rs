//! Reading audio files (WAV, MP3, M4A/MP4, FLAC, OGG) as mono samples at a chosen rate,
//! a block at a time, so a long recording never has to fit in memory.

use std::fs::File;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Taps on each side of the resampling kernel.
const HALF_TAPS: i64 = 24;

/// Windowed-sinc resampler that accepts its input in pieces.
///
/// The two rates always reduce to a ratio of whole numbers, so the kernel weights
/// repeat with a fixed period and are computed once per phase instead of once per
/// output sample.
pub(crate) struct StreamResampler {
    /// Output samples per `step` input samples, in lowest terms.
    phases: u64,
    step: u64,
    /// `weights[phase]` holds the kernel for taps `center - HALF_TAPS + 1 ..= center + HALF_TAPS`.
    weights: Vec<[f32; (HALF_TAPS * 2) as usize]>,
    /// Input not yet fully consumed, and the absolute index of its first sample.
    buffer: Vec<f32>,
    buffer_start: i64,
    received: i64,
    next_output: u64,
    passthrough: bool,
}

impl StreamResampler {
    pub(crate) fn new(source_rate: u32, target_rate: u32) -> Self {
        let passthrough = source_rate == target_rate || source_rate == 0 || target_rate == 0;
        let divisor = gcd(source_rate.max(1) as u64, target_rate.max(1) as u64);
        let phases = target_rate.max(1) as u64 / divisor;
        let step = source_rate.max(1) as u64 / divisor;
        // Low-pass at the lower of the two Nyquist frequencies to avoid aliasing when downsampling.
        let cutoff = f64::min(1.0, phases as f64 / step as f64);
        let weights = if passthrough {
            Vec::new()
        } else {
            (0..phases)
                .map(|phase| {
                    let fraction = phase as f64 / phases as f64;
                    let mut kernel = [0.0f32; (HALF_TAPS * 2) as usize];
                    for (slot, weight) in kernel.iter_mut().enumerate() {
                        let distance = fraction - (slot as i64 - HALF_TAPS + 1) as f64;
                        let x = distance * cutoff;
                        let sinc = if x.abs() < 1e-9 {
                            1.0
                        } else {
                            (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
                        };
                        let window = 0.5 + 0.5 * (std::f64::consts::PI * distance / HALF_TAPS as f64).cos();
                        *weight = (sinc * window) as f32;
                    }
                    kernel
                })
                .collect()
        };
        Self {
            phases,
            step,
            weights,
            buffer: Vec::new(),
            buffer_start: 0,
            received: 0,
            next_output: 0,
            passthrough,
        }
    }

    /// Adds input and appends every output sample that can now be computed to `output`.
    pub(crate) fn push(&mut self, input: &[f32], output: &mut Vec<f32>) {
        if self.passthrough {
            output.extend_from_slice(input);
            return;
        }
        self.buffer.extend_from_slice(input);
        self.received += input.len() as i64;
        self.produce(output, false);
    }

    /// Appends the samples that were waiting for input that will not come.
    pub(crate) fn finish(&mut self, output: &mut Vec<f32>) {
        if !self.passthrough {
            self.produce(output, true);
        }
    }

    fn produce(&mut self, output: &mut Vec<f32>, at_end: bool) {
        let total_outputs = self.rounded_total();
        loop {
            let position = self.next_output as u128 * self.step as u128;
            let center = (position / self.phases as u128) as i64;
            let phase = (position % self.phases as u128) as usize;
            if at_end {
                if self.next_output >= total_outputs {
                    break;
                }
            } else if center + HALF_TAPS >= self.received {
                break;
            }
            let kernel = &self.weights[phase];
            let mut acc = 0.0f32;
            let mut weight_sum = 0.0f32;
            for (slot, weight) in kernel.iter().enumerate() {
                let tap = center - HALF_TAPS + 1 + slot as i64;
                if tap < 0 || tap >= self.received {
                    continue;
                }
                acc += self.buffer[(tap - self.buffer_start) as usize] * weight;
                weight_sum += weight;
            }
            output.push(if weight_sum.abs() > 1e-9 { acc / weight_sum } else { 0.0 });
            self.next_output += 1;
        }

        // Drop input that no later output can reach.
        let position = self.next_output as u128 * self.step as u128;
        let center = (position / self.phases as u128) as i64;
        let keep_from = i64::max(self.buffer_start, center - HALF_TAPS + 1).min(self.received);
        let drop = (keep_from - self.buffer_start) as usize;
        if drop > 4096 {
            self.buffer.drain(..drop);
            self.buffer_start = keep_from;
        }
    }

    /// The output length for everything received, rounded to the nearest sample.
    fn rounded_total(&self) -> u64 {
        ((self.received as u128 * self.phases as u128 * 2 + self.step as u128) / (self.step as u128 * 2)) as u64
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

/// Resamples a whole clip in one call.
pub(crate) fn resample(input: &[f32], source_rate: u32, target_rate: u32) -> Vec<f32> {
    if source_rate == target_rate || input.is_empty() || source_rate == 0 {
        return input.to_vec();
    }
    let mut resampler = StreamResampler::new(source_rate, target_rate);
    let mut output = Vec::with_capacity((input.len() as u64 * target_rate as u64 / source_rate as u64) as usize + 1);
    resampler.push(input, &mut output);
    resampler.finish(&mut output);
    output
}

/// An audio file decoded on demand to mono at `target_rate`.
pub(crate) struct AudioFileReader {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    target_rate: u32,
    resampler: Option<StreamResampler>,
    duration_secs: Option<f32>,
    finished: bool,
}

impl AudioFileReader {
    pub(crate) fn open(path: &Path, target_rate: u32) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
        let stream = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(extension) = path.extension().and_then(|extension| extension.to_str()) {
            hint.with_extension(extension);
        }
        let probed = symphonia::default::get_probe()
            .format(&hint, stream, &FormatOptions::default(), &MetadataOptions::default())
            .map_err(|err| {
                anyhow!("This file is not a supported audio format (WAV, MP3, M4A, MP4, FLAC or OGG Vorbis): {err}")
            })?;
        let format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|track| track.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| anyhow!("No audio track was found in {}", path.display()))?;
        let track_id = track.id;
        let params = track.codec_params.clone();
        let decoder = symphonia::default::get_codecs()
            .make(&params, &DecoderOptions::default())
            .map_err(|err| anyhow!("The audio in this file uses a format that cannot be decoded: {err}"))?;
        let duration_secs = match (params.n_frames, params.sample_rate) {
            (Some(frames), Some(rate)) if rate > 0 => Some(frames as f32 / rate as f32),
            _ => None,
        };
        Ok(Self {
            format,
            decoder,
            track_id,
            target_rate,
            resampler: None,
            duration_secs,
            finished: false,
        })
    }

    /// Length of the recording, when the file states it.
    pub(crate) fn duration_secs(&self) -> Option<f32> {
        self.duration_secs
    }

    /// Appends the next piece of audio to `output`. Returns `false` once the file has ended.
    pub(crate) fn read_into(&mut self, output: &mut Vec<f32>) -> Result<bool> {
        if self.finished {
            return Ok(false);
        }
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(self.finish(output));
                }
                Err(SymphoniaError::ResetRequired) => return Ok(self.finish(output)),
                Err(err) => return Err(anyhow!("Failed to read the audio file: {err}")),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(decoded) => decoded,
                // A damaged packet is skipped; the rest of the file is still usable.
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(SymphoniaError::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(self.finish(output));
                }
                Err(err) => return Err(anyhow!("Failed to decode the audio file: {err}")),
            };
            let spec = *decoded.spec();
            let channels = usize::max(1, spec.channels.count());
            let mut samples = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
            samples.copy_interleaved_ref(decoded);
            let mono: Vec<f32> = samples
                .samples()
                .chunks(channels)
                .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
                .collect();
            if mono.is_empty() {
                continue;
            }
            let target_rate = self.target_rate;
            self.resampler
                .get_or_insert_with(|| StreamResampler::new(spec.rate, target_rate))
                .push(&mono, output);
            return Ok(true);
        }
    }

    fn finish(&mut self, output: &mut Vec<f32>) -> bool {
        self.finished = true;
        if let Some(resampler) = self.resampler.as_mut() {
            resampler.finish(output);
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The straightforward version of the resampler: every weight computed in place.
    fn reference_resample(input: &[f32], source_rate: u32, target_rate: u32) -> Vec<f32> {
        let ratio = target_rate as f64 / source_rate as f64;
        let cutoff = f64::min(1.0, ratio);
        let output_len = ((input.len() as f64) * ratio).round() as usize;
        let mut output = Vec::with_capacity(output_len);
        for out_index in 0..output_len {
            let position = out_index as f64 / ratio;
            let center = position.floor() as i64;
            let mut acc = 0.0f64;
            let mut weight_sum = 0.0f64;
            for tap in (center - HALF_TAPS + 1)..=(center + HALF_TAPS) {
                if tap < 0 || tap as usize >= input.len() {
                    continue;
                }
                let distance = position - tap as f64;
                let x = distance * cutoff;
                let sinc = if x.abs() < 1e-9 {
                    1.0
                } else {
                    (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
                };
                let window = 0.5 + 0.5 * (std::f64::consts::PI * distance / HALF_TAPS as f64).cos();
                let weight = sinc * window;
                acc += input[tap as usize] as f64 * weight;
                weight_sum += weight;
            }
            output.push(if weight_sum.abs() > 1e-9 { (acc / weight_sum) as f32 } else { 0.0 });
        }
        output
    }

    fn test_signal(len: usize, rate: u32) -> Vec<f32> {
        (0..len)
            .map(|i| {
                let t = i as f32 / rate as f32;
                0.5 * (t * 440.0 * std::f32::consts::TAU).sin() + 0.2 * (t * 1870.0 * std::f32::consts::TAU).sin()
            })
            .collect()
    }

    #[test]
    fn resample_matches_the_reference_for_common_rates() {
        for (source, target) in [(48_000, 16_000), (44_100, 16_000), (24_000, 44_100), (8_000, 16_000), (22_050, 16_000)] {
            let input = test_signal(source as usize / 2 + 137, source);
            let expected = reference_resample(&input, source, target);
            let actual = resample(&input, source, target);
            assert_eq!(actual.len(), expected.len(), "{source} -> {target}");
            let worst = actual.iter().zip(&expected).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
            assert!(worst < 1e-4, "{source} -> {target}: worst difference {worst}");
        }
    }

    #[test]
    fn resampling_in_pieces_gives_the_same_samples() {
        let input = test_signal(44_100 * 2 + 311, 44_100);
        let whole = resample(&input, 44_100, 16_000);
        let mut resampler = StreamResampler::new(44_100, 16_000);
        let mut pieces = Vec::new();
        for (index, piece) in input.chunks(1_153).enumerate() {
            // Uneven piece sizes, including very small ones.
            let (first, second) = piece.split_at(usize::min(piece.len(), index % 7));
            resampler.push(first, &mut pieces);
            resampler.push(second, &mut pieces);
        }
        resampler.finish(&mut pieces);
        assert_eq!(pieces.len(), whole.len());
        assert!(pieces.iter().zip(&whole).all(|(a, b)| a == b));
    }

    #[test]
    fn same_rate_is_passed_through() {
        let input = test_signal(1_000, 16_000);
        assert_eq!(resample(&input, 16_000, 16_000), input);
    }

    #[test]
    fn reads_a_wav_file_as_mono_at_the_target_rate() {
        let dir = std::env::temp_dir().join(format!("voicereader-audio-decode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stereo.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for sample in test_signal(48_000, 48_000) {
            let value = (sample * 20_000.0) as i16;
            writer.write_sample(value).unwrap();
            writer.write_sample(value).unwrap();
        }
        writer.finalize().unwrap();

        let mut reader = AudioFileReader::open(&path, 16_000).unwrap();
        assert!((reader.duration_secs().unwrap() - 1.0).abs() < 0.01);
        let mut samples = Vec::new();
        while reader.read_into(&mut samples).unwrap() {}
        assert!((samples.len() as i64 - 16_000).abs() <= 1, "got {} samples", samples.len());
        let peak = samples.iter().fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        assert!(peak > 0.2 && peak < 0.6, "peak {peak}");
        std::fs::remove_dir_all(&dir).ok();

        assert!(AudioFileReader::open(&dir.join("missing.wav"), 16_000).is_err());
    }
}
