from __future__ import annotations

import sys
from pathlib import Path


def _ensure_src_on_path() -> None:
    root = Path(__file__).resolve().parent
    src_dir = root / "src"
    src_text = str(src_dir)
    if src_text not in sys.path:
        sys.path.insert(0, src_text)


def main() -> int:
    _ensure_src_on_path()
    from tts_engine.main import main as engine_main

    return int(engine_main())


if __name__ == "__main__":
    raise SystemExit(main())
