from __future__ import annotations

import asyncio
import base64
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any
from uuid import UUID, uuid4

from .chunking import split_text_into_chunks
from .synth import BaseSynthesizer


TERMINAL_EVENT_TYPES = {"JOB_DONE", "JOB_CANCELED", "JOB_ERROR"}


@dataclass(slots=True)
class JobState:
    job_id: UUID
    voice_id: str
    text: str
    language: str | None
    max_chars: int
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

    async def start_job(self, voice_id: str, text: str, max_chars: int, language: str | None) -> JobState:
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

    async def _run_job(self, job: JobState) -> None:
        try:
            await self._publish(
                job,
                {
                    "type": "JOB_STARTED",
                    "job_id": str(job.job_id),
                },
            )

            chunks = split_text_into_chunks(job.text, max_chars=job.max_chars)
            sequence = 1
            for chunk in chunks:
                if job.cancel_event.is_set():
                    await self._publish(
                        job,
                        {
                            "type": "JOB_CANCELED",
                            "job_id": str(job.job_id),
                        },
                        terminal=True,
                    )
                    return

                synthesized = await asyncio.to_thread(
                    self._synthesizer.synthesize_chunk,
                    chunk.text,
                    job.voice_id,
                    job.language,
                )
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
