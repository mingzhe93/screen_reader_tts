// Pitch-preserving time-stretch for speech, run inside the audio player so a change of
// speed is heard at once. The method is WSOLA (waveform-similarity overlap-add), the
// same family as the SoX `tempo` effect the backend used before.
//
// No DOM or Web Audio types here: this file runs in the audio worklet and in Node tests.

/** A growable first-in first-out buffer of samples. */
export class FloatQueue {
  private data: Float32Array;
  private head = 0;
  private tail = 0;

  constructor(capacity = 4096) {
    this.data = new Float32Array(capacity);
  }

  get length(): number {
    return this.tail - this.head;
  }

  /** The queued samples, oldest first. Valid until the next push or drop. */
  view(): Float32Array {
    return this.data.subarray(this.head, this.tail);
  }

  push(samples: Float32Array): void {
    this.reserve(samples.length);
    this.data.set(samples, this.tail);
    this.tail += samples.length;
  }

  /** Appends `count` zeros and returns the region to fill in. */
  extend(count: number): Float32Array {
    this.reserve(count);
    const region = this.data.subarray(this.tail, this.tail + count);
    region.fill(0);
    this.tail += count;
    return region;
  }

  drop(count: number): void {
    this.head = Math.min(this.tail, this.head + count);
    if (this.head === this.tail) {
      this.head = 0;
      this.tail = 0;
    }
  }

  /** Moves up to `count` samples into `target` at `offset`; returns how many were moved. */
  read(target: Float32Array, offset: number, count: number): number {
    const taken = Math.min(count, this.length);
    target.set(this.data.subarray(this.head, this.head + taken), offset);
    this.drop(taken);
    return taken;
  }

  clear(): void {
    this.head = 0;
    this.tail = 0;
  }

  private reserve(extra: number): void {
    if (this.tail + extra <= this.data.length) {
      return;
    }
    const needed = this.length + extra;
    if (needed <= this.data.length && this.head > 0) {
      this.data.copyWithin(0, this.head, this.tail);
    } else {
      const grown = new Float32Array(Math.max(needed, this.data.length * 2));
      grown.set(this.data.subarray(this.head, this.tail));
      this.data = grown;
    }
    this.tail = this.length;
    this.head = 0;
  }
}

// Segment sizes that suit speech. Shorter segments than the music defaults keep
// syllables intact when speeding up.
const SEQUENCE_MS = 40;
const SEEK_MS = 15;
const OVERLAP_MS = 8;

/** One WSOLA pass. Its output length is its input length divided by the tempo. */
class TempoStage {
  private readonly sequence: number;
  private readonly seek: number;
  private readonly overlap: number;
  private readonly input = new FloatQueue();
  private readonly tail: Float32Array;
  private hasTail = false;
  private tempo = 1;
  private skipRemainder = 0;

  constructor(sampleRate: number) {
    this.overlap = Math.max(16, Math.round((sampleRate * OVERLAP_MS) / 1000));
    this.sequence = Math.max(this.overlap * 3, Math.round((sampleRate * SEQUENCE_MS) / 1000));
    this.seek = Math.max(16, Math.round((sampleRate * SEEK_MS) / 1000));
    this.tail = new Float32Array(this.overlap);
  }

  setTempo(tempo: number): void {
    this.tempo = tempo;
  }

  /** The most input one step can need, at the current tempo. */
  get inputPerStep(): number {
    return Math.max(Math.ceil(this.tempo * (this.sequence - this.overlap)) + 1 + this.overlap, this.sequence) + this.seek;
  }

  reset(): void {
    this.input.clear();
    this.hasTail = false;
    this.skipRemainder = 0;
  }

  /** Takes `samples` and appends whatever output is ready to `output`. */
  push(samples: Float32Array, output: FloatQueue): void {
    this.input.push(samples);
    const body = this.sequence - 2 * this.overlap;
    for (;;) {
      const advance = this.skipRemainder + this.tempo * (this.sequence - this.overlap);
      const skip = Math.floor(advance);
      if (this.input.length < Math.max(skip + this.overlap, this.sequence) + this.seek) {
        return;
      }
      const source = this.input.view();

      // Where, near the nominal position, does the input best continue what was
      // just played? Starting the next segment there avoids audible seams.
      const offset = this.hasTail ? this.bestOffset(source) : 0;
      const out = output.extend(this.sequence - this.overlap);
      if (this.hasTail) {
        for (let i = 0; i < this.overlap; i += 1) {
          const weight = i / this.overlap;
          out[i] = this.tail[i] * (1 - weight) + source[offset + i] * weight;
        }
      } else {
        out.set(source.subarray(offset, offset + this.overlap));
      }
      out.set(source.subarray(offset + this.overlap, offset + this.overlap + body), this.overlap);
      this.tail.set(source.subarray(offset + this.overlap + body, offset + this.sequence));
      this.hasTail = true;

      this.skipRemainder = advance - skip;
      this.input.drop(skip);
    }
  }

  /** Pushes out the audio still held back. Call once, after the last `push`. */
  finish(output: FloatQueue): void {
    // Enough silence for the steps to run past the last real sample, including the
    // few milliseconds each step holds back for its next crossfade.
    this.push(new Float32Array(this.inputPerStep + this.sequence), output);
    this.reset();
  }

  private bestOffset(source: Float32Array): number {
    const { overlap, seek, tail } = this;
    let best = 0;
    let bestScore = -Infinity;
    let energy = 0;
    for (let i = 0; i < overlap; i += 1) {
      energy += source[i] * source[i];
    }
    for (let offset = 0; offset < seek; offset += 1) {
      let correlation = 0;
      for (let i = 0; i < overlap; i += 1) {
        correlation += tail[i] * source[offset + i];
      }
      // Normalised, so a loud stretch is not preferred just for being loud.
      const score = correlation / Math.sqrt(energy + 1e-9);
      if (score > bestScore) {
        bestScore = score;
        best = offset;
      }
      const leaving = source[offset];
      const entering = source[offset + overlap];
      energy += entering * entering - leaving * leaving;
    }
    return best;
  }
}

/**
 * Changes the speed of speech without changing its pitch.
 *
 * Several gentle passes sound better than one large one, so the stretch is split over
 * a fixed number of stages. The number never changes, which lets the tempo be changed
 * mid-stream without dropping or repeating audio. At a tempo of exactly 1 every stage
 * passes its input through untouched.
 */
export class TempoStretcher {
  private readonly stages: TempoStage[];
  private readonly scratch: FloatQueue[];
  private readonly output = new FloatQueue(16384);

  // Two stages measured best: speech sped up 1.5 to 3 times was recognised more
  // accurately than with one stage or three, and than with the SoX chain used before.
  constructor(sampleRate: number, stageCount = 2) {
    this.stages = Array.from({ length: stageCount }, () => new TempoStage(sampleRate));
    this.scratch = Array.from({ length: stageCount - 1 }, () => new FloatQueue());
    this.setTempo(1);
  }

  setTempo(tempo: number): void {
    const perStage = Math.pow(Math.min(4, Math.max(0.25, tempo)), 1 / this.stages.length);
    for (const stage of this.stages) {
      stage.setTempo(perStage);
    }
  }

  /** Output samples ready to be read. */
  get available(): number {
    return this.output.length;
  }

  write(samples: Float32Array): void {
    this.run(samples, false);
  }

  /** Pushes out the audio the stages still hold. Call once, after the last `write`. */
  finish(): void {
    this.run(new Float32Array(0), true);
  }

  read(target: Float32Array, offset: number, count: number): number {
    return this.output.read(target, offset, count);
  }

  reset(): void {
    for (const stage of this.stages) {
      stage.reset();
    }
    for (const queue of this.scratch) {
      queue.clear();
    }
    this.output.clear();
  }

  private run(samples: Float32Array, finishing: boolean): void {
    let current = samples;
    this.stages.forEach((stage, index) => {
      const last = index === this.stages.length - 1;
      const target = last ? this.output : this.scratch[index];
      stage.push(current, target);
      if (finishing) {
        stage.finish(target);
      }
      if (!last) {
        // Copied, because the next stage's input may grow while this view is in use.
        current = target.view().slice();
        target.clear();
      }
    });
  }
}
