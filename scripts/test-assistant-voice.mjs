#!/usr/bin/env node

// Checks the Assistant turn-taking rules in src/assistant-turns.ts (plan §3.3, §6.1, §6.3,
// §7.2): hold versus toggle on the talk button, the Codex F7 join, heard-text history for
// interrupted and preempted answers, and the playback stall watchdog (Codex F9).
//
// Run with: npm run test:assistant-voice

import assert from 'node:assert/strict';
import os from 'os';
import path from 'path';
import { fileURLToPath, pathToFileURL } from 'url';
import { build } from 'esbuild';

const here = path.dirname(fileURLToPath(import.meta.url));
const bundle = path.join(os.tmpdir(), `voicereader-assistant-voice-${process.pid}.mjs`);
await build({
  entryPoints: [path.join(here, '..', 'src', 'assistant-turns.ts')],
  bundle: true,
  format: 'esm',
  platform: 'node',
  outfile: bundle,
  logLevel: 'silent',
});
const T = await import(pathToFileURL(bundle).href);

let passed = 0;
function check(name, fn) {
  try {
    fn();
    passed += 1;
  } catch (error) {
    console.error(`FAIL ${name}`);
    throw error;
  }
}

// -- Hold or toggle -----------------------------------------------------------

check('hold records one turn and release ends it', () => {
  const toggle = new T.HoldToggle();
  assert.equal(toggle.press(0), 'start');
  assert.equal(toggle.active, true);
  assert.equal(toggle.release(T.HOLD_MS), 'stop');
  assert.equal(toggle.active, false);
});

check('a short click toggles on, the next press stops, its release does nothing', () => {
  const toggle = new T.HoldToggle();
  assert.equal(toggle.press(0), 'start');
  assert.equal(toggle.release(T.HOLD_MS - 1), 'none');
  assert.equal(toggle.active, true);
  assert.equal(toggle.press(5000), 'stop');
  assert.equal(toggle.release(9000), 'none');
  assert.equal(toggle.active, false);
  assert.equal(toggle.press(10000), 'start');
});

check('key repeat while held is ignored', () => {
  const toggle = new T.HoldToggle();
  assert.equal(toggle.press(0), 'start');
  assert.equal(toggle.press(50), 'none');
  assert.equal(toggle.press(100), 'none');
  assert.equal(toggle.release(1000), 'stop');
  assert.equal(toggle.release(1001), 'none');
});

check('reset while held makes the release do nothing', () => {
  const toggle = new T.HoldToggle();
  toggle.press(0);
  toggle.reset();
  assert.equal(toggle.active, false);
  assert.equal(toggle.release(5000), 'none');
  assert.equal(toggle.press(6000), 'start');
});

check('reset while toggled on lets the next press start again', () => {
  const toggle = new T.HoldToggle();
  toggle.press(0);
  toggle.release(10);
  toggle.reset();
  assert.equal(toggle.press(20), 'start');
});

// -- F7 join ------------------------------------------------------------------

check('a final with no newer turn is submitted', () => {
  const joiner = new T.TurnJoiner();
  joiner.beginPending();
  joiner.resolvePending(1);
  assert.deepEqual(joiner.final(1, ' hello there '), { kind: 'submit', text: 'hello there' });
  assert.equal(joiner.busy, false);
});

check('F7: A final arriving while B records is held and joined to the start of B', () => {
  const joiner = new T.TurnJoiner();
  joiner.beginPending();
  joiner.resolvePending(1); // A
  joiner.beginPending();
  joiner.resolvePending(2); // B began before A's final
  assert.deepEqual(joiner.final(1, 'what is the weather'), { kind: 'hold' });
  assert.equal(joiner.heldText, 'what is the weather');
  assert.deepEqual(joiner.final(2, 'in Paris tomorrow'), {
    kind: 'submit',
    text: 'what is the weather in Paris tomorrow',
  });
  assert.equal(joiner.busy, false);
});

check('F7: a press whose begin is still running already counts as a newer turn', () => {
  const joiner = new T.TurnJoiner();
  joiner.begin(1);
  joiner.beginPending(); // B pressed, id not known yet
  assert.deepEqual(joiner.final(1, 'first'), { kind: 'hold' });
  joiner.resolvePending(2);
  assert.deepEqual(joiner.final(2, 'second'), { kind: 'submit', text: 'first second' });
});

check('F7: three quick turns join in order', () => {
  const joiner = new T.TurnJoiner();
  joiner.begin(1);
  joiner.begin(2);
  joiner.begin(3);
  assert.deepEqual(joiner.final(1, 'one'), { kind: 'hold' });
  assert.deepEqual(joiner.final(2, 'two'), { kind: 'hold' });
  assert.deepEqual(joiner.final(3, 'three'), { kind: 'submit', text: 'one two three' });
});

check('F7: an empty newer turn still sends the held text', () => {
  const joiner = new T.TurnJoiner();
  joiner.begin(1);
  joiner.begin(2);
  joiner.final(1, 'keep this');
  assert.deepEqual(joiner.final(2, '   '), { kind: 'submit', text: 'keep this' });
});

check('empty text after joining is "empty"', () => {
  const joiner = new T.TurnJoiner();
  joiner.begin(1);
  joiner.begin(2);
  assert.deepEqual(joiner.final(1, ''), { kind: 'hold' });
  assert.deepEqual(joiner.final(2, ''), { kind: 'empty' });
});

check('finals for unknown or repeated turns are ignored', () => {
  const joiner = new T.TurnJoiner();
  assert.deepEqual(joiner.final(7, 'stale'), { kind: 'ignore' });
  joiner.begin(8);
  joiner.final(8, 'x');
  assert.deepEqual(joiner.final(8, 'again'), { kind: 'ignore' });
});

check('a newer turn that fails to start releases the held text', () => {
  const joiner = new T.TurnJoiner();
  joiner.begin(1);
  joiner.beginPending();
  joiner.final(1, 'held words');
  assert.equal(joiner.abandonPending(), 'held words');
  assert.equal(joiner.busy, false);
  assert.equal(joiner.abandonPending(), null);
});

check('a cancelled newer turn releases the held text', () => {
  const joiner = new T.TurnJoiner();
  joiner.begin(1);
  joiner.begin(2);
  joiner.final(1, 'held');
  assert.equal(joiner.cancel(2), 'held');
  assert.equal(joiner.cancel(2), null);
});

check('reset drops held text and outstanding turns', () => {
  const joiner = new T.TurnJoiner();
  joiner.begin(1);
  joiner.begin(2);
  joiner.final(1, 'held');
  joiner.reset();
  assert.equal(joiner.busy, false);
  assert.deepEqual(joiner.final(2, 'late'), { kind: 'ignore' });
});

// -- Heard text and history ---------------------------------------------------

const phrases = [
  { seq: 1, text: 'It is sunny. ' },
  { seq: 0, text: 'Good morning.' },
  { seq: 2, text: 'Take a hat.' },
];

check('heard text joins the played phrases in order', () => {
  assert.equal(T.heardText(phrases, -1), '');
  assert.equal(T.heardText(phrases, 0), 'Good morning.');
  assert.equal(T.heardText(phrases, 1), 'Good morning. It is sunny.');
  assert.equal(T.heardText(phrases, 5), 'Good morning. It is sunny. Take a hat.');
});

check('interrupted and stopped answers keep the heard part and a marker', () => {
  assert.equal(T.cutShortText('Good morning.', T.INTERRUPTED_MARK), 'Good morning.\n\n[interrupted by user]');
  assert.equal(T.cutShortText('', T.INTERRUPTED_MARK), '[interrupted by user]');
  assert.equal(T.cutShortText('Hi.', T.STOPPED_MARK), 'Hi.\n\n[stopped]');
  assert.equal(T.cutShortText('  ', T.STOPPED_MARK), '[stopped]');
});

check('history alternates and leaves out failed, running and empty exchanges', () => {
  const history = T.buildVoiceHistory([
    { user: 'hello', status: 'done', history: 'Hi there.' },
    { user: 'broken', status: 'error', history: '' },
    { user: 'tell me a story', status: 'interrupted', history: 'Once upon a time.\n\n[interrupted by user]' },
    { user: 'and then', status: 'interrupted', history: '[interrupted by user]' },
    { user: 'read this', status: 'stopped', history: '[stopped]' },
    { user: 'nothing came', status: 'done', history: '  ' },
    { user: 'now', status: 'active', history: '' },
  ]);
  assert.deepEqual(
    history.map((message) => [message.role, message.text]),
    [
      ['user', 'hello'],
      ['assistant', 'Hi there.'],
      ['user', 'tell me a story'],
      ['assistant', 'Once upon a time.\n\n[interrupted by user]'],
      ['user', 'and then'],
      ['assistant', '[interrupted by user]'],
      ['user', 'read this'],
      ['assistant', '[stopped]'],
    ],
  );
  for (let index = 0; index < history.length; index += 1) {
    assert.equal(history[index].role, index % 2 === 0 ? 'user' : 'assistant');
    assert.deepEqual(history[index].images, []);
  }
});

check('the heard prefix is found by letters and digits, ignoring Markdown markers', () => {
  const full = 'Good **morning**. It is *sunny* today, take a hat.';
  assert.equal(T.heardPrefixLength(full, ''), 0);
  assert.equal(full.slice(0, T.heardPrefixLength(full, 'Good morning.')), 'Good **morning**.');
  assert.equal(full.slice(0, T.heardPrefixLength(full, 'Good morning. It is sunny today,')), 'Good **morning**. It is *sunny* today,');
  assert.equal(T.heardPrefixLength(full, `${full} and more words`), full.length);
  assert.equal(T.heardPrefixLength('Café à 9 h.', 'Café à 9 h.'), 'Café à 9 h.'.length);
});

check('speech is appended to the draft with one space', () => {
  assert.equal(T.appendToDraft('', ' hello '), 'hello');
  assert.equal(T.appendToDraft('Note:', 'buy milk'), 'Note: buy milk');
  assert.equal(T.appendToDraft('Note: ', 'buy milk'), 'Note: buy milk');
  assert.equal(T.appendToDraft('line\n', 'next'), 'line\nnext');
  assert.equal(T.appendToDraft('keep', '   '), 'keep');
});

// -- Watchdog -----------------------------------------------------------------

check('a slow model (nothing queued) is never a stall', () => {
  let watch = T.newPlaybackWatch(0);
  assert.equal(T.playbackVerdict(watch, 60000), 'ok');
  watch = T.notePlayback(watch, 0.1, null, 1000);
  assert.equal(T.playbackVerdict(watch, 60000), 'ok');
  watch = T.notePlayback(watch, 0, 3, 2000);
  assert.equal(T.playbackVerdict(watch, 120000), 'ok');
});

check('queued audio with no progress for 5 s is a stall', () => {
  let watch = T.newPlaybackWatch(0);
  watch = T.notePlayback(watch, 2.5, null, 1000);
  assert.equal(T.playbackVerdict(watch, 1000 + T.STALL_MS - 1), 'ok');
  assert.equal(T.playbackVerdict(watch, 1000 + T.STALL_MS), 'stalled');
});

check('repeated reports with the same queue are not progress, a changed one or a new phrase is', () => {
  let watch = T.newPlaybackWatch(0);
  watch = T.notePlayback(watch, 2, null, 0);
  watch = T.notePlayback(watch, 2, null, 3000);
  assert.equal(T.playbackVerdict(watch, 5000), 'stalled');
  watch = T.notePlayback(watch, 1.9, null, 5000);
  assert.equal(T.playbackVerdict(watch, 9000), 'ok');
  watch = T.notePlayback(watch, 1.9, 0, 9500);
  assert.equal(T.playbackVerdict(watch, 14000), 'ok');
  assert.equal(watch.playedSeq, 0);
  watch = T.notePlayback(watch, 1.9, 0, 14000); // the same phrase again is not news
  assert.equal(T.playbackVerdict(watch, 14500), 'stalled');
});

check('the queue threshold is exclusive', () => {
  let watch = T.newPlaybackWatch(0);
  watch = T.notePlayback(watch, T.QUEUED_SECS, null, 0);
  assert.equal(T.playbackVerdict(watch, 60000), 'ok');
});

check('after "done", drained must follow within the queued audio plus 5 s', () => {
  let watch = T.newPlaybackWatch(0);
  watch = T.notePlayback(watch, 0.15, 2, 1000);
  watch = T.noteSpeechDone(watch, 2000);
  assert.equal(T.playbackVerdict(watch, 2000 + 150 + T.STALL_MS - 1), 'ok');
  assert.equal(T.playbackVerdict(watch, 2000 + 150 + T.STALL_MS), 'undrained');
});

check('progress after "done" extends the drain deadline', () => {
  let watch = T.newPlaybackWatch(0);
  watch = T.notePlayback(watch, 6, null, 0);
  watch = T.noteSpeechDone(watch, 100);
  for (let t = 250, left = 5.75; left > 0; t += 250, left -= 0.25) {
    watch = T.notePlayback(watch, left, null, t);
    assert.equal(T.playbackVerdict(watch, t), 'ok');
  }
  watch = T.notePlayback(watch, 0, 9, 6100);
  assert.equal(T.playbackVerdict(watch, 6100 + T.STALL_MS - 1), 'ok');
  assert.equal(T.playbackVerdict(watch, 6100 + T.STALL_MS), 'undrained');
  // A second "done" does not move the deadline.
  assert.equal(T.noteSpeechDone(watch, 9000).doneAt, 100);
});

check('a non-finite report keeps the last queue', () => {
  let watch = T.newPlaybackWatch(0);
  watch = T.notePlayback(watch, 1, null, 0);
  watch = T.notePlayback(watch, Number.NaN, null, 1000);
  assert.equal(watch.buffered, 1);
});

// -- Level --------------------------------------------------------------------

check('level is 0 for silence and grows on a decibel scale', () => {
  assert.equal(T.levelFraction(0), 0);
  assert.equal(T.levelFraction(-1), 0);
  assert.equal(T.levelFraction(Number.NaN), 0);
  assert.equal(T.levelFraction(1), 1);
  const quiet = T.levelFraction(0.02);
  const loud = T.levelFraction(0.2);
  assert.ok(quiet > 0.3 && quiet < 0.5, `quiet ${quiet}`);
  assert.ok(loud > 0.85 && loud < 0.95, `loud ${loud}`);
});

console.log(`assistant voice: ${passed} checks passed`);
