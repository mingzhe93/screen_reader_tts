// Voice on the Assistant page: push-to-talk in the chat composer (Step 2) and voice
// conversation mode (Step 3, auto-send from Step 4). The turn-taking rules themselves are
// pure functions in assistant-turns.ts; this module wires them to the microphone session
// (assistant-mic.ts), the voice answer commands and events, and the DOM.
// Behaviour, events and manual tests: docs/assistant/conversation.md.
// The DOM is built with createElement and textContent only.

import { ToolTranscript, createThinkingLimit, createToolsToggle, finalAnswer, historyWithNote, renderToolTranscript, toolLimits } from "./assistant-tools";
import { invoke } from "@tauri-apps/api/tauri";
import { listen } from "@tauri-apps/api/event";
import { button, errorText, h, readStored, svgIcon, writeStored } from "./assistant-dom";
import { MIC_OPEN_ABANDONED, micClosedMessage, type Mic } from "./assistant-mic";
import {
  HoldToggle,
  INTERRUPTED_MARK,
  STOPPED_MARK,
  TurnJoiner,
  appendToDraft,
  buildVoiceHistory,
  cutShortText,
  heardPrefixLength,
  heardText,
  levelFraction,
  newPlaybackWatch,
  notePlayback,
  noteSpeechDone,
  playbackVerdict,
  type FinalOutcome,
  type PlaybackWatch,
  type VoiceExchangeRecord,
  type VoicePhrase,
} from "./assistant-turns";
import type {
  AssistantChatDonePayload,
  AssistantModelInfo,
  AssistantToolCallPayload,
  AssistantToolResultPayload,
  AssistantTurnPhasePayload,
  AssistantChatDeltaPayload,
  AssistantChatErrorPayload,
  AssistantPlaybackPayload,
  AssistantSampling,
  AssistantVoiceEndedPayload,
  AssistantVoicePhrasePayload,
  AssistantVoiceRespondResult,
} from "./types";

type Log = (message: string, level?: "info" | "error") => void;

const AUTOSEND_KEY = "voicereader.assistant.autosend";
/** The chat page closes an unused microphone after this long, to free the recogniser. */
const PTT_IDLE_CLOSE_MS = 60_000;
const NOTICE_MS = 2500;
const WATCHDOG_TICK_MS = 500;
const NEAR_BOTTOM_PX = 80;
/** Content width below which the talk button and the transcript stack instead of sitting side by side (300 + 340 + gap). */
const VOICE_SIDE_BY_SIDE_MIN_PX = 680;

export const micIcon = (size = 16) =>
  svgIcon(["M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3z", "M19 10v2a7 7 0 0 1-14 0v-2", "M12 19v3"], [], size);
const micOffIcon = (size = 16) =>
  svgIcon(
    [
      "M2 2l20 20",
      "M18.89 13.23A7.12 7.12 0 0 0 19 12v-2",
      "M5 10v2a7 7 0 0 0 12 5",
      "M15 9.34V5a3 3 0 0 0-5.68-1.33",
      "M9 9v3a3 3 0 0 0 5.12 2.12",
      "M12 19v3",
    ],
    [],
    size,
  );

/**
 * One handler for hold and toggle (see HoldToggle): pointer down/up, and Space down/up while
 * the button has focus. Enter is a short press, so it toggles. The button is never `disabled`
 * (a disabled button would swallow the release of a press already in progress); `press`
 * decides whether a press counts, and a release without a counted press does nothing.
 */
function bindTalkButton(element: HTMLButtonElement, press: () => void, release: () => void): void {
  let spaceDown = false;
  element.addEventListener("pointerdown", (event) => {
    if (event.button !== 0) return;
    event.preventDefault();
    element.focus();
    try {
      element.setPointerCapture(event.pointerId);
    } catch {
      // Not every pointer can be captured; pointerup still arrives on the button.
    }
    press();
  });
  const up = () => release();
  element.addEventListener("pointerup", up);
  element.addEventListener("pointercancel", up);
  element.addEventListener("lostpointercapture", up);
  element.addEventListener("keydown", (event) => {
    if (event.key === " " || event.code === "Space") {
      event.preventDefault();
      if (event.repeat || spaceDown) return;
      spaceDown = true;
      press();
    } else if (event.key === "Enter") {
      event.preventDefault();
      if (event.repeat) return;
      press();
      release();
    }
  });
  element.addEventListener("keyup", (event) => {
    if ((event.key === " " || event.code === "Space") && spaceDown) {
      event.preventDefault();
      spaceDown = false;
      release();
    }
  });
  element.addEventListener("blur", () => {
    if (spaceDown) {
      spaceDown = false;
      release();
    }
  });
  // The pointer and key handlers do the work; a synthetic click must not do it twice.
  element.addEventListener("click", (event) => event.preventDefault());
}

// ---------------------------------------------------------------------------
// Push-to-talk on the chat page
// ---------------------------------------------------------------------------

type PttTurn = { id: number | null; endRequested: boolean; abandoned: boolean };

export type PushToTalk = {
  /** Goes into `.assistant-ptt-slot`. */
  element: HTMLElement;
  /** Status ("Starting microphone…", "Listening…" and the partial text), under the composer. */
  statusEl: HTMLElement;
  errorEl: HTMLElement;
  /** Drops any turn in progress (leaving the page or the chat mode, ejecting the model). */
  cancel(): void;
};

export function createPushToTalk(options: {
  mic: Mic;
  log: Log;
  input: HTMLTextAreaElement;
  onDraftChanged: () => void;
  /** False in voice mode, where voice mode owns the microphone. */
  ownsMic: () => boolean;
}): PushToTalk {
  const { mic, input } = options;
  const toggle = new HoldToggle();
  let turn: PttTurn | null = null;
  /** Turns ended and waiting for their final text. */
  const awaiting = new Set<number>();
  let opening = false;
  let partial = "";
  let error = "";
  let idleTimer = 0;

  const meterFill = h("span", "assistant-ptt-meter-fill");
  const meter = h("span", "assistant-ptt-meter is-hidden", meterFill);
  meter.setAttribute("aria-hidden", "true");
  const talk = h("button", "btn btn-ghost btn-sm assistant-ptt-btn", micIcon());
  talk.type = "button";
  talk.setAttribute("aria-label", "Voice input");
  talk.title = "Voice input: hold to talk, or click to start and click again to stop. Space works while the button has focus. The text is added to your message, not sent.";
  const element = h("span", "assistant-ptt", talk, meter);
  const statusEl = h("p", "caption assistant-ptt-status is-hidden");
  statusEl.setAttribute("aria-live", "polite");
  const errorEl = h("p", "assistant-error is-hidden");
  errorEl.setAttribute("role", "alert");

  function render(): void {
    const recording = !!turn;
    talk.classList.toggle("is-recording", recording);
    talk.setAttribute("aria-pressed", String(recording));
    meter.classList.toggle("is-hidden", !recording || opening);
    if (!recording) meterFill.style.transform = "scaleX(0)";
    let text = "";
    if (opening) text = "Starting microphone…";
    else if (recording) text = partial ? `Listening… ${partial}` : "Listening… Release or click again when you are done.";
    else if (awaiting.size > 0) text = "Transcribing…";
    statusEl.textContent = text;
    statusEl.classList.toggle("is-hidden", !text);
    errorEl.textContent = error;
    errorEl.classList.toggle("is-hidden", !error);
  }

  function clearIdle(): void {
    if (idleTimer) window.clearTimeout(idleTimer);
    idleTimer = 0;
  }

  function scheduleIdle(): void {
    clearIdle();
    idleTimer = window.setTimeout(() => {
      idleTimer = 0;
      if (!turn && awaiting.size === 0 && !opening && options.ownsMic()) void mic.close();
    }, PTT_IDLE_CLOSE_MS);
  }

  async function startTurn(): Promise<void> {
    const current: PttTurn = { id: null, endRequested: false, abandoned: false };
    turn = current;
    error = "";
    partial = "";
    clearIdle();
    render();
    try {
      if (mic.state !== "open") {
        opening = true;
        render();
        await mic.open();
        opening = false;
      }
      if (current.abandoned) return;
      const id = await mic.begin();
      current.id = id;
      if (current.abandoned) {
        void mic.cancel(id).catch(() => undefined);
        return;
      }
      if (current.endRequested) await endTurn(current);
    } catch (failure) {
      opening = false;
      if (turn === current) {
        turn = null;
        toggle.reset();
      }
      const message = errorText(failure);
      if (!current.abandoned && message !== MIC_OPEN_ABANDONED) {
        error = message;
        options.log(`Assistant voice input failed: ${message}`, "error");
      }
      scheduleIdle();
    } finally {
      render();
    }
  }

  async function endTurn(current: PttTurn): Promise<void> {
    if (turn === current) turn = null;
    if (current.id === null) return;
    const id = current.id;
    awaiting.add(id);
    render();
    try {
      await mic.end(id);
    } catch (failure) {
      awaiting.delete(id);
      error = errorText(failure);
    }
    scheduleIdle();
    render();
  }

  function stopTurn(): void {
    const current = turn;
    if (!current) return;
    if (current.id === null) current.endRequested = true;
    else void endTurn(current);
  }

  function press(): void {
    const action = toggle.press(performance.now());
    if (action === "start") void startTurn();
    else if (action === "stop") stopTurn();
  }

  function release(): void {
    if (toggle.release(performance.now()) === "stop") stopTurn();
  }

  function lostSession(message: string): void {
    if (turn) turn.abandoned = true;
    turn = null;
    awaiting.clear();
    toggle.reset();
    opening = false;
    partial = "";
    clearIdle();
    if (message && options.ownsMic()) error = message;
    render();
  }

  bindTalkButton(talk, press, release);

  mic.subscribe({
    level(rms) {
      if (turn?.id == null) return;
      meterFill.style.transform = `scaleX(${levelFraction(rms).toFixed(3)})`;
    },
    partial(turnId, text) {
      if (turn?.id !== turnId) return;
      partial = text;
      render();
    },
    final(turnId, text) {
      const wasOpen = turn?.id === turnId;
      if (!awaiting.has(turnId) && !wasOpen) return;
      awaiting.delete(turnId);
      if (wasOpen) {
        // The backend ended a very long turn by itself (120 s).
        turn = null;
        toggle.reset();
      }
      partial = "";
      if (text.trim()) {
        input.value = appendToDraft(input.value, text);
        options.onDraftChanged();
        input.focus();
        input.setSelectionRange(input.value.length, input.value.length);
      }
      scheduleIdle();
      render();
    },
    error(message) {
      lostSession(message);
    },
    closed(reason) {
      lostSession(micClosedMessage(reason));
    },
  });

  render();
  return {
    element,
    statusEl,
    errorEl,
    cancel() {
      const current = turn;
      if (current) {
        current.abandoned = true;
        if (current.id !== null) void mic.cancel(current.id).catch(() => undefined);
      }
      turn = null;
      awaiting.clear();
      toggle.reset();
      opening = false;
      partial = "";
      error = "";
      clearIdle();
      render();
    },
  };
}

// ---------------------------------------------------------------------------
// Voice conversation mode
// ---------------------------------------------------------------------------

type ExchangeView = {
  thinking: HTMLDetailsElement;
  thinkingText: HTMLElement;
  thinkingSummary: HTMLElement;
  root: HTMLElement;
  heardEl: HTMLElement;
  fillerEl: HTMLElement;
  preambleEl: HTMLElement;
  toolEl: HTMLElement;
  restEl: HTMLElement;
  markEl: HTMLElement;
  noteEl: HTMLElement;
  errorEl: HTMLElement;
};

type Exchange = VoiceExchangeRecord & {
  answer: string;
  rounds: Map<number, string>;
  reasoning: Map<number, string>;
  finalRound?: number;
  terminal: boolean;
  historyNote: string;
  tools: ToolTranscript;
  phrases: VoicePhrase[];
  playedSeq: number;
  error: string;
  note: string;
  view: ExchangeView;
  frame: number;
};

type ActiveAnswer = {
  exchange: Exchange;
  requestId: string;
  jobId: string | null;
  watch: PlaybackWatch;
  drained: boolean;
  phase: string;
};

type VoiceTurn = { id: number | null; endRequested: boolean; abandoned: boolean };

export type VoiceMode = {
  /** The voice view for `.assistant-main`. The page shows and hides it. */
  view: HTMLElement;
  /** The body of the panel's Voice section. */
  panelBody: HTMLElement;
  /** True while the page shows voice mode: opens the microphone; false cancels any answer and closes it. */
  setEntered(entered: boolean): void;
  /** Call when the server status changes (the microphone follows whether the model is ready). */
  refresh(): void;
  /** Clears the voice conversation (New chat). */
  newConversation(): void;
};

export async function createVoiceMode(options: {
  mic: Mic;
  log: Log;
  /** Why talking is not possible now (no model loaded, ...), or "" when it is. */
  unavailableReason: () => string;
  sampling: () => AssistantSampling | null;
  model: () => AssistantModelInfo | null;
  onNewChat: () => void;
}): Promise<VoiceMode> {
  const { mic, log } = options;
  const toggle = new HoldToggle();
  const joiner = new TurnJoiner();
  const exchanges: Exchange[] = [];
  let entered = false;
  let active: ActiveAnswer | null = null;
  /** A push-to-talk turn being recorded. */
  let recording: VoiceTurn | null = null;
  /** Auto mode: the turn Rust reported as started and not yet ended. */
  let autoTurn: number | null = null;
  let micStarting = false;
  /** Set after a failed open or a lost session, so status updates do not reopen it in a loop. */
  let micBlocked = false;
  let autoPref = readAutoPref();
  /** Set by `assistant:mic-auto-failed`: off for this session; the stored choice is kept. */
  let autoFailed = "";
  /**
   * The last turn-detection state asked of Rust, set before the command returns. While it is
   * false (paused for an answer, or switched off), Rust discards an open automatic turn: its
   * "ended" comes without a final, and a "started" racing the pause is ignored.
   */
  let autoListening = false;
  /** True while Rust's turn detection is on for the open session (the last enable succeeded). */
  let autoApplied = false;
  /** Auto mode: the user pressed the talk button to stop listening. Cleared on resume or re-entry. */
  let autoPaused = false;
  /** Bumped per `applyAuto` call, so an older call that resolves late does not overwrite a newer one. */
  let autoSeq = 0;
  let partial = "";
  let error = "";
  let notice = "";
  let noticeTimer = 0;
  let watchdog = 0;
  let conversation = 0;
  let conversationId = crypto.randomUUID();
  const pendingMetadata = new Map<string, Exchange>();

  // -- DOM ------------------------------------------------------------------

  const ring = h("span", "assistant-talk-ring");
  ring.setAttribute("aria-hidden", "true");
  const talkOn = micIcon(40);
  const talkOff = micOffIcon(40);
  const talk = h("button", "assistant-talk-btn", talkOn);
  talk.type = "button";
  talk.setAttribute("aria-label", "Talk");
  const stateLine = h("p", "assistant-talk-state");
  stateLine.id = "assistant-talk-state";
  stateLine.setAttribute("aria-live", "polite");
  talk.setAttribute("aria-describedby", stateLine.id);
  const partialLine = h("p", "assistant-talk-partial is-hidden");
  const errorLine = h("p", "assistant-error assistant-talk-error is-hidden");
  errorLine.setAttribute("role", "alert");
  const toolsToggle = createToolsToggle("voice", options.model);
  const stage = h(
    "div",
    "assistant-voice-stage",
    h("div", "assistant-talk", ring, talk),
    stateLine,
    partialLine,
    errorLine,
    toolsToggle.element,
  );

  const transcriptList = h("div", "assistant-vt-list");
  const transcriptEmpty = h("p", "assistant-empty", "Your conversation appears here. It stays on this computer and is cleared when you close VoiceReader.");
  transcriptList.appendChild(transcriptEmpty);
  const newChatBtn = button("New chat", "btn btn-ghost btn-sm", () => options.onNewChat());
  const transcript = h(
    "section",
    "assistant-voice-transcript",
    h("div", "assistant-vt-head", h("h2", "assistant-vt-title", "Transcript"), newChatBtn),
    transcriptList,
  );
  transcript.setAttribute("aria-label", "Voice conversation transcript");
  const view = h("div", "assistant-voice is-hidden", stage, transcript);
  // Side by side needs room for both columns; below it they stack. A hidden view reports no
  // width, which keeps whichever layout it had.
  if (typeof ResizeObserver !== "undefined") {
    new ResizeObserver((entries) => {
      const width = entries[0]?.contentRect.width ?? 0;
      if (width > 0) view.toggleAttribute("data-narrow", width < VOICE_SIDE_BY_SIDE_MIN_PX);
    }).observe(view);
  }

  const autoBox = h("input");
  autoBox.type = "checkbox";
  autoBox.checked = autoPref;
  const autoLabel = h("label", "assistant-think", autoBox, "Send automatically when I stop talking (experimental)");
  const autoNote = h(
    "p",
    "caption",
    "Uses voice activity detection plus Smart Turn to decide when you have finished. It may cut you off mid-sentence; the final transcript is shown so you can see when it did. When it is off, the talk button sends.",
  );
  const autoError = h("p", "caption assistant-auto-unavailable is-hidden");
  const thinkKey = "voicereader.assistant.think.voice";
  const thinkBox = h("input"); thinkBox.type = "checkbox"; thinkBox.checked = readStored(thinkKey) === "true";
  thinkBox.addEventListener("change", () => writeStored(thinkKey, String(thinkBox.checked)));
  const thinkingLimit = createThinkingLimit("voice");
  const panelBody = h("div", "assistant-section-body", autoLabel, autoNote, autoError,
    h("label", "assistant-think", thinkBox, "Think before answering"), thinkingLimit.element);

  // -- Derived state ----------------------------------------------------------

  function autoOn(): boolean {
    return autoPref && !autoFailed;
  }

  function isRecording(): boolean {
    return !!recording || autoTurn !== null;
  }

  function stateText(): { text: string; tone: "idle" | "busy" | "error" } {
    const unavailable = options.unavailableReason();
    if (unavailable) return { text: unavailable, tone: "idle" };
    if (micStarting) return { text: "Starting microphone…", tone: "busy" };
    if (isRecording()) return { text: "Listening…", tone: "busy" };
    if (active) {
      if (active.phase) return { text: `${active.phase} — press to interrupt`, tone: "busy" };
      if (active.exchange.phrases.length > 0) return { text: "Speaking — press to interrupt", tone: "busy" };
      return { text: "Thinking…", tone: "busy" };
    }
    if (joiner.busy) return { text: "Thinking…", tone: "busy" };
    if (notice) return { text: notice, tone: "idle" };
    if (error) return { text: error, tone: "error" };
    if (autoOn()) {
      if (autoPaused) return { text: "Paused. Press to listen again.", tone: "idle" };
      if (autoApplied) return { text: "Start talking. It sends when you finish. Press to pause.", tone: "idle" };
      if (mic.state !== "open") return { text: "Press to start listening", tone: "idle" };
    }
    return { text: "Hold to talk", tone: "idle" };
  }

  // -- Rendering --------------------------------------------------------------

  function render(): void {
    const state = stateText();
    stateLine.textContent = state.text;
    stateLine.className = `assistant-talk-state tone-${state.tone}`;
    const unavailable = !!options.unavailableReason();
    // In auto mode the button interrupts an answer, or pauses and resumes listening.
    const inert = unavailable;
    const paused = autoOn() && autoPaused && !active;
    talk.setAttribute("aria-disabled", String(inert));
    talk.classList.toggle("is-paused", paused);
    const shown = paused ? talkOff : talkOn;
    if (talk.firstChild !== shown) talk.replaceChildren(shown);
    talk.classList.toggle("is-inert", inert);
    talk.setAttribute("aria-pressed", String(isRecording()));
    talk.classList.toggle("is-recording", isRecording());
    talk.classList.toggle("is-speaking", !!active && active.exchange.phrases.length > 0);
    talk.classList.toggle("is-thinking", (!!active && active.exchange.phrases.length === 0) || (!active && !isRecording() && joiner.busy));
    talk.title = active
      ? "Press to interrupt the answer"
      : autoOn()
        ? paused
          ? "Press to listen again"
          : "Press to pause listening. To hold-to-talk instead, turn off automatic sending in the panel's Voice section."
        : "Hold to talk, or click to start and click again to send. Space works while the button has focus.";
    talk.setAttribute("aria-label", autoOn() && !active ? (paused ? "Resume listening" : "Pause listening") : "Talk");
    if (!isRecording()) view.style.setProperty("--level", "0");
    partialLine.textContent = partial;
    partialLine.classList.toggle("is-hidden", !partial || !isRecording());
    // Errors also show in the state line when nothing else is going on; keep the line below for the rest.
    const showError = !!error && state.text !== error;
    errorLine.textContent = error;
    errorLine.classList.toggle("is-hidden", !showError);
    // After a failure the toggle shows off; ticking it again retries.
    autoBox.checked = autoPref && !autoFailed;
    const autoText = autoFailed ? `Automatic sending stopped: ${autoFailed} The talk button works as usual.` : "";
    autoError.textContent = autoText;
    autoError.classList.toggle("is-hidden", !autoText);
  }

  function nearBottom(): boolean {
    return transcriptList.scrollHeight - transcriptList.scrollTop - transcriptList.clientHeight < NEAR_BOTTOM_PX;
  }

  function createExchange(user: string): Exchange {
    const thinkingText = h("div", "assistant-thinking-text");
    const thinkingSummary = h("summary", "", "Thinking…");
    const thinking = h("details", "assistant-thinking assistant-vt-thinking is-hidden", thinkingSummary, thinkingText);
    const heardEl = h("span", "assistant-vt-heard");
    const fillerEl = h("p", "assistant-vt-fillers is-hidden");
    const preambleEl = h("p", "assistant-vt-preambles is-hidden");
    const toolEl = h("div", "assistant-tool-transcript");
    const restEl = h("span", "assistant-vt-rest");
    const markEl = h("span", "assistant-vt-mark is-hidden");
    const noteEl = h("p", "caption is-hidden");
    const errorEl = h("p", "assistant-error is-hidden");
    const root = h(
      "div",
      "assistant-vt-exchange",
      h("p", "assistant-vt-user", h("span", "assistant-vt-who", "You"), user),
      thinking,
      fillerEl,
      preambleEl,
      toolEl,
      h("p", "assistant-vt-assistant", h("span", "assistant-vt-who", "Assistant"), heardEl, restEl, markEl),
      noteEl,
      errorEl,
    );
    return {
      user,
      status: "active",
      history: "",
      answer: "",
      rounds: new Map(),
      reasoning: new Map(),
      terminal: false,
      historyNote: "",
      tools: new ToolTranscript(),
      phrases: [],
      playedSeq: -1,
      error: "",
      note: "",
      view: { thinking, thinkingText, thinkingSummary, root, heardEl, fillerEl, preambleEl, toolEl, restEl, markEl, noteEl, errorEl },
      frame: 0,
    };
  }

  function renderExchange(exchange: Exchange): void {
    if (exchange.frame) {
      cancelAnimationFrame(exchange.frame);
      exchange.frame = 0;
    }
    const stick = nearBottom();
    const { view: v } = exchange;
    let split = exchange.answer.length;
    if (exchange.status !== "done") {
      split = heardPrefixLength(exchange.answer, heardText(exchange.phrases, exchange.playedSeq, answerRound(exchange)));
    }
    const round = answerRound(exchange);
    v.thinkingText.textContent = Array.from(exchange.reasoning.values()).join("\n\n");
    v.thinking.classList.toggle("is-hidden", !v.thinkingText.textContent);
    v.thinkingSummary.textContent = exchange.status === "active" && !exchange.answer ? "Thinking…" : "Thinking";
    const preamble = Array.from(exchange.rounds.entries()).filter(([n]) => n !== round).map(([, text]) => text).join("\n\n");
    v.preambleEl.textContent = preamble;
    v.preambleEl.classList.toggle("is-hidden", !preamble);
    v.fillerEl.textContent = exchange.phrases.filter((phrase) => phrase.kind === "filler").map((phrase) => phrase.text).join(" ");
    v.fillerEl.classList.toggle("is-hidden", !v.fillerEl.textContent);
    renderToolTranscript(exchange.tools, v.toolEl);
    v.heardEl.textContent = exchange.answer.slice(0, split);
    v.restEl.textContent = exchange.answer.slice(split);
    v.root.classList.toggle("is-active", exchange.status === "active");
    v.root.classList.toggle("is-cut", exchange.status === "interrupted" || exchange.status === "stopped");
    const mark = exchange.status === "interrupted" ? "Interrupted" : exchange.status === "stopped" ? "Stopped" : "";
    v.markEl.textContent = mark;
    v.markEl.classList.toggle("is-hidden", !mark);
    v.noteEl.textContent = exchange.note;
    v.noteEl.classList.toggle("is-hidden", !exchange.note);
    v.errorEl.textContent = exchange.error;
    v.errorEl.classList.toggle("is-hidden", !exchange.error);
    if (stick) transcriptList.scrollTop = transcriptList.scrollHeight;
  }

  function scheduleExchange(exchange: Exchange): void {
    if (exchange.frame) return;
    exchange.frame = requestAnimationFrame(() => {
      exchange.frame = 0;
      renderExchange(exchange);
    });
  }

  function showNotice(text: string): void {
    notice = text;
    if (noticeTimer) window.clearTimeout(noticeTimer);
    noticeTimer = window.setTimeout(() => {
      noticeTimer = 0;
      notice = "";
      render();
    }, NOTICE_MS);
    render();
  }

  // -- Microphone and auto mode ----------------------------------------------

  async function applyAuto(enabled: boolean): Promise<void> {
    // A paused microphone stays off until the user resumes it.
    if (enabled && autoPaused) return;
    const seq = ++autoSeq;
    if (mic.state !== "open") {
      autoApplied = false;
      autoListening = false;
      return;
    }
    autoListening = enabled;
    try {
      await mic.setAuto(enabled);
      if (seq !== autoSeq) return;
      autoApplied = enabled;
    } catch (failure) {
      if (seq !== autoSeq) return;
      autoApplied = false;
      autoListening = false;
      if (enabled) {
        // Retryable (a busy download slot, a network error): the toggle shows off and ticking it again retries.
        autoFailed = errorText(failure);
        log(`Assistant auto-send failed: ${autoFailed}`, "error");
      }
    }
    render();
  }

  /** Auto mode: stop listening. An automatic turn still open is discarded (its "ended" comes without a final). */
  function pauseAuto(): void {
    autoPaused = true;
    partial = "";
    void applyAuto(false);
    render();
  }

  function resumeAuto(): void {
    autoPaused = false;
    error = "";
    if (mic.state === "open") void applyAuto(true);
    else void restartAuto();
    render();
  }

  /** Auto mode after the microphone was lost: a press opens it again and turns detection back on. */
  async function restartAuto(): Promise<void> {
    error = "";
    if (await openMic()) {
      if (entered && autoOn() && !active) await applyAuto(true);
    }
  }

  /** Opens the microphone for voice mode; auto mode is switched on when idle. */
  async function openMic(): Promise<boolean> {
    if (mic.state === "open") return true;
    const started = conversation;
    micStarting = true;
    render();
    try {
      await mic.open();
      micBlocked = false;
      return true;
    } catch (failure) {
      const message = errorText(failure);
      if (message !== MIC_OPEN_ABANDONED && entered && started === conversation) {
        error = message;
        micBlocked = true;
        log(`Assistant microphone failed: ${message}`, "error");
      }
      return false;
    } finally {
      micStarting = false;
      render();
    }
  }

  async function syncMic(): Promise<void> {
    const want = entered && !options.unavailableReason();
    if (!want) {
      if (entered && (mic.state !== "closed" || recording || autoTurn !== null)) {
        // The model went away (ejected or failed): stop listening; an answer cannot go on either.
        if (active) interrupt();
        dropTurns();
        void mic.close();
        autoApplied = false;
        render();
      }
      return;
    }
    if (mic.state === "closed" && !micBlocked && !micStarting) {
      if ((await openMic()) && entered && autoOn() && !active && !recording) await applyAuto(true);
    }
  }

  function dropTurns(): void {
    if (recording) {
      recording.abandoned = true;
      if (recording.id !== null) void mic.cancel(recording.id).catch(() => undefined);
    }
    recording = null;
    autoTurn = null;
    autoListening = false;
    toggle.reset();
    joiner.reset();
    partial = "";
  }

  /** An automatic turn that will get no final (paused or failed detection): stop waiting for it. */
  function discardAutoTurn(turnId: number): void {
    if (autoTurn === turnId) autoTurn = null;
    partial = "";
    const released = joiner.cancel(turnId);
    if (released) void startAnswer(released);
  }

  // -- Turns ------------------------------------------------------------------

  async function startTurn(): Promise<void> {
    const turn: VoiceTurn = { id: null, endRequested: false, abandoned: false };
    recording = turn;
    joiner.beginPending();
    error = "";
    partial = "";
    render();
    const started = conversation;
    try {
      if (mic.state !== "open" && !(await openMic())) throw new Error("");
      if (turn.abandoned) return;
      const id = await mic.begin();
      if (turn.abandoned || started !== conversation) {
        void mic.cancel(id).catch(() => undefined);
        return;
      }
      turn.id = id;
      joiner.resolvePending(id);
      if (turn.endRequested) await endTurn(turn);
    } catch (failure) {
      if (turn.abandoned || started !== conversation) return;
      if (recording === turn) {
        recording = null;
        toggle.reset();
      }
      const message = errorText(failure);
      if (message && message !== MIC_OPEN_ABANDONED) error = message;
      const released = turn.id === null ? joiner.abandonPending() : joiner.cancel(turn.id);
      if (released) void startAnswer(released);
    } finally {
      render();
    }
  }

  async function endTurn(turn: VoiceTurn): Promise<void> {
    if (recording === turn) recording = null;
    partial = "";
    render();
    if (turn.id === null) return;
    try {
      await mic.end(turn.id);
    } catch (failure) {
      error = errorText(failure);
      const released = joiner.cancel(turn.id);
      if (released) void startAnswer(released);
    }
    render();
  }

  function stopTurn(): void {
    const turn = recording;
    if (!turn) return;
    if (turn.id === null) turn.endRequested = true;
    else void endTurn(turn);
  }

  function press(): void {
    if (options.unavailableReason()) return;
    // Rule 1: a press during an answer interrupts it. In auto mode that is all it does
    // (Rust's turn detection is off during the answer, so test the user's choice, not autoApplied).
    if (autoOn()) {
      if (active) interrupt();
      else if (autoPaused) resumeAuto();
      else if (autoApplied || autoListening) pauseAuto();
      else void restartAuto();
      return;
    }
    const action = toggle.press(performance.now());
    if (action === "start") {
      if (active) interrupt();
      void startTurn();
    } else if (action === "stop") {
      stopTurn();
    }
  }

  function release(): void {
    if (toggle.release(performance.now()) === "stop") stopTurn();
  }

  function handleOutcome(outcome: FinalOutcome): void {
    if (outcome.kind === "submit") void startAnswer(outcome.text);
    else if (outcome.kind === "empty") showNotice("Didn't catch that");
    render();
  }

  // -- Answers ----------------------------------------------------------------

  async function startAnswer(userText: string): Promise<void> {
    if (!entered) return;
    if (active) interrupt();
    const exchange = createExchange(userText);
    const history = buildVoiceHistory(exchanges);
    const messages = [...history, { role: "user" as const, text: userText, images: [] }];
    transcriptEmpty.remove();
    exchanges.push(exchange);
    transcriptList.appendChild(exchange.view.root);
    renderExchange(exchange);
    transcriptList.scrollTop = transcriptList.scrollHeight;
    const requestId = crypto.randomUUID();
    // Registered before the command: its events can arrive before it resolves.
    active = { exchange, requestId, jobId: null, watch: newPlaybackWatch(performance.now()), drained: false, phase: "" };
    error = "";
    startWatchdog();
    // The microphone stays closed while the assistant speaks.
    if (autoApplied || autoListening) void applyAuto(false);
    render();
    try {
      const result = await invoke<AssistantVoiceRespondResult>("assistant_voice_respond", {
        requestId,
        messages,
        sampling: options.sampling(),
        conversationId,
        tools: toolsToggle.enabled(),
        enabledTools: toolsToggle.selection(),
        think: thinkBox.checked,
        thinkingBudget: thinkingLimit.value(),
        toolLimits: toolLimits(),
      });
      if (active?.requestId !== requestId) return;
      if (result?.job_id != null && active.jobId === null) active.jobId = String(result.job_id);
      if (result && result.dropped_messages > 0) {
        const count = result.dropped_messages;
        exchange.note = `${count} older message${count === 1 ? "" : "s"} no longer fit and ${count === 1 ? "was" : "were"} left out`;
        scheduleExchange(exchange);
      }
    } catch (failure) {
      if (active?.requestId !== requestId) return;
      fail(errorText(failure), false);
    }
  }

  function settle(status: Exchange["status"], history: string, message = ""): void {
    const current = active;
    if (!current) return;
    active = null;
    stopWatchdog();
    const exchange = current.exchange;
    exchange.status = status;
    // Late tool results of an interrupted or failed answer are ignored, so no chip stays "running".
    if (status !== "done") exchange.tools.settlePending();
    exchange.history = historyWithNote(history, exchange.historyNote);
    if (!exchange.terminal && (status === "interrupted" || status === "stopped")) pendingMetadata.set(current.requestId, exchange);
    exchange.error = message;
    renderExchange(exchange);
    if (entered && autoOn() && mic.state === "open" && !recording) void applyAuto(true);
    render();
  }

  function answerRound(exchange: Exchange): number {
    return exchange.finalRound ?? Math.max(0, ...exchange.rounds.keys());
  }

  function heardOf(exchange: Exchange): string {
    return heardText(exchange.phrases, exchange.playedSeq, answerRound(exchange));
  }

  function cancelRequest(requestId: string): void {
    void invoke("assistant_voice_cancel", { requestId }).catch((failure) =>
      log(`Assistant voice cancel failed: ${errorText(failure)}`, "error"),
    );
  }

  /** Rule 1: the answer keeps only what was heard. */
  function interrupt(): void {
    const current = active;
    if (!current) return;
    cancelRequest(current.requestId);
    settle("interrupted", cutShortText(heardOf(current.exchange), INTERRUPTED_MARK));
  }

  function complete(): void {
    const current = active;
    if (!current || !current.exchange.terminal || !current.drained) return;
    settle("done", current.exchange.answer.trim());
  }

  function fail(message: string, cancel: boolean): void {
    const current = active;
    if (!current) return;
    if (cancel) cancelRequest(current.requestId);
    error = message;
    log(`Assistant voice answer failed: ${message}`, "error");
    settle("error", "", message);
  }

  function startWatchdog(): void {
    stopWatchdog();
    watchdog = window.setInterval(() => {
      if (!active) return stopWatchdog();
      if (playbackVerdict(active.watch, performance.now()) !== "ok") fail("Playback stopped unexpectedly", true);
    }, WATCHDOG_TICK_MS);
  }

  function stopWatchdog(): void {
    if (watchdog) window.clearInterval(watchdog);
    watchdog = 0;
  }

  /** The current answer's job, or null when the event belongs to another one. */
  function forJob(jobId: string): ActiveAnswer | null {
    if (!active || active.jobId === null || String(jobId) !== active.jobId) return null;
    return active;
  }

  // -- Events -----------------------------------------------------------------

  await listen<AssistantChatDeltaPayload>("assistant:chat-delta", ({ payload }) => {
    if (!active || payload.request_id !== active.requestId) return;
    const round = payload.round ?? 0;
    if (payload.reasoning) active.exchange.reasoning.set(round, (active.exchange.reasoning.get(round) ?? "") + payload.reasoning);
    if (payload.content) {
      active.exchange.rounds.set(round, (active.exchange.rounds.get(round) ?? "") + payload.content);
      active.exchange.answer = active.exchange.rounds.get(round) ?? "";
    }
    if (payload.content || payload.reasoning) scheduleExchange(active.exchange);
  });
  await listen<AssistantChatDonePayload>("assistant:chat-done", ({ payload }) => {
    const current = active?.requestId === payload.request_id ? active : null;
    const exchange = current?.exchange ?? pendingMetadata.get(payload.request_id);
    if (!exchange) return;
    pendingMetadata.delete(payload.request_id);
    exchange.terminal = true;
    exchange.finalRound = payload.final_round ?? answerRound(exchange);
    exchange.answer = finalAnswer(exchange.rounds, payload);
    exchange.historyNote = payload.history_note ?? "";
    exchange.tools.addSources(payload.sources);
    if (!current) {
      const mark = exchange.status === "interrupted" ? INTERRUPTED_MARK : STOPPED_MARK;
      exchange.history = historyWithNote(cutShortText(heardOf(exchange), mark), exchange.historyNote);
    }
    scheduleExchange(exchange);
    complete();
  });
  await listen<AssistantToolCallPayload>("assistant:tool-call", ({ payload }) => {
    if (!active || payload.request_id !== active.requestId) return;
    active.exchange.tools.call(payload);
    active.phase = payload.name === "web_search" ? "Searching…" : payload.name === "fetch_page" ? "Opening page…" : payload.name === "get_weather" ? "Checking weather…" : payload.name === "run_python" ? "Running code…" : "Using tools…";
    scheduleExchange(active.exchange); render();
  });
  await listen<AssistantToolResultPayload>("assistant:tool-result", ({ payload }) => {
    if (!active || payload.request_id !== active.requestId) return;
    active.exchange.tools.result(payload); scheduleExchange(active.exchange);
  });
  await listen<AssistantTurnPhasePayload>("assistant:turn-phase", ({ payload }) => {
    if (!active || payload.request_id !== active.requestId) return;
    active.phase = payload.phase === "thinking" ? "Thinking…" : payload.phase === "tool" ? "Using tools…" : "Answering…";
    render();
  });
  await listen<AssistantChatErrorPayload>("assistant:chat-error", ({ payload }) => {
    if (!active || payload.request_id !== active.requestId) return;
    fail(payload.message || "The model returned an error.", true);
  });
  await listen<AssistantVoicePhrasePayload>("assistant:voice-phrase", ({ payload }) => {
    if (!active || payload.request_id !== active.requestId) return;
    const jobId = String(payload.job_id);
    if (active.jobId === null) active.jobId = jobId;
    else if (active.jobId !== jobId) return;
    active.exchange.phrases.push({ seq: Number(payload.seq), text: String(payload.text ?? ""), kind: payload.kind ?? "answer", round: payload.round ?? 0 });
    scheduleExchange(active.exchange);
    render();
  });
  await listen<AssistantPlaybackPayload>("assistant:playback", ({ payload }) => {
    const current = forJob(payload.job_id);
    if (!current) return;
    const playedSeq = typeof payload.played_seq === "number" ? payload.played_seq : null;
    current.watch = notePlayback(current.watch, Number(payload.buffered_secs), playedSeq, performance.now());
    if (playedSeq !== null && playedSeq > current.exchange.playedSeq) {
      current.exchange.playedSeq = playedSeq;
      scheduleExchange(current.exchange);
    }
    if (payload.drained) { current.drained = true; complete(); }
  });
  await listen<AssistantVoiceEndedPayload>("assistant:voice-ended", ({ payload }) => {
    if (!active || payload.request_id !== active.requestId) return;
    if (active.jobId !== null && payload.job_id != null && String(payload.job_id) !== active.jobId) return;
    const current = active;
    if (payload.reason === "done") {
      current.watch = noteSpeechDone(current.watch, performance.now());
      // Nothing was spoken, so no drained report will come.
      if (current.exchange.phrases.length === 0) { current.drained = true; complete(); }
    } else if (payload.reason === "error") {
      fail(payload.message || "The voice answer failed.", false);
    } else {
      // "preempted" (read-aloud took over) or a cancel this page did not ask for.
      settle("stopped", cutShortText(heardOf(current.exchange), STOPPED_MARK));
    }
  });

  mic.subscribe({
    level(rms) {
      if (!entered || !isRecording()) return;
      view.style.setProperty("--level", levelFraction(rms).toFixed(3));
    },
    partial(turnId, text) {
      if (!entered) return;
      if (recording?.id !== turnId && autoTurn !== turnId) return;
      partial = text;
      render();
    },
    final(turnId, text) {
      if (!entered) return;
      if (recording?.id === turnId) {
        // The backend ended a very long turn by itself (120 s).
        recording = null;
        toggle.reset();
      }
      if (autoTurn === turnId) autoTurn = null;
      partial = "";
      handleOutcome(joiner.final(turnId, text));
    },
    turn(turnId, state) {
      if (!entered) return;
      if (state === "started") {
        // A start racing our pause belongs to a turn Rust is about to discard.
        if (!autoOn() || !autoListening) return;
        // A new turn cancels an answer still running (plan §7.2).
        if (active) interrupt();
        autoTurn = turnId;
        partial = "";
        error = "";
        joiner.begin(turnId);
      } else if (!autoListening) {
        // Ended by our pause (or by switching auto off): discarded, no final follows.
        discardAutoTurn(turnId);
      } else if (autoTurn === turnId) {
        autoTurn = null;
      }
      render();
    },
    autoFailed(message) {
      if (!entered) return;
      autoFailed = message || "Turn detection failed.";
      autoApplied = false;
      autoListening = false;
      if (autoTurn !== null) discardAutoTurn(autoTurn);
      error = autoFailed;
      render();
    },
    error(message) {
      if (!entered) return;
      dropTurns();
      autoApplied = false;
      micBlocked = true;
      error = message;
      render();
    },
    closed(reason) {
      if (!entered) return;
      dropTurns();
      autoApplied = false;
      micBlocked = true;
      error = micClosedMessage(reason) || error;
      render();
    },
  });

  bindTalkButton(talk, press, release);

  autoBox.addEventListener("change", () => {
    autoPref = autoBox.checked;
    autoPaused = false;
    if (autoPref) autoFailed = "";
    writeStored(AUTOSEND_KEY, String(autoPref));
    if (!entered) return render();
    if (autoPref) {
      if (recording) stopTurn();
      if (!active) void applyAuto(true);
    } else if (autoApplied || autoListening) {
      // An open automatic turn is discarded by the pause; its "ended" clears it.
      void applyAuto(false);
    }
    render();
  });

  render();

  return {
    view,
    panelBody,
    setEntered(next) {
      if (next === entered) return;
      entered = next;
      if (entered) {
        error = "";
        autoFailed = "";
        autoPaused = false;
        micBlocked = false;
        void syncMic();
      } else {
        // Leaving cancels any answer (kept as heard so far) and closes the microphone.
        if (active) interrupt();
        dropTurns();
        autoApplied = false;
        micStarting = false;
        void mic.close();
      }
      render();
    },
    refresh() {
      render();
      void syncMic();
    },
    newConversation() {
      conversation += 1;
      const clearedConversationId = conversationId;
      conversationId = crypto.randomUUID();
      pendingMetadata.clear();
      void invoke("assistant_clear_tool_conversation", { conversationId: clearedConversationId }).catch((failure) => log(`Could not clear voice tool conversation: ${errorText(failure)}`, "error"));
      if (active) {
        const current = active;
        active = null;
        stopWatchdog();
        cancelRequest(current.requestId);
      }
      if (recording) {
        recording.abandoned = true;
        if (recording.id !== null) void mic.cancel(recording.id).catch(() => undefined);
      }
      recording = null;
      autoTurn = null;
      toggle.reset();
      joiner.reset();
      partial = "";
      error = "";
      for (const exchange of exchanges) if (exchange.frame) cancelAnimationFrame(exchange.frame);
      exchanges.length = 0;
      transcriptList.replaceChildren(transcriptEmpty);
      if (entered && autoOn() && mic.state === "open") void applyAuto(true);
      render();
    },
  };
}

function readAutoPref(): boolean {
  const stored = readStored(AUTOSEND_KEY);
  if (stored === "true") return true;
  if (stored === "false") return false;
  return import.meta.env.DEV;
}
