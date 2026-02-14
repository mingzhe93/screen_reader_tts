from __future__ import annotations

from datetime import datetime, timezone
import json
from pathlib import Path
import secrets
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
        ref_text: str,
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
            "ref_text": ref_text,
        }
        (voice_dir / "meta.json").write_text(json.dumps(meta, indent=2), encoding="utf-8")

        # Placeholder artifact until real clone prompt generation is wired in.
        (voice_dir / "prompt.safetensors").write_bytes(secrets.token_bytes(256))

        return VoiceSummary.model_validate(meta)

    def delete_voice(self, voice_id: UUID) -> bool:
        voice_dir = self._voice_dir(voice_id)
        if not voice_dir.exists():
            return False
        shutil.rmtree(voice_dir)
        return True

    def _voice_dir(self, voice_id: UUID) -> Path:
        return self._voices_dir / str(voice_id)

    def _default_voice_summary(self) -> VoiceSummary:
        return VoiceSummary(
            voice_id=DEFAULT_VOICE_ID,
            display_name="Default Built-in Voice",
            created_at=datetime(1970, 1, 1, tzinfo=timezone.utc),
            tts_model_id=self._active_model_id,
            language_hint="auto",
        )
