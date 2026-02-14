from __future__ import annotations

import argparse
import importlib.util
import json
from typing import Any

import uvicorn
from uvicorn.server import Server

from .app import create_app
from .config import (
    DEFAULT_TOKEN_ENV,
    EngineConfig,
    load_env_config_value,
    load_token,
    resolve_data_dir,
)


def main() -> int:
    parser = argparse.ArgumentParser(description="Speak Selection engine daemon")
    parser.add_argument("--server", action="store_true", help="Run the local HTTP/WS engine server")
    parser.add_argument("--host", default="127.0.0.1", help="Bind host (loopback only recommended)")
    parser.add_argument("--port", type=int, default=8765, help="Bind port")
    parser.add_argument("--token", default=None, help="Session token (Bearer auth)")
    parser.add_argument(
        "--token-env",
        default=DEFAULT_TOKEN_ENV,
        help=f"Environment variable name for token (default: {DEFAULT_TOKEN_ENV})",
    )
    parser.add_argument("--data-dir", default=None, help="Engine data directory")
    parser.add_argument(
        "--bootstrap-stdin",
        action="store_true",
        help="Read JSON bootstrap payload from stdin: {token,port,data_dir}",
    )
    args = parser.parse_args()

    if not args.server:
        parser.print_help()
        return 0

    bootstrap = _load_bootstrap_payload() if args.bootstrap_stdin else {}

    token = (
        (args.token or "").strip()
        or str(bootstrap.get("token", "")).strip()
        or load_token(None, token_env=args.token_env)
    )
    if not token:
        raise SystemExit(
            "Engine token is required. Pass --token, set token in --bootstrap-stdin payload, "
            f"or set ${args.token_env}."
        )

    port = int(bootstrap.get("port", args.port))
    data_dir = resolve_data_dir(str(bootstrap.get("data_dir")) if bootstrap.get("data_dir") else args.data_dir)
    _ensure_websocket_runtime()

    config = EngineConfig(
        token=token,
        host=args.host,
        port=port,
        data_dir=data_dir,
        synth_backend=load_env_config_value("VOICEREADER_SYNTH_BACKEND", "auto"),
        qwen_model_name=load_env_config_value(
            "VOICEREADER_QWEN_MODEL",
            "Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice",
        ),
        qwen_device_map=load_env_config_value("VOICEREADER_QWEN_DEVICE_MAP", "cuda:0"),
        qwen_dtype=load_env_config_value("VOICEREADER_QWEN_DTYPE", "bfloat16"),
        qwen_attn_implementation=load_env_config_value(
            "VOICEREADER_QWEN_ATTN_IMPLEMENTATION",
            "flash_attention_2",
        ),
        qwen_default_speaker=load_env_config_value("VOICEREADER_QWEN_SPEAKER", "Ryan"),
    )

    app = create_app(config)
    uvicorn_config = uvicorn.Config(app, host=config.host, port=config.port, log_level="info")
    server = uvicorn.Server(uvicorn_config)
    app.state.request_shutdown = lambda: _request_shutdown(server)
    server.run()
    return 0


def _load_bootstrap_payload() -> dict[str, Any]:
    import sys

    payload = sys.stdin.read().strip()
    if not payload:
        return {}
    try:
        parsed = json.loads(payload)
    except json.JSONDecodeError as exc:  # pragma: no cover - startup guard
        raise SystemExit(f"Invalid --bootstrap-stdin payload: {exc}") from exc
    if not isinstance(parsed, dict):
        raise SystemExit("Invalid --bootstrap-stdin payload: expected a JSON object")
    return parsed


def _ensure_websocket_runtime() -> None:
    has_websockets = importlib.util.find_spec("websockets") is not None
    has_wsproto = importlib.util.find_spec("wsproto") is not None
    if has_websockets or has_wsproto:
        return
    raise SystemExit(
        "No WebSocket runtime found. Install one of: "
        "`python -m pip install websockets` or `python -m pip install wsproto`."
    )


def _request_shutdown(server: Server) -> None:
    server.should_exit = True
