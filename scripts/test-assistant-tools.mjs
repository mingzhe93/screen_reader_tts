#!/usr/bin/env node
// Tool fragments and actual voice event handlers under a minimal text-only DOM/Tauri mock.
import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';
import fs from 'node:fs/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { build } from 'esbuild';
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const bundle = path.join(os.tmpdir(), `voicereader-tools-${process.pid}.mjs`);
const events = new Map();
const calls = [];
let installed = true;
let installFails = false;
let response = { job_id: 'job1', dropped_messages: 0 };
globalThis.__listen = async (name, fn) => { const list = events.get(name) ?? []; list.push(fn); events.set(name, list); return () => {}; };
globalThis.__invoke = async (name, args) => {
  calls.push({ name, args });
  if (name === 'assistant_tools_settings') return { provider: 'duckduckgo', brave_key_set: false, limits: { max_rounds: 5, search_timeout_secs: 8, fetch_timeout_secs: 10, weather_timeout_secs: 5, code_timeout_secs: 60 } };
  if (name === 'assistant_tools_status') return { installed, state: installed ? 'ready' : 'not_installed', version: '3.14', download_size_bytes: 15000000, installed_size_bytes: 40000000 };
  if (name === 'assistant_tools_install') { if (installFails) throw new Error('Download failed'); installed = true; }
  if (name === 'assistant_voice_respond') return response;
  return {};
};
class Element {
  constructor(tag, text = '') { this.tagName = tag; this.children = []; this.attributes = {}; this.dataset = {}; this.handlers = {}; this._text = text; this.className = ''; this.value = ''; this.open = false; this.scrollHeight = 100; this.scrollTop = 0; this.clientHeight = 100; this.style = { setProperty() {} }; this.classList = {
    contains: n => this.className.split(' ').includes(n),
    add: n => { if (!this.classList.contains(n)) this.className = `${this.className} ${n}`.trim(); },
    remove: n => { this.className = this.className.split(' ').filter(x => x !== n).join(' '); },
    toggle: (n, enabled) => { const on = enabled ?? !this.classList.contains(n); on ? this.classList.add(n) : this.classList.remove(n); return on; },
  }; }
  append(...nodes) { for (const node of nodes) { this.children.push(node); node.parent = this; } }
  appendChild(node) { this.append(node); return node; }
  replaceChildren(...nodes) { this.children = []; this._text = ''; this.append(...nodes); }
  remove() { if (this.parent) this.parent.children = this.parent.children.filter(n => n !== this); }
  set textContent(value) { this._text = value ?? ''; this.children = []; }
  get textContent() { return this._text + this.children.map(n => n.textContent).join(''); }
  set innerHTML(_) { throw new Error('Unsafe HTML rendering'); }
  get firstChild() { return this.children[0] ?? null; }
  setAttribute(name, value) { this.attributes[name] = String(value); }
  getAttribute(name) { return this.attributes[name]; }
  addEventListener(name, fn) { (this.handlers[name] ??= []).push(fn); }
  trigger(name, value = {}) { for (const fn of this.handlers[name] ?? []) fn({ preventDefault() {}, ...value }); }
  querySelectorAll(selector) { return this.children.flatMap(child => [ ...(selector === child.tagName || (selector.startsWith('.') && child.classList.contains(selector.slice(1))) ? [child] : []), ...child.querySelectorAll(selector) ]); }
  getBoundingClientRect() { return { left: 100, top: 500, bottom: 532 }; }
  contains(node) { return node === this || this.children.some(n => n.contains(node)); }
  focus() {} setPointerCapture() {} setCustomValidity() {} reportValidity() { return false; } checkValidity() { return true; }
}
const storage = new Map();
const confirms = [];
const windowConfirms = [];
let confirmation = true;
let dialogFails = false;
globalThis.__confirm = async (text) => { if (dialogFails) throw new Error('dialog not allowlisted'); confirms.push(text); return confirmation; };
let frame = 0;
const frames = new Map();
globalThis.document = { addEventListener() {}, createElement: tag => new Element(tag), createElementNS: (_, tag) => new Element(tag), createTextNode: text => new Element('#text', text) };
globalThis.window = { innerWidth: 1024, innerHeight: 768, addEventListener() {}, localStorage: { getItem: k => storage.get(k) ?? null, setItem: (k, v) => storage.set(k, v) }, // Tauri v1 really replaces window.confirm with a Promise-returning function (always truthy).
  confirm: text => { windowConfirms.push(text); return Promise.resolve(false); }, setInterval: () => 1, clearInterval() {}, setTimeout: () => 1, clearTimeout() {} };
globalThis.requestAnimationFrame = fn => { frames.set(++frame, fn); return frame; };
globalThis.cancelAnimationFrame = n => frames.delete(n);
function flush() { const callbacks = [...frames.values()]; frames.clear(); callbacks.forEach(fn => fn()); }
async function tick() { await new Promise(resolve => setImmediate(resolve)); flush(); }
function emit(name, payload) { for (const fn of events.get(name) ?? []) fn({ payload }); flush(); }
await build({ stdin: { contents: 'export * from "./src/assistant-tools"; export { createVoiceMode } from "./src/assistant-voice"; export { heardText } from "./src/assistant-turns"; export { confirmAction } from "./src/confirm-dialog"; export { default as webSearchTool } from "./src/tools/web-search"; export { createSandboxRow } from "./src/tools/python-runtime";', resolveDir: root }, bundle: true, format: 'esm', platform: 'node', outfile: bundle, logLevel: 'silent', define: { 'import.meta.env.DEV': 'false' }, plugins: [{ name: 'mock-tauri', setup(build) {
  build.onResolve({ filter: /^@tauri-apps\/api\// }, args => ({ path: args.path, namespace: 'tauri' }));
  build.onLoad({ filter: /.*/, namespace: 'tauri' }, () => ({ contents: 'export const invoke = (...args) => globalThis.__invoke(...args); export const listen = (...args) => globalThis.__listen(...args); export const confirm = (...args) => globalThis.__confirm(...args);' }));
} }] });
const T = await import(pathToFileURL(bundle).href);
let passed = 0;
async function check(name, fn) { try { await fn(); passed++; } catch (error) { console.error(`FAIL ${name}`); throw error; } }

await check('tagged budgets preserve auto/unlimited and reject invalid custom tokens', () => {
  assert.deepEqual(T.parseThinkingBudget(null), { mode: 'auto' });
  assert.deepEqual(T.parseThinkingBudget('unlimited'), { mode: 'unlimited' });
  assert.deepEqual(T.parseThinkingBudget('1024'), { mode: 'tokens', tokens: 1024 });
  for (const value of ['0', '-1', '2.5', 'Infinity', '9007199254740992', '1048577']) assert.deepEqual(T.parseThinkingBudget(value), { mode: 'auto' });
  assert.deepEqual(T.parseThinkingBudget('1048576'), { mode: 'tokens', tokens: 1048576 });
});
await check('a custom thinking limit above the backend maximum is shown as invalid and not saved', () => {
  storage.clear();
  const limit = T.createThinkingLimit('chat');
  const select = limit.element.querySelectorAll('select')[0];
  const custom = limit.element.querySelectorAll('input')[0];
  select.value = 'custom'; select.trigger('change');
  custom.value = '4096'; custom.trigger('input');
  assert.deepEqual(limit.value(), { mode: 'tokens', tokens: 4096 });
  custom.value = '5000000'; custom.trigger('input');
  assert.deepEqual(limit.value(), { mode: 'tokens', tokens: 4096 }, 'the previous valid limit is kept');
  assert.equal(storage.get('voicereader.assistant.thinkingBudget.chat'), '4096');
  const error = limit.element.querySelectorAll('.assistant-error')[0];
  assert.ok(error && !error.classList.contains('is-hidden') && error.textContent.includes('1,048,576'));
});
await check('indexed fragments append and terminal full arguments replace without duplication', () => {
  const transcript = new T.ToolTranscript();
  transcript.call({ round: 0, call_index: 0, delta: '{"que', phase: 'streaming' });
  transcript.call({ round: 0, call_index: 0, call_id: 'a', name: 'web_search', delta: 'ry":"news"}', phase: 'queued' });
  transcript.call({ round: 0, call_index: 0, arguments: '{"query":"news"}', delta: '', phase: 'running' });
  transcript.result({ round: 0, call_id: 'a', name: 'web_search', status: 'ok', summary: 'Found results', sources: [{ n: 2, title: 'second', url: 'https://example.com/2' }] });
  transcript.addSources([{ n: 1, title: 'first', url: 'https://example.com/1' }, { n: 2, title: 'updated', url: 'https://example.com/2' }]);
  assert.equal(transcript.entries.length, 1); assert.equal(transcript.entries[0].args, '{"query":"news"}');
  assert.deepEqual(transcript.sources.map(s => s.n), [1, 2]);
});
await check('an answer that ends early leaves no tool chip running', () => {
  const transcript = new T.ToolTranscript();
  transcript.call({ round: 0, call_index: 0, call_id: 'a', name: 'web_search', delta: '{}', phase: 'running' });
  transcript.call({ round: 0, call_index: 1, name: 'get_weather', delta: '{"ci', phase: 'streaming' });
  transcript.result({ round: 0, call_id: 'a', name: 'web_search', status: 'ok', summary: 'Found it', sources: [] });
  transcript.settlePending();
  assert.deepEqual(transcript.entries.map(e => [e.phase, e.status]), [['done', 'ok'], ['done', 'cancelled']]);
});
await check('untrusted code, fragments and sources remain text with no embedded resources', () => {
  const transcript = new T.ToolTranscript();
  transcript.result({ round: 0, call_id: 'x', name: 'run_python', status: 'ok', summary: '<script>alert(1)</script>', sources: [{ n: 1, title: '<img src=x>', url: 'javascript:alert(1)' }], output: '<img src="https://tracker.test/">' });
  const into = document.createElement('div'); T.renderToolTranscript(transcript, into);
  assert.ok(into.textContent.includes('<img src="https://tracker.test/">'));
  assert.equal(into.querySelectorAll('img').length, 0); assert.equal(into.querySelectorAll('script').length, 0); assert.equal(into.querySelectorAll('a').length, 0);
  for (const value of ['javascript:alert(1)', 'data:text/html,x', 'file:///tmp/x', 'https://user:pass@example.com']) assert.equal(T.sourceHref(value), null);
  assert.equal(T.sourceHref('https://example.com/a'), 'https://example.com/a');
});
await check('only terminal answer enters history, including explicitly empty final text', () => {
  const rounds = new Map([[0, 'Searching now.'], [1, 'The answer.']]);
  assert.equal(T.finalAnswer(rounds, { final_round: 1 }), 'The answer.');
  assert.equal(T.finalAnswer(rounds, { final_text: '' }), '');
  assert.equal(T.historyWithNote(T.finalAnswer(rounds), '(searched: news)'), 'The answer.\n\n(searched: news)');
});
await check('fillers and intermediate rounds never count as heard final answer', () => {
  assert.equal(T.heardText([{ seq: 0, text: 'Thinking.', kind: 'filler', round: 0 }, { seq: 1, text: 'Searching.', kind: 'answer', round: 0 }, { seq: 2, text: 'Final.', kind: 'answer', round: 1 }], 2, 1), 'Final.');
});
function findSwitch(menu, label) {
  const control = menu.element.querySelectorAll('input').find(node => node.getAttribute('aria-label') === label);
  assert.ok(control, `Switch ${label} exists`); return control;
}
async function changeSwitch(menu, label, value) { const control = findSwitch(menu, label); control.checked = value; control.trigger('change'); await tick(); }
await check('legacy mode booleans migrate, explicit selection stays granular and unknown names are dropped', () => {
  storage.clear(); storage.set('voicereader.assistant.tools.consent', 'true'); storage.set('voicereader.assistant.tools.chat', 'true');
  assert.deepEqual(T.savedToolSelection('chat'), ['web_search', 'fetch_page', 'get_weather', 'run_python']);
  storage.set('voicereader.assistant.tools.selection.chat', '["get_weather","unknown","get_weather"]');
  assert.deepEqual(T.savedToolSelection('chat'), ['get_weather']);
  storage.set('voicereader.assistant.tools.selection.chat', '[]'); assert.deepEqual(T.savedToolSelection('chat'), []);
  storage.clear();
});
await check('opening Tools only opens the menu; first enabling needs consent; weather does not download Python', async () => {
  calls.length = 0; confirms.length = 0;
  const chat = T.createToolsToggle('chat', () => ({ tools: false }));
  const voice = T.createToolsToggle('voice', () => ({ tools: false }));
  assert.equal(chat.enabled(), false); assert.equal(voice.enabled(), false);
  assert.ok(chat.button.textContent.includes('ToolsOff'));
  chat.button.trigger('click'); await tick(); assert.equal(chat.enabled(), false);
  assert.equal(chat.button.getAttribute('aria-expanded'), 'true');
  await changeSwitch(chat, 'Weather for Chat', true);
  assert.deepEqual(confirms.slice(-2), [T.TOOLS_CONSENT, T.SMALL_MODEL_WARNING]);
  assert.deepEqual(chat.selection(), ['get_weather']); assert.equal(voice.enabled(), false);
  assert.equal(calls.some(c => c.name === 'assistant_tools_install'), false);
  assert.equal(findSwitch(chat, 'All Chat tools').indeterminate, true);
  await changeSwitch(chat, 'All Chat tools', true);
  assert.equal(chat.selection().length, 4); assert.equal(findSwitch(chat, 'All Chat tools').checked, true);
  await changeSwitch(chat, 'All Chat tools', false); assert.deepEqual(chat.selection(), []);
});
await check('declined consent and failed Python install preserve the prior selection', async () => {
  storage.clear(); confirms.length = 0; calls.length = 0; installed = false; confirmation = false;
  const toggle = T.createToolsToggle('chat', () => ({ tools: true }));
  await changeSwitch(toggle, 'Weather for Chat', true);
  assert.equal(toggle.enabled(), false); assert.equal(calls.some(c => c.name === 'assistant_tools_install'), false);
  confirmation = true; await changeSwitch(toggle, 'Weather for Chat', true);
  installFails = true; await changeSwitch(toggle, 'Python for Chat', true);
  assert.deepEqual(toggle.selection(), ['get_weather']); assert.ok(toggle.message.textContent.includes('Download failed'));
  assert.equal(findSwitch(toggle, 'Python for Chat').checked, false);
  installFails = false; installed = true;
});
await check('confirmations use the native dialog, never window.confirm, and a failed dialog means Cancel', async () => {
  assert.equal(windowConfirms.length, 0, 'window.confirm returns a truthy Promise in Tauri v1 and must not be used');
  dialogFails = true; assert.equal(await T.confirmAction('Delete?'), false); dialogFails = false;
  confirmation = false; assert.equal(await T.confirmAction('Delete?'), false);
  confirmation = true; assert.equal(await T.confirmAction('Delete?'), true);
  const sources = await Promise.all(['src/main.ts', 'src/assistant-tools.ts', 'src/tools/consent.ts', 'src/tools/python-runtime.ts', 'src/tools/web-search.ts']
    .map(file => fs.readFile(path.join(root, file), 'utf8')));
  for (const source of sources) assert.equal(/window\.confirm\(/.test(source), false);
});
await check('switching to DuckDuckGo asks before deleting the saved Brave key', async () => {
  const refreshers = []; const refresh = value => refreshers.forEach(fn => fn(value)); const context = { register() {}, onRefresh(fn) { refreshers.push(fn); }, notice() {}, run: async fn => fn(), limits: () => ({ timeouts: {} }), saveLimits: async () => {} };
  const settings = T.webSearchTool.createSettings(context);
  refresh({ provider: 'brave', brave_key_set: true, limits: {} });
  const provider = settings.querySelectorAll('select')[0];
  const removals = () => calls.filter(c => c.name === 'assistant_set_search_key' && c.args.key === null).length;
  const before = removals();
  confirmation = false; provider.value = 'duckduckgo'; provider.trigger('change'); await tick();
  assert.equal(removals(), before, 'declining keeps the key'); assert.equal(provider.value, 'brave');
  confirmation = true; provider.value = 'duckduckgo'; provider.trigger('change'); await tick();
  assert.equal(removals(), before + 1);
});await check('a Python runtime download can be cancelled from its row', async () => {
  storage.set('voicereader.assistant.tools.consent', 'true');
  installed = false;
  let release; const pending = new Promise(resolve => { release = resolve; });
  const previous = globalThis.__invoke;
  globalThis.__invoke = async (name, args) => { if (name === 'assistant_tools_install') { calls.push({ name, args }); await pending; throw new Error('The Python code runner download was cancelled.'); } return previous(name, args); };
  const row = T.createSandboxRow(); await row.refresh();
  const download = row.element.querySelectorAll('button').find(b => b.textContent === 'Download');
  download.trigger('click'); await tick();
  const cancel = row.element.querySelectorAll('button').find(b => b.textContent === 'Cancel download');
  assert.ok(cancel && !cancel.disabled, 'a Cancel button replaces Download while it runs');
  cancel.trigger('click'); await tick();
  assert.ok(calls.some(c => c.name === 'assistant_tools_cancel_install'));
  release(); await tick(); await tick();
  assert.ok(row.element.querySelectorAll('button').some(b => b.textContent === 'Download'));
  globalThis.__invoke = previous; installed = true;
});
let subscriber;
const mic = { state: 'closed', async open() { this.state = 'open'; }, async close() { this.state = 'closed'; }, async setAuto() {}, subscribe(s) { subscriber = s; }, async cancel() {}, async begin() { return 7; }, async end() {} };
storage.set('voicereader.assistant.autosend', 'true');
const voice = await T.createVoiceMode({ mic, log() {}, unavailableReason: () => '', sampling: () => null, model: () => ({ tools: true }), onNewChat() {} });
voice.setEntered(true); await tick();
async function ask(id, text) { subscriber.turn(id, 'started'); subscriber.final(id, text); await tick(); return calls.filter(c => c.name === 'assistant_voice_respond').at(-1).args; }
let request = await ask(1, 'What is new?');
await check('voice requests include independent tools, think, tagged budget, conversation and limits', () => {
  assert.equal(request.tools, false); assert.equal(request.think, false); assert.deepEqual(request.thinkingBudget, { mode: 'auto' });
  assert.deepEqual(request.enabledTools, []);
  assert.equal(request.toolLimits.max_rounds, 5); assert.ok(request.conversationId);
});
await check('voice mock events preserve preamble/filler/tools but history keeps final round plus note', async () => {
  const id = request.requestId;
  emit('assistant:chat-delta', { request_id: id, round: 0, reasoning: 'REASONING_ONLY_<script>alert(1)</script>' });
  const thinking = voice.view.querySelectorAll('.assistant-vt-thinking')[0];
  assert.ok(thinking); assert.equal(thinking.open, false);
  assert.ok(thinking.textContent.includes('REASONING_ONLY_'));
  thinking.open = true;
  emit('assistant:chat-delta', { request_id: id, round: 0, reasoning: ' second fragment' });
  assert.equal(thinking.open, true);
  emit('assistant:chat-delta', { request_id: 'stale', reasoning: 'STALE_REASONING' });
  assert.equal(voice.view.textContent.includes('STALE_REASONING'), false);
  emit('assistant:chat-delta', { request_id: id, round: 0, content: 'I will search.' });
  emit('assistant:voice-phrase', { request_id: id, job_id: 'job1', seq: 0, text: 'I will search.', kind: 'answer', round: 0 });
  emit('assistant:voice-phrase', { request_id: id, job_id: 'job1', seq: 1, text: 'Still working.', kind: 'filler', round: 0 });
  emit('assistant:tool-call', { request_id: id, round: 0, call_index: 0, call_id: 'a', name: 'web_search', delta: '{"query":"news"}', phase: 'running' });
  emit('assistant:tool-result', { request_id: id, round: 0, call_id: 'a', name: 'web_search', status: 'ok', summary: 'Found it', sources: [] });
  emit('assistant:chat-delta', { request_id: 'stale', round: 1, content: 'STALE' });
  emit('assistant:chat-delta', { request_id: id, round: 1, content: 'Today’s news.' });
  emit('assistant:voice-phrase', { request_id: id, job_id: 'job1', seq: 2, text: 'Today’s news.', kind: 'answer', round: 1 });
  emit('assistant:playback', { job_id: 'job1', buffered_secs: 0, played_seq: 2, drained: true }); // metadata has not arrived: stay active
  assert.ok(voice.view.textContent.includes('Still working.'));
  assert.ok(voice.view.textContent.includes('I will search.'));
  emit('assistant:chat-done', { request_id: id, final_round: 1, final_text: 'Today’s news.', history_note: '(searched: news)', sources: [{ n: 1, title: 'News', url: 'https://example.com/news' }] });
  emit('assistant:voice-ended', { request_id: id, job_id: 'job1', reason: 'done' });
  response = { job_id: 'job2', dropped_messages: 0 };
  request = await ask(2, 'Tell me more');
  assert.equal(request.messages[1].text, 'Today’s news.\n\n(searched: news)');
  assert.equal(request.messages.some(m => m.text.includes('STALE') || m.text.includes('REASONING_ONLY_') || m.text.includes('Still working.')), false);
});
await check('interrupting a voice answer during a tool settles its chip', async () => {
  response = { job_id: 'job-int', dropped_messages: 0 };
  const r = await ask(9, 'Search something');
  emit('assistant:tool-call', { request_id: r.requestId, round: 0, call_index: 0, call_id: 'q', name: 'web_search', delta: '{"query":"x"}', phase: 'running' });
  await tick();
  assert.ok(voice.view.textContent.includes('web_search · running'), 'chip shows running first');
  const talk = voice.view.querySelectorAll('.assistant-talk-btn')[0];
  assert.ok(talk, 'talk button exists');
  talk.trigger('pointerdown', { button: 0, pointerId: 1 }); talk.trigger('pointerup', { button: 0, pointerId: 1 }); await tick();
  assert.ok(calls.some(c => c.name === 'assistant_voice_cancel' && c.args.requestId === r.requestId), 'the answer was interrupted');
  // No late result has arrived: the interruption itself must settle the chip.
  assert.equal(voice.view.textContent.includes('web_search · running'), false, 'no chip is left running');
  assert.ok(voice.view.textContent.includes('web_search · cancelled'));
});
await check('New chat clears the exact conversation allowlist and rejects old events', async () => {
  const previous = request;
  voice.newConversation(); await tick();
  const clear = calls.filter(c => c.name === 'assistant_clear_tool_conversation').at(-1);
  assert.equal(clear.args.conversationId, previous.conversationId);
  emit('assistant:chat-delta', { request_id: previous.requestId, round: 0, content: 'STALE NEW CHAT' });
  emit('assistant:chat-done', { request_id: previous.requestId, final_text: 'STALE NEW CHAT', history_note: 'stale' });
  assert.equal(voice.view.textContent.includes('STALE NEW CHAT'), false);
  response = { job_id: 'job3', dropped_messages: 0 };
  const next = await ask(3, 'Hello again');
  assert.notEqual(next.conversationId, previous.conversationId); assert.equal(next.messages.length, 1);
});
await check('Voice tools menu forwards the selected subset independently from Chat', async () => {
  voice.newConversation(); await tick();
  const weather = voice.view.querySelectorAll('input').find(node => node.getAttribute('aria-label') === 'Weather for Voice');
  assert.ok(weather); weather.checked = true; weather.trigger('change'); await tick();
  response = { job_id: 'job4', dropped_messages: 0 };
  const selected = await ask(4, 'Weather in Singapore?');
  assert.equal(selected.tools, true); assert.deepEqual(selected.enabledTools, ['get_weather']);
  assert.equal(selected.messages.some(m => m.text.includes('REASONING_ONLY_')), false);
});
await fs.unlink(bundle);
console.log(`assistant tools: ${passed} checks passed`);
