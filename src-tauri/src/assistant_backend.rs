//! Managed llama.cpp `llama-server` for the Assistant (plan §3.1, §4, §5.1): installs the
//! pinned backend, downloads catalogue models, runs one server on 127.0.0.1 with an API key,
//! unloads it when idle, cleans up servers left behind by a crashed VoiceReader, and exposes
//! the `assistant_*` Tauri commands.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use rand::{distributions::Alphanumeric, Rng};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::assistant_catalogue::{
    backend_asset, model_by_id, models, CatalogueModel, MemoryEstimate, ModelTier, SamplingDefaults, BACKEND_VERSION,
};
use crate::model_download::{download_verified_file, DownloadCancelled};

/// Emitted with an [`AssistantStatus`] whenever install, download or server state changes.
pub(crate) const STATUS_EVENT: &str = "assistant:status";
/// Emitted with a [`DownloadProgress`] at most every 250 ms while a download runs.
pub(crate) const PROGRESS_EVENT: &str = "assistant:download-progress";

const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(300);
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(250);
const IDLE_UNLOAD_AFTER: Duration = Duration::from_secs(10 * 60);
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(30);
/// How long a stopped server gets to exit on its own before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(3);
const LOG_CAPACITY: usize = 500;
/// Startup lines kept apart from the rolling log, so the device report survives.
const LOG_HEAD_CAPACITY: usize = 400;
const LOG_TAIL_ON_ERROR: usize = 20;
const MIN_CTX_SIZE: u32 = 512;
/// The context the Models page quotes memory estimates for (the default setting).
const ESTIMATE_CTX_SIZE: u32 = 32_768;
/// Log lines shown in diagnostics.
const DIAGNOSTICS_LOG_LINES: usize = 300;
/// How long `--list-devices` may take before it is abandoned.
const LIST_DEVICES_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_CTX_SIZE: u32 = 1_048_576;
/// How long a load waits for another VoiceReader to finish its own admission step.
const LOAD_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an unload waits for the load lock before leaving the run record in place.
const RECORD_LOCK_WAIT: Duration = Duration::from_secs(5);
const LOAD_LOCK_BUSY_MESSAGE: &str = "Another VoiceReader window is starting the Assistant. Try again in a moment.";
const IN_USE_ELSEWHERE_MESSAGE: &str = "The Assistant is in use in another VoiceReader window.";

#[cfg(target_os = "windows")]
const SERVER_BINARY: &str = "llama-server.exe";
#[cfg(not(target_os = "windows"))]
const SERVER_BINARY: &str = "llama-server";

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

// ---------------------------------------------------------------------------------------
// Payloads

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AssistantStatus {
    pub platform_supported: bool,
    pub backend_installed: bool,
    pub backend_version: String,
    pub backend_download_bytes: u64,
    pub models: Vec<ModelStatus>,
    pub download: Option<DownloadInfo>,
    pub server: ServerStatus,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ModelStatus {
    pub id: String,
    pub label: String,
    pub tier: ModelTier,
    pub download_bytes: u64,
    pub installed: bool,
    /// The longest context the model was trained for; longer settings are refused.
    pub max_context: u32,
    /// Estimated memory at [`ESTIMATE_CTX_SIZE`], for display.
    pub memory_estimate: MemoryEstimatePayload,
    /// The coefficients behind it, so the page can estimate any context length.
    pub memory: MemoryEstimate,
    pub sampling_defaults: SamplingDefaults,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct MemoryEstimatePayload {
    pub ctx_size: u32,
    /// GPU (or unified) memory for weights, KV cache, compute buffers and the projector.
    pub gpu_mib: u32,
    /// System RAM llama.cpp keeps on the host side.
    pub host_mib: u32,
    /// False when the figure is calculated from the model's layout rather than measured.
    pub measured: bool,
}

/// One GPU llama.cpp can use, from `llama-server --list-devices`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct HardwareDevice {
    pub name: String,
    pub total_mib: u64,
    pub free_mib: u64,
    /// Integrated graphics share system RAM, so they are judged by RAM (plan §5.2).
    pub integrated: bool,
}

/// This computer's memory and GPUs, and the model tier recommended for it (plan §5.2).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AssistantHardware {
    pub total_ram_mib: u64,
    /// Apple Silicon: the GPU uses system RAM.
    pub unified_memory: bool,
    pub devices: Vec<HardwareDevice>,
    /// False when the backend is not installed (or its device check failed), so GPU memory
    /// is unknown and only RAM was considered.
    pub devices_known: bool,
    pub recommended_tier: ModelTier,
    pub reason: String,
}

/// What the Assistant diagnostics panel shows. Never contains the API key.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AssistantDiagnostics {
    pub backend_version: String,
    pub platform_asset: Option<String>,
    pub assistant_root: String,
    pub server_binary: Option<String>,
    pub server: ServerStatus,
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub uptime_secs: Option<u64>,
    /// The running server's log, or the last one's after it stopped (key already hidden).
    pub log: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DownloadKind {
    Backend,
    Model,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DownloadInfo {
    pub kind: DownloadKind,
    /// The model id, or the backend version for a backend install.
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DownloadProgress {
    pub kind: DownloadKind,
    pub id: String,
    pub file: String,
    /// Bytes across the whole item (both files of a model).
    pub downloaded: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ServerState {
    Stopped,
    Starting,
    Ready,
    InUseElsewhere,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ServerStatus {
    pub state: ServerState,
    pub model_id: Option<String>,
    pub ctx_size: Option<u32>,
    pub device_label: Option<String>,
    pub error: Option<String>,
}

/// Where the ready server listens, for the chat module.
#[derive(Clone)]
pub(crate) struct Endpoint {
    pub base_url: String,
    pub api_key: String,
    pub model_id: String,
    pub ctx_size: u32,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("model_id", &self.model_id)
            .field("ctx_size", &self.ctx_size)
            .finish()
    }
}

// ---------------------------------------------------------------------------------------
// On-disk layout

/// Paths under the Assistant root (`<app data dir>/assistant`).
#[derive(Debug, Clone)]
struct Layout {
    root: PathBuf,
}

impl Layout {
    fn backend_dir(&self) -> PathBuf {
        self.root.join("backend").join(BACKEND_VERSION)
    }

    fn staging_dir(&self) -> PathBuf {
        self.root.join("backend").join(format!(".staging-{BACKEND_VERSION}"))
    }

    fn model_dir(&self, model: &CatalogueModel) -> PathBuf {
        self.root.join("models").join(model.id).join(model.revision)
    }

    fn run_record_path(&self) -> PathBuf {
        self.root.join("run").join("server.json")
    }

    fn load_lock_path(&self) -> PathBuf {
        self.root.join("run").join("load.lock")
    }

    /// The installed server binary, if the backend is installed.
    fn server_binary(&self) -> Option<PathBuf> {
        find_server_binary(&self.backend_dir())
    }

    fn model_installed(&self, model: &CatalogueModel) -> bool {
        let dir = self.model_dir(model);
        model
            .files()
            .iter()
            .all(|file| std::fs::metadata(dir.join(file.file)).map(|meta| meta.len() == file.size).unwrap_or(false))
    }
}

/// Finds `llama-server` (`.exe` on Windows) anywhere under `dir`, without following
/// directory symlinks.
fn find_server_binary(dir: &Path) -> Option<PathBuf> {
    let mut pending = VecDeque::from([(dir.to_path_buf(), 0usize)]);
    while let Some((current, depth)) = pending.pop_front() {
        let Ok(entries) = std::fs::read_dir(&current) else { continue };
        let mut subdirs = Vec::new();
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else { continue };
            if file_type.is_dir() {
                subdirs.push(entry.path());
            } else if is_server_binary_name(&entry.file_name()) {
                return Some(entry.path());
            }
        }
        if depth < 4 {
            subdirs.sort();
            pending.extend(subdirs.into_iter().map(|path| (path, depth + 1)));
        }
    }
    None
}

fn is_server_binary_name(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    if cfg!(target_os = "windows") {
        name.eq_ignore_ascii_case(SERVER_BINARY)
    } else {
        name == SERVER_BINARY
    }
}

// ---------------------------------------------------------------------------------------
// Run record and orphan cleanup (plan §4.4)

/// `<root>/run/server.json`: written after spawning, removed when the server stops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RunRecord {
    server_pid: u32,
    exe_path: PathBuf,
    started_at_unix_ms: u64,
    owner_pid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordDecision {
    /// No record: nothing to clean up.
    NoRecord,
    /// Another live VoiceReader owns a running server: leave it and do not start one.
    InUseElsewhere,
    /// Our server outlived its owner: stop it, then remove the record.
    StopOrphan,
    /// The server is gone or the PID now belongs to another program: only remove the record.
    Discard,
}

/// Process queries the run-record decision needs, so it can be tested without processes.
trait ProcessProbe {
    fn is_alive(&self, pid: u32) -> bool;
    fn exe_path(&self, pid: u32) -> Option<PathBuf>;
}

fn decide_run_record(record: Option<&RunRecord>, own_pid: u32, probe: &dyn ProcessProbe) -> RecordDecision {
    let Some(record) = record else {
        return RecordDecision::NoRecord;
    };
    let server_is_ours = probe.is_alive(record.server_pid)
        && probe
            .exe_path(record.server_pid)
            .map(|path| same_executable(&path, &record.exe_path))
            .unwrap_or(false);
    if !server_is_ours {
        return RecordDecision::Discard;
    }
    if record.owner_pid != own_pid && probe.is_alive(record.owner_pid) {
        RecordDecision::InUseElsewhere
    } else {
        RecordDecision::StopOrphan
    }
}

fn same_executable(a: &Path, b: &Path) -> bool {
    #[cfg(target_os = "windows")]
    {
        fn normalize(path: &Path) -> String {
            let text = path.to_string_lossy().replace('/', "\\");
            text.strip_prefix(r"\\?\").unwrap_or(&text).to_lowercase()
        }
        return normalize(a) == normalize(b);
    }
    #[cfg(not(target_os = "windows"))]
    {
        return a == b
            || match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            };
    }
}

fn read_run_record(path: &Path) -> Option<RunRecord> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_run_record(path: &Path, record: &RunRecord) -> Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow!("Run record path has no parent"))?;
    std::fs::create_dir_all(parent).with_context(|| format!("Failed to create {}", parent.display()))?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(record)?)
        .with_context(|| format!("Failed to write {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| format!("Failed to write {}", path.display()))
}

/// Exclusive OS file lock on `<root>/run/load.lock`. A load holds it from reading the run
/// record until the new record is written, and every record removal holds it, so two
/// VoiceReader processes cannot both decide to start a server. It is released on drop, and
/// by the OS if the process dies.
struct LoadLock {
    _file: std::fs::File,
}

impl LoadLock {
    /// Tries once; `Ok(None)` when another holder has the lock.
    fn try_acquire(path: &Path) -> Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("Failed to open {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(err)) => Err(err).with_context(|| format!("Failed to lock {}", path.display())),
        }
    }

    /// Waits up to `wait` for the lock; `Ok(None)` if it stayed busy.
    fn acquire_blocking(path: &Path, wait: Duration) -> Result<Option<Self>> {
        let deadline = Instant::now() + wait;
        loop {
            if let Some(lock) = Self::try_acquire(path)? {
                return Ok(Some(lock));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits up to `wait` for the lock without blocking the async runtime.
    async fn acquire(path: &Path, wait: Duration) -> Result<Self> {
        let deadline = Instant::now() + wait;
        loop {
            if let Some(lock) = Self::try_acquire(path)? {
                return Ok(lock);
            }
            if Instant::now() >= deadline {
                return Err(anyhow!(LOAD_LOCK_BUSY_MESSAGE));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Removes the run record only if it names this process as owner and `server_pid` as its
/// server, so a late unload never deletes a newer server's record. Needs the load lock.
fn remove_own_run_record_locked(path: &Path, server_pid: u32, _lock: &LoadLock) {
    let ours = read_run_record(path)
        .map(|record| record.owner_pid == std::process::id() && record.server_pid == server_pid)
        .unwrap_or(false);
    if ours {
        let _ = std::fs::remove_file(path);
    }
}

/// [`remove_own_run_record_locked`], taking the load lock for up to `wait`. If the lock stays
/// busy the record is left; the next load discards it once the server is gone.
fn remove_own_run_record(layout: &Layout, server_pid: u32, wait: Duration) {
    if let Ok(Some(lock)) = LoadLock::acquire_blocking(&layout.load_lock_path(), wait) {
        remove_own_run_record_locked(&layout.run_record_path(), server_pid, &lock);
    }
}

/// Removes a stale record found by a load (orphan cleanup), only if it is still the record
/// that was read. Needs the load lock.
fn remove_run_record_if_unchanged(path: &Path, expected: &RunRecord, _lock: &LoadLock) {
    if read_run_record(path).as_ref() == Some(expected) {
        let _ = std::fs::remove_file(path);
    }
}

fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

struct SystemProbe;

#[cfg(target_os = "macos")]
impl ProcessProbe for SystemProbe {
    fn is_alive(&self, pid: u32) -> bool {
        let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
        if pid <= 0 {
            return false;
        }
        // SAFETY: signal 0 only checks that the process exists and may be signalled.
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    fn exe_path(&self, pid: u32) -> Option<PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        let pid = libc::c_int::try_from(pid).ok()?;
        let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: the buffer is valid for `buffer.len()` bytes for the duration of the call.
        let len = unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
        if len <= 0 {
            return None;
        }
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buffer[..len as usize])))
    }
}

#[cfg(target_os = "windows")]
impl ProcessProbe for SystemProbe {
    fn is_alive(&self, pid: u32) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        if pid == 0 {
            return false;
        }
        // SAFETY: plain Win32 calls; the handle is closed before returning.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            ok != 0 && code == STILL_ACTIVE as u32
        }
    }

    fn exe_path(&self, pid: u32) -> Option<PathBuf> {
        use std::os::windows::ffi::OsStringExt;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: the buffer outlives the call and `len` holds its capacity in u16 units.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return None;
            }
            let mut buffer = vec![0u16; 32_768];
            let mut len = buffer.len() as u32;
            let ok = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut len);
            CloseHandle(handle);
            if ok == 0 {
                return None;
            }
            Some(PathBuf::from(OsString::from_wide(&buffer[..len as usize])))
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl ProcessProbe for SystemProbe {
    fn is_alive(&self, pid: u32) -> bool {
        Path::new(&format!("/proc/{pid}")).exists()
    }

    fn exe_path(&self, pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok()
    }
}

/// Stops a process that is not our child (an orphaned server from an earlier run).
fn terminate_pid(pid: u32) {
    #[cfg(target_os = "macos")]
    {
        let Ok(raw) = libc::pid_t::try_from(pid) else { return };
        // SAFETY: plain signal calls on a PID the caller verified is our llama-server.
        unsafe { libc::kill(raw, libc::SIGTERM) };
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline && SystemProbe.is_alive(pid) {
            std::thread::sleep(Duration::from_millis(50));
        }
        if SystemProbe.is_alive(pid) {
            unsafe { libc::kill(raw, libc::SIGKILL) };
        }
    }
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        };
        // SAFETY: plain Win32 calls; the handle is closed before returning.
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid);
            if handle.is_null() {
                return;
            }
            TerminateProcess(handle, 1);
            WaitForSingleObject(handle, STOP_GRACE.as_millis() as u32);
            CloseHandle(handle);
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = pid;
    }
}

/// A Job Object that kills its processes when the last handle closes, so Windows ends
/// llama-server if VoiceReader dies.
#[cfg(target_os = "windows")]
struct KillOnCloseJob(windows_sys::Win32::Foundation::HANDLE);

// SAFETY: a job handle may be used and closed from any thread.
#[cfg(target_os = "windows")]
unsafe impl Send for KillOnCloseJob {}

#[cfg(target_os = "windows")]
impl KillOnCloseJob {
    fn assign(child: &Child) -> Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        // SAFETY: plain Win32 calls; `info` lives across the call that reads it and the job
        // handle is owned by the returned value (closed in Drop, or here on failure).
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(anyhow!("CreateJobObjectW failed: {}", std::io::Error::last_os_error()));
            }
            let job = KillOnCloseJob(handle);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                return Err(anyhow!("SetInformationJobObject failed: {}", std::io::Error::last_os_error()));
            }
            if AssignProcessToJobObject(job.0, child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE) == 0 {
                return Err(anyhow!("AssignProcessToJobObject failed: {}", std::io::Error::last_os_error()));
            }
            Ok(job)
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateJobObjectW and is closed exactly once.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

// ---------------------------------------------------------------------------------------
// Server process

/// The last [`LOG_CAPACITY`] lines of the server's stdout and stderr, plus the first
/// [`LOG_HEAD_CAPACITY`] lines for parsing the startup report.
#[derive(Debug, Default)]
struct LogBuffer {
    head: Vec<String>,
    lines: VecDeque<String>,
}

impl LogBuffer {
    fn push(&mut self, line: String) {
        if self.head.len() < LOG_HEAD_CAPACITY {
            self.head.push(line.clone());
        }
        if self.lines.len() == LOG_CAPACITY {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    fn tail(&self, count: usize) -> String {
        let skip = self.lines.len().saturating_sub(count);
        self.lines.iter().skip(skip).cloned().collect::<Vec<_>>().join("\n")
    }
}

fn lock_log(log: &Mutex<LogBuffer>) -> MutexGuard<'_, LogBuffer> {
    log.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Reads `source` line by line into `log` on its own thread, hiding the API key.
fn capture_lines<R: Read + Send + 'static>(source: R, log: Arc<Mutex<LogBuffer>>, api_key: String) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(source);
        let mut raw = Vec::new();
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&raw);
                    let line = line.trim_end_matches(['\r', '\n']).replace(&api_key, "<api-key>");
                    lock_log(&log).push(line);
                }
            }
        }
    });
}

/// The llama-server command line (plan §4.4, §5.3, §5.4). Offload is left to llama.cpp's
/// default `--fit`, so no `-ngl` or `--fit` is passed. `-lv 4` makes build b11718 print its
/// device and offload report (and per-request timings), which the default level hides; it
/// does not log prompt or answer text.
fn server_args(port: u16, api_key: &str, weights: &Path, mmproj: &Path, ctx_size: u32) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["--host", "127.0.0.1", "--port", &port.to_string(), "--api-key", api_key]
        .into_iter()
        .map(OsString::from)
        .collect();
    args.extend([
        OsString::from("-m"),
        weights.as_os_str().to_os_string(),
        OsString::from("--mmproj"),
        mmproj.as_os_str().to_os_string(),
    ]);
    args.extend(
        [
            "-c",
            &ctx_size.to_string(),
            "--parallel",
            "1",
            "--jinja",
            "--reasoning-format",
            "deepseek",
            "--no-reasoning-preserve",
            "-lv",
            "4",
        ]
        .into_iter()
        .map(OsString::from),
    );
    args
}

fn generate_api_key() -> String {
    rand::thread_rng().sample_iter(&Alphanumeric).take(48).map(char::from).collect()
}

struct ServerProcess {
    child: Child,
    started: Instant,
    port: u16,
    api_key: String,
    log: Arc<Mutex<LogBuffer>>,
    #[cfg(target_os = "windows")]
    _job: Option<KillOnCloseJob>,
}

impl ServerProcess {
    fn spawn(binary: &Path, args: &[OsString], port: u16, api_key: String) -> Result<Self> {
        let mut command = Command::new(binary);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = binary.parent() {
            command.current_dir(dir);
        }
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("Failed to start {}", binary.display()))?;
        let log = Arc::new(Mutex::new(LogBuffer::default()));
        if let Some(stdout) = child.stdout.take() {
            capture_lines(stdout, log.clone(), api_key.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            capture_lines(stderr, log.clone(), api_key.clone());
        }
        #[cfg(target_os = "windows")]
        let job = match KillOnCloseJob::assign(&child) {
            Ok(job) => Some(job),
            Err(err) => {
                // The server still works; it just is not cleaned up by Windows if the app dies.
                lock_log(&log).push(format!("[VoiceReader] Job Object not applied: {err:#}"));
                None
            }
        };
        Ok(Self {
            child,
            started: Instant::now(),
            port,
            api_key,
            log,
            #[cfg(target_os = "windows")]
            _job: job,
        })
    }

    fn log_tail(&self, count: usize) -> String {
        lock_log(&self.log).tail(count)
    }

    /// The startup lines followed by the recent ones (which may repeat some of them).
    fn log_lines(&self) -> Vec<String> {
        let log = lock_log(&self.log);
        log.head.iter().chain(log.lines.iter()).cloned().collect()
    }

    /// Asks the server to exit, then kills it after `grace`. On Windows there is no
    /// console to send Ctrl+C to, so it is terminated directly.
    fn stop(mut self, grace: Duration) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        #[cfg(target_os = "macos")]
        {
            if let Ok(pid) = libc::pid_t::try_from(self.child.id()) {
                // SAFETY: the PID is our own unreaped child, so it cannot have been reused.
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
            let deadline = Instant::now() + grace;
            while Instant::now() < deadline {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = grace;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn wait_for_health(http: &Client, port: u16, api_key: &str) -> bool {
    let url = format!("http://127.0.0.1:{port}/health");
    match http.get(url).bearer_auth(api_key).send().await {
        Ok(response) => response.status() == reqwest::StatusCode::OK,
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------------------
// Device label

/// A short label of where the model runs, from the server's startup log: for example
/// "Vulkan: NVIDIA GeForce RTX 5090", "Metal: Apple M2 Max" or "CPU". Partial offload adds
/// the layer count. Unrecognised logs give "Unknown device".
fn parse_device_label<'a>(lines: impl IntoIterator<Item = &'a str>) -> String {
    let mut used: Vec<String> = Vec::new();
    let mut offloaded: Option<(u32, u32)> = None;
    let mut metal_gpu: Option<String> = None;
    let mut vulkan_gpu: Option<String> = None;
    let mut cpu_buffers = false;

    for line in lines {
        if let Some(rest) = text_after(line, "using device ") {
            if let Some(label) = label_from_using_device(rest) {
                if !used.contains(&label) {
                    used.push(label);
                }
            }
        } else if let Some(rest) = text_after(line, "offloaded ") {
            offloaded = parse_offloaded(rest).or(offloaded);
        } else if line.contains("ggml_metal") && line.contains("GPU name:") {
            // `GPU name:   MTL0 (Apple M2 Max)` in b11718, `GPU name:   Apple M1` in older builds.
            if let Some(name) = text_after(line, "GPU name:").map(str::trim).filter(|name| !name.is_empty()) {
                let name = first_parenthesised(name).map(str::trim).filter(|inner| !inner.is_empty()).unwrap_or(name);
                metal_gpu.get_or_insert_with(|| name.to_string());
            }
        } else if let Some(rest) = text_after(line, "ggml_vulkan: ") {
            if vulkan_gpu.is_none() {
                vulkan_gpu = parse_vulkan_device(rest);
            }
        }
        if line.contains("CPU model buffer size") || line.contains("CPU_Mapped model buffer size") {
            cpu_buffers = true;
        }
    }

    if matches!(offloaded, Some((0, _))) {
        return "CPU".to_string();
    }
    let partial = match offloaded {
        Some((done, total)) if done < total => format!(" ({done}/{total} layers)"),
        _ => String::new(),
    };
    if !used.is_empty() {
        return format!("{}{partial}", used.join(", "));
    }
    let offloads = offloaded.map(|(done, _)| done > 0).unwrap_or(false);
    if let Some(name) = metal_gpu.filter(|_| offloads) {
        return format!("Metal: {name}{partial}");
    }
    if let Some(name) = vulkan_gpu.filter(|_| offloads) {
        return format!("Vulkan: {name}{partial}");
    }
    if cpu_buffers {
        return "CPU".to_string();
    }
    "Unknown device".to_string()
}

fn text_after<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    line.find(marker).map(|index| &line[index + marker.len()..])
}

/// `Vulkan0 (NVIDIA GeForce RTX 5090) (0000:01:00.0) - 31000 MiB free` -> `Vulkan: NVIDIA ...`.
fn label_from_using_device(rest: &str) -> Option<String> {
    let name = rest.split_whitespace().next()?;
    let backend = name.trim_end_matches(|c: char| c.is_ascii_digit());
    let backend = match backend {
        "" => return None,
        "MTL" => "Metal",
        other => other,
    };
    let description = first_parenthesised(rest).map(str::trim).filter(|description| !description.is_empty());
    Some(match description {
        Some(description) => format!("{backend}: {description}"),
        None => backend.to_string(),
    })
}

/// The text inside the first balanced pair of parentheses, so `(Intel(R) Arc(TM)) (x)`
/// gives `Intel(R) Arc(TM)`.
fn first_parenthesised(text: &str) -> Option<&str> {
    let start = text.find('(')?;
    let mut depth = 0usize;
    for (offset, c) in text[start..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start + 1..start + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

/// `35/35 layers to GPU` -> `(35, 35)`.
fn parse_offloaded(rest: &str) -> Option<(u32, u32)> {
    let fraction = rest.split_whitespace().next()?;
    let (done, total) = fraction.split_once('/')?;
    Some((done.parse().ok()?, total.parse().ok()?))
}

/// `0 = NVIDIA GeForce RTX 5090 (NVIDIA) | uma: 0 | ...` -> `NVIDIA GeForce RTX 5090`.
fn parse_vulkan_device(rest: &str) -> Option<String> {
    let (index, description) = rest.split_once(" = ")?;
    index.trim().parse::<u32>().ok()?;
    let description = description.split('|').next()?.trim();
    let name = match description.rfind(" (") {
        Some(cut) if description.ends_with(')') => &description[..cut],
        _ => description,
    };
    Some(name.trim().to_string()).filter(|name| !name.is_empty())
}

// ---------------------------------------------------------------------------------------
// Manager state

struct ServerSlot {
    state: ServerState,
    model_id: Option<String>,
    ctx_size: Option<u32>,
    device_label: Option<String>,
    error: Option<String>,
    process: Option<ServerProcess>,
    /// Bumped by every unload, so a load in progress notices it was cancelled.
    generation: u64,
    in_use: usize,
    last_activity: Instant,
}

impl ServerSlot {
    fn new() -> Self {
        Self {
            state: ServerState::Stopped,
            model_id: None,
            ctx_size: None,
            device_label: None,
            error: None,
            process: None,
            generation: 0,
            in_use: 0,
            last_activity: Instant::now(),
        }
    }

    fn set_stopped(&mut self) {
        self.state = ServerState::Stopped;
        self.model_id = None;
        self.ctx_size = None;
        self.device_label = None;
        self.error = None;
    }

    fn set_error(&mut self, message: String) {
        self.state = ServerState::Error;
        self.device_label = None;
        self.error = Some(message);
    }
}

struct Inner {
    layout: Layout,
    slot: Mutex<ServerSlot>,
    /// Serialises loads so two requests never start two servers.
    load_lock: futures_util::lock::Mutex<()>,
    /// Serialises turn-detection model checks, so a second caller waits for a download in
    /// progress instead of finding the download slot busy.
    turn_models_lock: futures_util::lock::Mutex<()>,
    download: Mutex<Option<DownloadInfo>>,
    cancel_download: AtomicBool,
    idle_watch_started: AtomicBool,
    http: Client,
    downloads: Client,
    /// The most recent server's log, kept after it stops so diagnostics can show why.
    last_log: Mutex<Option<Arc<Mutex<LogBuffer>>>>,
    /// Servers taken out of the slot whose stop has not finished yet. A delete waits for
    /// zero, so it never removes files a stopping server still has open.
    stopping: AtomicUsize,
    /// The last successful hardware check; GPUs do not change while the app runs.
    hardware: Mutex<Option<AssistantHardware>>,
}

impl Inner {
    fn slot(&self) -> MutexGuard<'_, ServerSlot> {
        self.slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn download_slot(&self) -> MutexGuard<'_, Option<DownloadInfo>> {
        self.download.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn last_log(&self) -> MutexGuard<'_, Option<Arc<Mutex<LogBuffer>>>> {
        self.last_log.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Notices a server that exited on its own and records why. Returns its PID so the
    /// caller can call [`Inner::forget_exited`] once the slot is unlocked.
    /// A server still starting is left to `load`, which reports its exit itself.
    fn reap_exited(&self, slot: &mut ServerSlot) -> Option<u32> {
        if slot.state == ServerState::Starting {
            return None;
        }
        let process = slot.process.as_mut()?;
        let Ok(Some(status)) = process.child.try_wait() else { return None };
        let pid = process.child.id();
        let tail = process.log_tail(LOG_TAIL_ON_ERROR);
        slot.process = None;
        if slot.state == ServerState::Ready {
            slot.set_error(format!("llama-server stopped unexpectedly ({status}). Last log lines:\n{tail}"));
        }
        Some(pid)
    }

    /// Removes the record of a server [`Inner::reap_exited`] found dead, without waiting for
    /// the load lock. Never call it with the slot locked: the load lock is taken first by `load`.
    fn forget_exited(&self, reaped: Option<u32>) {
        if let Some(pid) = reaped {
            remove_own_run_record(&self.layout, pid, Duration::ZERO);
        }
    }

    /// Stops a server taken out of the slot and removes its run record if it is still ours.
    fn stop_and_forget(&self, process: Option<ServerProcess>, grace: Duration, lock_wait: Duration) {
        if let Some(process) = process {
            let pid = process.child.id();
            process.stop(grace);
            self.stopping.fetch_sub(1, Ordering::SeqCst);
            remove_own_run_record(&self.layout, pid, lock_wait);
        }
    }

    /// Takes the server out of the slot, marks it stopped and returns it for stopping.
    /// Every caller passes the result to [`Inner::stop_and_forget`].
    fn take_server(&self, slot: &mut ServerSlot) -> Option<ServerProcess> {
        slot.generation += 1;
        slot.set_stopped();
        let process = slot.process.take();
        if process.is_some() {
            self.stopping.fetch_add(1, Ordering::SeqCst);
        }
        process
    }

    /// Stops the server (blocking for up to [`STOP_GRACE`]) and removes its run record.
    fn unload_blocking(&self) {
        let process = {
            let mut slot = self.slot();
            self.take_server(&mut slot)
        };
        self.stop_and_forget(process, STOP_GRACE, RECORD_LOCK_WAIT);
    }

    /// Unloads only if the server is ready, unused and idle; returns whether it did.
    fn unload_if_idle(&self) -> bool {
        let (reaped, expired, process) = {
            let mut slot = self.slot();
            let reaped = self.reap_exited(&mut slot);
            let expired = idle_expired(slot.state, slot.in_use, slot.last_activity, Instant::now());
            let process = if expired { self.take_server(&mut slot) } else { None };
            (reaped, expired, process)
        };
        self.forget_exited(reaped);
        self.stop_and_forget(process, STOP_GRACE, RECORD_LOCK_WAIT);
        expired
    }
}

fn idle_expired(state: ServerState, in_use: usize, last_activity: Instant, now: Instant) -> bool {
    state == ServerState::Ready && in_use == 0 && now.saturating_duration_since(last_activity) >= IDLE_UNLOAD_AFTER
}

/// Shared Assistant state, registered with Tauri via `.manage(AssistantState::new(root))`.
pub(crate) struct AssistantState {
    inner: Arc<Inner>,
}

/// While any guard is alive the server counts as in use and is never idle-unloaded.
/// Dropping it records activity.
pub(crate) struct UseGuard {
    inner: Arc<Inner>,
}

impl Drop for UseGuard {
    fn drop(&mut self) {
        let mut slot = self.inner.slot();
        slot.in_use = slot.in_use.saturating_sub(1);
        slot.last_activity = Instant::now();
    }
}

/// Clears the active download when the download ends, however it ends.
struct DownloadGuard<'a> {
    inner: &'a Inner,
}

impl Drop for DownloadGuard<'_> {
    fn drop(&mut self) {
        *self.inner.download_slot() = None;
    }
}

impl AssistantState {
    pub(crate) fn new(root: PathBuf) -> Self {
        let user_agent = concat!("VoiceReader/", env!("CARGO_PKG_VERSION"));
        // Loopback calls carry the API key, so they must never go through a proxy.
        let http = Client::builder()
            .user_agent(user_agent)
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap_or_else(|_| Client::builder().no_proxy().build().unwrap_or_default());
        let downloads = Client::builder()
            .user_agent(user_agent)
            .connect_timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            inner: Arc::new(Inner {
                layout: Layout { root },
                slot: Mutex::new(ServerSlot::new()),
                load_lock: futures_util::lock::Mutex::new(()),
                turn_models_lock: futures_util::lock::Mutex::new(()),
                download: Mutex::new(None),
                cancel_download: AtomicBool::new(false),
                idle_watch_started: AtomicBool::new(false),
                http,
                downloads,
                last_log: Mutex::new(None),
                stopping: AtomicUsize::new(0),
                hardware: Mutex::new(None),
            }),
        }
    }

    /// Deletes a downloaded model's files (every revision of it). A model loaded here is
    /// unloaded first. Refused while any Assistant download runs, while the model is
    /// answering a request, and while another VoiceReader window runs the Assistant.
    async fn delete_model(&self, model_id: &str) -> Result<()> {
        let inner = &self.inner;
        let model = model_by_id(model_id).ok_or_else(|| anyhow!("Unknown Assistant model: {model_id}"))?;
        // Holding the download slot keeps a download from starting while files are removed.
        let _download = self
            .begin_download(DownloadKind::Model, model.id)
            .map_err(|_| anyhow!("Wait for the current Assistant download to finish, or cancel it, then delete."))?;
        // Holding the load lock keeps a load in this window from starting, or finishing,
        // mid-delete; the cross-process lock does the same for other VoiceReader windows.
        let _load = inner.load_lock.lock().await;
        let _admission = LoadLock::acquire(&inner.layout.load_lock_path(), LOAD_LOCK_TIMEOUT).await?;
        if self.status().server.state == ServerState::InUseElsewhere {
            return Err(anyhow!("The Assistant is in use in another VoiceReader window. Close that window first."));
        }
        // The answering check and taking the server happen under one slot lock, so a chat
        // cannot be admitted in between and then lose its server.
        let taken = {
            let mut slot = inner.slot();
            let loaded = slot.model_id.as_deref() == Some(model.id) && slot.process.is_some();
            if loaded && slot.in_use > 0 {
                return Err(anyhow!("{} is answering. Stop the answer, then delete.", model.label));
            }
            if loaded { inner.take_server(&mut slot) } else { None }
        };
        if let Some(process) = taken {
            // The run record is removed with the lock already held here (taking it again
            // through another handle would wait for ourselves).
            let pid = process.child.id();
            let stopper = inner.clone();
            tokio::task::spawn_blocking(move || {
                process.stop(STOP_GRACE);
                stopper.stopping.fetch_sub(1, Ordering::SeqCst);
            })
            .await
            .map_err(|err| anyhow!("Unloading {} failed: {err}", model.label))?;
            remove_own_run_record_locked(&inner.layout.run_record_path(), pid, &_admission);
        }
        // An unload that started before this delete may still be stopping its server.
        let deadline = Instant::now() + Duration::from_secs(10);
        while inner.stopping.load(Ordering::SeqCst) > 0 {
            if Instant::now() >= deadline {
                return Err(anyhow!("The Assistant server is still stopping. Try deleting again in a moment."));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let root = &inner.layout.root;
        let models_dir = root.join("models");
        let dir = models_dir.join(model.id);
        ensure_no_symlinks(&[root.as_path(), models_dir.as_path(), dir.as_path()])?;
        if dir.exists() {
            let target = dir.clone();
            tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&target))
                .await
                .map_err(|err| anyhow!("Deleting {} failed: {err}", model.label))?
                .with_context(|| format!("Failed to delete {}", dir.display()))?;
        }
        Ok(())
    }

    /// The turn-detection model files (Smart Turn, Silero VAD), downloading and verifying
    /// whichever is missing (about 11 MB). Shares the download slot with other downloads.
    pub(crate) async fn turn_detection_models(&self) -> Result<(PathBuf, PathBuf)> {
        use crate::assistant_catalogue::{SILERO_VAD_MODEL, SMART_TURN_MODEL};
        let dir = self.inner.layout.root.join("turn");
        let paths = (dir.join(SMART_TURN_MODEL.file), dir.join(SILERO_VAD_MODEL.file));
        let installed = |path: &Path, size: u64| std::fs::metadata(path).map(|meta| meta.len() == size).unwrap_or(false);
        let _serial = self.inner.turn_models_lock.lock().await;
        if installed(&paths.0, SMART_TURN_MODEL.size) && installed(&paths.1, SILERO_VAD_MODEL.size) {
            return Ok(paths);
        }
        let _download = self
            .begin_download(DownloadKind::Model, "turn-detection")
            .map_err(|_| anyhow!("Wait for the current Assistant download to finish, then turn on automatic sending."))?;
        ensure_no_symlinks(&[self.inner.layout.root.as_path(), dir.as_path()])?;
        std::fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
        for (model, path) in [(SMART_TURN_MODEL, &paths.0), (SILERO_VAD_MODEL, &paths.1)] {
            download_verified_file(&self.inner.downloads, model.url, model.size, model.sha256, path, &self.inner.cancel_download, |_| {})
                .await
                .with_context(|| format!("Downloading the turn-detection model {} failed", model.file))?;
        }
        Ok(paths)
    }

    /// Backend paths, the server's process details and its log, for the diagnostics panel.
    pub(crate) fn diagnostics(&self) -> AssistantDiagnostics {
        let status = self.status();
        let layout = &self.inner.layout;
        let (pid, port, uptime_secs, live_log) = {
            let slot = self.inner.slot();
            match slot.process.as_ref() {
                Some(process) => (
                    Some(process.child.id()),
                    Some(process.port),
                    Some(process.started.elapsed().as_secs()),
                    Some(process.log_tail(DIAGNOSTICS_LOG_LINES)),
                ),
                None => (None, None, None, None),
            }
        };
        let log = live_log
            .or_else(|| self.inner.last_log().as_ref().map(|log| lock_log(log).tail(DIAGNOSTICS_LOG_LINES)))
            .unwrap_or_default();
        AssistantDiagnostics {
            backend_version: BACKEND_VERSION.to_string(),
            platform_asset: backend_asset().map(|asset| asset.file.to_string()),
            assistant_root: layout.root.display().to_string(),
            server_binary: layout.server_binary().map(|path| path.display().to_string()),
            server: status.server,
            pid,
            port,
            uptime_secs,
            log,
        }
    }

    pub(crate) fn status(&self) -> AssistantStatus {
        let layout = &self.inner.layout;
        let asset = backend_asset();
        let (reaped, mut server) = {
            let mut slot = self.inner.slot();
            let reaped = self.inner.reap_exited(&mut slot);
            (reaped, ServerStatus {
                state: slot.state,
                model_id: slot.model_id.clone(),
                ctx_size: slot.ctx_size,
                device_label: slot.device_label.clone(),
                error: slot.error.clone(),
            })
        };
        self.inner.forget_exited(reaped);
        if matches!(server.state, ServerState::Stopped | ServerState::InUseElsewhere) {
            let record = read_run_record(&layout.run_record_path());
            if decide_run_record(record.as_ref(), std::process::id(), &SystemProbe) == RecordDecision::InUseElsewhere {
                server.state = ServerState::InUseElsewhere;
                server.error = Some(IN_USE_ELSEWHERE_MESSAGE.to_string());
            } else {
                server.state = ServerState::Stopped;
                server.error = None;
            }
        }
        AssistantStatus {
            platform_supported: asset.is_some(),
            backend_installed: asset.is_some() && layout.server_binary().is_some(),
            backend_version: BACKEND_VERSION.to_string(),
            backend_download_bytes: asset.map(|asset| asset.size).unwrap_or(0),
            models: models()
                .iter()
                .map(|model| ModelStatus {
                    id: model.id.to_string(),
                    label: model.label.to_string(),
                    tier: model.tier,
                    download_bytes: model.download_bytes(),
                    installed: layout.model_installed(model),
                    max_context: model.max_context,
                    memory_estimate: {
                        let (gpu_mib, host_mib) = model.memory.at(ESTIMATE_CTX_SIZE);
                        MemoryEstimatePayload { ctx_size: ESTIMATE_CTX_SIZE, gpu_mib, host_mib, measured: model.memory.measured }
                    },
                    memory: model.memory,
                    sampling_defaults: model.sampling,
                })
                .collect(),
            download: self.inner.download_slot().clone(),
            server,
        }
    }

    /// The ready server's address and key; `None` unless the state is `ready`.
    pub(crate) fn endpoint(&self) -> Option<Endpoint> {
        let (reaped, endpoint) = {
            let mut slot = self.inner.slot();
            let reaped = self.inner.reap_exited(&mut slot);
            let endpoint = match (slot.state, slot.process.as_ref(), slot.model_id.clone(), slot.ctx_size) {
                (ServerState::Ready, Some(process), Some(model_id), Some(ctx_size)) => Some(Endpoint {
                    base_url: format!("http://127.0.0.1:{}", process.port),
                    api_key: process.api_key.clone(),
                    model_id,
                    ctx_size,
                }),
                _ => None,
            };
            (reaped, endpoint)
        };
        self.inner.forget_exited(reaped);
        endpoint
    }

    /// Marks the server in use until the guard drops (for the length of a request).
    pub(crate) fn begin_use(&self) -> UseGuard {
        let mut slot = self.inner.slot();
        slot.in_use += 1;
        slot.last_activity = Instant::now();
        UseGuard { inner: self.inner.clone() }
    }

    /// Stops the server synchronously and removes its run record. For app exit. With no
    /// server running, the record (possibly another instance's) is left alone.
    pub(crate) fn shutdown(&self) {
        self.inner.cancel_download.store(true, Ordering::SeqCst);
        let process = {
            let mut slot = self.inner.slot();
            self.inner.take_server(&mut slot)
        };
        self.inner.stop_and_forget(process, Duration::from_secs(2), Duration::from_secs(1));
    }

    fn begin_download(&self, kind: DownloadKind, id: &str) -> Result<DownloadGuard<'_>> {
        let mut download = self.inner.download_slot();
        if download.is_some() {
            return Err(anyhow!("Another Assistant download is already running."));
        }
        *download = Some(DownloadInfo { kind, id: id.to_string() });
        self.inner.cancel_download.store(false, Ordering::SeqCst);
        Ok(DownloadGuard { inner: &self.inner })
    }

    fn cancel_download(&self) {
        self.inner.cancel_download.store(true, Ordering::SeqCst);
    }

    /// Downloads, verifies and extracts the pinned backend (plan §4.3).
    async fn install_backend(&self, on_change: &(dyn Fn() + Send + Sync), on_progress: &(dyn Fn(DownloadProgress) + Send + Sync)) -> Result<()> {
        let asset = backend_asset().ok_or_else(|| anyhow!("The Assistant is not available on this platform."))?;
        let layout = &self.inner.layout;
        if layout.server_binary().is_some() {
            return Ok(());
        }
        let _download = self.begin_download(DownloadKind::Backend, BACKEND_VERSION)?;
        on_change();

        let staging = layout.staging_dir();
        ensure_no_symlinks(&[&layout.root, &layout.root.join("backend"), &staging])?;
        std::fs::create_dir_all(&staging).with_context(|| format!("Failed to create {}", staging.display()))?;
        let archive = staging.join(asset.file);
        download_verified_file(
            &self.inner.downloads,
            &asset.url(),
            asset.size,
            asset.sha256,
            &archive,
            &self.inner.cancel_download,
            |downloaded| {
                on_progress(DownloadProgress {
                    kind: DownloadKind::Backend,
                    id: BACKEND_VERSION.to_string(),
                    file: asset.file.to_string(),
                    downloaded,
                    total: asset.size,
                })
            },
        )
        .await?;

        let (archive_path, staging_dir, final_dir) = (archive.clone(), staging.clone(), layout.backend_dir());
        tokio::task::spawn_blocking(move || extract_and_install(&archive_path, &staging_dir, &final_dir))
            .await
            .map_err(|err| anyhow!("Backend installation failed: {err}"))?
    }

    /// Downloads both files of a catalogue model into its revision directory.
    async fn download_model(
        &self,
        model_id: &str,
        on_change: &(dyn Fn() + Send + Sync),
        on_progress: &(dyn Fn(DownloadProgress) + Send + Sync),
    ) -> Result<()> {
        let model = model_by_id(model_id).ok_or_else(|| anyhow!("Unknown Assistant model: {model_id}"))?;
        let _download = self.begin_download(DownloadKind::Model, model.id)?;
        on_change();
        let dir = self.inner.layout.model_dir(model);
        let total = model.download_bytes();
        let mut offset = 0u64;
        for file in model.files() {
            download_verified_file(
                &self.inner.downloads,
                &model.file_url(file),
                file.size,
                file.sha256,
                &dir.join(file.file),
                &self.inner.cancel_download,
                |downloaded| {
                    on_progress(DownloadProgress {
                        kind: DownloadKind::Model,
                        id: model.id.to_string(),
                        file: file.file.to_string(),
                        downloaded: offset + downloaded,
                        total,
                    })
                },
            )
            .await?;
            offset += file.size;
        }
        Ok(())
    }

    /// Starts llama-server for `model_id` and waits until `/health` reports ready.
    /// `on_change` runs after each state change.
    async fn load(&self, model_id: &str, ctx_size: u32, on_change: &(dyn Fn() + Send + Sync)) -> Result<()> {
        let inner = &self.inner;
        let layout = &inner.layout;
        let model = model_by_id(model_id).ok_or_else(|| anyhow!("Unknown Assistant model: {model_id}"))?;
        if !(MIN_CTX_SIZE..=MAX_CTX_SIZE).contains(&ctx_size) {
            return Err(anyhow!("Context size must be between {MIN_CTX_SIZE} and {MAX_CTX_SIZE} tokens."));
        }
        if ctx_size > model.max_context {
            return Err(anyhow!(
                "{} supports up to {}K tokens of context. Choose a smaller context length.",
                model.label,
                model.max_context / 1024
            ));
        }
        if backend_asset().is_none() {
            return Err(anyhow!("The Assistant is not available on this platform."));
        }
        let binary = layout
            .server_binary()
            .ok_or_else(|| anyhow!("The llama.cpp backend is not installed."))?;
        if !layout.model_installed(model) {
            return Err(anyhow!("{} is not downloaded.", model.label));
        }

        let _load = inner.load_lock.lock().await;
        // A delete may have finished while this load waited for the lock.
        if !layout.model_installed(model) {
            return Err(anyhow!("{} is not downloaded.", model.label));
        }
        let generation = {
            let mut slot = inner.slot();
            inner.reap_exited(&mut slot);
            if slot.state == ServerState::Ready
                && slot.process.is_some()
                && slot.model_id.as_deref() == Some(model.id)
                && slot.ctx_size == Some(ctx_size)
            {
                slot.last_activity = Instant::now();
                return Ok(());
            }
            slot.generation
        };

        // Admission, serialised across VoiceReader processes: held until the new run
        // record is written, but not during the /health wait.
        let admission = LoadLock::acquire(&layout.load_lock_path(), LOAD_LOCK_TIMEOUT).await?;
        let record_path = layout.run_record_path();

        // Stop our current server (another model or context) before anything else.
        let current = {
            let mut slot = inner.slot();
            slot.set_stopped();
            slot.process.take()
        };
        if let Some(process) = current {
            let pid = process.child.id();
            tokio::task::spawn_blocking(move || process.stop(STOP_GRACE)).await.ok();
            remove_own_run_record_locked(&record_path, pid, &admission);
        }

        if self.admit(&admission, &SystemProbe).await == RecordDecision::InUseElsewhere {
            {
                let mut slot = inner.slot();
                slot.set_stopped();
                slot.state = ServerState::InUseElsewhere;
                slot.error = Some(IN_USE_ELSEWHERE_MESSAGE.to_string());
            }
            on_change();
            return Err(anyhow!(IN_USE_ELSEWHERE_MESSAGE));
        }

        let port = portpicker::pick_unused_port().ok_or_else(|| anyhow!("Failed to find a free localhost port"))?;
        let api_key = generate_api_key();
        let weights = layout.model_dir(model).join(model.weights.file);
        let mmproj = layout.model_dir(model).join(model.mmproj.file);
        let args = server_args(port, &api_key, &weights, &mmproj, ctx_size);
        let started = {
            let mut slot = inner.slot();
            if slot.generation != generation {
                return Err(anyhow!("Loading was cancelled."));
            }
            let started = ServerProcess::spawn(&binary, &args, port, api_key.clone()).and_then(|process| {
                let record = RunRecord {
                    server_pid: process.child.id(),
                    exe_path: std::fs::canonicalize(&binary).unwrap_or_else(|_| binary.clone()),
                    started_at_unix_ms: unix_millis_now(),
                    owner_pid: std::process::id(),
                };
                match write_run_record(&record_path, &record) {
                    Ok(()) => Ok(process),
                    Err(err) => {
                        process.stop(Duration::ZERO);
                        Err(err)
                    }
                }
            });
            match started {
                Ok(process) => {
                    *inner.last_log() = Some(process.log.clone());
                    slot.generation += 1;
                    slot.process = Some(process);
                    slot.state = ServerState::Starting;
                    slot.model_id = Some(model.id.to_string());
                    slot.ctx_size = Some(ctx_size);
                    slot.device_label = None;
                    slot.error = None;
                    Ok(slot.generation)
                }
                Err(err) => {
                    slot.set_error(format!("{err:#}"));
                    Err(err)
                }
            }
        };
        drop(admission);
        on_change();
        let generation = started?;

        let deadline = Instant::now() + SERVER_READY_TIMEOUT;
        let outcome: Result<()> = loop {
            {
                let mut slot = inner.slot();
                if slot.generation != generation {
                    return Err(anyhow!("Loading was cancelled."));
                }
                let Some(process) = slot.process.as_mut() else {
                    return Err(anyhow!("Loading was cancelled."));
                };
                if let Ok(Some(status)) = process.child.try_wait() {
                    break Err(anyhow!(
                        "llama-server exited while loading {} ({status}). Last log lines:\n{}",
                        model.label,
                        process.log_tail(LOG_TAIL_ON_ERROR)
                    ));
                }
            }
            if wait_for_health(&inner.http, port, &api_key).await {
                break Ok(());
            }
            if Instant::now() >= deadline {
                let tail = inner.slot().process.as_ref().map(|p| p.log_tail(LOG_TAIL_ON_ERROR)).unwrap_or_default();
                break Err(anyhow!(
                    "{} did not become ready within {} s. Last log lines:\n{tail}",
                    model.label,
                    SERVER_READY_TIMEOUT.as_secs()
                ));
            }
            tokio::time::sleep(HEALTH_POLL_INTERVAL).await;
        };

        let failed = {
            let mut slot = inner.slot();
            if slot.generation != generation {
                return Err(anyhow!("Loading was cancelled."));
            }
            match &outcome {
                Ok(()) => {
                    let label = slot
                        .process
                        .as_ref()
                        .map(|process| {
                            let lines = process.log_lines();
                            parse_device_label(lines.iter().map(String::as_str))
                        })
                        .unwrap_or_else(|| "Unknown device".to_string());
                    slot.state = ServerState::Ready;
                    slot.device_label = Some(label);
                    slot.last_activity = Instant::now();
                    None
                }
                Err(err) => {
                    slot.set_error(format!("{err:#}"));
                    slot.process.take()
                }
            }
        };
        if let Some(process) = failed {
            let inner = inner.clone();
            tokio::task::spawn_blocking(move || inner.stop_and_forget(Some(process), STOP_GRACE, RECORD_LOCK_WAIT))
                .await
                .ok();
        }
        on_change();
        outcome
    }

    /// Reads the run record and cleans up what it may (plan §4.4): stops an orphaned server
    /// and removes stale records. The caller holds the load lock, so no other VoiceReader
    /// changes the record meanwhile.
    async fn admit(&self, lock: &LoadLock, probe: &(dyn ProcessProbe + Sync)) -> RecordDecision {
        let path = self.inner.layout.run_record_path();
        let record = read_run_record(&path);
        let decision = decide_run_record(record.as_ref(), std::process::id(), probe);
        if let Some(record) = record.as_ref() {
            match decision {
                RecordDecision::StopOrphan => {
                    let pid = record.server_pid;
                    tokio::task::spawn_blocking(move || terminate_pid(pid)).await.ok();
                    remove_run_record_if_unchanged(&path, record, lock);
                }
                RecordDecision::Discard => remove_run_record_if_unchanged(&path, record, lock),
                RecordDecision::InUseElsewhere | RecordDecision::NoRecord => {}
            }
        }
        decision
    }

    /// Starts the once-per-app task that unloads the server after [`IDLE_UNLOAD_AFTER`].
    fn ensure_idle_watch(&self, app: &AppHandle) {
        if self.inner.idle_watch_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(IDLE_CHECK_INTERVAL).await;
                let Some(inner) = weak.upgrade() else { break };
                let unloaded = tokio::task::spawn_blocking(move || inner.unload_if_idle()).await.unwrap_or(false);
                if unloaded {
                    let state = app.state::<AssistantState>();
                    emit_status(&app, &state);
                }
            }
        });
    }
}

/// Extracts `archive` with the system `tar` into `<staging>/extract`, then moves that into
/// `final_dir`. Any failure leaves no `final_dir` and no extract directory behind.
fn extract_and_install(archive: &Path, staging: &Path, final_dir: &Path) -> Result<()> {
    let extract = staging.join("extract");
    // `staging` is `<root>/backend/.staging-<version>`.
    let backend = staging.parent().unwrap_or(staging);
    let root = backend.parent().unwrap_or(backend);
    ensure_no_symlinks(&[root, backend, staging, &extract, final_dir])?;
    if extract.exists() {
        std::fs::remove_dir_all(&extract).with_context(|| format!("Failed to reset {}", extract.display()))?;
    }
    std::fs::create_dir_all(&extract).with_context(|| format!("Failed to create {}", extract.display()))?;
    let result = (|| {
        run_tar(archive, &extract)?;
        if find_server_binary(&extract).is_none() {
            return Err(anyhow!("The llama.cpp archive does not contain {SERVER_BINARY}."));
        }
        if final_dir.exists() {
            std::fs::remove_dir_all(final_dir).with_context(|| format!("Failed to replace {}", final_dir.display()))?;
        }
        std::fs::rename(&extract, final_dir).with_context(|| format!("Failed to install {}", final_dir.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&extract);
        return result;
    }
    let _ = std::fs::remove_dir_all(staging);
    Ok(())
}

/// Refuses to continue if any of `paths` is a symbolic link (or, on Windows, a junction), so
/// removing or renaming inside it cannot reach files outside the Assistant root.
fn ensure_no_symlinks(paths: &[&Path]) -> Result<()> {
    for path in paths {
        let is_link = std::fs::symlink_metadata(path)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false);
        if is_link {
            return Err(anyhow!(
                "Refusing to install the Assistant backend: {} is a symbolic link. Remove it and try again.",
                path.display()
            ));
        }
    }
    Ok(())
}

/// The operating system's bsdtar, which reads both `.tar.gz` and `.zip` and keeps symlinks
/// and executable bits.
fn run_tar(archive: &Path, dir: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| OsString::from(r"C:\Windows"));
        let mut command = Command::new(PathBuf::from(system_root).join("System32").join("tar.exe"));
        command.arg("-xf").arg(archive).arg("-C").arg(dir);
        command.creation_flags(CREATE_NO_WINDOW);
        command
    };
    #[cfg(not(target_os = "windows"))]
    let mut command = {
        let mut command = Command::new("/usr/bin/tar");
        command.arg("-xzf").arg(archive).arg("-C").arg(dir);
        command
    };
    let output = command
        .stdin(Stdio::null())
        .output()
        .context("Failed to run tar to extract the llama.cpp archive")?;
    if !output.status.success() {
        return Err(anyhow!(
            "Extracting the llama.cpp archive failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Tauri commands

fn emit_status(app: &AppHandle, state: &AssistantState) {
    let _ = app.emit_all(STATUS_EVENT, state.status());
}

fn command_error(err: anyhow::Error) -> String {
    format!("{err:#}")
}

#[tauri::command]
pub(crate) fn assistant_status(state: State<'_, AssistantState>) -> Result<AssistantStatus, String> {
    Ok(state.status())
}

#[tauri::command]
pub(crate) async fn assistant_install_backend(
    app: AppHandle,
    state: State<'_, AssistantState>,
) -> Result<AssistantStatus, String> {
    let notify_app = app.clone();
    let on_change = move || emit_status(&notify_app, &notify_app.state::<AssistantState>());
    let progress_app = app.clone();
    let on_progress = move |progress: DownloadProgress| {
        let _ = progress_app.emit_all(PROGRESS_EVENT, progress);
    };
    let result = state.install_backend(&on_change, &on_progress).await;
    emit_status(&app, &state);
    result.map_err(command_error)?;
    Ok(state.status())
}

#[tauri::command]
pub(crate) async fn assistant_download_model(
    app: AppHandle,
    state: State<'_, AssistantState>,
    model_id: String,
) -> Result<AssistantStatus, String> {
    let notify_app = app.clone();
    let on_change = move || emit_status(&notify_app, &notify_app.state::<AssistantState>());
    let progress_app = app.clone();
    let on_progress = move |progress: DownloadProgress| {
        let _ = progress_app.emit_all(PROGRESS_EVENT, progress);
    };
    let result = state.download_model(&model_id, &on_change, &on_progress).await;
    emit_status(&app, &state);
    result.map_err(command_error)?;
    Ok(state.status())
}

/// Cancels the running download. Its command then fails with "Download cancelled." and
/// the partial file is kept for resuming.
#[tauri::command]
pub(crate) fn assistant_cancel_download(state: State<'_, AssistantState>) -> Result<(), String> {
    state.cancel_download();
    Ok(())
}

#[tauri::command]
pub(crate) async fn assistant_load(
    app: AppHandle,
    state: State<'_, AssistantState>,
    model_id: String,
    ctx_size: u32,
) -> Result<AssistantStatus, String> {
    let notify_app = app.clone();
    let on_change = move || emit_status(&notify_app, &notify_app.state::<AssistantState>());
    state.load(&model_id, ctx_size, &on_change).await.map_err(command_error)?;
    state.ensure_idle_watch(&app);
    Ok(state.status())
}

#[tauri::command]
pub(crate) async fn assistant_unload(app: AppHandle, state: State<'_, AssistantState>) -> Result<AssistantStatus, String> {
    let inner = state.inner.clone();
    tokio::task::spawn_blocking(move || inner.unload_blocking())
        .await
        .map_err(|err| format!("Unloading the Assistant failed: {err}"))?;
    emit_status(&app, &state);
    Ok(state.status())
}

#[tauri::command]
pub(crate) async fn assistant_delete_model(
    app: AppHandle,
    state: State<'_, AssistantState>,
    model_id: String,
) -> Result<AssistantStatus, String> {
    let result = state.delete_model(&model_id).await;
    emit_status(&app, &state);
    result.map_err(command_error)?;
    Ok(state.status())
}

#[tauri::command]
pub(crate) fn assistant_diagnostics(state: State<'_, AssistantState>) -> Result<AssistantDiagnostics, String> {
    Ok(state.diagnostics())
}

/// Runs the installed `llama-server --list-devices` and returns its output, to show which
/// GPUs (Metal, Vulkan) llama.cpp can see. Needs no model and starts no server.
#[tauri::command]
pub(crate) async fn assistant_list_devices(state: State<'_, AssistantState>) -> Result<String, String> {
    let binary = state
        .inner
        .layout
        .server_binary()
        .ok_or_else(|| "The llama.cpp backend is not installed.".to_string())?;
    tokio::task::spawn_blocking(move || list_devices(&binary))
        .await
        .map_err(|err| format!("Checking devices failed: {err}"))?
        .map_err(command_error)
}

fn list_devices(binary: &Path) -> Result<String> {
    let mut command = Command::new(binary);
    command.arg("--list-devices").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(dir) = binary.parent() {
        command.current_dir(dir);
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().context("Failed to start llama-server --list-devices")?;
    // Read both pipes on their own threads so a full pipe can never stall the child.
    let collect = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_string(&mut text);
            }
            text
        })
    };
    let stdout = collect(child.stdout.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>));
    let stderr = collect(child.stderr.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>));
    let deadline = Instant::now() + LIST_DEVICES_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().context("Failed to wait for llama-server")? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("llama-server --list-devices did not finish within 20 s."));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let output = [stdout.join().unwrap_or_default(), stderr.join().unwrap_or_default()]
        .into_iter()
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!("exit status: {status}\n{}", output.trim_end()))
}

/// This computer's memory and GPUs and the recommended model tier. The GPU list comes from
/// the installed `llama-server --list-devices`, so it matches what llama.cpp will use.
#[tauri::command]
pub(crate) async fn assistant_hardware(
    state: State<'_, AssistantState>,
    refresh: Option<bool>,
) -> Result<AssistantHardware, String> {
    if !refresh.unwrap_or(false) {
        if let Some(cached) = state.inner.hardware.lock().unwrap_or_else(|p| p.into_inner()).clone() {
            return Ok(cached);
        }
    }
    let binary = state.inner.layout.server_binary();
    let listing = match binary {
        Some(binary) => tokio::task::spawn_blocking(move || list_devices(&binary)).await.ok().and_then(Result::ok),
        None => None,
    };
    let devices_known = listing.is_some();
    let devices = listing.as_deref().map(parse_list_devices).unwrap_or_default();
    let unified_memory = cfg!(target_os = "macos");
    let total_ram_mib = system_ram_mib();
    let (recommended_tier, reason) = recommend_tier(total_ram_mib, unified_memory, &devices, devices_known);
    let hardware = AssistantHardware { total_ram_mib, unified_memory, devices, devices_known, recommended_tier, reason };
    if devices_known {
        *state.inner.hardware.lock().unwrap_or_else(|p| p.into_inner()) = Some(hardware.clone());
    }
    Ok(hardware)
}

/// Parses lines like `  Vulkan0: NVIDIA GeForce RTX 5090 (32607 MiB, 31502 MiB free)`.
/// Entries without memory (the `BLAS` helper, CPU) are not GPUs and are skipped.
fn parse_list_devices(output: &str) -> Vec<HardwareDevice> {
    output
        .lines()
        .filter_map(|line| {
            let (_, rest) = line.trim().split_once(": ")?;
            let open = rest.rfind(" (")?;
            let name = rest[..open].trim().to_string();
            let numbers: Vec<u64> = rest[open + 2..]
                .split(|c: char| !c.is_ascii_digit())
                .filter(|part| !part.is_empty())
                .filter_map(|part| part.parse().ok())
                .collect();
            let (&total_mib, &free_mib) = (numbers.first()?, numbers.get(1)?);
            (total_mib > 0).then(|| HardwareDevice { integrated: is_integrated_gpu(&name), name, total_mib, free_mib })
        })
        .collect()
}

/// Integrated graphics, recognised by name: Intel's UHD, Iris and Core Ultra "Arc Graphics",
/// and AMD's generic "Radeon(TM) Graphics" / Vega APUs. Discrete Arc cards are named
/// "Arc A..." / "Arc B..." and are not matched.
fn is_integrated_gpu(name: &str) -> bool {
    let lower = name.to_lowercase();
    let intel_integrated = lower.contains("intel")
        && (lower.contains("uhd") || lower.contains("iris") || lower.contains("hd graphics") || lower.ends_with("arc(tm) graphics") || lower.ends_with("arc graphics"));
    let amd_integrated = lower.contains("radeon")
        && (lower.ends_with("radeon(tm) graphics") || lower.ends_with("radeon graphics") || lower.contains("vega"));
    intel_integrated || amd_integrated
}

/// The largest tier whose 32K estimate passes the memory rule (plan §5.3): on unified memory,
/// CPU-only or integrated graphics, everything within 50% of RAM; on a discrete GPU, the GPU
/// part within its memory minus 1 GB and the host part within 50% of RAM. Only measured
/// estimates count: a calculated one may be off by more than the margin it is judged on.
fn recommend_tier(total_ram_mib: u64, unified: bool, devices: &[HardwareDevice], devices_known: bool) -> (ModelTier, String) {
    let ram_budget = total_ram_mib / 2;
    let discrete = if unified { None } else { devices.iter().filter(|d| !d.integrated).max_by_key(|d| d.total_mib) };
    let ram_gb = total_ram_mib as f64 / 1024.0;
    for model in models().iter().rev().filter(|model| model.memory.measured) {
        let (gpu, host) = model.memory.at(ESTIMATE_CTX_SIZE);
        let (gpu, host) = (u64::from(gpu), u64::from(host));
        let total_gb = (gpu + host) as f64 / 1024.0;
        let fits = match discrete {
            Some(device) => gpu + 1024 <= device.total_mib && host <= ram_budget,
            None => gpu + host <= ram_budget,
        };
        if fits {
            let reason = match discrete {
                Some(device) => format!(
                    "{} needs about {:.1} GB of GPU memory at 32K context; {} has {:.0} GB.",
                    model.label,
                    gpu as f64 / 1024.0,
                    device.name,
                    device.total_mib as f64 / 1024.0
                ),
                None if unified => format!(
                    "{} needs about {total_gb:.1} GB at 32K context, within half of this Mac's {ram_gb:.0} GB of memory.",
                    model.label
                ),
                None => format!(
                    "{} needs about {total_gb:.1} GB at 32K context, within half of this computer's {ram_gb:.0} GB of RAM{}.",
                    model.label,
                    if devices_known { "" } else { " (GPU memory is unknown until the backend is installed)" }
                ),
            };
            return (model.tier, reason);
        }
    }
    let smallest = &models()[0];
    (smallest.tier, format!("{} is the smallest model; this computer is below the recommended memory for any tier, so expect it to be slow.", smallest.label))
}

/// Physical memory in MiB, or 0 when it cannot be read.
fn system_ram_mib() -> u64 {
    #[cfg(target_os = "macos")]
    {
        let mut bytes: u64 = 0;
        let mut size = std::mem::size_of::<u64>();
        let name = b"hw.memsize\0";
        // SAFETY: `name` is NUL-terminated; `bytes` and `size` describe a valid u64 buffer.
        let result = unsafe {
            libc::sysctlbyname(name.as_ptr().cast(), (&mut bytes as *mut u64).cast(), &mut size, std::ptr::null_mut(), 0)
        };
        return if result == 0 { bytes / (1024 * 1024) } else { 0 };
    }
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        // SAFETY: MEMORYSTATUSEX is plain data; dwLength must be set before the call.
        let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
        return if ok != 0 { status.ullTotalPhys / (1024 * 1024) } else { 0 };
    }
    #[allow(unreachable_code)]
    0
}

/// True when `err` came from a cancelled download.
#[allow(dead_code)] // For callers that want to tell cancellation apart from failures.
pub(crate) fn is_download_cancelled(err: &anyhow::Error) -> bool {
    err.is::<DownloadCancelled>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeProbe {
        alive: Vec<u32>,
        exes: HashMap<u32, PathBuf>,
    }

    impl ProcessProbe for FakeProbe {
        fn is_alive(&self, pid: u32) -> bool {
            self.alive.contains(&pid)
        }

        fn exe_path(&self, pid: u32) -> Option<PathBuf> {
            self.exes.get(&pid).cloned()
        }
    }

    const OWN_PID: u32 = 100;
    const OTHER_OWNER: u32 = 200;
    const SERVER_PID: u32 = 300;

    fn server_exe() -> PathBuf {
        if cfg!(target_os = "windows") {
            PathBuf::from(r"C:\Users\me\AppData\Roaming\com.voicereader.desktop\assistant\backend\b11718\llama-server.exe")
        } else {
            PathBuf::from("/Users/me/Library/Application Support/com.voicereader.desktop/assistant/backend/b11718/llama-b11718/llama-server")
        }
    }

    fn record(owner_pid: u32) -> RunRecord {
        RunRecord { server_pid: SERVER_PID, exe_path: server_exe(), started_at_unix_ms: 1, owner_pid }
    }

    fn probe(alive: &[u32], server_exe_path: Option<PathBuf>) -> FakeProbe {
        let mut exes = HashMap::new();
        if let Some(path) = server_exe_path {
            exes.insert(SERVER_PID, path);
        }
        FakeProbe { alive: alive.to_vec(), exes }
    }

    #[test]
    fn assistant_record_none_means_nothing_to_do() {
        let probe = probe(&[], None);
        assert_eq!(decide_run_record(None, OWN_PID, &probe), RecordDecision::NoRecord);
    }

    #[test]
    fn assistant_record_live_owner_elsewhere_blocks() {
        let probe = probe(&[OTHER_OWNER, SERVER_PID], Some(server_exe()));
        assert_eq!(decide_run_record(Some(&record(OTHER_OWNER)), OWN_PID, &probe), RecordDecision::InUseElsewhere);
    }

    #[test]
    fn assistant_record_dead_owner_with_matching_server_is_stopped() {
        let probe = probe(&[SERVER_PID], Some(server_exe()));
        assert_eq!(decide_run_record(Some(&record(OTHER_OWNER)), OWN_PID, &probe), RecordDecision::StopOrphan);
    }

    #[test]
    fn assistant_record_from_this_process_is_stopped() {
        let probe = probe(&[OWN_PID, SERVER_PID], Some(server_exe()));
        assert_eq!(decide_run_record(Some(&record(OWN_PID)), OWN_PID, &probe), RecordDecision::StopOrphan);
    }

    #[test]
    fn assistant_record_reused_pid_is_left_alone() {
        let other = if cfg!(target_os = "windows") {
            PathBuf::from(r"C:\Windows\System32\notepad.exe")
        } else {
            PathBuf::from("/usr/bin/vim")
        };
        let probe = probe(&[SERVER_PID], Some(other.clone()));
        assert_eq!(decide_run_record(Some(&record(OTHER_OWNER)), OWN_PID, &probe), RecordDecision::Discard);
        // Even with a live owner, a PID that is no longer our server only needs the record removed.
        let probe = probe_with_owner(other);
        assert_eq!(decide_run_record(Some(&record(OTHER_OWNER)), OWN_PID, &probe), RecordDecision::Discard);
    }

    fn probe_with_owner(server_exe_path: PathBuf) -> FakeProbe {
        probe(&[OTHER_OWNER, SERVER_PID], Some(server_exe_path))
    }

    #[test]
    fn assistant_record_dead_server_is_discarded() {
        let probe = probe(&[OTHER_OWNER], None);
        assert_eq!(decide_run_record(Some(&record(OTHER_OWNER)), OWN_PID, &probe), RecordDecision::Discard);
    }

    #[test]
    fn assistant_record_unreadable_exe_is_left_alone() {
        let probe = probe(&[SERVER_PID], None);
        assert_eq!(decide_run_record(Some(&record(OTHER_OWNER)), OWN_PID, &probe), RecordDecision::Discard);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn assistant_same_executable_ignores_case_and_verbatim_prefix() {
        assert!(same_executable(
            Path::new(r"\\?\C:\Users\Me\llama-server.exe"),
            Path::new(r"c:\users\me\LLAMA-SERVER.EXE")
        ));
    }

    #[test]
    fn assistant_record_round_trips_through_disk() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("run").join("server.json");
        assert!(read_run_record(&path).is_none());
        write_run_record(&path, &record(OWN_PID)).unwrap();
        assert_eq!(read_run_record(&path), Some(record(OWN_PID)));
        let lock = LoadLock::try_acquire(&directory.path().join("run").join("load.lock")).unwrap().unwrap();
        let mut changed = record(OWN_PID);
        changed.server_pid += 1;
        remove_run_record_if_unchanged(&path, &changed, &lock);
        assert!(path.exists(), "a record that changed since it was read is kept");
        remove_run_record_if_unchanged(&path, &record(OWN_PID), &lock);
        assert!(!path.exists());
    }

    /// A stand-in server process: a long `sleep` (or `ping` on Windows).
    fn long_running_server() -> ServerProcess {
        #[cfg(target_os = "windows")]
        let (binary, args) = {
            let root = std::env::var_os("SystemRoot").unwrap_or_else(|| OsString::from(r"C:\Windows"));
            let binary = PathBuf::from(root).join("System32").join("PING.EXE");
            (binary, vec![OsString::from("-n"), OsString::from("60"), OsString::from("127.0.0.1")])
        };
        #[cfg(not(target_os = "windows"))]
        let (binary, args) = (PathBuf::from("/bin/sleep"), vec![OsString::from("60")]);
        ServerProcess::spawn(&binary, &args, 0, "k".repeat(48)).unwrap()
    }

    fn install_server(state: &AssistantState, process: ServerProcess) {
        let mut slot = state.inner.slot();
        slot.process = Some(process);
        slot.state = ServerState::Ready;
        slot.model_id = Some("minicpm-v-4.7-1b".to_string());
        slot.ctx_size = Some(4096);
    }

    fn own_record(server_pid: u32) -> RunRecord {
        RunRecord { server_pid, exe_path: server_exe(), started_at_unix_ms: 1, owner_pid: std::process::id() }
    }

    /// Writes placeholder files of the right size so the model counts as installed.
    fn fake_install(state: &AssistantState, id: &str) {
        let model = model_by_id(id).unwrap();
        let dir = state.inner.layout.model_dir(model);
        std::fs::create_dir_all(&dir).unwrap();
        for file in model.files() {
            let handle = std::fs::File::create(dir.join(file.file)).unwrap();
            handle.set_len(file.size).unwrap();
        }
        assert!(state.inner.layout.model_installed(model));
    }

    fn installed(state: &AssistantState, id: &str) -> bool {
        state.status().models.iter().any(|model| model.id == id && model.installed)
    }

    #[tokio::test]
    async fn assistant_delete_removes_only_that_model() {
        let directory = tempfile::tempdir().unwrap();
        let state = AssistantState::new(directory.path().to_path_buf());
        fake_install(&state, "minicpm-v-4.7-1b");
        fake_install(&state, "gemma-4-e4b");
        state.delete_model("minicpm-v-4.7-1b").await.unwrap();
        assert!(!installed(&state, "minicpm-v-4.7-1b"));
        assert!(!directory.path().join("models").join("minicpm-v-4.7-1b").exists());
        assert!(installed(&state, "gemma-4-e4b"));
        // Deleting a model that is not there is fine; an unknown id is not.
        state.delete_model("minicpm-v-4.7-1b").await.unwrap();
        assert!(state.delete_model("no-such-model").await.is_err());
    }

    #[tokio::test]
    async fn assistant_delete_waits_for_downloads_and_answers() {
        let directory = tempfile::tempdir().unwrap();
        let state = AssistantState::new(directory.path().to_path_buf());
        fake_install(&state, "minicpm-v-4.7-1b");
        {
            let _download = state.begin_download(DownloadKind::Model, "gemma-4-e4b").unwrap();
            let err = state.delete_model("minicpm-v-4.7-1b").await.unwrap_err();
            assert!(format!("{err:#}").contains("download"), "{err:#}");
        }
        install_server(&state, long_running_server());
        {
            let _answering = state.begin_use();
            let err = state.delete_model("minicpm-v-4.7-1b").await.unwrap_err();
            assert!(format!("{err:#}").contains("answering"), "{err:#}");
            assert!(installed(&state, "minicpm-v-4.7-1b"));
        }
        // Idle and loaded: it is unloaded, then deleted.
        state.delete_model("minicpm-v-4.7-1b").await.unwrap();
        assert_eq!(state.status().server.state, ServerState::Stopped);
        assert!(!installed(&state, "minicpm-v-4.7-1b"));
    }

    #[test]
    fn assistant_parses_device_listings() {
        let mac = "Available devices:\n  MTL0: Apple M2 Max (79626 MiB, 79625 MiB free)\n  BLAS: Accelerate (0 MiB, 0 MiB free)\n";
        assert_eq!(
            parse_list_devices(mac),
            vec![HardwareDevice { name: "Apple M2 Max".into(), total_mib: 79626, free_mib: 79625, integrated: false }]
        );
        let windows = "Available devices:\n  Vulkan0: NVIDIA GeForce RTX 5090 (32607 MiB, 31502 MiB free)\n  Vulkan1: AMD Radeon(TM) Graphics (2048 MiB, 1900 MiB free)\n";
        let devices = parse_list_devices(windows);
        assert_eq!(devices.len(), 2);
        assert!(!devices[0].integrated && devices[1].integrated);
        assert!(parse_list_devices("exit status: 1\nno devices").is_empty());
    }

    #[test]
    fn assistant_recognises_integrated_gpus() {
        for name in ["Intel(R) UHD Graphics 770", "Intel(R) Iris(R) Xe Graphics", "Intel(R) Arc(TM) Graphics", "AMD Radeon(TM) Graphics", "AMD Radeon Vega 8 Graphics"] {
            assert!(is_integrated_gpu(name), "{name}");
        }
        for name in ["NVIDIA GeForce RTX 4090", "Intel(R) Arc(TM) A770 Graphics", "AMD Radeon RX 7900 XTX", "Apple M2 Max"] {
            assert!(!is_integrated_gpu(name), "{name}");
        }
    }

    #[test]
    fn assistant_recommends_by_the_memory_rule() {
        let gpu = |name: &str, gib: u64| HardwareDevice { name: name.into(), total_mib: gib * 1024, free_mib: gib * 1024, integrated: is_integrated_gpu(name) };
        // Macs: 50% of unified memory. 12B needs about 9.1 GB, E4B about 7.2 GB at 32K.
        assert_eq!(recommend_tier(96 * 1024, true, &[gpu("Apple M2 Max", 78)], true).0, ModelTier::Large);
        assert_eq!(recommend_tier(16 * 1024, true, &[], true).0, ModelTier::Mid);
        assert_eq!(recommend_tier(8 * 1024, true, &[], true).0, ModelTier::Small);
        // A discrete 24 GB GPU with 64 GB RAM takes the large model; an 8 GB GPU does not.
        assert_eq!(recommend_tier(64 * 1024, false, &[gpu("NVIDIA GeForce RTX 4090", 24)], true).0, ModelTier::Large);
        assert_eq!(recommend_tier(16 * 1024, false, &[gpu("NVIDIA GeForce RTX 4060", 8)], true).0, ModelTier::Mid);
        assert_eq!(recommend_tier(16 * 1024, false, &[gpu("NVIDIA GeForce GTX 1650", 4)], true).0, ModelTier::Small);
        // Integrated graphics are judged by RAM.
        assert_eq!(recommend_tier(16 * 1024, false, &[gpu("Intel(R) Iris(R) Xe Graphics", 8)], true).0, ModelTier::Mid);
    }

    #[test]
    fn assistant_status_carries_estimates_and_context_limits() {
        let directory = tempfile::tempdir().unwrap();
        let status = AssistantState::new(directory.path().to_path_buf()).status();
        let e4b = status.models.iter().find(|model| model.id == "gemma-4-e4b").unwrap();
        assert_eq!(e4b.max_context, 131_072);
        assert_eq!(e4b.memory_estimate.ctx_size, 32_768);
        assert!(e4b.memory_estimate.measured && e4b.memory_estimate.gpu_mib > 4_000);
    }

    #[test]
    fn assistant_shutdown_without_a_server_leaves_a_foreign_record() {
        let directory = tempfile::tempdir().unwrap();
        let state = AssistantState::new(directory.path().to_path_buf());
        let path = state.inner.layout.run_record_path();
        write_run_record(&path, &record(OTHER_OWNER)).unwrap();
        state.shutdown();
        assert_eq!(read_run_record(&path), Some(record(OTHER_OWNER)));
        // A record owned by this process but naming no server it ran is also left alone.
        write_run_record(&path, &own_record(SERVER_PID)).unwrap();
        state.shutdown();
        assert_eq!(read_run_record(&path), Some(own_record(SERVER_PID)));
    }

    #[test]
    fn assistant_unload_removes_only_the_record_of_the_server_it_stopped() {
        let directory = tempfile::tempdir().unwrap();
        let state = AssistantState::new(directory.path().to_path_buf());
        let path = state.inner.layout.run_record_path();

        // A newer server's record (another PID) survives a late unload of this one.
        let process = long_running_server();
        let other_pid = process.child.id().wrapping_add(1);
        install_server(&state, process);
        write_run_record(&path, &own_record(other_pid)).unwrap();
        state.inner.unload_blocking();
        assert_eq!(read_run_record(&path), Some(own_record(other_pid)));
        assert_eq!(state.status().server.state, ServerState::Stopped);

        // The record of the server it stopped is removed.
        let process = long_running_server();
        let pid = process.child.id();
        install_server(&state, process);
        write_run_record(&path, &own_record(pid)).unwrap();
        state.inner.unload_blocking();
        assert!(!path.exists());
        assert!(!SystemProbe.is_alive(pid), "server {pid} was stopped");
    }

    #[tokio::test]
    async fn assistant_admission_is_exclusive_across_instances() {
        let directory = tempfile::tempdir().unwrap();
        let first = AssistantState::new(directory.path().to_path_buf());
        let second = AssistantState::new(directory.path().to_path_buf());
        let lock_path = first.inner.layout.load_lock_path();
        let record_path = first.inner.layout.run_record_path();

        // The first instance is inside admission and has written its live server's record.
        let held = LoadLock::acquire(&lock_path, Duration::ZERO).await.unwrap();
        write_run_record(&record_path, &record(OTHER_OWNER)).unwrap();

        // The second cannot get past the lock while it is held...
        let started = Instant::now();
        let err = LoadLock::acquire(&second.inner.layout.load_lock_path(), Duration::from_millis(300))
            .await
            .err()
            .expect("lock is busy");
        assert_eq!(err.to_string(), LOAD_LOCK_BUSY_MESSAGE);
        assert!(started.elapsed() >= Duration::from_millis(300));
        // ...nor remove records meanwhile.
        remove_own_run_record(&second.inner.layout, SERVER_PID, Duration::ZERO);
        assert!(record_path.exists());

        // Once released, admission sees the live owner and refuses to start a server.
        drop(held);
        let lock = LoadLock::acquire(&second.inner.layout.load_lock_path(), Duration::from_millis(300)).await.unwrap();
        let live = probe(&[OTHER_OWNER, SERVER_PID], Some(server_exe()));
        assert_eq!(second.admit(&lock, &live).await, RecordDecision::InUseElsewhere);
        assert_eq!(read_run_record(&record_path), Some(record(OTHER_OWNER)));

        // A record whose server is gone is removed by admission.
        let gone = probe(&[OTHER_OWNER], None);
        assert_eq!(second.admit(&lock, &gone).await, RecordDecision::Discard);
        assert!(!record_path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn assistant_refuses_a_symlinked_staging_dir() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("assistant");
        std::fs::create_dir_all(root.join("backend")).unwrap();
        let external = directory.path().join("external");
        std::fs::create_dir_all(external.join("extract")).unwrap();
        std::fs::write(external.join("sentinel"), b"keep").unwrap();
        std::fs::write(external.join("extract").join("sentinel"), b"keep").unwrap();
        let state = AssistantState::new(root.clone());
        let layout = &state.inner.layout;
        std::os::unix::fs::symlink(&external, layout.staging_dir()).unwrap();

        let staging = layout.staging_dir();
        let err = extract_and_install(&staging.join("backend.tar.gz"), &staging, &layout.backend_dir()).unwrap_err();
        assert!(format!("{err:#}").contains("symbolic link"), "{err:#}");
        if backend_asset().is_some() {
            let err = state.install_backend(&|| {}, &|_| {}).await.unwrap_err();
            assert!(format!("{err:#}").contains("symbolic link"), "{err:#}");
        }
        assert_eq!(std::fs::read(external.join("sentinel")).unwrap(), b"keep");
        assert_eq!(std::fs::read(external.join("extract").join("sentinel")).unwrap(), b"keep");
        assert!(!layout.backend_dir().exists());
    }

    #[cfg(unix)]
    #[test]
    fn assistant_refuses_a_symlinked_root_or_backend_dir() {
        let directory = tempfile::tempdir().unwrap();
        let external = directory.path().join("external");
        std::fs::create_dir_all(external.join("backend").join(".staging-b11718").join("extract")).unwrap();
        std::fs::write(external.join("backend").join(".staging-b11718").join("extract").join("sentinel"), b"keep").unwrap();
        let root = directory.path().join("assistant");
        std::os::unix::fs::symlink(&external, &root).unwrap();
        let layout = Layout { root };
        let staging = layout.staging_dir();
        assert!(extract_and_install(&staging.join("a.tar.gz"), &staging, &layout.backend_dir()).is_err());
        assert!(external.join("backend").join(".staging-b11718").join("extract").join("sentinel").exists());

        let root = directory.path().join("assistant2");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(external.join("backend"), root.join("backend")).unwrap();
        let layout = Layout { root };
        let staging = layout.staging_dir();
        assert!(extract_and_install(&staging.join("a.tar.gz"), &staging, &layout.backend_dir()).is_err());
        assert!(external.join("backend").join(".staging-b11718").join("extract").join("sentinel").exists());
    }

    #[test]
    fn assistant_system_probe_sees_this_process() {
        let probe = SystemProbe;
        assert!(probe.is_alive(std::process::id()));
        let exe = probe.exe_path(std::process::id()).expect("own executable path");
        assert!(same_executable(&exe, &std::env::current_exe().unwrap()), "{exe:?}");
    }

    #[test]
    fn assistant_server_args_bind_localhost_with_key() {
        let args = server_args(4321, "k".repeat(48).as_str(), Path::new("/m/w.gguf"), Path::new("/m/p.gguf"), 32768);
        let args: Vec<String> = args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect();
        let value_of = |flag: &str| args.iter().position(|arg| arg == flag).map(|index| args[index + 1].clone());
        assert_eq!(value_of("--host").as_deref(), Some("127.0.0.1"));
        assert_eq!(value_of("--port").as_deref(), Some("4321"));
        assert_eq!(value_of("--api-key").map(|key| key.len()), Some(48));
        assert_eq!(value_of("-m").as_deref(), Some("/m/w.gguf"));
        assert_eq!(value_of("--mmproj").as_deref(), Some("/m/p.gguf"));
        assert_eq!(value_of("-c").as_deref(), Some("32768"));
        assert_eq!(value_of("--parallel").as_deref(), Some("1"));
        assert_eq!(value_of("--reasoning-format").as_deref(), Some("deepseek"));
        assert!(args.contains(&"--jinja".to_string()));
        assert!(args.contains(&"--no-reasoning-preserve".to_string()));
        assert_eq!(value_of("-lv").as_deref(), Some("4"));
        assert!(!args.iter().any(|arg| arg == "-ngl" || arg == "--fit" || arg == "0.0.0.0"));
    }

    #[test]
    fn assistant_api_keys_are_long_and_random() {
        let (a, b) = (generate_api_key(), generate_api_key());
        assert!(a.len() >= 32 && a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(a, b);
    }

    #[test]
    fn assistant_finds_server_binary_in_nested_extract_dir() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("llama-b11718");
        std::fs::create_dir_all(nested.join("lib")).unwrap();
        std::fs::write(nested.join("libggml.dylib"), b"").unwrap();
        std::fs::write(nested.join(SERVER_BINARY), b"").unwrap();
        assert_eq!(find_server_binary(directory.path()), Some(nested.join(SERVER_BINARY)));
    }

    #[test]
    fn assistant_finds_server_binary_in_flat_extract_dir() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("llama-cli"), b"").unwrap();
        assert_eq!(find_server_binary(directory.path()), None);
        std::fs::write(directory.path().join(SERVER_BINARY), b"").unwrap();
        assert_eq!(find_server_binary(directory.path()), Some(directory.path().join(SERVER_BINARY)));
        assert_eq!(find_server_binary(&directory.path().join("missing")), None);
    }

    #[test]
    fn assistant_failed_extraction_installs_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join(".staging-b11718");
        std::fs::create_dir_all(&staging).unwrap();
        let archive = staging.join("broken.tar.gz");
        std::fs::write(&archive, b"not an archive").unwrap();
        let final_dir = directory.path().join("b11718");
        assert!(extract_and_install(&archive, &staging, &final_dir).is_err());
        assert!(!final_dir.exists());
        assert!(!staging.join("extract").exists());
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn assistant_extracts_tar_gz_and_installs_it() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("src");
        std::fs::create_dir_all(source.join("llama-b11718")).unwrap();
        std::fs::write(source.join("llama-b11718").join(SERVER_BINARY), b"#!/bin/sh\n").unwrap();
        let staging = directory.path().join(".staging-b11718");
        std::fs::create_dir_all(&staging).unwrap();
        let archive = staging.join("backend.tar.gz");
        let status = Command::new("/usr/bin/tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&source)
            .arg("llama-b11718")
            .status()
            .unwrap();
        assert!(status.success());
        let final_dir = directory.path().join("b11718");
        extract_and_install(&archive, &staging, &final_dir).unwrap();
        assert_eq!(find_server_binary(&final_dir), Some(final_dir.join("llama-b11718").join(SERVER_BINARY)));
        assert!(!staging.exists());
    }

    #[test]
    fn assistant_layout_paths() {
        let layout = Layout { root: PathBuf::from("root") };
        let model = model_by_id("gemma-4-e4b").unwrap();
        assert_eq!(layout.backend_dir(), Path::new("root").join("backend").join("b11718"));
        assert_eq!(layout.staging_dir(), Path::new("root").join("backend").join(".staging-b11718"));
        assert_eq!(
            layout.model_dir(model),
            Path::new("root").join("models").join("gemma-4-e4b").join("bfc15c382204943c3a8fff0c750b94ae2364d7a3")
        );
        assert_eq!(layout.run_record_path(), Path::new("root").join("run").join("server.json"));
    }

    #[test]
    fn assistant_device_label_vulkan() {
        let log = [
            "ggml_vulkan: Found 1 Vulkan devices:",
            "ggml_vulkan: 0 = NVIDIA GeForce RTX 5090 (NVIDIA) | uma: 0 | fp16: 1 | bf16: 1 | warp size: 32 | shared memory: 49152 | int dot: 1 | matrix cores: NV_coopmat2",
            "llama_model_load_from_file_impl: using device Vulkan0 (NVIDIA GeForce RTX 5090) (0000:01:00.0) - 31220 MiB free",
            "load_tensors: offloading 42 repeating layers to GPU",
            "load_tensors: offloaded 43/43 layers to GPU",
            "load_tensors:      Vulkan0 model buffer size =  4512.00 MiB",
            "load_tensors:   CPU_Mapped model buffer size =   525.00 MiB",
        ];
        assert_eq!(parse_device_label(log), "Vulkan: NVIDIA GeForce RTX 5090");
    }

    #[test]
    fn assistant_device_label_vulkan_without_using_device_line() {
        let log = [
            "ggml_vulkan: 0 = AMD Radeon RX 7800 XT (AMD proprietary driver) | uma: 0 | fp16: 1",
            "load_tensors: offloaded 43/43 layers to GPU",
        ];
        assert_eq!(parse_device_label(log), "Vulkan: AMD Radeon RX 7800 XT");
    }

    #[test]
    fn assistant_device_label_partial_offload() {
        let log = [
            "llama_model_load_from_file_impl: using device Vulkan0 (Intel(R) Arc(TM) Graphics) (0000:00:02.0) - 7000 MiB free",
            "load_tensors: offloaded 20/43 layers to GPU",
        ];
        assert_eq!(parse_device_label(log), "Vulkan: Intel(R) Arc(TM) Graphics (20/43 layers)");
    }

    #[test]
    fn assistant_device_label_metal() {
        // Captured from llama-server b11718 on an M2 Max with `-lv 4`.
        let log = [
            "0.00.042.306 I ggml_metal_device_init: GPU name:   MTL0 (Apple M2 Max)",
            "0.00.042.720 I cmn  common_param:   - MTL0    : Apple M2 Max (79626 MiB, 79625 MiB free)",
            "0.00.390.160 I llama_prepare_model_devices: using device MTL0 (Apple M2 Max) (unknown id) - 79625 MiB free",
            "0.00.530.781 I load_tensors: offloaded 25/25 layers to GPU",
            "0.00.530.782 I load_tensors:   CPU_Mapped model buffer size =   136.28 MiB",
            "0.00.530.783 I load_tensors:  MTL0_Mapped model buffer size =   198.74 MiB",
        ];
        assert_eq!(parse_device_label(log), "Metal: Apple M2 Max");
        let name_only = [
            "0.00.042.306 I ggml_metal_device_init: GPU name:   MTL0 (Apple M2 Max)",
            "0.00.530.781 I load_tensors: offloaded 25/25 layers to GPU",
        ];
        assert_eq!(parse_device_label(name_only), "Metal: Apple M2 Max");
        let older = ["ggml_metal_init: GPU name:   Apple M1", "load_tensors: offloaded 35/35 layers to GPU"];
        assert_eq!(parse_device_label(older), "Metal: Apple M1");
        let mtl = ["llama_model_load_from_file_impl: using device MTL0 (Apple M3 Pro) - 27000 MiB free"];
        assert_eq!(parse_device_label(mtl), "Metal: Apple M3 Pro");
    }

    #[test]
    fn assistant_device_label_cpu() {
        let hidden_gpu = [
            "ggml_vulkan: 0 = NVIDIA GeForce RTX 5090 (NVIDIA) | uma: 0",
            "load_tensors: offloaded 0/43 layers to GPU",
            "load_tensors:   CPU_Mapped model buffer size =  5000.00 MiB",
        ];
        assert_eq!(parse_device_label(hidden_gpu), "CPU");
        let cpu_only = ["load_tensors:   CPU_Mapped model buffer size =  5000.00 MiB"];
        assert_eq!(parse_device_label(cpu_only), "CPU");
    }

    #[test]
    fn assistant_device_label_unknown() {
        assert_eq!(parse_device_label(["main: server is listening on http://127.0.0.1:8080"]), "Unknown device");
        assert_eq!(parse_device_label(["", "using device ", "offloaded x/y layers"]), "Unknown device");
    }

    #[test]
    fn assistant_log_buffer_keeps_the_last_lines() {
        let mut log = LogBuffer::default();
        for index in 0..(LOG_CAPACITY + 10) {
            log.push(format!("line {index}"));
        }
        assert_eq!(log.lines.len(), LOG_CAPACITY);
        assert_eq!(log.head.len(), LOG_HEAD_CAPACITY);
        assert_eq!(log.head[0], "line 0");
        assert_eq!(log.lines.front().map(String::as_str), Some("line 10"));
        assert_eq!(log.tail(2), format!("line {}\nline {}", LOG_CAPACITY + 8, LOG_CAPACITY + 9));
    }

    #[test]
    fn assistant_idle_rule() {
        let start = Instant::now();
        let later = start + IDLE_UNLOAD_AFTER;
        assert!(idle_expired(ServerState::Ready, 0, start, later));
        assert!(!idle_expired(ServerState::Ready, 1, start, later));
        assert!(!idle_expired(ServerState::Ready, 0, start, start + Duration::from_secs(60)));
        assert!(!idle_expired(ServerState::Starting, 0, start, later));
    }

    #[test]
    fn assistant_use_guard_counts_and_records_activity() {
        let state = AssistantState::new(PathBuf::from("unused"));
        let before = state.inner.slot().last_activity;
        let first = state.begin_use();
        let second = state.begin_use();
        assert_eq!(state.inner.slot().in_use, 2);
        drop(first);
        assert_eq!(state.inner.slot().in_use, 1);
        drop(second);
        assert_eq!(state.inner.slot().in_use, 0);
        assert!(state.inner.slot().last_activity >= before);
        assert!(state.endpoint().is_none());
    }

    #[test]
    fn assistant_only_one_download_at_a_time() {
        let state = AssistantState::new(PathBuf::from("unused"));
        let guard = state.begin_download(DownloadKind::Model, "gemma-4-e4b").unwrap();
        assert!(state.begin_download(DownloadKind::Backend, BACKEND_VERSION).is_err());
        assert_eq!(
            state.status().download,
            Some(DownloadInfo { kind: DownloadKind::Model, id: "gemma-4-e4b".to_string() })
        );
        drop(guard);
        assert!(state.status().download.is_none());
        assert!(state.begin_download(DownloadKind::Backend, BACKEND_VERSION).is_ok());
    }

    #[tokio::test]
    async fn assistant_load_reports_missing_pieces() {
        let directory = tempfile::tempdir().unwrap();
        let state = AssistantState::new(directory.path().to_path_buf());
        let err = state.load("no-such-model", 4096, &|| {}).await.unwrap_err();
        assert!(format!("{err:#}").contains("Unknown Assistant model"));
        let err = state.load("gemma-4-e4b", 16, &|| {}).await.unwrap_err();
        assert!(format!("{err:#}").contains("Context size"));
        let err = state.load("gemma-4-e4b", 4096, &|| {}).await.unwrap_err();
        if backend_asset().is_some() {
            assert!(format!("{err:#}").contains("backend is not installed"), "{err:#}");
            // With a backend but no model files, the model is reported missing.
            let dir = state.inner.layout.backend_dir();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(SERVER_BINARY), b"").unwrap();
            let err = state.load("gemma-4-e4b", 4096, &|| {}).await.unwrap_err();
            assert!(format!("{err:#}").contains("Gemma 4 E4B is not downloaded"), "{err:#}");
        }
        assert_eq!(state.status().server.state, ServerState::Stopped);
    }

    #[test]
    fn assistant_status_serializes_snake_case() {
        let directory = tempfile::tempdir().unwrap();
        let state = AssistantState::new(directory.path().to_path_buf());
        let value = serde_json::to_value(state.status()).unwrap();
        assert_eq!(value["backend_version"], "b11718");
        assert_eq!(value["backend_installed"], false);
        assert_eq!(value["download"], serde_json::Value::Null);
        assert_eq!(value["server"]["state"], "stopped");
        assert_eq!(value["models"][0]["id"], "minicpm-v-4.7-1b");
        assert_eq!(value["models"][0]["tier"], "small");
        assert_eq!(value["models"][0]["download_bytes"], 1_782_121_664u64);
        assert_eq!(value["models"][0]["installed"], false);
        let progress = serde_json::to_value(DownloadProgress {
            kind: DownloadKind::Backend,
            id: "b11718".into(),
            file: "a.zip".into(),
            downloaded: 1,
            total: 2,
        })
        .unwrap();
        assert_eq!(progress["kind"], "backend");
        assert_eq!(
            serde_json::to_value(ServerState::InUseElsewhere).unwrap(),
            serde_json::Value::String("in_use_elsewhere".into())
        );
    }

    #[test]
    fn assistant_status_reports_in_use_elsewhere_from_the_record() {
        let directory = tempfile::tempdir().unwrap();
        let state = AssistantState::new(directory.path().to_path_buf());
        // This test binary stands in for the server; a child process stands in for the
        // other VoiceReader that owns it.
        let mut owner = long_running_process();
        let record = RunRecord {
            server_pid: std::process::id(),
            exe_path: std::env::current_exe().unwrap(),
            started_at_unix_ms: 1,
            owner_pid: owner.id(),
        };
        write_run_record(&state.inner.layout.run_record_path(), &record).unwrap();
        let status = state.status();
        assert_eq!(status.server.state, ServerState::InUseElsewhere);
        assert_eq!(status.server.error.as_deref(), Some(IN_USE_ELSEWHERE_MESSAGE));

        owner.kill().unwrap();
        owner.wait().unwrap();
        // Once the owner is gone the record is an orphan to clean up on the next load.
        assert_eq!(state.status().server.state, ServerState::Stopped);
    }

    fn long_running_process() -> Child {
        #[cfg(target_os = "windows")]
        let mut command = {
            let mut command = Command::new("ping");
            command.args(["-n", "60", "127.0.0.1"]);
            command
        };
        #[cfg(not(target_os = "windows"))]
        let mut command = {
            let mut command = Command::new("/bin/sleep");
            command.arg("60");
            command
        };
        command.stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap()
    }

    /// Installs the backend and downloads a model into `VOICEREADER_ASSISTANT_TEST_ROOT`
    /// through the same code the app uses, so the end-to-end test below has something to
    /// load. Resumes if interrupted. Run with:
    /// `VOICEREADER_ASSISTANT_TEST_ROOT=<root> cargo test --features build-base assistant_backend::tests::assistant_install_into_test_root -- --ignored --nocapture`
    /// Set `VOICEREADER_ASSISTANT_TEST_MODEL=none` to install only the backend.
    #[tokio::test]
    #[ignore = "needs network access; downloads the backend and a model (1.8 GB by default)"]
    async fn assistant_install_into_test_root() {
        let root = PathBuf::from(std::env::var("VOICEREADER_ASSISTANT_TEST_ROOT").expect("set VOICEREADER_ASSISTANT_TEST_ROOT"));
        let model_id = std::env::var("VOICEREADER_ASSISTANT_TEST_MODEL").unwrap_or_else(|_| "minicpm-v-4.7-1b".to_string());
        let state = AssistantState::new(root.clone());
        let last = Mutex::new(0u64);
        let on_progress = |progress: DownloadProgress| {
            let mut last = last.lock().unwrap();
            if progress.downloaded >= *last + 256 * 1024 * 1024 || progress.downloaded == progress.total {
                *last = progress.downloaded;
                println!("{} {}: {} / {}", progress.id, progress.file, progress.downloaded, progress.total);
            }
        };
        state.install_backend(&|| {}, &on_progress).await.expect("backend installs");
        let binary = state.inner.layout.server_binary().expect("server binary installed");
        let version = Command::new(&binary).arg("--version").current_dir(binary.parent().unwrap()).output().unwrap();
        println!("{}", String::from_utf8_lossy(&version.stderr).trim());
        println!("{}", String::from_utf8_lossy(&version.stdout).trim());
        assert!(version.status.success(), "llama-server --version failed");
        assert!(!state.inner.layout.staging_dir().exists());

        if model_id != "none" {
            *last.lock().unwrap() = 0;
            state.download_model(&model_id, &|| {}, &on_progress).await.expect("model downloads");
            let status = state.status();
            assert!(status.models.iter().any(|model| model.id == model_id && model.installed));
        }
    }

    fn loopback() -> Client {
        Client::builder().no_proxy().build().unwrap()
    }

    /// Loads a real model and unloads it. Needs an installed backend and the model under
    /// `VOICEREADER_ASSISTANT_TEST_ROOT` (the app's `<app data dir>/assistant`). Run with:
    /// `VOICEREADER_ASSISTANT_TEST_ROOT=<root> cargo test --features build-base assistant_backend::tests::assistant_end_to_end -- --ignored --nocapture`
    /// `VOICEREADER_ASSISTANT_TEST_MODEL` picks the model (default `minicpm-v-4.7-1b`) and
    /// `VOICEREADER_ASSISTANT_TEST_CTX` the context size (default 4096).
    #[tokio::test]
    #[ignore = "needs an installed llama.cpp backend and a downloaded model"]
    async fn assistant_end_to_end_load_health_unload() {
        let root = PathBuf::from(std::env::var("VOICEREADER_ASSISTANT_TEST_ROOT").expect("set VOICEREADER_ASSISTANT_TEST_ROOT"));
        let model_id = std::env::var("VOICEREADER_ASSISTANT_TEST_MODEL").unwrap_or_else(|_| "minicpm-v-4.7-1b".to_string());
        let ctx_size = std::env::var("VOICEREADER_ASSISTANT_TEST_CTX")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(4096);
        let state = AssistantState::new(root.clone());
        let status = state.status();
        assert!(status.backend_installed, "backend not installed under {}", root.display());

        state.load(&model_id, ctx_size, &|| {}).await.expect("model loads");
        let status = state.status();
        assert_eq!(status.server.state, ServerState::Ready);
        println!("device: {:?}", status.server.device_label);
        let device = status.server.device_label.clone().unwrap_or_default();
        assert!(!device.is_empty() && device != "Unknown device", "device label: {device}");
        let endpoint = state.endpoint().expect("endpoint while ready");
        assert!(endpoint.base_url.starts_with("http://127.0.0.1:"));
        assert_eq!(endpoint.model_id, model_id);
        let health = loopback().get(format!("{}/health", endpoint.base_url)).send().await.unwrap();
        assert_eq!(health.status(), reqwest::StatusCode::OK);
        let unauthorised = loopback().get(format!("{}/v1/models", endpoint.base_url)).send().await.unwrap();
        println!("/v1/models without key: {}", unauthorised.status());
        assert!(state.inner.layout.run_record_path().exists());

        // Loading the same model and context again returns at once.
        state.load(&model_id, ctx_size, &|| {}).await.unwrap();
        assert_eq!(state.endpoint().unwrap().base_url, endpoint.base_url);

        state.inner.unload_blocking();
        assert_eq!(state.status().server.state, ServerState::Stopped);
        assert!(state.endpoint().is_none());
        assert!(!state.inner.layout.run_record_path().exists());
        assert!(loopback().get(format!("{}/health", endpoint.base_url)).send().await.is_err());
    }
}
