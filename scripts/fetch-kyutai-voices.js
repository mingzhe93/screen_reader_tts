#!/usr/bin/env node

// Downloads the reference clips for the extra Pocket TTS preset voices into
// src-tauri/binaries/kyutai-voices. Upstream (kyutai/pocket-tts) ships these voices as
// precomputed states for newer, gated model versions; the bundled January 2026 weights
// can use the same voices by cloning them from their public source clips instead.

import fs from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

const SOURCE_REPO = 'https://huggingface.co/kyutai/tts-voices/resolve/main';

// name -> source clip, as listed in the kyutai-labs/pocket-tts README.
const voices = {
  anna: 'vctk/p228_023_enhanced.wav',
  vera: 'vctk/p229_023_enhanced.wav',
  charles: 'vctk/p254_023_enhanced.wav',
  paul: 'vctk/p259_023_enhanced.wav',
  george: 'vctk/p315_023_enhanced.wav',
  mary: 'vctk/p333_023_enhanced.wav',
  jane: 'vctk/p339_023_enhanced.wav',
  michael: 'vctk/p360_023_enhanced.wav',
  eve: 'vctk/p361_023_enhanced.wav',
  bill_boerst: 'voice-zero/bill_boerst.wav',
  caro_davy: 'voice-zero/caro_davy.wav',
  peter_yearsley: 'voice-zero/peter_yearsley.wav',
  stuart_bell: 'voice-zero/stuart_bell.wav',
};

const attribution = `Pocket TTS preset voice reference clips
Source: https://huggingface.co/kyutai/tts-voices

vctk/*        CSTR VCTK Corpus, University of Edinburgh. Licence: CC BY 4.0.
              Clips are the "enhanced" versions published by Kyutai.
voice-zero/*  LibriVox recordings selected by Kyutai. Licence: CC0.

${Object.entries(voices)
  .map(([name, source]) => `${name}.wav <- ${source}`)
  .join('\n')}
`;

const outputDir = path.join(__dirname, '..', 'src-tauri', 'binaries', 'kyutai-voices');
fs.mkdirSync(outputDir, { recursive: true });

let downloaded = 0;
for (const [name, source] of Object.entries(voices)) {
  const outputPath = path.join(outputDir, `${name}.wav`);
  if (fs.existsSync(outputPath) && fs.statSync(outputPath).size > 0) {
    continue;
  }
  const url = `${SOURCE_REPO}/${source}`;
  try {
    const response = await fetch(url);
    if (!response.ok) {
      throw new Error(`HTTP ${response.status}`);
    }
    // Write to a temporary name first so an interrupted download is not mistaken for a clip.
    const partPath = `${outputPath}.part`;
    fs.writeFileSync(partPath, Buffer.from(await response.arrayBuffer()));
    fs.renameSync(partPath, outputPath);
    downloaded += 1;
  } catch (error) {
    console.error(`✗ Failed to fetch Kyutai voice "${name}" from ${url}: ${error.message}`);
    process.exit(1);
  }
}
fs.writeFileSync(path.join(outputDir, 'ATTRIBUTION.txt'), attribution);

const total = Object.keys(voices).length;
console.log(
  downloaded > 0
    ? `✓ Downloaded ${downloaded} Kyutai voice clip(s); ${total} present in ${path.relative(process.cwd(), outputDir)}`
    : `✓ Kyutai voice clips already present (${total}) in ${path.relative(process.cwd(), outputDir)}`
);
