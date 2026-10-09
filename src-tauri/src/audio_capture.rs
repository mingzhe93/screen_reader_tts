//! Live audio from an input device (a microphone), as mono samples at a chosen rate.

use std::{fs::File, io::BufWriter, path::Path};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SizedSample, Stream, StreamConfig};

use crate::audio_decode::StreamResampler;

/// How long one read waits for audio before handing back what it has.
const READ_WINDOW: Duration = Duration::from_millis(250);
/// A device that delivers nothing for this long is treated as broken. A Bluetooth headset
/// needs over a second to start, so this must be generous.
const SILENT_DEVICE_TIMEOUT: Duration = Duration::from_secs(10);

/// Makes sure the audio system is first touched by a thread that never exits.
///
/// On Windows the audio library keeps one device enumerator for the whole process,
/// created on whichever thread uses it first. If that thread ends, the enumerator goes
/// with it, and the next use from another thread is an access violation. The app's
/// background threads do end when idle, so every entry point calls this first.
fn ensure_audio_host() {
    static HOST_THREAD: OnceLock<()> = OnceLock::new();
    HOST_THREAD.get_or_init(|| {
        let (ready, wait) = channel::<()>();
        let spawned = std::thread::Builder::new().name("audio-host".to_string()).spawn(move || {
            let _ = cpal::default_host().input_devices().map(|devices| devices.count());
            let _ = ready.send(());
            loop {
                std::thread::park();
            }
        });
        if spawned.is_ok() {
            let _ = wait.recv();
        }
    });
}

pub(crate) struct AudioInput {
    pub name: String,
    pub is_default: bool,
}

/// The input devices the system offers, default first.
pub(crate) fn list_inputs() -> Vec<AudioInput> {
    ensure_audio_host();
    let host = cpal::default_host();
    let default_name = host.default_input_device().and_then(|device| device.name().ok());
    let mut inputs: Vec<AudioInput> = host
        .input_devices()
        .map(|devices| {
            devices
                .filter_map(|device| device.name().ok())
                .map(|name| AudioInput {
                    is_default: Some(&name) == default_name.as_ref(),
                    name,
                })
                .collect()
        })
        .unwrap_or_default();
    inputs.sort_by_key(|input| !input.is_default);
    inputs
}

/// A running capture from one input device. Recording starts when this is opened and
/// ends when it is dropped. It must stay on the thread that opened it.
pub(crate) struct MicrophoneSource {
    _stream: Stream,
    recording: Option<hound::WavWriter<BufWriter<File>>>,
    receiver: Receiver<Vec<f32>>,
    resampler: StreamResampler,
    error: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    /// Samples (at the target rate) one read tries to collect before returning.
    read_block: usize,
    last_audio: Instant,
}

impl MicrophoneSource {
    /// Opens `device_name`, or the system default input when it is `None`. Setting
    /// `stop` ends the recording: reads then drain what was captured and report the end.
    #[cfg(test)]
    pub(crate) fn open(device_name: Option<&str>, target_rate: u32, stop: Arc<AtomicBool>) -> Result<Self> {
        Self::open_recorded(device_name, target_rate, stop, None)
    }

    pub(crate) fn open_recorded(device_name: Option<&str>, target_rate: u32, stop: Arc<AtomicBool>, recording_path: Option<&Path>) -> Result<Self> {
        ensure_audio_host();
        let host = cpal::default_host();
        let device = match device_name {
            Some(wanted) => host
                .input_devices()
                .context("Failed to list audio input devices")?
                .find(|device| device.name().ok().as_deref() == Some(wanted))
                .ok_or_else(|| anyhow!("The microphone \"{wanted}\" is no longer available."))?,
            None => host
                .default_input_device()
                .ok_or_else(|| anyhow!("No microphone was found. Connect one, or check the system's sound input settings."))?,
        };
        let supported = device
            .default_input_config()
            .map_err(|err| anyhow!("The microphone could not be opened: {err}"))?;
        let sample_format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let channels = usize::max(1, config.channels as usize);
        let source_rate = config.sample_rate.0;

        let (sender, receiver) = channel::<Vec<f32>>();
        let error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let stream = match sample_format {
            SampleFormat::F32 => build_stream::<f32>(&device, &config, channels, sender, error.clone(), stop.clone(), |s| s),
            SampleFormat::I16 => {
                build_stream::<i16>(&device, &config, channels, sender, error.clone(), stop.clone(), |s| s as f32 / 32_768.0)
            }
            SampleFormat::U16 => build_stream::<u16>(&device, &config, channels, sender, error.clone(), stop.clone(), |s| {
                (s as f32 - 32_768.0) / 32_768.0
            }),
            SampleFormat::I32 => {
                build_stream::<i32>(&device, &config, channels, sender, error.clone(), stop.clone(), |s| s as f32 / 2_147_483_648.0)
            }
            other => Err(anyhow!("The microphone uses a sample format that is not supported ({other:?}).")),
        }?;
        let recording = recording_path.map(|path| hound::WavWriter::create(path, hound::WavSpec {
            channels: 1, sample_rate: source_rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int,
        })).transpose().context("Could not create the recording")?;
        stream
            .play()
            .map_err(|err| anyhow!("The microphone could not be started: {err}"))?;

        Ok(Self {
            _stream: stream,
            recording,
            receiver,
            resampler: StreamResampler::new(source_rate, target_rate),
            error,
            stop,
            read_block: (target_rate / 10) as usize,
            last_audio: Instant::now(),
        })
    }

    /// Appends newly captured audio to `output`. Returns `false` once the recording
    /// was stopped and everything captured has been handed over.
    pub(crate) fn read_into(&mut self, output: &mut Vec<f32>) -> Result<bool> {
        let start_len = output.len();
        let deadline = Instant::now() + READ_WINDOW;
        loop {
            if let Some(message) = self.error.lock().ok().and_then(|mut error| error.take()) {
                return Err(anyhow!("The microphone stopped working: {message}"));
            }
            let stopping = self.stop.load(Ordering::SeqCst);
            let wait = if stopping { Duration::ZERO } else { Duration::from_millis(50) };
            match self.receiver.recv_timeout(wait) {
                Ok(block) => {
                    self.last_audio = Instant::now();
                    if let Some(writer) = self.recording.as_mut() {
                        write_recording_samples(writer, &block)?;
                    }
                    self.resampler.push(&block, output);
                }
                Err(RecvTimeoutError::Timeout) if stopping => {
                    self.resampler.finish(output);
                    if let Some(writer) = self.recording.take() { writer.finalize().context("Could not finish the recording")?; }
                    return Ok(false);
                }
                Err(RecvTimeoutError::Timeout) if self.last_audio.elapsed() > SILENT_DEVICE_TIMEOUT => {
                    return Err(anyhow!(
                        "No audio is arriving from the microphone. Check that it is connected and that apps are allowed to use it."
                    ));
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.resampler.finish(output);
                    if let Some(writer) = self.recording.take() { writer.finalize().context("Could not finish the recording")?; }
                    return Ok(false);
                }
            }
            if output.len() - start_len >= self.read_block || Instant::now() >= deadline {
                return Ok(true);
            }
        }
    }
}

fn write_recording_samples(writer: &mut hound::WavWriter<BufWriter<File>>, samples: &[f32]) -> Result<()> {
    for &sample in samples {
        let sample = if sample.is_finite() { sample.clamp(-1.0, 1.0) } else { 0.0 };
        writer.write_sample((sample * i16::MAX as f32).round() as i16).context("Could not write the recording")?;
    }
    Ok(())
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    channels: usize,
    sender: Sender<Vec<f32>>,
    error: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    to_f32: fn(T) -> f32,
) -> Result<Stream>
where
    T: SizedSample + Send + 'static,
{
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                // Stop capturing immediately, while queued audio is still drained
                // and the transcriber finishes the last words.
                if stop.load(Ordering::SeqCst) { return; }
                let mono: Vec<f32> = data
                    .chunks(channels)
                    .map(|frame| frame.iter().map(|sample| to_f32(*sample)).sum::<f32>() / frame.len() as f32)
                    .collect();
                // The receiver is gone once the recording has ended; nothing to do then.
                let _ = sender.send(mono);
            },
            move |err| {
                if let Ok(mut slot) = error.lock() {
                    *slot = Some(err.to_string());
                }
            },
            None,
        )
        .map_err(|err| anyhow!("The microphone could not be opened: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_keeps_native_rate_and_exact_sample_count() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recording.wav");
        let spec = hound::WavSpec { channels: 1, sample_rate: 48000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        write_recording_samples(&mut writer, &[0.0, 0.5]).unwrap();
        write_recording_samples(&mut writer, &[-0.5, 2.0, f32::NAN]).unwrap();
        writer.finalize().unwrap();
        let mut reader = hound::WavReader::open(path).unwrap();
        assert_eq!(reader.spec(), spec);
        assert_eq!(reader.samples::<i16>().map(Result::unwrap).collect::<Vec<_>>(), vec![0, 16384, -16384, 32767, 0]);
    }

    /// Each use comes from a thread that then ends, as the app's background threads do.
    /// Without `ensure_audio_host` the second use crashes the process on Windows.
    #[test]
    fn devices_can_be_listed_from_threads_that_end() {
        for _ in 0..3 {
            std::thread::spawn(|| {
                list_inputs();
                let _ = MicrophoneSource::open(Some("no such microphone"), 16_000, Arc::new(AtomicBool::new(false)));
            })
            .join()
            .unwrap();
        }
        list_inputs();
    }

    /// Records about a second from the default microphone. Run with:
    /// `cargo test --features build-base --lib -- --ignored --nocapture captures`
    #[test]
    #[ignore = "needs a microphone"]
    fn captures_from_the_default_microphone() {
        for input in list_inputs() {
            println!("input: {}{}", input.name, if input.is_default { " (default)" } else { "" });
        }
        let stop = Arc::new(AtomicBool::new(false));
        if list_inputs().is_empty() {
            // Nothing to record from: opening must fail cleanly rather than hang or crash.
            println!("no microphone connected; checking the error path only");
            assert!(MicrophoneSource::open(None, 16_000, stop).is_err());
            return;
        }
        let mut microphone = MicrophoneSource::open(None, 16_000, stop.clone()).unwrap();
        let started = Instant::now();
        let mut samples = Vec::new();
        // A Bluetooth headset can take over a second to start sending audio.
        while samples.is_empty() {
            assert!(started.elapsed() < Duration::from_secs(8), "no audio arrived");
            assert!(microphone.read_into(&mut samples).unwrap());
        }
        println!("first audio after {:.2?}", started.elapsed());
        let first_audio = Instant::now();
        while first_audio.elapsed() < Duration::from_millis(1_000) {
            assert!(microphone.read_into(&mut samples).unwrap());
        }
        stop.store(true, Ordering::SeqCst);
        while microphone.read_into(&mut samples).unwrap() {}
        let peak = samples.iter().fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        println!("{} samples, peak {peak:.4}", samples.len());
        assert!(samples.len() > 12_000 && samples.len() < 24_000, "got {} samples", samples.len());
        assert!(samples.iter().all(|sample| sample.is_finite()));

        let missing = MicrophoneSource::open(Some("no such microphone"), 16_000, stop);
        assert!(missing.is_err());
    }
}
