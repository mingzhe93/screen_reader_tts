from __future__ import annotations

import base64
import binascii
from pathlib import Path
from uuid import UUID

from fastapi import APIRouter, Depends, FastAPI, Request, WebSocket
from starlette.websockets import WebSocketDisconnect, WebSocketState

from .auth import verify_http_request, verify_websocket
from .config import EngineConfig
from .constants import DEFAULT_VOICE_ID
from .errors import EngineError, install_exception_handlers
from .jobs import JobManager, TERMINAL_EVENT_TYPES
from .schemas import (
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
)
from .synth import create_synthesizer
from .voices import VoiceStore


def create_app(config: EngineConfig) -> FastAPI:
    app = FastAPI(title="VoiceReader Engine", version=config.engine_version)
    install_exception_handlers(app)

    synthesizer = create_synthesizer(config)
    runtime_model_id = _resolve_runtime_model_id(config, synthesizer.status.backend)
    voice_store = VoiceStore(config.data_dir, runtime_model_id)
    jobs = JobManager(synthesizer)

    app.state.config = config
    app.state.synthesizer = synthesizer
    app.state.voice_store = voice_store
    app.state.jobs = jobs

    def _require_http_auth(request: Request) -> None:
        verify_http_request(request, config.token)

    router = APIRouter(prefix="/v1", dependencies=[Depends(_require_http_auth)])

    @router.get("/health", response_model=HealthResponse)
    async def get_health() -> HealthResponse:
        return HealthResponse(
            engine_version=config.engine_version,
            active_model_id=runtime_model_id,
            device=config.device,
            capabilities=HealthCapabilities(
                supports_voice_clone=synthesizer.status.supports_voice_clone,
                supports_audio_chunk_stream=True,
                supports_true_streaming_inference=False,
                languages=["zh", "en", "ja", "ko", "de", "fr", "es", "pt", "ru", "it", "auto"],
            ),
            runtime=RuntimeStatus(
                backend=synthesizer.status.backend,
                model_loaded=synthesizer.status.model_loaded,
                fallback_active=synthesizer.status.fallback_active,
                detail=synthesizer.status.detail,
                supports_default_voice=synthesizer.status.supports_default_voice,
                supports_cloned_voices=synthesizer.status.supports_cloned_voices,
            ),
        )

    @router.get("/voices", response_model=ListVoicesResponse)
    async def list_voices() -> ListVoicesResponse:
        return ListVoicesResponse(voices=voice_store.list_voices())

    @router.post("/voices/clone", response_model=CloneVoiceResponse)
    async def clone_voice(payload: CloneVoiceRequest) -> CloneVoiceResponse:
        if not payload.ref_text or not payload.ref_text.strip():
            raise EngineError(
                code="TRANSCRIPT_REQUIRED",
                message="ref_text is required until ASR is enabled",
                status_code=400,
            )

        _validate_reference_audio(payload.ref_audio.path, payload.ref_audio.wav_base64)

        voice = voice_store.create_voice(
            display_name=payload.display_name.strip(),
            language_hint=payload.language,
            ref_text=payload.ref_text.strip(),
        )
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
        return {"deleted": True}

    @router.post("/speak", response_model=SpeakResponse)
    async def speak(payload: SpeakRequest, request: Request) -> SpeakResponse:
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
        )

        ws_scheme = "wss" if request.url.scheme == "https" else "ws"
        port = request.url.port or config.port
        ws_url = f"{ws_scheme}://127.0.0.1:{port}/v1/stream/{job.job_id}"
        return SpeakResponse(job_id=job.job_id, ws_url=ws_url)

    @router.post("/cancel", response_model=CancelResponse)
    async def cancel(payload: CancelRequest) -> CancelResponse:
        if not await jobs.cancel_job(payload.job_id):
            raise EngineError(
                code="JOB_NOT_FOUND",
                message=f"Job {payload.job_id} was not found",
                status_code=404,
            )
        return CancelResponse(canceled=True)

    @router.post("/quit")
    async def quit_engine() -> dict[str, bool]:
        request_shutdown = getattr(app.state, "request_shutdown", None)
        if callable(request_shutdown):
            request_shutdown()
        return {"quitting": True}

    app.include_router(router)

    @app.websocket("/v1/stream/{job_id}")
    async def stream_job(websocket: WebSocket, job_id: UUID) -> None:
        authorized, subprotocol = await verify_websocket(websocket, config.token)
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


def _validate_reference_audio(path: str | None, wav_base64: str | None) -> None:
    if path:
        audio_path = Path(path).expanduser()
        if not audio_path.exists() or not audio_path.is_file():
            raise EngineError(code="INVALID_AUDIO", message="Reference audio path is invalid", status_code=400)
        if audio_path.stat().st_size == 0:
            raise EngineError(code="INVALID_AUDIO", message="Reference audio file is empty", status_code=400)
        return

    if wav_base64:
        try:
            raw = base64.b64decode(wav_base64, validate=True)
        except (binascii.Error, ValueError) as exc:
            raise EngineError(code="INVALID_AUDIO", message="Invalid base64 audio payload", status_code=400) from exc
        if not raw:
            raise EngineError(code="INVALID_AUDIO", message="Audio payload is empty", status_code=400)
        return

    raise EngineError(code="INVALID_AUDIO", message="No reference audio provided", status_code=400)


def _resolve_runtime_model_id(config: EngineConfig, backend: str) -> str:
    if backend == "qwen_custom_voice":
        return config.qwen_model_name
    return config.active_model_id
