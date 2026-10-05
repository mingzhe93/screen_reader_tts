#!/usr/bin/env python3
"""Build SoX with static libsox and only OS dylibs; keep Windows assets intact."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile

VERSION = "14.4.2"
SHA256 = "b45f598643ffbd8e363ff24d61166ccec4836fea6d3888881b8df53e3bb55f6c"
URL = f"https://downloads.sourceforge.net/project/sox/sox/{VERSION}/sox-{VERSION}.tar.gz"
MIN_MACOS = "14.0"
WITHOUT = (
    "libltdl magic png ladspa mad id3tag lame twolame oggvorbis opus flac amrwb "
    "amrnb wavpack sndio coreaudio alsa ao pulseaudio waveaudio sndfile oss "
    "sunaudio mp3 gsm lpc10"
).split()


def verify(executable: Path) -> None:
    arch = {"aarch64": "arm64", "x86_64": "x86_64", "arm64": "arm64"}[platform.machine()]
    actual = subprocess.check_output(["lipo", "-archs", str(executable)], text=True).strip()
    if actual != arch:
        raise RuntimeError(f"SoX architecture {actual} does not match host {arch}.")
    linked = subprocess.check_output(["otool", "-L", str(executable)], text=True)
    for line in linked.splitlines()[1:]:
        dependency = line.strip().split(" (", 1)[0]
        if not dependency.startswith(("/usr/lib/", "/System/Library/")):
            raise RuntimeError(f"SoX has a nonportable dependency: {dependency}")
    subprocess.run([str(executable), "--version"], check=True)


def main() -> int:
    if sys.platform != "darwin":
        raise RuntimeError("Build the macOS SoX runtime on a Mac.")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-archive", type=Path, help="Use the bundled source archive to rebuild offline.")
    args = parser.parse_args()
    recipe = Path(__file__).resolve()
    root = recipe.parents[1]
    target = root / "src-tauri/binaries/sox-macos"
    stamp = {
        "version": VERSION,
        "source_sha256": SHA256,
        "recipe_sha256": hashlib.sha256(recipe.read_bytes()).hexdigest(),
        "arch": platform.machine(),
        "minimum_macos": MIN_MACOS,
    }
    marker = target / "BUILD.json"
    if marker.exists() and json.loads(marker.read_text()) == stamp and (target / "sox").is_file():
        verify(target / "sox")
        print(f"Bundled macOS SoX already present: {target}")
        return 0

    cache = root / "build/sox-macos"
    cache.mkdir(parents=True, exist_ok=True)
    archive = args.source_archive or cache / f"sox-{VERSION}.tar.gz"
    if not archive.exists():
        subprocess.run(["curl", "--proto", "=https", "--fail", "--location", URL, "--output", str(archive)], check=True)
    if hashlib.sha256(archive.read_bytes()).hexdigest() != SHA256:
        raise RuntimeError(f"SoX source checksum mismatch: {archive}")

    with tempfile.TemporaryDirectory(prefix="voicereader-sox-") as temp:
        work = Path(temp)
        subprocess.run(["tar", "-xf", str(archive.resolve()), "-C", str(work)], check=True)
        source = work / f"sox-{VERSION}"
        # Apple's stdint types differ from SoX's long-based 64-bit typedefs.
        # The old pure annotation also incorrectly covers functions that initialize
        # global buffers (including the version string), which modern Clang optimizes away.
        header = source / "src/sox.h"
        patched = header.read_text().replace("#include <stddef.h>", "#include <stddef.h>\n#include <stdint.h>")
        patched = patched.replace("typedef long sox_int64_t;", "typedef int64_t sox_int64_t;")
        patched = patched.replace("typedef unsigned long sox_uint64_t;", "typedef uint64_t sox_uint64_t;")
        patched = patched.replace("#define LSX_RETURN_PURE __attribute__ ((pure))", "#define LSX_RETURN_PURE")
        header.write_text(patched)

        env = os.environ.copy()
        env.update(CC="clang", CFLAGS=f"-O2 -mmacosx-version-min={MIN_MACOS}",
                   LDFLAGS=f"-mmacosx-version-min={MIN_MACOS}", CPPFLAGS="", LIBS="", PKG_CONFIG="false")
        flags = ["--disable-shared", "--enable-static", "--disable-openmp", "--disable-symlinks"]
        flags.extend(f"--without-{library}" for library in WITHOUT)
        subprocess.run(["./configure", *flags], cwd=source, env=env, check=True)
        subprocess.run(["make", f"-j{min(os.cpu_count() or 2, 8)}"], cwd=source, env=env, check=True)
        executable = source / "src/sox"
        subprocess.run(["strip", str(executable)], check=True)
        subprocess.run(["codesign", "--force", "--sign", "-", str(executable)], check=True)
        verify(executable)

        staged = work / "bundle"
        staged.mkdir()
        shutil.copy2(executable, staged / "sox")
        for name in ("COPYING", "LICENSE.GPL", "LICENSE.LGPL", "AUTHORS"):
            shutil.copy2(source / name, staged / name)
        # Ship corresponding source and the exact recipe, including the Mac patches.
        corresponding = staged / "source"
        corresponding.mkdir()
        shutil.copy2(archive, corresponding / archive.name)
        shutil.copy2(recipe, corresponding / recipe.name)
        shutil.copy2(header, corresponding / "sox.h")
        (staged / "README.txt").write_text(
            f"SoX {VERSION}, built for {platform.machine()}, macOS {MIN_MACOS}+.\n"
            "Static libsox; no Homebrew or third-party dylibs required.\n"
            "Raw PCM, WAV and tempo effects enabled; external codecs and audio devices disabled.\n"
            "See COPYING and LICENSE.*. Corresponding source and build recipe are in source/.\n"
            "To rebuild: place the recipe in this repo's scripts/ directory and run it with\n"
            "--source-archive pointing to the included tar.gz. The recipe applies the Mac patches.\n"
            f"Upstream: {URL}\n", encoding="utf-8")
        (staged / "BUILD.json").write_text(json.dumps(stamp, indent=2) + "\n")
        if target.exists():
            shutil.rmtree(target)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copytree(staged, target)
    print(f"Bundled portable macOS SoX: {target}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
