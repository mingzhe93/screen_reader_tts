//! Turns selected text into chunks that a TTS model speaks one at a time.
//!
//! Each chunk is generated independently, so where the text is cut decides both how it
//! sounds (a cut resets intonation) and how soon audio starts (the first chunk has to be
//! generated before anything plays). The pipeline is:
//!   1. `normalize_for_speech` — repair line breaks from PDFs and web pages: re-join
//!      wrapped lines and hyphenated words, and turn headings and list items into
//!      sentences of their own so they are spoken with a pause.
//!   2. `chunk_text` — cut at sentence ends where possible, then at clause punctuation,
//!      then before a conjunction or preposition, then at any space.
//!
//! Shared by the Kyutai and Audio8 runtimes; neither model's own splitter is used.

/// A chunk may run this far past its budget to reach a sentence end.
pub(crate) const SOFT_OVERFLOW: f32 = 1.35;
/// When a sentence has to be split, no piece should be shorter than this share of the
/// ideal piece length.
const MIN_PIECE_SHARE: f32 = 0.5;
/// A line counts as wrapped (not a heading) when it is at least this share of the
/// longest line in its paragraph.
const WRAPPED_LINE_SHARE: f32 = 0.7;
/// Hard-wrapped text has lines at least this long; shorter blocks are headings or lists.
const MIN_WRAP_WIDTH: usize = 40;

/// Abbreviations whose full stop never ends a sentence.
const ABBREVIATIONS: [&str; 22] = [
    "mr", "mrs", "ms", "dr", "prof", "sr", "jr", "st", "vs", "e.g", "i.e", "cf", "fig", "eq", "vol", "pp", "approx",
    "ca", "gen", "col", "lt", "capt",
];
/// Month abbreviations, only when capitalized ("Jan. 5"; "mar." is not one).
const MONTH_ABBREVIATIONS: [&str; 11] = [
    "Jan", "Feb", "Mar", "Apr", "Jun", "Jul", "Aug", "Sep", "Sept", "Oct", "Nov",
];
/// A cut just before one of these words falls at a natural phrase boundary.
const PHRASE_WORDS: [&str; 33] = [
    "and", "or", "but", "because", "although", "though", "while", "whereas", "when", "where", "which", "who",
    "whose", "if", "unless", "until", "since", "after", "before", "then", "with", "without", "through", "during",
    "between", "among", "about", "against", "including", "by", "for", "from", "into",
];
/// A line ending in one of these words continues on the next line even if that line
/// starts with a capital letter.
const CONTINUATION_WORDS: [&str; 22] = [
    "the", "a", "an", "of", "and", "or", "to", "in", "for", "with", "on", "at", "by", "from", "is", "are", "was",
    "were", "be", "that", "which", "as",
];
const BULLET_GLYPHS: [char; 16] = [
    '•', '◦', '▪', '‣', '·', '●', '○', '■', '□', '➢', '➤', '►', '▶', '✓', '✔', '*',
];

pub(crate) fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x11FF
            | 0x2E80..=0x2FDF
            | 0x3000..=0x303F
            | 0x3040..=0x30FF
            | 0x3100..=0x31FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA960..=0xA97F
            | 0xAC00..=0xD7A3
            | 0xD7B0..=0xD7FF
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF01..=0xFF9F
            | 0x20000..=0x2FA1F)
}

/// Approximates Python's `unicodedata.category(ch).startswith("C")` for non-whitespace
/// characters: control, format, private-use and surrogate code points.
pub(crate) fn is_invisible(ch: char) -> bool {
    ch.is_control()
        || matches!(ch as u32,
            0x00AD
                | 0x0600..=0x0605
                | 0x061C
                | 0x200B..=0x200F
                | 0x202A..=0x202E
                | 0x2060..=0x2064
                | 0x2066..=0x206F
                | 0xFEFF
                | 0xFFF9..=0xFFFB
                | 0xE000..=0xF8FF
                | 0xF0000..=0x10FFFF)
}

/// Rough speaking-length weight of a character, in "English characters".
fn char_units(ch: char) -> f32 {
    if is_cjk(ch) {
        // CJK text is spoken at roughly 4-5 characters per second, English at ~15.
        3.5
    } else {
        1.0
    }
}

pub fn text_units(text: &str) -> f32 {
    text.chars().map(char_units).sum()
}

fn is_ascii_terminator(ch: char) -> bool {
    matches!(ch, '.' | '!' | '?' | '…')
}

fn is_cjk_terminator(ch: char) -> bool {
    matches!(ch, '。' | '！' | '？')
}

fn is_closing_mark(ch: char) -> bool {
    matches!(ch, '"' | '\'' | '”' | '’' | ')' | ']' | '}' | '」' | '』' | '）' | '》')
}

fn ends_with_terminator(text: &str) -> bool {
    text.trim_end_matches(is_closing_mark)
        .chars()
        .last()
        .is_some_and(|ch| is_ascii_terminator(ch) || is_cjk_terminator(ch) || matches!(ch, ':' | ';' | '：' | '；'))
}

/// Strips a list marker ("• ", "- ", "1. ", "2) ", "(a) ") from the start of a line.
/// Numbers are kept so numbered steps are still read out; glyphs are dropped.
fn strip_list_marker(line: &str) -> Option<String> {
    let mut chars = line.chars();
    let first = chars.next()?;
    let rest = chars.as_str();
    if (BULLET_GLYPHS.contains(&first) || matches!(first, '-' | '–' | '—')) && rest.starts_with(char::is_whitespace) {
        let item = rest.trim_start();
        return (!item.is_empty()).then(|| item.to_string());
    }

    let body = line.strip_prefix('(').unwrap_or(line);
    let digits = body.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if (1..=3).contains(&digits) {
        let after = &body[digits..];
        let mut after_chars = after.chars();
        if matches!(after_chars.next(), Some('.' | ')')) && after_chars.as_str().starts_with(char::is_whitespace) {
            let item = after_chars.as_str().trim_start();
            if !item.is_empty() {
                return Some(format!("{}. {item}", &body[..digits]));
            }
        }
    }
    // Lettered items only in the unambiguous "a)" / "(a)" form; "A. Smith" is a name.
    let mut body_chars = body.chars();
    if let (Some(letter), Some(')')) = (body_chars.next(), body_chars.next()) {
        if letter.is_ascii_alphabetic() && body_chars.as_str().starts_with(char::is_whitespace) {
            let item = body_chars.as_str().trim_start();
            if !item.is_empty() {
                return Some(format!("{letter}. {item}"));
            }
        }
    }
    None
}

/// Line width in columns; CJK characters are twice as wide as Latin ones.
fn display_width(line: &str) -> usize {
    line.chars().map(|ch| if is_cjk(ch) { 2 } else { 1 }).sum()
}

fn push_terminal_punctuation(text: &mut String) {
    let Some(last) = text.trim_end_matches(is_closing_mark).chars().last() else {
        return;
    };
    if ends_with_terminator(text) || matches!(last, ',' | '，' | '、') {
        return;
    }
    text.push(if is_cjk(last) { '。' } else { '.' });
}

/// Prepares raw selected text for chunking. Text copied from PDFs and web pages carries
/// line breaks that are either layout (a wrapped line, a hyphenated word) or structure
/// (a heading, a list item, a paragraph). Layout breaks are removed; structural breaks
/// become sentence ends so they are spoken with a pause.
pub fn normalize_for_speech(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .filter(|ch| ch.is_whitespace() || !is_invisible(*ch))
        .map(|ch| {
            if matches!(ch, '\r' | '\u{0B}' | '\u{0C}' | '\u{1C}'..='\u{1E}' | '\u{85}' | '\u{2028}' | '\u{2029}') {
                '\n'
            } else {
                ch
            }
        })
        .collect();
    // A paragraph is a run of non-blank lines.
    let mut paragraphs: Vec<Vec<String>> = vec![Vec::new()];
    for line in cleaned.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            paragraphs.push(Vec::new());
        } else if let Some(paragraph) = paragraphs.last_mut() {
            paragraph.push(line);
        }
    }

    let mut sentences: Vec<String> = Vec::new();
    for lines in paragraphs.iter().filter(|lines| !lines.is_empty()) {
        let longest = lines.iter().map(|line| display_width(line)).max().unwrap_or(0);
        let hard_wrapped = lines.len() >= 2 && longest >= MIN_WRAP_WIDTH;

        let mut current = String::new();
        let mut previous_line_len = 0usize;
        for line in lines {
            let line_len = display_width(line);
            if let Some(item) = strip_list_marker(line) {
                if !current.is_empty() {
                    push_terminal_punctuation(&mut current);
                    sentences.push(std::mem::take(&mut current));
                }
                current = item;
                previous_line_len = line_len;
                continue;
            }
            if current.is_empty() {
                current = line.clone();
                previous_line_len = line_len;
                continue;
            }

            let last = current.chars().last().unwrap_or(' ');
            let before_last = current.chars().rev().nth(1).unwrap_or(' ');
            let first = line.chars().next().unwrap_or(' ');
            let previous_was_full_width =
                hard_wrapped && previous_line_len as f32 >= longest as f32 * WRAPPED_LINE_SHARE;
            let last_word = current
                .rsplit(char::is_whitespace)
                .next()
                .unwrap_or("")
                .to_lowercase();

            if last == '-' && before_last.is_alphabetic() && first.is_lowercase() {
                // A word hyphenated across a line break.
                current.pop();
                current.push_str(line);
            } else if is_cjk(last) && is_cjk(first) {
                if !previous_was_full_width {
                    push_terminal_punctuation(&mut current);
                }
                current.push_str(line);
            } else if ends_with_terminator(&current)
                || matches!(last, ',' | '–' | '—' | '，' | '、')
                || first.is_lowercase()
                || first.is_ascii_digit()
                || previous_was_full_width
                || CONTINUATION_WORDS.contains(&last_word.as_str())
            {
                current.push(' ');
                current.push_str(line);
            } else {
                // A short line followed by a capitalized one: a heading or a list without
                // markers. Speak it as its own sentence.
                push_terminal_punctuation(&mut current);
                current.push(' ');
                current.push_str(line);
            }
            previous_line_len = line_len;
        }
        if !current.is_empty() {
            push_terminal_punctuation(&mut current);
            sentences.push(current);
        }
    }
    sentences.join(" ")
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Cut {
    /// End of a sentence.
    Sentence,
    /// Semicolon or colon.
    StrongClause,
    /// Comma or dash.
    Clause,
    /// Just before a conjunction, preposition or relative pronoun.
    Phrase,
    /// Any other space (or between two CJK characters).
    Space,
}

/// The word (letters, digits and inner full stops) that ends just before `end`.
fn token_before(chars: &[char], end: usize) -> String {
    let mut start = end;
    while start > 0 && (chars[start - 1].is_alphanumeric() || chars[start - 1] == '.') {
        start -= 1;
    }
    chars[start..end].iter().collect()
}

/// Decides whether the full stop at `index` ends a sentence, given the word before it.
fn full_stop_ends_sentence(chars: &[char], index: usize, next_word_start: Option<char>) -> bool {
    let token = token_before(chars, index);
    if token.is_empty() {
        return true;
    }
    let lower = token.to_lowercase();
    if ABBREVIATIONS.contains(&lower.as_str()) || MONTH_ABBREVIATIONS.contains(&token.as_str()) {
        return false;
    }
    // "No. 5" is a number; "He said no. Then..." is not.
    if token == "No" && next_word_start.is_some_and(|ch| ch.is_ascii_digit()) {
        return false;
    }
    // Initials and initialisms: "J. K. Rowling", "the U.S. Army".
    let is_initialism = token.split('.').all(|part| {
        let mut letters = part.chars();
        matches!((letters.next(), letters.next()), (Some(letter), None) if letter.is_uppercase())
    });
    if is_initialism {
        return false;
    }
    // A list number at the start of a sentence: "1. Download the installer."
    if token.chars().all(|ch| ch.is_ascii_digit()) {
        let token_start = index - token.chars().count();
        let before = chars[..token_start].iter().rev().find(|ch| !ch.is_whitespace());
        let starts_sentence = before.is_none_or(|ch| is_ascii_terminator(*ch) || is_cjk_terminator(*ch));
        if starts_sentence {
            return false;
        }
    }
    true
}

/// For every character, the best kind of cut allowed immediately after it.
fn classify_cuts(chars: &[char]) -> Vec<Option<Cut>> {
    let mut cuts: Vec<Option<Cut>> = vec![None; chars.len()];
    let next_non_space = |from: usize| chars[from..].iter().copied().find(|ch| !ch.is_whitespace());

    for index in 0..chars.len() {
        let ch = chars[index];
        let next = chars.get(index + 1).copied();
        let next_is_space = next.is_none_or(char::is_whitespace);

        if is_cjk_terminator(ch) || is_ascii_terminator(ch) {
            // Only the last mark of a run ("...", "?!") can end a sentence.
            if next.is_some_and(|next| is_ascii_terminator(next) || is_cjk_terminator(next)) {
                continue;
            }
            let mut end = index;
            while end + 1 < chars.len() && is_closing_mark(chars[end + 1]) {
                end += 1;
            }
            let after = chars.get(end + 1).copied();
            let following = next_non_space(end + 1);
            let cut = if is_cjk_terminator(ch) {
                Some(Cut::Sentence)
            } else if after.is_some_and(|after| !after.is_whitespace()) {
                // "3.14", "example.com", "e.g.": the mark is inside a word.
                None
            } else if following.is_some_and(char::is_lowercase) {
                // `"What?" she asked.` and unknown abbreviations: a pause, not a sentence end.
                Some(Cut::Clause)
            } else if ch == '.' && index > 0 && chars[index - 1] != '.' && !full_stop_ends_sentence(chars, index, following) {
                None
            } else {
                Some(Cut::Sentence)
            };
            if let Some(cut) = cut {
                cuts[end] = Some(cut);
            }
            continue;
        }
        if cuts[index].is_some() {
            continue;
        }

        cuts[index] = match ch {
            '；' | '：' => Some(Cut::StrongClause),
            '，' | '、' => Some(Cut::Clause),
            ';' | ':' if next_is_space => Some(Cut::StrongClause),
            ',' if next_is_space || next.is_some_and(is_closing_mark) => Some(Cut::Clause),
            '—' | '–' => Some(Cut::Clause),
            '-' if next_is_space && index > 0 && chars[index - 1].is_whitespace() => Some(Cut::Clause),
            _ if next.is_some_and(char::is_whitespace) => {
                let word: String = chars[index + 1..]
                    .iter()
                    .skip_while(|ch| ch.is_whitespace())
                    .take_while(|ch| ch.is_alphabetic())
                    .collect::<String>()
                    .to_lowercase();
                if PHRASE_WORDS.contains(&word.as_str()) {
                    Some(Cut::Phrase)
                } else {
                    Some(Cut::Space)
                }
            }
            _ if is_cjk(ch) => Some(Cut::Space),
            _ => None,
        };
    }
    cuts
}

/// Splits normalized text into chunks. `budgets` gives the size (in units, roughly
/// English characters) for chunk 0, 1, 2, ...; the last entry applies to all remaining
/// chunks. Small early budgets shorten the wait for the first audio.
///
/// A chunk holds as many whole sentences as fit its budget, and may run up to
/// `SOFT_OVERFLOW` past it to finish a sentence. A sentence longer than that is split
/// into pieces of similar length at the best boundary available: semicolon or colon,
/// then comma or dash, then before a conjunction or preposition, then any space.
pub fn chunk_text(text: &str, budgets: &[f32]) -> Vec<String> {
    let chars: Vec<char> = text.trim().chars().collect();
    if chars.is_empty() || budgets.is_empty() {
        return Vec::new();
    }
    let cuts = classify_cuts(&chars);
    // units_before[i] is the weight of chars[..i].
    let mut units_before = Vec::with_capacity(chars.len() + 1);
    units_before.push(0.0f32);
    for ch in &chars {
        units_before.push(units_before[units_before.len() - 1] + char_units(*ch));
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let budget = budgets[usize::min(chunks.len(), budgets.len() - 1)].max(8.0);
        let units_from_start = |end: usize| units_before[end] - units_before[start];
        let remaining = units_from_start(chars.len());

        let cut = if remaining <= budget {
            chars.len()
        } else {
            // Positions (exclusive end) of sentence ends after `start`.
            let mut last_sentence_within_budget: Option<usize> = None;
            let mut first_sentence_beyond_budget: Option<usize> = None;
            for index in start..chars.len() {
                if cuts[index] != Some(Cut::Sentence) {
                    continue;
                }
                if units_from_start(index + 1) <= budget {
                    last_sentence_within_budget = Some(index + 1);
                } else {
                    first_sentence_beyond_budget = Some(index + 1);
                    break;
                }
            }
            let sentence_end = first_sentence_beyond_budget.unwrap_or(chars.len());

            if let Some(end) = last_sentence_within_budget {
                end
            } else if units_from_start(sentence_end) <= budget * SOFT_OVERFLOW {
                sentence_end
            } else {
                split_long_sentence(&cuts, &units_before, start, sentence_end, budget)
            }
        };

        let piece: String = chars[start..cut].iter().collect();
        let piece = piece.trim();
        if !piece.is_empty() {
            chunks.push(piece.to_string());
        }
        start = cut;
    }
    chunks
}

/// Picks where to cut a sentence (`start..sentence_end`) that does not fit `budget`.
/// The sentence is divided into the fewest pieces that fit, and the cut is the
/// strongest boundary near the ideal piece length. Like a sentence end, a comma or
/// semicolon may be reached by running up to `SOFT_OVERFLOW` past the budget.
fn split_long_sentence(
    cuts: &[Option<Cut>],
    units_before: &[f32],
    start: usize,
    sentence_end: usize,
    budget: f32,
) -> usize {
    let units_from_start = |end: usize| units_before[end] - units_before[start];
    let total = units_from_start(sentence_end);
    let pieces = (total / budget).ceil().max(2.0);
    let ideal = total / pieces;
    let shortest = ideal * MIN_PIECE_SHARE;

    let mut best: Option<(Cut, f32, usize)> = None;
    let mut last_boundary: Option<usize> = None;
    let mut last_within_budget = start + 1;
    for index in start..sentence_end {
        let units = units_from_start(index + 1);
        if units > budget * SOFT_OVERFLOW {
            break;
        }
        let within_budget = units <= budget;
        if within_budget {
            last_within_budget = index + 1;
        }
        let Some(cut) = cuts[index] else {
            continue;
        };
        // Past the budget only punctuation is worth stretching for, as with sentence ends.
        if !within_budget && cut > Cut::Clause {
            continue;
        }
        if within_budget {
            last_boundary = Some(index + 1);
        }
        // A cut that leaves a very short piece on either side sounds worse than a weaker
        // boundary nearer the middle.
        if units < shortest || total - units < shortest {
            continue;
        }
        let distance = (units - ideal).abs();
        let better = match best {
            None => true,
            Some((best_cut, best_distance, _)) => cut < best_cut || (cut == best_cut && distance < best_distance),
        };
        if better {
            best = Some((cut, distance, index + 1));
        }
    }
    // Otherwise the last boundary that fits, however lopsided; a single word longer than
    // the budget is cut at the budget.
    best.map(|(_, _, end)| end)
        .or(last_boundary)
        .unwrap_or(last_within_budget)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(text: &str, budgets: &[f32]) -> Vec<String> {
        chunk_text(&normalize_for_speech(text), budgets)
    }

    fn squash(text: &str) -> String {
        text.chars().filter(|ch| !ch.is_whitespace()).collect()
    }

    #[test]
    fn abbreviations_decimals_and_urls_do_not_end_sentences() {
        let text = "Dr. Smith paid $3.50 at 10:30 a.m. on Jan. 5, e.g. for coffee. The U.S. economy grew 2.5% in Q3. \
                    Visit example.com/docs for details.";
        assert_eq!(
            chunk(text, &[70.0]),
            vec![
                "Dr. Smith paid $3.50 at 10:30 a.m. on Jan. 5, e.g. for coffee.",
                "The U.S. economy grew 2.5% in Q3. Visit example.com/docs for details.",
            ]
        );
    }

    #[test]
    fn quotes_ellipses_and_versions_stay_intact() {
        let text = "\"Wait... what?\" she asked. \"I didn't know!\" He shrugged; nobody had told her. \
                    Version 2.0.1 shipped yesterday.";
        assert_eq!(
            chunk(text, &[30.0]),
            vec![
                "\"Wait... what?\" she asked.",
                "\"I didn't know!\"",
                "He shrugged; nobody had told her.",
                "Version 2.0.1 shipped yesterday.",
            ]
        );
    }

    #[test]
    fn initials_and_numbered_items_are_not_sentence_ends() {
        assert_eq!(chunk("J. K. Rowling wrote it. Plan No. 5 failed.", &[25.0]).len(), 2);
        assert_eq!(
            chunk("Steps\n1. Download the installer\n2) Run it", &[200.0]),
            vec!["Steps. 1. Download the installer. 2. Run it."]
        );
        assert_eq!(
            chunk("1. Download the installer. 2. Run it.", &[30.0]),
            vec!["1. Download the installer.", "2. Run it."]
        );
    }

    #[test]
    fn wrapped_lines_are_rejoined_and_hyphenation_removed() {
        let text = "Text-to-speech systems con-\nvert written text into spoken\nwords. They are widely used in\naccessibility tools.";
        assert_eq!(
            normalize_for_speech(text),
            "Text-to-speech systems convert written text into spoken words. They are widely used in accessibility tools."
        );
    }

    #[test]
    fn wrapped_lines_starting_with_a_capital_are_rejoined() {
        let text = "The committee met for the first time on a rainy Tuesday in\nParis and decided to postpone the vote until the following\nMonday morning.";
        assert_eq!(
            normalize_for_speech(text),
            "The committee met for the first time on a rainy Tuesday in Paris and decided to postpone the vote until the following Monday morning."
        );
    }

    #[test]
    fn headings_and_bullets_become_sentences() {
        let text = "Getting Started\nInstall the app\n• Download the installer\n• Run setup.exe\n- Restart your computer\n\nYou are now ready.";
        assert_eq!(
            normalize_for_speech(text),
            "Getting Started. Install the app. Download the installer. Run setup.exe. Restart your computer. You are now ready."
        );
    }

    #[test]
    fn single_line_text_only_gains_a_final_stop() {
        assert_eq!(normalize_for_speech("  hello   world "), "hello world.");
        assert_eq!(normalize_for_speech("Is it done?"), "Is it done?");
        assert_eq!(normalize_for_speech("你好\n世界"), "你好。世界。");
    }

    #[test]
    fn long_sentences_split_at_clauses() {
        let text = "When the committee finally met after several months of delays caused by scheduling conflicts and a                     series of unexpected resignations, it decided that the proposal, which had been revised four times                     and reviewed by three separate working groups, should be sent back once more for a detailed cost                     analysis before any vote could be taken.";
        // The comma is preferred over nearer word boundaries, and the rest of the sentence
        // stays whole because it fits within the soft overflow.
        assert_eq!(
            chunk(text, &[150.0]),
            vec![
                "When the committee finally met after several months of delays caused by scheduling conflicts and a series of unexpected resignations,",
                "it decided that the proposal, which had been revised four times and reviewed by three separate working groups, should be sent back once more for a detailed cost analysis before any vote could be taken.",
            ]
        );
        // With a small budget every piece still starts at a comma or a connecting word.
        for piece in &chunk(text, &[60.0])[1..] {
            let first_word = piece.split_whitespace().next().unwrap().to_lowercase();
            let after_comma = text.contains(&format!(", {}", &piece[..piece.len().min(12)]));
            assert!(after_comma || PHRASE_WORDS.contains(&first_word.as_str()), "poor cut before `{piece}`");
        }
    }

    #[test]
    fn without_punctuation_cuts_fall_before_connecting_words() {
        let text = "The proposal had been revised four times by the working group and reviewed by three separate committees \
                    before the board finally agreed to consider it during the spring session";
        let chunks = chunk(text, &[70.0]);
        assert!(chunks.len() >= 3, "{chunks:?}");
        for piece in &chunks[1..] {
            let first_word = piece.split_whitespace().next().unwrap().to_lowercase();
            assert!(PHRASE_WORDS.contains(&first_word.as_str()), "cut before `{first_word}`: {chunks:?}");
        }
    }

    #[test]
    fn early_budgets_shorten_the_first_chunk() {
        let text = "VoiceReader reads text aloud. It runs on your own machine, with no cloud dependency at all. \
                    You can clone a voice once and reuse it for all future speech.";
        let chunks = chunk(text, &[45.0, 200.0]);
        assert_eq!(chunks[0], "VoiceReader reads text aloud.");
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn cjk_and_mixed_text_split_on_cjk_punctuation() {
        let text = "今天天气很好，我们一起去公园散步吧。The weather is nice today. 你觉得怎么样？我觉得不错！";
        assert_eq!(
            chunk(text, &[70.0]),
vec!["今天天气很好，我们一起去公园散步吧。", "The weather is nice today. 你觉得怎么样？", "我觉得不错！"]
        );
    }

    #[test]
    fn no_text_is_lost_and_budgets_hold() {
        let samples = [
            "Dr. Smith paid $3.50 at 10:30 a.m. on Jan. 5, e.g. for coffee. The U.S. economy grew 2.5% in Q3.",
            "A sentence with no punctuation at all that simply keeps going on and on without ever stopping for breath \
             or giving the reader any sign of where a natural pause might reasonably be expected to fall",
            "今天天气很好我们一起去公园散步吧然后去吃饭再去看电影最后回家休息明天还要上班所以不能太晚",
            "Short. Tiny. A. B. Longer sentence here, with a clause; and another one: done!",
        ];
        for sample in samples {
            let normalized = normalize_for_speech(sample);
            for budget in [20.0f32, 45.0, 80.0, 160.0] {
                let chunks = chunk_text(&normalized, &[budget]);
                assert_eq!(squash(&chunks.concat()), squash(&normalized), "text lost at budget {budget}");
                for piece in &chunks {
                    assert!(
                        text_units(piece) <= budget * SOFT_OVERFLOW + 1.0,
                        "over budget {budget}: {piece}"
                    );
                }
            }
        }
    }

    #[test]
    fn unbreakable_text_is_cut_at_the_budget() {
        let chunks = chunk_text(&"a".repeat(100), &[30.0]);
        // The last 40 characters fit within the soft overflow, so they stay together.
        assert_eq!(chunks.iter().map(String::len).collect::<Vec<_>>(), vec![30, 30, 40]);
        assert!(chunk_text("   ", &[30.0]).is_empty());
    }
}
