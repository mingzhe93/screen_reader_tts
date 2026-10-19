//! Optional, pinned CPython WASI runtime. Guest code never starts a host process.
//! Only the standard library and timezone database are preopened, both read-only.

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use wasmtime::{
    Config, Engine, Linker, Module, ResourceLimiter, Store, StoreLimits, StoreLimitsBuilder,
};
use wasmtime_wasi::{
    p2::pipe::MemoryOutputPipe,
    preview1::{self, WasiP1Ctx},
    DirPerms, FilePerms, WasiCtxBuilder, WasiView,
};

const VERSION: &str = "3.14.7";
const TZ_VERSION: &str = "2026.5";
const PY_URL: &str = "https://github.com/brettcannon/cpython-wasi-build/releases/download/v3.14.7/python-3.14.7-wasi_sdk-24.zip";
const PY_SIZE: u64 = 14_291_017;
const PY_HASH: &str = "2e064d3fb8172471d39d741348efa722349c40b96301f69968dff714999c584b";
const TZ_URL: &str = "https://files.pythonhosted.org/packages/94/21/1e5995a1c920cce14e4bffae20c665ec10e7ed03ab25e006cd741092b718/tzdata-2026.5-py2.py3-none-any.whl";
const TZ_SIZE: u64 = 347_996;
const TZ_HASH: &str = "b683bd1b6659ddcd810ff02ad09ba821d4bf1065072805063eb35c49617905ac";
const MEMORY_BYTES: usize = 256 * 1024 * 1024;
const STDOUT_BYTES: usize = 3_000;
const STDERR_BYTES: usize = 1_000;
/// Stderr is captured up to this size and its **end** is returned: a traceback's last line
/// is the exception the model needs.
const STDERR_CAPTURE_BYTES: usize = 16_384;
const MAX_WASI_RESOURCES: usize = 128;
const MANIFEST: &str = "CPython 3.14.7 WASI SDK24\ntzdata 2026.5\n2e064d3fb8172471d39d741348efa722349c40b96301f69968dff714999c584b\nb683bd1b6659ddcd810ff02ad09ba821d4bf1065072805063eb35c49617905ac\n";

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SandboxStatus {
    pub installed: bool,
    pub state: String,
    pub version: String,
    pub download_size_bytes: u64,
    pub installed_size_bytes: u64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct SandboxOutput {
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub out_of_memory: bool,
    pub cancelled: bool,
    pub output_truncated: bool,
}

#[derive(Clone)]
pub(crate) struct SandboxState {
    inner: Arc<Inner>,
}
struct Inner {
    root: PathBuf,
    gate: tokio::sync::Mutex<()>,
    state: Mutex<(String, Option<String>)>,
    compiled: Mutex<Option<Arc<Compiled>>>,
    /// The running install's cancel flag, so the page can stop a download.
    install_cancel: Mutex<Option<Arc<AtomicBool>>>,
    /// The installed size, measured once: walking about 3,000 files on every status call
    /// (the Tools menu refreshes it) is needless work on the main thread.
    installed_size: Mutex<Option<u64>>,
}
struct Compiled {
    engine: Engine,
    module: Module,
}

/// This lock also serves as a cross-process run lease. Keep it outside the runtime
/// directory so deleting/reinstalling cannot replace the locked inode.
struct Lease {
    _file: File,
}
impl Lease {
    fn acquire(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("sandbox.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => bail!("The Python sandbox is busy in another VoiceReader window. Try again when it finishes."),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }
}

impl SandboxState {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                root,
                gate: tokio::sync::Mutex::new(()),
                state: Mutex::new(("not_installed".into(), None)),
                compiled: Mutex::new(None),
                install_cancel: Mutex::new(None),
                installed_size: Mutex::new(None),
            }),
        }
    }
    fn runtime(&self) -> PathBuf {
        self.inner.root.join("sandbox")
    }
    fn installed(&self) -> bool {
        let path = self.runtime();
        std::fs::read_to_string(path.join("manifest.txt")).is_ok_and(|text| text == MANIFEST)
            && path.join("python.wasm").is_file()
            && path.join("lib/python3.14/encodings/__init__.py").is_file()
            && path.join("tz/tzdata/zoneinfo/Asia/Singapore").is_file()
            && path.join("LICENSE").is_file()
            && path
                .join("tz/tzdata-2026.5.dist-info/licenses/LICENSE")
                .is_file()
    }
    pub(crate) fn status(&self) -> SandboxStatus {
        let installed = self.installed();
        let (phase, error) = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        SandboxStatus {
            installed,
            state: if phase == "installing" || phase == "failed" {
                phase
            } else if installed {
                "ready".into()
            } else {
                "not_installed".into()
            },
            version: format!("CPython {VERSION} · tzdata {TZ_VERSION}"),
            download_size_bytes: PY_SIZE + TZ_SIZE,
            installed_size_bytes: if installed {
                *self
                    .inner
                    .installed_size
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get_or_insert_with(|| directory_size(&self.runtime()))
            } else {
                0
            },
            error,
        }
    }
    pub(crate) async fn install(&self, cancel: Arc<AtomicBool>) -> Result<SandboxStatus> {
        let state = self.clone();
        tokio::spawn(async move { state.install_locked(cancel).await })
            .await
            .context("Python installation worker failed")?
    }
    /// Stops a running download or extraction. Returns false when nothing is installing.
    pub(crate) fn cancel_install(&self) -> bool {
        match self.inner.install_cancel.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            Some(flag) => {
                flag.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }
    async fn install_locked(&self, cancel: Arc<AtomicBool>) -> Result<SandboxStatus> {
        let _gate = self.inner.gate.try_lock().map_err(|_| {
            anyhow!("The Python sandbox is busy. Wait for the current operation to finish.")
        })?;
        let _lease = Lease::acquire(&self.inner.root)?;
        if self.installed() {
            return Ok(self.status());
        }
        *self.inner.state.lock().unwrap_or_else(|p| p.into_inner()) = ("installing".into(), None);
        // The detached worker keeps its lease through extraction; reset status on panic.
        let mut guard = InstallGuard {
            inner: self.inner.clone(),
            finished: false,
        };
        *self.inner.install_cancel.lock().unwrap_or_else(|p| p.into_inner()) = Some(cancel.clone());
        let result = self.install_inner(cancel.clone()).await;
        *self.inner.install_cancel.lock().unwrap_or_else(|p| p.into_inner()) = None;
        *self.inner.installed_size.lock().unwrap_or_else(|p| p.into_inner()) = None;
        if result.is_err() {
            // A failed or cancelled install leaves no partial download (up to about 60 MB).
            let _ = std::fs::remove_dir_all(self.inner.root.join("sandbox.staging"));
        }
        let cancelled = cancel.load(Ordering::SeqCst);
        *self.inner.state.lock().unwrap_or_else(|p| p.into_inner()) = match &result {
            Ok(()) => ("ready".into(), None),
            Err(_) if cancelled => ("not_installed".into(), None),
            Err(error) => ("failed".into(), Some(format!("{error:#}"))),
        };
        guard.finished = true;
        if cancelled && result.is_err() {
            anyhow::bail!("The Python code runner download was cancelled.");
        }
        result?;
        Ok(self.status())
    }
    async fn install_inner(&self, cancel: Arc<AtomicBool>) -> Result<()> {
        let stage = self.inner.root.join("sandbox.staging");
        if stage.exists() {
            std::fs::remove_dir_all(&stage)?;
        }
        std::fs::create_dir_all(&stage)?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .https_only(true)
            .timeout(Duration::from_secs(300))
            .user_agent(concat!("VoiceReader/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let downloads = [
            (PY_URL, PY_SIZE, PY_HASH, "python.zip"),
            (TZ_URL, TZ_SIZE, TZ_HASH, "tzdata.whl"),
        ];
        for (url, size, hash, name) in downloads {
            crate::model_download::download_verified_file(
                &client,
                url,
                size,
                hash,
                &stage.join(name),
                &cancel,
                |_| {},
            )
            .await?;
        }
        let stage_copy = stage.clone();
        let extraction = tokio::task::spawn_blocking(move || -> Result<()> {
            extract_archive(&stage_copy.join("python.zip"), &stage_copy, &cancel)?;
            extract_archive(
                &stage_copy.join("tzdata.whl"),
                &stage_copy.join("tz"),
                &cancel,
            )?;
            if cancel.load(Ordering::Relaxed) {
                bail!("Python sandbox installation cancelled.");
            }
            std::fs::remove_file(stage_copy.join("python.zip"))?;
            std::fs::remove_file(stage_copy.join("tzdata.whl"))?;
            std::fs::write(stage_copy.join("manifest.txt"), MANIFEST)?;
            Ok(())
        })
        .await
        .context("Python sandbox extraction worker failed")?;
        extraction?;
        publish_runtime(&stage, &self.runtime())?;
        Ok(())
    }
    pub(crate) async fn delete(&self) -> Result<SandboxStatus> {
        let _gate = self.inner.gate.try_lock().map_err(|_| anyhow!("Stop the running Python tool or wait for installation before deleting the sandbox."))?;
        let _lease = Lease::acquire(&self.inner.root)?;
        if self.runtime().exists() {
            std::fs::remove_dir_all(self.runtime())?;
        }
        let stage = self.inner.root.join("sandbox.staging");
        if stage.exists() {
            std::fs::remove_dir_all(stage)?;
        }
        *self
            .inner
            .compiled
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = None;
        *self.inner.state.lock().unwrap_or_else(|p| p.into_inner()) =
            ("not_installed".into(), None);
        *self.inner.installed_size.lock().unwrap_or_else(|p| p.into_inner()) = None;
        Ok(self.status())
    }
    async fn compiled(&self) -> Result<Arc<Compiled>> {
        if let Some(compiled) = self
            .inner
            .compiled
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            return Ok(compiled);
        }
        let path = self.runtime().join("python.wasm");
        let compiled = tokio::task::spawn_blocking(move || -> Result<Arc<Compiled>> {
            let start = Instant::now();
            let mut config = Config::new();
            config.async_support(true).epoch_interruption(true);
            let engine = Engine::new(&config)?;
            let module = Module::from_file(&engine, path)
                .context("Failed to compile the Python WASI runtime")?;
            eprintln!(
                "Python sandbox session compilation: {:.3}s",
                start.elapsed().as_secs_f64()
            );
            Ok(Arc::new(Compiled { engine, module }))
        })
        .await
        .context("Python compilation worker failed")??;
        *self
            .inner
            .compiled
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(compiled.clone());
        Ok(compiled)
    }
    pub(crate) async fn run(
        &self,
        code: String,
        timeout: Duration,
        cancel: Arc<AtomicBool>,
    ) -> Result<SandboxOutput> {
        let state = self.clone();
        // Keep the lease through first-use compilation even if the caller drops its
        // future. Stop's flag is checked before execution; compilation cannot be
        // interrupted, and must not be duplicated by a subsequent request.
        tokio::spawn(async move { state.run_locked(code, timeout, cancel).await })
            .await
            .context("Python execution worker failed")?
    }
    async fn run_locked(
        &self,
        code: String,
        timeout: Duration,
        cancel: Arc<AtomicBool>,
    ) -> Result<SandboxOutput> {
        if code.len() > 128 * 1024 {
            bail!("Python code exceeds the 128 KB limit.");
        }
        let _gate = self
            .inner
            .gate
            .try_lock()
            .map_err(|_| anyhow!("Another Python sandbox operation is still running."))?;
        let _lease = Lease::acquire(&self.inner.root)?;
        if cancel.load(Ordering::Relaxed) {
            return Ok(SandboxOutput {
                cancelled: true,
                ..Default::default()
            });
        }
        if !self.installed() {
            bail!("Install the Python sandbox in Models before running code.");
        }
        let compiled = self.compiled().await?;
        if cancel.load(Ordering::Relaxed) {
            return Ok(SandboxOutput {
                cancelled: true,
                ..Default::default()
            });
        }
        execute(&compiled, &self.runtime(), code, timeout, cancel).await
    }
}

struct InstallGuard {
    inner: Arc<Inner>,
    finished: bool,
}
impl Drop for InstallGuard {
    fn drop(&mut self) {
        if !self.finished {
            *self.inner.state.lock().unwrap_or_else(|p| p.into_inner()) = (
                "failed".into(),
                Some("Python sandbox installation interrupted. Retry to install.".into()),
            );
        }
    }
}
fn directory_size(path: &Path) -> u64 {
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| {
                    entry
                        .file_type()
                        .map(|kind| {
                            if kind.is_dir() {
                                directory_size(&entry.path())
                            } else if kind.is_file() {
                                entry.metadata().map(|meta| meta.len()).unwrap_or(0)
                            } else {
                                0
                            }
                        })
                        .unwrap_or(0)
                })
                .sum()
        })
        .unwrap_or(0)
}
fn extract_archive(archive: &Path, target: &Path, cancel: &AtomicBool) -> Result<()> {
    let mut zip = zip::ZipArchive::new(File::open(archive)?)?;
    let mut total = 0u64;
    if zip.len() > 5000 {
        bail!("Python archive has too many files.");
    }
    for index in 0..zip.len() {
        if cancel.load(Ordering::Relaxed) {
            bail!("Python sandbox installation cancelled.");
        }
        let mut entry = zip.by_index(index)?;
        let name = entry
            .enclosed_name()
            .ok_or_else(|| anyhow!("Unsafe path in Python archive"))?;
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            bail!("Links are not allowed in Python archives.");
        }
        total = total
            .checked_add(entry.size())
            .ok_or_else(|| anyhow!("Python archive is too large"))?;
        if total > 64 * 1024 * 1024 {
            bail!("Python archive exceeds its unpacked size limit.");
        }
        let path = target.join(name);
        if entry.is_dir() {
            std::fs::create_dir_all(path)?;
        } else {
            std::fs::create_dir_all(
                path.parent()
                    .ok_or_else(|| anyhow!("Invalid archive path"))?,
            )?;
            let mut file = File::create(path)?;
            std::io::copy(&mut entry, &mut file)?;
            file.flush()?;
        }
    }
    Ok(())
}

struct Guest {
    wasi: WasiP1Ctx,
    limits: StoreLimits,
    out_of_memory: Arc<AtomicBool>,
}
impl ResourceLimiter for Guest {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool> {
        if desired > MEMORY_BYTES {
            self.out_of_memory.store(true, Ordering::Relaxed);
        }
        self.limits.memory_growing(current, desired, maximum)
    }
    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool> {
        self.limits.table_growing(current, desired, maximum)
    }
    fn instances(&self) -> usize {
        1
    }
    fn tables(&self) -> usize {
        1
    }
    fn memories(&self) -> usize {
        1
    }
}

async fn execute(
    compiled: &Compiled,
    runtime: &Path,
    code: String,
    timeout: Duration,
    cancel: Arc<AtomicBool>,
) -> Result<SandboxOutput> {
    let stdout = MemoryOutputPipe::new(STDOUT_BYTES);
    let stderr = MemoryOutputPipe::new(STDERR_CAPTURE_BYTES);
    let mut wasi = WasiCtxBuilder::new();
    // Explicit CPython path configuration lets -I ignore all environment and user files.
    // Zoneinfo is configured through an app-owned prelude, never host TZ/PYTHONPATH.
    let code_literal = serde_json::to_string(&code)?;
    let program = format!("import sys\nsys.path.insert(0, '/tz')\nimport zoneinfo\nzoneinfo.reset_tzpath(['/tz/tzdata/zoneinfo'])\nexec(compile({code_literal}, '<assistant>', 'exec'))");
    wasi.args(&["/python.wasm", "-I", "-S", "-B", "-u", "-c", &program])
        .stdout(stdout.clone())
        .stderr(stderr.clone())
        .allow_ip_name_lookup(false)
        .allow_udp(false)
        .allow_tcp(false)
        .allow_blocking_current_thread(false)
        .preopened_dir(
            runtime.join("lib"),
            "/usr/local/lib",
            DirPerms::READ,
            FilePerms::READ,
        )?
        .preopened_dir(runtime.join("tz"), "/tz", DirPerms::READ, FilePerms::READ)?;
    let oom = Arc::new(AtomicBool::new(false));
    let mut wasi = wasi.build_p1();
    // Bound host descriptors, streams and other WASI resources as well as Wasm tables.
    wasi.ctx().table.set_max_capacity(MAX_WASI_RESOURCES);
    let guest = Guest {
        wasi,
        limits: StoreLimitsBuilder::new()
            .memory_size(MEMORY_BYTES)
            .table_elements(100_000)
            .instances(1)
            .tables(1)
            .memories(1)
            .trap_on_grow_failure(true)
            .build(),
        out_of_memory: oom.clone(),
    };
    let mut store = Store::new(&compiled.engine, guest);
    store.limiter(|guest| guest);
    store.epoch_deadline_async_yield_and_update(1);
    let mut linker = Linker::new(&compiled.engine);
    preview1::add_to_linker_async(&mut linker, |guest: &mut Guest| &mut guest.wasi)?;
    let start = Instant::now();
    let mut output = SandboxOutput::default();
    // Ticks only while this run executes, on its own thread so it advances even while the
    // runtime worker is busy in a CPU loop; dropped (and joined) when the run ends.
    let _ticker = EpochTicker::start(compiled.engine.clone())?;
    let result = {
        let execution = async {
            let instance = linker
                .instantiate_async(&mut store, &compiled.module)
                .await?;
            let start = instance.get_typed_func::<(), ()>(&mut store, "_start")?;
            start.call_async(&mut store, ()).await
        };
        tokio::pin!(execution);
        tokio::select! {
            biased;
            _ = wait_cancel(&cancel) => {output.cancelled = true; None},
            _ = tokio::time::sleep(timeout) => {output.timed_out = true; None},
            result = &mut execution => Some(result),
        }
        // Dropping this future cancels asynchronous WASI poll_oneoff/time.sleep too.
    };
    let stdout_bytes = stdout.contents();
    let stderr_bytes = stderr.contents();
    // A cap can split a UTF-8 character; drop the replacement character it leaves.
    output.stdout = String::from_utf8_lossy(&stdout_bytes).trim_end_matches('\u{FFFD}').to_string();
    let (stderr_text, stderr_cut) = tail_chars(&String::from_utf8_lossy(&stderr_bytes), STDERR_BYTES);
    output.stderr = stderr_text;
    output.out_of_memory = oom.load(Ordering::Relaxed);
    output.output_truncated = stdout_bytes.len() >= STDOUT_BYTES || stderr_cut || stderr_bytes.len() >= STDERR_CAPTURE_BYTES;
    if let Some(Err(error)) = result {
        let successful_exit = error
            .downcast_ref::<wasmtime_wasi::I32Exit>()
            .is_some_and(|exit| exit.0 == 0);
        if !successful_exit && !output.out_of_memory {
            let message = error.root_cause().to_string();
            if message.contains("MemoryOutputPipe") {
                output.output_truncated = true;
            }
            if output.stderr.is_empty() {
                output.stderr = tail_chars(&message, STDERR_BYTES).0;
            }
        }
    }
    eprintln!(
        "Python sandbox execution: {:.3}s; flags timeout={} cancelled={} oom={}",
        start.elapsed().as_secs_f64(),
        output.timed_out,
        output.cancelled,
        output.out_of_memory
    );
    Ok(output)
}
/// The last `max` characters of `text`, marked with a leading "…" when cut.
fn tail_chars(text: &str, max: usize) -> (String, bool) {
    let count = text.chars().count();
    if count <= max {
        return (text.to_string(), false);
    }
    let tail: String = text.chars().skip(count - (max - 1)).collect();
    (format!("…{tail}"), true)
}

/// Advances the engine's epoch every 10 ms while a run executes.
struct EpochTicker {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl EpochTicker {
    fn start(engine: Engine) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new().name("python-wasi-epoch".into()).spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(10));
                engine.increment_epoch();
            }
        })?;
        Ok(Self { stop, thread: Some(thread) })
    }
}
impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn wait_cancel(cancel: &AtomicBool) {
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn stderr_keeps_the_end_of_a_traceback() {
        let traceback = format!("Traceback (most recent call last):\n{}ZeroDivisionError: division by zero", "  File \"x\", line 1\n".repeat(200));
        let (tail, cut) = super::tail_chars(&traceback, super::STDERR_BYTES);
        assert!(cut);
        assert_eq!(tail.chars().count(), super::STDERR_BYTES);
        assert!(tail.starts_with('…') && tail.ends_with("ZeroDivisionError: division by zero"));
        assert_eq!(super::tail_chars("short", 10), ("short".to_string(), false));
    }
    #[test]
    fn the_epoch_ticker_runs_only_while_alive() {
        let engine = wasmtime::Engine::default();
        let ticker = super::EpochTicker::start(engine).unwrap();
        let thread = ticker.thread.as_ref().unwrap().thread().id();
        drop(ticker); // joins: the thread has exited when drop returns
        let _ = thread;
    }
    #[test]
    fn publishing_keeps_the_old_runtime_until_the_new_one_is_in_place() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("sandbox");
        let stage = root.path().join("sandbox.staging");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::write(runtime.join("version"), "old").unwrap();
        // A failed swap (here: nothing staged) restores the working install.
        assert!(super::publish_runtime(&stage, &runtime).is_err());
        assert_eq!(std::fs::read_to_string(runtime.join("version")).unwrap(), "old");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join("version"), "new").unwrap();
        super::publish_runtime(&stage, &runtime).unwrap();
        assert_eq!(std::fs::read_to_string(runtime.join("version")).unwrap(), "new");
        assert!(!stage.exists());
        assert!(!root.path().join("sandbox.previous").exists());
    }

    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn missing_runtime_status_and_cross_process_lease() {
        let root = tempfile::tempdir().unwrap();
        let state = SandboxState::new(root.path().into());
        let status = state.status();
        assert!(!status.installed);
        assert_eq!(status.state, "not_installed");
        assert_eq!(status.download_size_bytes, 14_639_013);
        let lease = Lease::acquire(root.path()).unwrap();
        assert!(Lease::acquire(root.path()).is_err());
        drop(lease);
        assert!(Lease::acquire(root.path()).is_ok());
    }

    #[tokio::test]
    async fn cancelled_before_start_never_compiles() {
        let root = tempfile::tempdir().unwrap();
        let state = SandboxState::new(root.path().into());
        let result = state
            .run(
                "print(1)".into(),
                Duration::from_secs(1),
                Arc::new(AtomicBool::new(true)),
            )
            .await
            .unwrap();
        assert!(result.cancelled);
        assert!(state.inner.compiled.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn a_cancelled_install_returns_to_not_installed_without_staging() {
        let root = tempfile::tempdir().unwrap();
        let state = SandboxState::new(root.path().into());
        assert!(!state.cancel_install(), "nothing is installing");
        // A user's cancel is not a failure: the row goes back to Download, with no error.
        let error = state.install(Arc::new(AtomicBool::new(true))).await.unwrap_err();
        assert!(error.to_string().contains("cancelled"), "{error:#}");
        assert!(!root.path().join("sandbox.staging").exists(), "no partial download is left");
        let status = state.status();
        assert_eq!(status.state, "not_installed");
        assert!(!status.installed);
        assert!(status.error.is_none());
        assert_eq!(state.delete().await.unwrap().state, "not_installed");
    }

    #[tokio::test]
    #[ignore = "downloads the pinned CPython and tzdata release assets (15 MB)"]
    async fn real_install_download_and_delete() {
        let root = tempfile::tempdir().unwrap();
        let state = SandboxState::new(root.path().into());
        let flag = Arc::new(AtomicBool::new(false));
        let installed = state.install(flag.clone()).await.unwrap();
        assert!(installed.installed);
        assert_eq!(installed.state, "ready");
        assert!(installed.installed_size_bytes > 40_000_000);
        assert!(state.install(flag).await.unwrap().installed);
        assert!(!root.path().join("sandbox.staging").exists());
        assert!(!state.delete().await.unwrap().installed);
    }

    fn fixture_runtime(state: &SandboxState) {
        // Alternatively, copy an already installed runtime (its manifest pins the same hashes).
        if let Ok(installed) = std::env::var("VOICEREADER_SANDBOX_INSTALLED_DIR") {
            copy_dir(Path::new(&installed), &state.runtime());
            assert!(state.status().installed, "{installed} is not a complete pinned runtime");
            return;
        }
        let python = std::env::var("VOICEREADER_SANDBOX_PYTHON_ARCHIVE")
            .expect("set VOICEREADER_SANDBOX_PYTHON_ARCHIVE to the pinned CPython ZIP");
        let tzdata = std::env::var("VOICEREADER_SANDBOX_TZDATA_ARCHIVE")
            .expect("set VOICEREADER_SANDBOX_TZDATA_ARCHIVE to the pinned tzdata wheel");
        for (path, hash) in [(&python, PY_HASH), (&tzdata, TZ_HASH)] {
            let bytes = std::fs::read(path).unwrap();
            assert_eq!(format!("{:x}", Sha256::digest(&bytes)), hash);
        }
        let cancel = AtomicBool::new(false);
        extract_archive(Path::new(&python), &state.runtime(), &cancel).unwrap();
        extract_archive(Path::new(&tzdata), &state.runtime().join("tz"), &cancel).unwrap();
        std::fs::write(state.runtime().join("manifest.txt"), MANIFEST).unwrap();
        assert!(state.status().installed);
    }

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() { copy_dir(&entry.path(), &target); } else { std::fs::copy(entry.path(), target).unwrap(); }
        }
    }

    /// No host Python is executed. Set the two archive paths described in the fixture
    /// README; the exact production Wasmtime/WASI implementation runs the real module.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires pinned CPython WASI ZIP and tzdata wheel; see tests-data/sandbox/README.md"]
    async fn real_runtime_isolation_limits_cancellation_and_lifecycle() {
        let root = tempfile::tempdir().unwrap();
        let state = SandboxState::new(root.path().into());
        fixture_runtime(&state);
        let cancel = || Arc::new(AtomicBool::new(false));
        let start = Instant::now();
        let first = state
            .run(
                include_str!("../tests-data/sandbox/stdlib_and_isolation.py").into(),
                Duration::from_secs(30),
                cancel(),
            )
            .await
            .unwrap();
        eprintln!(
            "First compile plus execution: {:.3}s; installed {} bytes",
            start.elapsed().as_secs_f64(),
            state.status().installed_size_bytes
        );
        assert!(first.stdout.contains("zone=8:00:00"), "{first:?}");
        assert!(first.stdout.contains("math=323"), "{first:?}");
        assert!(first.stdout.contains("isolation=ok"), "{first:?}");
        assert!(["network=no-socket-module", "network=no-sockets", "network=refused"].iter().any(|reason| first.stdout.contains(reason)), "{first:?}");
        eprintln!("Network isolation: {}", first.stdout.lines().find(|line| line.starts_with("network=")).unwrap_or("?"));
        assert!(first.stderr.is_empty(), "{first:?}");
        let installed_size = state.status().installed_size_bytes;
        let compiled = state.inner.compiled.lock().unwrap().clone().unwrap();
        let start = Instant::now();
        let warm = state
            .run("print('warm')".into(), Duration::from_secs(10), cancel())
            .await
            .unwrap();
        eprintln!(
            "Warm fresh interpreter: {:.3}s",
            start.elapsed().as_secs_f64()
        );
        assert_eq!(warm.stdout, "warm\n");
        assert!(Arc::ptr_eq(
            &compiled,
            &state.inner.compiled.lock().unwrap().clone().unwrap()
        ));
        let loops = state
            .run(
                "while True: pass".into(),
                Duration::from_millis(500),
                cancel(),
            )
            .await
            .unwrap();
        assert!(loops.timed_out, "{loops:?}");
        let start = Instant::now();
        let sleeping = state
            .run(
                "import time; time.sleep(999)".into(),
                Duration::from_millis(500),
                cancel(),
            )
            .await
            .unwrap();
        assert!(sleeping.timed_out, "{sleeping:?}");
        assert!(start.elapsed() < Duration::from_secs(3));
        for code in ["import time; time.sleep(999)", "while True: pass"] {
            let flag = cancel();
            let running_state = state.clone();
            let run_flag = flag.clone();
            let task = tokio::spawn(async move {
                running_state
                    .run(code.into(), Duration::from_secs(30), run_flag)
                    .await
            });
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(
                state.delete().await.is_err(),
                "delete must refuse the active lease"
            );
            flag.store(true, Ordering::Relaxed);
            let result = tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(result.cancelled, "{result:?}");
        }
        let memory = state
            .run(
                "blocks = []\nwhile True:\n    block = bytearray(16 * 1024 * 1024)\n    for offset in range(0, len(block), 4096): block[offset] = 1\n    blocks.append(block)".into(),
                Duration::from_secs(10),
                cancel(),
            )
            .await
            .unwrap();
        assert!(memory.out_of_memory, "{memory:?}");
        let output = state
            .run(
                "print('x' * 100_000)".into(),
                Duration::from_secs(10),
                cancel(),
            )
            .await
            .unwrap();
        assert!(
            output.stdout.len() <= STDOUT_BYTES && output.stderr.len() <= STDERR_BYTES,
            "{output:?}"
        );
        assert!(output.output_truncated, "{output:?}");
        let descriptors = state
            .run(
                include_str!("../tests-data/sandbox/descriptor_limit.py").into(),
                Duration::from_secs(10),
                cancel(),
            )
            .await
            .unwrap();
        assert!(
            descriptors.stderr.contains("resource table"),
            "{descriptors:?}"
        );
        // A new store/interpreter must recover after every trap and retain no variables.
        let fresh = state
            .run(
                "print('x' in globals())".into(),
                Duration::from_secs(10),
                cancel(),
            )
            .await
            .unwrap();
        assert_eq!(fresh.stdout, "False\n");
        assert_eq!(
            state.status().installed_size_bytes,
            installed_size,
            "guest runs must never write bytecode or native compiled caches"
        );
        let deleted = state.delete().await.unwrap();
        assert!(!deleted.installed);
        assert!(!state.runtime().exists());
        assert!(state.inner.compiled.lock().unwrap().is_none());
    }
}

/// Swaps a staged runtime into place without a moment where a failure leaves no runtime:
/// the old install is moved aside, the new one renamed in, and the old one restored if that
/// rename fails (for example, Windows antivirus briefly holding a staged file).
fn publish_runtime(stage: &Path, runtime: &Path) -> std::io::Result<()> {
    let previous = runtime.with_extension("previous");
    if previous.exists() {
        std::fs::remove_dir_all(&previous)?;
    }
    let had_previous = runtime.exists();
    if had_previous {
        std::fs::rename(runtime, &previous)?;
    }
    if let Err(error) = std::fs::rename(stage, runtime) {
        if had_previous {
            let _ = std::fs::rename(&previous, runtime);
        }
        return Err(error);
    }
    if had_previous {
        let _ = std::fs::remove_dir_all(&previous);
    }
    Ok(())
}
