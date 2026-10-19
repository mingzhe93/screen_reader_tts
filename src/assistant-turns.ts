// Turn-taking rules for Assistant voice input and voice conversation (plan §3.3, §6.1, §6.3,
// §7.2). Pure logic with no DOM or Tauri access, so scripts/test-assistant-voice.mjs can test it:
// the press/hold/toggle button, the F7 join of a transcript that finishes while a newer turn
// records, the history sent back to the model (heard text for spoken answers), and the
// playback stall watchdog (Codex F9).

/** A press held at least this long ends the turn on release; a shorter one toggles recording on. */
export const HOLD_MS = 300;
/** Playback has stalled when this long passes with audio queued and no progress. */
export const STALL_MS = 5000;
/** Below this many seconds queued, a silent player is waiting for text, not stalled. */
export const QUEUED_SECS = 0.2;

export const INTERRUPTED_MARK = "[interrupted by user]";
export const STOPPED_MARK = "[stopped]";

// ---------------------------------------------------------------------------
// Hold or toggle
// ---------------------------------------------------------------------------

export type PressAction = "start" | "stop" | "none";

/**
 * One handler for hold-to-talk and click-to-toggle. Holding (press, wait, release) records one
 * turn; a short click starts recording and the next press stops it. Key repeat is ignored.
 */
export class HoldToggle {
  private recording = false;
  private down = false;
  private pressedAt = 0;
  /** The press that stopped a recording; its release does nothing. */
  private stoppedByPress = false;

  get active(): boolean {
    return this.recording;
  }

  press(now: number): PressAction {
    if (this.down) return "none";
    this.down = true;
    if (this.recording) {
      this.recording = false;
      this.stoppedByPress = true;
      return "stop";
    }
    this.recording = true;
    this.stoppedByPress = false;
    this.pressedAt = now;
    return "start";
  }

  release(now: number): PressAction {
    if (!this.down) return "none";
    this.down = false;
    if (this.stoppedByPress) {
      this.stoppedByPress = false;
      return "none";
    }
    if (this.recording && now - this.pressedAt >= HOLD_MS) {
      this.recording = false;
      return "stop";
    }
    return "none";
  }

  /** Recording ended for another reason (an error, a turn cut off by the backend). */
  reset(): void {
    this.recording = false;
    this.stoppedByPress = this.down;
  }
}

// ---------------------------------------------------------------------------
// F7: joining a transcript that finishes while a newer turn records
// ---------------------------------------------------------------------------

export type FinalOutcome =
  | { kind: "hold" }
  | { kind: "submit"; text: string }
  | { kind: "empty" }
  | { kind: "ignore" };

/** Joins transcript pieces with single spaces, dropping empty ones. */
export function joinTurnText(parts: string[]): string {
  return parts
    .map((part) => part.trim())
    .filter(Boolean)
    .join(" ");
}

/**
 * Codex F7 (plan §7.2): if a newer turn has begun when an older turn's final text arrives,
 * the older text is held and joined to the start of the newer turn's text when that one ends,
 * so the model never answers half a thought.
 *
 * A turn is "pending" from the press until its id is known (the begin command is still
 * running), then "open" until its final arrives. Turn ids increase.
 */
export class TurnJoiner {
  private held: string[] = [];
  private pending = 0;
  private open = new Set<number>();

  /** A turn was started and its id is not known yet. */
  beginPending(): void {
    this.pending += 1;
  }

  /** The pending turn now has an id. */
  resolvePending(turnId: number): void {
    if (this.pending > 0) this.pending -= 1;
    this.open.add(turnId);
  }

  /**
   * The pending turn could not start. If nothing else is outstanding, the held text is
   * released as a message of its own (or null when there is none).
   */
  abandonPending(): string | null {
    if (this.pending > 0) this.pending -= 1;
    return this.releaseHeldIfIdle();
  }

  /** A turn began with a known id (auto mode reports its own turns). */
  begin(turnId: number): void {
    this.open.add(turnId);
  }

  /** A turn was discarded without a final. Behaves like an empty final. */
  cancel(turnId: number): string | null {
    if (!this.open.delete(turnId)) return null;
    return this.releaseHeldIfIdle();
  }

  final(turnId: number, text: string): FinalOutcome {
    if (!this.open.delete(turnId)) return { kind: "ignore" };
    if (this.newerOutstanding(turnId)) {
      if (text.trim()) this.held.push(text);
      return { kind: "hold" };
    }
    const joined = joinTurnText([...this.held, text]);
    this.held = [];
    return joined ? { kind: "submit", text: joined } : { kind: "empty" };
  }

  /** True while any turn is pending or open, or held text is waiting. */
  get busy(): boolean {
    return this.pending > 0 || this.open.size > 0 || this.held.length > 0;
  }

  get heldText(): string {
    return joinTurnText(this.held);
  }

  reset(): void {
    this.held = [];
    this.pending = 0;
    this.open.clear();
  }

  private newerOutstanding(turnId: number): boolean {
    if (this.pending > 0) return true;
    for (const id of this.open) if (id > turnId) return true;
    return false;
  }

  private releaseHeldIfIdle(): string | null {
    if (this.pending > 0 || this.open.size > 0 || this.held.length === 0) return null;
    const text = joinTurnText(this.held);
    this.held = [];
    return text || null;
  }
}

// ---------------------------------------------------------------------------
// Heard text and history (plan §6.3)
// ---------------------------------------------------------------------------

export type VoicePhrase = { seq: number; text: string };

/** The phrases the player has finished (seq ≤ playedSeq), in order, joined with spaces. */
export function heardText(phrases: VoicePhrase[], playedSeq: number): string {
  return joinTurnText(
    phrases
      .filter((phrase) => phrase.seq <= playedSeq)
      .sort((a, b) => a.seq - b.seq)
      .map((phrase) => phrase.text),
  );
}

/** History text of an answer cut short: what was heard, then the marker. */
export function cutShortText(heard: string, mark: string): string {
  return heard.trim() ? `${heard.trim()}\n\n${mark}` : mark;
}

export type VoiceExchangeStatus = "active" | "done" | "interrupted" | "stopped" | "error";

export type VoiceExchangeRecord = {
  user: string;
  status: VoiceExchangeStatus;
  /** What the model is told it said: the full answer, or the heard part plus a marker. */
  history: string;
};

export type HistoryMessage = { role: "user" | "assistant"; text: string; images: string[] };

/**
 * The voice conversation sent back to the model, alternating user and assistant. Exchanges
 * that failed, are still running, or finished without text are left out entirely.
 */
export function buildVoiceHistory(exchanges: VoiceExchangeRecord[]): HistoryMessage[] {
  const messages: HistoryMessage[] = [];
  for (const exchange of exchanges) {
    if (!exchange.user.trim()) continue;
    const keep =
      exchange.status === "interrupted" ||
      exchange.status === "stopped" ||
      (exchange.status === "done" && exchange.history.trim() !== "");
    if (!keep) continue;
    messages.push({ role: "user", text: exchange.user, images: [] });
    messages.push({ role: "assistant", text: exchange.history, images: [] });
  }
  return messages;
}

const WORD_CHAR = /[\p{L}\p{N}]/u;

/**
 * Where the heard part of the streamed answer ends, as an index into `fullText`. Phrases are
 * the speakable form of the text (Markdown markers stripped), so the two are matched by their
 * letters and digits; trailing punctuation after the last heard word counts as heard.
 * The match is approximate when the spoken form rewrites words (for example numbers).
 */
export function heardPrefixLength(fullText: string, heard: string): number {
  let target = 0;
  for (const char of heard) if (WORD_CHAR.test(char)) target += 1;
  if (target === 0) return 0;
  let count = 0;
  let index = 0;
  const chars = Array.from(fullText);
  let offset = 0;
  for (; index < chars.length && count < target; index += 1) {
    if (WORD_CHAR.test(chars[index])) count += 1;
    offset += chars[index].length;
  }
  while (index < chars.length && !WORD_CHAR.test(chars[index]) && !/\s/u.test(chars[index])) {
    offset += chars[index].length;
    index += 1;
  }
  return offset;
}

/** Appends recognised speech to a draft, with a space between them if needed. */
export function appendToDraft(draft: string, addition: string): string {
  const text = addition.trim();
  if (!text) return draft;
  if (!draft || /\s$/u.test(draft)) return draft + text;
  return `${draft} ${text}`;
}

// ---------------------------------------------------------------------------
// Playback stall watchdog (plan §6.1, Codex F9)
// ---------------------------------------------------------------------------

export type PlaybackWatch = {
  /** Seconds queued at the last report. */
  buffered: number;
  /** When the last progress arrived (a changed `buffered`, a new played phrase), or the start. */
  progressAt: number;
  playedSeq: number;
  /** When `voice-ended` "done" arrived: every phrase was handed to the player. */
  doneAt: number | null;
};

export type PlaybackVerdict = "ok" | "stalled" | "undrained";

export function newPlaybackWatch(now: number): PlaybackWatch {
  return { buffered: 0, progressAt: now, playedSeq: -1, doneAt: null };
}

/** Records an `assistant:playback` report for the current job. */
export function notePlayback(watch: PlaybackWatch, bufferedSecs: number, playedSeq: number | null, now: number): PlaybackWatch {
  const buffered = Number.isFinite(bufferedSecs) ? Math.max(0, bufferedSecs) : watch.buffered;
  const newPhrase = playedSeq !== null && playedSeq > watch.playedSeq;
  const progressed = newPhrase || Math.abs(buffered - watch.buffered) > 1e-3;
  return {
    buffered,
    progressAt: progressed ? now : watch.progressAt,
    playedSeq: newPhrase ? playedSeq : watch.playedSeq,
    doneAt: watch.doneAt,
  };
}

export function noteSpeechDone(watch: PlaybackWatch, now: number): PlaybackWatch {
  return { ...watch, doneAt: watch.doneAt ?? now };
}

/**
 * "stalled": audio is queued but nothing has moved for STALL_MS. A silent player with nothing
 * queued is a pause in the model's output, not a stall. "undrained": every phrase was handed
 * over but `drained` has not come within the audio still queued plus STALL_MS of the last news.
 * The caller only asks while the answer is active and not cancelled.
 */
export function playbackVerdict(watch: PlaybackWatch, now: number): PlaybackVerdict {
  if (watch.buffered > QUEUED_SECS && now - watch.progressAt >= STALL_MS) return "stalled";
  if (watch.doneAt !== null) {
    const since = Math.max(watch.doneAt, watch.progressAt);
    if (now - since >= watch.buffered * 1000 + STALL_MS) return "undrained";
  }
  return "ok";
}

// ---------------------------------------------------------------------------
// Level meter
// ---------------------------------------------------------------------------

/** Microphone RMS (speech is about 0.02–0.2) to 0..1 on a decibel scale, −50 dB to −10 dB. */
export function levelFraction(rms: number): number {
  if (!(rms > 0) || !Number.isFinite(rms)) return 0;
  const db = 20 * Math.log10(rms);
  return Math.min(1, Math.max(0, (db + 50) / 40));
}
