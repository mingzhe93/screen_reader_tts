//! Resumable downloads of a model's files from Hugging Face.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use tokio::time::Duration;

/// Downloads `files` of the Hugging Face repo `repo` into `model_dir`, resuming partial
/// files. `on_progress(file, file_index, downloaded_bytes, total_bytes)` reports totals
/// across the whole model. Returns the total size in bytes.
pub(crate) async fn download_model_files<F>(
    repo: &str,
    files: &[&str],
    model_dir: &Path,
    on_progress: F,
) -> Result<u64>
where
    F: Fn(&str, usize, u64, u64),
{
    use std::io::Write;

    let client = Client::builder()
        .user_agent(concat!("VoiceReader/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("Failed to create download client")?;
    let file_url = |file: &str| format!("https://huggingface.co/{repo}/resolve/main/{file}");

    // Ask for every file's size first so progress can be reported across the whole model.
    let mut sizes: Vec<u64> = Vec::with_capacity(files.len());
    for file in files {
        let response = client
            .head(file_url(file))
            .send()
            .await
            .with_context(|| format!("Failed to reach Hugging Face for {file}"))?;
        if !response.status().is_success() {
            return Err(anyhow!("Hugging Face returned {} for {file}", response.status()));
        }
        let size = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| anyhow!("Hugging Face did not report a size for {file}"))?;
        sizes.push(size);
    }
    let total_bytes: u64 = sizes.iter().sum();

    let mut downloaded_bytes = 0u64;
    let mut last_emit = Instant::now();
    for (index, file) in files.iter().enumerate() {
        let expected = sizes[index];
        let target = file.split('/').fold(model_dir.to_path_buf(), |path, part| path.join(part));
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        let emit_progress = |downloaded_bytes: u64| on_progress(file, index + 1, downloaded_bytes, total_bytes);

        if std::fs::metadata(&target).map(|meta| meta.len() == expected).unwrap_or(false) {
            downloaded_bytes += expected;
            emit_progress(downloaded_bytes);
            continue;
        }

        let mut part_name = target.as_os_str().to_os_string();
        part_name.push(".part");
        let part_path = PathBuf::from(part_name);
        let mut have = std::fs::metadata(&part_path).map(|meta| meta.len()).unwrap_or(0);
        if have > expected {
            have = 0;
        }

        if have < expected {
            let mut request = client.get(file_url(file));
            if have > 0 {
                request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
            }
            let mut response = request
                .send()
                .await
                .with_context(|| format!("Failed to download {file}"))?;
            let status = response.status();
            if !status.is_success() {
                return Err(anyhow!("Hugging Face returned {status} for {file}"));
            }
            // A server that ignores the range sends the whole file again.
            let resuming = have > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT;
            if !resuming {
                have = 0;
            }
            let mut output = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(resuming)
                .truncate(!resuming)
                .open(&part_path)
                .with_context(|| format!("Failed to open {}", part_path.display()))?;
            emit_progress(downloaded_bytes + have);
            while let Some(bytes) = response
                .chunk()
                .await
                .with_context(|| format!("Download of {file} was interrupted"))?
            {
                output
                    .write_all(&bytes)
                    .with_context(|| format!("Failed to write {}", part_path.display()))?;
                have += bytes.len() as u64;
                if last_emit.elapsed() >= Duration::from_millis(250) {
                    last_emit = Instant::now();
                    emit_progress(downloaded_bytes + have);
                }
            }
            output
                .flush()
                .with_context(|| format!("Failed to write {}", part_path.display()))?;
        }

        if have != expected {
            return Err(anyhow!(
                "Download of {file} is incomplete ({have} of {expected} bytes). Try again to resume."
            ));
        }
        if target.exists() {
            std::fs::remove_file(&target).with_context(|| format!("Failed to replace {}", target.display()))?;
        }
        std::fs::rename(&part_path, &target).with_context(|| format!("Failed to finalize {}", target.display()))?;
        downloaded_bytes += expected;
        emit_progress(downloaded_bytes);
    }

    Ok(total_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio8_local::{is_audio8_model_dir, AUDIO8_MODEL_FILES, AUDIO8_REPO};
    use crate::voicereader_core::AUDIO8_DOWNLOAD_SIZE_BYTES;

    /// Downloads into a copy of an existing model directory with one file missing and one
    /// partially downloaded, so only a few megabytes are fetched. Run with:
    /// `VOICEREADER_AUDIO8_TEST_MODEL_DIR=<model dir> cargo test --features build-base -- --ignored`
    #[tokio::test]
    #[ignore = "needs network access and the downloaded Audio8 model"]
    async fn download_fetches_missing_files_and_resumes_partial_ones() {
        let source = PathBuf::from(
            std::env::var("VOICEREADER_AUDIO8_TEST_MODEL_DIR").expect("set VOICEREADER_AUDIO8_TEST_MODEL_DIR"),
        );
        let target = std::env::temp_dir().join(format!("voicereader-audio8-download-{}", std::process::id()));
        let missing = "runtime_manifest.json";
        let partial = "tokenizer/tokenizer.json";
        for file in AUDIO8_MODEL_FILES {
            let to = target.join(file);
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            if file == missing {
                continue;
            }
            if file == partial {
                let bytes = std::fs::read(source.join(file)).unwrap();
                std::fs::write(target.join(format!("{file}.part")), &bytes[..bytes.len() / 3]).unwrap();
                continue;
            }
            std::fs::copy(source.join(file), &to).unwrap();
        }
        assert!(!is_audio8_model_dir(&target));

        let last_progress = std::sync::Mutex::new((0u64, 0u64));
        let total = download_model_files(AUDIO8_REPO, &AUDIO8_MODEL_FILES, &target, |_, _, downloaded, total| {
            let mut guard = last_progress.lock().unwrap();
            assert!(downloaded >= guard.0, "progress went backwards");
            *guard = (downloaded, total);
        })
        .await
        .expect("download succeeds");

        assert_eq!(total, AUDIO8_DOWNLOAD_SIZE_BYTES);
        assert_eq!(*last_progress.lock().unwrap(), (total, total));
        assert!(is_audio8_model_dir(&target));
        for file in [missing, partial] {
            assert_eq!(
                std::fs::read(target.join(file)).unwrap(),
                std::fs::read(source.join(file)).unwrap(),
                "{file} differs from the reference copy"
            );
            assert!(!target.join(format!("{file}.part")).exists());
        }
        std::fs::remove_dir_all(&target).ok();
    }
}
