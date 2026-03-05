from __future__ import annotations

import asyncio
import base64
from dataclasses import dataclass, field
from datetime import datetime, timezone
import os
from pathlib import Path
import shutil
import subprocess
import sys
from typing import Any
from uuid import UUID, uuid4

import numpy as np

from .chunking import split_text_into_chunks
from .synth import BaseSynthesizer, SynthesizedAudio


TERMINAL_EVENT_TYPES = {"JOB_DONE", "JOB_CANCELED", "JOB_ERROR"}
_LIBROSA_MODULE = None
_LIBROSA_IMPORT_ATTEMPTED = False
_SOX_PATH = None
_SOX_LOOKUP_ATTEMPTED = False


@dataclass(slots=True)
class JobState:
    job_id: UUID
    voice_id: str
    text: str
    language: str | None
    max_chars: int
    rate: float
    pitch: float
    volume: float
    created_at: datetime = field(default_factory=lambda: datetime.now(timezone.utc))
    cancel_event: asyncio.Event = field(default_factory=asyncio.Event)
    done_event: asyncio.Event = field(default_factory=asyncio.Event)
    history: list[dict[str, Any]] = field(default_factory=list)
    subscribers: set[asyncio.Queue[dict[str, Any] | None]] = field(default_factory=set)
    task: asyncio.Task[None] | None = None


class JobManager:
    def __init__(self, synthesizer: BaseSynthesizer) -> None:
        self._synthesizer = synthesizer
        self._jobs: dict[UUID, JobState] = {}
        self._active_job_id: UUID | None = None
        self._lock = asyncio.Lock()

    async def start_job(
        self,
        voice_id: str,
        text: str,
        max_chars: int,
        language: str | None,
        rate: float,
        pitch: float,
        volume: float,
    ) -> JobState:
        async with self._lock:
            if self._active_job_id is not None:
                active_job = self._jobs.get(self._active_job_id)
                if active_job and not active_job.done_event.is_set():
                    active_job.cancel_event.set()

            job = JobState(
                job_id=uuid4(),
                voice_id=voice_id,
                text=text,
                language=language,
                max_chars=max_chars,
                rate=rate,
                pitch=pitch,
                volume=volume,
            )
            self._jobs[job.job_id] = job
            self._active_job_id = job.job_id
            job.task = asyncio.create_task(self._run_job(job))
            self._prune_finished_jobs_locked(max_jobs=64)
            return job

    async def cancel_job(self, job_id: UUID) -> bool:
        async with self._lock:
            job = self._jobs.get(job_id)
            if not job:
                return False
            job.cancel_event.set()
            return True

    async def subscribe(
        self, job_id: UUID
    ) -> tuple[asyncio.Queue[dict[str, Any] | None], list[dict[str, Any]]]:
        async with self._lock:
            job = self._jobs.get(job_id)
            if not job:
                raise KeyError(job_id)
            queue: asyncio.Queue[dict[str, Any] | None] = asyncio.Queue(maxsize=128)
            job.subscribers.add(queue)
            return queue, list(job.history)

    async def unsubscribe(self, job_id: UUID, queue: asyncio.Queue[dict[str, Any] | None]) -> None:
        async with self._lock:
            job = self._jobs.get(job_id)
            if not job:
                return
            job.subscribers.discard(queue)

    async def has_active_job(self) -> bool:
        async with self._lock:
            if self._active_job_id is None:
                return False
            active_job = self._jobs.get(self._active_job_id)
            if active_job is None:
                return False
            return not active_job.done_event.is_set()

    async def _run_job(self, job: JobState) -> None:
        # Holds the Future for the next chunk's synthesis so we can pre-start it
        # before awaiting the current chunk's SoX processing.
        next_synth_future: asyncio.Future[SynthesizedAudio] | None = None

        async def _cancel_next_synth() -> None:
            nonlocal next_synth_future
            if next_synth_future is not None and not next_synth_future.done():
                next_synth_future.cancel()
                try:
                    await next_synth_future
                except (asyncio.CancelledError, Exception):
                    pass
                next_synth_future = None

        try:
            await self._publish(
                job,
                {
                    "type": "JOB_STARTED",
                    "job_id": str(job.job_id),
                },
            )

            chunks = split_text_into_chunks(job.text, max_chars=job.max_chars)
            if not chunks:
                await self._publish(
                    job,
                    {"type": "JOB_DONE", "job_id": str(job.job_id)},
                    terminal=True,
                )
                return

            sequence = 1
            loop = asyncio.get_running_loop()

            # Pre-submit synthesis of the first chunk so it starts running
            # immediately, before we enter the loop.
            next_synth_future = loop.run_in_executor(
                None,
                self._synthesizer.synthesize_chunk,
                chunks[0].text,
                job.voice_id,
                job.language,
            )

            for i, chunk in enumerate(chunks):
                if job.cancel_event.is_set():
                    await _cancel_next_synth()
                    await self._publish(
                        job,
                        {"type": "JOB_CANCELED", "job_id": str(job.job_id)},
                        terminal=True,
                    )
                    return

                # Await synthesis of chunk i (was already running in the thread pool).
                assert next_synth_future is not None
                synthesized = await next_synth_future
                next_synth_future = None

                if job.cancel_event.is_set():
                    await self._publish(
                        job,
                        {"type": "JOB_CANCELED", "job_id": str(job.job_id)},
                        terminal=True,
                    )
                    return

                # Immediately pre-submit synthesis of chunk i+1 to the thread pool
                # BEFORE awaiting SoX for chunk i.  synthesize_chunk uses C extensions
                # (GIL released during inference) and SoX runs as a subprocess, so both
                # threads execute truly in parallel — no concurrent model access because
                # chunk i synthesis has already completed above.
                if i + 1 < len(chunks):
                    next_synth_future = loop.run_in_executor(
                        None,
                        self._synthesizer.synthesize_chunk,
                        chunks[i + 1].text,
                        job.voice_id,
                        job.language,
                    )

                # Apply SoX/controls in the thread pool so the event loop stays
                # responsive and SoX runs concurrently with chunk i+1 synthesis.
                synthesized = await asyncio.to_thread(
                    _apply_playback_controls,
                    synthesized,
                    job.rate,
                    job.pitch,
                    job.volume,
                )

                if job.cancel_event.is_set():
                    await _cancel_next_synth()
                    await self._publish(
                        job,
                        {"type": "JOB_CANCELED", "job_id": str(job.job_id)},
                        terminal=True,
                    )
                    return

                event = {
                    "type": "AUDIO_CHUNK",
                    "job_id": str(job.job_id),
                    "seq": sequence,
                    "audio": {
                        "format": "pcm_s16le",
                        "sample_rate": synthesized.sample_rate,
                        "channels": synthesized.channels,
                        "data_base64": base64.b64encode(synthesized.pcm_s16le).decode("ascii"),
                    },
                    "text_range": {
                        "chunk_index": chunk.chunk_index,
                        "start_char": chunk.start_char,
                        "end_char": chunk.end_char,
                    },
                }
                await self._publish(job, event)
                sequence += 1

                # Yield back to the event loop between chunks.
                await asyncio.sleep(0)

            if job.cancel_event.is_set():
                await self._publish(
                    job,
                    {
                        "type": "JOB_CANCELED",
                        "job_id": str(job.job_id),
                    },
                    terminal=True,
                )
            else:
                await self._publish(
                    job,
                    {
                        "type": "JOB_DONE",
                        "job_id": str(job.job_id),
                    },
                    terminal=True,
                )
        except asyncio.CancelledError:
            await _cancel_next_synth()
            if not self._has_terminal_event(job):
                await self._publish(
                    job,
                    {
                        "type": "JOB_CANCELED",
                        "job_id": str(job.job_id),
                    },
                    terminal=True,
                )
            raise
        except Exception as exc:  # pragma: no cover - fallback guard
            await _cancel_next_synth()
            await self._publish(
                job,
                {
                    "type": "JOB_ERROR",
                    "job_id": str(job.job_id),
                    "error": {
                        "code": "INFERENCE_FAILED",
                        "message": str(exc),
                        "details": {},
                    },
                },
                terminal=True,
            )
        finally:
            job.done_event.set()
            async with self._lock:
                if self._active_job_id == job.job_id:
                    self._active_job_id = None

    async def _publish(self, job: JobState, event: dict[str, Any], terminal: bool = False) -> None:
        job.history.append(event)

        stale_queues: list[asyncio.Queue[dict[str, Any] | None]] = []
        for queue in list(job.subscribers):
            try:
                queue.put_nowait(event)
            except asyncio.QueueFull:
                stale_queues.append(queue)
        for queue in stale_queues:
            job.subscribers.discard(queue)

        if terminal:
            for queue in list(job.subscribers):
                try:
                    queue.put_nowait(None)
                except asyncio.QueueFull:
                    job.subscribers.discard(queue)

    def _prune_finished_jobs_locked(self, max_jobs: int) -> None:
        if len(self._jobs) <= max_jobs:
            return
        finished_jobs = [job for job in self._jobs.values() if job.done_event.is_set()]
        finished_jobs.sort(key=lambda item: item.created_at)
        for job in finished_jobs:
            if len(self._jobs) <= max_jobs:
                break
            self._jobs.pop(job.job_id, None)

    @staticmethod
    def _has_terminal_event(job: JobState) -> bool:
        if not job.history:
            return False
        return job.history[-1].get("type") in TERMINAL_EVENT_TYPES


def _apply_playback_controls(
    audio: SynthesizedAudio,
    rate: float,
    pitch: float,
    volume: float,
) -> SynthesizedAudio:
    # Keep a fast path for default settings.
    if rate == 1.0 and pitch == 1.0 and volume == 1.0:
        return audio

    samples = np.frombuffer(audio.pcm_s16le, dtype=np.int16).astype(np.float32)
    if samples.size == 0:
        return audio

    # Playback-speed control (rate): time-stretch to preserve perceived pitch.
    if rate != 1.0:
        samples = _time_stretch_preserve_pitch(samples, rate=rate, sample_rate=audio.sample_rate)

    # Reserved for future model-aware pitch handling.
    _ = pitch

    if volume != 1.0:
        samples *= volume

    np.clip(samples, -32768.0, 32767.0, out=samples)
    return SynthesizedAudio(
        pcm_s16le=samples.astype(np.int16).tobytes(),
        sample_rate=audio.sample_rate,
        channels=audio.channels,
    )


def _resample_linear(samples: np.ndarray, target_len: int) -> np.ndarray:
    if target_len <= 1:
        return np.asarray([samples[0]], dtype=np.float32)
    if samples.shape[0] <= 1:
        return np.full((target_len,), float(samples[0]), dtype=np.float32)
    if target_len == samples.shape[0]:
        return samples

    src_x = np.linspace(0.0, float(samples.shape[0] - 1), num=samples.shape[0], dtype=np.float32)
    dst_x = np.linspace(0.0, float(samples.shape[0] - 1), num=target_len, dtype=np.float32)
    return np.interp(dst_x, src_x, samples).astype(np.float32)


def _time_stretch_preserve_pitch(samples: np.ndarray, rate: float, sample_rate: int) -> np.ndarray:
    if rate <= 0.0:
        return samples

    target_len = max(1, int(round(samples.shape[0] / rate)))
    if samples.shape[0] <= 8:
        return _resample_linear(samples, target_len)

    sox_stretched = _time_stretch_with_sox(samples, rate=rate, sample_rate=sample_rate)
    if sox_stretched is not None:
        return sox_stretched

    librosa = _load_librosa()
    if librosa is None:
        return _resample_linear(samples, target_len)

    normalized = np.asarray(samples, dtype=np.float32) / 32768.0
    try:
        stretched = librosa.effects.time_stretch(normalized, rate=rate)
    except Exception:
        return _resample_linear(samples, target_len)

    if stretched is None:
        return _resample_linear(samples, target_len)

    stretched = np.asarray(stretched, dtype=np.float32).reshape(-1)
    if stretched.size == 0:
        return _resample_linear(samples, target_len)
    return stretched * 32768.0


def _time_stretch_with_sox(
    samples: np.ndarray,
    rate: float,
    sample_rate: int,
) -> np.ndarray | None:
    sox_path = _resolve_sox_path()
    if sox_path is None:
        return None
    if sample_rate <= 0:
        return None

    pcm_int16 = np.asarray(np.clip(samples, -32768.0, 32767.0), dtype=np.int16)
    factors = _decompose_tempo_factors(rate)
    if not factors:
        return pcm_int16.astype(np.float32)

    command = [
        sox_path,
        "-q",
        "-t",
        "raw",
        "-r",
        str(sample_rate),
        "-e",
        "signed-integer",
        "-b",
        "16",
        "-c",
        "1",
        "-L",
        "-",
        "-t",
        "raw",
        "-e",
        "signed-integer",
        "-b",
        "16",
        "-c",
        "1",
        "-L",
        "-",
    ]
    for factor in factors:
        command.extend(["tempo", f"{factor:.6f}"])

    try:
        result = subprocess.run(
            command,
            input=pcm_int16.tobytes(),
            capture_output=True,
            check=True,
        )
    except Exception:
        return None

    if not result.stdout:
        return None
    stretched_int16 = np.frombuffer(result.stdout, dtype=np.int16)
    if stretched_int16.size == 0:
        return None
    return stretched_int16.astype(np.float32)


def _decompose_tempo_factors(rate: float) -> list[float]:
    if rate <= 0.0:
        return []
    remaining = float(rate)
    factors: list[float] = []

    while remaining > 2.0:
        factors.append(2.0)
        remaining /= 2.0
    while remaining < 0.5:
        factors.append(0.5)
        remaining /= 0.5

    factors.append(remaining)
    return factors


def _load_librosa():
    global _LIBROSA_MODULE, _LIBROSA_IMPORT_ATTEMPTED
    if _LIBROSA_IMPORT_ATTEMPTED:
        return _LIBROSA_MODULE

    _LIBROSA_IMPORT_ATTEMPTED = True
    try:
        import librosa
    except Exception:
        _LIBROSA_MODULE = None
    else:
        _LIBROSA_MODULE = librosa
    return _LIBROSA_MODULE


def _resolve_sox_path() -> str | None:
    global _SOX_PATH, _SOX_LOOKUP_ATTEMPTED
    if _SOX_LOOKUP_ATTEMPTED:
        return _SOX_PATH

    _SOX_LOOKUP_ATTEMPTED = True
    env_override = os.getenv("VOICEREADER_SOX_PATH", "").strip()
    if env_override:
        candidate = Path(env_override)
        if candidate.exists():
            _SOX_PATH = str(candidate.resolve())
            return _SOX_PATH

    bundled = _find_bundled_sox_near_runtime()
    if bundled is not None:
        _SOX_PATH = bundled
        return _SOX_PATH

    _SOX_PATH = shutil.which("sox")
    if _SOX_PATH:
        return _SOX_PATH

    _SOX_PATH = _find_sox_in_windows_winget_location()
    return _SOX_PATH


def _find_bundled_sox_near_runtime() -> str | None:
    binary_name = "sox.exe" if os.name == "nt" else "sox"
    roots: list[Path] = []

    try:
        exe_parent = Path(sys.executable).resolve().parent
        roots.append(exe_parent)
        roots.append(exe_parent.parent)
        roots.append(exe_parent.parent.parent)
    except Exception:
        pass
    try:
        roots.append(Path.cwd())
    except Exception:
        pass

    seen: set[Path] = set()
    deduped_roots: list[Path] = []
    for root in roots:
        if root in seen:
            continue
        seen.add(root)
        deduped_roots.append(root)

    for root in deduped_roots:
        candidates = [
            root / "binaries" / "sox" / binary_name,
            root / "resources" / "binaries" / "sox" / binary_name,
            root / "binaries" / binary_name,
            root / "resources" / "binaries" / binary_name,
            root / "sox" / binary_name,
            root / "resources" / "sox" / binary_name,
            root / binary_name,
        ]
        for candidate in candidates:
            if candidate.exists():
                return str(candidate.resolve())

    return None


def _find_sox_in_windows_winget_location() -> str | None:
    if os.name != "nt":
        return None

    local_app_data = os.getenv("LOCALAPPDATA", "").strip()
    if not local_app_data:
        return None

    root = Path(local_app_data) / "Microsoft" / "WinGet" / "Packages"
    if not root.exists():
        return None

    candidates = sorted(root.glob("ChrisBagwell.SoX_*"))
    for candidate in candidates:
        # Common layout: ...\sox-14.4.2\sox.exe
        nested_bins = sorted(candidate.glob("sox-*/sox.exe"))
        for binary in nested_bins:
            if binary.exists():
                return str(binary.resolve())

        direct_binary = candidate / "sox.exe"
        if direct_binary.exists():
            return str(direct_binary.resolve())

    return None
