# Assistant tool qualification, 2026-10-19

These reports use the production Rust model/tool loop, actual Chat/Voice system prompts and app tools policy, with the date fixed in `src-tauri/tests-data/tools/model-cases.json`. Spoken-prompt mode changes the prompt/policy; it does not test the microphone, speech synthesis or playback. [Implementation and result interpretation](../../assistant/tools.md).

Each model runs 12 cases × three seeds × two modes, at its catalogue sampling settings with thinking off. A case passes on two seeds; the settled threshold is at least 11/12 in each mode plus every required city-clarification/no-tool/Python invariant. Structural safeguards (forced finalization, truncated calls, cancellation and event ordering) have separate deterministic regressions. Network responses are recorded and Python executes in the real WASI sandbox. A passing score is evidence for the capability flag, not a guarantee that every answer is correct.

The JSON records pin model revision/weights, b11718 server binary, its actual chat template, both complete app prompts, fixture bytes and tool schemas. They retain every call and final answer; newer reports also retain result payloads. Inputs are public synthetic questions and fixtures. No API credential is included.

To reproduce, use `pinned_models_twelve_cases_three_seeds` in `src-tauri/src/assistant_tool_loop.rs`. It starts and cleans up its own isolated loopback llama server. Leave the app's server alone. Install the optional sandbox in a separate test root, or follow the hash-checked archive fixture setup in [sandbox tests](../../../src-tauri/tests-data/sandbox/README.md). Set:

```sh
export VR_TOOLS_MODEL_ID=gemma-4-e4b
export VR_TOOLS_MODEL_PATH="/absolute/path/to/the/catalogue-pinned/model.gguf"
export VR_TOOLS_SERVER_BIN="/absolute/path/to/b11718/llama-server"
export VR_TOOLS_SERVER_SHA256="<binary hash from the matching report, or the verified Windows asset>"
export VR_TOOLS_TEMPLATE_SHA256="<template hash from the matching report>"
export VR_TOOLS_SANDBOX_ROOT="/absolute/path/to/isolated/assistant-root"
export VR_TOOLS_REPORT_PATH="/absolute/path/to/new-report.json"
cargo test --manifest-path src-tauri/Cargo.toml --features build-base pinned_models_twelve_cases_three_seeds -- --ignored --nocapture
```

The sandbox root contains `sandbox/`; it is not that directory itself. On Windows set the same variables with PowerShell `$env:NAME = "value"`, use the verified Windows executable and its actual binary hash, and keep the template pin from the same model. Failed qualification intentionally exits nonzero; it still saves the full report if the run completed. Do not automatically enable the model flag from a process exit alone: inspect the report, hard cases and current pins. The catalogue regression test ties any enabled flag to retained evidence and fails after a policy/model/backend change until requalification.


## Decimal Voice prompt revision

The follow-up default Voice prompt asks for decimal numbers as “28 point 8” to avoid the pause I heard from the current TTS at a decimal dot. Reports ending in `-decimal-voice.json` rerun the same 12 cases × three seeds × both modes for each verified Gemma model with this revised prompt. The fixture, schemas, sampling and qualification threshold remain unchanged. Original reports are retained for comparison; the catalogue regression check uses the revised reports. The fixture's weather values are integers, so qualification checks tool behavior after the prompt change, not decimal pronunciation or audio quality.


Current revised-prompt evidence: [Gemma E4B](2026-10-19-gemma-4-e4b-decimal-voice.json) passes 12/12 Chat and 11/12 Voice (the stock case remains its allowed failure); [Gemma 12B](2026-10-19-gemma-4-12b-decimal-voice.json) passes 12/12 in both modes. Both pass every hard invariant. These reruns completed on the M2 Max with b11718 and thinking off. MiniCPM and MiMo reports still describe the earlier prompt and remain unqualified.


## 2026-10-20: MiMo guidance, Gemma re-check and robustness

Run on Windows 11 with an RTX 5090 (Vulkan) and the Windows `llama-server` b11718, so `server_sha256` differs from the Mac reports; the model weights, template, fixture, tool schemas and prompts are the same pins. The Windows run of the original MiMo code reproduced the Mac's scores.

- [MiMo, guided](2026-10-20-mimo-v2.6-9b-guided.json): 12/12 in Chat and Voice, all hard cases pass. Run with `VR_TOOLS_THINKING=app-defaults`, so Chat thinks and Voice does not, as in the app, and with the model's `tools_guidance` in the prompt. The catalogue regression test binds MiMo's flag to this report.
- [Gemma E4B re-check](2026-10-20-gemma-4-e4b-recheck.json) (12/12 Chat, 11/12 Voice) and [Gemma 12B re-check](2026-10-20-gemma-4-12b-recheck.json) (12/12 both), thinking off, on the current loop and sandbox. The regression test binds their flags to these reports.
- [MiniCPM, guided](2026-10-20-minicpm-v-4.7-1b-guided.json): 11/12 in both modes with all hard cases passing, with its guidance and thinking on in both modes. **Not used for a flag.**
- Fresh-seed checks (seeds not used for tuning; `experiment: true`, never `qualified`): [MiMo](2026-10-20-mimo-v2.6-9b-fresh-seeds.json), nine seeds, every case 9/9 except Voice Python and Voice named-city weather at 8/9; [MiniCPM](2026-10-20-minicpm-v-4.7-1b-fresh-seeds.json), six seeds, Voice Python 3/6, Voice named-city weather 2/6, Chat stock 0/6, Chat rewrite 4/6. That is why MiniCPM is not verified although its official run passed.

The `thinking` field is now `{"chat": bool, "voice": bool}`. New harness switches: `VR_TOOLS_THINKING=app-defaults`, and for experiments `VR_TOOLS_CASES`, `VR_TOOLS_MODES`, `VR_TOOLS_SEEDS`, `VR_TOOLS_THINK=1`, `VR_TOOLS_TEMPERATURE`, `VR_TOOLS_POLICY_TIME`, `VR_TOOLS_EXTRA_SYSTEM` and `VR_TOOLS_DEBUG`. Any experiment switch makes the run an experiment that never writes `qualified: true`. The harness needs `src-tauri/tests-data/tools/*` to keep LF line endings (`.gitattributes`), because it hashes those files.
