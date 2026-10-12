//! Colors, and turning text from the model, the tools and the file system
//! into lines that are safe to draw and fit the screen.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::agent::printable;

/// The palette: a deep-sea ground, shell orange for whatever waits on the
/// user, and one fixed meaning for every other color.
#[derive(Clone, Copy)]
pub enum Tone {
    /// The user, their input, what waits on them, the live service versions.
    Shell,
    /// Attempts and other work in progress.
    Sea,
    /// Memory: anything Molt remembers rather than works out.
    Mem,
    Ok,
    Bad,
    Warn,
    Dim,
    Plain,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Dark,
    Light,
    Ansi,
}

impl Theme {
    pub fn detect(colorterm: Option<&str>) -> Self {
        if colorterm.is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "truecolor" | "24bit")) {
            Self::Dark
        } else {
            Self::Ansi
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            "ansi" => Some(Self::Ansi),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Ansi => "ansi",
        }
    }

    pub fn background(self) -> Color {
        match self {
            Self::Dark => Color::Rgb(16, 30, 33),
            Self::Light => Color::Rgb(255, 255, 255),
            Self::Ansi => Color::Reset,
        }
    }

    pub fn color(self, tone: Tone) -> Color {
        match self {
            Self::Dark => match tone {
                Tone::Shell => Color::Rgb(235, 154, 82),
                Tone::Sea => Color::Rgb(95, 189, 176),
                Tone::Mem => Color::Rgb(185, 162, 230),
                Tone::Ok => Color::Rgb(147, 207, 110),
                Tone::Bad => Color::Rgb(236, 111, 106),
                Tone::Warn => Color::Rgb(230, 199, 90),
                Tone::Dim => Color::Rgb(108, 127, 131),
                Tone::Plain => Color::Rgb(230, 236, 238),
            },
            Self::Light => match tone {
                Tone::Shell => Color::Rgb(145, 65, 12),
                Tone::Sea => Color::Rgb(0, 101, 92),
                Tone::Mem => Color::Rgb(105, 66, 145),
                Tone::Ok => Color::Rgb(45, 105, 29),
                Tone::Bad => Color::Rgb(175, 35, 35),
                Tone::Warn => Color::Rgb(120, 88, 0),
                Tone::Dim => Color::Rgb(90, 100, 105),
                Tone::Plain => Color::Rgb(25, 35, 40),
            },
            Self::Ansi => match tone {
                Tone::Shell => Color::Yellow,
                Tone::Sea => Color::Cyan,
                Tone::Mem => Color::Magenta,
                Tone::Ok => Color::Green,
                Tone::Bad => Color::Red,
                Tone::Warn => Color::LightYellow,
                Tone::Dim => Color::DarkGray,
                Tone::Plain => Color::Reset,
            },
        }
    }

    /// Remap canonical colors at draw time, so old logs change with the theme too.
    pub fn paint(self, buffer: &mut ratatui::buffer::Buffer) {
        if colorless() {
            return;
        }
        const TONES: [Tone; 8] =
            [Tone::Shell, Tone::Sea, Tone::Mem, Tone::Ok, Tone::Bad, Tone::Warn, Tone::Dim, Tone::Plain];
        for cell in &mut buffer.content {
            if let Some(tone) = TONES.into_iter().find(|&t| Self::Dark.color(t) == cell.fg) {
                cell.fg = self.color(tone);
            } else if cell.fg == Color::Reset {
                cell.fg = self.color(Tone::Plain);
            }
            if cell.bg == Color::Reset {
                cell.bg = self.background();
            }
        }
    }
}

/// Colors are left out when `NO_COLOR` is set; the glyphs still tell states apart.
pub fn colorless() -> bool {
    static NO_COLOR: OnceLock<bool> = OnceLock::new();
    *NO_COLOR.get_or_init(|| std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()))
}

pub fn style(tone: Tone) -> Style {
    if colorless() {
        return match tone {
            Tone::Dim => Style::new().add_modifier(Modifier::DIM),
            Tone::Shell => Style::new().add_modifier(Modifier::BOLD),
            _ => Style::new(),
        };
    }
    Style::new().fg(Theme::Dark.color(tone))
}

pub fn span(text: impl Into<String>, tone: Tone) -> Span<'static> {
    Span::styled(text.into(), style(tone))
}

/// `text` safe to draw: control characters escaped (an escape sequence
/// from the model could rewrite the screen), tabs as spaces, one entry per
/// line.
pub fn clean(text: &str) -> Vec<String> {
    printable(text).replace('\t', "    ").split('\n').map(str::to_owned).collect()
}

/// [`clean`] text as one line, its line breaks shown as spaces.
pub fn clean_line(text: &str) -> String {
    clean(text).join(" ")
}

/// Lines of `text` in one tone, each after `indent`.
pub fn block(text: &str, indent: &str, tone: Tone) -> Vec<Line<'static>> {
    clean(text.trim_end()).into_iter().map(|l| Line::from(vec![Span::raw(indent.to_owned()), span(l, tone)])).collect()
}

/// `line` broken into lines at most `width` cells wide. Breaks fall
/// between characters; each piece keeps the styles of its spans.
pub fn wrap(line: &Line<'static>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for s in &line.spans {
        let mut piece = String::new();
        for c in s.content.chars() {
            let w = c.width().unwrap_or(0);
            if used + w > width && used > 0 {
                if !piece.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut piece), s.style));
                }
                out.push(Line::from(std::mem::take(&mut current)).style(line.style));
                used = 0;
            }
            piece.push(c);
            used += w;
        }
        if !piece.is_empty() {
            current.push(Span::styled(piece, s.style));
        }
    }
    out.push(Line::from(current).style(line.style));
    out
}

/// All of `lines`, wrapped to `width`.
pub fn wrap_all(lines: &[Line<'static>], width: u16) -> Vec<Line<'static>> {
    lines.iter().flat_map(|l| wrap(l, width)).collect()
}

/// The width of `text` in cells.
pub fn width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// `text` cut to `max` cells, with `…` when anything was cut.
pub fn fit(text: &str, max: usize) -> String {
    if width(text) <= max {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > max {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// A running time to the second: `45s`, `2m14s`, `1h02m`.
pub fn duration(d: std::time::Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

/// A confidence from 0 to 1 as a bar ten cells wide, in eighths.
pub fn bar(value: f64) -> String {
    const PARTS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let eighths = (value.clamp(0.0, 1.0) * 80.0).round() as usize;
    let mut out = "█".repeat(eighths / 8);
    if !eighths.is_multiple_of(8) {
        out.push(PARTS[eighths % 8]);
    }
    while out.chars().count() < 10 {
        out.push(' ');
    }
    out
}

/// Milliseconds since the epoch as `YYYY-MM-DD HH:MM` UTC.
pub fn when(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}", rest / 3600, rest % 3600 / 60)
}

/// Lines added and removed in a unified diff.
pub fn diff_counts(patch: &str) -> (usize, usize) {
    let mut counts = (0, 0);
    for line in patch.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            counts.0 += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            counts.1 += 1;
        }
    }
    counts
}

/// A unified diff, colored by line.
pub fn diff_lines(patch: &str) -> Vec<Line<'static>> {
    clean(patch)
        .into_iter()
        .map(|l| {
            let tone = if l.starts_with("+++") || l.starts_with("---") || l.starts_with("diff ") {
                Tone::Dim
            } else if l.starts_with('+') {
                Tone::Ok
            } else if l.starts_with('-') {
                Tone::Bad
            } else if l.starts_with("@@") {
                Tone::Sea
            } else {
                Tone::Plain
            };
            Line::from(span(l, tone))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
    }

    #[test]
    fn light_text_meets_aa_and_ansi_uses_only_terminal_colors() {
        fn luminance(c: Color) -> f64 {
            let Color::Rgb(r, g, b) = c else { panic!("expected RGB") };
            let linear = |v: u8| {
                let v = f64::from(v) / 255.0;
                if v <= 0.04045 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
        }
        for tone in [Tone::Shell, Tone::Sea, Tone::Mem, Tone::Ok, Tone::Bad, Tone::Warn, Tone::Dim, Tone::Plain] {
            assert!(
                (luminance(Theme::Light.background()) + 0.05) / (luminance(Theme::Light.color(tone)) + 0.05) >= 4.5
            );
            assert!(matches!(Theme::Dark.color(tone), Color::Rgb(..)));
            assert!(!matches!(Theme::Ansi.color(tone), Color::Rgb(..) | Color::Indexed(..)));
        }
        assert_eq!(Theme::detect(Some("24bit")), Theme::Dark);
        assert_eq!(Theme::detect(None), Theme::Ansi);
        assert_eq!(Theme::parse("bad"), None);
    }

    #[test]
    fn wrapping_counts_cells_and_keeps_styles() {
        let line = Line::from(vec![span("abc", Tone::Ok), span("defg", Tone::Bad)]);
        let wrapped = wrap(&line, 3);
        assert_eq!(texts(&wrapped), ["abc", "def", "g"]);
        assert_eq!(wrapped[1].spans[0].style, style(Tone::Bad));
        // Wide characters take two cells and are never split.
        assert_eq!(texts(&wrap(&Line::from("数组末尾"), 5)), ["数组", "末尾"]);
        assert_eq!(texts(&wrap(&Line::from(""), 4)), [""]);
    }

    #[test]
    fn text_from_outside_cannot_reach_the_terminal() {
        assert_eq!(clean("a\x1b[2Jb\tc\nd"), ["a\\u{1b}[2Jb    c", "d"]);
        assert_eq!(clean_line("one\ntwo"), "one two");
    }

    #[test]
    fn fitting_bars_dates_and_diffs() {
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("abc", 4), "abc");
        assert_eq!(bar(1.0), "██████████");
        assert_eq!(bar(0.0), "          ");
        assert_eq!(bar(0.5).trim_end(), "█████");
        assert_eq!(when(0), "1970-01-01 00:00");
        assert_eq!(when(1_791_594_357_000), "2026-10-10 01:05");
        assert_eq!(diff_counts("--- a\n+++ b\n@@\n-x\n+y\n+z\n"), (2, 1));
    }
}
