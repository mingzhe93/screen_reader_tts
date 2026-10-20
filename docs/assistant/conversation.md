# Voice input and voice conversation (frontend)

Status on 2026-10-19: the base Assistant is committed in `5e62732`, and I have tested it working on macOS. This document now includes the tools-phase extensions; [tools.md](tools.md) records their validation and remaining manual checks. I have since tested it on Windows too, and it works (2026-10-20). The Rust voice-input path is in [voice-input.md](voice-input.md); the page is in [ui.md](ui.md).

This covers two features of the Assistant page:

- **Push-to-talk on the chat page.** Dictate into the message box; nothing is sent until you press Send.
- **Voice mode** (the Chat | Voice switch). Talk, and hear the answer spoken phrase by phrase, with a transcript of both sides.

It implements `docs/v0.3.0_assistant_plan.md`:

- §3.2: push-to-talk input on the chat page (hold or toggle).
- §3.3: voice mode, push-to-talk as the turn boundary, the microphone closed while the assistant speaks, a press interrupting the answer, reasoning originally off (the tools phase adds an optional Think toggle), and the experimental auto-send toggle.
- §6.1 and Codex F8/F9: heard text comes only from `played_seq` acknowledgements, and the watchdog detects stalls rather than imposing a total-duration deadline.
- §6.2: a read-aloud request preempts a voice answer.
- §6.3: an interrupted answer is kept in history up to the last played phrase, followed by `[interrupted by user]`.
- §7.2 and Codex F7: a transcript that finishes while a newer turn is recording is held and joined to that turn.
- §7.4: auto mode reacts to the backend's own turn events.

## Files

| File | Role |
|---|---|
| `src/assistant-turns.ts` | The pure rules (no DOM, no Tauri): `HoldToggle`, `TurnJoiner`, `heardText`, `cutShortText`, `buildVoiceHistory`, `heardPrefixLength`, `appendToDraft`, `newPlaybackWatch` / `notePlayback` / `noteSpeechDone` / `playbackVerdict`, `levelFraction`, and the constants `HOLD_MS` (300), `STALL_MS` (5000), `QUEUED_SECS` (0.2), `INTERRUPTED_MARK`, `STOPPED_MARK`. |
| `src/assistant-mic.ts` | `createMic(log)`: the one microphone session, shared by both features. `open`, `begin`, `end`, `cancel`, `setAuto`, `close`, `subscribe(events)`, and `state` (`closed` / `opening` / `open`). `micClosedMessage(reason)`. |
| `src/assistant-voice.ts` | `createPushToTalk({ mic, log, input, onDraftChanged, ownsMic })` and `createVoiceMode({ mic, log, unavailableReason, sampling, onNewChat })`. Wires the rules to the commands, events and DOM. |
| `src/assistant.ts` | Creates the three above, places them, and calls `setMode`, `voice.setEntered`, `voice.refresh`, `ptt.cancel` and `mic.close` on mode, page and status changes (see [ui.md](ui.md), "Voice"). |
| `src/types.ts` | `AssistantMicOpenResult`, `AssistantMicLevelPayload`, `AssistantMicTextPayload`, `AssistantMicErrorPayload`, `AssistantMicClosedReason`, `AssistantMicClosedPayload`, `AssistantMicTurnPayload`, `AssistantVoiceRespondResult`, `AssistantVoicePhrasePayload`, `AssistantVoiceEndedPayload`, `AssistantPlaybackPayload`. |
| `src/styles.css` | In the Assistant block: `.assistant-ptt*` (composer button, meter, status) and `.assistant-voice*`, `.assistant-talk*`, `.assistant-vt-*` (voice view). |
| `scripts/test-assistant-voice.mjs` | `npm run test:assistant-voice`. |

`src/main.ts` (not changed here) relays the player's progress as `assistant:playback` while the playing job is an Assistant voice answer (`kind: "assistant"` in `voicereader:job-started`).

## Backend contract

Tauri v1: `invoke` arguments are camelCase; payload fields are snake_case.

### Microphone (one session at a time; it holds the speech recogniser)

| Command | Args | Result |
|---|---|---|
| `assistant_mic_open` | `{}` (optional `device` not sent yet: the default microphone is used) | `{ ok, message }`. The frontend treats `ok: false` and a rejection the same way and shows the message, for example "Transcription in progress. Stop it on the Transcribe page, then try again." |
| `assistant_mic_begin` | none | `turn_id: number` |
| `assistant_mic_end` | `{ turnId }` | nothing; the text arrives as `assistant:mic-final` |
| `assistant_mic_cancel` | `{ turnId }` | nothing; no final follows |
| `assistant_mic_close` | none | nothing |
| `assistant_mic_set_auto` | `{ enabled }` | nothing; rejects when turn detection is not installed. `false` **pauses** the detector and `true` resumes it (its models stay loaded for the session), so it is called around every answer |

| Event | Payload | Use |
|---|---|---|
| `assistant:mic-level` | `{ rms }` | level meter (`levelFraction`: −50 dB → 0, −10 dB → 1) |
| `assistant:mic-partial` | `{ turn_id, text }` | live text while recording |
| `assistant:mic-final` | `{ turn_id, text }` | once per ended turn |
| `assistant:mic-error` | `{ message }` | the session is closed; the message is shown |
| `assistant:mic-closed` | `{ reason: "transcription" \| "error" \| "closed" \| "exit" }` | the session ended; "transcription" shows "Voice input stopped because Transcribe started…" |
| `assistant:mic-turn` | `{ turn_id, state: "started" \| "ended" }` | auto mode only. A turn that is open when detection is paused gets "ended" and **no** `mic-final` (it was discarded) |
| `assistant:mic-auto-failed` | `{ message }` | automatic sending failed and Rust switched it off; the microphone stays open and push-to-talk works |

`createMic` treats `mic-error` and `mic-closed` as a lost session only while the session is `open`. While one is opening, the open command reports the failure itself; after the page's own `close()`, a late event must not affect a newer session.

### Voice answers

| Command | Args | Result |
|---|---|---|
| `assistant_voice_respond` | `{ requestId, messages, sampling, conversationId, tools, enabledTools, think, thinkingBudget, toolLimits }` (see [tools.md](tools.md)) | `{ request_id, prompt_tokens, dropped_messages, job_id }` |
| `assistant_voice_cancel` | `{ requestId }` | nothing |

Rust uses the saved Voice system prompt with thinking off by default (the optional Voice Think control enables it), streams the text as `assistant:chat-delta` / `chat-done` / `chat-error` with this `request_id` and round metadata. When thinking is enabled, reasoning also reaches the collapsed transcript accordion; only content is sent to the speech queue as playback job `job_id`.

| Event | Payload |
|---|---|
| `assistant:voice-phrase` | `{ request_id, job_id, seq, text, kind: "answer"|"filler", round }`: the phrase about to be spoken |
| `assistant:voice-ended` | `{ request_id, job_id, reason: "done" \| "cancelled" \| "preempted" \| "error", message }`. "done": every phrase was handed to the player |
| `assistant:playback` (from `main.ts`) | `{ job_id, buffered_secs, played_seq, drained }`: about every 250 ms, once per played phrase, once with `drained: true` |

The request id is registered before `assistant_voice_respond` is awaited, because its events can arrive first. The job id is taken from the first of `voice-phrase` or the command result. Every event whose `request_id` or `job_id` is not the current answer's is ignored; after an answer ends there is no current answer, so late events of a cancelled one change nothing.

## Push-to-talk on the chat page

- The mic button sits between Attach and New chat. **Hold** it (pointer down until up, or Space while it has focus) to record one turn; a **short click** (under 300 ms) starts recording and the next press stops it; Enter is a short click. One handler does all of this (`HoldToggle`). The button has `aria-pressed`, an `aria-label` and a tooltip. It is never `disabled`, because a disabled button would swallow the release of a press in progress.
- The first press opens the microphone and shows "Starting microphone…" until it is ready (1–3 s the first time). A release during that time ends the turn as soon as it has begun.
- While recording: a level meter beside the button, and "Listening… <partial text>" under the composer. After release: "Transcribing…".
- The final text is **appended** to the message box (`appendToDraft`: one space between the existing text and the new text unless the draft ends in whitespace), focus moves there with the caret at the end, and nothing is sent.
- Errors (open failure, `mic-error`, Transcribe taking the recogniser) show in a line under the composer.
- The microphone is closed after **60 s without use**, when leaving the Assistant page, when switching to voice mode and back, and when the model stops being ready (Eject, model switch, crash).
- Finals arrive in order from the backend, so several quick turns are appended in order; the F7 join is not needed here.

## Voice mode

### Layout

- The main area shows a 128 px round **talk button** inside a ring that grows with the microphone level (`--level`), a **state line**, the partial text while recording, and an error line.
- Beside it, or under it when the area is narrower than 680 px (`data-narrow`, set by a resize observer in `assistant-voice.ts`), the **transcript pane**: "You" and "Assistant" entries and a New chat button. Assistant text shows what has been heard (played phrases) in normal colour and the rest muted. An interrupted or stopped answer shows the unheard rest struck through and an "Interrupted" or "Stopped" tag. Errors show under the entry. Text is plain (`textContent`), never Markdown or HTML. Both columns fill the page height and only the transcript list scrolls, so the talk button and the Tools button stay in view however long the conversation gets.
- State lines: "Hold to talk", "Starting microphone…", "Listening…", "Thinking…" (waiting for the final transcript or for the first phrase), "Speaking — press to interrupt", "Didn't catch that" (2.5 s), an error, or, when the model is not usable, "Load the model to start talking.", "Loading model…", "Unloading…", "Another VoiceReader window is using the model." or "Install a model on the Models page to talk." (the button is then inert). In auto mode: "Start talking. It sends when you finish. Press to pause.", "Paused. Press to listen again." (crossed-out microphone icon) and, after the microphone was lost, "Press to start listening".
- The right panel stays. In voice mode the chat Reasoning section is hidden; the **Voice** section provides its own Think toggle, thinking limit and the toggle "Send automatically when I stop talking (experimental)" with a note that it uses voice activity detection plus Smart Turn and may cut you off. It is stored as `voicereader.assistant.autosend`; the default is on in a dev build (`import.meta.env.DEV`) and off in a release build. If `assistant_mic_set_auto(true)` rejects, the checkbox shows unticked with "Automatic sending stopped: <reason>", push-to-talk is used, and ticking it again retries. Concurrent enables are safe: the backend serialises the turn-model check, so a second call waits for a download in progress instead of failing.
- On `assistant:mic-auto-failed` the message is shown in the state line and under the toggle ("Automatic sending stopped: <message> The talk button works as usual."), the toggle shows off and the talk button works as push-to-talk. The stored preference is not changed: ticking the toggle again retries, and the next time voice mode is entered auto mode is tried again.

### Conversation and lifecycle

- Voice mode has **its own conversation**, separate from the chat one. Switching modes keeps both. New chat (in the transcript pane) clears only the voice conversation; New chat in the composer clears only the chat.
- Entering voice mode (with the page visible and the model ready) opens the microphone, then turns auto mode on if it is chosen. Leaving it, or leaving the page, cancels any answer (kept as heard so far, `[interrupted by user]`), cancels any recording turn, drops held text and closes the microphone. Showing the page again in voice mode reopens it.
- While the model is not ready the microphone is closed (and an answer in progress is interrupted); it reopens when the model is ready again. After a failed open or a lost session it is not reopened automatically; the next press tries again.

### Turn-taking rules

1. **Press while an answer is active** (thinking or speaking): `assistant_voice_cancel`, and the assistant turn is recorded as the played phrases joined with spaces, then `\n\n[interrupted by user]`, or `[interrupted by user]` alone if nothing was heard. Then the new turn begins.
2. **Release:** `assistant_mic_end`. When the final arrives (`TurnJoiner.final`):
   - **F7 join.** If a newer turn has begun (pressed, even if its `assistant_mic_begin` has not returned yet), the text is held and joined, with a space, to the start of the newer turn's text when that turn ends. Several held turns join in order. If the newer turn fails to start or is cancelled, the held text is sent on its own.
   - Empty text after joining: "Didn't catch that", then idle.
   - Otherwise the user turn is added to the transcript and `assistant_voice_respond` is called with the voice history plus this turn (`buildVoiceHistory`): user and assistant alternate, assistant turns use their history text (the full answer, or the heard part plus a marker), and exchanges that failed, are still running or produced no text are left out.
3. **Answer lifecycle:**
   - `chat-delta` content is appended to the transcript entry (at most one render per animation frame).
   - `voice-phrase` adds a phrase and switches the state to "Speaking — press to interrupt". `played_seq` from `assistant:playback` advances the heard part.
   - `drained: true` for the current job completes the answer: its history text is the full answer, and the state returns to "Hold to talk" (auto mode: `assistant_mic_set_auto(true)`).
   - `voice-ended` "done" with no phrase at all completes the answer at once, because no `drained` will follow.
   - `voice-ended` "preempted" (read-aloud took over), or a "cancelled" this page did not ask for: the heard part is recorded with `[stopped]`, then idle.
   - `voice-ended` "error", `chat-error`, or a rejected `assistant_voice_respond`: the message is shown and the exchange is left out of history.
4. **Stall watchdog** (checked every 500 ms while an answer is active, `playbackVerdict`):
   - "stalled": more than 0.2 s of audio is queued (`buffered_secs > 0.2`) and no progress (a changed `buffered_secs` or a new `played_seq`) has arrived for 5 s.
   - "undrained": `voice-ended` "done" arrived but `drained` has not followed within the last reported `buffered_secs` + 5 s, counted from the later of "done" and the last progress.
   - Either one cancels the answer, shows "Playback stopped unexpectedly", leaves the exchange out of history and goes idle. A pause in model output (nothing queued) is never a stall.
5. **Auto mode** (toggle on and accepted by the backend): the button does not begin or end turns. `assistant:mic-turn` "started" shows "Listening…" (and interrupts an answer if one is somehow running, plan §7.2), "ended" shows "Thinking…", and finals go through the same F7 and answer rules. `assistant_mic_set_auto(false)` (pause) is sent when an answer starts and `true` (resume) after it drains, is interrupted, stopped or fails. Pressing the button during an answer interrupts it, and turn detection then resumes. Pressing it otherwise **pauses** listening (`set_auto(false)`; an automatic turn still open is discarded, its "ended" arrives without a final); the next press resumes. A pause holds through answers that were already on their way (no `set_auto(true)` while paused) and is cleared by re-entering voice mode or ticking the toggle. Each `applyAuto` call carries a sequence number, so an older enable that resolves after a newer pause cannot mark detection as on.
   - The page remembers the last state it asked for (`autoListening`, set before the command returns). While it is paused or off, a "started" is ignored (it raced the pause and Rust will discard that turn), and an "ended" means the turn was discarded: the page stops waiting for its final (`TurnJoiner.cancel`), and any text held for it by F7 is sent on its own. Switching the toggle off with a turn open is handled the same way. `mic-auto-failed` with a turn open discards it too.

### Decisions made here (please review)

- A failed playback (watchdog) counts as a failed exchange and is left out of history, like other failures, even though part of it may have been heard.
- The heard part shown in the transcript is located in the streamed text by counting letters and digits (`heardPrefixLength`), because phrases are the speakable form of the text (Markdown markers stripped). If the backend rewrites words in phrases (for example "9" to "nine"), the split is approximate; the history text uses the phrases themselves and is exact.
- A 300 ms press separates hold from click.
- The undrained deadline counts from the later of "done" and the last progress report, so a long queue that is still playing is not cut off.
- `assistant_mic_open` is called without `device`, so the system default microphone is used, not the one chosen on the Transcribe page.

## How to test

Automated (no backend):

```
npx tsc --noEmit
npm run build
npm run test:assistant-voice
npm run test:assistant
npm run test:assistant-fit
npm run test:player
grep -nE "innerHTML|outerHTML|insertAdjacentHTML|DOMParser|document\.write" src/assistant*.ts   # prints nothing
```

`test:assistant-voice` bundles `src/assistant-turns.ts` with esbuild and checks 28 cases: hold versus toggle, key repeat, reset during a hold; F7 holding and joining (also while the newer turn's begin is pending, three turns, an empty newer turn, a newer turn that fails or is cancelled, stale finals, reset); heard text order; the interrupted and stopped markers; history alternation and the excluded exchanges; the heard-prefix split with Markdown and accented text; appending to the draft; the watchdog for a slow model, a stalled queue, repeated identical reports, the 0.2 s threshold, the drain deadline and its extension by progress; and the level scale.

Browser check, done on macOS with a temporary mocked `window.__TAURI_IPC__` (since deleted) that simulated the commands and events above, in an 800×600 window, dark and light themes: push-to-talk by hold, by click toggle and by Space, appending to an existing draft without sending; a mic error and an open failure shown under the composer; voice mode with a full answer (phrases, `played_seq`, `drained`) and its history on the next turn; interrupting during speech (cancel sent, heard text plus `[interrupted by user]` in the next request's history, late events of the cancelled answer ignored); F7 (release A, press B, A's final during B, one request with the joined text); "Didn't catch that"; preempted (`[stopped]` in history); a `voice-ended` error left out of history; the watchdog not firing during 6 s of model silence, firing 5 s after a stalled queue, and firing when `drained` never came after "done"; a mic error in voice mode and recovery on the next press; auto mode (button inert when idle, mic-turn started/ended, a "started" racing the pause ignored, an "ended" without a final after toggling off not leaving "Thinking…", `mic-auto-failed` showing the message, unticking the toggle without changing storage and falling back to the button, and ticking it again retrying, F7 across auto turns, `set_auto(false)` during the answer and `true` after drain or interrupt, an interrupt by button, a mic-turn during an answer, the "not installed" rejection); switching modes (answer cancelled and kept as heard, microphone closed and reopened, both conversations kept, each New chat clearing only its own); leaving the page (push-to-talk turn cancelled, microphone closed); Eject (microphone closed, button inert) and Load (reopened, auto back on). Not covered there: the 60 s idle close (code-reviewed only) and real audio.

### Manual checks with the real backend

I have tested and accepted Voice conversations and tools in the packaged macOS app. Use this list for regression checks with the transcription model downloaded and an Assistant model loaded; it is not an item-by-item record of every edge case. Windows x64 still needs its own build and microphone/playback validation.

1. **Push-to-talk, hold.** On the chat page, type "Note:", hold the mic button, say "buy milk", release. Expected: "Starting microphone…" the first time, then "Listening…" with the meter moving and the partial text, then "Transcribing…", then the box reads "Note: buy milk" with the caret at the end. Nothing is sent.
2. **Push-to-talk, toggle and Space.** Click once (short), speak, click again: the text is appended. Tab to the mic button, hold Space, speak, release: appended again.
3. **Idle close.** After a push-to-talk turn, wait 60 s, then start a transcription on the Transcribe page. Expected: it starts (the recogniser was freed). Pressing the mic button again reopens the microphone.
4. **Transcribe exclusion.** Start a Transcribe recording, then press the mic button. Expected: "Transcription in progress. Stop it on the Transcribe page, then try again." under the composer; the recording is not stopped. With the microphone open, start Transcribe: the Assistant shows "Voice input stopped because Transcribe started…".
5. **Voice mode, full answer.** Switch to Voice. Hold the talk button, ask "What is the capital of France? Answer in two sentences.", release. Expected: "Thinking…", then "Speaking — press to interrupt" as the voice starts; the transcript text turns from muted to normal phrase by phrase in step with the audio; when the audio ends the state returns to "Hold to talk". The panel shows the Voice section and no Think control.
6. **Interrupt.** Ask for a long story. During the second or third sentence, press the button and ask "What was the last thing you said?". Expected: the audio stops at once; the story entry shows the unheard rest struck through and "Interrupted"; the answer refers only to what was actually heard.
7. **F7 join.** Hold, say "What is the weather", release, and within half a second hold again and say "in Paris tomorrow", release. Expected: one user entry "What is the weather in Paris tomorrow" and one answer.
8. **Nothing said.** Press and release without speaking. Expected: "Didn't catch that", no request.
9. **Preempted.** During a voice answer, use the read-selection hotkey on some text in another app. Expected: the read-aloud starts, the voice answer stops, its entry shows "Stopped", and the next voice answer's context ends with `[stopped]` (check in the llama-server log or diagnostics if needed).
10. **Watchdog.** Hard to force with the real player; if an answer ever hangs while speaking, expect "Playback stopped unexpectedly" within about 5 s and a usable button. A slow model (long pause before the first phrase, for example the 12B model on CPU) must **not** trigger it.
11. **Auto mode.** Turn on "Send automatically when I stop talking". (If detection fails while running, expect "Automatic sending stopped: …", the toggle unticked and the talk button working.) Expected: "Start talking. It sends when you finish."; speak a question and pause; it sends by itself, the final transcript appears, and the microphone does not pick up the spoken answer (it is closed while speaking). Press the button during the answer: it is interrupted, and you can speak again without pressing. Press the button while idle: it shows "Paused. Press to listen again." with a crossed-out microphone, and speaking does nothing; press again to resume. Pause while speaking mid-sentence: that turn is dropped, not sent. On the very first use the turn models (about 11 MB) download first; if that fails (offline, or another Assistant download is running), the toggle shows "Automatic sending stopped: …" and ticking it again retries. Note mid-sentence cut-offs.
12. **Leaving.** During a voice answer switch to Chat: the audio stops and the chat conversation is intact; switch back: the voice transcript is intact. During an answer go to another page: the audio stops. Then start Transcribe: it is allowed (the microphone was closed).
13. **Eject.** In voice mode press Eject: the button becomes inert with "Load the model to start talking."; press Load: after loading, the button works again without leaving voice mode.
14. **Narrow window and themes.** Make the window narrow: the transcript moves under the talk button. Check both themes: the ring, the muted unheard text, the tags and the error colour stay legible.
15. **Windows specifics.** Check that holding the button with a touch screen or pen records (pointer capture), and that Space on the focused button does not scroll the page.

## Known gaps and next steps

- Not run against the real backend; the contract above is the coordinator's and may still change (in particular whether `assistant_mic_open` rejects or returns `ok: false`, and the `job_id` type, compared as a string here).
- No microphone choice: the default device is used. The Transcribe page's device list could be shared and passed as `device`.
- A short first phrase at 2x speed may not reach the player's start threshold (0.85 s at 2x); if the model then pauses for more than 5 s, more than 0.2 s is queued without progress, and the watchdog reports a stall that is really a slow model. If that shows up, ignore stalls until the first `played_seq` or until the queue first shrinks, or have the player start streaming jobs at a lower threshold.
- A turn longer than 120 s is ended by the backend; its final then arrives while the button may still be held and is handled as a normal final (the release afterwards does nothing).
- Voice conversations, like chats, are not saved.
- The partial transcript is not shown in the transcript pane itself, only under the button.


## Tool-round extensions

The model turn spans every tool round. App-owned fillers, model preambles and final answers share one ordered speech queue with a global sequence and round/kind tags. Preamble and filler text remain visible, but only played final-round answer phrases enter history. The watchdog remains enabled while tools run. Automatic listening waits for both terminal model metadata and actual playback drain. New chat revokes the old conversation’s URL grants, including in-flight search completions. See [tools.md](tools.md) for the complete contracts and regression coverage.

Chat and Voice each expose the wrench Tools menu with All tools, per-tool selection and expandable settings. These settings live only on the Assistant page. When Voice thinking is enabled, a collapsed Thinking accordion in each exchange displays reasoning from all rounds; only content goes to speech, and reasoning never enters later history. Opening the accordion is preserved while streaming. The request’s `enabledTools` list is immutable for that turn.
