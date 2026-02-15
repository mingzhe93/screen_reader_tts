from __future__ import annotations

import asyncio
import base64
import binascii
from dataclasses import replace
from datetime import datetime, timezone
from pathlib import Path
import time
from uuid import UUID

from fastapi import APIRouter, Depends, FastAPI, Request, WebSocket
from starlette.websockets import WebSocketDisconnect, WebSocketState

from .auth import verify_http_request, verify_websocket
from .config import EngineConfig
from .constants import DEFAULT_VOICE_ID
from .errors import EngineError, install_exception_handlers
from .jobs import JobManager, TERMINAL_EVENT_TYPES
from .model_store import (
    KYUTAI_POCKET_MODEL_REPO,
    QWEN_BASE_MODEL_REPO,
    QWEN_CUSTOM_MODEL_REPO,
    configure_hf_cache,
    download_repo_to_local_dir,
)
from .schemas import (
    ActivateModelRequest,
    ActivateModelResponse,
    CancelRequest,
    CancelResponse,
    CloneVoiceRequest,
    CloneVoiceResponse,
    HealthCapabilities,
    HealthResponse,
    ListVoicesResponse,
    RuntimeStatus,
    SpeakRequest,
    SpeakResponse,
    PrefetchModelsRequest,
    PrefetchModelsResponse,
    VoiceSummary,
    UpdateVoiceRequest,
    WarmupRequest,
    WarmupResponse,
    WarmupStatus,
)
from .synth import create_synthesizer
from .voices import VoiceStore


def create_app(config: EngineConfig) -> FastAPI:
    engine_config = config
    app = FastAPI(title="VoiceReader Engine", version=engine_config.engine_version)
    install_exception_handlers(app)

    synthesizer = create_synthesizer(engine_config)
    runtime_model_id = _resolve_runtime_model_id(engine_config, synthesizer.status.backend)
    voice_store = VoiceStore(engine_config.data_dir, runtime_model_id)
    jobs = JobManager(synthesizer)

    app.state.config = engine_config
    app.state.synthesizer = synthesizer
    app.state.voice_store = voice_store
    app.state.jobs = jobs
    app.state.runtime_model_id = runtime_model_id
    app.state.warmup_state = _new_warmup_state()
    app.state.warmup_task = None
    runtime_lock = asyncio.Lock()

    def _warmup_snapshot() -> WarmupStatus:
        state = app.state.warmup_state
        return WarmupStatus(
            status=state["status"],
            runs=state["runs"],
            last_reason=state["last_reason"],
            last_started_at=state["last_started_at"],
            last_completed_at=state["last_completed_at"],
            last_duration_ms=state["last_duration_ms"],
            last_error=state["last_error"],
        )

    def _runtime_snapshot() -> RuntimeStatus:
        return RuntimeStatus(
            backend=synthesizer.status.backend,
            model_loaded=synthesizer.status.model_loaded,
            fallback_active=synthesizer.status.fallback_active,
            detail=synthesizer.status.detail,
            supports_default_voice=synthesizer.status.supports_default_voice,
            supports_cloned_voices=synthesizer.status.supports_cloned_voices,
            warmup=_warmup_snapshot(),
        )

    def _sync_runtime_state() -> None:
        app.state.config = engine_config
        app.state.synthesizer = synthesizer
        app.state.voice_store = voice_store
        app.state.jobs = jobs
        app.state.runtime_model_id = runtime_model_id

    async def _run_warmup(reason: str) -> None:
        state = app.state.warmup_state
        state["status"] = "running"
        state["last_reason"] = reason
        state["last_started_at"] = datetime.now(timezone.utc)
        state["last_error"] = None

        started = time.perf_counter()
        try:
            await asyncio.to_thread(synthesizer.warmup, engine_config.warmup_text, engine_config.warmup_language)
            duration_ms = int((time.perf_counter() - started) * 1000.0)
            state["status"] = "ready"
            state["runs"] += 1
            state["last_duration_ms"] = duration_ms
            state["last_completed_at"] = datetime.now(timezone.utc)
            state["last_error"] = None
        except Exception as exc:
            duration_ms = int((time.perf_counter() - started) * 1000.0)
            state["status"] = "error"
            state["runs"] += 1
            state["last_duration_ms"] = duration_ms
            state["last_completed_at"] = datetime.now(timezone.utc)
            state["last_error"] = str(exc)
        finally:
            app.state.warmup_task = None

    async def trigger_warmup(wait: bool, force: bool, reason: str) -> bool:
        current_task = app.state.warmup_task
        if current_task is not None and not current_task.done():
            if wait:
                await current_task
            return False

        current_status = app.state.warmup_state["status"]
        should_start = force or current_status in {"not_started", "error"}
        if not should_start:
            return False

        task = asyncio.create_task(_run_warmup(reason=reason))
        app.state.warmup_task = task
        if wait:
            await task
        return True

    app.state.trigger_warmup = trigger_warmup

    @app.on_event("startup")
    async def _startup_warmup() -> None:
        if engine_config.warmup_on_startup:
            await trigger_warmup(wait=False, force=False, reason="startup")

    def _require_http_auth(request: Request) -> None:
        verify_http_request(request, engine_config.token)

    router = APIRouter(prefix="/v1", dependencies=[Depends(_require_http_auth)])

    @router.get("/health", response_model=HealthResponse)
    async def get_health() -> HealthResponse:
        return HealthResponse(
            engine_version=engine_config.engine_version,
            active_model_id=runtime_model_id,
            device=_resolve_runtime_device(engine_config, synthesizer.status.backend),
            capabilities=HealthCapabilities(
                supports_voice_clone=synthesizer.status.supports_voice_clone,
                supports_audio_chunk_stream=True,
                supports_true_streaming_inference=False,
                languages=_resolve_runtime_languages(synthesizer.status.backend),
            ),
            runtime=_runtime_snapshot(),
        )

    @router.get("/voices", response_model=ListVoicesResponse)
    async def list_voices() -> ListVoicesResponse:
        return ListVoicesResponse(voices=voice_store.list_voices())

    @router.post("/voices/clone", response_model=CloneVoiceResponse)
    async def clone_voice(payload: CloneVoiceRequest) -> CloneVoiceResponse:
        if not synthesizer.status.supports_voice_clone:
            raise EngineError(
                code="MODEL_NOT_READY",
                message=(
                    f'Configured synthesis backend "{synthesizer.status.backend}" '
                    "does not support voice cloning"
                ),
                status_code=409,
            )

        voice = voice_store.create_voice(
            display_name=payload.display_name.strip(),
            language_hint=payload.language,
            ref_text=(payload.ref_text.strip() if payload.ref_text and payload.ref_text.strip() else None),
            description=payload.description,
        )
        try:
            reference_source = _prepare_reference_audio_source(
                path=payload.ref_audio.path,
                wav_base64=payload.ref_audio.wav_base64,
                voice_store=voice_store,
                voice_id=voice.voice_id,
            )
            await asyncio.to_thread(
                synthesizer.prepare_cloned_voice,
                voice.voice_id,
                reference_source,
            )
        except EngineError:
            voice_store.delete_voice(UUID(voice.voice_id))
            raise
        except Exception as exc:
            voice_store.delete_voice(UUID(voice.voice_id))
            raise EngineError(
                code="VOICE_CLONE_FAILED",
                message=f"Failed to create cloned voice: {exc}",
                status_code=400,
            ) from exc
        return CloneVoiceResponse.model_validate(voice.model_dump())

    @router.delete("/voices/{voice_id}")
    async def delete_voice(voice_id: str) -> dict[str, bool]:
        normalized_voice_id = voice_id.strip()
        if normalized_voice_id == DEFAULT_VOICE_ID:
            raise EngineError(
                code="FORBIDDEN",
                message='Built-in default voice "0" cannot be deleted',
                status_code=403,
            )
        try:
            cloned_voice_id = UUID(normalized_voice_id)
        except ValueError as exc:
            raise EngineError(
                code="VOICE_NOT_FOUND",
                message=f"Voice {normalized_voice_id} was not found",
                status_code=404,
            ) from exc

        if not voice_store.delete_voice(cloned_voice_id):
            raise EngineError(
                code="VOICE_NOT_FOUND",
                message=f"Voice {normalized_voice_id} was not found",
                status_code=404,
            )
        synthesizer.forget_voice(str(cloned_voice_id))
        return {"deleted": True}

    @router.patch("/voices/{voice_id}", response_model=VoiceSummary)
    async def update_voice(voice_id: str, payload: UpdateVoiceRequest) -> VoiceSummary:
        normalized_voice_id = voice_id.strip()
        if normalized_voice_id == DEFAULT_VOICE_ID:
            raise EngineError(
                code="FORBIDDEN",
                message='Built-in default voice "0" cannot be edited',
                status_code=403,
            )
        try:
            cloned_voice_id = UUID(normalized_voice_id)
        except ValueError as exc:
            raise EngineError(
                code="VOICE_NOT_FOUND",
                message=f"Voice {normalized_voice_id} was not found",
                status_code=404,
            ) from exc

        updated = voice_store.update_voice(
            cloned_voice_id,
            display_name=payload.display_name,
            language_hint=payload.language,
            description=payload.description,
            fields_to_update=set(payload.model_fields_set),
        )
        if updated is None:
            raise EngineError(
                code="VOICE_NOT_FOUND",
                message=f"Voice {normalized_voice_id} was not found",
                status_code=404,
            )

        return updated

    @router.post("/speak", response_model=SpeakResponse)
    async def speak(payload: SpeakRequest, request: Request) -> SpeakResponse:
        async with runtime_lock:
            text = payload.text.strip()
            if not text:
                raise EngineError(code="EMPTY_TEXT", message="Text must not be empty", status_code=400)

            if not voice_store.voice_exists(payload.voice_id):
                raise EngineError(
                    code="VOICE_NOT_FOUND",
                    message=f"Voice {payload.voice_id} was not found",
                    status_code=404,
                )
            if not synthesizer.supports_voice_id(payload.voice_id):
                raise EngineError(
                    code="MODEL_NOT_READY",
                    message=(
                        f'Configured synthesis backend "{synthesizer.status.backend}" '
                        f'does not support voice_id "{payload.voice_id}" yet'
                    ),
                    status_code=409,
                )

            job = await jobs.start_job(
                voice_id=payload.voice_id,
                text=text,
                max_chars=payload.settings.chunking.max_chars,
                language=payload.language,
                rate=payload.settings.rate,
                pitch=payload.settings.pitch,
                volume=payload.settings.volume,
            )

            ws_scheme = "wss" if request.url.scheme == "https" else "ws"
            port = request.url.port or engine_config.port
            ws_url = f"{ws_scheme}://127.0.0.1:{port}/v1/stream/{job.job_id}"
            return SpeakResponse(job_id=job.job_id, ws_url=ws_url)

    @router.post("/cancel", response_model=CancelResponse)
    async def cancel(payload: CancelRequest) -> CancelResponse:
        async with runtime_lock:
            if not await jobs.cancel_job(payload.job_id):
                raise EngineError(
                    code="JOB_NOT_FOUND",
                    message=f"Job {payload.job_id} was not found",
                    status_code=404,
                )
            return CancelResponse(canceled=True)

    @router.post("/models/activate", response_model=ActivateModelResponse)
    async def activate_model(payload: ActivateModelRequest | None = None) -> ActivateModelResponse:
        nonlocal engine_config, synthesizer, runtime_model_id, voice_store, jobs
        request_payload = payload or ActivateModelRequest()
        async with runtime_lock:
            if await jobs.has_active_job():
                raise EngineError(
                    code="JOB_IN_PROGRESS",
                    message="Cannot activate a model while a speak job is running",
                    status_code=409,
                )

            current_warmup_task = app.state.warmup_task
            if current_warmup_task is not None and not current_warmup_task.done():
                await current_warmup_task

            next_config = replace(
                engine_config,
                synth_backend=request_payload.synth_backend or engine_config.synth_backend,
                active_model_id=_coalesce_str(request_payload.active_model_id, engine_config.active_model_id),
                qwen_model_name=_coalesce_str(request_payload.qwen_model_name, engine_config.qwen_model_name),
                qwen_device_map=_coalesce_str(request_payload.qwen_device_map, engine_config.qwen_device_map),
                qwen_dtype=_coalesce_str(request_payload.qwen_dtype, engine_config.qwen_dtype),
                qwen_attn_implementation=_coalesce_str(
                    request_payload.qwen_attn_implementation,
                    engine_config.qwen_attn_implementation,
                ),
                qwen_default_speaker=_coalesce_str(
                    request_payload.qwen_default_speaker,
                    engine_config.qwen_default_speaker,
                ),
                kyutai_model_name=_coalesce_str(
                    request_payload.kyutai_model_name,
                    engine_config.kyutai_model_name,
                ),
                kyutai_voice_prompt=_coalesce_str(
                    request_payload.kyutai_voice_prompt,
                    engine_config.kyutai_voice_prompt,
                ),
                kyutai_sample_rate=(
                    int(request_payload.kyutai_sample_rate)
                    if request_payload.kyutai_sample_rate is not None
                    else engine_config.kyutai_sample_rate
                ),
            )

            try:
                next_synthesizer = await asyncio.to_thread(create_synthesizer, next_config)
            except Exception as exc:
                raise EngineError(
                    code="MODEL_NOT_READY",
                    message=f"Failed to activate model/backend: {exc}",
                    status_code=409,
                ) from exc

            engine_config = next_config
            synthesizer = next_synthesizer
            runtime_model_id = _resolve_runtime_model_id(engine_config, synthesizer.status.backend)
            voice_store = VoiceStore(engine_config.data_dir, runtime_model_id)
            jobs = JobManager(synthesizer)
            _sync_runtime_state()

            app.state.warmup_state = _new_warmup_state()
            app.state.warmup_task = None
            warmup_accepted = await trigger_warmup(
                wait=request_payload.warmup_wait,
                force=request_payload.warmup_force,
                reason=(request_payload.reason or "model_activate"),
            )

            return ActivateModelResponse(
                reloaded=True,
                warmup_accepted=warmup_accepted,
                active_model_id=runtime_model_id,
                runtime=_runtime_snapshot(),
            )

    @router.post("/models/prefetch", response_model=PrefetchModelsResponse)
    async def prefetch_models(payload: PrefetchModelsRequest | None = None) -> PrefetchModelsResponse:
        request_payload = payload or PrefetchModelsRequest()
        repos = _resolve_prefetch_repos(request_payload.mode)
        cache_paths = configure_hf_cache(engine_config.data_dir)

        saved_to: dict[str, str] = {}
        for repo_id in repos:
            local_dir = await asyncio.to_thread(download_repo_to_local_dir, repo_id, engine_config.data_dir)
            saved_to[repo_id] = str(local_dir)

        return PrefetchModelsResponse(
            mode=request_payload.mode,
            downloaded=repos,
            saved_to=saved_to,
            data_dir=str(engine_config.data_dir),
            models_dir=str((engine_config.data_dir / "models").resolve()),
            hf_cache_dir=str(cache_paths.cache_root),
        )

    @router.post("/warmup", response_model=WarmupResponse)
    async def warmup(payload: WarmupRequest | None = None) -> WarmupResponse:
        request_payload = payload or WarmupRequest()
        accepted = await trigger_warmup(
            wait=request_payload.wait,
            force=request_payload.force,
            reason=(request_payload.reason or "api"),
        )
        return WarmupResponse(accepted=accepted, warmup=_warmup_snapshot())

    @router.post("/quit")
    async def quit_engine() -> dict[str, bool]:
        request_shutdown = getattr(app.state, "request_shutdown", None)
        if callable(request_shutdown):
            request_shutdown()
        return {"quitting": True}

    app.include_router(router)

    @app.websocket("/v1/stream/{job_id}")
    async def stream_job(websocket: WebSocket, job_id: UUID) -> None:
        authorized, subprotocol = await verify_websocket(websocket, engine_config.token)
        if not authorized:
            await websocket.close(code=4401)
            return

        queue = None
        try:
            queue, history = await jobs.subscribe(job_id)
        except KeyError:
            await websocket.close(code=4404)
            return

        await websocket.accept(subprotocol=subprotocol)

        try:
            for event in history:
                await websocket.send_json(event)
                if event.get("type") in TERMINAL_EVENT_TYPES:
                    return

            while True:
                event = await queue.get()
                if event is None:
                    return
                await websocket.send_json(event)
                if event.get("type") in TERMINAL_EVENT_TYPES:
                    return
        except WebSocketDisconnect:
            return
        finally:
            if queue is not None:
                await jobs.unsubscribe(job_id, queue)
            if websocket.application_state != WebSocketState.DISCONNECTED:
                await websocket.close()

    return app


def _new_warmup_state() -> dict[str, object]:
    return {
        "status": "not_started",
        "runs": 0,
        "last_reason": None,
        "last_started_at": None,
        "last_completed_at": None,
        "last_duration_ms": None,
        "last_error": None,
    }


def _coalesce_str(candidate: str | None, fallback: str) -> str:
    if candidate is None:
        return fallback
    normalized = candidate.strip()
    return normalized or fallback


def _prepare_reference_audio_source(
    path: str | None,
    wav_base64: str | None,
    voice_store: VoiceStore,
    voice_id: str,
) -> str:
    if path:
        audio_path = Path(path).expanduser()
        if not audio_path.exists() or not audio_path.is_file():
            raise EngineError(code="INVALID_AUDIO", message="Reference audio path is invalid", status_code=400)
        if audio_path.stat().st_size == 0:
            raise EngineError(code="INVALID_AUDIO", message="Reference audio file is empty", status_code=400)
        return str(audio_path.resolve())

    if wav_base64:
        try:
            raw = base64.b64decode(wav_base64, validate=True)
        except (binascii.Error, ValueError) as exc:
            raise EngineError(code="INVALID_AUDIO", message="Invalid base64 audio payload", status_code=400) from exc
        if not raw:
            raise EngineError(code="INVALID_AUDIO", message="Audio payload is empty", status_code=400)
        destination = voice_store.reference_audio_path(voice_id, suffix=".wav")
        try:
            destination.write_bytes(raw)
        except OSError as exc:
            raise EngineError(code="INVALID_AUDIO", message="Failed to persist base64 audio payload", status_code=400) from exc
        return str(destination.resolve())

    raise EngineError(code="INVALID_AUDIO", message="No reference audio provided", status_code=400)


def _resolve_runtime_model_id(config: EngineConfig, backend: str) -> str:
    if backend == "qwen_custom_voice":
        return config.qwen_model_name
    if backend == "kyutai_pocket_tts":
        return config.kyutai_model_name
    return config.active_model_id


def _resolve_runtime_device(config: EngineConfig, backend: str) -> str:
    if backend == "qwen_custom_voice":
        return config.device
    if backend == "kyutai_pocket_tts":
        return "cpu"
    return "cpu"


def _resolve_runtime_languages(backend: str) -> list[str]:
    if backend == "kyutai_pocket_tts":
        # Pocket TTS currently supports English generation in this app integration.
        return ["en"]
    if backend == "qwen_custom_voice":
        return ["zh", "en", "ja", "ko", "de", "fr", "es", "pt", "ru", "it", "auto"]
    # Mock fallback stays permissive for API smoke testing.
    return ["zh", "en", "ja", "ko", "de", "fr", "es", "pt", "ru", "it", "auto"]


def _resolve_prefetch_repos(mode: str) -> list[str]:
    if mode == "qwen_custom":
        return [QWEN_CUSTOM_MODEL_REPO]
    if mode == "qwen_base":
        return [QWEN_BASE_MODEL_REPO]
    if mode == "all":
        return [QWEN_CUSTOM_MODEL_REPO, QWEN_BASE_MODEL_REPO, KYUTAI_POCKET_MODEL_REPO]
    # default qwen_all
    return [QWEN_CUSTOM_MODEL_REPO, QWEN_BASE_MODEL_REPO]
