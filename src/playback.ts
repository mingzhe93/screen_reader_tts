export function minPrebufferSeconds(rate: number): number {
  // Faster playback drains buffered audio sooner; hold more before starting.
  const base = 0.24;
  if (rate <= 1) {
    return base;
  }
  if (rate <= 2) {
    const scaled = base + (rate - 1) * 0.45;
    return Math.min(0.85, Math.max(base, scaled));
  }
  const highRateScaled = 0.85 + (rate - 2) * 1.0;
  return Math.min(2.0, Math.max(0.85, highRateScaled));
}

export function rebufferSeconds(rate: number, rebufferCount: number): number {
  // Each time a job runs dry, wait for more audio than the last time before resuming, so
  // a model that cannot keep up gives a few longer pauses instead of constant stutter.
  const REBUFFER_STEP_SECONDS = 1.0;
  const REBUFFER_MAX_SECONDS = 2.0;
  const target = Math.min(REBUFFER_MAX_SECONDS, REBUFFER_STEP_SECONDS * rebufferCount);
  return Math.max(minPrebufferSeconds(rate), target);
}

export function prependSilence(samples: Float32Array, sampleRate: number, ms: number): Float32Array {
  const silenceFrames = Math.max(0, Math.round((sampleRate * ms) / 1000));
  if (silenceFrames === 0) {
    return samples;
  }
  const withSilence = new Float32Array(silenceFrames + samples.length);
  withSilence.set(samples, silenceFrames);
  return withSilence;
}

export function decodePcm16Base64ToFloat32(base64Data: string): Float32Array {
  const binary = atob(base64Data);
  const bytes = new Uint8Array(binary.length);
  for (let idx = 0; idx < binary.length; idx += 1) {
    bytes[idx] = binary.charCodeAt(idx);
  }

  const view = new DataView(bytes.buffer);
  const sampleCount = Math.floor(bytes.byteLength / 2);
  const output = new Float32Array(sampleCount);
  for (let idx = 0; idx < sampleCount; idx += 1) {
    const value = view.getInt16(idx * 2, true);
    output[idx] = value / 32768;
  }
  return output;
}
