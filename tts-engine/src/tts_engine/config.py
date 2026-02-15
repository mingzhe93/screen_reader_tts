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
DEFAULT_KYUTAI_MODEL_NAME = "Verylicious/pocket-tts-ungated"
DEFAULT_KYUTAI_VOICE_PROMPT = "alba"
DEFAULT_KYUTAI_SAMPLE_RATE = 24_000
DEFAULT_WARMUP_ON_STARTUP = True
DEFAULT_WARMUP_TEXT = "Engine warmup sentence."
DEFAULT_WARMUP_LANGUAGE = "auto"


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
    kyutai_model_name: str = DEFAULT_KYUTAI_MODEL_NAME
    kyutai_voice_prompt: str = DEFAULT_KYUTAI_VOICE_PROMPT
    kyutai_sample_rate: int = DEFAULT_KYUTAI_SAMPLE_RATE
    warmup_on_startup: bool = DEFAULT_WARMUP_ON_STARTUP
    warmup_text: str = DEFAULT_WARMUP_TEXT
    warmup_language: str = DEFAULT_WARMUP_LANGUAGE

    @property
    def device(self) -> str:
        normalized = self.qwen_device_map.strip().lower()
        if normalized.startswith("cuda"):
            return "cuda"
        if normalized.startswith("cpu"):
            return "cpu"
        if normalized.startswith("mps"):
            return "mps"
        if ":" in normalized:
            return normalized.split(":", maxsplit=1)[0]
        return normalized or "unknown"


def resolve_data_dir(raw_path: str | None) -> Path:
    if raw_path:
        return _normalize_windows_extended_path(Path(raw_path).expanduser().resolve())
    return _normalize_windows_extended_path((Path.cwd() / ".data").resolve())


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


def load_env_bool(env_name: str, default: bool) -> bool:
    raw = os.getenv(env_name)
    if raw is None:
        return default
    normalized = raw.strip().lower()
    if normalized in {"1", "true", "yes", "y", "on"}:
        return True
    if normalized in {"0", "false", "no", "n", "off"}:
        return False
    return default


def _normalize_windows_extended_path(path: Path) -> Path:
    if os.name != "nt":
        return path

    raw = str(path)
    if raw.startswith("\\\\?\\UNC\\"):
        return Path("\\\\" + raw[len("\\\\?\\UNC\\"):])
    if raw.startswith("\\\\?\\"):
        return Path(raw[len("\\\\?\\"):])
    return path
