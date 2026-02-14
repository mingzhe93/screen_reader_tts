from __future__ import annotations

import argparse
import asyncio
import base64
from dataclasses import dataclass
import io
import json
import platform
import sys
import time
import wave

import httpx
import websockets
from websockets.exceptions import ConnectionClosed


TERMINAL_EVENTS = {"JOB_DONE", "JOB_CANCELED", "JOB_ERROR"}


@dataclass(slots=True)
class AudioChunk:
    index: int
    recv_ms: int
    sample_rate: int
    channels: int
    pcm_s16le: bytes


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Queue-based stream playback test for VoiceReader engine")
    parser.add_argument("--base-url", default="http://127.0.0.1:8765")
    parser.add_argument("--token", default="dev-token")
    parser.add_argument("--voice-id", default="0")
    parser.add_argument("--chunk-max-chars", type=int, default=160)
    parser.add_argument("--rate", type=float, default=1.0)
    parser.add_argument("--pitch", type=float, default=1.0)
    parser.add_argument("--volume", type=float, default=1.0)
    parser.add_argument("--ws-timeout-sec", type=int, default=120)
    parser.add_argument("--prefetch-queue-size", type=int, default=5)
    parser.add_argument("--start-playback-after", type=int, default=2)
    parser.add_argument("--warmup-wait", action="store_true")
    parser.add_argument("--warmup-force", action="store_true")
    parser.add_argument("--use-subprotocol-auth", action="store_true")
    parser.add_argument("--quit-on-done", action="store_true")
    parser.add_argument("--save-wav-path", default="")
    parser.add_argument(
        "--text",
        default=(
            "This is a standalone queue playback test. "
            "It uses multiple sentences to validate chunking and buffering behavior. "
            "You should hear smoother playback when prebuffering is enabled."
        ),
    )
    return parser


def _pcm_to_wav_bytes(pcm_s16le: bytes, sample_rate: int, channels: int) -> bytes:
    with io.BytesIO() as stream:
        with wave.open(stream, "wb") as wav:
            wav.setnchannels(channels)
            wav.setsampwidth(2)  # s16le
            wav.setframerate(sample_rate)
            wav.writeframes(pcm_s16le)
        return stream.getvalue()


def _play_wav_sync(wav_bytes: bytes) -> None:
    if platform.system().lower().startswith("win"):
        import winsound

        winsound.PlaySound(wav_bytes, winsound.SND_MEMORY)
        return

    # Non-windows fallback: no-op playback to keep script portable.
    return


def _elapsed_ms(start_perf: float) -> int:
    return int((time.perf_counter() - start_perf) * 1000.0)


async def _receive_ws_events(
    ws,
    queue: asyncio.Queue[AudioChunk | None],
    ws_timeout_sec: int,
    start_perf: float,
) -> str | None:
    chunk_count = 0
    terminal_event: str | None = None
    last_recv_ms = 0

    try:
        while True:
            try:
                message = await asyncio.wait_for(ws.recv(), timeout=ws_timeout_sec)
            except asyncio.TimeoutError as exc:
                raise RuntimeError(f"Timed out waiting for websocket event after {ws_timeout_sec}s") from exc
            except ConnectionClosed:
                break

            event = json.loads(message)
            event_type = str(event.get("type", ""))
            print(f"  ws_event={event_type}", flush=True)

            if event_type == "AUDIO_CHUNK":
                chunk_count += 1
                recv_ms = _elapsed_ms(start_perf)
                gap_ms = recv_ms - last_recv_ms
                last_recv_ms = recv_ms

                audio = event["audio"]
                pcm_s16le = base64.b64decode(audio["data_base64"])
                chunk = AudioChunk(
                    index=chunk_count,
                    recv_ms=recv_ms,
                    sample_rate=int(audio["sample_rate"]),
                    channels=int(audio["channels"]),
                    pcm_s16le=pcm_s16le,
                )
                await queue.put(chunk)
                print(
                    f"    chunk={chunk.index} recv_t={recv_ms}ms gap_since_prev={gap_ms}ms "
                    f"bytes={len(pcm_s16le)} qsize={queue.qsize()}",
                    flush=True,
                )

            if event_type in TERMINAL_EVENTS:
                terminal_event = event_type
                break
    finally:
        await queue.put(None)

    return terminal_event


async def _consume_and_play(
    queue: asyncio.Queue[AudioChunk | None],
    start_playback_after: int,
    start_perf: float,
    save_wav_path: str,
) -> int:
    played = 0
    pcm_accumulator = bytearray()
    output_sample_rate = 24_000
    output_channels = 1

    # Prebuffer before starting playback to reduce underflow gaps.
    prebuffer: list[AudioChunk] = []
    while len(prebuffer) < start_playback_after:
        item = await queue.get()
        if item is None:
            break
        prebuffer.append(item)

    async def _play_chunk(chunk: AudioChunk) -> None:
        nonlocal played, output_sample_rate, output_channels
        play_start_ms = _elapsed_ms(start_perf)
        wait_before_play_ms = play_start_ms - chunk.recv_ms
        wav_bytes = _pcm_to_wav_bytes(chunk.pcm_s16le, chunk.sample_rate, chunk.channels)
        await asyncio.to_thread(_play_wav_sync, wav_bytes)
        play_dur_ms = _elapsed_ms(start_perf) - play_start_ms
        played += 1
        output_sample_rate = chunk.sample_rate
        output_channels = chunk.channels
        pcm_accumulator.extend(chunk.pcm_s16le)
        print(
            f"    chunk={chunk.index} playback_wait={wait_before_play_ms}ms playback_dur={play_dur_ms}ms",
            flush=True,
        )

    for chunk in prebuffer:
        await _play_chunk(chunk)

    while True:
        item = await queue.get()
        if item is None:
            break
        await _play_chunk(item)

    if save_wav_path and pcm_accumulator:
        wav = _pcm_to_wav_bytes(bytes(pcm_accumulator), sample_rate=output_sample_rate, channels=output_channels)
        with open(save_wav_path, "wb") as f:
            f.write(wav)
        print(f"  saved combined wav to: {save_wav_path}", flush=True)

    return played


async def _run(args: argparse.Namespace) -> int:
    if args.chunk_max_chars < 100:
        print(
            f"WARNING: chunk-max-chars={args.chunk_max_chars} is below API minimum 100, using 100.",
            flush=True,
        )
        args.chunk_max_chars = 100
    if args.prefetch_queue_size < 2:
        raise RuntimeError("prefetch-queue-size must be >= 2")
    if args.start_playback_after < 1:
        raise RuntimeError("start-playback-after must be >= 1")
    if args.start_playback_after > args.prefetch_queue_size:
        raise RuntimeError("start-playback-after cannot exceed prefetch-queue-size")
    if args.rate < 0.25 or args.rate > 4.0:
        raise RuntimeError("rate must be in [0.25, 4.0]")
    if args.pitch < 0.5 or args.pitch > 2.0:
        raise RuntimeError("pitch must be in [0.5, 2.0]")
    if args.volume < 0.0 or args.volume > 2.0:
        raise RuntimeError("volume must be in [0.0, 2.0]")

    headers = {"Authorization": f"Bearer {args.token}"}

    print("[1/5] Health check...", flush=True)
    with httpx.Client(timeout=30.0) as client:
        health = client.get(f"{args.base_url}/v1/health", headers=headers)
        health.raise_for_status()
        health_payload = health.json()
        print(
            f"  model={health_payload.get('active_model_id')} device={health_payload.get('device')}",
            flush=True,
        )
        runtime = health_payload.get("runtime", {})
        if runtime:
            print(
                f"  runtime.backend={runtime.get('backend')} "
                f"fallback_active={runtime.get('fallback_active')}",
                flush=True,
            )
            detail = runtime.get("detail")
            if detail:
                print(f"  runtime.detail={detail}", flush=True)
            warmup = runtime.get("warmup") or {}
            if warmup:
                print(
                    f"  runtime.warmup_status={warmup.get('status')} "
                    f"runs={warmup.get('runs')}",
                    flush=True,
                )
            if runtime.get("backend") == "mock":
                print("WARNING: mock backend active; audio is placeholder, not real speech.", flush=True)

        if args.warmup_wait:
            print("[1.5/5] Warmup request...", flush=True)
            warmup_payload = {
                "wait": True,
                "force": bool(args.warmup_force),
                "reason": "stream_play_queue_test",
            }
            warmup_response = client.post(f"{args.base_url}/v1/warmup", headers=headers, json=warmup_payload)
            warmup_response.raise_for_status()
            warmup = warmup_response.json().get("warmup", {})
            print(
                f"  warmup.accepted={warmup_response.json().get('accepted')} "
                f"status={warmup.get('status')} "
                f"duration_ms={warmup.get('last_duration_ms')} "
                f"error={warmup.get('last_error')}",
                flush=True,
            )

        print("[2/5] Voice list check...", flush=True)
        voices = client.get(f"{args.base_url}/v1/voices", headers=headers)
        voices.raise_for_status()
        voice_list = voices.json().get("voices", [])
        selected = next((voice for voice in voice_list if str(voice.get("voice_id")) == args.voice_id), None)
        if not selected:
            raise RuntimeError(f"Requested voice_id '{args.voice_id}' not found")
        print(f"  using voice_id={args.voice_id} display_name={selected.get('display_name')}", flush=True)

        print("[3/5] Speak request...", flush=True)
        print(
            f"  settings.rate={args.rate} settings.pitch={args.pitch} settings.volume={args.volume}",
            flush=True,
        )
        speak_body = {
            "voice_id": args.voice_id,
            "text": args.text,
            "settings": {
                "rate": args.rate,
                "pitch": args.pitch,
                "volume": args.volume,
                "chunking": {"max_chars": args.chunk_max_chars},
            },
        }
        speak = client.post(f"{args.base_url}/v1/speak", headers=headers, json=speak_body)
        speak.raise_for_status()
        speak_payload = speak.json()
        job_id = speak_payload["job_id"]
        ws_url = speak_payload["ws_url"]
        print(f"  job_id={job_id}", flush=True)
        print(f"  ws_url={ws_url}", flush=True)

    print("[4/5] Stream + queue playback...", flush=True)
    print(
        f"  queue.prefetch={args.prefetch_queue_size} start_playback_after={args.start_playback_after}",
        flush=True,
    )

    connect_kwargs: dict[str, object] = {}
    if args.use_subprotocol_auth:
        print("  auth mode: Sec-WebSocket-Protocol fallback", flush=True)
        connect_kwargs["subprotocols"] = ["auth.bearer.v1", args.token]
    else:
        print("  auth mode: Authorization header", flush=True)
        connect_kwargs["additional_headers"] = headers

    # Compatibility for older websockets versions that use extra_headers.
    if "additional_headers" in connect_kwargs:
        import inspect

        signature = inspect.signature(websockets.connect)
        if "additional_headers" not in signature.parameters:
            connect_kwargs["extra_headers"] = connect_kwargs.pop("additional_headers")

    start_perf = time.perf_counter()
    queue: asyncio.Queue[AudioChunk | None] = asyncio.Queue(maxsize=args.prefetch_queue_size)

    terminal_event: str | None = None
    played_count = 0
    async with websockets.connect(ws_url, **connect_kwargs) as ws:
        recv_task = asyncio.create_task(
            _receive_ws_events(
                ws=ws,
                queue=queue,
                ws_timeout_sec=args.ws_timeout_sec,
                start_perf=start_perf,
            )
        )
        play_task = asyncio.create_task(
            _consume_and_play(
                queue=queue,
                start_playback_after=args.start_playback_after,
                start_perf=start_perf,
                save_wav_path=args.save_wav_path,
            )
        )
        terminal_event = await recv_task
        played_count = await play_task

    if played_count < 1:
        raise RuntimeError("No AUDIO_CHUNK events were played")
    if not terminal_event:
        raise RuntimeError("No terminal websocket event received")

    print(f"  played_chunks={played_count} terminal={terminal_event}", flush=True)

    if args.quit_on_done:
        print("[5/5] Sending /v1/quit...", flush=True)
        with httpx.Client(timeout=15.0) as client:
            quit_response = client.post(f"{args.base_url}/v1/quit", headers=headers, json={})
            quit_response.raise_for_status()
            payload = quit_response.json()
            if not payload.get("quitting"):
                raise RuntimeError("Engine did not acknowledge quit request")
        print("  quit acknowledged", flush=True)
    else:
        print("[5/5] Engine left running (no quit requested).", flush=True)

    print("STREAM_PLAY_QUEUE_TEST_OK", flush=True)
    return 0


def main() -> int:
    parser = _build_parser()
    args = parser.parse_args()
    try:
        return asyncio.run(_run(args))
    except Exception as exc:
        print(f"ERROR: {exc}", file=sys.stderr, flush=True)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
