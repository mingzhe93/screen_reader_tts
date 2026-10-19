//! Turns the model's streamed answer into speakable phrases for text-to-speech.
//!
//! In voice conversation the answer arrives as small text deltas, but speech needs whole
//! sentences or sensible clauses. [`PhraseBuffer`] does two things in one pass:
//!
//! 1. A streaming **cleaner** removes what must not be spoken (`<think>` blocks, fenced code,
//!    Markdown markers, URLs, emoji) and normalises whitespace.
//! 2. A **segmenter** cuts the cleaned text into [`Phrase`]s at sentence ends, clause marks
//!    or, as a last resort, word boundaries.
//!
//! The module is pure: no threads, no I/O, no clock reads (time is passed in). Its key
//! property is split invariance: [`PhraseBuffer::push`] and [`PhraseBuffer::finish`] return the
//! same phrases however the text is divided into deltas. Every decision that needs a look-ahead
//! waits until that look-ahead exists (or the answer has finished), so it never depends on where
//! a delta ended. Only [`PhraseBuffer::tick`] is time dependent, by design. See
//! `docs/assistant/phrases.md`.

/// Spoken in place of a skipped fenced code block, once per block.
const CODE_NOTICE: &str = "I've put the code on screen.";
/// Longest Markdown link text looked for, in characters.
const LINK_TEXT_MAX: usize = 300;
/// Longest Markdown link target looked for, in characters.
const LINK_URL_MAX: usize = 1500;
/// Longest line inspected when deciding whether it is a rule or table separator.
const RULE_LINE_MAX: usize = 200;

/// Sizes and timings that shape the phrases.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PhraseConfig {
    /// Do not emit a phrase shorter than this unless the answer finished.
    pub min_chars: usize,
    /// Cut a longer phrase at a clause boundary, else at a word boundary.
    pub max_chars: usize,
    /// If no new text arrives for this long, `tick` emits what is there.
    pub max_wait_ms: u64,
    /// The first phrase may be shorter, so speech starts sooner.
    pub first_min_chars: usize,
}

impl Default for PhraseConfig {
    fn default() -> Self {
        Self {
            min_chars: 24,
            max_chars: 220,
            max_wait_ms: 1200,
            first_min_chars: 12,
        }
    }
}

/// One piece of cleaned text ready for TTS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Phrase {
    /// 0, 1, 2, ... in speaking order.
    pub seq: u32,
    /// Cleaned text; never empty or whitespace-only.
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Text,
    /// Inside `<think>...</think>`.
    Think,
    /// Inside a fenced code block opened with this fence byte.
    Code(u8),
}

#[derive(Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    NeedMore,
}

enum Prefix {
    /// Input was consumed; stay in line-start mode.
    Consumed,
    NeedMore,
    /// No more line prefix; process the character as inline text.
    Inline,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Tag {
    Full,
    Partial,
    No,
}

enum LinkScan {
    NeedMore,
    NotLink,
    /// `text` is the byte range of the link text inside the scanned slice; `total` the
    /// length of the whole `[text](url)`.
    Link { text: (usize, usize), total: usize },
}

enum RuleScan {
    NeedMore,
    Rule(usize),
    NotRule,
}

enum Find {
    /// The phrase ends at this byte offset of the pending text (exclusive).
    Cut(usize),
    NeedMore,
}

enum Run {
    NeedMore,
    /// A sentence ends at `end` (after closing quotes); the punctuation run ends at `run_end`.
    Boundary { end: usize, run_end: usize },
    No { run_end: usize },
}

enum Abbr {
    No,
    Always,
    UnlessCapitalNext,
    NumberNext,
}

/// Streaming cleaner and phrase segmenter. Create one per answer.
pub(crate) struct PhraseBuffer {
    cfg: PhraseConfig,
    // Cleaner state.
    raw: String,
    mode: Mode,
    at_line_start: bool,
    skip_line: bool,
    in_url: bool,
    prev_raw: Option<char>,
    pipe_due: bool,
    line_has_text: bool,
    last_out: Option<char>,
    /// Whitespace seen after the last cleaned character: `' '` or `'\n'`.
    sep: Option<char>,
    // Segmenter state.
    pending: String,
    scan_byte: usize,
    scan_chars: usize,
    seeking_space: bool,
    seq: u32,
    grew: bool,
    last_growth_ms: u64,
}

impl PhraseBuffer {
    pub(crate) fn new(config: PhraseConfig) -> Self {
        Self {
            cfg: config,
            raw: String::new(),
            mode: Mode::Text,
            at_line_start: true,
            skip_line: false,
            in_url: false,
            prev_raw: None,
            pipe_due: false,
            line_has_text: false,
            last_out: None,
            sep: None,
            pending: String::new(),
            scan_byte: 0,
            scan_chars: 0,
            seeking_space: false,
            seq: 0,
            grew: false,
            last_growth_ms: 0,
        }
    }

    /// Appends model output. `now_ms` is a monotonic time from the caller. Returns phrases that
    /// became complete.
    pub(crate) fn push(&mut self, delta: &str, now_ms: u64) -> Vec<Phrase> {
        let mut out = Vec::new();
        if delta.is_empty() {
            return out;
        }
        self.raw.push_str(delta);
        self.clean(false);
        if self.grew {
            self.grew = false;
            self.last_growth_ms = now_ms;
        }
        self.segment(false, false, &mut out);
        out
    }

    /// Called periodically (for example every 100 ms) while waiting for more text. Returns a
    /// phrase when the pending text has waited too long and can be cut at a sensible point.
    ///
    /// Unlike `push` and `finish` this depends on time, so it can cut where a stream without
    /// the stall would not.
    pub(crate) fn tick(&mut self, now_ms: u64) -> Vec<Phrase> {
        let mut out = Vec::new();
        if now_ms.saturating_sub(self.last_growth_ms) < self.cfg.max_wait_ms {
            return out;
        }
        if self.pending.trim().is_empty() {
            return out;
        }
        // A stalled stream: treat the end of the text as definite, so a final "." counts.
        self.segment(true, false, &mut out);
        self.scan_byte = 0;
        self.scan_chars = 0;
        self.seeking_space = false;
        while let Some(end) = self.stall_cut() {
            self.emit_phrase(end, &mut out);
            self.trim_start();
        }
        out
    }

    /// The answer has ended: flushes everything left as final phrases. The buffer is reset
    /// and can start a new answer.
    pub(crate) fn finish(&mut self) -> Vec<Phrase> {
        self.clean(true);
        self.sep = None;
        let mut out = Vec::new();
        self.segment(true, true, &mut out);
        *self = Self::new(self.cfg);
        out
    }

    // ----------------------------------------------------------------------------------
    // Cleaner
    // ----------------------------------------------------------------------------------

    fn clean(&mut self, eof: bool) {
        let mut raw = std::mem::take(&mut self.raw);
        let mut pos = 0;
        while pos < raw.len() {
            if self.step(&mut raw, &mut pos, eof) == Flow::NeedMore {
                break;
            }
        }
        self.raw = if eof { String::new() } else { raw.split_off(pos) };
    }

    fn step(&mut self, raw: &mut String, pos: &mut usize, eof: bool) -> Flow {
        match self.mode {
            Mode::Think => return self.step_think(raw, pos, eof),
            Mode::Code(fence) => return self.step_code(raw, pos, eof, fence),
            Mode::Text => {}
        }
        if self.skip_line {
            match raw[*pos..].find('\n') {
                Some(i) => {
                    *pos += i;
                    self.skip_line = false;
                }
                None => *pos = raw.len(),
            }
            return Flow::Continue;
        }
        if self.in_url {
            return self.step_url(raw, pos, eof);
        }
        if self.at_line_start {
            match self.line_prefix(raw, pos, eof) {
                Prefix::Consumed => return Flow::Continue,
                Prefix::NeedMore => return Flow::NeedMore,
                Prefix::Inline => {}
            }
        }
        self.step_inline(raw, pos, eof)
    }

    fn step_think(&mut self, raw: &str, pos: &mut usize, eof: bool) -> Flow {
        match raw[*pos..].find('<') {
            None => {
                *pos = raw.len();
                return Flow::Continue;
            }
            Some(i) => *pos += i,
        }
        match match_tag(&raw[*pos..], "</think>") {
            Tag::Full => {
                *pos += "</think>".len();
                self.mode = Mode::Text;
                self.prev_raw = Some(' ');
            }
            Tag::Partial if !eof => return Flow::NeedMore,
            Tag::Partial => *pos = raw.len(),
            Tag::No => *pos += 1,
        }
        Flow::Continue
    }

    fn step_code(&mut self, raw: &str, pos: &mut usize, eof: bool, fence: u8) -> Flow {
        if self.at_line_start {
            let rest = &raw[*pos..];
            let b = rest.as_bytes()[0];
            if b == b' ' || b == b'\t' || b == b'\r' {
                *pos += 1;
                return Flow::Continue;
            }
            if b == fence {
                let run = rest.bytes().take_while(|&x| x == fence).count();
                if run >= 3 {
                    *pos += run;
                    self.mode = Mode::Text;
                    self.skip_line = true;
                    self.at_line_start = false;
                    self.prev_raw = Some(' ');
                    return Flow::Continue;
                }
                if run == rest.len() && !eof {
                    return Flow::NeedMore;
                }
            }
            self.at_line_start = false;
        }
        match raw[*pos..].find('\n') {
            Some(i) => {
                *pos += i + 1;
                self.at_line_start = true;
            }
            None => *pos = raw.len(),
        }
        Flow::Continue
    }

    fn step_url(&mut self, raw: &str, pos: &mut usize, eof: bool) -> Flow {
        let rest = &raw[*pos..];
        let end = rest.find(char::is_whitespace);
        let run = &rest[..end.unwrap_or(rest.len())];
        let core = run.trim_end_matches(is_url_trailing);
        *pos += core.len();
        self.prev_raw = Some('/');
        if end.is_some() || eof {
            // Trailing punctuation stays in the input and is cleaned like normal text.
            self.in_url = false;
            Flow::Continue
        } else if core.len() == run.len() {
            Flow::Continue
        } else {
            Flow::NeedMore
        }
    }

    /// Strips Markdown block prefixes at the start of a line.
    fn line_prefix(&mut self, raw: &mut String, pos: &mut usize, eof: bool) -> Prefix {
        let rest = &raw[*pos..];
        let c = rest.chars().next().expect("step is called with input");
        match c {
            ' ' | '\t' | '\r' => {
                *pos += 1;
                return Prefix::Consumed;
            }
            '\n' => return Prefix::Inline,
            '`' | '~' => {
                let byte = c as u8;
                let run = rest.bytes().take_while(|&x| x == byte).count();
                if run >= 3 {
                    *pos += run;
                    self.mode = Mode::Code(byte);
                    self.at_line_start = false;
                    self.prev_raw = Some(' ');
                    self.notice();
                    return Prefix::Consumed;
                }
                if run == rest.len() && !eof {
                    return Prefix::NeedMore;
                }
            }
            '#' => {
                let run = rest.bytes().take_while(|&x| x == b'#').count();
                if run == rest.len() && !eof {
                    return Prefix::NeedMore;
                }
                if run <= 6 {
                    if let Some(&b) = rest.as_bytes().get(run) {
                        if b == b' ' || b == b'\t' {
                            *pos += run + 1;
                            self.prev_raw = Some(' ');
                            return Prefix::Consumed;
                        }
                    }
                }
            }
            '>' => {
                if rest.len() == 1 && !eof {
                    return Prefix::NeedMore;
                }
                let extra = usize::from(rest.as_bytes().get(1) == Some(&b' '));
                *pos += 1 + extra;
                self.prev_raw = Some(' ');
                return Prefix::Consumed;
            }
            '-' | '*' | '+' | '_' | '=' | '|' | ':' => {
                match rule_scan(rest, eof) {
                    RuleScan::NeedMore => return Prefix::NeedMore,
                    RuleScan::Rule(len) => {
                        *pos += len;
                        self.prev_raw = Some(' ');
                        return Prefix::Consumed;
                    }
                    RuleScan::NotRule => {}
                }
                if matches!(c, '-' | '*' | '+') {
                    match rest.as_bytes().get(1) {
                        None if !eof => return Prefix::NeedMore,
                        Some(b' ') | Some(b'\t') => {
                            *pos += 2;
                            self.prev_raw = Some(' ');
                            return Prefix::Consumed;
                        }
                        _ => {}
                    }
                }
            }
            '0'..='9' => {
                let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                if digits == rest.len() {
                    // Only up to three digits can still become a list prefix ("12. "), so
                    // only then is it worth waiting; a long run is plain text (Codex F11).
                    if !eof && digits <= 3 {
                        return Prefix::NeedMore;
                    }
                } else if digits <= 3 {
                    let m = rest.as_bytes()[digits];
                    if m == b'.' || m == b')' {
                        match rest.as_bytes().get(digits + 1) {
                            None if !eof => return Prefix::NeedMore,
                            Some(b' ') | Some(b'\t') => {
                                *pos += digits + 2;
                                self.prev_raw = Some(' ');
                                return Prefix::Consumed;
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
        self.at_line_start = false;
        Prefix::Inline
    }

    fn step_inline(&mut self, raw: &mut String, pos: &mut usize, eof: bool) -> Flow {
        let rest = &raw[*pos..];
        let Some(c) = rest.chars().next() else {
            return Flow::Continue;
        };
        let clen = c.len_utf8();
        match c {
            '\n' => {
                self.sep = Some('\n');
                self.pipe_due = false;
                self.line_has_text = false;
                self.at_line_start = true;
                self.prev_raw = Some('\n');
                *pos += 1;
            }
            '`' | '*' => {
                *pos += 1;
                self.prev_raw = Some(c);
            }
            '_' => {
                let next = rest[1..].chars().next();
                if next.is_none() && !eof {
                    return Flow::NeedMore;
                }
                let intraword = self.prev_raw.is_some_and(char::is_alphanumeric)
                    && next.is_some_and(char::is_alphanumeric);
                if intraword {
                    self.emit('_');
                }
                *pos += 1;
                self.prev_raw = Some('_');
            }
            '<' => {
                let open = match_tag(rest, "<think>");
                let close = match_tag(rest, "</think>");
                if open == Tag::Full {
                    *pos += "<think>".len();
                    self.mode = Mode::Think;
                    self.prev_raw = Some(' ');
                } else if close == Tag::Full {
                    *pos += "</think>".len();
                    self.prev_raw = Some(' ');
                } else if (open == Tag::Partial || close == Tag::Partial) && !eof {
                    return Flow::NeedMore;
                } else {
                    self.emit('<');
                    *pos += 1;
                    self.prev_raw = Some('<');
                }
            }
            '[' => match scan_link(rest, eof) {
                LinkScan::NeedMore => return Flow::NeedMore,
                LinkScan::NotLink => {
                    self.emit('[');
                    *pos += 1;
                    self.prev_raw = Some('[');
                }
                LinkScan::Link { text, total } => {
                    let t = rest[text.0..text.1].to_string();
                    raw.replace_range(*pos..*pos + total, &t);
                }
            },
            '!' => match rest[1..].chars().next() {
                None if !eof => return Flow::NeedMore,
                Some('[') => match scan_link(&rest[1..], eof) {
                    LinkScan::NeedMore => return Flow::NeedMore,
                    LinkScan::NotLink => {
                        self.emit('!');
                        *pos += 1;
                        self.prev_raw = Some('!');
                    }
                    LinkScan::Link { text, total } => {
                        let t = rest[1 + text.0..1 + text.1].to_string();
                        raw.replace_range(*pos..*pos + 1 + total, &t);
                    }
                },
                _ => {
                    self.emit('!');
                    *pos += 1;
                    self.prev_raw = Some('!');
                }
            },
            '|' => {
                if self.line_has_text {
                    self.pipe_due = true;
                }
                if self.sep.is_none() {
                    self.sep = Some(' ');
                }
                *pos += 1;
                self.prev_raw = Some('|');
            }
            'h' | 'H' | 'w' | 'W' => {
                let (tag, len) = if self.word_start() {
                    url_prefix(rest)
                } else {
                    (Tag::No, 0)
                };
                match tag {
                    Tag::Full => {
                        self.emit_str("a link");
                        *pos += len;
                        self.in_url = true;
                        self.prev_raw = Some('/');
                    }
                    Tag::Partial if !eof => return Flow::NeedMore,
                    _ => {
                        self.emit(c);
                        *pos += clen;
                        self.prev_raw = Some(c);
                    }
                }
            }
            c if c.is_whitespace() || is_pictograph(c) => {
                self.emit_space();
                *pos += clen;
                self.prev_raw = Some(' ');
            }
            c if is_invisible(c) || c.is_control() => *pos += clen,
            c => {
                self.emit(c);
                *pos += clen;
                self.prev_raw = Some(c);
            }
        }
        Flow::Continue
    }

    fn word_start(&self) -> bool {
        match self.prev_raw {
            None => true,
            Some(c) => c.is_whitespace() || "([{<\"'\u{201c}\u{2018}\u{ab}:*_`~".contains(c),
        }
    }

    /// Queues the "code on screen" sentence as a phrase of its own.
    fn notice(&mut self) {
        self.sep = Some('\n');
        self.emit_str(CODE_NOTICE);
        self.sep = Some('\n');
        self.line_has_text = false;
        self.pipe_due = false;
    }

    fn emit_space(&mut self) {
        if self.sep.is_none() {
            self.sep = Some(' ');
        }
    }

    fn emit_str(&mut self, s: &str) {
        for c in s.chars() {
            if c == ' ' {
                self.emit_space();
            } else {
                self.emit(c);
            }
        }
    }

    /// Appends one non-whitespace character to the cleaned text, preceded by any pending
    /// whitespace (collapsed to one space or one newline).
    fn emit(&mut self, c: char) {
        if self.pipe_due {
            self.pipe_due = false;
            if self.line_has_text
                && !matches!(self.last_out, Some(',' | ';' | ':' | '.' | '!' | '?'))
            {
                // Appended after the whitespace, never before it: the segmenter has already
                // looked at that whitespace, so text must not be inserted ahead of it.
                self.pending.push(self.sep.take().unwrap_or(' '));
                self.pending.push(',');
                self.last_out = Some(',');
                self.sep = Some(' ');
            }
        }
        if let Some(s) = self.sep.take() {
            self.pending.push(s);
        }
        self.pending.push(c);
        self.last_out = Some(c);
        self.line_has_text = true;
        self.grew = true;
    }

    // ----------------------------------------------------------------------------------
    // Segmenter
    // ----------------------------------------------------------------------------------

    fn trim_start(&mut self) {
        let k = self.pending.len() - self.pending.trim_start().len();
        if k > 0 {
            self.pending.drain(..k);
            self.scan_byte = 0;
            self.scan_chars = 0;
            self.seeking_space = false;
        }
    }

    /// Emits every phrase that is decidable. `eof` resolves look-ahead at the end of the text
    /// as "nothing follows"; `finishing` also flushes the remainder.
    fn segment(&mut self, eof: bool, finishing: bool, out: &mut Vec<Phrase>) {
        // Whitespace after the last character is visible to the scan as one trailing char.
        let tentative = self.sep;
        if let Some(s) = tentative {
            self.pending.push(s);
        }
        loop {
            self.trim_start();
            if self.pending.is_empty() {
                break;
            }
            match self.find_end(eof, finishing) {
                Find::Cut(end) => self.emit_phrase(end, out),
                Find::NeedMore => break,
            }
        }
        if tentative.is_some() && self.pending.ends_with(char::is_whitespace) {
            self.pending.pop();
        }
        if self.scan_byte > self.pending.len() {
            self.scan_byte = self.pending.len();
            self.scan_chars = self.scan_chars.saturating_sub(1);
        }
    }

    fn emit_phrase(&mut self, end: usize, out: &mut Vec<Phrase>) {
        let text = self.pending[..end]
            .trim_end()
            .replace('\n', " ")
            .replace(" ,", ",");
        self.pending.drain(..end);
        self.scan_byte = 0;
        self.scan_chars = 0;
        self.seeking_space = false;
        // A fragment with nothing to say (for example a lone ",") is dropped.
        if text.chars().any(char::is_alphanumeric) {
            out.push(Phrase {
                seq: self.seq,
                text,
            });
            self.seq += 1;
        }
    }

    fn min_chars(&self) -> usize {
        let min = if self.seq == 0 {
            self.cfg.first_min_chars
        } else {
            self.cfg.min_chars
        };
        min.min(self.cfg.max_chars.max(1))
    }

    /// Finds where the first phrase of the pending text ends. The scan is incremental:
    /// `scan_byte` is the first position not yet decided.
    fn find_end(&mut self, eof: bool, finishing: bool) -> Find {
        let max = self.cfg.max_chars.max(1);
        let min = self.min_chars();
        let p = self.pending.as_str();
        if self.seeking_space {
            return match p[self.scan_byte..].find(char::is_whitespace) {
                Some(i) => Find::Cut(self.scan_byte + i),
                None => {
                    self.scan_byte = p.len();
                    if finishing {
                        Find::Cut(p.len())
                    } else {
                        Find::NeedMore
                    }
                }
            };
        }
        let mut b = self.scan_byte;
        let mut n = self.scan_chars;
        let result = loop {
            if n > max {
                match length_cut(p, min, max) {
                    Cut::At(end) => break Find::Cut(end),
                    Cut::Seek(from) => {
                        self.seeking_space = true;
                        self.scan_byte = from;
                        return self.find_end(eof, finishing);
                    }
                }
            }
            let Some(c) = p[b..].chars().next() else {
                break if finishing {
                    Find::Cut(p.len())
                } else {
                    Find::NeedMore
                };
            };
            if c == '\n' {
                if n >= min && n <= max {
                    break Find::Cut(b);
                }
            } else if is_sentence_punct(c)
                && !p[..b].chars().next_back().is_some_and(is_sentence_punct)
            {
                match resolve_run(p, b, eof) {
                    Run::NeedMore => break Find::NeedMore,
                    Run::Boundary { end, run_end } => {
                        let len = n + p[b..end].chars().count();
                        if len >= min && len <= max {
                            break Find::Cut(end);
                        }
                        n += p[b..run_end].chars().count();
                        b = run_end;
                        continue;
                    }
                    Run::No { run_end } => {
                        n += p[b..run_end].chars().count();
                        b = run_end;
                        continue;
                    }
                }
            }
            b += c.len_utf8();
            n += 1;
        };
        self.scan_byte = b;
        self.scan_chars = n;
        result
    }

    /// Where `tick` may cut stalled text: at the last clause mark, else after the last
    /// complete word. `None` when too little is pending.
    fn stall_cut(&self) -> Option<usize> {
        let p = self.pending.as_str();
        let min = self.min_chars();
        let complete_end = if self.sep.is_some() {
            p.len()
        } else {
            p.rfind(char::is_whitespace)?
        };
        let region = &p[..complete_end];
        if region.chars().count() < min {
            return None;
        }
        let mut best = complete_end;
        for (idx, (bi, c)) in region.char_indices().enumerate() {
            let after = bi + c.len_utf8();
            let clause = match c {
                ',' | ';' | ':' => region[after..]
                    .chars()
                    .next()
                    .map_or(true, char::is_whitespace),
                '\u{2014}' => true,
                _ => false,
            };
            if clause && idx + 1 >= min {
                best = after;
            }
        }
        Some(best)
    }
}

// --------------------------------------------------------------------------------------
// Free helpers (no access to buffer state, so borrows stay simple)
// --------------------------------------------------------------------------------------

enum Cut {
    At(usize),
    /// No cut inside the window: wait for the first whitespace at or after this offset.
    Seek(usize),
}

/// The pending text has more than `max` characters and no sentence end inside. Looks at the
/// first `max + 1` characters only, so the result does not depend on how much more has arrived.
fn length_cut(p: &str, min: usize, max: usize) -> Cut {
    let w: Vec<(usize, char)> = p.char_indices().take(max + 1).collect();
    // Whitespace right after `max` characters means they fit exactly. This is the same cut a
    // newline there would give, and a trailing space may still turn into one.
    if w.len() == max + 1 && w[max].1.is_whitespace() && max >= min {
        return Cut::At(w[max].0);
    }
    for j in (0..max.min(w.len())).rev() {
        let (bi, c) = w[j];
        let clause = match c {
            ',' | ';' | ':' => w.get(j + 1).is_some_and(|&(_, d)| d.is_whitespace()),
            '\u{2014}' | '\u{ff0c}' | '\u{3001}' | '\u{ff1b}' | '\u{ff1a}' => true,
            _ => false,
        };
        if clause && j + 1 >= min {
            return Cut::At(bi + c.len_utf8());
        }
    }
    for j in (1..w.len()).rev() {
        if w[j].1.is_whitespace() && j >= min {
            return Cut::At(w[j].0);
        }
    }
    let w_end = p
        .char_indices()
        .nth(max + 1)
        .map_or(p.len(), |(i, _)| i);
    Cut::Seek(w_end)
}

fn is_sentence_punct(c: char) -> bool {
    matches!(
        c,
        '.' | '!' | '?' | '\u{3002}' | '\u{ff01}' | '\u{ff1f}' | '\u{2026}'
    )
}

fn is_closer(c: char) -> bool {
    matches!(
        c,
        '"' | '\'' | ')' | ']' | '}' | '\u{201d}' | '\u{2019}' | '\u{bb}' | '\u{300d}'
            | '\u{300f}' | '\u{ff09}' | '\u{3011}' | '\u{203a}'
    )
}

/// Decides whether the punctuation run starting at `b` ends a sentence.
fn resolve_run(p: &str, b: usize, eof: bool) -> Run {
    let run_end = match p[b..].find(|c| !is_sentence_punct(c)) {
        Some(i) => b + i,
        None if eof => p.len(),
        None => return Run::NeedMore,
    };
    let run = &p[b..run_end];
    let mut k = run_end;
    for c in p[run_end..].chars() {
        if is_closer(c) {
            k += c.len_utf8();
        } else {
            break;
        }
    }
    if k == p.len() && !eof {
        return Run::NeedMore;
    }
    let fullwidth = run
        .chars()
        .any(|c| matches!(c, '\u{3002}' | '\u{ff01}' | '\u{ff1f}'));
    if fullwidth {
        return Run::Boundary { end: k, run_end };
    }
    let run_chars = run.chars().count();
    let ellipsis = run.chars().all(|c| c == '.' || c == '\u{2026}')
        && (run_chars >= 2 || run.contains('\u{2026}'));
    if ellipsis {
        return match p[k..].chars().find(|c| !c.is_whitespace()) {
            None if eof => Run::Boundary { end: k, run_end },
            None => Run::NeedMore,
            Some(c) if c.is_uppercase() => Run::Boundary { end: k, run_end },
            Some(_) => Run::No { run_end },
        };
    }
    match p[k..].chars().next() {
        None => return Run::Boundary { end: k, run_end },
        Some(c) if !c.is_whitespace() => return Run::No { run_end },
        Some(_) => {}
    }
    if run == "." {
        let next_word = p[k..].chars().find(|c| !c.is_whitespace());
        match abbreviation(&p[..b]) {
            Abbr::No => {}
            Abbr::Always => return Run::No { run_end },
            Abbr::UnlessCapitalNext => {
                return match next_word {
                    None if eof => Run::Boundary { end: k, run_end },
                    None => Run::NeedMore,
                    Some(c) if c.is_uppercase() => Run::Boundary { end: k, run_end },
                    Some(_) => Run::No { run_end },
                };
            }
            Abbr::NumberNext => {
                return match next_word {
                    None if eof => Run::Boundary { end: k, run_end },
                    None => Run::NeedMore,
                    Some(c) if c.is_ascii_digit() => Run::No { run_end },
                    Some(_) => Run::Boundary { end: k, run_end },
                };
            }
        }
    }
    Run::Boundary { end: k, run_end }
}

/// Classifies the word that ends just before a period.
fn abbreviation(before: &str) -> Abbr {
    let token = before
        .rsplit(char::is_whitespace)
        .next()
        .unwrap_or("")
        .trim_start_matches(|c| "(\"'[{\u{201c}\u{2018}\u{ab}<".contains(c));
    if token.is_empty() {
        return Abbr::No;
    }
    let lower = token.to_ascii_lowercase();
    if matches!(lower.as_str(), "e.g" | "i.e" | "vs" | "cf" | "a.m" | "p.m") {
        return Abbr::Always;
    }
    if lower == "etc" {
        return Abbr::UnlessCapitalNext;
    }
    if matches!(
        token,
        "Mr" | "Mrs" | "Ms" | "Dr" | "St" | "Prof" | "Jr" | "Sr" | "Fig" | "Figs"
    ) {
        return Abbr::Always;
    }
    if token == "No" {
        return Abbr::NumberNext;
    }
    // Single capital initial ("J."), but not the pronoun "I".
    let mut chars = token.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_uppercase() && c != 'I' {
            return Abbr::Always;
        }
    }
    // Dotted initialisms such as "U.S" or "Ph.D".
    if token.contains('.') {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() >= 2 && parts.iter().all(|s| s.len() == 1 && s.as_bytes()[0].is_ascii_alphabetic())
        {
            return Abbr::Always;
        }
    }
    Abbr::No
}

/// Case-insensitive ASCII comparison of the start of `rest` with `tag`.
fn match_tag(rest: &str, tag: &str) -> Tag {
    let r = rest.as_bytes();
    let t = tag.as_bytes();
    if r.len() >= t.len() {
        if r[..t.len()].eq_ignore_ascii_case(t) {
            Tag::Full
        } else {
            Tag::No
        }
    } else if r.eq_ignore_ascii_case(&t[..r.len()]) {
        Tag::Partial
    } else {
        Tag::No
    }
}

/// Checks for `http://`, `https://` or `www.` at the start of `rest`.
fn url_prefix(rest: &str) -> (Tag, usize) {
    let mut partial = false;
    for tag in ["http://", "https://", "www."] {
        match match_tag(rest, tag) {
            Tag::Full => return (Tag::Full, tag.len()),
            Tag::Partial => partial = true,
            Tag::No => {}
        }
    }
    (if partial { Tag::Partial } else { Tag::No }, 0)
}

fn is_url_trailing(c: char) -> bool {
    matches!(
        c,
        '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\'' | '"' | '\u{201d}'
            | '\u{2019}' | '\u{bb}' | '>' | '*' | '_' | '`' | '~'
    )
}

/// Looks for `[text](target)` at the start of `s`.
fn scan_link(s: &str, eof: bool) -> LinkScan {
    let mut close = None;
    for (count, (i, c)) in s.char_indices().skip(1).enumerate() {
        if count >= LINK_TEXT_MAX {
            return LinkScan::NotLink;
        }
        match c {
            ']' => {
                close = Some(i);
                break;
            }
            '[' | '\n' => return LinkScan::NotLink,
            _ => {}
        }
    }
    let Some(close) = close else {
        return if eof { LinkScan::NotLink } else { LinkScan::NeedMore };
    };
    let after = &s[close + 1..];
    match after.chars().next() {
        None => return if eof { LinkScan::NotLink } else { LinkScan::NeedMore },
        Some('(') => {}
        Some(_) => return LinkScan::NotLink,
    }
    let mut depth = 0usize;
    for (count, (i, c)) in after.char_indices().enumerate() {
        if count > LINK_URL_MAX {
            return LinkScan::NotLink;
        }
        match c {
            '\n' => return LinkScan::NotLink,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return LinkScan::Link {
                        text: (1, close),
                        total: close + 1 + i + 1,
                    };
                }
            }
            _ => {}
        }
    }
    if eof {
        LinkScan::NotLink
    } else {
        LinkScan::NeedMore
    }
}

/// Is the line starting at `rest` a horizontal rule or a table separator row?
fn rule_scan(rest: &str, eof: bool) -> RuleScan {
    let mut count = 0;
    for (scanned, (i, c)) in rest.char_indices().enumerate() {
        if c == '\n' {
            return if count >= 3 {
                RuleScan::Rule(i)
            } else {
                RuleScan::NotRule
            };
        }
        if !matches!(c, '|' | '-' | ':' | '*' | '_' | '=' | '+' | ' ' | '\t' | '\r') {
            return RuleScan::NotRule;
        }
        if !matches!(c, ' ' | '\t' | '\r') {
            count += 1;
        }
        if scanned >= RULE_LINE_MAX {
            return RuleScan::NotRule;
        }
    }
    if !eof {
        RuleScan::NeedMore
    } else if count >= 3 {
        RuleScan::Rule(rest.len())
    } else {
        RuleScan::NotRule
    }
}

/// Emoji and other pictographs, treated as whitespace.
fn is_pictograph(c: char) -> bool {
    matches!(c as u32,
        0x1F000..=0x1FAFF
        | 0x2300..=0x23FF
        | 0x25A0..=0x25FF
        | 0x2600..=0x27BF
        | 0x2B00..=0x2BFF
        | 0x3030 | 0x303D | 0x3297 | 0x3299)
}

/// Zero-width and variation characters: removed without leaving a space.
fn is_invisible(c: char) -> bool {
    matches!(c as u32,
        0x200B..=0x200F | 0x2060 | 0xFE00..=0xFE0F | 0xFEFF | 0x20E3 | 0xE0020..=0xE007F)
}

// --------------------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_streamed_digit_run_does_not_wait_or_rescan() {
        let mut buffer = PhraseBuffer::new(PhraseConfig::default());
        let started = std::time::Instant::now();
        let mut phrases = Vec::new();
        for _ in 0..20_000 {
            phrases.extend(buffer.push("7", 0));
        }
        phrases.extend(buffer.push(" is the number.", 0));
        phrases.extend(buffer.finish());
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "took {:?}", started.elapsed());
        let text: String = phrases.iter().map(|phrase| phrase.text.as_str()).collect::<Vec<_>>().join(" ");
        assert!(text.contains("is the number"), "{text}");
    }

    fn tiny() -> PhraseConfig {
        PhraseConfig {
            min_chars: 1,
            max_chars: 220,
            max_wait_ms: 1200,
            first_min_chars: 1,
        }
    }

    fn small() -> PhraseConfig {
        PhraseConfig {
            min_chars: 8,
            max_chars: 40,
            max_wait_ms: 1200,
            first_min_chars: 4,
        }
    }

    fn run_chunks<'a>(cfg: PhraseConfig, chunks: impl IntoIterator<Item = &'a str>) -> Vec<Phrase> {
        let mut buf = PhraseBuffer::new(cfg);
        let mut out = Vec::new();
        for (i, c) in chunks.into_iter().enumerate() {
            out.extend(buf.push(c, i as u64 * 7));
        }
        out.extend(buf.finish());
        out
    }

    fn texts(cfg: PhraseConfig, input: &str) -> Vec<String> {
        run_chunks(cfg, [input]).into_iter().map(|p| p.text).collect()
    }

    fn chars_of(s: &str) -> Vec<String> {
        s.chars().map(String::from).collect()
    }

    fn words_of(s: &str) -> Vec<String> {
        s.split_inclusive(' ').map(String::from).collect()
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
    }

    fn random_chunks(s: &str, seed: u64, max_len: u64) -> Vec<String> {
        let mut rng = Lcg(seed);
        let chars: Vec<char> = s.chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            let len = 1 + (rng.next() % max_len) as usize;
            let end = (i + len).min(chars.len());
            out.push(chars[i..end].iter().collect());
            i = end;
        }
        out
    }

    fn check_valid(phrases: &[Phrase]) {
        for (i, p) in phrases.iter().enumerate() {
            assert_eq!(p.seq as usize, i, "seq must count up");
            assert!(!p.text.trim().is_empty(), "empty phrase at {i}");
            assert_eq!(p.text, p.text.trim());
            assert!(!p.text.contains('\n'));
        }
    }

    /// Runs the text under many split patterns and asserts the phrases never change.
    fn assert_invariant(cfg: PhraseConfig, input: &str) {
        let whole = run_chunks(cfg, [input]);
        check_valid(&whole);
        let check = |name: &str, chunks: Vec<String>| {
            let got = run_chunks(cfg, chunks.iter().map(String::as_str));
            check_valid(&got);
            assert_eq!(got, whole, "split pattern {name} changed the phrases for {input:?}");
        };
        check("chars", chars_of(input));
        check("words", words_of(input));
        for seed in 1..=12u64 {
            check(&format!("lcg{seed}/3"), random_chunks(input, seed, 3));
            check(&format!("lcg{seed}/9"), random_chunks(input, seed * 31, 9));
            check(&format!("lcg{seed}/40"), random_chunks(input, seed * 977, 40));
        }
        // Every two-way split.
        for (i, _) in input.char_indices().skip(1) {
            let got = run_chunks(cfg, [&input[..i], &input[i..]]);
            assert_eq!(got, whole, "two-way split at {i} changed the phrases for {input:?}");
        }
    }

    const PROSE: &str = "Sure, I can help with that. The capital of France is Paris, a city of about 2.1 million people. \
It sits on the Seine, and it has been a centre of art for centuries! Have you been there? \
Dr. Smith and Mr. Jones went there in v0.3.0 of the story, e.g. in the spring, at 10.30 sharp. \
J. R. R. Tolkien never did, but he wrote well... Then again, who knows? So do I. Done.";

    const LIST: &str = "Here are three things:\n\n- First, buy milk.\n- Second, call Dr. Lee about the 3.5 mm bolt.\n* Third, relax\n1. One more item to do today\n2) And another one right here\nThat is all, thanks.";

    const CODE: &str = "Here is how to do it:\n\n```python\ndef f(x):\n    return x * 2  # not. a. sentence\n```\n\nThat doubles the number. Try it!\n\n```\nsecond block\n```\nAnd that is it.";

    const THINK: &str = "<think>The user wants a greeting. Maybe I should say hello. Or not.</think>Hello there, friend. How can I help you today? <think>again</think>Let me know.";

    const TABLE: &str = "Results below:\n\n| Name | Score |\n|------|-------|\n| Ann | 90 |\n| Bob | 85 |\n\nAnn won the round, clearly.";

    const URLS: &str = "Visit https://example.com/a.b/c?d=1&e=2. Then email john.doe@example.com or see [the docs](https://docs.example.com/x_(y)) and www.example.org, please. Also [https://x.io](https://x.io) works.";

    const MARKUP: &str = "# Title here\n\nThis is **bold**, *italic*, __also bold__ and _under_ with snake_case_name kept. Use `inline code` now.\n> A quoted line that goes on.\n\n---\n\nEmoji \u{1F389} are dropped \u{2764}\u{fe0f} cleanly ![alt text](img.png) done.";

    fn corpus() -> Vec<&'static str> {
        vec![PROSE, LIST, CODE, THINK, TABLE, URLS, MARKUP]
    }

    // --- Split invariance ----------------------------------------------------------------

    #[test]
    fn split_invariance_default_config() {
        for text in corpus() {
            assert_invariant(PhraseConfig::default(), text);
        }
    }

    #[test]
    fn split_invariance_tiny_and_small_config() {
        for text in corpus() {
            assert_invariant(tiny(), text);
            assert_invariant(small(), text);
        }
    }

    #[test]
    fn split_invariance_all_three_way_splits_on_tricky_text() {
        let text = "Cost is 3.14 e.g. now... Then \"Go.\" No. 5 etc. Next [a](b) <think>x</think>_i_ *\n- a\nhttps://x.y/z. ok";
        let cfg = tiny();
        let whole = run_chunks(cfg, [text]);
        let idx: Vec<usize> = text.char_indices().map(|(i, _)| i).chain([text.len()]).collect();
        for (a, &i) in idx.iter().enumerate() {
            for &j in &idx[a..] {
                let got = run_chunks(cfg, [&text[..i], &text[i..j], &text[j..]]);
                assert_eq!(got, whole, "three-way split {i},{j}");
            }
        }
    }

    #[test]
    fn split_invariance_fuzz() {
        let atoms = [
            ".", "!", "?", " ", " ", " ", "\n", "\n\n", "*", "**", "_", "`", "```", "[", "]", "(", ")",
            "<", ">", "-", "#", "|", "1", "3", "a", "b", "Hello", "world", "Dr", "e.g", "etc", "No",
            "http://", "https://x.com", "www.", "<think>", "</think>", "think", "\u{1F389}", "\u{2026}",
            "...", ", ", "; ", ": ", "\u{2014}", "\"", "'", "\u{3002}", "I", "J", "~~~", "ht", "T", "](", "![", "Mr", ". ", "- ", "1. ", "> ", "| ", "\u{a0}", "_x_", "foo", "bar baz",
        ];
        let mut rng = Lcg(42);
        for case in 0..2000u64 {
            let len = 5 + (rng.next() % 60) as usize;
            let mut s = String::new();
            for _ in 0..len {
                s.push_str(atoms[(rng.next() as usize) % atoms.len()]);
            }
            let cfg = match case % 4 {
                0 => tiny(),
                1 => small(),
                2 => PhraseConfig { min_chars: 3, max_chars: 15, max_wait_ms: 1200, first_min_chars: 2 },
                _ => PhraseConfig::default(),
            };
            let whole = run_chunks(cfg, [s.as_str()]);
            check_valid(&whole);
            let by_char = run_chunks(cfg, chars_of(&s).iter().map(String::as_str));
            assert_eq!(by_char, whole, "chars differ for {s:?} cfg {case}");
            let rnd = random_chunks(&s, case + 7, 11);
            let by_rnd = run_chunks(cfg, rnd.iter().map(String::as_str));
            assert_eq!(by_rnd, whole, "random chunks differ for {s:?} cfg {case}");
        }
    }

    #[test]
    fn push_times_do_not_change_phrases() {
        let cfg = PhraseConfig::default();
        let mut a = PhraseBuffer::new(cfg);
        let mut b = PhraseBuffer::new(cfg);
        let mut out_a = Vec::new();
        let mut out_b = Vec::new();
        for (i, c) in chars_of(PROSE).iter().enumerate() {
            out_a.extend(a.push(c, i as u64));
            out_b.extend(b.push(c, (i as u64) * 5000));
        }
        out_a.extend(a.finish());
        out_b.extend(b.finish());
        assert_eq!(out_a, out_b);
    }

    #[test]
    fn plain_prose_is_preserved() {
        // No markup in this text, so the phrases joined with spaces are the input.
        let input = "The quick brown fox jumps over the lazy dog. It was a sunny day in the park, and everyone \
was happy! Dr. Who arrived at 3.15 with a book.\nThe end of the story is near, so hold on tight.";
        let got = texts(PhraseConfig::default(), input).join(" ");
        let expect = input.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(got, expect);
        let got = texts(small(), input).join(" ");
        assert_eq!(got, expect);
    }

    // --- Rule 1: boundaries ----------------------------------------------------------------

    #[test]
    fn splits_at_sentence_punctuation() {
        let got = texts(tiny(), "Hello there. How are you? I am fine! Great");
        assert_eq!(got, ["Hello there.", "How are you?", "I am fine!", "Great"]);
    }

    #[test]
    fn does_not_split_decimals_versions_times() {
        assert_eq!(texts(tiny(), "Pi is 3.14 today. Next."), ["Pi is 3.14 today.", "Next."]);
        assert_eq!(
            texts(tiny(), "Use v0.3.0 now. The talk is at 10.30 sharp. Bye."),
            ["Use v0.3.0 now.", "The talk is at 10.30 sharp.", "Bye."]
        );
        // Held across deltas until the next character decides.
        let got = run_chunks(tiny(), ["Pi is 3.", "14 and more. Done."]);
        assert_eq!(got.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), ["Pi is 3.14 and more.", "Done."]);
    }

    #[test]
    fn does_not_split_abbreviations() {
        let t = "Mr. Smith and Dr. Jones met Mrs. Lee, Ms. Ray and St. John vs. the rest, e.g. Paris, i.e. France, etc. and more.";
        assert_eq!(texts(tiny(), t), [t]);
        assert_eq!(texts(tiny(), "J. R. R. Tolkien wrote. Next."), ["J. R. R. Tolkien wrote.", "Next."]);
        assert_eq!(texts(tiny(), "See Fig. 2 for it. Next."), ["See Fig. 2 for it.", "Next."]);
        assert_eq!(texts(tiny(), "The U.S. is big. Next."), ["The U.S. is big.", "Next."]);
    }

    #[test]
    fn abbreviation_edge_cases_still_split_when_they_should() {
        assert_eq!(texts(tiny(), "So do I. Then we left."), ["So do I.", "Then we left."]);
        assert_eq!(texts(tiny(), "Lots of fruit, etc. Then we left."), ["Lots of fruit, etc.", "Then we left."]);
        assert_eq!(texts(tiny(), "Room No. 5 is open. Next."), ["Room No. 5 is open.", "Next."]);
        assert_eq!(texts(tiny(), "No. I disagree."), ["No.", "I disagree."]);
    }

    #[test]
    fn ellipsis_splits_only_before_a_capital() {
        assert_eq!(texts(tiny(), "Well... maybe not. Ok."), ["Well... maybe not.", "Ok."]);
        assert_eq!(texts(tiny(), "Well... Maybe not."), ["Well...", "Maybe not."]);
        assert_eq!(texts(tiny(), "Well\u{2026} Maybe not."), ["Well\u{2026}", "Maybe not."]);
    }

    #[test]
    fn urls_and_emails_do_not_split() {
        let got = texts(tiny(), "Visit https://example.com/a.b. Then mail john.doe@example.com. Ok.");
        assert_eq!(got, ["Visit a link.", "Then mail john.doe@example.com.", "Ok."]);
    }

    #[test]
    fn newline_ends_a_phrase() {
        assert_eq!(texts(tiny(), "First line\nSecond line\n\n\nThird"), ["First line", "Second line", "Third"]);
    }

    #[test]
    fn newline_is_not_a_boundary_inside_blank_stream() {
        assert_eq!(texts(tiny(), "\n\n  Hello.\n\n"), ["Hello."]);
    }

    #[test]
    fn closing_quotes_stay_with_the_sentence() {
        assert_eq!(
            texts(tiny(), "He said \"Go.\" Then he left. (Really!) Fine."),
            ["He said \"Go.\"", "Then he left.", "(Really!)", "Fine."]
        );
    }

    #[test]
    fn full_width_punctuation_ends_a_phrase() {
        assert_eq!(texts(tiny(), "\u{4f60}\u{597d}\u{3002}\u{518d}\u{89c1}！\u{6700}\u{540e}"), ["\u{4f60}\u{597d}\u{3002}", "\u{518d}\u{89c1}！", "\u{6700}\u{540e}"]);
    }

    #[test]
    fn trailing_punctuation_is_held_until_the_next_char() {
        let mut buf = PhraseBuffer::new(tiny());
        assert!(buf.push("It costs 3.", 0).is_empty());
        assert!(buf.push("14 dollars.", 1).is_empty());
        let out = buf.push(" Next", 2);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "It costs 3.14 dollars.");
    }

    // --- Rule 2: clause cuts ---------------------------------------------------------------

    #[test]
    fn long_sentence_is_cut_at_a_clause() {
        let cfg = PhraseConfig { min_chars: 10, max_chars: 50, max_wait_ms: 1200, first_min_chars: 10 };
        let input = "This is a rather long sentence, with several clauses in it, and it keeps going on and on without an end in sight";
        let got = texts(cfg, input);
        assert!(got.len() >= 2);
        assert_eq!(got[0], "This is a rather long sentence,");
        for g in &got[..got.len() - 1] {
            assert!(g.chars().count() <= 50, "{g}");
        }
        assert_eq!(got.join(" "), input);
    }

    #[test]
    fn long_text_without_clauses_is_cut_at_a_word() {
        let cfg = PhraseConfig { min_chars: 10, max_chars: 30, max_wait_ms: 1200, first_min_chars: 10 };
        let input = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau";
        let got = texts(cfg, input);
        assert!(got.len() >= 3);
        for g in &got {
            assert!(g.chars().count() <= 30, "{g}");
            for w in g.split(' ') {
                assert!(input.split(' ').any(|x| x == w), "split inside a word: {w}");
            }
        }
        assert_eq!(got.join(" "), input);
    }

    #[test]
    fn a_word_longer_than_max_is_never_split() {
        let cfg = PhraseConfig { min_chars: 5, max_chars: 20, max_wait_ms: 1200, first_min_chars: 5 };
        let giant = "x".repeat(100);
        let input = format!("Hi there {giant} and more");
        let mut buf = PhraseBuffer::new(cfg);
        let mut out = Vec::new();
        for c in chars_of(&input) {
            out.extend(buf.push(&c, 0));
        }
        // The giant word is still arriving or just ended: nothing may have split it.
        for p in &out {
            assert!(!p.text.contains(&giant[..30]) || p.text.contains(&giant));
        }
        out.extend(buf.finish());
        let joined = out.iter().map(|p| p.text.as_str()).collect::<Vec<_>>().join(" ");
        assert_eq!(joined, input);
        assert!(out.iter().any(|p| p.text.contains(&giant)));
    }

    #[test]
    fn colon_and_comma_inside_numbers_are_not_clause_marks() {
        let cfg = PhraseConfig { min_chars: 4, max_chars: 20, max_wait_ms: 1200, first_min_chars: 4 };
        let got = texts(cfg, "at 10:30 or 1,000,000 ok then maybe");
        for g in &got {
            assert!(!g.ends_with("10:") && !g.ends_with("1,") && !g.ends_with("1,000,"), "{g}");
        }
    }

    // --- Rule 3: minimum size ----------------------------------------------------------------

    #[test]
    fn short_sentences_are_joined() {
        let got = texts(PhraseConfig::default(), "Yes. I can do that for you right now. Good.");
        assert_eq!(got, ["Yes. I can do that for you right now.", "Good."]);
    }

    #[test]
    fn finish_may_emit_a_short_phrase() {
        assert_eq!(texts(PhraseConfig::default(), "Yes."), ["Yes."]);
    }

    #[test]
    fn first_phrase_uses_the_smaller_minimum() {
        let got = texts(PhraseConfig::default(), "Hello there. How are you doing today my friend?");
        assert_eq!(got, ["Hello there.", "How are you doing today my friend?"]);
        // The second phrase needs 24.
        let got = texts(PhraseConfig::default(), "Hello there. Fine thanks. And you my good friend?");
        assert_eq!(got, ["Hello there.", "Fine thanks. And you my good friend?"]);
    }

    #[test]
    fn short_phrase_not_emitted_while_streaming() {
        let mut buf = PhraseBuffer::new(PhraseConfig::default());
        assert!(buf.push("Yes. ", 0).is_empty());
        assert!(buf.push("No.", 1).is_empty());
        assert_eq!(buf.finish().len(), 1);
    }

    // --- Rule 4: speakable filter ------------------------------------------------------------

    #[test]
    fn drops_think_blocks() {
        let got = texts(tiny(), THINK);
        assert_eq!(got, ["Hello there, friend.", "How can I help you today?", "Let me know."]);
    }

    #[test]
    fn think_tags_split_across_deltas() {
        let got = run_chunks(tiny(), ["Before. <th", "ink>secret. stuff</thi", "nk> After."]);
        assert_eq!(got.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), ["Before.", "After."]);
    }

    #[test]
    fn unfinished_think_is_dropped_and_lookalike_tags_kept() {
        assert_eq!(texts(tiny(), "Hi. <think>never closed. text"), ["Hi."]);
        assert_eq!(texts(tiny(), "Use a < b and <b>bold</b> ok"), ["Use a < b and <b>bold</b> ok"]);
    }

    #[test]
    fn skips_code_blocks_with_one_notice_each() {
        let got = texts(tiny(), CODE);
        assert_eq!(
            got,
            [
                "Here is how to do it:",
                "I've put the code on screen.",
                "That doubles the number.",
                "Try it!",
                "I've put the code on screen.",
                "And that is it."
            ]
        );
    }

    #[test]
    fn unclosed_code_block_is_skipped_to_the_end() {
        let got = texts(tiny(), "Look:\n```rust\nfn main() {}\nmore code");
        assert_eq!(got, ["Look:", "I've put the code on screen."]);
    }

    #[test]
    fn inline_triple_backticks_are_not_a_block() {
        assert_eq!(texts(tiny(), "Use ```x``` here."), ["Use x here."]);
    }

    #[test]
    fn code_fence_split_across_deltas() {
        let got = run_chunks(tiny(), ["Look:\n`", "``py\ncode\n``", "`\nDone."]);
        assert_eq!(
            got.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            ["Look:", "I've put the code on screen.", "Done."]
        );
    }

    #[test]
    fn strips_markdown_markers() {
        let got = texts(tiny(), MARKUP);
        assert_eq!(
            got,
            [
                "Title here",
                "This is bold, italic, also bold and under with snake_case_name kept.",
                "Use inline code now.",
                "A quoted line that goes on.",
                "Emoji are dropped cleanly alt text done."
            ]
        );
    }

    #[test]
    fn markers_split_across_deltas() {
        let got = run_chunks(tiny(), ["This is **bo", "ld** and `co", "de` and _it", "alic_ text.\n- it", "em one\n1", "0. item two"]);
        assert_eq!(
            got.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            ["This is bold and code and italic text.", "item one", "item two"]
        );
    }

    #[test]
    fn underscores_inside_words_are_kept() {
        assert_eq!(texts(tiny(), "Call my_function_name now."), ["Call my_function_name now."]);
        assert_eq!(texts(tiny(), "Call _this_ now."), ["Call this now."]);
    }

    #[test]
    fn list_bullets_and_numbers() {
        let got = texts(tiny(), LIST);
        assert_eq!(
            got,
            [
                "Here are three things:",
                "First, buy milk.",
                "Second, call Dr. Lee about the 3.5 mm bolt.",
                "Third, relax",
                "One more item to do today",
                "And another one right here",
                "That is all, thanks."
            ]
        );
    }

    #[test]
    fn numbers_and_dashes_at_line_start_are_kept() {
        assert_eq!(texts(tiny(), "3.14 is pi.\n-5 degrees\n2024 was good"), ["3.14 is pi.", "-5 degrees", "2024 was good"]);
    }

    #[test]
    fn table_pipes_become_commas() {
        let got = texts(tiny(), TABLE);
        assert_eq!(
            got,
            ["Results below:", "Name, Score", "Ann, 90", "Bob, 85", "Ann won the round, clearly."]
        );
    }

    #[test]
    fn links_and_urls() {
        let got = texts(tiny(), URLS);
        assert_eq!(
            got,
            [
                "Visit a link.",
                "Then email john.doe@example.com or see the docs and a link, please.",
                "Also a link works."
            ]
        );
    }

    #[test]
    fn url_split_across_deltas_and_trailing_punctuation() {
        let got = run_chunks(tiny(), ["Go to ht", "tps://exa", "mple.com/a.", "b, now. And **www.x.org**. Bye"]);
        assert_eq!(
            got.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            ["Go to a link, now.", "And a link.", "Bye"]
        );
    }

    #[test]
    fn words_starting_with_h_or_w_are_not_urls() {
        assert_eq!(texts(tiny(), "What http is, why www"), ["What http is, why www"]);
        assert_eq!(texts(tiny(), "Say hello to the whole world."), ["Say hello to the whole world."]);
    }

    #[test]
    fn rules_and_table_separators_are_dropped() {
        assert_eq!(texts(tiny(), "Above\n---\nBelow\n***\n| - | - |\nEnd"), ["Above", "Below", "End"]);
    }

    #[test]
    fn collapses_whitespace_and_drops_emoji() {
        assert_eq!(texts(tiny(), "  Great   job \u{1F389}\u{1F389}  team \t\r\n done "), ["Great job team", "done"]);
        assert_eq!(texts(tiny(), "Fine\u{1F44D}\u{1F3FD}job \u{2764}\u{fe0f} ok"), ["Fine job ok"]);
        assert_eq!(texts(tiny(), "zero\u{200b}width"), ["zerowidth"]);
    }

    #[test]
    fn heading_markers() {
        assert_eq!(texts(tiny(), "## Section two\nText."), ["Section two", "Text."]);
        assert_eq!(texts(tiny(), "#hashtag and #1 stay"), ["#hashtag and #1 stay"]);
    }

    // --- Rule 5 and 6 ------------------------------------------------------------------------

    #[test]
    fn seq_counts_up_and_phrases_are_never_empty() {
        for text in corpus() {
            let got = run_chunks(small(), chars_of(text).iter().map(String::as_str));
            check_valid(&got);
            assert!(!got.is_empty());
        }
    }

    #[test]
    fn no_text_lost_with_default_config() {
        let got = texts(PhraseConfig::default(), PROSE).join(" ");
        let expect = PROSE.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(got, expect);
    }

    #[test]
    fn buffer_is_reusable_after_finish() {
        let mut buf = PhraseBuffer::new(tiny());
        let mut a = buf.push("One. Two", 0);
        a.extend(buf.finish());
        let mut b = buf.push("One. Two", 0);
        b.extend(buf.finish());
        assert_eq!(a, b);
        assert_eq!(a[0].seq, 0);
    }

    #[test]
    fn empty_input_gives_nothing() {
        let mut buf = PhraseBuffer::new(PhraseConfig::default());
        assert!(buf.push("", 0).is_empty());
        assert!(buf.push("   \n ", 1).is_empty());
        assert!(buf.tick(5000).is_empty());
        assert!(buf.finish().is_empty());
    }

    #[test]
    fn linear_performance() {
        let unit = "Hello, this is a sentence with 3.14 in it, and Dr. Who. \n- item **bold** `code` https://x.com/a. ";
        let mut text = String::new();
        while text.len() < 50_000 {
            text.push_str(unit);
        }
        let chars = chars_of(&text);
        let start = std::time::Instant::now();
        let mut buf = PhraseBuffer::new(PhraseConfig::default());
        let mut count = 0;
        for c in &chars {
            count += buf.push(c, 0).len();
        }
        count += buf.finish().len();
        let ms = start.elapsed().as_millis();
        assert!(count > 100);
        let bound = if cfg!(debug_assertions) { 3000 } else { 50 };
        assert!(ms < bound, "took {ms} ms");
    }

    // --- Timing --------------------------------------------------------------------------------

    #[test]
    fn tick_waits_for_max_wait() {
        let mut buf = PhraseBuffer::new(PhraseConfig::default());
        assert!(buf.push("I think we should probably go to the ", 1000).is_empty());
        assert!(buf.tick(1100).is_empty());
        assert!(buf.tick(2199).is_empty());
        let out = buf.tick(2200);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "I think we should probably go to the");
    }

    #[test]
    fn tick_never_cuts_inside_a_word() {
        let mut buf = PhraseBuffer::new(PhraseConfig::default());
        assert!(buf.push("I think we should probably go to the sto", 0).is_empty());
        let out = buf.tick(5000);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "I think we should probably go to the");
        // The unfinished word is still pending and completes later.
        let mut rest = buf.push("re now.", 5100);
        rest.extend(buf.finish());
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].text, "store now.");
    }

    #[test]
    fn tick_prefers_a_clause_cut() {
        let mut buf = PhraseBuffer::new(PhraseConfig::default());
        buf.push("When the weather is nice today, we will go to the beach and ", 0);
        let out = buf.tick(3000);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "When the weather is nice today,");
        assert_eq!(out[1].text, "we will go to the beach and");
    }

    #[test]
    fn tick_below_minimum_emits_nothing() {
        let mut buf = PhraseBuffer::new(PhraseConfig::default());
        buf.push("Well, hello ", 0);
        assert!(buf.tick(5000).is_empty());
        let mut out = buf.push("my dear friend.", 5100);
        out.extend(buf.finish());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "Well, hello my dear friend.");
    }

    #[test]
    fn tick_resolves_a_held_final_period() {
        let mut buf = PhraseBuffer::new(PhraseConfig::default());
        assert!(buf.push("The answer to your question is forty two.", 0).is_empty());
        let out = buf.tick(1500);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "The answer to your question is forty two.");
        assert!(buf.tick(3000).is_empty());
        assert!(buf.finish().is_empty());
    }

    #[test]
    fn tick_with_nothing_pending_does_nothing() {
        let mut buf = PhraseBuffer::new(PhraseConfig::default());
        assert!(buf.tick(10_000).is_empty());
        buf.push("Hello there my friend. ", 0);
        assert!(buf.tick(10_000).is_empty());
    }

    #[test]
    fn dropped_text_does_not_reset_the_stall_timer() {
        let mut buf = PhraseBuffer::new(PhraseConfig::default());
        assert!(buf.push("Here is some sample text for you ", 0).is_empty());
        // A hidden thinking block keeps streaming, so the pending text is not growing.
        assert!(buf.push("<think>musing about it", 600).is_empty());
        let out = buf.tick(1300);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "Here is some sample text for you");
    }

    #[test]
    fn tick_text_still_adds_up() {
        let input = "I think we should probably go to the store now and buy some milk and eggs for breakfast tomorrow morning";
        let mut buf = PhraseBuffer::new(PhraseConfig::default());
        let mut out = Vec::new();
        let mut t = 0;
        for w in words_of(input) {
            out.extend(buf.push(&w, t));
            t += 1500;
            out.extend(buf.tick(t));
        }
        out.extend(buf.finish());
        check_valid(&out);
        let joined = out.iter().map(|p| p.text.as_str()).collect::<Vec<_>>().join(" ");
        assert_eq!(joined, input);
    }

    #[test]
    fn regressions_found_by_fuzzing() {
        // A table-pipe comma was once inserted before whitespace the segmenter had already seen.
        assert_invariant(small(), "1 e.gHelloDr\"</think>think  ?) _|_'~~~>[```<think>*</think>., ``JNo\u{3002}bwww.https://x.com 'www.\nthinkhttp://No\u{2014}!\"");
        // A trailing space at exactly max_chars must cut like a newline there.
        assert_invariant(small(), "\u{1F389}](...think I| e.g3bar baz.Hello| \u{a0}#~~~- \n]-worldhttp://: > \u{2014}\n\n- 3_x_> ._x_>'");
        let cfg = PhraseConfig { min_chars: 4, max_chars: 10, max_wait_ms: 1200, first_min_chars: 4 };
        assert_invariant(cfg, "abcdefghij klm\nabcdefghij\nabcdefghij klm nop");
    }
}
