// The speech player: takes synthesized audio as it arrives and plays it at the chosen
// speed. The audio is kept at normal speed and time-stretched while it plays (see
// tempo-worklet.ts), so changing the speed is heard at once.

import workletUrl from "./tempo-worklet.ts?worker&url";
import { minPrebufferSeconds, prependSilence, rebufferSeconds } from "./playback";
import type { PlayerCommand, PlayerReport } from "./player-messages";

type PlayerEvents = {
  /** The job's last audio has been played. */
  onFinished: () => void;
  /** Audio ran out mid-job; playback waits for `waitSeconds` of audio before going on. */
  onRebuffer: (count: number, waitSeconds: number) => void;
  onError: (message: string) => void;
};

/** Silence put in front of the first audio after the output device is opened, which can clip it. */
const DEVICE_WAKE_SILENCE_MS = 160;

export class SpeechPlayer {
  private context: AudioContext | null = null;
  private node: AudioWorkletNode | null = null;
  /** The sample rate the worklet runs at. Equal to the audio's own rate when the browser allows it. */
  private contextRate = 0;
  /** The rate of the audio being sent, before any resampling to `contextRate`. */
  private sourceRate = 0;
  private deviceWoken = false;

  private epoch = 0;
  private tempo = 1;
  private paused = false;
  /** Samples sent for the current job, and how many of them have been played. */
  private sent = 0;
  private consumed = 0;
  private hasAudio = false;
  private started = false;
  private ended = false;
  private rebufferCount = 0;
  /** Keeps audio in arrival order: opening the audio device is asynchronous. */
  private pending: Promise<void> = Promise.resolve();

  constructor(private readonly events: PlayerEvents) {}

  /** True when nothing is playing and nothing is waiting to be played. */
  get idle(): boolean {
    return !this.hasAudio;
  }

  get isPaused(): boolean {
    return this.paused;
  }

  /** Playback speed. Takes effect on the audio that is about to be heard. */
  setTempo(tempo: number): void {
    this.tempo = tempo;
    this.post({ type: "tempo", tempo });
    this.startWhenReady();
  }

  /**
   * Opens the audio device ahead of time for audio at `sampleRate`, so the first words
   * of the first job do not wait for it (about half a second, once per start of the app).
   */
  prepare(sampleRate: number): void {
    this.inOrder(() => this.open(sampleRate));
  }

  /** Stops playback and drops everything queued. */
  reset(): void {
    this.epoch += 1;
    this.sent = 0;
    this.consumed = 0;
    this.hasAudio = false;
    this.started = false;
    this.ended = false;
    this.rebufferCount = 0;
    this.post({ type: "reset", epoch: this.epoch });
  }

  /** Adds audio (mono) to the end of the current job. */
  enqueue(samples: Float32Array, sampleRate: number): void {
    const epoch = this.epoch;
    this.hasAudio = true;
    this.inOrder(async () => {
      if (epoch !== this.epoch) {
        return;
      }
      await this.open(sampleRate);
      if (epoch !== this.epoch || !this.node) {
        return;
      }
      let audio = samples;
      if (!this.deviceWoken) {
        audio = prependSilence(audio, sampleRate, DEVICE_WAKE_SILENCE_MS);
        this.deviceWoken = true;
      }
      if (this.contextRate !== sampleRate) {
        audio = resampleLinear(audio, sampleRate, this.contextRate);
      }
      this.sent += audio.length;
      this.node.port.postMessage({ type: "speech", epoch, samples: audio } satisfies PlayerCommand, [
        audio.buffer as ArrayBuffer,
      ]);
      this.startWhenReady();
    });
  }

  /** Says that the current job will send no more audio; what is queued plays out. */
  end(): void {
    const epoch = this.epoch;
    this.inOrder(async () => {
      if (epoch !== this.epoch) {
        return;
      }
      this.ended = true;
      if (!this.hasAudio) {
        // The job produced no audio, or all of it was skipped.
        this.events.onFinished();
        return;
      }
      this.post({ type: "end", epoch });
      this.startWhenReady();
    });
  }

  /** Jumps past the audio that is waiting to be played. */
  skipQueued(): void {
    this.consumed = this.sent;
    this.post({ type: "skip", epoch: this.epoch });
  }

  async setPaused(paused: boolean): Promise<void> {
    this.paused = paused;
    if (!this.context) {
      return;
    }
    if (paused && this.context.state === "running") {
      await this.context.suspend();
    } else if (!paused && this.context.state === "suspended") {
      await this.context.resume();
    }
  }

  private inOrder(step: () => Promise<void>): void {
    this.pending = this.pending.then(step).catch((error) => {
      this.events.onError(`Audio playback failed: ${String(error)}`);
    });
  }

  private post(command: PlayerCommand): void {
    this.node?.port.postMessage(command);
  }

  /** Opens the audio device for audio at `sampleRate`, reusing it when the rate is unchanged. */
  private async open(sampleRate: number): Promise<void> {
    if (!this.context || this.sourceRate !== sampleRate) {
      const previous = this.context;
      this.context = null;
      this.node = null;
      if (previous) {
        await previous.close().catch(() => undefined);
      }
      // Running the device at the audio's own rate leaves resampling to the browser,
      // which does it better than this file would. Not every browser allows every rate.
      let context: AudioContext;
      try {
        context = new AudioContext({ sampleRate });
      } catch {
        context = new AudioContext();
      }
      await context.audioWorklet.addModule(workletUrl);
      const node = new AudioWorkletNode(context, "tempo-player", {
        numberOfInputs: 0,
        numberOfOutputs: 1,
        outputChannelCount: [1],
      });
      node.port.onmessage = (event: MessageEvent<PlayerReport>) => this.handleReport(event.data);
      node.onprocessorerror = () => this.events.onError("The audio player stopped unexpectedly.");
      node.connect(context.destination);
      this.context = context;
      this.node = node;
      this.contextRate = context.sampleRate;
      this.sourceRate = sampleRate;
      this.deviceWoken = false;
      this.post({ type: "reset", epoch: this.epoch });
      this.post({ type: "tempo", tempo: this.tempo });
    }
    if (this.context.state === "suspended" && !this.paused) {
      // Not awaited: a browser that wants a click first leaves this pending, and the
      // audio queued behind it must not wait on that.
      void this.context.resume().catch(() => undefined);
    }
  }

  private handleReport(report: PlayerReport): void {
    if (report.epoch !== this.epoch) {
      return;
    }
    if (report.type === "status") {
      this.consumed = Math.max(this.consumed, report.consumed);
      this.startWhenReady();
      return;
    }
    if (report.type === "finished") {
      this.hasAudio = false;
      this.started = false;
      this.events.onFinished();
      return;
    }
    // Starved: the audio ran out before more arrived. Wait for a larger reserve than
    // last time before going on, so a slow model gives a few pauses instead of stutter.
    this.started = false;
    if (!this.ended) {
      this.rebufferCount += 1;
      this.events.onRebuffer(this.rebufferCount, rebufferSeconds(this.tempo, this.rebufferCount));
    }
    this.startWhenReady();
  }

  /** Starts, or resumes after a refill, once enough audio is waiting for the current speed. */
  private startWhenReady(): void {
    if (this.started || !this.node || !this.hasAudio || this.contextRate === 0) {
      return;
    }
    // Faster playback drains the reserve sooner, so the reserve is measured in
    // seconds of listening, not seconds of source audio.
    const waitingSeconds = (this.sent - this.consumed) / this.contextRate / this.tempo;
    const required =
      this.rebufferCount > 0 ? rebufferSeconds(this.tempo, this.rebufferCount) : minPrebufferSeconds(this.tempo);
    if (this.ended || waitingSeconds >= required) {
      this.started = true;
      this.post({ type: "play", epoch: this.epoch });
    }
  }
}

/** Fallback for a browser that refuses to open the audio device at the audio's own rate. */
function resampleLinear(samples: Float32Array, fromRate: number, toRate: number): Float32Array {
  if (fromRate === toRate || samples.length === 0) {
    return samples;
  }
  const output = new Float32Array(Math.max(1, Math.round((samples.length * toRate) / fromRate)));
  const step = fromRate / toRate;
  for (let index = 0; index < output.length; index += 1) {
    const position = index * step;
    const left = Math.min(samples.length - 1, Math.floor(position));
    const right = Math.min(samples.length - 1, left + 1);
    const fraction = position - left;
    output[index] = samples[left] * (1 - fraction) + samples[right] * fraction;
  }
  return output;
}
