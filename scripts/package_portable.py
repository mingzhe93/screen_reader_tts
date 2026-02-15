#!/usr/bin/env python3
from __future__ import annotations

import json
import shutil
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


def main() -> int:
    root = _repo_root()
    src_tauri = root / "src-tauri"
    target_release = src_tauri / "target" / "release"
    binaries_dir = src_tauri / "binaries"
    portable_root = target_release / "portable"
    bundle_portable_dir = target_release / "bundle" / "portable"

    product_name, version = _read_app_meta(src_tauri)
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
    print("Copying sidecar runtime and bundled models...", flush=True)
    _copytree(binaries_dir, portable_dir / "binaries")

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
