# Assistant page and Models page (frontend)

This document covers the browser side of the v0.3.0 Assistant: the Assistant page (laid out like LM Studio), its settings panel, the chat, the safe Markdown renderer, image attachments, and the Assistant section of the Models page, where all installing and downloading now happens. The backend (a managed `llama-server`) is written in Rust and documented in [backend.md](backend.md) and [chat.md](chat.md).

It implements `docs/v0.3.0_assistant_plan.md` §3.1 (model lifecycle), §3.2 (chat and safe rendering, Codex F4), §5.2 (tier recommendation, shown as a badge), §5.3 (memory rule and the fit warning, Step 5 UI) and §6.3 (images and context choices). Push-to-talk on the chat page and voice conversation mode (§3.3, §6, §7) are summarised under "Voice" below and documented in full in [conversation.md](conversation.md).

## Files

| File | Role |
|---|---|
| `src/assistant.ts` | The page. `initAssistant({ log, isPageActive })` builds the top bar, the conversation, the composer and the right panel inside `#assistant-root`, and wires commands and events. |
| `src/assistant-models-page.ts` | `initAssistantModels({ log })`: the Assistant card of the Models page (`#assistant-models-slot`): backend install, model download, cancel, delete, memory badge, fit dot and Recommended badge. |
| `src/assistant-format.ts` | Shared text: `gigabytes`, `contextLabel`, `memoryTooltip`, `memoryBadge`, `copyToClipboard`, and the memory-fit rule `memoryAt(memory, ctx)`, `memoryFit(memory, ctx, hardware)` and `fitDot(fit, prefix)`. |
| `src/assistant-hardware.ts` | One shared `assistant_hardware` result for both pages: `ensureHardware`, `requestHardware(refresh)`, `onHardware(listener)`, `noteBackendInstalled(installed)`. |
| `src/assistant-sampling.ts` | Per-model sampling storage (`samplingFor(model)`, `recommendedSampling(model)`) and the panel's Sampling section (`createSamplingSection()`). |
| `src/assistant-prompts.ts` | The panel's System prompt section (`createPromptsSection({ log })`), `MAX_PROMPT_CHARS = 8000`. |
| `src/assistant-dom.ts` | DOM and storage helpers: `h`, `button`, `svgIcon` and its icons, `errorText`, `readStored`/`writeStored`/`removeStored`, `panelSection`, `megabytes`. |
| `src/assistant-markdown.ts` | `parseMarkdown`, `renderMarkdown`, `markdownToPlainText` (unchanged by this redesign). |
| `src/assistant-images.ts` | `prepareImages(files, alreadyAttached)` (unchanged). |
| `src/assistant-tools.ts` | Shared wrench menu, per-mode selections, consent, tool activity/source rendering and thinking-limit controls. `src/tools/` owns each tool’s settings and Python runtime controls. |
| `src/assistant-mic.ts` | `createMic(log)`: the one microphone session shared by push-to-talk and voice mode (commands `assistant_mic_*`, events `assistant:mic-*`). |
| `src/assistant-voice.ts` | `createPushToTalk(...)` (the composer's mic button) and `createVoiceMode(...)` (the voice view, its panel section and the answer lifecycle). |
| `src/assistant-turns.ts` | Pure turn-taking rules with no DOM: `HoldToggle`, `TurnJoiner` (Codex F7), `heardText`, `cutShortText`, `buildVoiceHistory`, `heardPrefixLength`, `appendToDraft`, the playback watchdog (`notePlayback`, `noteSpeechDone`, `playbackVerdict`) and `levelFraction`. |
| `src/types.ts` | Assistant types at the end: `AssistantStatus`, `AssistantModelInfo` (with `sampling_defaults` and `memory`), `AssistantSampling`, `AssistantPrompts`, `AssistantHardware`, event payloads. |
| `src/main.ts` | The `data-page="assistant"` section is a visually hidden `<h1>` plus `#assistant-root`; the page builds everything else. The Models page has `#assistant-models-slot` under its "Assistant" group heading. |
| `src/styles.css` | The "Assistant page" block (`.assistant-*`, `.md*`) and the "Models page: Assistant section" block. Other selectors are untouched. |
| `scripts/test-assistant-markdown.mjs` | `npm run test:assistant`. |
| `scripts/test-assistant-fit.mjs` | `npm run test:assistant-fit`. |
| `scripts/test-assistant-voice.mjs` | `npm run test:assistant-voice` (the rules in `assistant-turns.ts`). |

All DOM is built with `document.createElement`, `textContent` and text nodes. `grep -nE "innerHTML|outerHTML|insertAdjacentHTML|DOMParser|document\.write" src/assistant*.ts` prints nothing. The only markup strings are the static shell in `main.ts` and `icons.ts`, which are not model-controlled.

## Backend contract the frontend relies on

Tauri v1: `invoke` argument names are camelCase, payload fields are snake_case.

| Command | Args | Resolves to |
|---|---|---|
| `assistant_status` | none | `AssistantStatus` |
| `assistant_install_backend` | none | `AssistantStatus` after finishing; rejects on failure or cancel |
| `assistant_download_model` | `{ modelId }` | `AssistantStatus` after finishing; rejects on failure or cancel |
| `assistant_cancel_download` | none | nothing |
| `assistant_delete_model` | `{ modelId }` | `AssistantStatus`; rejects with a readable refusal |
| `assistant_load` | `{ modelId, ctxSize }` | `AssistantStatus` once ready; rejects with a readable string |
| `assistant_unload` | none | `AssistantStatus` |
| `assistant_chat_start` | `{ requestId, messages, think, sampling, conversationId, tools, enabledTools, thinkingBudget, toolLimits }` | `{ request_id, prompt_tokens, dropped_messages }`; rejects with a readable string |
| `assistant_chat_cancel` | `{ requestId }` | nothing |
| `assistant_get_prompts` | none | `{ chat, voice, chat_default, voice_default }` |
| `assistant_set_prompts` | `{ chat: string \| null, voice: string \| null }` (null resets that prompt) | the same payload; rejects with a readable string (for example a prompt that is too long) |
| `assistant_hardware` | `{}` or `{ refresh: true }` | `{ total_ram_mib, unified_memory, devices: [{ name, total_mib, free_mib, integrated }], devices_known, recommended_tier, reason }` |
| `assistant_read_dropped_image` | `{ path }` | a data URL |
| `speak_text` (existing) | `{ text }` | existing result |
| `assistant_mic_*`, `assistant_voice_respond`, `assistant_voice_cancel` | see [conversation.md](conversation.md) | |

`messages` is `{ role: "user" | "assistant", text, images: string[] }[]` (images are data URLs), oldest first, ending with the new user message. `sampling` is `{ temperature, top_p, top_k, min_p, repeat_penalty, max_tokens: number | null }` for the model the server has loaded, or `null` when the page knows no model (the backend then uses the model's defaults). The Chat system prompt is not sent: Rust applies the saved one.

Each `AssistantStatus.models[]` entry has a `tools` qualification flag, `sampling_defaults` (`temperature`, `top_p`, `top_k`, `min_p`, `repeat_penalty`) and `memory` (`gpu_fixed_mib`, `gpu_mib_per_1k_ctx`, `host_fixed_mib`, `host_mib_per_1k_ctx`, `measured`), besides the older `memory_estimate` used by the Memory badge tooltip.

Events (all via `listen`): `assistant:status` (`AssistantStatus`), `assistant:download-progress` (`{ kind, id, file, downloaded, total }`), `assistant:chat-delta` (`{ request_id, round, content, reasoning }`), `assistant:chat-done` (`{ request_id, finish_reason, stopped, final_text, final_round, history_note, sources, timings, thinking_budget_tokens, completion_tokens }`, extended fields optional), `assistant:chat-error` (`{ request_id, message }`), and Tauri's `tauri://file-drop` (paths). Chat events whose `request_id` is not the current request are ignored.

## Assistant page layout

The app's left sidebar stays. When the Assistant page is active, `.content-inner:has(> .page[data-page="assistant"].active)` removes the 920 px width cap and the padding, so the page fills the content area. The page has three parts.

### Top bar (`.assistant-topbar`, one row, sticky)

- **Model picker** (`<select>`): installed models only, labelled "Gemma 4 E4B · Mid". The selection is the stored choice if installed, else the running model, else the first installed one. Changing it stores the choice and loads that model (unload, then load), unless the server is in use by another window. It is disabled while a model loads.
- **Status pill**: "Stopped", "Loading model…", "Unloading…", "Ready · <device>", "In use in another window", or the load error / server error text (ellipsis, full text in the tooltip).
- **Load / Eject**: one button. "Eject" (unload) while the server is ready; "Load" otherwise, including after an error (load unloads first). "Loading…" / "Ejecting…" while busy. Disabled in another window's use or without a model.
- **Chat | Voice** switch (`aria-pressed` on each half). Each mode keeps its own conversation; switching keeps both. Entering Voice opens the microphone; leaving it (or the page) cancels any spoken answer and closes the microphone.
- **Panel toggle** (sliders icon), with `aria-expanded`.
- **Nothing installed** (backend missing or no model installed): the picker, pill and Load/Eject are hidden and the bar shows "No Assistant model installed" and **Open Models**, which clicks `.nav-item[data-nav="models"]`. There is no install or download UI on this page.

A failed `assistant_status` after a successful one shows a red notice line under the bar. On an unsupported platform the bar and panel are hidden.

### Conversation (`.assistant-main`)

The messages column scrolls (`.assistant-messages`, centred to 760 px with the scrollbar at the edge) and the composer is pinned under it. Empty states replace both:

- "Checking the Assistant…" until the first status arrives; "The Assistant could not start" with the error if that fails.
- "The Assistant is not available on this computer" (needs Windows x64 or Apple Silicon).
- "No Assistant model installed" with a sentence on what to install and an **Open Models** button. Shown only while the conversation is empty, so a model deleted mid-conversation does not hide the messages; the composer then says "Install a model on the Models page to keep chatting."

Composer row: Attach, the push-to-talk mic button (in `.assistant-ptt-slot`, with a level meter while recording), New chat, then Stop (while streaming) or Send. Under the composer: the push-to-talk status ("Starting microphone…", "Listening… <partial text>", "Transcribing…"), the microphone error line, the chat error line, and a hint ("Load the model to start chatting.", "Another VoiceReader window is using the model.").

In voice mode `.assistant-messages` and the composer are hidden and `.assistant-voice` is shown instead: the round talk button with a level ring and a state line, and the transcript pane with its own New chat button. See [conversation.md](conversation.md).

### Right panel (`#assistant-panel`, `.assistant-panel`)

320 px wide, scrolls on its own. Below a window width of 1000 px (CSS media query, mirrored by `NARROW_QUERY` in `assistant.ts`) it overlays the conversation with a shadow instead of squeezing it; Escape inside the panel then closes it and returns focus to the toggle. The panel header has a close button. Each section is a `<details>` (open by default):

1. **Model**: Context length 8K–256K. Options above the selected model's `max_context` are disabled, with the tooltip "<model> supports up to <N> tokens." A stored value above the limit stays selected and loads at the limit, with a note. The note always says "Applies the next time the model loads."; if the loaded context differs, it adds "The loaded model uses <N> tokens." and **Reload now** appears. Below it, the memory fit for the selected model at the context it will load with, as "✅ Fits in memory at 32K" (or ⚠️/⛔ with the label) and the figures as a caption. "Checking this computer's memory…" until the hardware is known; hidden without a model.
2. **System prompt**: tabs **Chat** and **Voice** (arrow keys switch tabs). A textarea per tab (max 8000 characters, counter "N / 8000"), drafts kept per tab while switching. **Save** is enabled when the draft differs from the saved prompt. **Reset to default** is a two-step button (`makeConfirmButton`: "Reset this prompt?") and sends `null` for that tab. Saving one tab sends the other tab's saved value, or `null` if it equals its default, so it keeps following the default. The Voice tab notes that thinking is controlled separately in the Voice section. Its default prompt asks for decimals as “28 point 8” rather than “28.8”, because the current TTS can treat a decimal dot as a sentence pause. Custom saved prompts remain mine to edit or reset. Prompts load at start and are retried when the page opens if loading failed; errors show in the section.
3. **Reasoning**: the "Think before answering" checkbox, stored as before. Hidden in voice mode, which has its own optional Think toggle (off by default) and independent thinking limit.
   - **Voice** (voice mode only, shown in place of Reasoning): Think and its own thinking limit, plus "Send automatically when I stop talking (experimental)", with a note that it uses voice activity detection plus Smart Turn and may cut you off. When the backend refuses it (for example another Assistant download is running, or the model download failed), the checkbox shows unticked with "Automatic sending stopped: <reason>"; ticking it again retries.
4. **Sampling**: Temperature (slider 0–2, step 0.05, plus a number box). A collapsed **Advanced** group: Top-p (0–1), Top-k (0–200, integer, 0 = off), Min-p (0–1), Repeat penalty (1.0–2.0), Max response length (empty = no limit, else 16–32768). Values outside the range are clamped; an unusable entry restores the previous value. **Reset to recommended** restores the model's `sampling_defaults` (disabled when already there). A caption lists the recommended values. The section shows the selected model's settings; requests use the loaded model's.

## Memory fit (plan §5.3)

`memoryFit(memory, ctx, hardware)` in `assistant-format.ts` is the only implementation; both pages call it.

- `gpu = gpu_fixed_mib + gpu_mib_per_1k_ctx × ceil(ctx / 1024)`, and the same for `host`.
- **Unified memory, no discrete GPU (integrated only or none), or `devices_known` false**: total = gpu + host against `total_ram_mib`. ✅ "Fits in memory" up to 50%, ⚠️ "Tight, may slow the computer" up to 75%, ⛔ "Likely to run out of memory" beyond. With `devices_known` false (and not unified) the detail adds "GPU memory unknown until the backend is installed."
- **Discrete GPU** (the largest non-integrated device): budget = `total_mib − 1024`. ✅ "Fits in GPU memory" if gpu ≤ budget, otherwise ⚠️ "Won't fit in GPU memory; parts run on the CPU and it will be slower" with spill = gpu − budget. In both cases ⛔ "Likely to run out of memory" if host + spill exceeds 50% of system RAM.
- `total_ram_mib` of 0 gives ⚠️ "Memory could not be checked", never a fit.

The panel shows icon, label and detail. All model badges read **Memory**; MiMo’s tooltip retains the distinction that its coefficients are calculated rather than measured. The Models page shows a 10 px dot (`.assistant-fit-dot.fit-fits|fit-tight|fit-over`: accent, amber, danger) at 32K with "At 32K context: …" as tooltip and accessible name. The plan's extra allowance for ASR and TTS models loaded at the same time is not included yet.

`assistant-hardware.ts` asks `assistant_hardware` once at start (shared by both pages; a failed query is retried by the next `ensureHardware`). Every status passes `backend_installed` to `noteBackendInstalled`; the first time the backend is installed while `devices_known` is false, it asks again with `{ refresh: true }`, once per session. The newest reply wins.

## Models page: Assistant card

The card sits under the Models page's "Assistant" group heading (added in `main.ts` by the coordinator), so it has no title of its own: a short description, then the rows, then the muted line "Custom GGUF models from Hugging Face and API endpoints are planned for a later version."

- **Backend row**: "llama.cpp backend (b11718, 12 MB)" from `backend_version` and `backend_download_bytes`. **Install** while not installed (disabled while another download runs), then a progress bar and **Cancel** while installing, then "Installed". No delete.
- **Model rows**: name, tier, size, **Memory** badge (tooltip: the 32K estimate), the fit dot, and **Recommended** (accent badge, tooltip = `reason`) on the model whose tier is `recommended_tier`. **Download** while not installed; disabled until the backend is installed ("Install the llama.cpp backend first") and while any download runs. Progress and **Cancel** while downloading. **Delete** once installed: two steps, "Delete X GB?" (`makeConfirmButton`).
- Downloads never start by themselves. The backend allows one at a time.
- Progress comes from `assistant:download-progress` and is painted in place; a progress event that beats the status announcing the download marks it active.
- A `generation` counter drops replies older than the newest status event. When a command's reply is stale, the card fetches `assistant_status` again.
- A failed install, download or delete shows its error in that row's status line until the row's next action. A cancelled download shows no error.

## Chat behaviour (unchanged rules)

- User messages show text and thumbnails. Answers show a collapsed "Thinking…" `<details>` (only if reasoning arrived), then the safe Markdown answer, then **Speak** (`speak_text` with `markdownToPlainText(answer)`, never the reasoning) and **Copy**.
- Deltas re-render one message at most once per animation frame.
- History sent with each request: exchanges whose answer finished with text, and stopped answers with `\n\n[stopped by user]`; an exchange whose answer failed or had no text is left out entirely, so roles alternate. Request ids come from `crypto.randomUUID()`.
- The request id is registered before `assistant_chat_start` is awaited (events can arrive first). If the start rejects, both turns are removed, the error shows under the composer and the draft and images stay. `dropped_messages > 0` adds a note under the answer.
- `finish_reason: "length"` (not stopped) adds "The answer was cut off because the context length or the maximum response length was reached."
- Race fixes kept: a `conversation` counter bumped by New chat, so a late start reply, image decode or desktop drop from an earlier conversation changes nothing; New chat resets the page before awaiting the cancel; Rust remembers early cancels (see [chat.md](chat.md)).
- Enter sends, Shift+Enter adds a newline. Conversations live in memory only.

## Images

Unchanged: `prepareImages` accepts PNG, JPEG and WebP, decodes with `createImageBitmap`, scales the long edge to at most 1024 px and encodes PNG as PNG, others as JPEG 0.9 on white. Up to 4 per message, by Attach, paste (picture-only clipboard), HTML5 drop on the composer, or a desktop drop: `tauri://file-drop` gives paths, `assistant_read_dropped_image` reads each in Rust, and the usual downscaling applies. Drops are ignored when the Assistant page is not active.

## localStorage keys

Every access is wrapped in try/catch. If storage is blocked, choices last until the app closes (sampling keeps a session copy).

| Key | Values | Default |
|---|---|---|
| `voicereader.assistant.model` | model id | first installed model |
| `voicereader.assistant.ctx` | `8192` … `262144` | `32768` |
| `voicereader.assistant.think` | `"true"` or `"false"` | on |
| `voicereader.assistant.panel` | `"open"` or `"closed"` | open on a window wider than 1000 px, else closed |
| `voicereader.assistant.autosend` | `"true"` or `"false"` | on in a dev build (`import.meta.env.DEV`), off in a release build |
| `voicereader.assistant.sampling.<model id>` | JSON with only the values that differ from that model's `sampling_defaults`, for example `{"temperature":0.9,"max_tokens":2048}` | absent (recommended values) |

Stored sampling values are re-validated on read (finite, clamped into range); a malformed entry falls back to the defaults.

## Voice (push-to-talk and voice mode)

Built on the Step 2–4 backend contract; full rules, events and manual tests in [conversation.md](conversation.md). In `assistant.ts`:

- `createMic(log)` makes the one microphone session; `createPushToTalk` fills `.assistant-ptt-slot`; `createVoiceMode` provides the voice view (inserted into `.assistant-main`) and the panel's Voice section body.
- `setMode(mode)` toggles `.is-voice-mode` on `.assistant-shell`, cancels a push-to-talk turn and calls `voice.setEntered(mode === "voice" && pageActive)`.
- `renderState()` shows the chat column or the voice view, and the Reasoning or the Voice panel section.
- `applyStatus()`: when the server stops being ready (Eject, model switch, crash), push-to-talk is cancelled and the microphone is closed. `renderAll()` calls `voice.refresh()`, which closes the microphone in voice mode while the model is not ready and reopens it when it is.
- Leaving the page cancels push-to-talk and closes the microphone; voice mode reopens it when the page is shown again.
- New chat clears only the conversation of the mode on screen.

## Markdown safety

Model output is untrusted and never becomes HTML.

- `parseMarkdown` is a pure function from text to data (`MdBlock`, `MdInline`) with no DOM access. Supported: paragraphs, ATX headings, emphasis, bold, inline code, fenced code (also unterminated while streaming), lists with one nesting level, block quotes, horizontal rules, pipe tables, `[text](url)` links and bare `http(s)://` URLs. Everything else, including raw HTML, stays literal text.
- **Tables** are a header row plus a delimiter row with the same number of columns (`| a | b |` then `|---|:-:|`), with optional outer pipes, `:` alignment and `\|` for a literal pipe. Cells hold inline text only, so HTML, images and `javascript:` links stay inert. Rows continue until a blank line, a line without a pipe or another block; a row still streaming is padded to the header width. At most 24 columns and 500 rows, beyond which the text stays literal. A table can interrupt a paragraph. It renders as a real `<table>` built from text nodes inside a box that scrolls sideways when it is wider than the answer. Speak and Copy turn each row into cells joined by ` | `.
- `renderMarkdown` uses `createElement` and text nodes only. Headings are shifted down two levels.
- Links become `<button class="md-link">`; a click calls `shell.open(url)` only for `^https?://`. Other schemes render as an inert `<span>`. The WebView never navigates.
- Markdown images stay literal text. Parsing cost is bounded (see `assistant-markdown.ts`).

## How to test

Automated (no backend needed):

```
npx tsc --noEmit
npm run build
npm run test:assistant
npm run test:assistant-fit
npm run test:assistant-voice
npm run test:player
grep -nE "innerHTML|outerHTML|insertAdjacentHTML|DOMParser|document\.write" src/assistant*.ts   # prints nothing
```

`test:assistant-fit` bundles `src/assistant-format.ts` with esbuild and checks: rounding up to 1K blocks, the 50%/75% unified boundaries, context length moving the verdict, integrated-only and no-device machines using the RAM rule, the discrete budget boundary (total − 1 GB), spill making it tight, host + spill over 50% making it over, the largest non-integrated device being chosen, the "GPU memory unknown" note, and unknown RAM never fitting.

The redesign was exercised in a browser (Vite dev server) against a temporary mocked `window.__TAURI_IPC__`, since deleted: top bar and panel in dark and light themes; panel open/close, its stored state and the overlay with Escape below 1000 px; the no-model, no-backend and unsupported empty states and Open Models; prompt save, tab switch and two-step reset (args checked); sampling stored per model, clamping, and the values sent in `assistant_chat_start`; the 256K option disabled for a 128K model; fit levels (fits, tight, spill on an 8 GB GPU); the model picker unloading and loading; a rejected start keeping the draft; Stop and the `[stopped by user]` history; New chat during a stream; Eject; and on the Models page install, cancel, download, cancel, download, delete, the Recommended badge, the fit dots and the hardware refresh after install. That earlier mocked run did not use the real backend; I subsequently tested and accepted the macOS app, as recorded below.

Regression checklist with the real backend (`npm run desktop:dev` or the packaged app). I have tested and accepted the current UI and Assistant/tools behavior on Apple Silicon macOS, including the final spacing/alignment and Voice decimal prompt. I have since tested it on Windows too, and it works (2026-10-20). The list remains useful for repeat testing; individual edge cases are not marked complete without a specific record.

1. **Clean profile.** Open Assistant. Expect the top bar to say "No Assistant model installed" with Open Models, the same empty state in the conversation area, and no download buttons. Press Open Models: the Models page opens.
2. **Install the backend.** On the Models page, the backend row reads "llama.cpp backend (b11718, … MB)" and the model Download buttons are disabled with the tooltip "Install the llama.cpp backend first". Press Install: progress and Cancel appear. Cancel mid-way: no error, Install returns. Install again to the end: the row says "Installed" and Download buttons enable. On Windows, hover the fit dots afterwards: they should name your GPU (the devices are re-read after install).
3. **Recommended and fit.** Exactly one model shows Recommended; its tooltip gives a reason matching your hardware. On a discrete GPU with ≤ 8 GB, the 12B dot should be amber ("Won't fit in GPU memory…"). On a 16 GB Mac, check the E4B dot against the measured memory use.
4. **Download.** Press Download on Small: progress with file name, MB and percent; other Download buttons are disabled. Cancel and retry; let it finish: "Downloaded." and a Delete button.
5. **Auto-load.** Open the Assistant page. The picker shows "MiniCPM-V 4.7 1B · Small", the pill goes "Loading model…" then "Ready · <device>" with no click.
6. **Switch model.** With two models installed, choose the other in the picker: the pill shows loading, then Ready with the new model; the llama-server log shows a restart.
7. **Eject and Load.** Press Eject: the pill says Stopped and the button says Load. Press Load: it loads again.
8. **Context and fit.** Open the panel, choose 16K: "The loaded model uses 32K tokens." and Reload now appear; press it, and the server log shows the new context. Choose a context above the model's limit: the option is disabled with a tooltip. Watch the fit line change as you pick 128K or 256K.
9. **System prompt.** Change the Chat prompt to "Answer in French." and Save. Ask a question: the answer is in French. Reset to default (two clicks): the default text returns and answers are in English. Paste more than 8000 characters: the box stops at 8000. Restart the app: the saved prompt is still there.
10. **Sampling.** Set temperature to 0 and ask the same question twice: the answers should match closely. Set Max response length to 16: answers stop early with the "cut off" note. Reset to recommended. Switch to another model: its own values show. Restart the app: changed values are kept per model.
11. **Panel.** Hide and show the panel; restart the app: the state is kept. Make the window narrower than about 1000 px: the panel floats over the conversation; Escape closes it.
12. **Chat, Think on/off, Stop, long message, Speak, Copy, New chat, images, hostile HTML, another window, themes**: as before. Think is now in the panel's Reasoning section. In particular: ask the model to repeat `<img src=x onerror=alert(1)>`, `<script>alert(1)</script>` and `[click me](javascript:alert(1))` verbatim; all must appear as text and nothing may run. Drop an image file from Explorer or Finder onto the window; it attaches.
13. **Voice switch.** Choose Voice: the talk button and transcript replace the chat; the panel shows Voice instead of Reasoning. Choose Chat: the chat conversation is still there. The voice checks are in [conversation.md](conversation.md).
14. **Delete.** On the Models page, Delete the loaded model (two clicks): it unloads and the Assistant page returns to its picker without that model, or to the empty state if none is left.
15. **Light and dark theme.** Check both on both pages; the pill, fit colours, tabs, code blocks and the Thinking block should be legible.

## Known gaps and next steps

- The fit rule does not yet add the ASR (about 1.3 GB) and TTS models loaded alongside, as plan §5.3 asks for host RAM. Add them in `memoryFit` when the app reports which are loaded.
- No fallback to the last context that worked when a load fails (plan §5.3); the error shows in the pill.
- The full-height layout uses `:has()`. WebView2 and current macOS WebKit support it; on macOS older than 12.3 the page falls back to the normal 920 px column and the messages area grows with the page instead of scrolling on its own.
- Load is no longer disabled while a model downloads (it was in Step 1, when downloads were on this page). If the backend turns out to refuse a load during a download, its error appears in the pill.
- Markdown extensions other than pipe tables (footnotes, task lists, strikethrough, reference links) render as literal text. Conversations are not saved, by design.
- Voice gaps are listed in [conversation.md](conversation.md).


## Optional tools

Chat and Voice have separate wrench Tools menus, off by default, and a shared first-enable consent. Each menu has All tools, per-tool switches and expandable settings. Web search owns provider/plain-text Brave key settings, each tool owns its timeout, and Python owns runtime Download/Delete. These controls live only on the Assistant page; no tools section remains in global Settings or Models. Only enabling Python prepares the optional runtime. Changes apply to the next message; Rust filters both schemas and execution to that turn’s selection. When Voice thinking is enabled, reasoning streams into a collapsed transcript accordion and is never spoken or retained in history. Tool activity, results and sources are rendered as text nodes. New chat clears the exact conversation URL grants; stale events cannot alter a new conversation. Per-tool frontend modules and their registration list are documented in [src/tools/README.md](../../src/tools/README.md); runtime-created tools remain a [future plan](tool-plugins-plan.md). See [tools.md](tools.md) for the extended command/event contracts, voice history rules and validation.
