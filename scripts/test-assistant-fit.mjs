#!/usr/bin/env node

// Checks the Assistant memory-fit rule (memoryFit in src/assistant-format.ts, plan §5.3):
// unified memory and CPU-only machines are judged against 50% / 75% of system RAM, discrete
// GPUs against their memory minus 1 GB with host RAM (plus any spill) within 50%.
//
// Run with: npm run test:assistant-fit

import assert from 'node:assert/strict';
import os from 'os';
import path from 'path';
import { fileURLToPath, pathToFileURL } from 'url';
import { build } from 'esbuild';

const here = path.dirname(fileURLToPath(import.meta.url));
const bundle = path.join(os.tmpdir(), `voicereader-assistant-fit-${process.pid}.mjs`);
await build({
  entryPoints: [path.join(here, '..', 'src', 'assistant-format.ts')],
  bundle: true,
  format: 'esm',
  platform: 'node',
  outfile: bundle,
  logLevel: 'silent',
});
const { memoryAt, memoryFit } = await import(pathToFileURL(bundle).href);

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

const GIB = 1024;
/** GPU and host pools; per 1K tokens of context. */
const memory = (gpuFixed, gpuPer1k, hostFixed = 0, hostPer1k = 0) => ({
  gpu_fixed_mib: gpuFixed,
  gpu_mib_per_1k_ctx: gpuPer1k,
  host_fixed_mib: hostFixed,
  host_mib_per_1k_ctx: hostPer1k,
  measured: true,
});
const mac = (ramGib) => ({
  total_ram_mib: ramGib * GIB,
  unified_memory: true,
  devices: [{ name: 'Apple M2', total_mib: ramGib * GIB * 0.75, free_mib: 0, integrated: true }],
  devices_known: true,
  recommended_tier: 'mid',
  reason: '',
});
const pc = (ramGib, devices, known = true) => ({
  total_ram_mib: ramGib * GIB,
  unified_memory: false,
  devices,
  devices_known: known,
  recommended_tier: 'mid',
  reason: '',
});
const gpu = (name, gib, integrated = false) => ({ name, total_mib: gib * GIB, free_mib: gib * GIB, integrated });

check('memoryAt rounds the context up to whole 1K blocks', () => {
  const m = memory(1000, 10, 200, 2);
  assert.deepEqual(memoryAt(m, 1024), { gpu: 1010, host: 202 });
  assert.deepEqual(memoryAt(m, 1025), { gpu: 1020, host: 204 });
  assert.deepEqual(memoryAt(m, 32768), { gpu: 1320, host: 264 });
});

check('unified memory: fits up to 50%, tight up to 75%, over beyond', () => {
  const hw = mac(16); // 16384 MiB: 50% = 8192, 75% = 12288
  assert.equal(memoryFit(memory(8192, 0), 8192, hw).level, 'fits');
  assert.equal(memoryFit(memory(8193, 0), 8192, hw).level, 'tight');
  assert.equal(memoryFit(memory(6000, 0, 6288, 0), 8192, hw).level, 'tight');
  assert.equal(memoryFit(memory(12289, 0), 8192, hw).level, 'over');
  const over = memoryFit(memory(12289, 0), 8192, hw);
  assert.equal(over.icon, '⛔');
  assert.equal(over.label, 'Likely to run out of memory');
  assert.equal(memoryFit(memory(8193, 0), 8192, hw).label, 'Tight, may slow the computer');
});

check('unified memory: the context length moves the verdict', () => {
  const hw = mac(16);
  const m = memory(4000, 100, 2000, 0); // 6000 + 100 per 1K
  assert.equal(memoryFit(m, 8192, hw).level, 'fits'); // 6800
  assert.equal(memoryFit(m, 32768, hw).level, 'tight'); // 9200
  assert.equal(memoryFit(m, 131072, hw).level, 'over'); // 18800
});

check('no discrete GPU (integrated only, or none) uses the RAM rule', () => {
  const integrated = pc(16, [gpu('Intel Iris Xe', 8, true)]);
  assert.equal(memoryFit(memory(7000, 0, 1000, 0), 8192, integrated).level, 'fits');
  assert.equal(memoryFit(memory(9000, 0, 1000, 0), 8192, integrated).level, 'tight');
  const none = pc(16, []);
  assert.equal(memoryFit(memory(13000, 0), 8192, none).level, 'over');
});

check('discrete GPU: fits within its memory minus 1 GB', () => {
  const hw = pc(32, [gpu('NVIDIA RTX 4060', 8)]); // budget 7168
  const fit = memoryFit(memory(7168, 0, 2000, 0), 8192, hw);
  assert.equal(fit.level, 'fits');
  assert.equal(fit.label, 'Fits in GPU memory');
  assert.match(fit.detail, /RTX 4060/);
});

check('discrete GPU: a GPU pool over the budget spills to the CPU (tight)', () => {
  const hw = pc(32, [gpu('NVIDIA RTX 4060', 8)]);
  const fit = memoryFit(memory(7169, 0, 2000, 0), 8192, hw);
  assert.equal(fit.level, 'tight');
  assert.equal(fit.icon, '⚠️');
  assert.equal(fit.label, "Won't fit in GPU memory; parts run on the CPU and it will be slower");
});

check('discrete GPU: host RAM over 50% is over, with or without spill', () => {
  const hw = pc(16, [gpu('NVIDIA RTX 4090', 24)]); // 50% RAM = 8192
  assert.equal(memoryFit(memory(4000, 0, 8193, 0), 8192, hw).level, 'over');
  assert.equal(memoryFit(memory(4000, 0, 8192, 0), 8192, hw).level, 'fits');
  // 6 GB card: budget 5120; 9000 GPU spills 3880, host 5000 + 3880 = 8880 > 8192.
  const small = pc(16, [gpu('Radeon RX 6500', 6)]);
  assert.equal(memoryFit(memory(9000, 0, 5000, 0), 8192, small).level, 'over');
  // The same spill on 32 GB RAM stays tight.
  assert.equal(memoryFit(memory(9000, 0, 5000, 0), 8192, pc(32, [gpu('Radeon RX 6500', 6)])).level, 'tight');
});

check('discrete GPU: the largest non-integrated device is used', () => {
  const hw = pc(64, [gpu('Intel UHD', 32, true), gpu('GTX 1650', 4), gpu('RTX 3090', 24)]);
  const fit = memoryFit(memory(20000, 0), 8192, hw);
  assert.equal(fit.level, 'fits');
  assert.match(fit.detail, /RTX 3090/);
});

check('devices not known: judged by RAM only, with a note', () => {
  const hw = pc(32, [gpu('RTX 3090', 24)], false);
  const fit = memoryFit(memory(20000, 0), 8192, hw); // 20000 > 16384 (50%), <= 24576 (75%)
  assert.equal(fit.level, 'tight');
  assert.match(fit.detail, /GPU memory unknown until the backend is installed/);
  assert.doesNotMatch(memoryFit(memory(20000, 0), 8192, mac(32)).detail, /unknown/);
});

check('unknown system memory never claims a fit', () => {
  const hw = { ...mac(16), total_ram_mib: 0 };
  assert.notEqual(memoryFit(memory(1, 0), 8192, hw).level, 'fits');
});

console.log(`assistant fit tests passed (${passed} checks)`);
