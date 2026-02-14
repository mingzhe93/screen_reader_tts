from __future__ import annotations

from array import array
from dataclasses import dataclass
import math
from typing import Protocol

from .config import EngineConfig
from .constants import DEFAULT_VOICE_ID


_QWEN_LANGUAGE_MAP = {
    "auto": "Auto",
    "zh": "Chinese",
    "en": "English",
    "ja": "Japanese",
    "ko": "Korean",
    "de": "German",
    "fr": "French",
    "es": "Spanish",
    "pt": "Portuguese",
    "ru": "Russian",
    "it": "Italian",
}


@dataclass(frozen=True, slots=True)
class SynthesizedAudio:
    pcm_s16le: bytes
    sample_rate: int
    channels: int = 1


@dataclass(frozen=True, slots=True)
class SynthBackendStatus:
    backend: str
    model_loaded: bool
    fallback_active: bool
    detail: str | None = None
    supports_voice_clone: bool = False
    supports_default_voice: bool = True
    supports_cloned_voices: bool = False


class BaseSynthesizer(Protocol):
    status: SynthBackendStatus

    def supports_voice_id(self, voice_id: str) -> bool:
        raise NotImplementedError

    def synthesize_chunk(self, chunk_text: str, voice_id: str, language: str | None = None) -> SynthesizedAudio:
        raise NotImplementedError


class MockSynthesizer:
    """Fallback backend that produces deterministic PCM tones per text chunk."""

    def __init__(self, sample_rate: int = 24_000, detail: str | None = None, fallback_active: bool = False) -> None:
        self._sample_rate = sample_rate
        self.status = SynthBackendStatus(
            backend="mock",
            model_loaded=True,
            fallback_active=fallback_active,
            detail=detail,
            supports_voice_clone=True,
            supports_default_voice=True,
            supports_cloned_voices=True,
        )

    def supports_voice_id(self, voice_id: str) -> bool:
        return True

    def synthesize_chunk(self, chunk_text: str, voice_id: str, language: str | None = None) -> SynthesizedAudio:
        duration_seconds = max(0.18, min(1.2, len(chunk_text) / 90.0))
        sample_count = int(duration_seconds * self._sample_rate)
        frequency_hz = 220.0
        amplitude = int(32767 * 0.18)

        waveform = array("h")
        for idx in range(sample_count):
            sample = int(
                amplitude
                * math.sin(2.0 * math.pi * frequency_hz * (idx / self._sample_rate))
            )
            waveform.append(sample)

        return SynthesizedAudio(pcm_s16le=waveform.tobytes(), sample_rate=self._sample_rate, channels=1)


class QwenCustomVoiceSynthesizer:
    """Qwen custom-voice backend (no-clone default voice path)."""

    def __init__(self, config: EngineConfig) -> None:
        try:
            import numpy as np
            import torch
            from qwen_tts import Qwen3TTSModel
        except Exception as exc:  # pragma: no cover - runtime-dependent import
            raise RuntimeError(f"Qwen dependencies are unavailable: {exc}") from exc

        self._np = np
        self._torch = torch
        self._model_name = config.qwen_model_name
        self._default_speaker = config.qwen_default_speaker

        dtype = self._resolve_torch_dtype(config.qwen_dtype)
        device_map = config.qwen_device_map
        attn_impl = config.qwen_attn_implementation
        detail_note = f"model={self._model_name}, device_map={device_map}, dtype={config.qwen_dtype}, attn={attn_impl}"

        if self._is_cuda_device_map(device_map) and not torch.cuda.is_available():
            raise RuntimeError(
                "Qwen backend requested CUDA, but current torch build has no CUDA runtime. "
                "Install a CUDA-enabled torch wheel, or set "
                "VOICEREADER_QWEN_DEVICE_MAP=cpu and VOICEREADER_QWEN_DTYPE=float32 for CPU testing."
            )

        try:
            self._model = Qwen3TTSModel.from_pretrained(
                self._model_name,
                device_map=device_map,
                dtype=dtype,
                attn_implementation=attn_impl,
            )
        except Exception as exc:
            # Common fallback when flash attention isn't available on Windows.
            if attn_impl == "flash_attention_2":
                fallback_attn = "sdpa"
                self._model = Qwen3TTSModel.from_pretrained(
                    self._model_name,
                    device_map=device_map,
                    dtype=dtype,
                    attn_implementation=fallback_attn,
                )
                detail_note = (
                    f"{detail_note}; flash_attention_2 failed ({exc}); "
                    f"using attn={fallback_attn}"
                )
            else:  # pragma: no cover - runtime-dependent import
                raise RuntimeError(f"Failed to load Qwen model: {exc}") from exc

        self.status = SynthBackendStatus(
            backend="qwen_custom_voice",
            model_loaded=True,
            fallback_active=False,
            detail=detail_note,
            supports_voice_clone=False,
            supports_default_voice=True,
            supports_cloned_voices=False,
        )

    def supports_voice_id(self, voice_id: str) -> bool:
        return voice_id == DEFAULT_VOICE_ID

    def synthesize_chunk(self, chunk_text: str, voice_id: str, language: str | None = None) -> SynthesizedAudio:
        if voice_id != DEFAULT_VOICE_ID:
            raise RuntimeError('Qwen custom-voice backend currently supports only voice_id "0"')

        resolved_language = _resolve_qwen_language(language)
        try:
            wavs, sample_rate = self._model.generate_custom_voice(
                text=chunk_text,
                language=resolved_language,
                speaker=self._default_speaker,
            )
        except Exception as exc:  # pragma: no cover - runtime-dependent inference
            raise RuntimeError(f"Qwen inference failed: {exc}") from exc

        if not wavs:
            raise RuntimeError("Qwen inference returned no audio")

        wave = wavs[0]
        if hasattr(wave, "detach"):
            wave = wave.detach().cpu().numpy()
        wave = self._np.asarray(wave, dtype=self._np.float32).reshape(-1)
        wave = self._np.clip(wave, -1.0, 1.0)
        pcm = (wave * 32767.0).astype(self._np.int16).tobytes()

        return SynthesizedAudio(
            pcm_s16le=pcm,
            sample_rate=int(sample_rate),
            channels=1,
        )

    def _resolve_torch_dtype(self, dtype: str):
        normalized = dtype.strip().lower()
        if normalized == "bfloat16":
            return self._torch.bfloat16
        if normalized == "float16":
            return self._torch.float16
        if normalized == "float32":
            return self._torch.float32
        raise RuntimeError(f"Unsupported VOICEREADER_QWEN_DTYPE={dtype}")

    @staticmethod
    def _is_cuda_device_map(device_map: str) -> bool:
        normalized = device_map.strip().lower()
        return normalized.startswith("cuda")


def create_synthesizer(config: EngineConfig) -> BaseSynthesizer:
    backend_choice = config.synth_backend.strip().lower()
    if backend_choice not in {"auto", "qwen", "mock"}:
        raise RuntimeError(
            "Invalid VOICEREADER_SYNTH_BACKEND. Use one of: auto, qwen, mock."
        )

    if backend_choice == "mock":
        return MockSynthesizer(detail="Selected explicitly via VOICEREADER_SYNTH_BACKEND=mock")

    if backend_choice in {"auto", "qwen"}:
        try:
            return QwenCustomVoiceSynthesizer(config)
        except Exception as exc:
            if backend_choice == "qwen":
                raise
            return MockSynthesizer(
                detail=f"Fell back from qwen backend: {exc}",
                fallback_active=True,
            )

    # Unreachable due to validation guard above.
    return MockSynthesizer(detail="Unreachable backend guard fallback", fallback_active=True)


def _resolve_qwen_language(language: str | None) -> str:
    if not language:
        return "Auto"
    normalized = language.strip().lower()
    if not normalized:
        return "Auto"
    if normalized in _QWEN_LANGUAGE_MAP:
        return _QWEN_LANGUAGE_MAP[normalized]
    return language
