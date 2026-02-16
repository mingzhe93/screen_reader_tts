from __future__ import annotations

from dataclasses import dataclass


_SENTENCE_BOUNDARY_CHARS = ".!?;:\n\u3002\uff01\uff1f"
DEFAULT_CHUNK_MAX_CHARS = 200
DEFAULT_MAX_SENTENCES_PER_CHUNK = 1
FIRST_CHUNK_MAX_CHARS = 200
FIRST_CHUNK_MAX_SENTENCES = 1


@dataclass(slots=True, frozen=True)
class TextChunk:
    chunk_index: int
    text: str
    start_char: int
    end_char: int


def split_text_into_chunks(
    text: str,
    max_chars: int = DEFAULT_CHUNK_MAX_CHARS,
    max_sentences_per_chunk: int = DEFAULT_MAX_SENTENCES_PER_CHUNK,
) -> list[TextChunk]:
    if max_chars < 1:
        raise ValueError("max_chars must be >= 1")
    if max_sentences_per_chunk < 1:
        raise ValueError("max_sentences_per_chunk must be >= 1")
    max_chars = min(max_chars, FIRST_CHUNK_MAX_CHARS)
    max_sentences_per_chunk = min(max_sentences_per_chunk, FIRST_CHUNK_MAX_SENTENCES)

    chunks: list[TextChunk] = []
    sentence_spans = _extract_sentence_spans(text)

    grouped_text_parts: list[str] = []
    grouped_start: int | None = None
    grouped_end = 0
    grouped_chars = 0
    grouped_sentences = 0

    def flush_group() -> None:
        nonlocal grouped_start, grouped_end, grouped_chars, grouped_sentences
        if grouped_start is None or not grouped_text_parts:
            grouped_text_parts.clear()
            grouped_start = None
            grouped_end = 0
            grouped_chars = 0
            grouped_sentences = 0
            return
        chunks.append(
            TextChunk(
                chunk_index=len(chunks),
                text=" ".join(grouped_text_parts),
                start_char=grouped_start,
                end_char=grouped_end,
            )
        )
        grouped_text_parts.clear()
        grouped_start = None
        grouped_end = 0
        grouped_chars = 0
        grouped_sentences = 0

    for span_start, span_end in sentence_spans:
        sentence_text = text[span_start:span_end].strip()
        if not sentence_text:
            continue

        building_first_chunk = len(chunks) == 0
        active_sentence_limit = (
            min(max_sentences_per_chunk, FIRST_CHUNK_MAX_SENTENCES)
            if building_first_chunk
            else max_sentences_per_chunk
        )
        active_char_limit = min(max_chars, FIRST_CHUNK_MAX_CHARS) if building_first_chunk else max_chars
        if active_sentence_limit < 1:
            active_sentence_limit = 1
        if active_char_limit < 100:
            active_char_limit = max(1, active_char_limit)

        sentence_len = len(sentence_text)
        if sentence_len > active_char_limit:
            flush_group()
            for piece_text, piece_start, piece_end in _split_span_by_chars(
                text, span_start, span_end, active_char_limit
            ):
                chunks.append(
                    TextChunk(
                        chunk_index=len(chunks),
                        text=piece_text,
                        start_char=piece_start,
                        end_char=piece_end,
                    )
                )
            continue

        projected_chars = sentence_len if grouped_chars == 0 else grouped_chars + 1 + sentence_len
        if grouped_sentences >= active_sentence_limit or (
            grouped_sentences > 0 and projected_chars > active_char_limit
        ):
            flush_group()

        if grouped_start is None:
            grouped_start = span_start
        grouped_end = span_end
        grouped_text_parts.append(sentence_text)
        grouped_sentences += 1
        grouped_chars = projected_chars if grouped_chars > 0 else sentence_len

    flush_group()
    return chunks


def _extract_sentence_spans(text: str) -> list[tuple[int, int]]:
    spans: list[tuple[int, int]] = []
    length = len(text)
    cursor = 0

    while cursor < length:
        while cursor < length and text[cursor].isspace():
            cursor += 1
        if cursor >= length:
            break

        end = cursor
        while end < length:
            ch = text[end]
            end += 1
            if ch in _SENTENCE_BOUNDARY_CHARS:
                while end < length and text[end] in _SENTENCE_BOUNDARY_CHARS:
                    end += 1
                break

        raw_segment = text[cursor:end]
        trimmed_segment = raw_segment.strip()
        if not trimmed_segment:
            cursor = end
            continue

        relative_start = raw_segment.find(trimmed_segment)
        absolute_start = cursor + relative_start
        absolute_end = absolute_start + len(trimmed_segment)
        spans.append((absolute_start, absolute_end))
        cursor = end

    return spans


def _split_span_by_chars(text: str, start: int, end: int, max_chars: int) -> list[tuple[str, int, int]]:
    pieces: list[tuple[str, int, int]] = []
    cursor = start

    while cursor < end:
        while cursor < end and text[cursor].isspace():
            cursor += 1
        if cursor >= end:
            break

        hard_end = min(cursor + max_chars, end)
        piece_end = hard_end
        if hard_end < end:
            split_at = text.rfind(" ", cursor, hard_end)
            if split_at > cursor:
                piece_end = split_at

        raw_piece = text[cursor:piece_end]
        trimmed_piece = raw_piece.strip()
        if not trimmed_piece:
            cursor = piece_end
            continue

        relative_start = raw_piece.find(trimmed_piece)
        absolute_start = cursor + relative_start
        absolute_end = absolute_start + len(trimmed_piece)
        pieces.append((trimmed_piece, absolute_start, absolute_end))
        cursor = piece_end

    return pieces
