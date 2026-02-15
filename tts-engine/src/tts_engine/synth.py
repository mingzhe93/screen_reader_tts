from __future__ import annotations

from array import array
from dataclasses import dataclass
import math
from pathlib import Path
from typing import Any, Protocol
from uuid import UUID

from .config import EngineConfig
from .constants import DEFAULT_VOICE_ID
from .model_store import resolve_model_source


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

_KYUTAI_LANGUAGE_ALLOWLIST = {
    "en",
    "fr",
    "es",
    "de",
    "it",
    "pt",
    "ru",
    "zh",
    "ja",
    "ko",
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

    def prepare_cloned_voice(self, voice_id: str, reference_audio_source: str) -> None:
        raise NotImplementedError

    def forget_voice(self, voice_id: str) -> None:
        raise NotImplementedError

    def synthesize_chunk(self, chunk_text: str, voice_id: str, language: str | None = None) -> SynthesizedAudio:
        raise NotImplementedError

    def warmup(self, text: str, language: str | None = None) -> None:
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

    def prepare_cloned_voice(self, voice_id: str, reference_audio_source: str) -> None:
        # Mock backend accepts any voice id without precomputation.
        _ = (voice_id, reference_audio_source)

    def forget_voice(self, voice_id: str) -> None:
        _ = voice_id

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

    def warmup(self, text: str, language: str | None = None) -> None:
        # Lightweight no-op warmup for mock backend.
        _ = self.synthesize_chunk(text, voice_id=DEFAULT_VOICE_ID, language=language)


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
        self._model_source = resolve_model_source(config.data_dir, config.qwen_model_name)
        self._default_speaker = config.qwen_default_speaker

        dtype = self._resolve_torch_dtype(config.qwen_dtype)
        device_map = config.qwen_device_map
        attn_impl = config.qwen_attn_implementation
        detail_note = (
            f"model={self._model_name}, source={self._model_source}, "
            f"device_map={device_map}, dtype={config.qwen_dtype}, attn={attn_impl}"
        )

        if self._is_cuda_device_map(device_map) and not torch.cuda.is_available():
            raise RuntimeError(
                "Qwen backend requested CUDA, but current torch build has no CUDA runtime. "
                "Install a CUDA-enabled torch wheel, or set "
                "VOICEREADER_QWEN_DEVICE_MAP=cpu and VOICEREADER_QWEN_DTYPE=float32 for CPU testing."
            )

        try:
            self._model = Qwen3TTSModel.from_pretrained(
                self._model_source,
                device_map=device_map,
                dtype=dtype,
                attn_implementation=attn_impl,
            )
        except Exception as exc:
            # Common fallback when flash attention isn't available on Windows.
            if attn_impl == "flash_attention_2":
                fallback_attn = "sdpa"
                self._model = Qwen3TTSModel.from_pretrained(
                    self._model_source,
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

    def prepare_cloned_voice(self, voice_id: str, reference_audio_source: str) -> None:
        raise RuntimeError(
            "Qwen custom-voice backend does not support cloning in this engine. "
            "Switch to the Kyutai backend for /v1/voices/clone."
        )

    def forget_voice(self, voice_id: str) -> None:
        _ = voice_id

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

    def warmup(self, text: str, language: str | None = None) -> None:
        # Run a tiny generation to trigger lazy graph/runtime allocations.
        self.synthesize_chunk(text, voice_id=DEFAULT_VOICE_ID, language=language)

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


class PocketTtsSynthesizer:
    """Kyutai Pocket TTS backend with built-in and cloned voice prompt support."""

    def __init__(self, config: EngineConfig) -> None:
        try:
            import numpy as np
            import yaml
            from pocket_tts import TTSModel
        except Exception as exc:  # pragma: no cover - runtime-dependent import
            raise RuntimeError(
                "Kyutai Pocket TTS dependencies are unavailable. Install with "
                "`python -m pip install pocket-tts`."
            ) from exc

        self._np = np
        self._yaml = yaml
        self._tts_model_cls = TTSModel
        self._model_name = config.kyutai_model_name
        self._model_source = resolve_model_source(config.data_dir, config.kyutai_model_name)
        self._default_voice_prompt = config.kyutai_voice_prompt.strip() or "alba"
        self._default_sample_rate = int(config.kyutai_sample_rate)
        self._voices_dir = config.data_dir / "voices"
        self._voice_state_cache: dict[str, Any] = {}
        self._model_source_dir = self._as_existing_dir(self._model_source)
        model_config_arg = self._resolve_model_config_arg()
        detail_note = (
            f"model={self._model_name}, source={self._model_source}, "
            f"config={model_config_arg}, default_voice_prompt={self._default_voice_prompt}"
        )

        try:
            self._model = self._tts_model_cls.load_model(model_config_arg)
        except Exception as exc:  # pragma: no cover - runtime-dependent import
            raise RuntimeError(f"Failed to load Pocket TTS model: {exc}") from exc

        if hasattr(self._model, "sample_rate"):
            try:
                self._default_sample_rate = int(getattr(self._model, "sample_rate"))
            except Exception:
                pass

        # Resolve and cache voice prompt state once so chunk synthesis avoids repeated setup work.
        voice_prompt_source = self._resolve_voice_prompt_source(self._default_voice_prompt)
        try:
            self._voice_state = self._model.get_state_for_audio_prompt(voice_prompt_source)
        except Exception as exc:
            raise RuntimeError(
                f"Failed to resolve Pocket TTS voice prompt '{self._default_voice_prompt}' "
                f"(source={voice_prompt_source}): {exc}"
            ) from exc

        supports_voice_clone = bool(getattr(self._model, "has_voice_cloning", False))
        self.status = SynthBackendStatus(
            backend="kyutai_pocket_tts",
            model_loaded=True,
            fallback_active=False,
            detail=detail_note,
            supports_voice_clone=supports_voice_clone,
            supports_default_voice=True,
            supports_cloned_voices=supports_voice_clone,
        )

    def supports_voice_id(self, voice_id: str) -> bool:
        if voice_id == DEFAULT_VOICE_ID:
            return True
        if not _is_uuid_like(voice_id):
            return False
        return self._prompt_path_for_voice_id(voice_id).exists()

    def prepare_cloned_voice(self, voice_id: str, reference_audio_source: str) -> None:
        if not self.status.supports_voice_clone:
            raise RuntimeError("Pocket TTS voice cloning is not available in the loaded model configuration")
        if not _is_uuid_like(voice_id):
            raise RuntimeError('Cloned voice_id must be a UUID string (default voice uses "0")')

        normalized_source = reference_audio_source.strip()
        if not normalized_source:
            raise RuntimeError("Reference audio source is empty")

        prompt_path = self._prompt_path_for_voice_id(voice_id)
        prompt_path.parent.mkdir(parents=True, exist_ok=True)

        save_audio_prompt = getattr(self._model, "save_audio_prompt", None)
        if not callable(save_audio_prompt):
            raise RuntimeError("Pocket TTS model does not expose save_audio_prompt")
        try:
            save_audio_prompt(normalized_source, str(prompt_path))
        except TypeError:
            save_audio_prompt(normalized_source, str(prompt_path), False)
        except Exception as exc:
            raise RuntimeError(f"Failed to build voice prompt state: {exc}") from exc

        if not prompt_path.exists():
            generated_candidates = sorted(prompt_path.parent.glob("*.safetensors"))
            if generated_candidates:
                prompt_path = generated_candidates[0]
            else:
                raise RuntimeError("Voice cloning completed but no prompt.safetensors artifact was written")

        # Preload into memory so first speak request for this voice avoids prompt parsing overhead.
        try:
            self._voice_state_cache[voice_id] = self._model.get_state_for_audio_prompt(str(prompt_path))
        except Exception as exc:
            raise RuntimeError(f"Failed to load cloned voice prompt state from {prompt_path}: {exc}") from exc

    def forget_voice(self, voice_id: str) -> None:
        if voice_id == DEFAULT_VOICE_ID:
            return
        self._voice_state_cache.pop(voice_id, None)

    def synthesize_chunk(self, chunk_text: str, voice_id: str, language: str | None = None) -> SynthesizedAudio:
        voice_state = self._resolve_voice_state(voice_id)
        generated = self._generate_audio(voice_state=voice_state, chunk_text=chunk_text, language=language)
        pcm_s16le, sample_rate = _coerce_pcm16_from_generated_audio(
            generated=generated,
            np_module=self._np,
            default_sample_rate=self._default_sample_rate,
        )
        return SynthesizedAudio(
            pcm_s16le=pcm_s16le,
            sample_rate=sample_rate,
            channels=1,
        )

    def warmup(self, text: str, language: str | None = None) -> None:
        self.synthesize_chunk(text, voice_id=DEFAULT_VOICE_ID, language=language)

    def _resolve_voice_state(self, voice_id: str) -> Any:
        if voice_id == DEFAULT_VOICE_ID:
            return self._voice_state
        if not _is_uuid_like(voice_id):
            raise RuntimeError(f'Unsupported voice_id "{voice_id}" for Pocket TTS backend')

        cached = self._voice_state_cache.get(voice_id)
        if cached is not None:
            return cached

        prompt_path = self._prompt_path_for_voice_id(voice_id)
        if not prompt_path.exists():
            raise RuntimeError(
                f'Cloned voice "{voice_id}" has no prompt state on disk. '
                "Call /v1/voices/clone before speaking with it."
            )
        try:
            state = self._model.get_state_for_audio_prompt(str(prompt_path))
        except Exception as exc:
            raise RuntimeError(f'Failed to load cloned voice "{voice_id}": {exc}') from exc
        self._voice_state_cache[voice_id] = state
        return state

    def _generate_audio(self, voice_state: Any, chunk_text: str, language: str | None) -> Any:
        normalized_language = _resolve_kyutai_language(language)

        # Try known call signatures in order and only fall through on signature mismatch.
        if normalized_language:
            try:
                return self._model.generate_audio(voice_state, chunk_text, lang=normalized_language)
            except TypeError:
                pass

        try:
            return self._model.generate_audio(voice_state, chunk_text)
        except TypeError:
            pass

        if normalized_language:
            try:
                return self._model.generate_audio(chunk_text, lang=normalized_language)
            except TypeError:
                pass

        try:
            return self._model.generate_audio(chunk_text)
        except TypeError as exc:
            raise RuntimeError(
                "Pocket TTS API signature mismatch while calling generate_audio"
            ) from exc

    def _prompt_path_for_voice_id(self, voice_id: str) -> Path:
        return self._voices_dir / voice_id / "prompt.safetensors"

    def _resolve_model_config_arg(self) -> str:
        # Pocket TTS expects either a known variant id (e.g. b6369a24) or a YAML config file path.
        if self._model_source_dir is not None:
            yaml_candidates = sorted(self._model_source_dir.glob("*.yaml"))
            if yaml_candidates:
                return str(yaml_candidates[0].resolve())

            generated_yaml = self._build_local_model_config(self._model_source_dir)
            if generated_yaml is not None:
                return str(generated_yaml.resolve())

        normalized_source = self._model_source.strip()
        if "/" in normalized_source:
            # A raw HF repo id is not a valid Pocket config argument. Use the default known variant.
            return "b6369a24"
        return normalized_source or "b6369a24"

    def _resolve_voice_prompt_source(self, voice_prompt: str) -> str:
        normalized_prompt = voice_prompt.strip()
        if not normalized_prompt:
            normalized_prompt = "alba"

        prompt_path = Path(normalized_prompt).expanduser()
        if prompt_path.exists():
            return str(prompt_path.resolve())

        if self._model_source_dir is not None:
            embedding = self._model_source_dir / "embeddings" / f"{normalized_prompt}.safetensors"
            if embedding.exists():
                return str(embedding.resolve())

        return normalized_prompt

    def _build_local_model_config(self, model_dir: Path) -> Path | None:
        tokenizer_path = model_dir / "tokenizer.model"
        weight_candidates = sorted(model_dir.glob("tts_*.safetensors"))
        if not tokenizer_path.exists() or not weight_candidates:
            return None

        # __module__ path to file: pocket_tts/models/tts_model.py
        module_file = Path(__import__(self._tts_model_cls.__module__, fromlist=["__file__"]).__file__).resolve()
        default_yaml = module_file.parents[1] / "config" / "b6369a24.yaml"
        if not default_yaml.exists():
            raise RuntimeError(f"Pocket TTS default config was not found at {default_yaml}")

        with default_yaml.open("r", encoding="utf-8") as handle:
            config_data = self._yaml.safe_load(handle)

        weights_path = str(weight_candidates[0].resolve())
        config_data["weights_path"] = weights_path
        config_data["weights_path_without_voice_cloning"] = weights_path
        config_data["flow_lm"]["lookup_table"]["tokenizer_path"] = str(tokenizer_path.resolve())

        generated_yaml = model_dir / "voicereader-pocket-tts.yaml"
        with generated_yaml.open("w", encoding="utf-8") as handle:
            self._yaml.safe_dump(config_data, handle, sort_keys=False)
        return generated_yaml

    @staticmethod
    def _as_existing_dir(path_candidate: str) -> Path | None:
        if not path_candidate:
            return None
        try:
            candidate = Path(path_candidate).expanduser()
        except Exception:
            return None
        if candidate.exists() and candidate.is_dir():
            return candidate.resolve()
        return None


def create_synthesizer(config: EngineConfig) -> BaseSynthesizer:
    backend_choice = config.synth_backend.strip().lower()
    if backend_choice not in {"auto", "kyutai", "qwen", "mock"}:
        raise RuntimeError(
            "Invalid VOICEREADER_SYNTH_BACKEND. Use one of: auto, kyutai, qwen, mock."
        )

    if backend_choice == "mock":
        return MockSynthesizer(detail="Selected explicitly via VOICEREADER_SYNTH_BACKEND=mock")

    if backend_choice == "kyutai":
        return PocketTtsSynthesizer(config)

    if backend_choice == "qwen":
        return QwenCustomVoiceSynthesizer(config)

    # Auto mode: prefer Kyutai for fast first-run read-aloud, then Qwen, then mock fallback.
    auto_errors: list[str] = []
    try:
        return PocketTtsSynthesizer(config)
    except Exception as exc:
        auto_errors.append(f"kyutai backend: {exc}")

    try:
        return QwenCustomVoiceSynthesizer(config)
    except Exception as exc:
        auto_errors.append(f"qwen backend: {exc}")

    return MockSynthesizer(
        detail=f"Fell back from auto backends: {' | '.join(auto_errors)}",
        fallback_active=True,
    )


def _resolve_qwen_language(language: str | None) -> str:
    if not language:
        return "Auto"
    normalized = language.strip().lower()
    if not normalized:
        return "Auto"
    if normalized in _QWEN_LANGUAGE_MAP:
        return _QWEN_LANGUAGE_MAP[normalized]
    return language


def _resolve_kyutai_language(language: str | None) -> str | None:
    if not language:
        return None
    normalized = language.strip().lower()
    if not normalized or normalized == "auto":
        return None
    if normalized in _KYUTAI_LANGUAGE_ALLOWLIST:
        return normalized
    return normalized


def _is_uuid_like(candidate: str) -> bool:
    try:
        UUID(candidate)
        return True
    except ValueError:
        return False


def _coerce_pcm16_from_generated_audio(
    generated: Any,
    np_module,
    default_sample_rate: int,
) -> tuple[bytes, int]:
    sample_rate = int(default_sample_rate)
    audio_payload = generated

    # Common shape: (audio, sample_rate)
    if isinstance(generated, tuple) and len(generated) >= 2 and isinstance(generated[1], (int, float)):
        audio_payload = generated[0]
        sample_rate = int(generated[1])
    elif hasattr(generated, "sample_rate"):
        try:
            sample_rate = int(getattr(generated, "sample_rate"))
        except Exception:
            sample_rate = int(default_sample_rate)
        if hasattr(generated, "audio"):
            audio_payload = getattr(generated, "audio")

    if hasattr(audio_payload, "detach"):
        # Torch tensor path.
        audio_payload = audio_payload.detach().cpu().numpy()

    array_data = np_module.asarray(audio_payload)
    if array_data.size == 0:
        raise RuntimeError("Pocket TTS inference returned empty audio")

    if array_data.ndim > 1:
        if array_data.shape[0] == 1:
            array_data = array_data[0]
        elif array_data.shape[-1] in (1, 2):
            array_data = array_data.mean(axis=-1)
        else:
            array_data = array_data.reshape(-1)

    array_data = array_data.reshape(-1)
    if array_data.dtype.kind in {"i", "u"}:
        pcm = np_module.clip(array_data, -32768, 32767).astype(np_module.int16)
    else:
        float_audio = np_module.asarray(array_data, dtype=np_module.float32)
        float_audio = np_module.clip(float_audio, -1.0, 1.0)
        pcm = (float_audio * 32767.0).astype(np_module.int16)

    return pcm.tobytes(), int(sample_rate)
