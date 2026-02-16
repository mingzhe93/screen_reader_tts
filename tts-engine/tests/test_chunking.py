from tts_engine.chunking import split_text_into_chunks


def test_split_text_prefers_sentence_boundaries() -> None:
    text = "One short sentence. Another sentence follows. Third one ends here."
    chunks = split_text_into_chunks(text, max_chars=32)
    assert len(chunks) >= 2
    assert all(chunk.end_char > chunk.start_char for chunk in chunks)
    assert chunks[0].text.endswith(".")


def test_split_text_hard_splits_long_content() -> None:
    text = "A" * 1000
    chunks = split_text_into_chunks(text, max_chars=200)
    assert len(chunks) == 5
    assert all(len(chunk.text) <= 200 for chunk in chunks)


def test_split_text_groups_up_to_three_sentences() -> None:
    text = "One. Two. Three. Four. Five."
    chunks = split_text_into_chunks(text, max_chars=500)
    assert len(chunks) == 5
    assert chunks[0].text == "One."
    assert chunks[1].text == "Two."
    assert chunks[2].text == "Three."
    assert chunks[3].text == "Four."
    assert chunks[4].text == "Five."
