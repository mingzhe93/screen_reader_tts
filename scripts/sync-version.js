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

// Replace version in [package] section (first occurrence)
cargoToml = cargoToml.replace(
  /^version = "[^"]*"$/m,
  `version = "${version}"`
);

fs.writeFileSync(cargoTomlPath, cargoToml);
console.log(`✓ Synced version ${version} to src-tauri/Cargo.toml`);
