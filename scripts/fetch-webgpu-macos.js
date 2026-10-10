#!/usr/bin/env node

// Bundle Microsoft's native WebGPU/Metal plugin for Apple Silicon Base builds.
// The wheel is just an archive: the packaged Rust app does not need Python.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const version = '0.4.0';
const sha256 = '8d0a91d44b43d931c3068b9134aed9770f27803e353c2b58c93e1700bb68bcca';
const url = 'https://files.pythonhosted.org/packages/c3/9f/1e09865bbe4c9202ff1334545535be32dfdbeffc46e7d4c36c5de67ffafe/onnxruntime_ep_webgpu-0.4.0-py3-none-macosx_14_0_universal2.whl';
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const output = path.join(root, 'src-tauri/binaries/onnxruntime');
const library = 'libonnxruntime_providers_webgpu.dylib';

if (process.platform !== 'darwin' || process.arch !== 'arm64') {
  console.log('Native Metal plugin is only bundled on Apple Silicon macOS; skipping on this platform.');
  process.exit(0);
}
const coreVersion = fs.readFileSync(path.join(output, 'VERSION.txt'), 'utf8').trim();
if (coreVersion !== '1.24.4') {
  throw new Error(`The pinned WebGPU plugin requires core ONNX Runtime 1.24.4, found ${coreVersion}. Run npm run onnxruntime:fetch.`);
}
const marker = path.join(output, 'WEBGPU_VERSION.json');
if (fs.existsSync(marker) && fs.existsSync(path.join(output, library))) {
  const installed = JSON.parse(fs.readFileSync(marker, 'utf8'));
  const digest = createHash('sha256').update(fs.readFileSync(path.join(output, library))).digest('hex');
  if (installed.version === version && installed.archive_sha256 === sha256 && installed.library_sha256 === digest) {
    console.log(`✓ Native WebGPU ${version} already bundled`);
    process.exit(0);
  }
}

const work = fs.mkdtempSync(path.join(os.tmpdir(), 'voicereader-webgpu-'));
try {
  console.log(`Downloading native WebGPU ${version} for Metal acceleration`);
  const response = await fetch(url);
  if (!response.ok) throw new Error(`WebGPU download failed with HTTP ${response.status}`);
  const archive = Buffer.from(await response.arrayBuffer());
  if (createHash('sha256').update(archive).digest('hex') !== sha256) throw new Error('WebGPU archive checksum mismatch');
  const archivePath = path.join(work, 'webgpu.whl');
  fs.writeFileSync(archivePath, archive);
  execFileSync('tar', ['-xf', archivePath, '-C', work]);
  fs.copyFileSync(path.join(work, 'onnxruntime_ep_webgpu', library), path.join(output, library));
  const distInfo = fs.readdirSync(work).find(name => name.endsWith('.dist-info'));
  const licenses = path.join(work, distInfo, 'licenses');
  if (!fs.existsSync(licenses)) throw new Error('WebGPU package license directory is missing');
  fs.cpSync(licenses, path.join(output, 'webgpu-licenses'), { recursive: true });
  fs.copyFileSync(path.join(work, 'onnxruntime_ep_webgpu/README.md'), path.join(output, 'WEBGPU_README.md'));
  fs.writeFileSync(marker, JSON.stringify({
    version, source: url, archive_sha256: sha256,
    library_sha256: createHash('sha256').update(fs.readFileSync(path.join(output, library))).digest('hex'),
  }, null, 2) + '\n');
  console.log(`✓ Native WebGPU ${version} bundled (Metal, macOS 14+)`);
} finally {
  fs.rmSync(work, { recursive: true, force: true });
}
