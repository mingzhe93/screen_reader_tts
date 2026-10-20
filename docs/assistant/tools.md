# Assistant tools: implementation and validation

Updated 2026-10-19. This implements the settled decisions in [the tools plan](../v0.3.0_tools_plan.md), especially sections 14 and 15. The app version is `0.3.0`, in release preparation. I have since tested on my Windows machine (2026-10-20) and it works; the existing Assistant gaps remain. Tools are optional and off by default.

## Using tools

Chat and Voice each have a wrench **Tools** button with a count of enabled tools. It opens a filterable list with an **All tools** switch and an independent switch for each tool. A partial selection shows a mixed master switch. Each mode remembers its own selection; both start off. Changes apply to the next message: an active turn keeps its capability snapshot. Only selected tools appear in the model schema, and execution checks the same selection before running a call. A model cannot invoke a disabled tool by guessing its name.

Each tool has an expandable **Settings** section here on the Assistant page. Web search owns DuckDuckGo/Brave selection, the Brave key field with Save, Remove and Test, and its timeout. Weather and page reading each own their timeout. Python owns its timeout and optional runtime Download/Delete controls. **Advanced** contains the shared maximum execution rounds. Provider, key and limits are shared across modes; tool selections are separate. Tool settings no longer appear on the global Settings or Models pages.

The first enable asks for consent. Models without a passing qualification result show a reliability warning that I can override. Enabling only weather/search/page reading does not download Python. Enabling Python downloads and verifies its approximately 15 MB runtime; All tools includes that download. A declined confirmation or failed installation preserves the previous selection. No host Python installation is required.

The Brave key is stored in plain text in the app settings file, as disclosed beside the field. Selecting DuckDuckGo removes the saved Brave key. The key is never returned to the page, included in model messages, or emitted in events.

The four tools are `web_search(query)`, `fetch_page(url)`, `get_weather(city)` and `run_python(code)`. Searches use an honest VoiceReader User-Agent and a two-second admission throttle. DuckDuckGo blocks, CAPTCHA pages or changed markup produce a readable failure; they do not trigger provider rotation. Brave is used only when I save a key. Weather uses the first Open-Meteo geocoding result, names the resolved place and timezone, reports Celsius, and credits Open-Meteo (CC BY 4.0). Its free service is for this non-commercial distribution; revisit before paid distribution.

Python provides CPython 3.14 standard-library computation with timezone data. It cannot install packages, access the network, read host files or write files. Charts, numpy/pandas, generated files and PDF fetching remain outside this phase.

## Per-tool modules and registration

`src-tauri/src/assistant_tools/available.rs` is the built-in Rust registration list. Each registered module owns its function description/schema, argument size limit, timeout default/range and execution handler. Shared `assistant_tools.rs` owns selection, conversation grants, outcome envelopes and lifecycle rather than a tool-name dispatch match. Search-specific key commands delegate to the search module. The contributor instructions are in [the tools folder](../../src-tauri/src/assistant_tools/README.md).

`src/tools/available-tools.ts` registers the frontend modules. Each module owns its title, description, settings controls and optional preparation hook. `assistant-tools.ts` renders the shared list without branching on individual tool names. The two registries serve different compilation targets: adding a built-in means adding its Rust module/registration and UI module/registration; custom settings stay beside the relevant tool. This is compile-time extensibility, not runtime plugin installation.

`ToolLimits.timeouts` maps a tool name to its timeout. The per-tool map wins over the older flat timeout fields; those fields remain readable/writable for compatibility. Unknown names and out-of-range values are rejected in Rust. New tools use their registry metadata rather than adding another flat field to shared settings. `assistant_tools_settings` exposes registry metadata alongside provider/key-presence/limits, without exposing secrets. Every turn receives a settings snapshot.

I want tools created through chat to install without changing app source. That is **planned, not implemented**: [the runtime plugin plan](tool-plugins-plan.md) covers manifests, per-plugin settings, permissions, validation, updates and the review/install workflow. It deliberately leaves the runtime and capability scope open for a later decision.

## Ownership of a turn

`assistant_chat.rs` owns validation and the turn registry; `assistant_tool_loop.rs` owns the model → tools → model loop. Both chat and voice use it, including when tools are off. One registry slot, cancellation flag, backend generation and use guard span the whole turn. An intermediate HTTP completion does not finish the voice job or release the model.

A maximum of N rounds means N tool-execution batches followed by one final request without tool definitions. Arguments are assembled by streamed call index and bounded before strict validation. The loop suppresses canonical repeats, permits one malformed-call retry and never executes a length-truncated call. Finalization is claimed once; Stop observed before that claim wins. All tool events and final metadata precede the one terminal event. A voice startup failure is idempotent even when both the registered loop and command wrapper report it.

The internal messages include tool calls, matching results and current-turn reasoning. `/apply-template` receives the real tools and messages, followed by `/tokenize`. Fitting removes whole older turns, then shortens the oldest current-turn results. It preserves the current user message and all call/result pairs. Insufficient room disables further tools. Thinking tokens and answer headroom are bounded by the remaining context.

A cut-off or empty call forces a new answer using an app-owned system-policy instruction. It must not append a synthetic assistant message: the pinned llama.cpp backend treats that as a prefill. A real b11718 probe returned the recovery instruction itself instead of an answer; regression tests now reject that message shape.

Only the final round's content enters conversation history, together with a compact tool note and numbered sources. Earlier tool-round preambles may be displayed/spoken but are not retained as the answer. Raw tool results and reasoning are current-turn only. Source numbers are allocated once per turn across its calls.

Backend admission captures endpoint, generation and use guard under one mutex. Unload, replacement, crash and shutdown invalidate the generation and signal all registered turns. HTTP futures are dropped on cancellation; a running tool is awaited through teardown. Unload retains ownership until all turn workers drain. A first Wasm compilation is non-interruptible, so Stop waits for compilation before guest code can start. The load path refuses replacement if its bounded drain fails. Never wait for a terminal sink while holding the backend slot lock: the sink releases a use guard which needs that same lock.

## Network permissions

`assistant_tools.rs` owns the conversation-scoped URL grants and dispatch. A request carries a random `conversationId`; user-pasted URLs and search results grant only their exact normalized URL. A different path on the same host is insufficient. New chat revokes and closes the old grant object; a late search completion cannot recreate its permissions. Restart clears all grants.

`assistant_tools_network.rs` accepts only public HTTP(S) destinations without URL credentials. It rejects private, loopback, link-local, multicast, unspecified and reserved addresses, including IPv4 embedded in IPv6 and DNS answers mixing public/private addresses. Fetch uses `no_proxy()`, pins the checked DNS addresses and keeps the original hostname for TLS. Server redirects are manually followed up to five times, checked at every hop; HTTPS downgrade is refused. The model-selected initial URL must be allowlisted; a server-selected redirect may go to another public origin.

Bodies are bounded: search 1 MB, weather 256 KB, page 2 MB. Pages must be HTML/plain text/Markdown; extraction drops script/style/navigation and limits returned text to approximately 6,000 tokens. Tool data is explicitly untrusted in the model policy. This reduces authority confusion but does not guarantee a model will ignore misleading web content. I accepted that residual risk in the plan; fixed provider destinations, fetch grants and the network-free Python sandbox constrain the available actions.

## Python runtime and limits

A final bare expression in the code is printed, as a REPL or notebook cell does, so `sum(...)` shows its value; `print(...)` returns None and is not echoed. Models often omit `print`, and without this the run returned nothing and the model retried until its rounds ran out.

`assistant_sandbox.rs` downloads pinned CPython WASI 3.14.7 / SDK24 and tzdata 2026.5, verifies size and SHA-256, extracts into staging and atomically installs under `<assistant root>/sandbox/`. Their complete licence notices stay in that directory. Wasmtime and WASI are pinned to 36.0.18 and compiled into Base builds; CPython and tzdata are optional downloads. The legacy Full build excludes them.

Each call gets a fresh asynchronous WASI Preview 1 store, isolated Python flags, empty stdin/environment and only read-only stdlib/timezone directories. No writable scratch directory exists. Guest linear memory is capped at 256 MiB; the resource table at 128 entries; one instance, memory and Wasm table; output at 3,000 stdout plus 1,000 stderr bytes. CPU deadlines use epoch interruption; asynchronous WASI calls, including sleep, are cancellable. The default execution timeout is 60 seconds. Exhaustion, timeout, cancellation and truncated output are explicit results.

Install/delete/run share an in-process operation gate and a cross-process file lease outside the deletable runtime directory. Delete refuses active operations. Compilation holds its lease even if its caller drops; it finishes once and checks cancellation before executing guest code. The compiled native module remains in memory for the app session. No native executable cache is deserialized from disk.

Integrated M2 Max debug measurements: session compilation **1.141 s**, first compile plus execution **1.357 s**, warm fresh interpreter **0.118 s**; installed files **41,380,140 bytes**. CPU/sleep deadlines returned at 0.513/0.501 s for a 0.5 s limit; cancellations at 0.311/0.306 s for a flag scheduled at 0.3 s. The earlier isolated peak-RSS stress run measured **564,215,808 bytes (about 538 MiB)** while filling guest memory. The 256 MiB guest limit is not a cap on total host-process memory. Debug compiler dependencies are optimized; the initial unoptimized session compile took about 55 seconds.

## Thinking, speech and playback

Voice has its own Think toggle, off by default. When enabled, streamed reasoning appears in a collapsed **Thinking** accordion above the answer in each Voice transcript exchange. Reasoning-only events are supported, and opening the accordion stays open as fragments arrive. Reasoning is displayed as text, never spoken or retained in later conversation history. The backend suppresses reasoning events when Voice thinking is off. Chat and Voice have independent tagged thinking limits: Auto, Unlimited or a positive token count. Chat Auto is unlimited; Voice Auto targets roughly 15 seconds using recent per-model decode speed and a conservative 16 tokens/second fallback (240 tokens). Each model request resets the budget. A finite budget raises the response cap to at least budget + 1,024, within available context.

One speech worker owns the entire response queue and its global phrase sequence. Each phrase has `kind: answer|filler` and `round`. Fillers start after 1.2 seconds of thinking, repeat around every six seconds, and share the ordered queue ahead of answers; queued fillers are never silently dropped. Complete streamed query/city fields permit early tool fillers. Only the first round may speak a search query; later searches say “Searching the web.” A model preamble suppresses that round's call filler. Phrase buffers flush at round boundaries.

Only played answer phrases from the final round count as heard answer history; fillers never enter history. Stop cancels model generation, pending text, synthesis and playback together. The stall watchdog remains enabled. Automatic listening resumes only after the model's terminal metadata and actual playback drain; `voice-ended` alone means synthesis submission finished, not that the listener heard everything.

## Checks and remaining validation

**289 Rust tests pass**, with 22 asset/network/model checks ignored by default; the real sandbox and model qualification checks were also run separately. Frontend type checking, production build, all four Assistant suites and the player suite pass. Automated checks cover bounded streaming assembly, context fitting, round limits 1/5, repeated/malformed/truncated calls, terminal races, cancellation during fitting/tools, real backend admission/unload/crash/generation invalidation, grant revocation, provider parsers/CAPTCHA, DNS/redirect guards, voice filler timing/ordering, UI consent/history/event handling and the real Python runtime's isolation/deadlines/limits/recovery. Live DDG, public-page fetch and Open-Meteo smoke checks passed on 2026-10-19. A live Brave request with my own key has since been made and works (2026-10-20).

The ignored `pinned_models_twelve_cases_three_seeds` Rust test runs 12 recorded-tool prompts with seeds 1–3 in both chat and spoken-prompt modes. A case needs two passing seeds; qualification needs 11/12 and all hard invariants. It validates model, server and template hashes and writes a report with prompt/fixture/tool-schema/sampling pins. Network tool responses are recorded; Python runs in the real sandbox. This measures model/loop behavior, not microphone recognition, TTS quality or speaker playback. Qualification with actual app prompts (thinking off, b11718, M2 Max):

| Model | Chat cases | Spoken-prompt cases | Hard invariants | Tools flag |
|---|---|---|---|---|
| Gemma 4 E4B | 12/12 | 11/12 | pass | verified |
| MiniCPM-V 4.7 1B | 7/12 | 8/12 | fail | unverified; explicit warning/override |
| MiMo-V2.6 9B (first run, Mac) | 11/12 | 10/12 | fail | later qualified with its own guidance; see the 2026-10-20 findings below |
| Gemma 4 12B | 12/12 | 12/12 | pass | verified |

Gemma E4B's spoken stock-price case failed all three seeds: it sometimes declines or asks to search instead of using the available search tool. This is the one allowed failure under the settled 11/12 rule, and a real limitation to watch in listening tests. MiniCPM misses clarification/no-tool/Python invariants, so it remains explicitly unreliable. [Pinned reports and reproduction steps](../benchmarks/assistant-tools/README.md) retain every seed, call and final answer. I also inspected the qualified models’ named-city arguments, Apple search queries and supplied-page URLs; they match their prompts. A regression check binds both Gemma capability flags to the current model revision/hash, backend build, tools schema, fixtures and actual app prompts; changing those requires fresh evidence. MiMo fails the missing-city clarification hard invariant in chat and Python coverage in spoken prompts; it remains unverified. Some generations produce tool-shaped prose rather than structured calls; the app deliberately does not execute that prose.

Run the ordinary tests with `cargo test --features build-base --lib`, `npm run test:assistant-tools`, `npm run test:assistant-voice`, `npm run test:assistant`, `npm run test:assistant-fit`, `npm run test:player`, `npx tsc --noEmit` and `npm run build`. Real sandbox instructions and archive pins are in [tests-data/sandbox/README.md](../../src-tauri/tests-data/sandbox/README.md). Qualification environment variables are documented above its Rust test.

I tested tools successfully in Chat and Voice, then tested and accepted the current wrench menu, reasoning transcript and final UI polish on macOS. A native Chat check also returned Singapore weather through `get_weather`; isolated QA checks covered filtering, mixed selection, per-tool settings and the loaded decimal Voice default. Keep these targeted regression checks for future runs rather than treating general macOS acceptance as individual completion of each edge case: new runtime delete/reinstall paths, city clarification, current search, pasted-page summary and exact Python calculation in chat and actual voice; thinking fillers and interruption; New chat during a search; Eject during first compilation and during a running tool; two app windows; offline/CAPTCHA/API failure recovery. Windows compilation and runtime, and the microphone/TTS checks, have since been tested and work (2026-10-20). The Windows package sizes are measured below. A live Brave key check has since passed (2026-10-20). No 0.3.0 release has been published.


## macOS package and review record

The Apple Silicon testing app builds and packages successfully; its signature passes the portable packager's ad-hoc verification. Embedded tool-engine code raises the native executable from 25,735,504 to about 37,959,744 bytes (about **11.66 MiB** extra); this compares the saved pre-tools executable with the same release build path, not an independently controlled dependency-only benchmark. CPython/tzdata are not in the app bundle. The portable ZIP is about 220.7 MB. Bundled `notices/assistant-tools.txt` contains Wasmtime/WASI/Cranelift, scraper and chrono notices.

### Windows package size (measured 2026-10-20)

Measured from the Windows portable packages built on my machine, read from the ZIP files themselves (the archives store files uncompressed, so ZIP and file sizes agree). `VoiceReader.exe` grows from **24,064,000 bytes** (22.95 MiB, the v0.2.3 package, which has no Assistant) to **40,176,640 bytes** (38.32 MiB, the v0.3.0 package built at 11:16 on 2026-10-20), which is **+16,112,640 bytes (+15.37 MiB, +67%)**. The portable ZIP grows from 314,600,060 to **330,712,700 bytes** (315.38 MiB), +5.1%. That increase is exactly the executable's: every other file in the two packages has the same size, because the models and runtimes are the same and the Assistant's llama.cpp backend, models and Python runtime are downloaded after install, not shipped.

This is not comparable with the Mac figure above. The Mac number isolates the tools engine (an executable with the base Assistant against one with tools). The Windows number is the whole Assistant: backend management, Chat, Voice, turn detection, the tools engine and the larger frontend. I did not build a pre-tools Windows executable, so there is no tools-only Windows figure. The v0.2.3 package is the one on this machine, and I did not rebuild it.

Measuring it turned up a packaging gap: the Windows portable package contains **no `notices/` folder**, so `notices/assistant-tools.txt` (the Wasmtime, WASI, scraper and chrono notices) was not delivered. `scripts/package_portable.py` copied only `binaries/`; the macOS app bundle includes `notices/` as a resource. The packager now copies it, and the 0.3.0 Windows ZIP has to be re-packaged (see [RELEASING.md](../RELEASING.md)).

Before the menu refactor, I launched the package with a separate `VOICEREADER_DATA_DIR` and checked Chat/Voice thinking controls (Voice off by default), provider/key disclosure and timeout defaults, plus the then-current Models runtime card through the real native UI. No API key, microphone permission or model load was needed for this smoke check. It does not substitute for listening tests.

Three GPT-6.1 Sol/high helpers implemented scoped network, sandbox, frontend/core-loop and voice work in isolated worktrees. Opus 5.5 independently reviewed the integrated source: three findings accepted, none rejected or left unverified. The fixes cover recovery-prefill handling, idempotent voice startup errors and real backend generation/drain coverage. Other integration regressions cover late New chat grants and Stop winning terminal finalization.


The controls follow-up passes **289 Rust tests** (22 ignored), frontend type checking, the 12 tools checks, 28 Voice checks, 25 Markdown checks, 10 context-fit checks, player checks and production build. `build-full` also compiles. The controls refactor retained the all-tools schema hash and qualification prompt. The later decimal wording change was requalified on both verified Gemma models; the benchmark README links the current `-decimal-voice.json` reports and retains the earlier evidence. A separate native QA copy verified the Chat/Voice menu placement, filtering, mixed master state, search key/provider fields, weather timeout, Python runtime controls and removal of the global tools settings. I unloaded the QA model and closed the copy afterward. Voice reasoning isolation/accordion streaming has automated coverage; I subsequently accepted the current macOS app behavior. The automated record is distinct from my app/listening acceptance.

The controls testing ZIP and verification record are saved under `outputs/v0.3.0-tools-controls` in the task output directory; the later spacing/decimal-prompt build is under `outputs/v0.3.0-tools-polish`. It is ad-hoc signed. The previous three-finding Opus review applies to the initial tools phase; the new controls review could not run because the Claude subscription session limit was reached. No structured verdict was returned and no billing/model fallback was used.


### UI polish and decimal speech guidance

I found the guidance below All tools and the settings labels too close to their following content. The menu now has 12 px above its guidance and tool settings have a 6 px label/field gap. Assistant diagnostics actions align to the right, including when the header wraps, matching the TTS diagnostics layout.

The default Voice prompt now asks for decimal numbers as “28 point 8 degrees Celsius” rather than “28.8 degrees Celsius”. I heard the current TTS pause at the decimal dot, so this asks the language model to produce speakable wording before synthesis. It is prompt guidance, not a deterministic rewrite of every TTS input. Existing custom prompts are preserved; Reset to default adopts the new default. The Voice prompt helper also correctly says thinking is controlled separately in the Voice section.

The new spacing, diagnostics layout, loaded Voice default and helper text were checked in an isolated native QA copy. I closed that copy after the checks. Fresh real-model qualification reports for the two verified Gemma models pin the changed Voice prompt; earlier reports remain historical evidence.

## Review fixes (2026-10-20)

A four-part Sonnet review of the uncommitted tools work (tool loop, network tools, sandbox, page and contract) found three blocking defects and ten more worth fixing. All are fixed with regression tests; afterwards 300 Rust tests pass (22 ignored), `build-full` compiles, and the frontend suites pass (tools: 17 checks). Not yet re-checked in the running app.

- **Confirmations never appeared and always counted as accepted.** Tauri v1's injected script replaces `window.confirm` with a function that returns a Promise (always truthy), and `dialog.confirm` was not allowlisted. Every confirmation (tools consent, the small-model warning, deleting the Python runtime, deleting a saved voice, and now removing the Brave key) goes through `confirmAction` in `src/confirm-dialog.ts`, a native dialog that treats any failure as Cancel; `dialog.confirm` is allowlisted with the `dialog-confirm` Cargo feature. The tools test mocks `window.confirm` the way Tauri really behaves and fails if any source calls it.
- **A panicked turn hung unload and exit.** A `FinishOnDrop` guard ends a turn whose task panics or is dropped, with one terminal error; unload, model switch and exit wait at most 60 s for turns to drain.
- **Thinking fillers never played while reasoning streamed.** The speech thread runs its filler timer at least every 100 ms (`tick_if_due`), not only when no event arrives.
- **Today's date in every prompt (added afterwards):** every Chat and Voice system message ends with "Today's date: Tuesday, 20 October 2026.", with or without tools.
- **The prompt cache never matched across turns.** The tools policy in the system message carried the time to the second. It now carries the date and time zone only, so the system message is identical all day; the time to the minute is appended to the newest user message instead, as "(Local time when sent: HH:MM)". Earlier user messages come back from the page without it, so the next turn re-reads only the latest exchange. The policy wording, and so the qualification evidence, is unchanged.
- **Web text in the app's voice and history.** The weather filler names the city only in the first round, like search; the history note lists source host names instead of page titles.
- **"captcha" in a result page meant "unavailable".** Block detection now relies on a page with no result elements; sponsored results are skipped.
- **Stop in voice** stops queued audio before waiting for a tool to wind down; **chips** of an interrupted or failed voice answer are marked cancelled.
- **Choosing DuckDuckGo** asks before deleting the saved Brave key; a **custom thinking limit** above 1,048,576 is shown as invalid and not saved.
- **The settings file** is written through a temporary file and renamed (owner-only on Unix); **installing the Python runtime** moves the old one aside and restores it if the swap fails; **malformed tool-call arguments** are replayed to llama.cpp as `{}`.
- Also fixed in passing: whitespace before a tool call no longer suppresses its filler.

A second pass the same day fixed most of the smaller items:

- **Weather:** a missing high, low or rain chance is reported as "not available" instead of failing; "City, Country" names search the city and use the rest to pick the match.
- **Python:** stderr keeps the end of a traceback (its last line is the exception); summaries say which limit stopped a run, and the output notes the 256 MB memory limit; the epoch ticker runs only while code runs; a runtime download can be cancelled from its row (Cancel download), and a cancelled or failed install leaves no staging folder and returns to "not installed"; the installed size is measured once instead of on every status call.
- **Pages:** an HTTP 401/403/404/429 from a page is worded for the page, not the search key.
- **Notices:** `notices/assistant-tools.txt` adds the MPL-2.0 text and source locations for `selectors`, `cssparser`, `cssparser-macros` and `dtoa-short`, and lists all 228 crates the tools features pull in (macOS arm64 and Windows x64) with their declared licences.
- **Sandbox test:** the isolation script also tries mkdir, unlink, rename, writing an existing stdlib file and symlinks, and prints which network barrier applied. Run against the installed runtime (`VOICEREADER_SANDBOX_INSTALLED_DIR`), it reports `network=no-sockets`: this CPython build has a `_socket` module without real sockets or name lookup, and WASI grants none. CPU-loop and sleep deadlines still stop at about 0.50 s for a 0.5 s limit with the per-run ticker.
- **Start failures reported twice:** checked, not changed. The chat page and voice mode each ignore whichever of the event and the command rejection comes second, so the user sees one error.

## Live test findings and the MiniCPM and MiMo work (2026-10-20)

I tested MiMo-V2.6 9B in Voice on macOS with tools on. Its tool calls work: web search, page reading and weather ran and it answered from the results. Two things went wrong: a turn with failing searches ended with raw tool-call text as its answer, and its answers ignored the Voice prompt's formatting rules. I asked for the first to be fixed before the 0.3.0 release and for MiMo to pass the qualification, because I do not want to release a curated list with models that do not work. This section records what was found, what was changed and the evidence. Each claim says whether it was confirmed.

The new measurements were made on Windows 11 with an RTX 5090 (Vulkan) and the Windows `llama-server` b11718, not on the Mac's Metal backend that produced the earlier reports. MiMo's first run here gave exactly the Mac's scores (11/12 Chat, 10/12 Voice, same hard cases failing), which is some evidence that the results carry over, but they have not been re-run on the Mac.

### 1. Raw tool-call text as the answer

What I saw: I asked for Causeway Point's opening hours with the round limit at 5. `web_search` kept returning "Search is unavailable right now." and the model kept trying. In two separate turns it ended with `<tool_call><function=fetch_page><parameter=url>https://www.causewaypoint.com.sg/visit</parameter></function></tool_call>` as the Assistant's answer.

Confirmed cause. With the raw reply captured (`VR_TOOLS_DEBUG=1`), MiMo writes well-formed XML calls as plain text whenever the request carries no `tools`: at the round limit, and again in the first recovery request. llama.cpp only turns that format into a structured call when `tools` are present, so the text came back as the answer. The "do not call tools" line sat in the system prompt, far from where the model writes its reply, and MiMo ignored it. The loop also did nothing for a reply that was only call text, and `finalAnswer` in `src/assistant-tools.ts` returns that text as the answer, so in Chat it was also what the history kept. Nothing is executed from the text, so this was a failed answer, not a safety problem.

Fixed, each with a test (disabling the detection makes four of the new tests fail):
- Whenever tools go away after one has run (round limit, repeated bad calls, a context that no longer fits), the model is told so in the system message and again after the last tool result, which is the last thing it reads: "write your answer now in plain sentences; if a tool failed, say so briefly instead of guessing". Before, only a cut-off or empty call got that line.
- Text from the first `<tool_call>`, `<function=` or `<|tool_call` marker on is held back from the page and the speech queue and is never the answer. A held-back `<` that turns out to be ordinary text is released at the end of the round. Text written before the marker is kept.
- A final reply that is only call text, or a call the model may no longer make, gets one more request with the notice; if that also fails the user gets "Sorry, I couldn't finish that. Please try again." instead of an empty or raw answer. The Voice speech worker flushes at the end of the turn, so the apology is spoken.
- After a `web_search` fails once in a turn, further searches in that turn are answered at once with "do not search again", so retries no longer use up the rounds. The repeat guard cannot catch them, because every retry had a different query.

### 2. Why the search failed is still not known

I suspect DuckDuckGo refused repeated requests, but the app cannot tell: any network or HTTP error, and any results page without result elements (a block, a CAPTCHA, a changed page), becomes the same message, and the only protection is a gap of at least 2 seconds between requests with no back-off after a failure. The message now names the Tools menu on the Assistant page (it said "Settings", which is the app's Settings page). Not done: separate messages for offline, blocked and changed page, a cool-down after a failure, and logging the HTTP status on a failure. A skipped call's reason is already visible when its chip is expanded; it was missing from my copy of the transcript.

### 3. Why MiMo failed the qualification, and what fixed it

All of these were measured on the recorded cases (12 cases, 3 seeds, thinking off unless stated). "Hard" is the Python and no-tool and city-clarification cases that must pass.

| Run | Chat | Voice | Notes |
|---|---|---|---|
| Mac report, 2026-10-19 | 11/12, hard fail | 10/12, hard fail | |
| This machine, same code as the Mac | 11/12, hard fail | 10/12, hard fail | reproduces the Mac |
| Time zone given as an offset only | 11/12, hard fail | 10/12, hard fail | still guesses a city (London, Paris) |
| Thinking on, both modes | 11/12, hard fail | 11/12, hard fail | only Python fails |
| Guidance, thinking off | 10/12, hard fail | **12/12, hard pass** | Chat still guesses a city |
| App defaults (Chat thinks), no guidance | **12/12, hard pass** | 10/12, hard fail | |
| App defaults and guidance | 12/12, hard pass | 12/12, hard pass | |

Causes, each confirmed by the runs above:
- **Python printed nothing.** MiMo writes `sum(i*i for i in range(...))` without `print`, gets empty output, and tries again until its rounds run out; two Chat runs ended in the apology. A trailing bare expression in `run_python` is now echoed like a REPL cell does (`print(...)` returns None and is not echoed). The warm interpreter start went from 0.118 s to 0.126 s. The Python check also wrongly rejected a labelled line such as `sum = 333383335000`; it now accepts the right number anywhere in the output. Gemma's recorded results are identical under both checks.
- **Voice with thinking off answers from memory** ("I don't have a live stock-price tool", an invented 31 point 4 degrees for Singapore, a wrong sum). Four short lines of model-specific guidance fix it: never state a current price, weather, news, date or exact calculation from memory; web_search finds stock prices and news; ask for a city instead of choosing one; call get_weather for any weather question and run_python for an exact calculation or a Python request.
- **Chat guessed a city.** Thinking fixes it, and Chat's Think toggle is on by default. It was not caused by the fixture naming `Asia/Singapore`: the real app only prints an offset in its policy line, and removing the name changed nothing.
- The first guess, that "do not use code" in the Voice prompt stopped MiMo running Python, was wrong: a line allowing it made no difference.

The guidance is a per-model catalogue field (`tools_guidance`, MiMo only), appended after the shared policy, so the Gemma prompts and their pins are unchanged. It is part of the prompt text each report pins.

Result: the official run (`2026-10-20-mimo-v2.6-9b-guided.json`, Chat thinking on and Voice off, as the app runs them) is **12/12 in both modes with every hard case passing**. On nine seeds that were not used for tuning (`...-fresh-seeds.json`) every case passed 9/9 except Voice Python and Voice named-city weather, 8/9 each. MiMo's flag is now on, and it is bound to that report by the catalogue regression test. Its limits: with Chat's Think switched off it guesses a city again, and in Voice it still sometimes answers a weather question from memory.

Voice formatting, from the same harness: MiMo's Voice answers are plain sentences and use "28 point 8" style decimal wording. That suggests the Markdown and decimal dots in my live transcript came from the Voice prompt saved in my app data rather than from the model. Not confirmed: check Settings, since only Reset to default adopts the new default prompt.

### 4. MiniCPM-V 4.7 1B

It started at 7/12 Chat and 8/12 Voice. It calls tools for "17 times 23" and for rewriting text, answers prices from memory and guesses a city. With the fixes alone it reached 9/12 in both modes. With its own guidance (`MINICPM_TOOLS_GUIDANCE`: call a tool only for something current, a page or Python; answer greetings, explanations, rewrites and simple sums yourself) and thinking on, the official run is 11/12 in both modes with every hard case passing (`2026-10-20-minicpm-v-4.7-1b-guided.json`). Thinking is on for any turn with tools, whatever the toggle says, because Voice with thinking off stayed at 10/12 and failed hard cases (`tools_need_thinking` in the catalogue).

I do not count it as verified. On six seeds that were not used for tuning (`...-fresh-seeds.json`), Voice Python passed 3 of 6, Voice named-city weather 2 of 6 (it usually invents the weather), Chat stock 0 of 6 and Chat rewrite 4 of 6. It passed the three official seeds by luck. Its flag stays off, so it keeps the reliability warning and the override. Open decision: keep it with the warning, block tools for it so it is chat-only, or remove it from the catalogue.

### 5. Gemma re-check

The loop, sandbox and recovery changes touched code the Gemma qualification ran through, so both were run again on the current build with the original protocol (thinking off): Gemma E4B 12/12 Chat and 11/12 Voice, Gemma 12B 12/12 in both modes, all hard cases passing, the same scores as before. The catalogue regression test now binds their flags to these 2026-10-20 reports; the earlier Mac reports stay as history.

### 6. Changes to the qualification harness and its fixtures

- `VR_TOOLS_THINKING=app-defaults` runs Chat with thinking on and Voice off, as the app does, and the report records the thinking each mode used. A model with `tools_need_thinking` always thinks. Voice thinking is capped at 240 tokens, the app's first-turn budget.
- Experiment switches: `VR_TOOLS_CASES`, `VR_TOOLS_MODES`, `VR_TOOLS_SEEDS`, `VR_TOOLS_THINK=1`, `VR_TOOLS_TEMPERATURE`, `VR_TOOLS_POLICY_TIME`, `VR_TOOLS_EXTRA_SYSTEM`, `VR_TOOLS_DEBUG`. Any of them marks the run as an experiment, which prints its results and never writes `qualified: true`. Each run also prints one line per case and seed.
- The report's prompt hashes now include the model's `tools_guidance`.
- The catalogue regression test checks that a report's Voice thinking matches the model's `tools_need_thinking`.
- `.gitattributes` marks `src-tauri/tests-data/tools/*` as `-text`. The test and the reports hash those fixtures byte for byte, and a Windows checkout with `core.autocrlf` turned them into CRLF, so the check could never pass there and a report made on Windows would have pinned the wrong hash.

### 7. Limits

- All new evidence is from Windows with Vulkan. It should be repeated on the Mac before the release.
- MiMo's margin in Voice is thin for Python and named-city weather (8/9). Qualification is evidence for these cases, not a guarantee.
- Formatting adherence is still not measured for any model.
- The error classes, cool-down and status logging for search failures (section 2) are not done.

Left for the planned page-fetch rework: trimming pasted URLs, HTML extraction gaps (`header`, `form`, first `main`/`article`, empty pages) and non-UTF-8 charsets. Still open: the sandbox's behaviour tests need the runtime and stay `#[ignore]` by default, and VoiceReader as a whole has no generated notices file for every crate it links.
