//! Keeps only the current session's audio and exports it with the transcript.
use std::{fs::File, io::Write, path::{Path, PathBuf}, sync::{Arc, Mutex}};
use anyhow::{anyhow, Context, Result};
use tempfile::{NamedTempFile, TempDir};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

pub(crate) struct SessionAudio {
    pub path: PathBuf,
    // Owns the temporary microphone WAV; replaced sessions are removed automatically.
    pub _temporary_directory: Option<TempDir>,
}

static LAST_AUDIO: Mutex<Option<(String, Arc<SessionAudio>)>> = Mutex::new(None);

pub(crate) fn clear_audio() {
    if let Ok(mut audio) = LAST_AUDIO.lock() { *audio = None; }
}

pub(crate) fn keep_audio(job_id: String, audio: SessionAudio) -> Result<()> {
    *LAST_AUDIO.lock().map_err(|_| anyhow!("Recording state lock poisoned"))? = Some((job_id, Arc::new(audio)));
    Ok(())
}

pub(crate) fn export_session(job_id: &str, destination: &Path, transcript: &str, format: &str) -> Result<()> {
    let audio = LAST_AUDIO.lock().map_err(|_| anyhow!("Recording state lock poisoned"))?
        .as_ref().filter(|(id, _)| id == job_id).map(|(_, audio)| audio.clone())
        .ok_or_else(|| anyhow!("This recording is no longer available. Finish a new recording before exporting it."))?;
    export_archive(&audio.path, destination, transcript, format)
}

fn export_archive(audio_path: &Path, destination: &Path, transcript: &str, format: &str) -> Result<()> {
    if !matches!(format, "txt" | "md" | "srt") { return Err(anyhow!("Unsupported transcript format")); }
    let mut input = File::open(audio_path).context("The source recording is no longer available")?;
    let parent = destination.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    // Build alongside the destination, then replace it only after all writes succeed.
    let mut temporary = NamedTempFile::new_in(parent).context("Could not create the export file")?;
    {
        let mut archive = ZipWriter::new(temporary.as_file_mut());
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored).large_file(true);
        let extension = audio_path.extension().and_then(|s| s.to_str())
            .filter(|s| !s.is_empty() && s.len() <= 10 && s.chars().all(|c| c.is_ascii_alphanumeric()))
            .unwrap_or("audio");
        archive.start_file(format!("recording.{extension}"), options)?;
        std::io::copy(&mut input, &mut archive).context("Could not save the recording")?;
        archive.start_file(format!("transcription.{format}"), options)?;
        archive.write_all(transcript.as_bytes())?;
        archive.finish()?;
    }
    temporary.persist(destination).map_err(|err| anyhow!("Could not save the export: {}", err.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    #[test]
    fn archive_preserves_original_audio_and_transcript() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.wav");
        let output = directory.path().join("session.zip");
        let mut wav = hound::WavWriter::create(&source, hound::WavSpec { channels: 1, sample_rate: 48000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int }).unwrap();
        for sample in [0i16, 12000, -12000] { wav.write_sample(sample).unwrap(); }
        wav.finalize().unwrap();
        let text = "[0:00] Alice: Hello.\n";
        export_archive(&source, &output, text, "txt").unwrap();
        let mut archive = zip::ZipArchive::new(File::open(&output).unwrap()).unwrap();
        assert_eq!(archive.len(), 2);
        let mut audio = Vec::new();
        archive.by_name("recording.wav").unwrap().read_to_end(&mut audio).unwrap();
        assert_eq!(audio, std::fs::read(source).unwrap());
        let mut saved = String::new();
        archive.by_name("transcription.txt").unwrap().read_to_string(&mut saved).unwrap();
        assert_eq!(saved, text);
    }
    #[test]
    fn missing_audio_does_not_replace_an_existing_export() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("session.zip");
        std::fs::write(&output, "keep this").unwrap();
        assert!(export_archive(&directory.path().join("missing.wav"), &output, "hello", "txt").is_err());
        assert_eq!(std::fs::read_to_string(output).unwrap(), "keep this");
    }
    #[test]
    fn stale_job_cannot_export_another_recording() {
        assert!(export_session("missing-job", Path::new("unused.zip"), "text", "txt").is_err());
    }
}
