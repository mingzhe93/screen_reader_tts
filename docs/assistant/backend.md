# Assistant backend: llama.cpp server, catalogue and downloads

This is the Rust side of the v0.3.0 Assistant, Step 1. It installs a pinned llama.cpp
`llama-server`, downloads the pinned models with SHA-256 checks, runs one server on
`127.0.0.1` with an API key, unloads it when idle, and cleans up servers left behind by a
VoiceReader that crashed. The chat page and tool loop use `AssistantState::admit_turn()` to capture the endpoint, generation and use guard atomically. See [tools.md](tools.md) for turn cancellation and draining.

Plan sections implemented (`docs/v0.3.0_assistant_plan.md`):

- §3.1: load and unload, a "loading" state while `/health` returns 503, the device label once ready, unload after 10 minutes idle, one model at a time.
- §4.1 and §4.2: one pinned build (`b11718`) per OS, with its asset URL, size and SHA-256 compiled in.
- §4.3: staging directory, cancellation, verify then extract, rename into a versioned directory.
- §4.4: binds `127.0.0.1` on a free port, uses an app-generated API key, waits for `/health`, captures the log, notices when the process exits, and stops the server on app exit. Also orphan cleanup with an owner PID (Codex F5) and a Windows Job Object.
- §5.1: the curated catalogue, pinned by Hugging Face revision and per-file SHA-256.
- §5.3: the default context is 32768.
- §5.4: `--no-reasoning-preserve` and `--reasoning-format deepseek`. Each model records its thinking control as data.

## Files

| File | Contents |
|---|---|
| `src-tauri/src/assistant_catalogue.rs` | Pinned backend assets, model catalogue, lookups. No I/O. |
| `src-tauri/src/assistant_backend.rs` | `AssistantState`, install, downloads, server lifecycle, run record, Tauri commands. |
| `src-tauri/src/model_download.rs` | New `download_verified_file` and helpers. `download_model_files` is unchanged. |
| `src-tauri/Cargo.toml` | Adds `sha2 0.10` (optional, in `build-base`) and macOS `libc 0.2` (optional, in `build-base`). Adds windows-sys features `Win32_System_JobObjects`, `Win32_System_Threading` and `Win32_Security`. |

Both new modules are meant to compile only with `build-base`.

## Integration (done)

- `lib.rs` declares `assistant_backend`, `assistant_catalogue`, `assistant_chat` and `assistant_commands`, each behind `#[cfg(feature = "build-base")]`.
- `voicereader_core.rs` `setup` manages `AssistantState::new(<engine data dir>/assistant)`. The engine data dir comes from `resolve_engine_data_dir`, which already strips the Windows `\\?\` prefix. Nothing starts at app launch.
- Commands are registered through `app_command_handler()`, which appends the Assistant commands to the shared `app_commands!` list in the Base build only. Tauri v1's `generate_handler!` cannot take individual commands behind `#[cfg]`, so this lets the deprecated Full build keep compiling.
- The `RunEvent::ExitRequested` / `RunEvent::Exit` hook calls `AssistantState::shutdown()` before the engine shuts down. The call is synchronous: it first drains cancelled tool workers (including any first session Wasm compilation), then allows 2 s for the server to stop.
- The chat commands in `assistant_commands.rs` take a `UseGuard` from `admit_turn()` for the lifetime of each whole turn, so the idle timer never unloads the model mid-answer.

## Catalogue (`assistant_catalogue.rs`)

- `BACKEND_VERSION = "b11718"`. The default context (32K) is chosen by the page (`src/assistant.ts`) and passed to `assistant_load`.
- `backend_asset() -> Option<&PinnedFile>` returns the asset for the running platform. `backend_asset_for(os, arch)` takes the values of `std::env::consts::{OS, ARCH}`. Only `windows/x86_64` (`llama-b11718-bin-win-vulkan-x64.zip`, 32 307 966 bytes) and `macos/aarch64` (`llama-b11718-bin-macos-arm64.tar.gz`, 12 292 559 bytes) are supported. Every other platform gets `None`, and the status reports `platform_supported: false`.
- `PinnedFile { file, size, sha256 }`. `url()` gives `https://github.com/ggml-org/llama.cpp/releases/download/b11718/<file>`.
- `models()` lists the four models in a fixed order (small, mid, mid-high, large), sorted by memory at 32K because the tier recommendation walks it from the end (a test checks the order). `model_by_id(id)` looks one up.
- `CatalogueModel` holds `id`, `label`, `tier` (`small|mid|mid_high|large`), `repo`, `revision` (40-hex commit), `weights`, `mmproj`, `image_token_estimate`, `thinking`, maximum context, sampling defaults and memory coefficients. Tool qualification is attached to the verified model IDs with pinned report tests. `file_url(file)` gives `https://huggingface.co/<repo>/resolve/<revision>/<file>`, never `main`.
- `image_token_estimate` is 512 for all four (measured 329–338 tokens for a 1024×768 image on the first three; MiMo is not measured yet).
- `thinking` is `ThinkingControl::ChatTemplateKwarg("enable_thinking")` for all four. The chat code sends it as `chat_template_kwargs: {"enable_thinking": bool}`. It is an enum so a future model can use another mechanism.

| id | Download total (bytes) |
|---|---|
| `minicpm-v-4.7-1b` | 1 782 121 664 |
| `gemma-4-e4b` | 6 116 679 616 |
| `mimo-v2.6-9b` | 6 759 215 168 |
| `gemma-4-12b` | 7 541 539 200 |

## Verified downloader (`model_download.rs`)

```rust
pub(crate) async fn download_verified_file<F: FnMut(u64)>(
    client: &reqwest::Client, url: &str, expected_size: u64, expected_sha256: &str,
    target: &Path, cancel: &AtomicBool, on_progress: F) -> anyhow::Result<()>
```

- The file is written to `<target>.part`. A later call resumes with `Range: bytes=<have>-`. The part is restarted when the server ignores the range (no 206) or when the part is larger than expected. A download that grows past the expected size is discarded.
- `cancel` is checked at least every 250 ms: while waiting for the response headers (a server that accepts the connection and never answers can still be cancelled) and while data arrives, including when the connection stalls. Cancelling returns `DownloadCancelled` inside the `anyhow::Error`, which displays as "Download cancelled.". Detect it with `err.is::<DownloadCancelled>()`. The `.part` file is kept.
- Once all bytes have arrived, `finalize_part_file(part, target, sha256)` hashes the part in 1 MiB blocks on a blocking thread. On a match it renames the part to `target`. On a mismatch it deletes the part and returns an error naming the file, and `target` is never created.
- If `target` already exists, it is hashed before anything else (review finding F1). A matching file is kept and the download is skipped. A file with the wrong size or hash is deleted and downloaded again, so a corrupted or swapped file never passes on its size alone. This hash only runs when a download is requested. In a debug build, hashing the 1.8 GB small model takes over a minute because `sha2` is unoptimised; release builds are much faster.
- `on_progress(bytes_of_this_file)` is called at the start, at most every 250 ms, and at the end.
- `sha256_file(path)` and `part_path(target)` are also `pub(crate)`.

## On-disk layout

The root is `<engine data dir>/assistant`, beside the app's other models:

- Release builds: `<app local data dir>/data/assistant`. With identifier `com.voicereader.desktop` this is `%LOCALAPPDATA%\com.voicereader.desktop\data\assistant` on Windows and `~/Library/Application Support/com.voicereader.desktop/data/assistant` on macOS.
- Debug builds (`npm run desktop:dev`): `tts-engine/.data/assistant` in the repository.
- `VOICEREADER_DATA_DIR`, when set, replaces the engine data dir in both.

```
<root>/backend/b11718/                   installed backend (the extracted archive)
<root>/backend/b11718/llama-b11718/...   macOS: the archive has this top-level folder
<root>/backend/.staging-b11718/          download and extraction staging
    <asset file>(.part)                  the archive, then extract/ while extracting
<root>/models/<model id>/<revision>/<file>(.part)
<root>/run/server.json                   run record while a server runs
<root>/run/load.lock                     admission lock file (empty)
```

"Backend installed" means `llama-server` (`llama-server.exe` on Windows) exists somewhere in the first four levels under `backend/b11718/`. The directory containing it is used as the working directory when the server starts.

A model is installed when both of its files exist with their pinned sizes. `status()` checks sizes only, because hashing several GB on every status call is too slow. The hash is verified when the file is downloaded, and again whenever `assistant_download_model` runs for an existing file.

`<root>/run/load.lock` is the cross-process admission lock (see "Run record and single owner"). It is never deleted; only the OS lock on it matters.

## Backend install

1. `download_verified_file` downloads the archive into the staging directory.
2. The archive is extracted with the system bsdtar into `staging/extract`: `/usr/bin/tar -xzf` on macOS, or `%SystemRoot%\System32\tar.exe -xf` on Windows, spawned with `CREATE_NO_WINDOW`. bsdtar keeps symlinks and executable bits, and it refuses `..` and absolute member paths by default.
3. If no server binary is found, the extract directory is deleted and an error is returned.
4. Any existing `backend/b11718` is removed, `extract` is renamed into place, and the staging directory is deleted.

Before creating staging and before any `remove_dir_all` or rename, the install checks `<root>`, `<root>/backend`, the staging directory, `staging/extract` and `backend/b11718` with `symlink_metadata`. If any of them is a symbolic link, the install stops with "Refusing to install the Assistant backend: <path> is a symbolic link. Remove it and try again." (review finding F2). Without this check, a symlinked staging directory would let the cleanup delete files outside the root. On Windows, `is_symlink()` also covers junctions.

If extraction fails, only the extract directory is reset. The verified archive stays, so a retry extracts it again without downloading. A cancelled or interrupted download keeps its `.part` file for resume.

The upstream archive contains a `LICENSE` file, which ends up in the install directory (plan §4.3 step 5). The UI does not show it yet.

## Server lifecycle

The command line is built by `server_args()`:

```
llama-server --host 127.0.0.1 --port <free port> --api-key <48 alphanumeric chars>
  -m <weights> --mmproj <mmproj> -c <ctx> --parallel 1 --jinja
  --reasoning-format deepseek --no-reasoning-preserve -lv 4
```

`-ngl` and `--fit` are not passed, because llama.cpp's default `--fit` decides the offload.

`-lv 4` is an addition to the ticket's command line. At the default verbosity, build b11718 prints only about 9 lines and no device or offload information, which I checked on macOS. At level 4 it prints the Metal/Vulkan device, `using device ...` and `offloaded N/M layers`, plus about 26 lines of slot and timing information per request. It does not log prompt or answer text, and it masks the API key as `****<last 4>`.

- **Log:** stdout and stderr are read line by line on two threads into memory: the last 500 lines, plus the first 400 lines for the startup report. The full API key is replaced with `<api-key>` in every captured line. Error messages include the last 20 lines.
- **Readiness:** `GET /health` (with the bearer key; llama.cpp does not require it there) is polled every 250 ms until it returns 200. 503 means the model is still loading. The client used for it is built with `.no_proxy()`, so the key never goes through `HTTP_PROXY` or a system proxy (review finding F7). The download client keeps proxy support. If the process exits during startup, the load fails with its exit status and the log tail. The load times out after 300 s.
- **Device label:** `parse_device_label()` reads the startup log. In order of preference: `offloaded 0/N` gives `CPU`; then `using device <Backend><n> (<description>)` gives `<Backend>: <description>`, with `MTL` renamed to `Metal`; then `ggml_metal ... GPU name:` or `ggml_vulkan: <i> = <name> (...)` when layers were offloaded; then CPU-only buffers give `CPU`. Anything else gives `Unknown device`. Partial offload appends ` (n/m layers)`. Examples: `Vulkan: NVIDIA GeForce RTX 5090`, `Metal: Apple M2 Max`, `CPU`.
- **`load(model_id, ctx_size)`:**
  - It checks the model id, `512 <= ctx_size <= 1048576`, the platform, the backend and the model files, in that order.
  - Loads are serialised by an async lock. If a ready server already has the same model and context, the call returns at once.
  - Otherwise it takes the cross-process admission lock, stops our current server, checks the run record (next section), spawns, writes the run record, releases the lock, and waits for readiness. The lock is not held during the `/health` wait.
  - If `unload` runs during the wait, the load fails with "Loading was cancelled.".
- **`unload`:** invalidates the generation, cancels and drains tool turns, then stops the child and deletes the run record, but only if the record still names this process and that server's PID (below). On macOS this is SIGTERM, then SIGKILL after 3 s. On Windows it is `TerminateProcess` straight away: the server has no console, so there is nothing to send Ctrl+C to.
- **Idle unload:** after the first successful load, a task checks every 30 s. It unloads when the state is `ready`, no `UseGuard` is alive, and nothing has happened for 10 minutes. The check and the takeover happen under the same lock as `admit_turn()`, so a request that has just started cannot lose its server.
- **Crash while ready:** `status()` and `endpoint()` notice that the process has exited. They move the state to `error` with the log tail and remove the record if it is still ours. That removal only tries the lock once and does not wait for it.
- **`shutdown()`:** sets the download cancel flag, stops the child (2 s grace) and removes that server's run record, waiting at most 1 s for the lock. With no server running it does not touch the record, which may belong to another instance. It is synchronous, for the exit hook.

### API for the chat module

- `state.endpoint() -> Option<Endpoint>` returns `Endpoint { base_url: "http://127.0.0.1:<port>", api_key, model_id, ctx_size }`. It returns `None` unless the state is `ready`. Its `Debug` output hides the key.
- `state.admit_turn() -> Result<(Endpoint, UseGuard, u64)>` captures the endpoint, use lease and generation atomically. Hold the guard through every model/tool round. Dropping it records activity. `is_generation_ready(generation)` rejects stale turns; unload/crash/replacement/exit cancel their registered workers.

## Run record and single owner (plan §4.4, Codex F5)

`<root>/run/server.json`:

```json
{"server_pid": 1234, "exe_path": "<canonical path of llama-server>", "started_at_unix_ms": 1760000000000, "owner_pid": 5678}
```

It holds no API key. Before spawning, `decide_run_record(record, own_pid, probe)` decides what to do:

| Record | Decision |
|---|---|
| none | start normally |
| server PID alive, its executable matches `exe_path`, owner alive and not this process | `in_use_elsewhere`: stop nothing, start nothing |
| server PID alive and matches, owner dead or this process | stop that server (orphan), remove the record, start |
| server PID dead, or alive with another executable (PID reused), or its path cannot be read | leave the process alone, remove the record, start |

One refinement to the ticket: a record whose server is dead is discarded even if its owner PID is alive. The ticket blocks whenever the owner is alive. With that rule, a crashed owner whose PID has been reused by an unrelated program would block the Assistant forever on macOS. If the server is not running, there is nothing to protect.

Paths are compared case-insensitively on Windows, ignoring a `\\?\` prefix. On macOS they are compared exactly or after canonicalising. Process probing goes through the `ProcessProbe` trait: `libc::kill(pid, 0)` and `proc_pidpath` on macOS, `OpenProcess`, `GetExitCodeProcess` and `QueryFullProcessImageNameW` on Windows. The tests use a fake.

### Admission lock (review finding F5)

Two VoiceReader processes could otherwise both read "no record" and both start a server. Admission is therefore serialised with an exclusive OS file lock (`std::fs::File::try_lock`, stable since Rust 1.89; `flock` on macOS, `LockFileEx` on Windows) on `<root>/run/load.lock`.

- A load holds the lock from reading the record, through stopping an orphan and spawning, until the new record is written. It does not hold it during the `/health` wait.
- A load waits up to 30 s for the lock, then fails with "Another VoiceReader window is starting the Assistant. Try again in a moment.".
- The OS releases the lock if the process dies, so a crash never leaves it held.
- Lock order is lock file first, then the in-process slot mutex. Code holding the slot mutex never takes the file lock.

### Record removal (review finding F4)

- Unload, idle unload, shutdown, a failed load and a crashed server remove `server.json` only when it names **this process** as owner **and** the PID of the server this instance just stopped. A late unload therefore never deletes a newer server's record.
- Each removal takes the admission lock: unload waits up to 5 s, shutdown 1 s, a crash detection tries once. If the lock stays busy, the record is left; the next load discards it once its server is gone.
- During admission, a load removes stale records (orphan stopped, server dead, PID reused) only if the file still equals the record it read.

`status()` also derives `in_use_elsewhere` from the record whenever this instance has no server. A second window therefore shows the state as soon as it opens. When another instance owns the server, `assistant_load` emits that status and returns the error "The Assistant is in use in another VoiceReader window.".

On Windows, each server is assigned to a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. The handle is kept for the server's lifetime, so Windows ends `llama-server.exe` when VoiceReader dies. If assigning the job fails, the server still runs and the log gets a `[VoiceReader] Job Object not applied: ...` line.

## Tauri commands

JS argument names are camelCase. Payloads are snake_case. Errors are readable strings.

| Command | JS args | Returns |
|---|---|---|
| `assistant_status` | none | `AssistantStatus` |
| `assistant_install_backend` | none | `AssistantStatus` once installed (returns at once if already installed) |
| `assistant_download_model` | `modelId: string` | `AssistantStatus` once both files are verified |
| `assistant_cancel_download` | none | `null`. The running download command then fails with `"Download cancelled."` |
| `assistant_load` | `modelId: string, ctxSize: number` | `AssistantStatus` once ready, or an error |
| `assistant_unload` | none | `AssistantStatus` |
| `assistant_delete_model` | `modelId: string` | `AssistantStatus` after deleting `<root>/models/<id>/` (every revision) |
| `assistant_diagnostics` | none | `AssistantDiagnostics`: version, package, paths, server status, PID, port, uptime, and the last 300 log lines (of the running server, or of the last one after it stopped). Never the API key. |
| `assistant_list_devices` | none | Output of `llama-server --list-devices` with its exit status. Starts no server; gives up after 20 s. |

`assistant_delete_model` holds the download slot and the in-process load lock while it runs, so no download or load can start meanwhile. It refuses while any Assistant download runs, while the model is answering a chat request, and while another VoiceReader window runs the Assistant. A model loaded in this window is unloaded first. A load that was waiting for the lock re-checks that the model is still installed.

`assistant_load` also refuses a context above the model's trained limit (`max_context`: MiniCPM 256K, Gemma 4 E4B 128K, MiMo-V2.6 9B 256K, Gemma 4 12B 256K).

Only one download (backend or model) runs at a time. A second request fails with "Another Assistant download is already running.".

```json
{
  "platform_supported": true,
  "backend_installed": true,
  "backend_version": "b11718",
  "backend_download_bytes": 12292559,
  "models": [{"id": "minicpm-v-4.7-1b", "label": "MiniCPM-V 4.7 1B", "tier": "small", "download_bytes": 1782121664, "installed": true,
              "max_context": 262144, "memory_estimate": {"ctx_size": 32768, "gpu_mib": 2123, "host_mib": 176, "measured": true}}],
  "download": null,
  "server": {"state": "ready", "model_id": "minicpm-v-4.7-1b", "ctx_size": 32768, "device_label": "Metal: Apple M2 Max", "error": null}
}
```

- `server.state` is one of `stopped|starting|ready|in_use_elsewhere|error`.
- `download` is `null` or `{"kind": "backend|model", "id": "<model id or b11718>"}`.

Events, emitted with `emit_all`:

- `assistant:status` carries an `AssistantStatus`. It is emitted when a download starts and ends, when a load starts, becomes ready or fails, after an unload or idle unload, and when another instance blocks a load.
- `assistant:download-progress` carries `{"kind": "backend|model", "id": "...", "file": "<file name>", "downloaded": <bytes across the item>, "total": <bytes>}`, at most every 250 ms for each file. For a model, `downloaded` counts both files.

## Tests

Run these from `src-tauri` with the stable toolchain. The tests compile only once the two `mod` lines from the Integration section are in `lib.rs`.

```bash
cd src-tauri
cargo test --features build-base assistant_          # 40 tests: catalogue and backend
cargo test --features build-base model_download::    # 11 tests, plus 2 ignored
```

What the unit tests cover:

- catalogue order, lookups, 40-hex revisions, 64-hex hashes, URLs pinned to the revision, totals, and the platform table
- SHA-256 against a known test vector and across multiple blocks
- finalize on match, on replacement and on mismatch (part deleted, no target)
- an existing target with the right hash is skipped; one with the right size but wrong hash (`abd` against the hash of `abc`) is deleted and downloaded again from a loopback mock server; a bad target is deleted even if the new download fails
- cancellation keeps the part; cancelling while a mock server accepts and never answers returns `DownloadCancelled` within about 250 ms
- the run-record decisions: no record; owner alive elsewhere; owner dead with a matching server; owner is this process; PID reused; server dead; path unreadable
- `SystemProbe` on the test process itself
- run-record round trip; a record that changed since it was read is not removed
- `shutdown()` on a fresh state leaves a foreign record and an own record naming another server
- unload with a real stand-in child (`sleep`, or `PING.EXE` on Windows) leaves a record naming another server PID and removes the one naming its own
- admission: a second state sharing the root cannot take the lock or remove records while the first holds it and has written a live record; once released, admission reports `in_use_elsewhere` with a fake probe, and discards a record whose server is gone
- symlinks (unix): a symlinked staging dir, root or backend dir is refused, and sentinel files in the external directory survive
- server arguments, API key shape
- binary lookup in nested and flat directories
- failed extraction installs nothing; `tar.gz` extraction on macOS
- layout paths
- device-label parsing: Vulkan, partial offload, Metal (including real b11718 lines), CPU, unknown
- log buffer bounds
- the idle rule and `UseGuard` counting
- one download at a time
- JSON field names
- `in_use_elsewhere` derived from a record owned by a live child process
- load precondition errors

Ignored tests, which need the network or a real server:

```bash
# Download and verify the pinned macOS archive (12 MB). Any OS.
cargo test --features build-base model_download::tests::verified_download_fetches -- --ignored

# Install the backend and a model into a scratch root through the app's code.
# MODEL defaults to minicpm-v-4.7-1b (1.8 GB); "none" installs only the backend.
VOICEREADER_ASSISTANT_TEST_ROOT=<dir> cargo test --features build-base \
  assistant_backend::tests::assistant_install_into_test_root -- --ignored --nocapture

# Load, check /health is 200 and the device label is known, load again (no restart), unload.
VOICEREADER_ASSISTANT_TEST_ROOT=<dir> cargo test --features build-base \
  assistant_backend::tests::assistant_end_to_end -- --ignored --nocapture
# Optional: VOICEREADER_ASSISTANT_TEST_MODEL=<id>, VOICEREADER_ASSISTANT_TEST_CTX=<n> (default 4096)
```

Windows PowerShell form:

```powershell
$env:VOICEREADER_ASSISTANT_TEST_ROOT = "$env:TEMP\vr-assistant"
cargo test --features build-base assistant_backend::tests::assistant_install_into_test_root -- --ignored --nocapture
cargo test --features build-base assistant_backend::tests::assistant_end_to_end -- --ignored --nocapture
```

On macOS (M2 Max), all three ignored tests passed. They downloaded and verified the backend archive and both MiniCPM files. `llama-server --version` reported build 11718. The load became ready in about 1 s with label `Metal: Apple M2 Max`. `/v1/models` without the key returned 401. After unload, the record was gone and the port was closed.

After the review fixes, I re-ran the install test (existing files are now hashed and kept) and the end-to-end test on macOS. Both passed, and `run/` held only `load.lock` afterwards.

By hand on macOS, I also checked two run-record cases with a real `llama-server`:

- a record with a dead owner: the server was stopped and the load went ahead
- a record whose owner was a live shell: the load failed with the in-use message and the other server kept running

## Manual checks on Windows (not run; no Windows machine was available)

The Windows-only code (the Job Object, process probes, `tar.exe` and `CREATE_NO_WINDOW`) compiles and works on Windows (2026-10-20). If a later change breaks it, run `cargo build --features build-base` and fix any compile error in the `#[cfg(target_os = "windows")]` blocks of `assistant_backend.rs`.

1. **Unit tests:** `cargo test --features build-base assistant_` and `model_download::`. Expected: all pass, including `assistant_system_probe_sees_this_process` and `assistant_status_reports_in_use_elsewhere_from_the_record`, which use real Windows process APIs, `assistant_unload_removes_only_the_record_of_the_server_it_stopped`, which spawns `PING.EXE` as a stand-in server, and `assistant_admission_is_exclusive_across_instances`, which checks that `LockFileEx` conflicts between two handles in one process. The symlink tests are unix-only.
2. **Vulkan zip installs and runs:** run the install test and then the end-to-end test above.
   - Expected: `llama-server.exe` is under `<root>\backend\b11718\` (the zip is flat) and `.staging-b11718` is gone.
   - `--version` prints build 11718.
   - The load becomes ready with a label like `Vulkan: NVIDIA GeForce RTX 5090`.
   - No console window appears for tar or the server.
   - Then repeat from the integrated app UI.
3. **CPU fallback with the GPU hidden:** three ways to try it.
   - Most reliable: rename `ggml-vulkan.dll` in the install directory. The build loads backends dynamically, so only the CPU backend is left. Rename it back afterwards.
   - Alternative: set `GGML_VK_VISIBLE_DEVICES` to an index that does not exist before starting the test or the app. llama-server inherits the environment. Whether `-1` hides all devices is unverified, so check the log.
   - Manual run: start the installed `llama-server.exe` by hand with the arguments above plus `--device none`.

   Expected: the load still becomes ready, and the label is `CPU` (from `offloaded 0/N layers`). Write down which method worked.
4. **Job Object:** load a model in VoiceReader, then end `VoiceReader.exe` from Task Manager (End task).
   - Expected: `llama-server.exe` disappears from Task Manager within a few seconds.
   - `run\server.json` remains. The next VoiceReader start and load discards it silently (server PID dead) and loads normally.
   - If the server survives, check the log for `Job Object not applied`.
5. **Second instance:** with a model loaded in the first VoiceReader, start a second one and open its Assistant page.
   - Expected: `assistant_status` reports `in_use_elsewhere`, and Load fails with "The Assistant is in use in another VoiceReader window.".
   - The first instance's server keeps answering, and its PID is unchanged.
   - Close the first instance and press Load in the second. Expected: it loads.
   - Race: with no server running, press Load in both windows at about the same time. Expected: exactly one `llama-server.exe` starts. The other window reports `in_use_elsewhere`, or "Another VoiceReader window is starting the Assistant" if it waited more than 30 s.
   - Unload in the second window (which has no server) must not delete the first window's `run\server.json`.
6. **SmartScreen and Defender:**
   - VoiceReader downloads with reqwest, which does not add the Mark of the Web. `Get-Item <path>\llama-server.exe -Stream *` should list no `Zone.Identifier`, and no SmartScreen prompt should appear at first launch.
   - Note any Defender scan delay on the first start, which shows as a slow first load, and any quarantine.
   - Note any AppLocker or policy block on executables under `%APPDATA%`.
7. **Cancel and resume:** start a model download, cancel it part way, then start it again.
   - Expected: the first command fails with "Download cancelled.".
   - `<file>.part` remains, and the second run continues from that size instead of restarting. Progress jumps to the previous value.

## macOS regression checklist

I have tested and accepted the current macOS app. Keep these lifecycle checks for regression testing; force-quit/orphan edge cases are not individually signed off by that acceptance.

- Install from the UI.
- Check `xattr -l <root>/backend/b11718/llama-b11718/llama-server` shows no `com.apple.quarantine`. The binary ran without a Gatekeeper prompt in the tests.
- Quit the app while a model is loaded. Expected: no `llama-server` remains (`pgrep llama-server`).
- Force-quit the app (`kill -9`) while loaded. Expected: the server keeps running as an orphan (macOS has no Job Object), and the next load stops it.

## Known gaps and next steps

- No disk-space check before a download. Plan §4.3 step 1 asks to show the required downloads and space. The status provides the sizes; the UI shows them.
- Hashing after a download (up to about 15 s for the large model) sends no progress and cannot be cancelled. The UI sits at 100% during it.
- There is no fallback to the last working context size when a load fails (plan §5.3). The caller gets the error and the log tail.
- On Windows, stopping is immediate termination, not a graceful exit.
- The API key is visible on the server's command line to other processes of the same user. llama.cpp also has `--api-key-file` if that ever matters.
- Memory estimates and `--list-devices` hardware detection are implemented. MiMo coefficients are estimated rather than measured; the fit rule still omits the additional ASR/TTS allowance. Windows device labels and RAM/VRAM splits still need native validation.
- The licence notice is installed but not shown in the UI.
- Next: the Windows checks above, including tools/runtime packaging and voice integration. Push-to-talk, spoken replies, automatic sending and tools are already integrated; their guides are linked from [README.md](README.md).
