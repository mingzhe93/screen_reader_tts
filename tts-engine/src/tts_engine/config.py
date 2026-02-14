from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import os


DEFAULT_MODEL_ID = "qwen3-tts-12hz-0.6b-base"
DEFAULT_TOKEN_ENV = "SPEAK_SELECTION_ENGINE_TOKEN"
DEFAULT_SYNTH_BACKEND = "auto"
DEFAULT_QWEN_MODEL_NAME = "Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice"
DEFAULT_QWEN_DEVICE_MAP = "cuda:0"
DEFAULT_QWEN_DTYPE = "bfloat16"
DEFAULT_QWEN_ATTN = "flash_attention_2"
DEFAULT_QWEN_SPEAKER = "Ryan"


@dataclass(slots=True, frozen=True)
class EngineConfig:
    token: str
    host: str
    port: int
    data_dir: Path
    active_model_id: str = DEFAULT_MODEL_ID
    engine_version: str = "0.1.0"
    synth_backend: str = DEFAULT_SYNTH_BACKEND
    qwen_model_name: str = DEFAULT_QWEN_MODEL_NAME
    qwen_device_map: str = DEFAULT_QWEN_DEVICE_MAP
    qwen_dtype: str = DEFAULT_QWEN_DTYPE
    qwen_attn_implementation: str = DEFAULT_QWEN_ATTN
    qwen_default_speaker: str = DEFAULT_QWEN_SPEAKER

    @property
    def device(self) -> str:
        return "cuda"


def resolve_data_dir(raw_path: str | None) -> Path:
    if raw_path:
        return Path(raw_path).expanduser().resolve()
    return (Path.cwd() / ".data").resolve()


def load_token(explicit_token: str | None, token_env: str = DEFAULT_TOKEN_ENV) -> str | None:
    if explicit_token:
        candidate = explicit_token.strip()
        if candidate:
            return candidate

    token = os.getenv(token_env, "").strip()
    return token or None


def load_env_config_value(env_name: str, default: str) -> str:
    value = os.getenv(env_name, "").strip()
    return value or default
