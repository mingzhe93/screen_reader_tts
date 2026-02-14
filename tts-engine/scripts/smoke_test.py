from __future__ import annotations

import argparse
import asyncio
import json

import httpx
import websockets
from websockets.exceptions import ConnectionClosed


TERMINAL_EVENTS = {"JOB_DONE", "JOB_CANCELED", "JOB_ERROR"}


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Standalone smoke test for VoiceReader engine")
    parser.add_argument("--base-url", default="http://127.0.0.1:8765")
    parser.add_argument("--token", default="dev-token")
    parser.add_argument("--ws-timeout-sec", type=int, default=30)
    parser.add_argument("--use-subprotocol-auth", action="store_true")
    parser.add_argument("--quit-on-success", action="store_true")
    return parser


def _make_ws_connect_kwargs(token: str, use_subprotocol_auth: bool) -> dict[str, object]:
    if use_subprotocol_auth:
        print("  auth mode: Sec-WebSocket-Protocol fallback", flush=True)
        return {"subprotocols": ["auth.bearer.v1", token]}

    print("  auth mode: Authorization header", flush=True)
    headers = {"Authorization": f"Bearer {token}"}
    kwargs: dict[str, object] = {"additional_headers": headers}

    # Compatibility for older websockets versions that use extra_headers.
    import inspect

    signature = inspect.signature(websockets.connect)
    if "additional_headers" not in signature.parameters:
        kwargs["extra_headers"] = kwargs.pop("additional_headers")
    return kwargs


async def _stream_smoke(ws_url: str, token: str, ws_timeout_sec: int, use_subprotocol_auth: bool) -> None:
    saw_audio_chunk = False
    saw_terminal = False
    connect_kwargs = _make_ws_connect_kwargs(token=token, use_subprotocol_auth=use_subprotocol_auth)

    try:
        async with websockets.connect(ws_url, **connect_kwargs) as ws:
            while True:
                try:
                    message = await asyncio.wait_for(ws.recv(), timeout=ws_timeout_sec)
                except asyncio.TimeoutError as exc:
                    raise RuntimeError(
                        f"Timed out waiting for websocket event after {ws_timeout_sec}s"
                    ) from exc
                except ConnectionClosed:
                    break

                event = json.loads(message)
                event_type = str(event.get("type", ""))
                print(f"  ws_event={event_type}", flush=True)

                if event_type == "AUDIO_CHUNK":
                    saw_audio_chunk = True
                if event_type in TERMINAL_EVENTS:
                    saw_terminal = True
                    break
    except Exception as exc:
        raise RuntimeError(
            "WebSocket connect/stream failed. Ensure websocket runtime is installed "
            "and auth settings match."
        ) from exc

    if not saw_audio_chunk:
        raise RuntimeError("No AUDIO_CHUNK event received.")
    if not saw_terminal:
        raise RuntimeError("No terminal WS event (JOB_DONE/JOB_CANCELED/JOB_ERROR) received.")


async def _run(args: argparse.Namespace) -> int:
    headers = {"Authorization": f"Bearer {args.token}"}

    print("[1/4] Health check...", flush=True)
    with httpx.Client(timeout=30.0) as client:
        health = client.get(f"{args.base_url}/v1/health", headers=headers)
        health.raise_for_status()
        health_payload = health.json()
        print(
            f"  engine_version={health_payload.get('engine_version')} "
            f"model={health_payload.get('active_model_id')} "
            f"device={health_payload.get('device')}",
            flush=True,
        )
        runtime = health_payload.get("runtime", {})
        if runtime:
            print(
                f"  runtime.backend={runtime.get('backend')} "
                f"fallback_active={runtime.get('fallback_active')}",
                flush=True,
            )
            if runtime.get("backend") == "mock":
                print(
                    "WARNING: Engine is running on mock backend. "
                    "This validates API/streaming but not real model inference.",
                    flush=True,
                )

        print("[2/4] Voice list check...", flush=True)
        voices = client.get(f"{args.base_url}/v1/voices", headers=headers)
        voices.raise_for_status()
        voice_list = voices.json().get("voices", [])
        default_voice = next((voice for voice in voice_list if str(voice.get("voice_id")) == "0"), None)
        if not default_voice:
            raise RuntimeError("Default voice_id '0' not found in /v1/voices response.")
        print(f"  default voice found: {default_voice.get('display_name')}", flush=True)

        print("[3/4] Speak request with default voice_id=0...", flush=True)
        speak_body = {
            "voice_id": "0",
            "text": "VoiceReader standalone engine smoke test using the default built-in voice.",
        }
        speak = client.post(f"{args.base_url}/v1/speak", headers=headers, json=speak_body)
        speak.raise_for_status()
        speak_payload = speak.json()
        print(f"  job_id={speak_payload['job_id']}", flush=True)
        print(f"  ws_url={speak_payload['ws_url']}", flush=True)

    print("[4/4] WebSocket stream check...", flush=True)
    await _stream_smoke(
        ws_url=speak_payload["ws_url"],
        token=args.token,
        ws_timeout_sec=args.ws_timeout_sec,
        use_subprotocol_auth=args.use_subprotocol_auth,
    )

    if args.quit_on_success:
        print("[5/5] Sending /v1/quit...", flush=True)
        with httpx.Client(timeout=15.0) as client:
            quit_response = client.post(f"{args.base_url}/v1/quit", headers=headers, json={})
            quit_response.raise_for_status()
            payload = quit_response.json()
            if not payload.get("quitting"):
                raise RuntimeError("Engine did not acknowledge quit request.")
        print("  quit acknowledged", flush=True)

    print("SMOKE_TEST_OK", flush=True)
    return 0


def main() -> int:
    parser = _build_parser()
    args = parser.parse_args()
    return asyncio.run(_run(args))


if __name__ == "__main__":
    raise SystemExit(main())
