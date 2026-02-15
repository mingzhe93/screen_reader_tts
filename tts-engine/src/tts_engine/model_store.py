from __future__ import annotations

from dataclasses import dataclass
import os
from pathlib import Path


QWEN_CUSTOM_MODEL_REPO = "Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice"
QWEN_BASE_MODEL_REPO = "Qwen/Qwen3-TTS-12Hz-0.6B-Base"
KYUTAI_POCKET_MODEL_REPO = "Verylicious/pocket-tts-ungated"


@dataclass(frozen=True, slots=True)
class CachePaths:
    cache_root: Path
    hub_cache: Path
    transformers_cache: Path


def configure_hf_cache(data_dir: Path) -> CachePaths:
    default_cache_root = (data_dir / "hf-cache").resolve()
    cache_root = Path(os.getenv("VOICEREADER_HF_CACHE_DIR", str(default_cache_root))).expanduser().resolve()
    hub_cache = Path(
        os.getenv("VOICEREADER_HF_HUB_CACHE_DIR", str((cache_root / "hub").resolve()))
    ).expanduser().resolve()
    transformers_cache = Path(
        os.getenv("VOICEREADER_TRANSFORMERS_CACHE_DIR", str((cache_root / "transformers").resolve()))
    ).expanduser().resolve()

    cache_root.mkdir(parents=True, exist_ok=True)
    hub_cache.mkdir(parents=True, exist_ok=True)
    transformers_cache.mkdir(parents=True, exist_ok=True)

    # Force process-local ownership so engine does not drift to user-global cache dirs.
    os.environ["HF_HOME"] = str(cache_root)
    os.environ["HF_HUB_CACHE"] = str(hub_cache)
    os.environ["HUGGINGFACE_HUB_CACHE"] = str(hub_cache)
    # Prefer HF_HOME-based cache routing; TRANSFORMERS_CACHE is deprecated in transformers v5.
    os.environ.pop("TRANSFORMERS_CACHE", None)

    return CachePaths(
        cache_root=cache_root,
        hub_cache=hub_cache,
        transformers_cache=transformers_cache,
    )


def resolve_model_source(data_dir: Path, model_ref: str) -> str:
    normalized_ref = model_ref.strip()
    if not normalized_ref:
        return model_ref

    explicit_path = Path(normalized_ref).expanduser()
    if explicit_path.exists():
        return str(explicit_path.resolve())

    if "/" not in normalized_ref:
        return model_ref

    local_repo_dir = repo_id_to_local_dir(data_dir, normalized_ref)
    if local_repo_dir.exists():
        return str(local_repo_dir.resolve())
    return model_ref


def repo_id_to_local_dir(data_dir: Path, repo_id: str) -> Path:
    segments = [segment.strip() for segment in repo_id.split("/") if segment.strip()]
    if len(segments) < 2:
        raise ValueError(f"Invalid Hugging Face repo id: {repo_id}")
    if any(segment in {".", ".."} for segment in segments):
        raise ValueError(f"Invalid Hugging Face repo id: {repo_id}")
    return (data_dir / "models" / Path(*segments)).resolve()


def download_repo_to_local_dir(repo_id: str, data_dir: Path) -> Path:
    # Imported lazily so engine startup doesn't require hub import overhead.
    from huggingface_hub import snapshot_download

    target_dir = repo_id_to_local_dir(data_dir, repo_id)
    target_dir.mkdir(parents=True, exist_ok=True)
    snapshot_download(
        repo_id=repo_id,
        local_dir=str(target_dir),
    )
    return target_dir
