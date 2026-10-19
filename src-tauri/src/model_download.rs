//! Resumable downloads of a model's files from Hugging Face, and single-file downloads
//! verified against a pinned size and SHA-256.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Returned (inside `anyhow::Error`) by [`download_verified_file`] when its cancel flag was
/// set. The `.part` file is kept so the next attempt resumes. Detect it with
/// `err.is::<DownloadCancelled>()`.
#[derive(Debug)]
pub(crate) struct DownloadCancelled;

impl std::fmt::Display for DownloadCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Download cancelled.")
    }
}

impl std::error::Error for DownloadCancelled {}

/// Downloads `url` to `target`, which only ever appears after its SHA-256 matched
/// `expected_sha256`. Data goes to `<target>.part` first and a later call resumes it with an
/// HTTP range request. A `target` that already exists is hashed: it is kept if it matches and
/// deleted and downloaded again otherwise. `on_progress(downloaded)` reports bytes of this
/// file, at most every 250 ms plus once at the start and end. `cancel` is checked at least
/// every 250 ms, both while waiting for the response and while data arrives.
pub(crate) async fn download_verified_file<F>(
    client: &Client,
    url: &str,
    expected_size: u64,
    expected_sha256: &str,
    target: &Path,
    cancel: &AtomicBool,
    mut on_progress: F,
) -> Result<()>
where
    F: FnMut(u64),
{
    use std::io::Write;

    let name = file_label(target);
    if let Ok(meta) = std::fs::metadata(target) {
        let verified = meta.len() == expected_size && {
            let (path, expected) = (target.to_path_buf(), expected_sha256.trim().to_string());
            tokio::task::spawn_blocking(move || sha256_file(&path).map(|actual| actual.eq_ignore_ascii_case(&expected)))
                .await
                .map_err(|err| anyhow!("Verifying {name} failed: {err}"))??
        };
        if verified {
            on_progress(expected_size);
            return Ok(());
        }
        std::fs::remove_file(target)
            .with_context(|| format!("Failed to remove {}, which failed verification", target.display()))?;
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let part_path = part_path(target);
    let mut have = std::fs::metadata(&part_path).map(|meta| meta.len()).unwrap_or(0);
    if have > expected_size {
        std::fs::remove_file(&part_path).with_context(|| format!("Failed to reset {}", part_path.display()))?;
        have = 0;
    }

    if have < expected_size {
        if cancel.load(Ordering::SeqCst) {
            return Err(DownloadCancelled.into());
        }
        let mut request = client.get(url);
        if have > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
        }
        // Wake regularly so a server that never answers can still be cancelled.
        let mut send = std::pin::pin!(request.send());
        let mut response = loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(DownloadCancelled.into());
            }
            match tokio::time::timeout(Duration::from_millis(250), send.as_mut()).await {
                Ok(result) => break result.with_context(|| format!("Failed to download {name}"))?,
                Err(_) => continue,
            }
        };
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow!("Server returned {status} for {name}"));
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
        on_progress(have);
        let mut last_emit = Instant::now();
        loop {
            if cancel.load(Ordering::SeqCst) {
                output.flush().ok();
                return Err(DownloadCancelled.into());
            }
            // Wake regularly so a stalled connection can still be cancelled.
            let chunk = match tokio::time::timeout(Duration::from_millis(250), response.chunk()).await {
                Ok(chunk) => chunk.with_context(|| format!("Download of {name} was interrupted"))?,
                Err(_) => continue,
            };
            let Some(bytes) = chunk else { break };
            output
                .write_all(&bytes)
                .with_context(|| format!("Failed to write {}", part_path.display()))?;
            have += bytes.len() as u64;
            if have > expected_size {
                drop(output);
                std::fs::remove_file(&part_path).ok();
                return Err(anyhow!(
                    "Download of {name} is larger than the expected {expected_size} bytes; it was discarded."
                ));
            }
            if last_emit.elapsed() >= Duration::from_millis(250) {
                last_emit = Instant::now();
                on_progress(have);
            }
        }
        output
            .flush()
            .with_context(|| format!("Failed to write {}", part_path.display()))?;
    }

    if have != expected_size {
        return Err(anyhow!(
            "Download of {name} is incomplete ({have} of {expected_size} bytes). Try again to resume."
        ));
    }
    // Hashing several GB takes seconds, so keep it off the async worker threads.
    let (part, final_target, expected) = (part_path.clone(), target.to_path_buf(), expected_sha256.to_string());
    tokio::task::spawn_blocking(move || finalize_part_file(&part, &final_target, &expected))
        .await
        .map_err(|err| anyhow!("Verifying {name} failed: {err}"))??;
    on_progress(expected_size);
    Ok(())
}

/// `<target>.part`, where [`download_verified_file`] writes before verification.
pub(crate) fn part_path(target: &Path) -> PathBuf {
    let mut part_name = target.as_os_str().to_os_string();
    part_name.push(".part");
    PathBuf::from(part_name)
}

/// Lowercase hex SHA-256 of a file, read in 1 MiB blocks.
pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut file = std::fs::File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Moves a fully downloaded `part` to `target` if its SHA-256 equals `expected_sha256`
/// (case-insensitive hex). On a mismatch the part file is deleted and `target` is untouched.
pub(crate) fn finalize_part_file(part: &Path, target: &Path, expected_sha256: &str) -> Result<()> {
    let name = file_label(target);
    let actual = sha256_file(part)?;
    if !actual.eq_ignore_ascii_case(expected_sha256.trim()) {
        std::fs::remove_file(part).ok();
        return Err(anyhow!(
            "{name} failed verification (SHA-256 {actual}, expected {}). The download was deleted; try again.",
            expected_sha256.trim().to_ascii_lowercase()
        ));
    }
    if target.exists() {
        std::fs::remove_file(target).with_context(|| format!("Failed to replace {}", target.display()))?;
    }
    std::fs::rename(part, target).with_context(|| format!("Failed to finalize {}", target.display()))
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio8_local::{is_audio8_model_dir, AUDIO8_MODEL_FILES, AUDIO8_REPO};
    use crate::voicereader_core::AUDIO8_DOWNLOAD_SIZE_BYTES;
    use std::sync::Arc;

    /// SHA-256 of the ASCII bytes `abc` (FIPS 180-2 test vector).
    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn sha256_file_matches_known_vector() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("abc.bin");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(sha256_file(&path).unwrap(), ABC_SHA256);
    }

    #[test]
    fn sha256_file_handles_data_larger_than_one_block() {
        use sha2::{Digest, Sha256};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("big.bin");
        let data: Vec<u8> = (0..(3 * (1 << 20) + 17)).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let expected: String = Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(sha256_file(&path).unwrap(), expected);
    }

    #[test]
    fn finalize_moves_part_to_target_when_hash_matches() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("model.gguf");
        let part = part_path(&target);
        std::fs::write(&part, b"abc").unwrap();
        finalize_part_file(&part, &target, &ABC_SHA256.to_ascii_uppercase()).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"abc");
        assert!(!part.exists());
    }

    #[test]
    fn finalize_replaces_an_existing_target_when_hash_matches() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("model.gguf");
        std::fs::write(&target, b"old").unwrap();
        let part = part_path(&target);
        std::fs::write(&part, b"abc").unwrap();
        finalize_part_file(&part, &target, ABC_SHA256).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"abc");
    }

    #[test]
    fn finalize_deletes_part_and_creates_no_target_on_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("model.gguf");
        let part = part_path(&target);
        std::fs::write(&part, b"abd").unwrap();
        let err = finalize_part_file(&part, &target, ABC_SHA256).unwrap_err();
        assert!(format!("{err:#}").contains("model.gguf"), "error names the file: {err:#}");
        assert!(!part.exists());
        assert!(!target.exists());
    }

    #[test]
    fn part_path_appends_suffix() {
        assert_eq!(part_path(Path::new("dir/a.tar.gz")), PathBuf::from("dir/a.tar.gz.part"));
    }

    /// Serves `body` with status 200 to one connection on a free loopback port.
    fn serve_once(body: &'static [u8]) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                request.push(byte[0]);
            }
            let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            stream.write_all(header.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
        });
        format!("http://{address}/file")
    }

    fn loopback_client() -> Client {
        Client::builder().no_proxy().build().unwrap()
    }

    #[tokio::test]
    async fn verified_download_replaces_an_existing_target_that_fails_the_hash() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("model.gguf");
        // Right size, wrong content: must not be accepted on size alone.
        std::fs::write(&target, b"abd").unwrap();
        let url = serve_once(b"abc");
        download_verified_file(&loopback_client(), &url, 3, ABC_SHA256, &target, &AtomicBool::new(false), |_| {})
            .await
            .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"abc");
        assert!(!part_path(&target).exists());
    }

    #[tokio::test]
    async fn verified_download_deletes_a_bad_target_even_when_the_download_fails() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("model.gguf");
        std::fs::write(&target, b"abd").unwrap();
        let result = download_verified_file(
            &loopback_client(),
            "http://127.0.0.1:9/unused",
            3,
            ABC_SHA256,
            &target,
            &AtomicBool::new(false),
            |_| {},
        )
        .await;
        assert!(result.is_err());
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn verified_download_cancels_while_waiting_for_response_headers() {
        // A server that accepts the connection and never answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/stalled", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let held: Vec<_> = listener.incoming().take(1).collect();
            std::thread::sleep(Duration::from_secs(10));
            drop(held);
        });
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("stalled.bin");
        let cancel = Arc::new(AtomicBool::new(false));
        let setter = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            setter.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            download_verified_file(&loopback_client(), &url, 3, ABC_SHA256, &target, &cancel, |_| {}),
        )
        .await
        .expect("download returned after cancel");
        assert!(result.unwrap_err().is::<DownloadCancelled>());
        assert!(started.elapsed() < Duration::from_millis(1200), "took {:?}", started.elapsed());
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn verified_download_skips_an_existing_target_of_the_right_size() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("done.bin");
        std::fs::write(&target, b"abc").unwrap();
        let mut reported = Vec::new();
        // The URL is never contacted because the target is complete and matches its hash.
        download_verified_file(
            &Client::new(),
            "http://127.0.0.1:9/unused",
            3,
            ABC_SHA256,
            &target,
            &AtomicBool::new(false),
            |downloaded| reported.push(downloaded),
        )
        .await
        .unwrap();
        assert_eq!(reported, vec![3]);
    }

    #[tokio::test]
    async fn verified_download_reports_cancellation_and_keeps_the_part() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("partial.bin");
        std::fs::write(part_path(&target), b"ab").unwrap();
        let err = download_verified_file(
            &Client::new(),
            "http://127.0.0.1:9/unused",
            3,
            ABC_SHA256,
            &target,
            &AtomicBool::new(true),
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(err.is::<DownloadCancelled>());
        assert_eq!(std::fs::read(part_path(&target)).unwrap(), b"ab");
        assert!(!target.exists());
    }

    /// Downloads the pinned macOS llama.cpp archive (12 MB) and verifies it. Run with:
    /// `cargo test --features build-base model_download:: -- --ignored`
    #[tokio::test]
    #[ignore = "needs network access"]
    async fn verified_download_fetches_and_verifies_the_pinned_backend_archive() {
        let asset = crate::assistant_catalogue::backend_asset_for("macos", "aarch64").unwrap();
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join(asset.file);
        let client = Client::builder()
            .user_agent(concat!("VoiceReader/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap();
        download_verified_file(&client, &asset.url(), asset.size, asset.sha256, &target, &AtomicBool::new(false), |_| {})
            .await
            .expect("download succeeds");
        assert_eq!(std::fs::metadata(&target).unwrap().len(), asset.size);
        assert!(!part_path(&target).exists());
    }

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
