#!/usr/bin/env node

// Downloads the ONNX Runtime shared library used by the Audio8 TTS backend into
// src-tauri/binaries/onnxruntime. ONNX Runtime is loaded at run time instead of being
// linked statically, because its bundled protobuf clashes with sentencepiece (used by
// the Kyutai Pocket TTS runtime) at link time.
//
// On Windows this is the DirectML build, plus DirectML.dll, so the audio decoder can
// run on any DirectX 12 GPU. It still contains the CPU provider, which everything
// else uses. Other platforms get the official release build (CPU, and Core ML on macOS).

import fs from 'fs';
import os from 'os';
import path from 'path';
import { execFileSync } from 'child_process';
import { fileURLToPath } from 'url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

// Keep within the range the `ort` crate is built for (api-22 => ONNX Runtime >= 1.22).
const ORT_VERSION = '1.24.4';
// Last release that still ships an Intel macOS build.
const ORT_VERSION_MACOS_X64 = '1.23.2';
const DIRECTML_VERSION = '1.15.4';

const nuget = (id, version) =>
  `https://api.nuget.org/v3-flatcontainer/${id}/${version}/${id}.${version}.nupkg`;
const release = (version, archive) =>
  `https://github.com/microsoft/onnxruntime/releases/download/v${version}/${archive}`;

const windowsDirectMl = (ortRuntime, dmlBin) => ({
  version: `${ORT_VERSION}+directml-${DIRECTML_VERSION}`,
  downloads: [
    {
      url: nuget('microsoft.ml.onnxruntime.directml', ORT_VERSION),
      archive: 'onnxruntime-directml.zip',
      member: `runtimes/${ortRuntime}/native/onnxruntime.dll`,
      output: 'onnxruntime.dll',
    },
    {
      url: nuget('microsoft.ai.directml', DIRECTML_VERSION),
      archive: 'directml.zip',
      member: `bin/${dmlBin}/DirectML.dll`,
      output: 'DirectML.dll',
    },
  ],
});

const releaseBuild = (version, name, libraryMember, output) => ({
  version,
  downloads: [
    {
      url: release(version, `${name}.tgz`),
      archive: `${name}.tgz`,
      member: `${name}/lib/${libraryMember}`,
      output,
    },
  ],
});

const platforms = {
  'win32-x64': windowsDirectMl('win-x64', 'x64-win'),
  'win32-arm64': windowsDirectMl('win-arm64', 'arm64-win'),
  'darwin-arm64': releaseBuild(
    ORT_VERSION,
    `onnxruntime-osx-arm64-${ORT_VERSION}`,
    `libonnxruntime.${ORT_VERSION}.dylib`,
    'libonnxruntime.dylib'
  ),
  'darwin-x64': releaseBuild(
    ORT_VERSION_MACOS_X64,
    `onnxruntime-osx-x86_64-${ORT_VERSION_MACOS_X64}`,
    `libonnxruntime.${ORT_VERSION_MACOS_X64}.dylib`,
    'libonnxruntime.dylib'
  ),
  'linux-x64': releaseBuild(
    ORT_VERSION,
    `onnxruntime-linux-x64-${ORT_VERSION}`,
    `libonnxruntime.so.${ORT_VERSION}`,
    'libonnxruntime.so'
  ),
};

const key = `${process.platform}-${process.arch}`;
const target = platforms[key];
if (!target) {
  console.error(`✗ No ONNX Runtime download is configured for ${key}`);
  process.exit(1);
}

const outputDir = path.join(__dirname, '..', 'src-tauri', 'binaries', 'onnxruntime');
const versionPath = path.join(outputDir, 'VERSION.txt');
const relativeOutputDir = path.relative(process.cwd(), outputDir);

const installedVersion = fs.existsSync(versionPath) ? fs.readFileSync(versionPath, 'utf8').trim() : '';
const allPresent = target.downloads.every((item) => fs.existsSync(path.join(outputDir, item.output)));
if (allPresent && installedVersion === target.version) {
  console.log(`✓ ONNX Runtime ${target.version} already present in ${relativeOutputDir}`);
  process.exit(0);
}

// bsdtar (shipped with Windows 10+, macOS and Linux) reads .zip, .nupkg and .tgz.
// On Windows, name the system tar explicitly: a GNU tar earlier on PATH (Git Bash)
// cannot read zip archives or drive-letter paths.
const systemTar = path.join(process.env.SystemRoot ?? 'C:\\Windows', 'System32', 'tar.exe');
const tar = process.platform === 'win32' && fs.existsSync(systemTar) ? systemTar : 'tar';

const workDir = fs.mkdtempSync(path.join(os.tmpdir(), 'voicereader-onnxruntime-'));
try {
  fs.mkdirSync(outputDir, { recursive: true });
  for (const item of target.downloads) {
    console.log(`Downloading ${item.url}`);
    const response = await fetch(item.url);
    if (!response.ok) {
      throw new Error(`download of ${item.output} failed with HTTP ${response.status}`);
    }
    const archivePath = path.join(workDir, item.archive);
    fs.writeFileSync(archivePath, Buffer.from(await response.arrayBuffer()));
    const extractDir = path.join(workDir, `${item.output}-extracted`);
    fs.mkdirSync(extractDir);
    execFileSync(tar, ['-xf', archivePath, '-C', extractDir, item.member], { stdio: 'inherit' });
    fs.copyFileSync(path.join(extractDir, ...item.member.split('/')), path.join(outputDir, item.output));
  }
  fs.writeFileSync(versionPath, `${target.version}\n`);
  console.log(`✓ ONNX Runtime ${target.version} installed to ${relativeOutputDir}`);
} catch (error) {
  console.error(`✗ Failed to fetch ONNX Runtime: ${error.message}`);
  process.exit(1);
} finally {
  fs.rmSync(workDir, { recursive: true, force: true });
}
