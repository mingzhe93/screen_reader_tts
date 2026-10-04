# VoiceReader copy of parakeet-rs

This is parakeet-rs 0.3.8 (https://github.com/altunenes/parakeet-rs, MIT OR Apache-2.0)
with two changes to `src/multitalker.rs`. The examples, tests and scripts of the upstream crate are left out.

## The change: speaker hold (`src/multitalker.rs`)

The multitalker model emits a word a little after it was spoken. Upstream gives the
speech encoder each speaker's diarizer activity as it is, so the moment a speaker stops,
their input is switched off and the words still on their way out are lost. In practice
the last words before every pause or speaker change went missing.

The patch keeps a speaker's activity switched on for a short time after the diarizer
says they stopped:

- `MultitalkerConfig::speaker_hold_secs` and `background_hold_secs`, both 0 by default,
  which is upstream behaviour.
- `MultitalkerASR::set_speaker_hold(speaker_secs, background_secs)`.
- `hold_activity`, which applies the hold to the diarizer output, and its unit tests.

## The second change: speakers beyond the limit

Speakers beyond `max_speakers` are skipped without a trace upstream, so their words
are missing from the transcript. The patch:

- transcribes them together as one extra speaker with the id `UNASSIGNED_SPEAKER_ID`
  (`MultitalkerASR::set_transcribe_beyond_limit`, off by default);
- counts how much speech the diarizer gave to them
  (`MultitalkerASR::speech_beyond_limit_secs`).

Every changed place is marked with a `VoiceReader patch` comment. To move to a newer
upstream version, copy its `src` here and reapply those places.
