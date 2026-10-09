// The audio-thread half of the speech player. It holds the speech that has been
// synthesized but not yet heard, and time-stretches it on the way to the speakers, so a
// change of speed applies to the very next fraction of a second.
//
// The main-thread half is in player.ts; the two talk through the node's message port.

import { TempoStretcher } from "./tempo-stretch";
import type { PlayerCommand, PlayerReport } from "./player-messages";

// Globals of the audio worklet scope, which the DOM typings do not cover.
declare const sampleRate: number;
declare function registerProcessor(name: string, processor: unknown): void;
declare class AudioWorkletProcessor {
  readonly port: MessagePort;
}

/** Source samples handed to the stretcher at a time. */
const FEED_SAMPLES = 1024;
/** Render quanta between status reports (128 frames each: about 50 ms at 48 kHz). */
const STATUS_EVERY_QUANTA = 16;

class TempoPlayerProcessor extends AudioWorkletProcessor {
  private readonly stretcher = new TempoStretcher(sampleRate);
  /** Speech waiting to be played, oldest first, with the read position in the first piece. */
  private pieces: Float32Array[] = [];
  private pieceOffset = 0;
  private queued = 0;
  /** Source samples taken from the queue since the last reset. */
  private consumed = 0;
  private epoch = 0;
  private playing = false;
  /** No more speech will arrive for this job. */
  private ended = false;
  private flushed = false;
  private quanta = 0;

  constructor() {
    super();
    this.port.onmessage = (event: MessageEvent<PlayerCommand>) => this.handle(event.data);
  }

  private handle(command: PlayerCommand): void {
    if (command.type === "reset") {
      this.epoch = command.epoch;
      this.pieces = [];
      this.pieceOffset = 0;
      this.queued = 0;
      this.consumed = 0;
      this.playing = false;
      this.ended = false;
      this.flushed = false;
      this.stretcher.reset();
      return;
    }
    if (command.type === "tempo") {
      this.stretcher.setTempo(command.tempo);
      return;
    }
    if (command.epoch !== this.epoch) {
      // Sent for a job that has since been stopped or replaced.
      return;
    }
    switch (command.type) {
      case "speech":
        this.pieces.push(command.samples);
        this.queued += command.samples.length;
        break;
      case "play":
        this.playing = true;
        break;
      case "end":
        this.ended = true;
        break;
      case "skip":
        // Jump past everything that is waiting.
        this.consumed += this.queued;
        this.pieces = [];
        this.pieceOffset = 0;
        this.queued = 0;
        this.stretcher.reset();
        break;
    }
  }

  private report(message: PlayerReport): void {
    this.port.postMessage(message);
  }

  /** Moves the next source samples into the stretcher. */
  private feed(): void {
    let wanted = FEED_SAMPLES;
    while (wanted > 0 && this.pieces.length > 0) {
      const piece = this.pieces[0];
      const take = Math.min(wanted, piece.length - this.pieceOffset);
      this.stretcher.write(piece.subarray(this.pieceOffset, this.pieceOffset + take));
      this.pieceOffset += take;
      this.queued -= take;
      this.consumed += take;
      wanted -= take;
      if (this.pieceOffset >= piece.length) {
        this.pieces.shift();
        this.pieceOffset = 0;
      }
    }
  }

  process(_inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
    const output = outputs[0]?.[0];
    if (!output) {
      return true;
    }
    if (this.playing) {
      while (this.stretcher.available < output.length) {
        if (this.queued > 0) {
          this.feed();
        } else if (this.ended && !this.flushed) {
          this.stretcher.finish();
          this.flushed = true;
        } else {
          break;
        }
      }
      const filled = this.stretcher.read(output, 0, output.length);
      if (filled < output.length) {
        // Out of audio. Stop pulling until the main thread says to go on, so the
        // player refills instead of emitting one late scrap at a time.
        this.playing = false;
        this.report({ type: this.ended ? "finished" : "starved", epoch: this.epoch });
      }
      for (let channel = 1; channel < outputs[0].length; channel += 1) {
        outputs[0][channel].set(output);
      }
    }

    this.quanta += 1;
    if (this.quanta >= STATUS_EVERY_QUANTA) {
      this.quanta = 0;
      this.report({ type: "status", epoch: this.epoch, consumed: this.consumed });
    }
    return true;
  }
}

registerProcessor("tempo-player", TempoPlayerProcessor);
