#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
from pathlib import Path
from zipfile import ZIP_STORED, ZipFile


def _repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def _read_app_meta(src_tauri_dir: Path) -> tuple[str, str]:
    conf_path = src_tauri_dir / "tauri.conf.json"
    payload = json.loads(conf_path.read_text(encoding="utf-8"))
    package = payload.get("package", {})
    product_name = package.get("productName", "VoiceReader")
    version = package.get("version", "0.0.0")
    return str(product_name), str(version)


def _copytree(src: Path, dst: Path) -> None:
    if dst.exists():
        try:
            shutil.rmtree(dst)
        except PermissionError as exc:
            raise RuntimeError(
                f"Failed to remove {dst}. Close any running VoiceReader portable app in that folder and retry."
            ) from exc
    shutil.copytree(src, dst)


def _zip_dir(source_dir: Path, zip_path: Path) -> None:
    zip_path.parent.mkdir(parents=True, exist_ok=True)
    if zip_path.exists():
        zip_path.unlink()

    files = [path for path in source_dir.rglob("*") if path.is_file()]
    print(f"Zipping {len(files)} files into {zip_path.name} (store mode, no compression)...", flush=True)
    with ZipFile(zip_path, mode="w", compression=ZIP_STORED) as zf:
        for index, path in enumerate(files, start=1):
            arcname = path.relative_to(source_dir.parent)
            zf.write(path, arcname=arcname)
            if index % 500 == 0:
                print(f"  zipped {index}/{len(files)} files...", flush=True)


def _package_macos(target_release: Path, product_name: str, version: str, variant: str) -> int:
    if variant != "base":
        raise RuntimeError("The macOS portable release currently supports the Base build only.")
    app = target_release / "bundle" / "macos" / f"{product_name}.app"
    if not app.is_dir():
        raise RuntimeError(f"App bundle missing: {app}. Run `npm run desktop:build:macos:portable`.")
    # Inspect the actual executable, rather than labelling a cross-build with the host architecture.
    import plistlib
    info = plistlib.loads((app / "Contents" / "Info.plist").read_bytes())
    executable = app / "Contents" / "MacOS" / info["CFBundleExecutable"]
    arch = subprocess.check_output(["lipo", "-archs", str(executable)], text=True).strip()
    arch_label = {"arm64": "arm64", "x86_64": "x64"}.get(arch)
    if arch_label is None:
        raise RuntimeError(f"Unsupported app architectures: {arch}; bundle matching ONNX Runtime first.")
    resources = app / "Contents" / "Resources" / "binaries"
    for relative in [
        "onnxruntime/libonnxruntime.dylib",
        "sox-macos/sox",
        "models/Verylicious/pocket-tts-ungated/tts_b6369a24.safetensors",
        "models/Verylicious/pocket-tts-ungated/tokenizer.model",
        "models/Verylicious/pocket-tts-ungated/voicereader-pocket-tts.yaml",
        "models/Verylicious/pocket-tts-ungated/embeddings/alba.safetensors",
        "kyutai-voices/ATTRIBUTION.txt",
    ]:
        if not (resources / relative).is_file():
            raise RuntimeError(f"Required bundled asset missing: {resources / relative}")
    runtime_arch = subprocess.check_output(
        ["lipo", "-archs", str(resources / "onnxruntime/libonnxruntime.dylib")], text=True
    ).split()
    if arch not in runtime_arch:
        raise RuntimeError(f"App architecture {arch} does not match ONNX Runtime {runtime_arch}.")
    if arch == "arm64":
        plugin = resources / "onnxruntime/libonnxruntime_providers_webgpu.dylib"
        if not plugin.is_file():
            raise RuntimeError("The Apple Silicon app requires its bundled WebGPU plugin.")
        plugin_arch = subprocess.check_output(["lipo", "-archs", str(plugin)], text=True).split()
        if arch not in plugin_arch:
            raise RuntimeError(f"WebGPU plugin does not support {arch}: {plugin_arch}")
        for relative in ["audio8-metal/codec_decoder_fp32.onnx", "audio8-metal/codec_decoder_fp16.source.onnx", "audio8-metal/LICENSE", "audio8-metal/NOTICE"]:
            if not (resources / relative).is_file():
                raise RuntimeError(f"Required FP32 decoder asset missing: {resources / relative}")
    # Seal all bundled libraries and resources after Tauri has assembled the app.
    subprocess.run(["codesign", "--force", "--deep", "--sign", "-", str(app)], check=True)
    subprocess.run(["codesign", "--verify", "--deep", "--strict", str(app)], check=True)
    zip_path = target_release / "bundle" / "portable" / f"{product_name}_{version}_macos_{arch_label}_portable.zip"
    zip_path.parent.mkdir(parents=True, exist_ok=True)
    # ditto preserves app permissions, symlinks and macOS bundle metadata.
    zip_path.unlink(missing_ok=True)
    subprocess.run(["ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", str(app), str(zip_path)], check=True)
    print(f"PORTABLE_APP={app}")
    print(f"PORTABLE_ZIP={zip_path}")
    print(f"PORTABLE_ZIP_SIZE_MB={zip_path.stat().st_size / (1024 * 1024):.2f}")
    print("PORTABLE_PACKAGE_OK")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Package VoiceReader portable folder/zip.")
    parser.add_argument(
        "--variant",
        choices=("full", "base"),
        default="full",
        help="Packaging variant. full copies full binaries tree; base copies bundled model files only.",
    )
    args = parser.parse_args()

    root = _repo_root()
    src_tauri = root / "src-tauri"
    target_release = src_tauri / "target" / "release"
    binaries_dir = src_tauri / "binaries"
    portable_root = target_release / "portable"
    bundle_portable_dir = target_release / "bundle" / "portable"

    product_name, version = _read_app_meta(src_tauri)
    if sys.platform == "darwin":
        return _package_macos(target_release, product_name, version, args.variant)
    exe_name = f"{product_name}.exe"
    exe_path = target_release / exe_name

    if not exe_path.exists():
        raise RuntimeError(
            f"Portable packaging failed: app executable not found at {exe_path}. "
            "Run desktop release build first."
        )
    if not binaries_dir.exists():
        raise RuntimeError(
            f"Portable packaging failed: binaries directory not found at {binaries_dir}. "
            "Run `npm run sidecar:build` first."
        )

    portable_dir = portable_root / f"{product_name}-portable-win-x64"
    if portable_dir.exists():
        try:
            shutil.rmtree(portable_dir)
        except PermissionError as exc:
            raise RuntimeError(
                f"Failed to remove {portable_dir}. Close any running VoiceReader portable app and retry."
            ) from exc
    portable_dir.mkdir(parents=True, exist_ok=True)

    print(f"Preparing portable folder: {portable_dir}", flush=True)
    shutil.copy2(exe_path, portable_dir / exe_name)
    if args.variant == "full":
        print("Copying sidecar runtime and bundled models...", flush=True)
        _copytree(binaries_dir, portable_dir / "binaries")
    else:
        source_models_dir = binaries_dir / "models"
        if not source_models_dir.exists():
            raise RuntimeError(
                f"Portable packaging failed: bundled models directory not found at {source_models_dir}. "
                "Run `npm run models:bundle:kyutai` first."
            )
        print("Copying bundled models (base variant)...", flush=True)
        (portable_dir / "binaries").mkdir(parents=True, exist_ok=True)
        _copytree(source_models_dir, portable_dir / "binaries" / "models")
        source_onnxruntime_dir = binaries_dir / "onnxruntime"
        if source_onnxruntime_dir.exists():
            _copytree(source_onnxruntime_dir, portable_dir / "binaries" / "onnxruntime")
        else:
            print(
                "WARNING: ONNX Runtime library not found under src-tauri/binaries/onnxruntime. "
                "Audio8 TTS will be unavailable in this package; run `npm run onnxruntime:fetch`."
            )
        source_kyutai_voices_dir = binaries_dir / "kyutai-voices"
        if source_kyutai_voices_dir.exists():
            _copytree(source_kyutai_voices_dir, portable_dir / "binaries" / "kyutai-voices")
        else:
            print(
                "WARNING: extra Kyutai voice clips not found under src-tauri/binaries/kyutai-voices. "
                "Only the eight built-in presets will work; run `npm run kyutai-voices:fetch`."
            )
        source_sox_dir = binaries_dir / "sox"
        if source_sox_dir.exists():
            print("Copying bundled SoX runtime (base variant)...", flush=True)
            _copytree(source_sox_dir, portable_dir / "binaries" / "sox")
        else:
            print(
                "WARNING: bundled SoX runtime not found under src-tauri/binaries/sox. "
                "Rate control will use pitch-shifting fallback on machines without SoX installed.",
                flush=True,
            )

    readme_path = portable_dir / "README-PORTABLE.txt"
    readme_path.write_text(
        "\n".join(
            [
                f"{product_name} Portable",
                "",
                "Run:",
                f"- {exe_name}",
                "",
                "Notes:",
                "- No installer/admin rights required.",
                "- Keep the binaries folder next to the exe.",
                f"- Variant: {args.variant}",
                "- Voice data and runtime cache are written under LocalAppData.",
            ]
        )
        + "\n",
        encoding="utf-8",
    )

    zip_name = f"{product_name}_{version}_x64_portable.zip"
    zip_path = bundle_portable_dir / zip_name
    _zip_dir(portable_dir, zip_path)

    portable_mb = sum(p.stat().st_size for p in portable_dir.rglob("*") if p.is_file()) / (1024 * 1024)
    zip_mb = zip_path.stat().st_size / (1024 * 1024)

    print(f"PORTABLE_DIR={portable_dir}")
    print(f"PORTABLE_SIZE_MB={portable_mb:.2f}")
    print(f"PORTABLE_ZIP={zip_path}")
    print(f"PORTABLE_ZIP_SIZE_MB={zip_mb:.2f}")
    print("PORTABLE_PACKAGE_OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
