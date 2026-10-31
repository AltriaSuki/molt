//! How `fs.edit` finds the text it replaces, and what it says when it finds
//! none (#261); [`molt_api::fs::EditRequest`] has the rules.

use std::collections::HashMap;
use std::fmt::Write as _;

use molt_api::fs::{Anchor, Tolerance, EDIT_SHOWS_LINES};
use sha2::{Digest, Sha256};

/// Line comparisons a miss hint may make over every window of the file; a
/// larger file only gets the windows that share a line with `old`.
const HINT_COMPARISONS: usize = 400_000;
/// Windows a miss hint scores at most, when they are picked by shared lines.
const HINT_WINDOWS: usize = 2_000;
/// How alike a window must be to be shown, from 0 to 1.
const HINT_MIN: f64 = 0.5;
const HINT_CANDIDATES: usize = 2;
/// Lines of a candidate shown; the rest are named.
const HINT_LINES: usize = 40;
/// Characters of a line quoted in a sentence.
const QUOTE_CHARS: usize = 160;
/// Matches an ambiguity error lists.
const LISTED: usize = 20;

/// One place the old text matched: a byte range of the file, and the
/// 1-based line it starts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Span {
    pub start: usize,
    pub end: usize,
    pub line: u64,
}

#[derive(Clone, Copy, Debug)]
struct Line {
    start: usize,
    /// Before its `\r\n` or `\n`.
    text_end: usize,
    end: usize,
}

/// A file's text and its lines.
pub(crate) struct Lines<'a> {
    text: &'a str,
    lines: Vec<Line>,
}

impl<'a> Lines<'a> {
    pub fn new(text: &'a str) -> Self {
        let mut lines = Vec::new();
        let mut start = 0;
        for (i, _) in text.match_indices('\n') {
            let text_end = if text[..i].ends_with('\r') { i - 1 } else { i };
            lines.push(Line { start, text_end, end: i + 1 });
            start = i + 1;
        }
        if start < text.len() {
            lines.push(Line { start, text_end: text.len(), end: text.len() });
        }
        Self { text, lines }
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// The text of line `index` (0-based), without its line ending.
    fn text(&self, index: usize) -> &'a str {
        let l = self.lines[index];
        &self.text[l.start..l.text_end]
    }

    /// The 0-based line `offset` is on.
    fn index_of(&self, offset: usize) -> usize {
        self.lines.partition_point(|l| l.end <= offset).min(self.lines.len().saturating_sub(1))
    }

    /// The anchor of 1-based `line`, which must exist.
    pub fn anchor(&self, line: u64) -> Anchor {
        Anchor { line, hash: Some(line_hash(self.text(line as usize - 1))) }
    }

    /// Whether the line `offset` is on ends in `\r\n`; for the last line,
    /// which may have no ending, whether the line before does.
    pub fn crlf_at(&self, offset: usize) -> bool {
        let index = self.index_of(offset);
        let crlf = |l: &Line| self.text[l.text_end..l.end].starts_with('\r');
        match self.lines.get(index) {
            Some(l) if l.end > l.text_end => crlf(l),
            _ => index > 0 && crlf(&self.lines[index - 1]),
        }
    }

    fn span(&self, start: usize, end: usize) -> Span {
        Span { start, end, line: self.index_of(start) as u64 + 1 }
    }
}

/// The hash in an [`Anchor`]: the first four hex digits of the SHA-256 of
/// the line without trailing whitespace.
pub(crate) fn line_hash(line: &str) -> String {
    hex::encode(&Sha256::digest(line.trim_end().as_bytes())[..2])
}

/// The version an edit error names: the first twelve hex digits of the
/// SHA-256 of the file.
pub(crate) fn version(text: &str) -> String {
    hex::encode(&Sha256::digest(text.as_bytes())[..6])
}

/// The matches of `old` in the first step that has any, with what that step
/// ignores (`None`: exact); no matches when no step has any. Matches do not
/// overlap and are in file order.
pub(crate) fn find(lines: &Lines, old: &str) -> (Option<Tolerance>, Vec<Span>) {
    let text = lines.text;
    let exact: Vec<Span> = text.match_indices(old).map(|(i, m)| lines.span(i, i + m.len())).collect();
    if !exact.is_empty() {
        return (None, exact);
    }
    let endings = ignoring_line_endings(lines, old);
    if !endings.is_empty() {
        return (Some(Tolerance::LineEndings), endings);
    }
    let trailing = ignoring_trailing_whitespace(lines, old);
    if !trailing.is_empty() {
        return (Some(Tolerance::TrailingWhitespace), trailing);
    }
    (None, Vec::new())
}

fn ignoring_line_endings(lines: &Lines, old: &str) -> Vec<Span> {
    let text = lines.text;
    if !text.contains('\r') && !old.contains('\r') {
        return Vec::new();
    }
    // The file with each `\r\n` made `\n`, and where in it those were, so a
    // match maps back: a position moves right by the `\r`s dropped before it.
    let mut flat = String::with_capacity(text.len());
    let mut dropped = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("\r\n") {
        flat.push_str(&rest[..i]);
        dropped.push(flat.len());
        rest = &rest[i + 1..];
    }
    flat.push_str(rest);
    let needle = old.replace("\r\n", "\n");
    let back = |p: usize| p + dropped.partition_point(|&d| d < p);
    flat.match_indices(&needle).map(|(i, m)| lines.span(back(i), back(i + m.len()))).collect()
}

fn ignoring_trailing_whitespace(lines: &Lines, old: &str) -> Vec<Span> {
    let old = old.replace("\r\n", "\n");
    let ends_nl = old.ends_with('\n');
    let body = if ends_nl { &old[..old.len() - 1] } else { &old[..] };
    let parts: Vec<&str> = body.split('\n').collect();
    let m = parts.len();
    // A run of blank lines matches too much, and one line that does not reach
    // its end has no trailing whitespace to ignore.
    if parts.iter().all(|p| p.trim().is_empty()) || (m == 1 && !ends_nl && parts[0].trim_end() == parts[0]) {
        return Vec::new();
    }
    let mut spans: Vec<Span> = Vec::new();
    for i in 0..(lines.len() + 1).saturating_sub(m) {
        let Some((start, end)) = window_match(lines, i, &parts, ends_nl) else { continue };
        if spans.last().is_none_or(|s| s.end <= start) {
            spans.push(lines.span(start, end));
        }
    }
    spans
}

/// Where `parts` match lines `first..` ignoring trailing whitespace. The
/// first part may start inside its line, and the last, when `old` does not
/// end in a newline or trailing whitespace, may end inside its line.
fn window_match(lines: &Lines, first: usize, parts: &[&str], ends_nl: bool) -> Option<(usize, usize)> {
    let (mut start, mut end) = (0, 0);
    for (k, part) in parts.iter().enumerate() {
        let line = lines.lines[first + k];
        let text = lines.text(first + k);
        let trimmed = text.trim_end();
        let want = part.trim_end();
        let last = k + 1 == parts.len();
        let to_end = !last || ends_nl || want.len() < part.len();
        match (k == 0, to_end) {
            (false, true) if trimmed != want => return None,
            (false, false) if !text.starts_with(want) => return None,
            (true, true) if !trimmed.ends_with(want) => return None,
            (true, false) => return None,
            _ => {}
        }
        if k == 0 {
            start = line.start + trimmed.len() - want.len();
        }
        if last {
            end = match (ends_nl, to_end) {
                (true, _) => line.end,
                (false, true) => line.text_end,
                (false, false) => line.start + want.len(),
            };
        }
    }
    Some((start, end))
}

/// `new` with `\n` line endings, and with `\r\n`.
pub(crate) fn with_endings(new: &str) -> (String, String) {
    let lf = new.replace("\r\n", "\n");
    let crlf = lf.replace('\n', "\r\n");
    (lf, crlf)
}

/// `matched ignoring line endings`, for messages.
fn how(tolerance: Option<Tolerance>) -> &'static str {
    match tolerance {
        None => "",
        Some(Tolerance::LineEndings) => " (ignoring line endings)",
        Some(Tolerance::TrailingWhitespace) => " (ignoring line endings and trailing whitespace)",
    }
}

/// `4 (anchor 4:1a2b), 9 (anchor 9:3c4d)`.
fn listed(lines: &Lines, spans: &[Span]) -> String {
    let mut out: Vec<String> =
        spans.iter().take(LISTED).map(|s| format!("{} (anchor {})", s.line, lines.anchor(s.line))).collect();
    if spans.len() > LISTED {
        out.push(format!("and {} more", spans.len() - LISTED));
    }
    out.join(", ")
}

pub(crate) fn ambiguous(name: &str, lines: &Lines, tolerance: Option<Tolerance>, spans: &[Span]) -> String {
    format!(
        "old text occurs {} times in {name}{}, starting on lines {}: add surrounding lines to make it unique, give \
         the anchor of the one to change, or set replace_all, {EDIT_SHOWS_LINES}",
        spans.len(),
        how(tolerance),
        listed(lines, spans),
    )
}

pub(crate) fn off_anchor(name: &str, lines: &Lines, tolerance: Option<Tolerance>, spans: &[Span], line: u64) -> String {
    let s = if spans.len() == 1 { "" } else { "s" };
    format!(
        "old text occurs in {name}{} starting on line{s} {}, not on line {line}: give the anchor of the one to \
         change, {EDIT_SHOWS_LINES}",
        how(tolerance),
        listed(lines, spans),
    )
}

/// Why an anchor does not fit the file, if it does not.
pub(crate) fn check_anchor(name: &str, lines: &Lines, anchor: &Anchor) -> Option<String> {
    let total = lines.len() as u64;
    if anchor.line > total {
        return Some(format!("anchor {anchor} is past the end of {name}, which has {total} lines"));
    }
    let now = lines.anchor(anchor.line);
    match &anchor.hash {
        Some(hash) if Some(hash) != now.hash.as_ref() => Some(format!(
            "anchor {anchor} does not match {name}: line {} has changed and now reads `{}` (anchor {now})",
            anchor.line,
            quote(lines.text(anchor.line as usize - 1)),
        )),
        _ => None,
    }
}

/// The error for an `old` that matched nothing: the file's version and the
/// lines most like `old`.
pub(crate) fn miss(name: &str, lines: &Lines, old: &str) -> String {
    let mut out = format!("old text not found in {name} (version {})", version(lines.text));
    if lines.len() == 0 {
        out.push_str(", which is empty");
        return out;
    }
    let old = old.replace("\r\n", "\n");
    let wanted: Vec<&str> = old.strip_suffix('\n').unwrap_or(&old).split('\n').collect();
    let found = closest(lines, &wanted);
    if found.is_empty() {
        out.push_str(", and no lines in it are like it");
        return out;
    }
    out.push_str(". The closest lines:");
    for &(first, len, score) in &found {
        let (line, last) = (first as u64 + 1, (first + len) as u64);
        let _ = write!(out, "\n\nlines {line}-{last} ({:.0}% alike, anchor {}):\n", score * 100.0, lines.anchor(line));
        let shown: Vec<&str> = (first..first + len.min(HINT_LINES)).map(|i| lines.text(i)).collect();
        let shown = shown.join("\n");
        let fence = fence(&shown);
        let _ = write!(out, "{fence}\n{shown}\n{fence}");
        if len > HINT_LINES {
            let _ = write!(
                out,
                "\n(the first {HINT_LINES} of {len} lines; read from line {} for the rest)",
                line + HINT_LINES as u64
            );
        }
    }
    let (first, len, _) = found[0];
    out.push_str("\n\n");
    out.push_str(&difference(lines, first, len, &wanted));
    let _ =
        write!(out, "\nThe lines above are {name} as it is now: correct the old text from them, {EDIT_SHOWS_LINES}.");
    out
}

/// The first line of the window at `first` that differs from `wanted`.
fn difference(lines: &Lines, first: usize, len: usize, wanted: &[&str]) -> String {
    for (k, want) in wanted.iter().enumerate().take(len) {
        let have = lines.text(first + k);
        if key(have) != key(want) {
            return format!(
                "Line {} differs: the file has `{}` where the old text has `{}`.",
                first + k + 1,
                quote(have),
                quote(want)
            );
        }
    }
    if wanted.len() > len {
        return format!("The old text has {} lines and the file only {len} from line {}.", wanted.len(), first + 1);
    }
    let indents = |texts: &mut dyn Iterator<Item = &str>| {
        let (mut tabs, mut spaces) = (false, false);
        for t in texts {
            let lead = &t[..t.len() - t.trim_start().len()];
            tabs |= lead.contains('\t');
            spaces |= lead.contains(' ');
        }
        (tabs, spaces)
    };
    let lines_ = format!("Lines {}-{}", first + 1, first + len);
    let file = indents(&mut (first..first + len).map(|i| lines.text(i)));
    let theirs = indents(&mut wanted.iter().copied());
    match (file, theirs) {
        ((true, false), (false, true)) => {
            format!("{lines_} differ from the old text only in whitespace: the file indents them with tabs, the old text with spaces.")
        }
        ((false, true), (true, false)) => {
            format!("{lines_} differ from the old text only in whitespace: the file indents them with spaces, the old text with tabs.")
        }
        _ => format!("{lines_} differ from the old text only in whitespace (indentation or spacing within lines): copy them as shown."),
    }
}

/// The windows of the file most like `wanted`, best first, as (first line,
/// lines, how alike): at most [`HINT_CANDIDATES`], not overlapping.
fn closest(lines: &Lines, wanted: &[&str]) -> Vec<(usize, usize, f64)> {
    let want: Vec<Profile> = wanted.iter().map(|w| Profile::new(w)).collect();
    if want.iter().all(|w| w.key.is_empty()) {
        return Vec::new();
    }
    let total = lines.len();
    let len = want.len().min(total);
    let windows = total - len + 1;
    let starts: Vec<usize> = if windows.saturating_mul(len) <= HINT_COMPARISONS {
        (0..windows).collect()
    } else {
        // Too many to score them all: only windows where some telling line
        // of `old` is in its place, whitespace aside.
        let mut at: HashMap<&str, Vec<usize>> = HashMap::new();
        let keys: Vec<String> = (0..total).map(|i| key(lines.text(i))).collect();
        for (i, k) in keys.iter().enumerate() {
            at.entry(k.as_str()).or_default().push(i);
        }
        let mut starts: Vec<usize> = want
            .iter()
            .enumerate()
            .filter(|(_, w)| w.key.chars().count() >= 4)
            .flat_map(|(k, w)| at.get(w.key.as_str()).into_iter().flatten().filter_map(move |&j| j.checked_sub(k)))
            .filter(|&s| s < windows)
            .collect();
        starts.sort_unstable();
        starts.dedup();
        starts.truncate(HINT_WINDOWS);
        starts
    };
    let mut profiles: Vec<Option<Profile>> = vec![None; total];
    let mut scored: Vec<(usize, f64)> = starts
        .into_iter()
        .map(|s| {
            let (mut sum, mut weight) = (0.0, 0.0);
            for (k, w) in want.iter().take(len).enumerate() {
                let have = profiles[s + k].get_or_insert_with(|| Profile::new(lines.text(s + k)));
                let n = w.key.len().max(have.key.len()).max(1) as f64;
                sum += w.alike(have) * n;
                weight += n;
            }
            // Lines of `old` past the end of the file count as unlike.
            for w in &want[len..] {
                weight += w.key.len().max(1) as f64;
            }
            (s, sum / weight)
        })
        .filter(|&(_, score)| score >= HINT_MIN)
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut picked: Vec<(usize, usize, f64)> = Vec::new();
    for (s, score) in scored {
        if picked.len() == HINT_CANDIDATES {
            break;
        }
        if picked.iter().all(|&(p, _, _)| s.abs_diff(p) >= len) {
            picked.push((s, len, score));
        }
    }
    picked
}

/// A line with its whitespace runs made one space, and its character pairs.
#[derive(Clone)]
struct Profile {
    key: String,
    pairs: Vec<(char, char)>,
}

impl Profile {
    fn new(line: &str) -> Self {
        let key = key(line);
        let chars: Vec<char> = key.chars().collect();
        let mut pairs: Vec<(char, char)> = chars.windows(2).map(|w| (w[0], w[1])).collect();
        pairs.sort_unstable();
        Self { key, pairs }
    }

    /// From 0 to 1: the share of character pairs the two have in common.
    fn alike(&self, other: &Self) -> f64 {
        if self.key == other.key {
            return 1.0;
        }
        if self.pairs.is_empty() || other.pairs.is_empty() {
            return 0.0;
        }
        let (mut i, mut j, mut common) = (0, 0, 0);
        while i < self.pairs.len() && j < other.pairs.len() {
            match self.pairs[i].cmp(&other.pairs[j]) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    common += 1;
                    i += 1;
                    j += 1;
                }
            }
        }
        2.0 * common as f64 / (self.pairs.len() + other.pairs.len()) as f64
    }
}

fn key(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn quote(line: &str) -> String {
    let line = line.trim_end();
    match line.char_indices().nth(QUOTE_CHARS) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_owned(),
    }
}

/// A Markdown fence longer than any run of backticks in `text`.
fn fence(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(text: &str, old: &str) -> (Option<Tolerance>, Vec<(usize, usize, u64)>) {
        let lines = Lines::new(text);
        let (t, s) = find(&lines, old);
        (t, s.into_iter().map(|s| (s.start, s.end, s.line)).collect())
    }

    #[test]
    fn a_match_ignoring_line_endings_maps_back_to_the_crlf_bytes() {
        let text = "fn a() {\r\n    1\r\n}\r\nfn b() {\r\n    2\r\n}\r\n";
        let (t, s) = spans(text, "fn b() {\n    2\n}\n");
        assert_eq!(t, Some(Tolerance::LineEndings));
        assert_eq!(s, [(20, text.len(), 4)]);
        assert_eq!(&text[20..], "fn b() {\r\n    2\r\n}\r\n");
        // A match that ends before a `\r\n` leaves it alone.
        let (_, s) = spans(text, "    2\n}");
        assert_eq!(&text[s[0].0..s[0].1], "    2\r\n}");
        // An LF file and CRLF old text.
        let (t, s) = spans("a\nb\nc\n", "a\r\nb\r\n");
        assert_eq!((t, s), (Some(Tolerance::LineEndings), vec![(0, 4, 1)]));
    }

    #[test]
    fn trailing_whitespace_is_ignored_but_indentation_is_not() {
        let text = "def f():  \n    return 1\t\n\ndef g():\n\treturn 2\n";
        let (t, s) = spans(text, "def f():\n    return 1\n");
        assert_eq!(t, Some(Tolerance::TrailingWhitespace));
        assert_eq!(&text[s[0].0..s[0].1], "def f():  \n    return 1\t\n");
        // The old text's trailing spaces, which the file lacks.
        let (t, s) = spans(text, "def g():   \n\treturn");
        assert_eq!(t, Some(Tolerance::TrailingWhitespace));
        assert_eq!(&text[s[0].0..s[0].1], "def g():\n\treturn");
        // Spaces where the file has a tab never match.
        assert_eq!(spans(text, "def g():\n    return 2\n").1, []);
        // Nor does a blank run, or one line without trailing whitespace.
        assert_eq!(spans(text, "  \n\n").1, []);
        assert_eq!(spans(text, "return 3").1, []);
    }

    #[test]
    fn line_hashes_ignore_line_endings_and_trailing_whitespace_only() {
        assert_eq!(line_hash("let 名字 = 1;"), line_hash("let 名字 = 1;  \r"));
        assert_ne!(line_hash("let 名字 = 1;"), line_hash("let 名字 = 2;"));
        assert_ne!(line_hash("\tx"), line_hash("    x"));
        assert_eq!(line_hash("x").len(), 4);
        let lines = Lines::new("a\r\nb");
        assert!(lines.crlf_at(0) && lines.crlf_at(3), "the last line takes the ending of the one before");
        assert!(!Lines::new("a").crlf_at(0) && !Lines::new("a\nb\r\n").crlf_at(0));
    }

    #[test]
    fn the_closest_window_is_found_in_a_large_file_by_its_shared_lines() {
        let mut text = String::new();
        for i in 0..60_000 {
            let _ = writeln!(text, "let value_{i} = compute({i});");
        }
        let lines = Lines::new(&text);
        let old =
            "let value_41000 = compute(41000);\nlet value_41001 = compute(4100l);\nlet value_41002 = compute(41002);\n";
        let found = closest(&lines, &old.trim_end().split('\n').collect::<Vec<_>>());
        assert_eq!(found[0].0, 41000, "{found:?}");
        assert!(found[0].2 > 0.9);
    }
}
