//! HTML as plain text a model can read: the words of the page, a line per
//! block, links kept as `[text](url)`, and nothing that only runs or styles
//! it. Not a full HTML parser: documentation pages need no more, and a page
//! it reads badly still comes out as text.

use reqwest::Url;

/// Elements whose content is never text for a reader.
const SKIPPED: &[&str] = &["script", "style", "noscript", "template", "svg", "iframe", "object", "head"];
/// Elements that start and end a line.
const BLOCKS: &[&str] = &[
    "p",
    "div",
    "br",
    "hr",
    "section",
    "article",
    "header",
    "footer",
    "nav",
    "main",
    "aside",
    "ul",
    "ol",
    "dl",
    "dt",
    "dd",
    "table",
    "tr",
    "blockquote",
    "figure",
    "figcaption",
    "form",
    "details",
    "summary",
    "title",
    "body",
];

/// `html` as text, with links resolved against `base`.
pub fn to_text(html: &str, base: &Url) -> String {
    let mut out = Out::default();
    let mut rest = html;
    // The title is kept even though the rest of <head> is not.
    let mut skipping: Option<&str> = None;
    let mut link: Option<String> = None;
    while !rest.is_empty() {
        let Some(open) = rest.find('<') else {
            if skipping.is_none() {
                out.text(rest);
            }
            break;
        };
        if skipping.is_none() {
            out.text(&rest[..open]);
        }
        rest = &rest[open..];
        if let Some(after) = rest.strip_prefix("<!--") {
            rest = after.find("-->").map_or("", |end| &after[end + 3..]);
            continue;
        }
        let Some(close) = tag_end(rest) else {
            // A lone `<`: text.
            if skipping.is_none() {
                out.text("<");
            }
            rest = &rest[1..];
            continue;
        };
        let tag = &rest[1..close];
        rest = &rest[close + 1..];
        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        if name.is_empty() {
            // A doctype, a processing instruction, or `< ` as text.
            continue;
        }
        if let Some(skipped) = skipping {
            if closing && name == skipped {
                skipping = None;
            } else if name == "title" && skipped == "head" && !closing {
                // Read the title out of <head>, then go on skipping.
                let end = find_ci(rest, "</title").unwrap_or(rest.len());
                out.block();
                out.push_str("# ");
                out.text(&rest[..end]);
                out.block();
                rest = &rest[end..];
            }
            continue;
        }
        if !closing && !tag.ends_with('/') && SKIPPED.contains(&name.as_str()) {
            skipping = SKIPPED.iter().copied().find(|s| *s == name);
            continue;
        }
        match (name.as_str(), closing) {
            ("pre", false) => {
                out.block();
                out.pre += 1;
            }
            ("pre", true) => {
                out.pre = out.pre.saturating_sub(1);
                out.block();
            }
            ("li", false) => {
                out.line();
                out.push_str("- ");
            }
            (h, false) if is_heading(h) => {
                out.block();
                out.push_str(&"#".repeat(h[1..].parse().unwrap_or(1)));
                out.push_str(" ");
            }
            (h, true) if is_heading(h) => out.block(),
            ("td" | "th", false) => out.push_str(" | "),
            ("a", false) => {
                link = attribute(tag, "href").and_then(|href| linked(&href, base));
                if link.is_some() {
                    out.push_str("[");
                }
            }
            ("a", true) => {
                if let Some(url) = link.take() {
                    out.push_str(&format!("]({url})"));
                }
            }
            ("img", _) => {
                if let Some(alt) = attribute(tag, "alt").filter(|a| !a.trim().is_empty()) {
                    out.text(&format!("[image: {alt}]"));
                }
            }
            (b, _) if BLOCKS.contains(&b) => out.block(),
            _ => {}
        }
    }
    out.finish()
}

fn is_heading(name: &str) -> bool {
    name.len() == 2
        && name.starts_with('h')
        && name.as_bytes()[1].is_ascii_digit()
        && (b'1'..=b'6').contains(&name.as_bytes()[1])
}

/// Where the tag that `s` starts with ends, quotes respected.
fn tag_end(s: &str) -> Option<usize> {
    let second = s[1..].chars().next()?;
    if !(second.is_ascii_alphabetic() || matches!(second, '/' | '!' | '?')) {
        return None;
    }
    let mut quote = None;
    for (i, c) in s.char_indices().skip(1) {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    haystack.to_ascii_lowercase().find(needle)
}

/// The value of attribute `name` in `tag` (the text between `<` and `>`), entities decoded.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name).map(|i| i + from) {
        from = i + name.len();
        let before = lower[..i].chars().last();
        if !before.is_some_and(char::is_whitespace) {
            continue;
        }
        let rest = tag[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else { continue };
        let rest = rest.trim_start();
        let value = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => rest[1..].split(q).next().unwrap_or_default(),
            _ => rest.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or_default(),
        };
        return Some(decode(value));
    }
    None
}

/// A link's target as an absolute URL, or `None` for one that leads nowhere
/// a reader could go (an anchor on the page, a script).
fn linked(href: &str, base: &Url) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let url = base.join(href).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
}

/// `s` with character references replaced.
fn decode(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest[1..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '#')).map(|e| e + 1);
        let (name, len) = match end {
            Some(e) if rest[e..].starts_with(';') => (&rest[1..e], e + 1),
            _ => ("", 0),
        };
        let c = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "hellip" => Some('…'),
            "copy" => Some('©'),
            n if n.starts_with("#x") || n.starts_with("#X") => {
                u32::from_str_radix(&n[2..], 16).ok().and_then(char::from_u32)
            }
            n if n.starts_with('#') => n[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The text built so far, with whitespace collapsed outside `<pre>`.
#[derive(Default)]
struct Out {
    text: String,
    pre: u32,
}

impl Out {
    fn text(&mut self, raw: &str) {
        let decoded = decode(raw);
        if self.pre > 0 {
            self.text.push_str(&decoded);
            return;
        }
        let mut words = decoded.split_whitespace().peekable();
        if words.peek().is_none() {
            if !decoded.is_empty() {
                self.space();
            }
            return;
        }
        for (i, word) in words.enumerate() {
            if i > 0 || decoded.starts_with(char::is_whitespace) {
                self.space();
            }
            self.text.push_str(word);
        }
        if decoded.ends_with(char::is_whitespace) {
            self.space();
        }
    }

    /// One space between words, none at the start of a line or a link.
    fn space(&mut self) {
        if !self.text.is_empty() && !self.text.ends_with([' ', '\n', '[']) {
            self.text.push(' ');
        }
    }

    fn push_str(&mut self, s: &str) {
        self.text.push_str(s);
    }

    /// End the line.
    fn line(&mut self) {
        let trimmed = self.text.trim_end_matches(' ').len();
        self.text.truncate(trimmed);
        if !self.text.is_empty() && !self.text.ends_with('\n') {
            self.text.push('\n');
        }
    }

    /// End the paragraph: a blank line, once.
    fn block(&mut self) {
        self.line();
        if !self.text.is_empty() && !self.text.ends_with("\n\n") {
            self.text.push('\n');
        }
    }

    fn finish(self) -> String {
        let mut out = String::with_capacity(self.text.len());
        let mut blank = 0;
        for line in self.text.lines() {
            let line = line.trim_end();
            if line.is_empty() {
                blank += 1;
                if blank > 1 {
                    continue;
                }
            } else {
                blank = 0;
            }
            out.push_str(line);
            out.push('\n');
        }
        out.trim().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(html: &str) -> String {
        to_text(html, &Url::parse("https://docs.rs/serde/latest/serde/").unwrap())
    }

    #[test]
    fn a_page_keeps_its_words_headings_lists_and_links() {
        let html = r##"<!DOCTYPE html><html><head><title>serde - Rust</title>
            <style>body { color: red }</style><script>alert("x < y")</script></head>
            <body><nav><a href="#main">Skip</a></nav>
            <h1>Crate   <code>serde</code></h1>
            <p>Serde is a <b>framework</b> for <a href="trait.Serialize.html">serializing</a> &amp;
            deserializing.</p><!-- a comment <p>hidden</p> -->
            <ul><li>One</li><li>Two &lt;T&gt;</li></ul>
            <pre>fn main() {
    println!("hi");
}</pre><p>Done&nbsp;&#8212;&#x2713;</p></body></html>"##;
        assert_eq!(
            text(html),
            "# serde - Rust\n\nSkip\n\n# Crate serde\n\nSerde is a framework for \
             [serializing](https://docs.rs/serde/latest/serde/trait.Serialize.html) & deserializing.\n\n- One\n- \
             Two <T>\n\nfn main() {\n    println!(\"hi\");\n}\n\nDone —✓"
        );
    }

    #[test]
    fn broken_markup_still_comes_out_as_text() {
        assert_eq!(text("a < b and c<d"), "a < b and c<d");
        assert_eq!(text("<p>unclosed <a href='x.html'>link"), "unclosed [link");
        assert_eq!(text("<p>tail &amp no semicolon &bogus;</p>"), "tail &amp no semicolon &bogus;");
        assert_eq!(text("<script>never closed"), "");
        assert_eq!(
            text("<a href=\"javascript:void(0)\">js</a> <a href=https://x.org/a>x</a>"),
            "js [x](https://x.org/a)"
        );
        assert_eq!(text("<img src=a.png alt=\"A chart\"><img src=b.png>"), "[image: A chart]");
    }

    #[test]
    fn attributes_are_found_by_whole_name() {
        assert_eq!(attribute("a data-href=\"no\" href='yes'", "href"), Some("yes".to_owned()));
        assert_eq!(attribute("a href = \"a&amp;b\"", "href"), Some("a&b".to_owned()));
        assert_eq!(attribute("a class=x", "href"), None);
    }
}
