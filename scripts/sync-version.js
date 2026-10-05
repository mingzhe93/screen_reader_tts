#!/usr/bin/env node

import fs from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

// Read version from package.json
const packageJsonPath = path.join(__dirname, '..', 'package.json');
const packageJson = JSON.parse(fs.readFileSync(packageJsonPath, 'utf8'));
const version = packageJson.version;

// Update Cargo.toml
const cargoTomlPath = path.join(__dirname, '..', 'src-tauri', 'Cargo.toml');
let cargoToml = fs.readFileSync(cargoTomlPath, 'utf8');
cargoToml = cargoToml.replace(
  /^version = "[^"]*"$/m,
  `version = "${version}"`
);
fs.writeFileSync(cargoTomlPath, cargoToml);

// Update tauri.conf.json
const tauriConfPath = path.join(__dirname, '..', 'src-tauri', 'tauri.conf.json');
const tauriConf = JSON.parse(fs.readFileSync(tauriConfPath, 'utf8'));
tauriConf.package.version = version;
fs.writeFileSync(tauriConfPath, JSON.stringify(tauriConf, null, 2) + '\n');

// Keep the root npm package and the app's Cargo lock entry aligned, without
// changing dependency versions.
const packageLockPath = path.join(__dirname, '..', 'package-lock.json');
const packageLock = JSON.parse(fs.readFileSync(packageLockPath, 'utf8'));
packageLock.version = version;
packageLock.packages[''].version = version;
fs.writeFileSync(packageLockPath, JSON.stringify(packageLock, null, 2) + '\n');

const cargoLockPath = path.join(__dirname, '..', 'src-tauri', 'Cargo.lock');
if (fs.existsSync(cargoLockPath)) {
  const cargoLock = fs.readFileSync(cargoLockPath, 'utf8').replace(
    /(\[\[package\]\]\r?\nname = "voicereader-desktop"\r?\nversion = ")[^"]*(")/,
    (_match, prefix, suffix) => `${prefix}${version}${suffix}`
  );
  fs.writeFileSync(cargoLockPath, cargoLock);
}

// The optional Full-build engine reports the same release version as the app.
for (const [relativePath, pattern, replacement] of [
  ['tts-engine/pyproject.toml', /^version = "[^"]*"$/m, `version = "${version}"`],
  ['tts-engine/src/tts_engine/__init__.py', /^__version__ = "[^"]*"$/m, `__version__ = "${version}"`],
  ['tts-engine/src/tts_engine/config.py', /^(\s*)engine_version: str = "[^"]*"$/m, `$1engine_version: str = "${version}"`],
]) {
  const filePath = path.join(__dirname, '..', relativePath);
  const contents = fs.readFileSync(filePath, 'utf8').replace(pattern, replacement);
  fs.writeFileSync(filePath, contents);
}

console.log(`✓ Synced version ${version} across desktop, lockfiles and Python engine metadata`);
