from __future__ import annotations

from datetime import datetime
from typing import Any
from uuid import UUID

from pydantic import BaseModel, Field, field_validator, model_validator

from .constants import DEFAULT_VOICE_ID


class ErrorBody(BaseModel):
    code: str
    message: str
    details: dict[str, Any] = Field(default_factory=dict)


class ErrorResponse(BaseModel):
    error: ErrorBody


class HealthCapabilities(BaseModel):
    supports_voice_clone: bool
    supports_audio_chunk_stream: bool
    supports_true_streaming_inference: bool
    languages: list[str]


class WarmupStatus(BaseModel):
    status: str
    runs: int
    last_reason: str | None = None
    last_started_at: datetime | None = None
    last_completed_at: datetime | None = None
    last_duration_ms: int | None = None
    last_error: str | None = None


class RuntimeStatus(BaseModel):
    backend: str
    model_loaded: bool
    fallback_active: bool
    detail: str | None = None
    supports_default_voice: bool = True
    supports_cloned_voices: bool = False
    warmup: WarmupStatus


class HealthResponse(BaseModel):
    engine_version: str
    active_model_id: str
    device: str
    capabilities: HealthCapabilities
    runtime: RuntimeStatus


class VoiceSummary(BaseModel):
    voice_id: str
    display_name: str
    created_at: datetime
    tts_model_id: str
    language_hint: str | None = None


class ListVoicesResponse(BaseModel):
    voices: list[VoiceSummary]


class RefAudioInput(BaseModel):
    path: str | None = None
    wav_base64: str | None = None

    @model_validator(mode="after")
    def validate_any_input(self) -> "RefAudioInput":
        if not self.path and not self.wav_base64:
            raise ValueError("Either ref_audio.path or ref_audio.wav_base64 must be provided")
        return self


class CloneOptions(BaseModel):
    normalize_audio: bool = True


class CloneVoiceRequest(BaseModel):
    display_name: str = Field(min_length=1, max_length=80)
    ref_audio: RefAudioInput
    ref_text: str | None = None
    language: str | None = None
    options: CloneOptions = Field(default_factory=CloneOptions)


class CloneVoiceResponse(VoiceSummary):
    pass


class ChunkingSettings(BaseModel):
    max_chars: int = Field(default=400, ge=100, le=2000)


class SpeakSettings(BaseModel):
    rate: float = Field(default=1.0, ge=0.25, le=4.0)
    pitch: float = Field(default=1.0, ge=0.5, le=2.0)
    volume: float = Field(default=1.0, ge=0.0, le=2.0)
    chunking: ChunkingSettings = Field(default_factory=ChunkingSettings)


class SpeakRequest(BaseModel):
    voice_id: str = DEFAULT_VOICE_ID
    text: str
    language: str | None = None
    settings: SpeakSettings = Field(default_factory=SpeakSettings)

    @field_validator("voice_id", mode="before")
    @classmethod
    def normalize_voice_id(cls, value: object) -> str:
        if value is None:
            return DEFAULT_VOICE_ID
        if isinstance(value, int):
            value = str(value)
        elif isinstance(value, UUID):
            value = str(value)
        elif not isinstance(value, str):
            raise ValueError('voice_id must be "0" or a UUID string')

        normalized = value.strip()
        if not normalized:
            return DEFAULT_VOICE_ID
        if normalized == DEFAULT_VOICE_ID:
            return normalized
        try:
            UUID(normalized)
        except ValueError as exc:
            raise ValueError('voice_id must be "0" or a UUID string') from exc
        return normalized


class SpeakResponse(BaseModel):
    job_id: UUID
    ws_url: str


class CancelRequest(BaseModel):
    job_id: UUID


class CancelResponse(BaseModel):
    canceled: bool


class WarmupRequest(BaseModel):
    wait: bool = False
    force: bool = False
    reason: str | None = None


class WarmupResponse(BaseModel):
    accepted: bool
    warmup: WarmupStatus


class ActivateModelRequest(BaseModel):
    synth_backend: str | None = None
    active_model_id: str | None = None
    qwen_model_name: str | None = None
    qwen_device_map: str | None = None
    qwen_dtype: str | None = None
    qwen_attn_implementation: str | None = None
    qwen_default_speaker: str | None = None
    warmup_wait: bool = True
    warmup_force: bool = True
    reason: str | None = None

    @field_validator("synth_backend")
    @classmethod
    def validate_synth_backend(cls, value: str | None) -> str | None:
        if value is None:
            return value
        normalized = value.strip().lower()
        if not normalized:
            return None
        if normalized not in {"auto", "qwen", "mock"}:
            raise ValueError("synth_backend must be one of: auto, qwen, mock")
        return normalized


class ActivateModelResponse(BaseModel):
    reloaded: bool
    warmup_accepted: bool
    active_model_id: str
    runtime: RuntimeStatus
