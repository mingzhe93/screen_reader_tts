# Phrase buffer and speakable-text filter (`assistant_phrases.rs`)

Status: Step 1 groundwork for conversation mode (later step). Rust only, pure, written and unit-tested on macOS. Nothing calls it yet; the compiler reports `dead_code` warnings for it until the conversation controller does.

## What it does

`src-tauri/src/assistant_phrases.rs` turns the model's streamed answer (small text deltas) into an ordered list of **phrases** ready for text-to-speech. It implements `docs/v0.3.0_assistant_plan.md` section 8.1 (speakable text: a light final filter, the Voice system prompt does most of the work) and the phrase-buffer part of section 8.2 (speak complete sentences or sensible clauses, bounded waiting and length, abbreviations, numbers, partial words, committed phrases never revised).

It is pure: no threads, no I/O, no clock reads (the caller passes `now_ms`), no new dependencies. It is compiled only with the `build-base` feature (the `mod` line in `lib.rs` is added by the integrator).

Two stages run inside one `PhraseBuffer`:

1. **Cleaner** (streaming state machine): drops `<think>` blocks and fenced code, strips Markdown markers, replaces URLs, drops emoji, collapses whitespace.
2. **Segmenter**: cuts the cleaned text into phrases at sentence ends, clause marks or word boundaries.

## Interface (all `pub(crate)`)

```rust
struct PhraseConfig { min_chars: usize, max_chars: usize, max_wait_ms: u64, first_min_chars: usize } // Default below
struct Phrase { seq: u32, text: String }          // Debug, Clone, PartialEq
impl PhraseBuffer {
    fn new(config: PhraseConfig) -> Self;
    fn push(&mut self, delta: &str, now_ms: u64) -> Vec<Phrase>;
    fn tick(&mut self, now_ms: u64) -> Vec<Phrase>;
    fn finish(&mut self) -> Vec<Phrase>;          // flushes, then resets the buffer for reuse
}
```

Config defaults and the reason for each:

| Field | Default | Why |
|---|---|---|
| `min_chars` | 24 | A phrase like "Yes." synthesises with poor prosody and causes many tiny audio jobs; short sentences are joined with the next one. |
| `first_min_chars` | 12 | The first phrase may be shorter so speech starts sooner. |
| `max_chars` | 220 | Bounds how long TTS works on one phrase (about 15 s of speech) and so how late a cancel or interrupt takes effect. |
| `max_wait_ms` | 1200 | Roughly the player's refill threshold (plan 8.3): do not leave spoken text sitting while the model stalls. |

Phrases: `seq` is 0, 1, 2, ... in speaking order; `text` is never empty and has no newline or surrounding whitespace. Phrases with no letter or digit at all (a lone `,`) are dropped.

## Rules

**Boundaries.** A phrase ends at `.` `!` `?` `…` (and `。！？`), optionally followed by closing quotes or brackets, then whitespace or the end of the stream. A newline after text also ends a phrase. Not a boundary:

- a `.` not followed by whitespace, so decimals (`3.14`), versions (`v0.3.0`), times (`10.30`), domains, emails;
- abbreviations: `e.g.` `i.e.` `vs.` `cf.` `a.m.` `p.m.` `Mr.` `Mrs.` `Ms.` `Dr.` `St.` `Prof.` `Jr.` `Sr.` `Fig.`, dotted initialisms (`U.S.`), single capital initials (`J. R. R.`, but not the pronoun `I`);
- `etc.` unless a capital letter follows; `No.` only when a digit follows (`No. 5`) so "No. I disagree." splits;
- an ellipsis (`...` or `…`) unless the next non-space character is a capital letter.

Full-width `。！？` end a phrase even without a following space (CJK text has none). Punctuation at the very end of the text so far is held until the next character shows whether it is a boundary (`3.` then `14`).

**Clause cuts.** When no sentence end appears within `max_chars`, the phrase is cut at the last `,` `;` `:` (only when followed by a space) or em dash (full-width `，、；：` also) inside the first `max_chars + 1` characters, else at the last space. If the character right after the first `max_chars` characters is whitespace, the cut is made there (an exact fit, the same result a newline there would give). A cut is only used if the phrase is at least the minimum size. A single word longer than `max_chars` is never split: the buffer waits for its end (or `finish`).

**Minimum size.** A boundary that would give a phrase shorter than `min_chars` (`first_min_chars` for seq 0) is skipped, so the short text joins the following text (and the join may then be cut by `max_chars`). `finish` flushes whatever is left even if short.

**Speakable filter.**

- `<think>...</think>` is dropped (tags are matched case-insensitively and may be split across deltas). A stray `</think>` is dropped. An unclosed think block is dropped to the end.
- Fenced code (a line starting with three or more backticks or tildes, up to the closing fence line) is skipped. One phrase "I've put the code on screen." is emitted per block, at the opening fence, as a phrase of its own (unless the minimum size merges it).
- Markdown: leading `#`s, `*` and backticks removed, `_` removed unless inside a word (`snake_case` is kept), `>` quotes, list bullets (`-` `*` `+`, `1.` / `1)`), horizontal rules and table separator rows dropped, table `|` becomes a comma pause (the comma is added after the cell's trailing space and normalised to `a, b` when the phrase is emitted; a trailing `|` adds nothing), `[text](url)` becomes `text`, `![alt](url)` becomes `alt`.
- Bare `http://`, `https://` and `www.` URLs become "a link"; trailing punctuation after the URL is kept.
- Emoji and pictographs are treated as spaces; zero-width characters are removed; whitespace is collapsed (a newline wins over a space).

**Order and integrity.** The phrases joined with a space equal the cleaned answer (whitespace normalised, plus the code notice). No text is lost or duplicated, whatever the delta boundaries.

## Split invariance (the key property)

`push` plus `finish` return the same phrases (same `seq`, same text) for any division of the input into deltas, and `now_ms` passed to `push` has no effect. How it is achieved: every decision that needs look-ahead (a trailing period, a `<th` that may become `<think>`, a possible `[link](`, a `_` that needs its right neighbour, a fence run, a URL prefix, a list marker) returns "need more" until the look-ahead exists or `finish` is called; a decision is then always a function of the same stream prefix. Look-ahead is bounded (link text 300 characters, link target 1500, rule-line check 200) so nothing waits forever on stray brackets. The max-length cut looks at the first `max_chars + 1` characters only. Whitespace is kept as a pending separator and is visible to the segmenter only as a virtual trailing character, so it can be upgraded from a space to a newline later without changing earlier decisions.

Only `tick` is time dependent, deliberately.

## `tick`

`tick(now_ms)` does nothing until `now_ms - last_growth >= max_wait_ms`, where `last_growth` is the `now_ms` of the last `push` that added speakable text (a delta that is entirely dropped, such as code, does not reset the timer). When stalled it:

1. resolves held look-ahead at the end of the text as "nothing follows", so a final `.` ends a sentence (the usual case);
2. otherwise, if at least `min_chars` of complete words are pending, emits them, cut at the last clause mark if there is one, else after the last complete word. A trailing word with no space after it is assumed unfinished and stays pending.

A tick cut is not undone, so it can split where an uninterrupted stream would not; that is the price of not leaving speech waiting.

## Tests

Unit tests are in the module (`#[cfg(test)]`): each rule has positive and negative cases, plus

- split invariance over a corpus (prose, numbers, abbreviations, list, code block, think block, table, URLs and links, Markdown) under default, tiny (`min 1`) and small (`max 40`) configs, with splits by single characters, words, a seeded LCG at three chunk sizes, every two-way split, all three-way splits of a tricky text, and a 2000-case random fuzz of Markdown-like atoms (it found two real bugs when run with 1.5 million cases: text inserted before already-seen whitespace, and a trailing space versus a newline at exactly `max_chars`; both are fixed and pinned in `regressions_found_by_fuzzing`; raise the case count in `split_invariance_fuzz` for a longer soak);
- timing tests for `tick`;
- a 50 KB answer pushed one character at a time (bound: 50 ms in release, 3 s in debug).

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"   # macOS toolchain path; on Windows cargo is normally on PATH
cd src-tauri
cargo test --features build-base assistant_phrases::
cargo test --release --features build-base assistant_phrases::linear_performance   # optional, checks the 50 ms bound
```

The module has no platform-specific code, so Windows needs no special manual check beyond running the same tests. Manual checks once it is wired up (expected results):

- Speak a long answer in conversation mode with a Voice prompt answer and with deliberately Markdown-heavy text: no spoken `star`, `hash`, `pipe` or `backtick`; code is replaced by one "I've put the code on screen."
- Throttle the model (slow tokens): after about 1.2 s of silence mid-sentence, speech continues at a comma or word boundary and never mid-word.
- A reply containing "3.14", "v0.3.0", "Dr. Smith" is not cut inside those.

## How the coordinator uses it

One `PhraseBuffer` per answer (the buffer resets after `finish`, but a new one per response id keeps stale state out):

1. `push(delta, now_ms)` for every `content` delta from the chat stream (not `reasoning`), with a monotonic `now_ms` (for example milliseconds since the response began). Queue the returned phrases for TTS in order.
2. `tick(now_ms)` from a 100 ms timer while the answer is open. It is cheap when nothing is pending.
3. `finish()` when the chat reports done (also after Stop, if the text received so far should still be spoken), and queue the result.
4. On cancel, drop the buffer; do not call `finish`.

The chat page still shows the raw text; the filter only affects what is spoken.

## Known limitations and gaps

- English-centred. Abbreviation lists, "capital letter after ellipsis" and the `I`/initial heuristic are English. Other languages with period-based abbreviations may split early (harmless, joined by the minimum size) or the abbreviation list will not apply.
- CJK without spaces: sentence ends (`。！？`) and full-width clause marks are handled, but a very long CJK run with no such mark and no space waits until `finish` (no word boundary to cut at).
- Nested or unusual Markdown is not handled: link text containing `[`, reference-style links (`[a][b]`), `~~strike~~`, task-list boxes `[ ]`, HTML tags other than `<think>`, and setext headings are left as text (or lightly stripped). A fence is recognised by its first three characters; the CommonMark rule that a backtick info string has no backticks is not applied.
- `_` between letters is kept, so `snake__case` loses its double underscore; `*` is always removed, so `5*3` is read as `53`.
- A single capital letter followed by a period (other than `I`) is treated as an initial, so "Plan B. Then" does not split there; the minimum/maximum sizes bound the effect.
- The stall timer measures time since text last grew, not since the phrase began. A model that trickles one token per second never triggers `tick` but is still bounded by `max_chars`.
- Emoji removal uses fixed ranges; unusual symbols outside them are passed to TTS.

Next things to build: the conversation controller that owns the response audio stream (plan 8.2 and 6.1), the conversation-specific buffering policy (plan 8.3), and a check of how the existing text normaliser treats a phrase that ends without final punctuation.
