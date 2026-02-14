from __future__ import annotations

import os
from pathlib import Path

from tts_engine.model_store import (
    QWEN_CUSTOM_MODEL_REPO,
    configure_hf_cache,
    repo_id_to_local_dir,
    resolve_model_source,
)


def test_resolve_model_source_prefers_explicit_existing_path(tmp_path: Path) -> None:
    model_dir = tmp_path / "my_model"
    model_dir.mkdir(parents=True)
    resolved = resolve_model_source(tmp_path / "data", str(model_dir))
    assert resolved == str(model_dir.resolve())


def test_resolve_model_source_uses_local_repo_mirror_when_present(tmp_path: Path) -> None:
    data_dir = tmp_path / "data"
    local_repo_dir = repo_id_to_local_dir(data_dir, QWEN_CUSTOM_MODEL_REPO)
    local_repo_dir.mkdir(parents=True)
    resolved = resolve_model_source(data_dir, QWEN_CUSTOM_MODEL_REPO)
    assert resolved == str(local_repo_dir.resolve())


def test_configure_hf_cache_defaults_to_data_dir(monkeypatch, tmp_path: Path) -> None:
    for env_key in (
        "VOICEREADER_HF_CACHE_DIR",
        "VOICEREADER_HF_HUB_CACHE_DIR",
        "VOICEREADER_TRANSFORMERS_CACHE_DIR",
        "HF_HOME",
        "HF_HUB_CACHE",
        "HUGGINGFACE_HUB_CACHE",
    ):
        monkeypatch.delenv(env_key, raising=False)

    data_dir = tmp_path / "data"
    paths = configure_hf_cache(data_dir)

    assert paths.cache_root == (data_dir / "hf-cache").resolve()
    assert paths.hub_cache == (paths.cache_root / "hub").resolve()
    assert paths.transformers_cache == (paths.cache_root / "transformers").resolve()
    assert os.environ["HF_HOME"] == str(paths.cache_root)
    assert os.environ["HF_HUB_CACHE"] == str(paths.hub_cache)
    assert os.environ["HUGGINGFACE_HUB_CACHE"] == str(paths.hub_cache)
    assert "TRANSFORMERS_CACHE" not in os.environ
