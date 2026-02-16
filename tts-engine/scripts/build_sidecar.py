#!/usr/bin/env python3
from __future__ import annotations

import argparse
import os
import platform
import shutil
import subprocess
import sys
import time
from pathlib import Path


def detect_target_triple() -> str:
    machine = platform.machine().lower()
    if machine in {"amd64", "x86_64", "x64"}:
        arch = "x86_64"
    elif machine in {"arm64", "aarch64"}:
        arch = "aarch64"
    else:
        raise RuntimeError(f"Unsupported architecture for sidecar build: {machine}")

    if sys.platform.startswith("win"):
        return f"{arch}-pc-windows-msvc"
    if sys.platform == "darwin":
        return f"{arch}-apple-darwin"
    if sys.platform.startswith("linux"):
        return f"{arch}-unknown-linux-gnu"

    raise RuntimeError(f"Unsupported platform for sidecar build: {sys.platform}")


def resolve_python(engine_dir: Path) -> Path:
    if sys.platform.startswith("win"):
        venv_python = engine_dir / ".venv" / "Scripts" / "python.exe"
    else:
        venv_python = engine_dir / ".venv" / "bin" / "python"

    if venv_python.exists():
        return venv_python
    return Path(sys.executable)


def ensure_pyinstaller(python: Path, engine_dir: Path) -> None:
    probe = subprocess.run(
        [str(python), "-c", "import PyInstaller"],
        cwd=engine_dir,
        capture_output=True,
        text=True,
    )
    if probe.returncode == 0:
        return
    raise RuntimeError(
        "PyInstaller is not installed in the selected engine environment. "
        "Run: tts-engine/.venv/Scripts/python -m pip install pyinstaller"
    )


def _kyutai_model_dir(base: Path) -> Path:
    return base / "models" / "Verylicious" / "pocket-tts-ungated"


def _is_kyutai_model_ready(repo_dir: Path) -> bool:
    required = [
        repo_dir / "voicereader-pocket-tts.yaml",
        repo_dir / "tts_b6369a24.safetensors",
        repo_dir / "tokenizer.model",
        repo_dir / "embeddings" / "alba.safetensors",
    ]
    return all(path.exists() for path in required)


def _copy_kyutai_model_repo(source_repo: Path, target_repo: Path) -> None:
    if target_repo.exists():
        _remove_path_with_retry(target_repo)
    target_repo.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(source_repo, target_repo, ignore=shutil.ignore_patterns(".cache"))

    cache_dir = target_repo / ".cache"
    if cache_dir.exists():
        _remove_path_with_retry(cache_dir)


def _remove_path_with_retry(path: Path, attempts: int = 24, delay_seconds: float = 0.35) -> None:
    last_error: Exception | None = None
    for attempt in range(1, attempts + 1):
        try:
            if not path.exists():
                return
            if path.is_dir():
                shutil.rmtree(path)
            else:
                path.unlink()
            return
        except FileNotFoundError:
            return
        except PermissionError as exc:
            last_error = exc
        except OSError as exc:
            # Windows file lock / access denied while process still holds an open handle.
            if getattr(exc, "winerror", None) in (5, 32):
                last_error = exc
            else:
                raise

        if attempt < attempts:
            time.sleep(delay_seconds)

    raise RuntimeError(
        "Failed to replace bundled sidecar because files are in use. "
        "Close VoiceReader and stop any running tts-engine.exe process, then retry."
    ) from last_error


def ensure_bundled_kyutai_model(root: Path, engine_dir: Path, python: Path) -> Path:
    bundled_models_root = root / "src-tauri" / "binaries"
    target_repo = _kyutai_model_dir(bundled_models_root)
    if _is_kyutai_model_ready(target_repo):
        print(f"Bundled Kyutai model already present: {target_repo}")
        return target_repo

    local_data_dir = engine_dir / ".data"
    source_repo = _kyutai_model_dir(local_data_dir)
    if not _is_kyutai_model_ready(source_repo):
        prefetch_script = engine_dir / "scripts" / "prefetch_models.py"
        if not prefetch_script.exists():
            raise RuntimeError(f"Prefetch script not found: {prefetch_script}")
        print("Prefetching Kyutai model into local engine store...")
        subprocess.run(
            [
                str(python),
                str(prefetch_script),
                "--data-dir",
                str(local_data_dir),
                "--kyutai-only",
            ],
            cwd=engine_dir,
            check=True,
        )

    if not _is_kyutai_model_ready(source_repo):
        raise RuntimeError(
            "Kyutai model mirror is missing required files after prefetch. "
            f"Expected repo dir: {source_repo}"
        )

    _copy_kyutai_model_repo(source_repo, target_repo)
    print(f"Copied bundled Kyutai model to: {target_repo}")
    return target_repo


def _find_sox_executable() -> Path | None:
    raw_override = os.getenv("VOICEREADER_SOX_PATH", "").strip()
    if raw_override:
        candidate = Path(raw_override)
        if candidate.exists():
            return candidate

    which_path = shutil.which("sox")
    if which_path:
        return Path(which_path)

    if not sys.platform.startswith("win"):
        return None

    local_app_data = os.getenv("LOCALAPPDATA", "").strip()
    if not local_app_data:
        return None
    root = Path(local_app_data) / "Microsoft" / "WinGet" / "Packages"
    if not root.exists():
        return None

    for candidate in sorted(root.glob("ChrisBagwell.SoX_*")):
        for nested in sorted(candidate.glob("sox-*/sox.exe")):
            if nested.exists():
                return nested
        direct = candidate / "sox.exe"
        if direct.exists():
            return direct
    return None


def ensure_bundled_sox(root: Path) -> Path | None:
    sox_executable = _find_sox_executable()
    target_dir = root / "src-tauri" / "binaries" / "sox"

    if sox_executable is None:
        print("WARNING: SoX not found on build machine. Portable/runtime will use pitch-shifting rate fallback.")
        return None

    source_dir = sox_executable.parent
    if not source_dir.exists():
        print("WARNING: SoX path resolved but parent directory is missing; skipping SoX bundling.")
        return None

    if target_dir.exists():
        _remove_path_with_retry(target_dir)
    shutil.copytree(source_dir, target_dir)
    print(f"Bundled SoX runtime to: {target_dir}")
    return target_dir


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Build the Python sidecar runtime and/or ensure bundled Kyutai model files."
    )
    parser.add_argument(
        "--models-only",
        action="store_true",
        help="Skip PyInstaller sidecar build and only ensure bundled Kyutai model files are present.",
    )
    args = parser.parse_args()

    root = Path(__file__).resolve().parents[2]
    engine_dir = root / "tts-engine"
    src_tauri_binaries = root / "src-tauri" / "binaries"
    src_tauri_binaries.mkdir(parents=True, exist_ok=True)

    entrypoint = engine_dir / "sidecar_entry.py"
    if not entrypoint.exists():
        raise RuntimeError(f"Engine entrypoint not found: {entrypoint}")

    python = resolve_python(engine_dir)
    if not python.exists():
        raise RuntimeError(f"Python executable not found: {python}")

    if args.models_only:
        ensure_bundled_kyutai_model(root=root, engine_dir=engine_dir, python=python)
        ensure_bundled_sox(root=root)
        return 0

    ensure_pyinstaller(python, engine_dir)
    cmd = [
        str(python),
        "-m",
        "PyInstaller",
        "--noconfirm",
        "--clean",
        "--onedir",
        "--paths",
        str(engine_dir / "src"),
        "--name",
        "tts-engine",
        str(entrypoint),
    ]

    exe_suffix = ".exe" if sys.platform.startswith("win") else ""
    target_triple = detect_target_triple()
    sidecar_bin_name = f"tts-engine{exe_suffix}"
    built_dir = engine_dir / "dist" / "tts-engine"
    built_exe = built_dir / sidecar_bin_name
    bundled_dir = src_tauri_binaries / f"tts-engine-{target_triple}"
    bundled_exe = bundled_dir / sidecar_bin_name
    profile_marker = bundled_dir / ".build-profile"
    legacy_onefile_path = src_tauri_binaries / f"tts-engine-{target_triple}{exe_suffix}"

    marker_value = ""
    if profile_marker.exists():
        marker_value = profile_marker.read_text(encoding="utf-8").strip()
    expected_marker = f"profile=full|python={python}"

    if not (
        bundled_exe.exists()
        and bundled_exe.stat().st_mtime >= entrypoint.stat().st_mtime
        and marker_value == expected_marker
    ):
        print("Building sidecar executable with PyInstaller...")
        print(f"Using Python: {python}")
        subprocess.run(cmd, cwd=engine_dir, check=True)

        if not built_exe.exists():
            raise RuntimeError(f"PyInstaller output not found: {built_exe}")

        if bundled_dir.exists():
            _remove_path_with_retry(bundled_dir)
        shutil.copytree(built_dir, bundled_dir)
        profile_marker.write_text(expected_marker, encoding="utf-8")
        print(f"Copied sidecar runtime directory to: {bundled_dir}")
    else:
        print(f"Sidecar runtime already up to date: {bundled_dir} ({expected_marker})")

    if legacy_onefile_path.exists():
        _remove_path_with_retry(legacy_onefile_path)
        print(f"Removed legacy onefile sidecar: {legacy_onefile_path}")

    ensure_bundled_kyutai_model(root=root, engine_dir=engine_dir, python=python)
    ensure_bundled_sox(root=root)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
