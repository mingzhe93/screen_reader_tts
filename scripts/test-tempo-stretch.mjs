#!/usr/bin/env node

// Checks the time-stretcher the speech player uses (src/tempo-stretch.ts): that normal
// speed leaves the audio untouched, that other speeds give the right length, and that
// changing speed mid-stream neither breaks nor loses audio.
//
// Run with: npm run test:player

import assert from 'node:assert/strict';
import os from 'os';
import path from 'path';
import { fileURLToPath, pathToFileURL } from 'url';
import { build } from 'esbuild';

const here = path.dirname(fileURLToPath(import.meta.url));
const bundle = path.join(os.tmpdir(), `voicereader-tempo-stretch-${process.pid}.mjs`);
await build({
  entryPoints: [path.join(here, '..', 'src', 'tempo-stretch.ts')],
  bundle: true,
  format: 'esm',
  outfile: bundle,
  logLevel: 'silent',
});
const { TempoStretcher, FloatQueue } = await import(pathToFileURL(bundle).href);

const RATE = 24000;

// A voice-like test signal: a pitch that wanders, with pauses.
function speechLike(seconds) {
  const samples = new Float32Array(Math.round(seconds * RATE));
  let phase = 0;
  for (let i = 0; i < samples.length; i += 1) {
    const t = i / RATE;
    const pitch = 120 + 40 * Math.sin(2 * Math.PI * 0.7 * t);
    phase += (2 * Math.PI * pitch) / RATE;
    const loudness = Math.max(0, Math.sin(2 * Math.PI * 1.3 * t));
    samples[i] = 0.3 * loudness * (Math.sin(phase) + 0.5 * Math.sin(2 * phase) + 0.25 * Math.sin(3 * phase));
  }
  return samples;
}

// Feeds `input` in uneven pieces; `tempoAt(position)` may change the speed on the way.
function stretch(input, tempoAt) {
  const stretcher = new TempoStretcher(RATE);
  const pieces = [];
  const take = () => {
    const ready = stretcher.available;
    if (ready > 0) {
      const piece = new Float32Array(ready);
      assert.equal(stretcher.read(piece, 0, ready), ready);
      pieces.push(piece);
    }
  };
  let position = 0;
  let step = 0;
  while (position < input.length) {
    const size = [1024, 37, 4096, 128, 911][step % 5];
    stretcher.setTempo(tempoAt(position / RATE));
    stretcher.write(input.subarray(position, Math.min(input.length, position + size)));
    take();
    position += size;
    step += 1;
  }
  stretcher.finish();
  take();
  const output = new Float32Array(pieces.reduce((total, piece) => total + piece.length, 0));
  let at = 0;
  for (const piece of pieces) {
    output.set(piece, at);
    at += piece.length;
  }
  return output;
}

const peak = (samples) => samples.reduce((max, sample) => Math.max(max, Math.abs(sample)), 0);
const input = speechLike(6);

// Normal speed is an exact pass-through.
const same = stretch(input, () => 1);
assert.ok(same.length >= input.length);
for (let i = 0; i < input.length; i += 1) {
  assert.equal(same[i], input[i], `sample ${i} changed at normal speed`);
}

// Other speeds give the right length (plus the short tail of silence that flushing adds).
for (const tempo of [0.25, 0.5, 0.75, 1.25, 1.5, 2, 3, 4]) {
  const output = stretch(input, () => tempo);
  const seconds = output.length / RATE;
  const expected = 6 / tempo;
  assert.ok(seconds >= expected - 0.1 && seconds <= expected + 0.6, `${tempo}x gave ${seconds.toFixed(2)} s, expected about ${expected.toFixed(2)} s`);
  assert.ok(output.every(Number.isFinite), `${tempo}x produced a value that is not a number`);
  assert.ok(peak(output) <= peak(input) * 1.05, `${tempo}x got louder than the input`);
  assert.ok(peak(output) >= peak(input) * 0.6, `${tempo}x lost the signal`);
}

// Changing speed mid-stream: 2 s at 1x, 2 s at 2x, 2 s at 0.5x should take about 2 + 1 + 4 s.
const mixed = stretch(input, (seconds) => (seconds < 2 ? 1 : seconds < 4 ? 2 : 0.5));
const mixedSeconds = mixed.length / RATE;
assert.ok(mixedSeconds > 6.6 && mixedSeconds < 7.8, `mixed speeds gave ${mixedSeconds.toFixed(2)} s, expected about 7 s`);
// The stretch before the first change is still untouched.
for (let i = 0; i < RATE; i += 1) {
  assert.equal(mixed[i], input[i], `sample ${i} changed before the first speed change`);
}
// No clicks: a jump between neighbouring samples far beyond anything in the input.
const largestStep = (samples) => {
  let largest = 0;
  for (let i = 1; i < samples.length; i += 1) {
    largest = Math.max(largest, Math.abs(samples[i] - samples[i - 1]));
  }
  return largest;
};
assert.ok(largestStep(mixed) <= largestStep(input) * 3, 'changing speed left a click in the audio');

// The queue keeps samples in order across growth and partial reads.
const queue = new FloatQueue(4);
queue.push(Float32Array.of(1, 2, 3));
const firstTwo = new Float32Array(2);
assert.equal(queue.read(firstTwo, 0, 2), 2);
queue.push(Float32Array.of(4, 5, 6, 7, 8));
assert.deepEqual(Array.from(firstTwo), [1, 2]);
assert.deepEqual(Array.from(queue.view()), [3, 4, 5, 6, 7, 8]);
queue.drop(100);
assert.equal(queue.length, 0);

console.log('✓ tempo stretcher: pass-through at 1x, lengths at 0.25x to 4x, mid-stream speed changes');
