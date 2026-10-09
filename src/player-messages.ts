// Messages between the speech player (player.ts) and its audio-thread half
// (tempo-worklet.ts). `epoch` numbers the jobs: it goes up each time playback is reset,
// and anything carrying an older number is ignored by whichever side receives it.

export type PlayerCommand =
  | { type: "reset"; epoch: number }
  | { type: "tempo"; tempo: number }
  | { type: "speech"; epoch: number; samples: Float32Array }
  /** Start, or carry on after running out of audio. */
  | { type: "play"; epoch: number }
  /** No more speech will be sent for this job. */
  | { type: "end"; epoch: number }
  /** Drop everything that is waiting to be played. */
  | { type: "skip"; epoch: number };

export type PlayerReport =
  /** Sent several times a second. `consumed` counts source samples played so far. */
  | { type: "status"; epoch: number; consumed: number }
  /** Ran out of audio before the job ended; playback is on hold. */
  | { type: "starved"; epoch: number }
  /** The job's last audio has been played. */
  | { type: "finished"; epoch: number };
