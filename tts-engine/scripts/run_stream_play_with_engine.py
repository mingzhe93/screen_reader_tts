from __future__ import annotations

import argparse
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time

import httpx


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Run stream+play test with auto-start/stop engine")
    parser.add_argument("--token", default="dev-token")
    parser.add_argument("--voice-id", default="0")
    parser.add_argument("--synth-backend", choices=["auto", "kyutai", "qwen", "mock"], default="auto")
    parser.add_argument("--kyutai-model", default="")
    parser.add_argument("--kyutai-voice-prompt", default="")
    parser.add_argument("--qwen-device-map", default="")
    parser.add_argument("--qwen-dtype", default="")
    parser.add_argument("--qwen-attn-implementation", default="")
    parser.add_argument("--qwen-speaker", default="")
    parser.add_argument("--ws-timeout-sec", type=int, default=120)
    parser.add_argument("--chunk-max-chars", type=int, default=200)
    parser.add_argument("--rate", type=float, default=1.0)
    parser.add_argument("--pitch", type=float, default=1.0)
    parser.add_argument("--volume", type=float, default=1.0)
    parser.add_argument("--prefetch-queue-size", type=int, default=5)
    parser.add_argument("--start-playback-after", type=int, default=2)
    parser.add_argument("--skip-warmup", action="store_true")
    parser.add_argument("--force-warmup", action="store_true")
    parser.add_argument("--use-subprotocol-auth", action="store_true")
    parser.add_argument("--save-wav-path", default="")
    parser.add_argument(
        "--text",
        default=(
            "This is a standalone streaming playback test. "
            "It uses multiple sentences to validate chunking behavior. "
            "You should hear several short audio chunks played in sequence. "
            "If this works, the engine streaming path and local playback loop are both healthy."
        ),
    )
    parser.add_argument("--data-dir", default="")
    return parser


def _get_free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        sock.listen(1)
        return int(sock.getsockname()[1])


def _resolve_python(engine_root: Path) -> str:
    if os.name == "nt":
        candidates = [
            engine_root / ".venv" / "Scripts" / "python.exe",
            engine_root / ".venv" / "Scripts" / "python",
        ]
    else:
        candidates = [engine_root / ".venv" / "bin" / "python"]
    for candidate in candidates:
        if candidate.exists():
            return str(candidate)
    return sys.executable or "python"


def _tail(path: Path, lines: int = 40) -> str:
    if not path.exists():
        return ""
    content = path.read_text(encoding="utf-8", errors="replace").splitlines()
    return "\n".join(content[-lines:])


def _wait_for_health(base_url: str, token: str, proc: subprocess.Popen[bytes], stdout_log: Path, stderr_log: Path) -> None:
    headers = {"Authorization": f"Bearer {token}"}
    for _ in range(60):
        time.sleep(0.2)
        if proc.poll() is not None:
            raise RuntimeError(
                f"Engine exited early with code {proc.returncode}.\n"
                f"STDOUT:\n{_tail(stdout_log)}\n"
                f"STDERR:\n{_tail(stderr_log)}"
            )
        try:
            response = httpx.get(f"{base_url}/v1/health", headers=headers, timeout=2.0)
            if response.status_code == 200:
                return
        except Exception:
            continue
    raise RuntimeError("Engine did not become healthy in time.")


def _validate_args(args: argparse.Namespace) -> None:
    if args.chunk_max_chars < 100:
        raise ValueError("chunk-max-chars must be >= 100")
    if args.prefetch_queue_size < 2:
        raise ValueError("prefetch-queue-size must be >= 2")
    if args.start_playback_after < 1:
        raise ValueError("start-playback-after must be >= 1")
    if args.start_playback_after > args.prefetch_queue_size:
        raise ValueError("start-playback-after cannot exceed prefetch-queue-size")
    if args.rate < 0.25 or args.rate > 4.0:
        raise ValueError("rate must be in [0.25, 4.0]")
    if args.pitch < 0.5 or args.pitch > 2.0:
        raise ValueError("pitch must be in [0.5, 2.0]")
    if args.volume < 0.0 or args.volume > 2.0:
        raise ValueError("volume must be in [0.0, 2.0]")


def _run(args: argparse.Namespace) -> int:
    _validate_args(args)
    engine_root = Path(__file__).resolve().parents[1]
    play_script = Path(__file__).resolve().parent / "stream_play_queue_test.py"
    python_exe = _resolve_python(engine_root)
    port = _get_free_port()
    base_url = f"http://127.0.0.1:{port}"
    print(f"Starting engine on {base_url} ...", flush=True)

    stdout_log = Path(tempfile.mkstemp(prefix="tts_engine_stdout_", suffix=".log")[1])
    stderr_log = Path(tempfile.mkstemp(prefix="tts_engine_stderr_", suffix=".log")[1])

    env = os.environ.copy()
    env["SPEAK_SELECTION_ENGINE_TOKEN"] = args.token
    env["PYTHONPATH"] = str((engine_root / "src").resolve())
    env["VOICEREADER_SYNTH_BACKEND"] = args.synth_backend
    if args.kyutai_model:
        env["VOICEREADER_KYUTAI_MODEL"] = args.kyutai_model
    if args.kyutai_voice_prompt:
        env["VOICEREADER_KYUTAI_VOICE_PROMPT"] = args.kyutai_voice_prompt
    if args.qwen_device_map:
        env["VOICEREADER_QWEN_DEVICE_MAP"] = args.qwen_device_map
    if args.qwen_dtype:
        env["VOICEREADER_QWEN_DTYPE"] = args.qwen_dtype
    if args.qwen_attn_implementation:
        env["VOICEREADER_QWEN_ATTN_IMPLEMENTATION"] = args.qwen_attn_implementation
    if args.qwen_speaker:
        env["VOICEREADER_QWEN_SPEAKER"] = args.qwen_speaker

    engine_cmd = [
        python_exe,
        "-m",
        "tts_engine",
        "--server",
        "--port",
        str(port),
    ]
    if args.data_dir:
        engine_cmd.extend(["--data-dir", args.data_dir])

    with stdout_log.open("wb") as out_file, stderr_log.open("wb") as err_file:
        proc = subprocess.Popen(
            engine_cmd,
            cwd=str(engine_root),
            env=env,
            stdout=out_file,
            stderr=err_file,
        )

    try:
        _wait_for_health(base_url=base_url, token=args.token, proc=proc, stdout_log=stdout_log, stderr_log=stderr_log)

        play_cmd = [
            python_exe,
            str(play_script),
            "--base-url",
            base_url,
            "--token",
            args.token,
            "--voice-id",
            args.voice_id,
            "--ws-timeout-sec",
            str(args.ws_timeout_sec),
            "--chunk-max-chars",
            str(args.chunk_max_chars),
            "--rate",
            str(args.rate),
            "--pitch",
            str(args.pitch),
            "--volume",
            str(args.volume),
            "--prefetch-queue-size",
            str(args.prefetch_queue_size),
            "--start-playback-after",
            str(args.start_playback_after),
            "--text",
            args.text,
            "--quit-on-done",
        ]
        if not args.skip_warmup:
            play_cmd.append("--warmup-wait")
        if args.force_warmup:
            play_cmd.append("--warmup-force")
        if args.use_subprotocol_auth:
            play_cmd.append("--use-subprotocol-auth")
        if args.save_wav_path:
            play_cmd.extend(["--save-wav-path", args.save_wav_path])

        completed = subprocess.run(play_cmd, cwd=str(engine_root), env=env)
        if completed.returncode != 0:
            raise RuntimeError(f"stream_play_queue_test.py failed with exit code {completed.returncode}")
    finally:
        headers = {"Authorization": f"Bearer {args.token}"}
        if proc.poll() is None:
            try:
                httpx.post(f"{base_url}/v1/quit", headers=headers, json={}, timeout=5.0)
                time.sleep(0.7)
            except Exception:
                pass
        if proc.poll() is None:
            proc.kill()
        try:
            proc.wait(timeout=5.0)
        except Exception:
            pass
        for path in (stdout_log, stderr_log):
            for _ in range(5):
                try:
                    path.unlink(missing_ok=True)
                    break
                except PermissionError:
                    time.sleep(0.1)

    return 0


def main() -> int:
    parser = _build_parser()
    args = parser.parse_args()
    return _run(args)


if __name__ == "__main__":
    raise SystemExit(main())
