# Assistant (v0.3.0) handover

Status on 2026-10-19: **all planned v0.3.0 Assistant functionality is implemented and uncommitted**: Steps 1–5 of the order of work in [the plan](../v0.3.0_assistant_plan.md) §10, plus the redesigned page and model management. The app version is `0.3.0-testing`. Everything has been built and unit-tested on macOS (Apple Silicon). The text chat has been run in the real app, and the voice path has been exercised against a mocked backend and with real models in Rust tests. **Nothing has been compiled or run on Windows, and the voice conversation has not yet been tried end to end in the real app.** This document is the starting point for that testing, on Windows and macOS, and for continuing with Claude there.

Read these in order:

1. [The plan](../v0.3.0_assistant_plan.md): every product and technical decision, with the ones accepted on an AI recommendation marked *(AI-recommended)*.
2. This file: what exists, how to build and test it, and what to check first.
3. The area docs:
   - [backend.md](backend.md): downloads, llama-server lifecycle, run record, Windows Job Object.
   - [chat.md](chat.md): the streaming bridge and context fitting.
   - [ui.md](ui.md): the page layout, panel, safe Markdown, images, Models page.
   - [voice-input.md](voice-input.md): the microphone session and turn order.
   - [turn-detection.md](turn-detection.md): VAD and Smart Turn, with parity results.
   - [phrases.md](phrases.md): the phrase buffer and speakable filter.
   - [conversation.md](conversation.md): voice mode rules and manual checks.

## What exists

**Assistant page** (sidebar group "Assist"; Base build only), laid out like LM Studio:
- A top bar: model picker (installed models), status pill, Load/Eject, a **Chat | Voice** switch, and the settings-panel toggle.
- The conversation fills the page; the composer is pinned at the bottom.
- A collapsible right panel:
  - context length (8K–256K, capped at each model's trained limit) with an estimated memory fit;
  - the **Chat** and **Voice** system prompts (saved by Rust in the app settings file);
  - Think (chat only);
  - temperature, plus advanced sampling, defaulting to each model's published recommended values and stored per model;
  - in voice mode, the experimental auto-send toggle.

**Chat:**
- Answers stream as Markdown that never becomes HTML.
- Reasoning is collapsed, never spoken, and never sent back to the model.
- Stop, Speak (read-aloud), Copy, and up to 4 images per message.
- **Push-to-talk** (hold, or click to toggle) transcribes into the composer; you press Send.

**Voice mode:**
- A large talk button and a transcript pane.
- Release to send; the answer is spoken phrase by phrase while it streams, with thinking off and the Voice prompt.
- Pressing during an answer interrupts it; history keeps only what was heard, then `[interrupted by user]`.
- A finished transcript that arrives while a newer turn is recording is joined to it (plan §7.2, F7).
- Read-aloud preempts a voice answer.
- A stall watchdog recovers from stuck playback.
- **Auto-send (experimental):** Silero VAD plus Smart Turn v3.2 ends the turn when you stop talking. Its model files (about 11 MB) download the first time it is switched on.

**Models page**, in three groups:
- **Text to speech:** Kyutai (bundled), Audio8 (Download/Delete).
- **Speech recognition:** Transcription (Download/Delete).
- **Assistant:** the llama.cpp backend install, plus the four models, each with Download, Cancel and Delete, a Memory tooltip, a fit dot and a **Recommended** badge from hardware detection.

**Settings → Assistant diagnostics:** backend paths, server state, PID/port/uptime, the server log, Check devices (`--list-devices`) and Copy. The API key never appears.

**Backend safety:**
- Pinned and hash-verified downloads, with staged installs.
- llama-server listens on loopback only and requires an API key.
- Orphan cleanup with the owner's PID and a cross-process load lock.
- A Windows Job Object.
- The microphone and Transcribe share the one ASR slot: starting a transcription closes the Assistant's microphone, and the microphone refuses to open while a transcription runs.

## Where the code is

| Area | Files |
|---|---|
| Backend, catalogue, hardware | `src-tauri/src/assistant_backend.rs`, `src-tauri/src/assistant_catalogue.rs`, `download_verified_file` in `src-tauri/src/model_download.rs` |
| Chat bridge | `src-tauri/src/assistant_chat.rs` (system prompt, sampling, context fitting, SSE) |
| Glue, prompts, dropped images | `src-tauri/src/assistant_commands.rs` |
| Microphone session | `src-tauri/src/assistant_voice.rs` (capture thread with in-band turn boundaries, ASR worker); `InputStream` in `src-tauri/src/audio_capture.rs` |
| Turn detection | `src-tauri/src/assistant_turn.rs` (Silero VAD, Smart Turn, Whisper log-mel); runs inside the capture thread; fixtures in `src-tauri/tests-data/smart_turn/`; reference generator `scripts/smart_turn_reference.py` |
| Spoken answers | `src-tauri/src/assistant_phrases.rs` (phrase buffer), `src-tauri/src/assistant_conversation.rs` (answer → phrases → speech, backpressure), `AssistantSpeechJob` in `src-tauri/src/voicereader_core.rs` |
| Microphone and model commands | `assistant_mic_*`, `delete_audio8_model` and `delete_asr_model` in `src-tauri/src/voicereader_core.rs` (with `app_command_handler()` and the setup and exit wiring) |
| Player feedback | `src/player.ts` (streaming policy, phrase acknowledgements), `reportAssistantPlayback` and `PHRASE_DONE` handling in `src/main.ts` |
| Page | `src/assistant.ts`, `assistant-voice.ts`, `assistant-turns.ts` (pure turn-taking logic), `assistant-mic.ts`, `assistant-models-page.ts`, `assistant-diagnostics.ts`, `assistant-format.ts`, `assistant-hardware.ts`, `assistant-sampling.ts`, `assistant-prompts.ts`, `assistant-dom.ts`, `assistant-markdown.ts`, `assistant-images.ts`, `confirm-button.ts` |
| Tests | Rust unit tests in each module (229 pass, 19 ignored that need models or the network); `npm run test:assistant`, `test:assistant-fit`, `test:assistant-voice`, `test:player` |
| Dependencies | `sha2`, `realfft` (all platforms), `libc` (macOS), extra `windows-sys` features (Windows). No npm dependencies. |

Data lives under `<engine data dir>/assistant` (backend, models, turn-detection models in `turn/`, run record). In a debug build that is `tts-engine/.data/assistant`; in a release build it is `%LOCALAPPDATA%\com.voicereader.desktop\data\assistant` on Windows and `~/Library/Application Support/com.voicereader.desktop/data/assistant` on macOS. Deleting that folder resets the Assistant completely.

## Build and test on Windows

Prerequisites are the ones in the repository [README](../../README.md) (Rust, Node, CMake via `winget install --id Kitware.CMake -e`, and the Visual Studio C++ build tools).

```powershell
npm install
npm run assets:fetch
cd src-tauri
cargo build --features build-base
```

**Expect this first build to need small fixes.** The `#[cfg(target_os = "windows")]` blocks in `assistant_backend.rs` (Job Object, `OpenProcess` / `QueryFullProcessImageNameW` / `TerminateProcess`, `tar.exe`, `CREATE_NO_WINDOW`) were written against `windows-sys` 0.59 but have never been compiled. Fix compile errors only inside those blocks, and keep the behaviour described in [backend.md](backend.md).

Then run the automated tests. All should pass; the network and end-to-end tests are `#[ignore]` and are covered next.

```powershell
cargo test --features build-base assistant_
cargo test --features build-base model_download::
cd ..
npm run test:assistant
npm run test:player
npx tsc --noEmit
```

Real end-to-end test of the backend, without the UI (downloads 32 MB plus about 1.8 GB for the small model):

```powershell
cd src-tauri
$env:VOICEREADER_ASSISTANT_TEST_ROOT = "$env:TEMP\va-assistant"
cargo test --features build-base assistant_backend::tests::assistant_install_into_test_root -- --ignored --nocapture
cargo test --features build-base assistant_backend::tests::assistant_end_to_end -- --ignored --nocapture
```

Expected: both hashes match, the server reports build 11718, `/health` returns 200, the device label names your GPU (for example "Vulkan: NVIDIA GeForce RTX 5090") and unload closes the port.

Then run the app with `npm run desktop:dev` and work through the manual checks.

## Manual checks on Windows, in priority order

Each check is described in detail in the area doc named.

1. **Backend install and load** ([backend.md](backend.md)): install from the page, download the small model, and confirm it loads and the pill shows a Vulkan device.
2. **CPU fallback** ([backend.md](backend.md)): hide the GPU, as described there, and confirm the model still loads, on CPU.
3. **Job Object** ([backend.md](backend.md)): with a model loaded, end VoiceReader in Task Manager. `llama-server.exe` must disappear too.
4. **Second instance** ([backend.md](backend.md)): start a second VoiceReader while the first has a model loaded. The second must say the Assistant is in use in another window, and the first must keep working.
5. **Orphan cleanup** ([backend.md](backend.md)): if the Job Object fails, kill VoiceReader and start it again. The leftover server must be stopped on the next load.
6. **SmartScreen/Defender** ([backend.md](backend.md)): note whether the downloaded `llama-server.exe` is blocked or delayed.
7. **Chat** ([ui.md](ui.md)): Think on and off, Stop, the too-long-message error (paste a very long text at 8K context), Speak, Copy, New chat.
8. **Model management:** delete each kind of model from the Models page and confirm the refusals above. On Windows in particular, delete Audio8 after using it and then switching Read aloud to Kyutai. Windows locks open files, so this checks that the runtime was really released.
9. **Diagnostics:** Settings → Assistant diagnostics → Check devices should list the Vulkan GPU. If the model runs on CPU, this is the first place to look.
10. **Images** ([ui.md](ui.md)): attach, paste, and drop a file from Explorer. Drops go through Tauri's `tauri://file-drop` and the `assistant_read_dropped_image` command.
11. **Hostile output** ([ui.md](ui.md)): ask the model to repeat `<img src=x onerror=alert(1)>` and a `javascript:` link verbatim. Both must appear as text, and nothing may run.
12. **Device label** ([backend.md](backend.md)): the Vulkan and CPU log patterns were reconstructed, not observed. If the pill says "Unknown device", copy the server log lines that name the device into a test and fix `device_label`.

What has been verified on macOS (M2 Max):
- The backend end-to-end test passed ("Metal: Apple M2 Max").
- With the pinned b11718 server and the small model, run with the app's arguments:
  - `/apply-template` returns `{"prompt": ...}` and `/tokenize` returns `{"tokens": [...]}`, the shapes `assistant_chat.rs` expects;
  - with thinking on, reasoning streams as `reasoning_content`;
  - with `reasoning_effort: "none"` and `enable_thinking: false`, no reasoning arrives;
  - an image sent as a PNG data URL is understood (a 64×64 test image used about 80 prompt tokens);
  - requests without the API key get 401.

The app's UI has not been run against the real backend on either platform.

## Model management, memory estimates and diagnostics (added after Step 1)

- **Models page.** Audio8 and Transcription have a **Delete** button once downloaded. The Assistant group installs the backend and downloads, cancels and deletes the models (the Assistant page no longer downloads anything). Delete needs two separate clicks at least 0.5 s apart (a double-click does nothing); the first arms the button for 4 s.
- **When a delete is refused.**
  - Audio8: while it downloads, while it is the selected Read aloud model (switch to Kyutai first), or while a read-aloud job still holds it.
  - Transcription: while it downloads, or while any transcription, including a voice-clip one, is running. The ASR job slot is held during the delete.
  - Assistant: see [backend.md](backend.md).
  - Only `<models>/<org>/<repo>` (or `assistant/models/<id>/`) is removed, never through a symlink. An empty publisher folder is removed too.
- **Memory estimates.** Hover the **Memory** badge on an Assistant model for the estimate at 32K context. The figures come from llama-server b11718's own buffer report on an M2 Max at 8K and 32K. Weights, KV cache, compute buffers and the projector's worst case are included, split into GPU and host memory:

| Model | GPU (VRAM) | System RAM | Total | Source |
|---|---|---|---|---|
| MiniCPM-V 4.7 1B | 2.1 GB | 0.2 GB | 2.2 GB | measured; hybrid model, only 6 attention layers |
| Gemma 4 E4B | 4.9 GB | 2.3 GB | 7.2 GB | measured; the per-layer embedding tables stay in RAM |
| MiMo-V2.6 9B | 7.2 GB | 0.6 GB | 7.8 GB | **calculated** from its GGUF layout, calibrated on MiniCPM (same `qwen35` hybrid: 8 of 32 layers use attention); measure it next |
| Gemma 4 12B | 8.4 GB | 0.7 GB | 9.1 GB | measured (an earlier layout-based estimate was within 2%) |

  The coefficients live in `assistant_catalogue.rs` (`MemoryEstimate`, linear in context). The panel's fit verdict and the Models page's dot and Recommended badge use them with the detected RAM and GPUs. The Recommended badge only considers measured models, so MiMo is never recommended until it is measured (a 16 GB Mac would otherwise sit 2.6% inside the budget on a guess). On Windows, check the GPU/RAM split on Vulkan against the server log's `buffer size` lines.
- **Context lengths.** The gear panel offers 8K to 256K. Options above the selected model's trained limit (Gemma 4 E4B: 128K) are disabled with a tooltip. A stored setting above the limit loads at the limit, and the panel says so.
- **Settings → Assistant diagnostics (llama.cpp):** backend version, package, binary path, Assistant folder, server state, model, context, device, PID, port and uptime, plus the server log (the last one is kept after the server stops). There are **Check devices** (`llama-server --list-devices`, handy for checking that Vulkan sees the GPU on Windows) and **Copy** buttons. The API key never appears.

## Step 5 measurements (macOS, M2 Max, llama-server b11718)

- **Memory:** the table above, at 8K and 32K context.
- **Image cost:** a 1024×768 image (the page downscales to at most 1024 px) costs 329 tokens on MiniCPM and 338 on both Gemma 4 models. `image_token_estimate` is 512, with margin. MiniCPM's `-4x` projector costs 1,273 tokens for the same image, with slightly less memory. It stays out of the catalogue (images would fill the context about 4× faster) as a possible "fine detail" option later.
- **Constrained profile** (CPU only, 4 threads, 4K context, a short question): first token after 0.07 / 0.38 / 0.76 s, decoding at 90 / 21 / 12 tokens per second (MiniCPM / E4B / 12B). On Metal: 0.03 / 0.44 / 0.21 s and 138 / 58 / 29 tokens per second. The M2 Max CPU is much faster than typical friends-and-family laptops, so treat these as a lower bound.
- **Voice input:** release-to-final-text latency of 270–410 ms on 6 threads (325–506 ms on 4); [voice-input.md](voice-input.md).
- **Turn detection:** Smart Turn in 21 ms per call (2 threads, release build), VAD in 0.09 ms per 32 ms frame; [turn-detection.md](turn-detection.md).
- **Hardware detection:** RAM from the OS, GPUs from `llama-server --list-devices`. The recommended tier is the largest model that passes the memory rule at 32K (50% of RAM on unified memory, CPU-only or integrated graphics; GPU memory minus 1 GB, plus RAM, on a discrete GPU).

## Decisions made during implementation (please review)

These came up while building Step 1 and are not in the plan's decision list:

- **llama-server runs with `-lv 4`.** At the default verbosity, build b11718 prints no device or offload lines, so the device label could never be found. Level 4 adds about 26 log lines per request, and logs no prompt text. The log is kept in memory only (last ~500 lines).
- **Run-record refinement.** If the recorded server process is gone, or its PID now belongs to another program, the record is discarded even when the recorded owner PID is alive. That PID may have been reused after a crash.
- **Windows stop is immediate** (`TerminateProcess`). macOS sends SIGTERM, then SIGKILL after 3 s.
- **Chat history alternates strictly.** An exchange whose answer failed is left out entirely, so two user messages never follow each other; some chat templates reject that. An answer ended by Stop is kept with `[stopped by user]`.
- **Desktop file drops** are read in Rust from the dropped path (PNG, JPEG or WebP by file signature, at most 25 MB). Transcribe already uses Tauri's native file drop, so it stays on.
- **The context size is chosen by the page** (32K default) and validated in Rust (512 to 1,048,576 tokens).
- **`sha2` is optimised in debug builds** (`[profile.dev.package.sha2]` in `Cargo.toml`). Unoptimised hashing took 75 s for the 1.8 GB model and 4.6 s optimised, which matters when testing with `npm run desktop:dev`.
- **Proc macros and build scripts are not stripped in release builds** (`[profile.release.build-override] strip = false`). With Xcode 27 (`ld-27037`, macOS 27.0.1), Cargo's default release stripping left some proc-macro dylibs with a "mis-aligned LINKEDIT string pool" that dyld refused to load, and rustc reported this as `can't find crate for ...`. The app binary is still stripped. This is a macOS toolchain issue; it should not affect Windows.
- **The command list** is built with an `app_commands!` macro, so the Base build can add the Assistant commands while the deprecated Full build still compiles (`cargo check --features build-full` passes).

## Cross-model review of the Step 1 code (2026-10-18)

The code was written by Claude agents: Opus for the backend, Sonnet for the chat bridge and the page, Opus for integration. Codex `gpt-6.1-sol` (`xhigh`) then reviewed it read-only. It reported 12 findings. Each was checked against the source; 11 were accepted and fixed with tests, and 1 was rejected:

| Finding | Fix |
|---|---|
| F1: an existing file of the right size counted as verified | The existing target is hashed before it is skipped; a mismatch is deleted and downloaded again |
| F2: staging cleanup could follow a symlinked directory | Root, `backend`, staging, `extract` and the final dir must not be symlinks |
| F3: closing the main window skipped the Assistant shutdown (Tauri's `exit` ends the process directly) | `shutdown_assistant` also runs on the window-close path |
| F4: the run record was deleted without checking ownership | A record is removed only when it names this process and the server it stopped |
| F5: two instances could both pass admission | Cross-process lock on `run/load.lock` from reading the record to writing it |
| F7: the API key could go through a system proxy | Loopback clients use `no_proxy()` |
| F8: a dropped device file such as `/dev/zero` was read without end | Only regular files are read, capped at 25 MB |
| F9: Cancel could not interrupt a download waiting for headers | The cancel flag is polled every 250 ms while waiting |
| F10: Stop during token counting left the counting requests running | Cancellation covers context fitting |
| F11: a late start response could clear a draft typed after New chat | Start responses from an earlier conversation are ignored |
| F12: parallel attaches could exceed 4 images | The remaining room is rechecked after decoding |
| F6 (rejected): PID reuse by another copy of the same llama-server | Only VoiceReader starts that binary from its own data folder, every start rewrites the record, and F5's lock serialises those writes |

A Codex re-review of the fixes confirmed F1–F5, F7–F9 and F12. It found no lock inversion and saw that the file lock is released before the `/health` wait. It found three more ordering issues, all fixed with tests or type-checked changes. These three fixes have not been re-reviewed.

| Finding | Fix |
|---|---|
| R1: Stop could reach Rust before the start command registered the request, so the cancel was lost | `cancel_chat` remembers up to 64 early-cancelled ids; a later `start_chat` for one sends `done(stopped)` and starts nothing |
| R2: New chat could wipe a draft typed while its cancel was pending | New chat resets the page first, then awaits the cancel |
| R3: an image dropped before New chat could attach to the new conversation | Drops are tied to the conversation they started in |

## Cross-model review of the voice pipeline (2026-10-19)

Codex reviewed the Steps 2–4 Rust code and the player changes. Of 11 findings, 10 were accepted and fixed and 1 was rejected:

| Finding | Fix |
|---|---|
| F1: automatic turn ends were judged on a lagging copy of the audio, so newer speech could land in the older turn | The detector now runs **inside the capture thread** and judges each block before it is routed; a test checks exact block boundaries |
| F2: a phrase arrives in several chunks, so it was acknowledged early and twice | Rust sends `PHRASE_DONE` after each phrase; only that marks where the phrase ends |
| F3: "consumed" counts samples fed into the stretcher, slightly ahead of what is heard | A phrase counts as heard 0.2 s after its last sample is consumed |
| F4: Skip reported skipped phrases as heard | Skip drops pending phrase marks |
| F5: interrupting after synthesis finished left queued audio playing | A late cancel sends `JOB_CANCELED` for that answer's playback job |
| F6: a microphone still opening could outlive a close or an error | A generation counter; an open that loses the race closes what it opened |
| F7: a disable during an enable's model load was overridden | A generation counter; the newest call wins |
| F8: a detector failure left a dead detector behind | The capture stage drops it and reports `assistant:mic-auto-failed`; push-to-talk keeps working |
| F9: an unbounded detector queue | Gone with F1 (no separate queue) |
| F11: a long streamed digit run was rescanned quadratically | Only runs of up to 3 digits (possible list numbers) wait for more text |
| F10 (rejected): unbounded pending model text during backpressure | Bounded by the answer's `max_tokens`, so at most about 1 MB even at 256K context |

Switching auto-send off now **pauses** the detector instead of unloading it, so the models load once per microphone session.

## Known gaps

- `0.3.0-testing` is valid semver but not a valid Python (PEP 440) version. `sync-version.js` also writes it into the deprecated Full engine's `tts-engine/pyproject.toml`, so `pip install` of that engine would reject it. The Base build does not read it. Windows MSI/NSIS installers also need a numeric version; the portable build (`--bundles none`) does not.
- The test build has the same app identifier (`com.voicereader.desktop`) as v0.2.3, so both share settings and downloaded models. Its `.app` is also named `VoiceReader.app`, so unzip it in a different folder from the working one.

- Hashing a large model after its download (about 15 s for 12B) shows no progress and cannot be cancelled.
- The chat stream has no stall timeout. A server that stops sending without closing the connection waits until Stop.
- The API key is on llama-server's command line, so other processes of the same user can see it. The server only listens on loopback.
- The app-wide CSP is still `null`. The Markdown renderer is the protection against model-generated HTML; tightening the CSP is a separate follow-up (plan §3.2).

- Smart Turn produced one false cut-off on a synthetic mid-sentence clip (a TTS voice reading a fragment with sentence-final intonation). It needs testing with real voices; the knobs are `complete_threshold` and `pause_ms` in `assistant_turn.rs`.
- Loudspeakers can leak the end of a spoken answer into the next turn's pre-roll (300 ms push-to-talk, 800 ms auto). There is no echo cancellation; use headphones, or lower the pre-roll.
- At 2× speed, a short first phrase followed by more than 5 s of model silence could be reported as a playback stall ([conversation.md](conversation.md)).
- The memory-fit rule leaves out the separate allowance for ASR and TTS (about 1.3 GB while transcribing).

## Next steps

1. **On this Mac, in the real app:** chat-page push-to-talk, then voice mode (a full answer, interrupting, read-aloud preempting, and auto-send on), using the manual checks in [conversation.md](conversation.md).
2. Commit when satisfied (nothing has been committed).
3. **On Windows:** the build, the checks above, and the voice checks; fix anything Windows-specific.
4. Later: custom GGUF models from Hugging Face (model plus mmproj) and API endpoints (shown as "planned" on the Models page); tighten the CSP; per-model tuning of Smart Turn thresholds with real voices.
