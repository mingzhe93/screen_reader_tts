from __future__ import annotations

import base64
from pathlib import Path
import time
from unittest.mock import patch

from fastapi.testclient import TestClient

from tts_engine.app import create_app
from tts_engine.config import EngineConfig
from tts_engine.constants import WS_AUTH_SUBPROTOCOL
from tts_engine.synth import MockSynthesizer


TOKEN = "test-token"


def _auth_headers() -> dict[str, str]:
    return {"Authorization": f"Bearer {TOKEN}"}


def _make_client(tmp_path: Path) -> TestClient:
    config = EngineConfig(
        token=TOKEN,
        host="127.0.0.1",
        port=8765,
        data_dir=tmp_path / "data",
        synth_backend="mock",
        warmup_on_startup=False,
    )
    return TestClient(create_app(config))


def test_health_requires_bearer_token(tmp_path: Path) -> None:
    client = _make_client(tmp_path)

    response = client.get("/v1/health")
    assert response.status_code == 401
    assert response.json()["error"]["code"] == "UNAUTHORIZED"


def test_health_includes_runtime_status(tmp_path: Path) -> None:
    client = _make_client(tmp_path)
    response = client.get("/v1/health", headers=_auth_headers())
    assert response.status_code == 200
    runtime = response.json()["runtime"]
    assert runtime["backend"] in {"qwen_custom_voice", "kyutai_pocket_tts", "mock"}
    assert isinstance(runtime["model_loaded"], bool)
    assert isinstance(runtime["fallback_active"], bool)
    assert "warmup" in runtime
    assert runtime["warmup"]["status"] in {"not_started", "running", "ready", "error"}


def test_clone_speak_and_stream_job(tmp_path: Path) -> None:
    client = _make_client(tmp_path)
    sample_wav = tmp_path / "sample.wav"
    sample_wav.write_bytes(b"RIFF....WAVEfmt ")

    clone_resp = client.post(
        "/v1/voices/clone",
        headers=_auth_headers(),
        json={
            "display_name": "My Voice",
            "ref_audio": {"path": str(sample_wav)},
            "ref_text": "hello world",
            "language": "en",
        },
    )
    assert clone_resp.status_code == 200
    voice_id = clone_resp.json()["voice_id"]

    speak_resp = client.post(
        "/v1/speak",
        headers=_auth_headers(),
        json={
            "voice_id": voice_id,
            "text": "This is a test. It should emit at least one chunk.",
            "language": "en",
            "settings": {"chunking": {"max_chars": 120}},
        },
    )
    assert speak_resp.status_code == 200
    job_id = speak_resp.json()["job_id"]

    events = []
    with client.websocket_connect(f"/v1/stream/{job_id}", headers=_auth_headers()) as websocket:
        while True:
            event = websocket.receive_json()
            events.append(event)
            if event["type"] in {"JOB_DONE", "JOB_CANCELED", "JOB_ERROR"}:
                break

    event_types = {event["type"] for event in events}
    assert "JOB_STARTED" in event_types
    assert "AUDIO_CHUNK" in event_types
    assert event_types.intersection({"JOB_DONE", "JOB_CANCELED", "JOB_ERROR"})


def test_clone_with_base64_audio_persists_reference_payload(tmp_path: Path) -> None:
    client = _make_client(tmp_path)
    raw_audio = b"RIFF....WAVEfmt data"
    clone_resp = client.post(
        "/v1/voices/clone",
        headers=_auth_headers(),
        json={
            "display_name": "Base64 Voice",
            "ref_audio": {"wav_base64": base64.b64encode(raw_audio).decode("ascii")},
            "language": "en",
        },
    )
    assert clone_resp.status_code == 200
    voice_id = clone_resp.json()["voice_id"]
    saved_audio = tmp_path / "data" / "voices" / voice_id / "reference_audio.wav"
    assert saved_audio.exists()
    assert saved_audio.read_bytes() == raw_audio


def test_update_and_delete_saved_voice(tmp_path: Path) -> None:
    client = _make_client(tmp_path)
    sample_wav = tmp_path / "sample.wav"
    sample_wav.write_bytes(b"RIFF....WAVEfmt ")

    clone_resp = client.post(
        "/v1/voices/clone",
        headers=_auth_headers(),
        json={
            "display_name": "Original Name",
            "ref_audio": {"path": str(sample_wav)},
            "language": "en",
        },
    )
    assert clone_resp.status_code == 200
    voice_id = clone_resp.json()["voice_id"]

    patch_resp = client.patch(
        f"/v1/voices/{voice_id}",
        headers=_auth_headers(),
        json={
            "display_name": "Updated Name",
            "language": "en-US",
            "description": "My custom voice profile",
        },
    )
    assert patch_resp.status_code == 200
    updated = patch_resp.json()
    assert updated["voice_id"] == voice_id
    assert updated["display_name"] == "Updated Name"
    assert updated["language_hint"] == "en-US"
    assert updated["description"] == "My custom voice profile"

    list_resp = client.get("/v1/voices", headers=_auth_headers())
    assert list_resp.status_code == 200
    updated_voice = next(voice for voice in list_resp.json()["voices"] if voice["voice_id"] == voice_id)
    assert updated_voice["display_name"] == "Updated Name"
    assert updated_voice["description"] == "My custom voice profile"

    delete_resp = client.delete(f"/v1/voices/{voice_id}", headers=_auth_headers())
    assert delete_resp.status_code == 200
    assert delete_resp.json()["deleted"] is True

    missing_resp = client.delete(f"/v1/voices/{voice_id}", headers=_auth_headers())
    assert missing_resp.status_code == 404


def test_edit_or_delete_default_voice_is_forbidden(tmp_path: Path) -> None:
    client = _make_client(tmp_path)

    patch_resp = client.patch(
        "/v1/voices/0",
        headers=_auth_headers(),
        json={"description": "should fail"},
    )
    assert patch_resp.status_code == 403
    assert patch_resp.json()["error"]["code"] == "FORBIDDEN"

    delete_resp = client.delete("/v1/voices/0", headers=_auth_headers())
    assert delete_resp.status_code == 403
    assert delete_resp.json()["error"]["code"] == "FORBIDDEN"


def test_prefetch_models_endpoint_reports_storage_paths(tmp_path: Path) -> None:
    client = _make_client(tmp_path)

    def _fake_download(repo_id: str, data_dir: Path) -> Path:
        target = data_dir / "models" / Path(*repo_id.split("/"))
        target.mkdir(parents=True, exist_ok=True)
        return target

    with patch("tts_engine.app.download_repo_to_local_dir", side_effect=_fake_download):
        response = client.post(
            "/v1/models/prefetch",
            headers=_auth_headers(),
            json={"mode": "qwen_all"},
        )

    assert response.status_code == 200
    payload = response.json()
    assert payload["ok"] is True
    assert payload["mode"] == "qwen_all"
    assert payload["data_dir"].endswith("data")
    assert payload["models_dir"].endswith("data\\models") or payload["models_dir"].endswith("data/models")
    assert "Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice" in payload["downloaded"]
    assert "Qwen/Qwen3-TTS-12Hz-0.6B-Base" in payload["downloaded"]


def test_default_voice_available_and_speak_without_clone(tmp_path: Path) -> None:
    client = _make_client(tmp_path)

    voices_resp = client.get("/v1/voices", headers=_auth_headers())
    assert voices_resp.status_code == 200
    voices = voices_resp.json()["voices"]
    assert any(voice["voice_id"] == "0" for voice in voices)

    speak_resp = client.post(
        "/v1/speak",
        headers=_auth_headers(),
        json={"text": "Test default voice path."},
    )
    assert speak_resp.status_code == 200
    job_id = speak_resp.json()["job_id"]

    with client.websocket_connect(f"/v1/stream/{job_id}", headers=_auth_headers()) as websocket:
        while True:
            event = websocket.receive_json()
            if event["type"] in {"JOB_DONE", "JOB_CANCELED", "JOB_ERROR"}:
                break


def test_ws_subprotocol_auth_fallback(tmp_path: Path) -> None:
    client = _make_client(tmp_path)
    speak_resp = client.post(
        "/v1/speak",
        headers=_auth_headers(),
        json={"voice_id": "0", "text": "Subprotocol auth test"},
    )
    assert speak_resp.status_code == 200
    job_id = speak_resp.json()["job_id"]

    with client.websocket_connect(
        f"/v1/stream/{job_id}",
        subprotocols=[WS_AUTH_SUBPROTOCOL, TOKEN],
    ) as websocket:
        first_event = websocket.receive_json()
        assert first_event["type"] in {"JOB_STARTED", "AUDIO_CHUNK"}


def test_quit_endpoint_triggers_shutdown_callback(tmp_path: Path) -> None:
    config = EngineConfig(
        token=TOKEN,
        host="127.0.0.1",
        port=8765,
        data_dir=tmp_path / "data",
        synth_backend="mock",
        warmup_on_startup=False,
    )
    app = create_app(config)
    called = {"value": False}
    app.state.request_shutdown = lambda: called.__setitem__("value", True)
    client = TestClient(app)

    response = client.post("/v1/quit", headers=_auth_headers(), json={})
    assert response.status_code == 200
    assert response.json()["quitting"] is True
    assert called["value"] is True


def test_warmup_endpoint_wait_mode(tmp_path: Path) -> None:
    client = _make_client(tmp_path)
    response = client.post(
        "/v1/warmup",
        headers=_auth_headers(),
        json={"wait": True, "force": True, "reason": "test"},
    )
    assert response.status_code == 200
    payload = response.json()
    assert payload["accepted"] is True
    assert payload["warmup"]["status"] in {"ready", "error"}


def test_activate_model_triggers_warmup_and_updates_model_id(tmp_path: Path) -> None:
    client = _make_client(tmp_path)
    response = client.post(
        "/v1/models/activate",
        headers=_auth_headers(),
        json={
            "synth_backend": "mock",
            "active_model_id": "mock-model-v2",
            "warmup_wait": True,
            "warmup_force": True,
            "reason": "test_activate",
        },
    )
    assert response.status_code == 200
    payload = response.json()
    assert payload["reloaded"] is True
    assert payload["active_model_id"] == "mock-model-v2"
    assert payload["runtime"]["backend"] == "mock"
    assert payload["runtime"]["warmup"]["status"] in {"ready", "error"}

    health = client.get("/v1/health", headers=_auth_headers())
    assert health.status_code == 200
    assert health.json()["active_model_id"] == "mock-model-v2"


def test_cancel_drops_inflight_chunk_output(tmp_path: Path) -> None:
    original_synthesize = MockSynthesizer.synthesize_chunk

    def _slow_synthesize(self: MockSynthesizer, chunk_text: str, voice_id: str, language: str | None = None):
        time.sleep(0.25)
        return original_synthesize(self, chunk_text, voice_id, language)

    with patch.object(MockSynthesizer, "synthesize_chunk", new=_slow_synthesize):
        client = _make_client(tmp_path)
        speak_resp = client.post(
            "/v1/speak",
            headers=_auth_headers(),
            json={
                "voice_id": "0",
                "text": "Cancel me while first chunk is synthesizing.",
                "settings": {"chunking": {"max_chars": 400}},
            },
        )
        assert speak_resp.status_code == 200
        job_id = speak_resp.json()["job_id"]

        time.sleep(0.05)
        cancel_resp = client.post(
            "/v1/cancel",
            headers=_auth_headers(),
            json={"job_id": job_id},
        )
        assert cancel_resp.status_code == 200
        assert cancel_resp.json()["canceled"] is True

        events = []
        with client.websocket_connect(f"/v1/stream/{job_id}", headers=_auth_headers()) as websocket:
            while True:
                event = websocket.receive_json()
                events.append(event)
                if event["type"] in {"JOB_DONE", "JOB_CANCELED", "JOB_ERROR"}:
                    break

        event_types = [event["type"] for event in events]
        assert event_types[-1] == "JOB_CANCELED"
        assert "AUDIO_CHUNK" not in event_types


def test_rate_setting_changes_output_chunk_length(tmp_path: Path) -> None:
    client = _make_client(tmp_path)
    text = "Rate control sample sentence for deterministic mock output."

    def _first_chunk_pcm_bytes(rate: float) -> int:
        speak_resp = client.post(
            "/v1/speak",
            headers=_auth_headers(),
            json={
                "voice_id": "0",
                "text": text,
                "settings": {
                    "rate": rate,
                    "chunking": {"max_chars": 400},
                },
            },
        )
        assert speak_resp.status_code == 200
        job_id = speak_resp.json()["job_id"]

        with client.websocket_connect(f"/v1/stream/{job_id}", headers=_auth_headers()) as websocket:
            while True:
                event = websocket.receive_json()
                if event["type"] == "AUDIO_CHUNK":
                    return len(base64.b64decode(event["audio"]["data_base64"]))
                if event["type"] in {"JOB_DONE", "JOB_CANCELED", "JOB_ERROR"}:
                    raise AssertionError("No AUDIO_CHUNK received")

    normal_len = _first_chunk_pcm_bytes(rate=1.0)
    faster_len = _first_chunk_pcm_bytes(rate=2.0)
    slower_len = _first_chunk_pcm_bytes(rate=0.5)

    assert faster_len < normal_len
    assert slower_len > normal_len


def test_cancel_unknown_job_returns_404(tmp_path: Path) -> None:
    client = _make_client(tmp_path)
    response = client.post(
        "/v1/cancel",
        headers=_auth_headers(),
        json={"job_id": "00000000-0000-0000-0000-000000000001"},
    )
    assert response.status_code == 404
    assert response.json()["error"]["code"] == "JOB_NOT_FOUND"
