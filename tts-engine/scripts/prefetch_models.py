from __future__ import annotations

import argparse
from pathlib import Path
import sys


def _load_engine_modules() -> None:
    repo_root = Path(__file__).resolve().parents[1]
    src_dir = repo_root / "src"
    if str(src_dir) not in sys.path:
        sys.path.insert(0, str(src_dir))


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Prefetch VoiceReader model repos into local data_dir/models",
    )
    parser.add_argument(
        "--data-dir",
        default=str((Path(__file__).resolve().parents[1] / ".data").resolve()),
        help="Engine data directory (default: ./tts-engine/.data)",
    )
    mode_group = parser.add_mutually_exclusive_group()
    mode_group.add_argument("--custom-only", action="store_true", help="Download only custom-voice model")
    mode_group.add_argument("--base-only", action="store_true", help="Download only base model")
    return parser


def main() -> int:
    _load_engine_modules()
    parser = _build_parser()
    args = parser.parse_args()

    from tts_engine.model_store import (
        QWEN_BASE_MODEL_REPO,
        QWEN_CUSTOM_MODEL_REPO,
        configure_hf_cache,
        download_repo_to_local_dir,
    )

    data_dir = Path(args.data_dir).expanduser().resolve()
    data_dir.mkdir(parents=True, exist_ok=True)

    cache_paths = configure_hf_cache(data_dir)
    print(f"data_dir={data_dir}", flush=True)
    print(f"hf_cache_root={cache_paths.cache_root}", flush=True)
    print(f"hf_hub_cache={cache_paths.hub_cache}", flush=True)
    print(f"transformers_cache={cache_paths.transformers_cache}", flush=True)

    if args.custom_only:
        repos = [QWEN_CUSTOM_MODEL_REPO]
    elif args.base_only:
        repos = [QWEN_BASE_MODEL_REPO]
    else:
        repos = [QWEN_CUSTOM_MODEL_REPO, QWEN_BASE_MODEL_REPO]

    print(f"prefetch_count={len(repos)}", flush=True)
    for repo_id in repos:
        print(f"downloading={repo_id}", flush=True)
        local_dir = download_repo_to_local_dir(repo_id, data_dir=data_dir)
        print(f"saved_to={local_dir}", flush=True)

    print("PREFETCH_MODELS_OK", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
