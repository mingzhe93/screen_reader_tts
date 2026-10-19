// The Assistant's one microphone session (plan §7.1), shared by the chat page's push-to-talk
// and voice mode. While it is open Rust holds the speech recogniser, so Transcribe cannot
// run; closing it frees the recogniser. Events are passed to every subscriber, and each one
// ignores turns it did not start. Commands and events: docs/assistant/conversation.md.

import { invoke } from "@tauri-apps/api/tauri";
import { listen } from "@tauri-apps/api/event";
import { errorText } from "./assistant-dom";
import type {
  AssistantMicClosedPayload,
  AssistantMicClosedReason,
  AssistantMicErrorPayload,
  AssistantMicAutoFailedPayload,
  AssistantMicLevelPayload,
  AssistantMicOpenResult,
  AssistantMicTextPayload,
  AssistantMicTurnPayload,
} from "./types";

/** Thrown by `open()` when `close()` was called before the microphone finished opening. */
export const MIC_OPEN_ABANDONED = "The microphone was closed while it was starting.";

export type MicEvents = {
  level?: (rms: number) => void;
  partial?: (turnId: number, text: string) => void;
  final?: (turnId: number, text: string) => void;
  /** The session is closed after an error. */
  error?: (message: string) => void;
  /** The session ended for another reason than our own `close()`. */
  closed?: (reason: AssistantMicClosedReason) => void;
  turn?: (turnId: number, state: "started" | "ended") => void;
  /** Automatic sending failed and Rust switched it off; the microphone stays open. */
  autoFailed?: (message: string) => void;
};

export type Mic = {
  readonly state: "closed" | "opening" | "open";
  /** Opens the microphone (and loads the recogniser) if it is not open. Rejects with a readable message. */
  open(): Promise<void>;
  begin(): Promise<number>;
  end(turnId: number): Promise<void>;
  cancel(turnId: number): Promise<void>;
  /** Auto turn detection (Step 4). Rejects when it is not installed. */
  setAuto(enabled: boolean): Promise<void>;
  close(): Promise<void>;
  subscribe(events: MicEvents): () => void;
};

export async function createMic(log: (message: string, level?: "info" | "error") => void): Promise<Mic> {
  let state: Mic["state"] = "closed";
  /** Bumped by every close, so an open that finishes after it is undone. */
  let generation = 0;
  let opening: Promise<void> | null = null;
  const subscribers = new Set<MicEvents>();

  function emit<K extends keyof MicEvents>(name: K, ...args: Parameters<NonNullable<MicEvents[K]>>): void {
    for (const subscriber of subscribers) {
      const handler = subscriber[name] as ((...values: unknown[]) => void) | undefined;
      try {
        handler?.(...args);
      } catch (error) {
        log(`Assistant microphone handler failed: ${errorText(error)}`, "error");
      }
    }
  }

  /** The session ended on the Rust side (error, Transcribe, exit). */
  function lost(): void {
    generation += 1;
    state = "closed";
  }

  const mic: Mic = {
    get state() {
      return state;
    },

    open() {
      if (state === "open") return Promise.resolve();
      if (opening) return opening;
      const started = generation;
      state = "opening";
      opening = (async () => {
        try {
          const result = await invoke<AssistantMicOpenResult | null>("assistant_mic_open", {});
          if (result && result.ok === false) throw new Error(result.message || "The microphone could not be opened.");
          if (started !== generation) {
            void invoke("assistant_mic_close").catch(() => undefined);
            throw new Error(MIC_OPEN_ABANDONED);
          }
          state = "open";
        } catch (error) {
          if (started === generation) state = "closed";
          throw new Error(errorText(error));
        } finally {
          opening = null;
        }
      })();
      return opening;
    },

    async begin() {
      const id = await invoke<number>("assistant_mic_begin");
      return Number(id);
    },

    async end(turnId) {
      await invoke("assistant_mic_end", { turnId });
    },

    async cancel(turnId) {
      await invoke("assistant_mic_cancel", { turnId });
    },

    async setAuto(enabled) {
      await invoke("assistant_mic_set_auto", { enabled });
    },

    async close() {
      const wasClosed = state === "closed" && !opening;
      generation += 1;
      state = "closed";
      if (wasClosed) return;
      try {
        await invoke("assistant_mic_close");
      } catch (error) {
        log(`Assistant microphone close failed: ${errorText(error)}`, "error");
      }
    },

    subscribe(events) {
      subscribers.add(events);
      return () => subscribers.delete(events);
    },
  };

  await listen<AssistantMicLevelPayload>("assistant:mic-level", ({ payload }) => emit("level", Number(payload.rms)));
  await listen<AssistantMicTextPayload>("assistant:mic-partial", ({ payload }) =>
    emit("partial", Number(payload.turn_id), String(payload.text ?? "")),
  );
  await listen<AssistantMicTextPayload>("assistant:mic-final", ({ payload }) =>
    emit("final", Number(payload.turn_id), String(payload.text ?? "")),
  );
  // Only an open session can be lost: while one is opening, the open command reports the
  // failure itself, and after our own close() a late event must not touch a newer session.
  await listen<AssistantMicErrorPayload>("assistant:mic-error", ({ payload }) => {
    const message = payload.message || "Voice input stopped unexpectedly.";
    log(`Assistant microphone error: ${message}`, "error");
    if (state !== "open") return;
    lost();
    emit("error", message);
  });
  await listen<AssistantMicClosedPayload>("assistant:mic-closed", ({ payload }) => {
    if (state !== "open") return;
    lost();
    if (payload.reason === "error") emit("error", "Voice input stopped unexpectedly.");
    else emit("closed", payload.reason);
  });
  await listen<AssistantMicTurnPayload>("assistant:mic-turn", ({ payload }) =>
    emit("turn", Number(payload.turn_id), payload.state),
  );
  await listen<AssistantMicAutoFailedPayload>("assistant:mic-auto-failed", ({ payload }) => {
    log(`Assistant auto-send failed: ${payload.message}`, "error");
    emit("autoFailed", String(payload.message ?? ""));
  });
  return mic;
}

/** Text for a session the backend ended, or "" when there is nothing to tell the user. */
export function micClosedMessage(reason: AssistantMicClosedReason): string {
  if (reason === "transcription") return "Voice input stopped because Transcribe started. Press the microphone to start it again.";
  return "";
}
