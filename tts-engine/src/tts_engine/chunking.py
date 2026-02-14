from __future__ import annotations

from dataclasses import dataclass


_BOUNDARY_CHARS = ".!?;:,\n。！？"


@dataclass(slots=True, frozen=True)
class TextChunk:
    chunk_index: int
    text: str
    start_char: int
    end_char: int


def split_text_into_chunks(text: str, max_chars: int = 400) -> list[TextChunk]:
    if max_chars < 1:
        raise ValueError("max_chars must be >= 1")

    chunks: list[TextChunk] = []
    cursor = 0
    index = 0
    length = len(text)

    while cursor < length:
        while cursor < length and text[cursor].isspace():
            cursor += 1
        if cursor >= length:
            break

        hard_end = min(cursor + max_chars, length)
        end = hard_end

        if hard_end < length:
            best_boundary = -1
            for boundary_char in _BOUNDARY_CHARS:
                candidate = text.rfind(boundary_char, cursor, hard_end)
                if candidate > best_boundary:
                    best_boundary = candidate
            if best_boundary >= cursor + max_chars // 3:
                end = best_boundary + 1

        raw_segment = text[cursor:end]
        trimmed_segment = raw_segment.strip()
        if not trimmed_segment:
            cursor = end
            continue

        relative_start = raw_segment.find(trimmed_segment)
        absolute_start = cursor + relative_start
        absolute_end = absolute_start + len(trimmed_segment)

        chunks.append(
            TextChunk(
                chunk_index=index,
                text=trimmed_segment,
                start_char=absolute_start,
                end_char=absolute_end,
            )
        )

        index += 1
        cursor = end

    return chunks
