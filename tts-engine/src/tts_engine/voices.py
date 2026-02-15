from __future__ import annotations

from datetime import datetime, timezone
import json
from pathlib import Path
import shutil
from uuid import UUID, uuid4

from .constants import DEFAULT_VOICE_ID
from .schemas import VoiceSummary


class VoiceStore:
    def __init__(self, data_dir: Path, active_model_id: str) -> None:
        self._data_dir = data_dir
        self._voices_dir = self._data_dir / "voices"
        self._active_model_id = active_model_id
        self.ensure_layout()

    def ensure_layout(self) -> None:
        for folder_name in ("models", "voices", "cache", "logs"):
            (self._data_dir / folder_name).mkdir(parents=True, exist_ok=True)

    def list_voices(self) -> list[VoiceSummary]:
        voices: list[VoiceSummary] = [self._default_voice_summary()]
        if not self._voices_dir.exists():
            return voices

        for voice_dir in sorted(self._voices_dir.iterdir()):
            if not voice_dir.is_dir():
                continue
            meta_path = voice_dir / "meta.json"
            if not meta_path.exists():
                continue
            try:
                payload = json.loads(meta_path.read_text(encoding="utf-8"))
                voices.append(VoiceSummary.model_validate(payload))
            except (json.JSONDecodeError, OSError, ValueError):
                continue

        voices.sort(key=lambda voice: voice.created_at)
        return voices

    def voice_exists(self, voice_id: str) -> bool:
        if voice_id == DEFAULT_VOICE_ID:
            return True
        return (self._voice_dir(UUID(voice_id)) / "meta.json").exists()

    def create_voice(
        self,
        display_name: str,
        language_hint: str | None,
        ref_text: str | None,
        description: str | None = None,
    ) -> VoiceSummary:
        voice_id = uuid4()
        created_at = datetime.now(timezone.utc)

        voice_dir = self._voice_dir(voice_id)
        voice_dir.mkdir(parents=True, exist_ok=False)

        meta = {
            "voice_id": str(voice_id),
            "display_name": display_name,
            "created_at": created_at.isoformat(),
            "tts_model_id": self._active_model_id,
            "language_hint": language_hint,
            "description": description,
            "ref_text": ref_text,
        }
        (voice_dir / "meta.json").write_text(json.dumps(meta, indent=2), encoding="utf-8")

        return VoiceSummary.model_validate(meta)

    def update_voice(
        self,
        voice_id: UUID,
        *,
        display_name: str | None = None,
        language_hint: str | None = None,
        description: str | None = None,
        fields_to_update: set[str],
    ) -> VoiceSummary | None:
        voice_dir = self._voice_dir(voice_id)
        meta_path = voice_dir / "meta.json"
        if not meta_path.exists():
            return None

        try:
            payload = json.loads(meta_path.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError, ValueError):
            return None

        if "display_name" in fields_to_update and display_name is not None:
            payload["display_name"] = display_name
        if "language" in fields_to_update:
            payload["language_hint"] = language_hint
        if "description" in fields_to_update:
            payload["description"] = description

        voice = VoiceSummary.model_validate(payload)
        payload["voice_id"] = voice.voice_id
        payload["display_name"] = voice.display_name
        payload["created_at"] = voice.created_at.isoformat()
        payload["tts_model_id"] = voice.tts_model_id
        payload["language_hint"] = voice.language_hint
        payload["description"] = voice.description
        meta_path.write_text(json.dumps(payload, indent=2), encoding="utf-8")
        return voice

    def delete_voice(self, voice_id: UUID) -> bool:
        voice_dir = self._voice_dir(voice_id)
        if not voice_dir.exists():
            return False
        shutil.rmtree(voice_dir)
        return True

    def _voice_dir(self, voice_id: UUID) -> Path:
        return self._voices_dir / str(voice_id)

    def voice_prompt_path(self, voice_id: str) -> Path:
        return self._voice_dir(UUID(voice_id)) / "prompt.safetensors"

    def reference_audio_path(self, voice_id: str, suffix: str = ".wav") -> Path:
        normalized_suffix = suffix if suffix.startswith(".") else f".{suffix}"
        return self._voice_dir(UUID(voice_id)) / f"reference_audio{normalized_suffix}"

    def _default_voice_summary(self) -> VoiceSummary:
        return VoiceSummary(
            voice_id=DEFAULT_VOICE_ID,
            display_name="Default Built-in Voice",
            created_at=datetime(1970, 1, 1, tzinfo=timezone.utc),
            tts_model_id=self._active_model_id,
            language_hint="auto",
        )
