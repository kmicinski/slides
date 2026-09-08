//! Deck source (`deck.md`) → [`Deck`]: the slide tree with rendered HTML.
//!
//! The source conventions are those of reveal.js's markdown plugin, which the
//! existing course decks were written for, so they render unchanged:
//!
//! * a line that is exactly `---`, with a blank line on either side, starts a
//!   new horizontal slide; `--` likewise starts a vertical slide in the same column;
//! * `Note:` at the start of a line begins the slide's speaker notes;
//! * an HTML comment `<!-- .slide: attr="value" ... -->` sets attributes on the slide's `<section>`;
//! * `<!-- title: ... -->` and `<!-- theme: ... -->` anywhere set the deck title and theme;
//! * `$...$` / `$$...$$` is LaTeX, rendered here by KaTeX (never inside code) — also
//!   inside raw HTML blocks such as callouts, where the markdown parser sees no markdown;
//! * fenced code becomes `<pre><code class="LANG">`, the markup reveal's highlight plugin expects
//!   (a bare language name, no class at all when untagged so highlight.js auto-detects).
//!
//! Unlike reveal's plugin, separators inside fenced code are not honoured.
//! Rendering a whole deck costs a few milliseconds, so it is simply redone on
//! every edit. KaTeX output is cached because a KaTeX call costs about a
//! millisecond and a deck holds hundreds of formulas.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use regex::Regex;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Slide {
    /// Raw attribute text for the `<section>` tag, e.g. `class="title-slide"`.
    pub attrs: String,
    pub html: String,
    /// Rendered speaker notes; empty when the slide has none.
    pub notes: String,
    /// 1-based source line where the slide starts (not sent to clients as part of the body).
    #[serde(skip)]
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Diagnostic {
    pub line: usize,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Deck {
    pub title: Option<String>,
    pub theme: Option<String>,
    /// Horizontal columns, each a vertical stack of one or more slides.
    pub columns: Vec<Vec<Slide>>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Deck {
    pub fn count(&self) -> usize {
        self.columns.iter().map(Vec::len).sum()
    }
}

/// Renders decks, keeping KaTeX output across renders: `(display, tex)` →
/// `(html, error)`. Errors are cached too so they are re-reported every render.
#[derive(Default)]
pub struct Renderer {
    math: HashMap<(bool, String), (String, Option<String>)>,
}

impl Renderer {
    pub fn render(&mut self, src: &str) -> Deck {
        let directive = |key: &str| {
            comments(src)
                .find_map(|c| c.strip_prefix(key))
                .map(|v| v.trim().to_string())
        };
        let mut diagnostics = Vec::new();
        let mut columns = Vec::new();
        for col in split(src) {
            let mut slides = Vec::new();
            for (line, md) in col {
                let attrs = comments(md)
                    .filter_map(|c| c.strip_prefix(".slide:"))
                    .map(str::trim)
                    .collect::<Vec<_>>()
                    .join(" ");
                let (body, notes) = split_notes(md);
                let html = self.markdown(body, line, &mut diagnostics);
                let notes = notes
                    .map(|(n, at)| self.markdown(n, line + at, &mut diagnostics))
                    .unwrap_or_default();
                slides.push(Slide {
                    attrs,
                    html,
                    notes,
                    line,
                });
            }
            columns.push(slides);
        }
        Deck {
            title: directive("title:"),
            theme: directive("theme:"),
            columns,
            diagnostics,
        }
    }

    fn markdown(&mut self, md: &str, first_line: usize, diags: &mut Vec<Diagnostic>) -> String {
        let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_MATH;
        let (md, hoisted) = hoist(md, first_line);
        let md = md.as_str();
        let line_of = |at: usize| {
            first_line
                + md[..at].matches('\n').count()
                + hoisted
                    .iter()
                    .filter(|h| h.at < at)
                    .map(|h| h.newlines)
                    .sum::<usize>()
        };
        let mut events = Vec::new();
        let mut in_code = false;
        // Raw HTML arrives line by line; gather a block so display math inside it can span lines.
        let mut raw: Option<(usize, String)> = None;
        for (ev, range) in Parser::new_ext(md, opts).into_offset_iter() {
            if let Event::Html(html) = &ev {
                raw.get_or_insert_with(|| (range.start, String::new()))
                    .1
                    .push_str(html);
                continue;
            }
            if let Some((at, html)) = raw.take() {
                let html = self.math_in_html(&html, line_of(at), &hoisted, diags);
                events.push(Event::Html(stamp(&html, line_of(at)).into()));
            }
            let line = || line_of(range.start);
            events.push(match ev {
                Event::InlineMath(tex) => Event::Html(self.math(&tex, false, line_of(range.start), &hoisted, diags).into()),
                Event::DisplayMath(tex) => Event::Html(self.math(&tex, true, line_of(range.start), &hoisted, diags).into()),
                Event::Start(Tag::CodeBlock(kind)) => {
                    in_code = true;
                    let lang = match &kind {
                        CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or(""),
                        CodeBlockKind::Indented => "",
                    };
                    Event::Html(match lang {
                        "" => format!("<pre data-line=\"{}\"><code>", line()).into(),
                        lang => format!("<pre data-line=\"{}\"><code class=\"{}\">", line(), escape(lang)).into(),
                    })
                }
                // Block starts carry their source line so the editor can jump to
                // what was clicked in the preview (the heading-attributes
                // extension is off, so headings have no id/class to preserve).
                Event::Start(Tag::Paragraph) => Event::Html(format!("<p data-line=\"{}\">", line()).into()),
                Event::Start(Tag::Heading { level, .. }) => Event::Html(format!("<{level} data-line=\"{}\">", line()).into()),
                Event::Start(Tag::Item) => Event::Html(format!("<li data-line=\"{}\">", line()).into()),
                Event::Start(Tag::BlockQuote(_)) => Event::Html(format!("<blockquote data-line=\"{}\">", line()).into()),
                Event::Start(Tag::Table(_)) => Event::Html(format!("<table data-line=\"{}\">", line()).into()),
                Event::End(TagEnd::CodeBlock) => {
                    in_code = false;
                    Event::Html("</code></pre>\n".into())
                }
                // A `$` that survives as text is a math delimiter that found no partner
                // (pulldown-cmark also refuses spans with unbalanced braces).
                Event::Text(t) if !in_code && t.contains('$') => {
                    diags.push(Diagnostic { line: line_of(range.start), message: "unmatched $ — math delimiter without a partner, or unbalanced braces inside math".into() });
                    Event::Text(t)
                }
                ev => ev,
            });
        }
        if let Some((at, html)) = raw.take() {
            let html = self.math_in_html(&html, line_of(at), &hoisted, diags);
            events.push(Event::Html(stamp(&html, line_of(at)).into()));
        }
        let mut out = String::with_capacity(md.len() * 2);
        pulldown_cmark::html::push_html(&mut out, events.into_iter());
        out
    }

    /// Math in a raw HTML block (a callout, a column layout). The old build
    /// substituted math before markdown ran, so the decks rely on this. Code,
    /// `<pre>`, `<script>`, `<style>` and comments inside the block are left alone.
    fn math_in_html(
        &mut self,
        html: &str,
        first_line: usize,
        hoisted: &[Hoisted],
        diags: &mut Vec<Diagnostic>,
    ) -> String {
        static PROTECTED: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?is)<(?:code|pre|script|style)\b[^>]*>.*?</(?:code|pre|script|style)>|<!--.*?-->").unwrap()
        });
        static MATH: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?s)\$\$(.+?)\$\$|\$([^$]+?)\$").unwrap());
        let mut out = String::with_capacity(html.len());
        let mut last = 0;
        for protected in PROTECTED
            .find_iter(html)
            .map(|m| m.range())
            .chain(std::iter::once(html.len()..html.len()))
        {
            let free = &html[last..protected.start];
            let base = last;
            out.push_str(&MATH.replace_all(free, |c: &regex::Captures| {
                let line = first_line
                    + html[..base + c.get(0).unwrap().start()]
                        .matches('\n')
                        .count();
                match (c.get(1), c.get(2)) {
                    (Some(display), _) => self.math(display.as_str(), true, line, hoisted, diags),
                    (_, Some(inline)) => self.math(inline.as_str(), false, line, hoisted, diags),
                    _ => unreachable!(),
                }
            }));
            out.push_str(&html[protected.clone()]);
            last = protected.end;
        }
        out
    }

    fn math(
        &mut self,
        tex: &str,
        display: bool,
        line: usize,
        hoisted: &[Hoisted],
        diags: &mut Vec<Diagnostic>,
    ) -> String {
        let (tex, line) = match tex
            .strip_prefix('\u{E000}')
            .and_then(|n| n.parse::<usize>().ok())
            .and_then(|n| hoisted.get(n))
        {
            Some(h) => (h.tex.as_str(), h.line),
            None => (tex, line),
        };
        let key = (display, tex.to_string());
        if !self.math.contains_key(&key) {
            if self.math.len() >= 4096 {
                self.math.clear();
            }
            let opts = |strict| {
                katex::Opts::builder()
                    .display_mode(display)
                    .throw_on_error(strict)
                    .build()
                    .unwrap()
            };
            let entry = match katex::render_with_opts(tex, opts(true)) {
                Ok(html) => (html, None),
                // Lenient KaTeX renders the offending source in red, as the browser build did.
                Err(e) => (
                    katex::render_with_opts(tex, opts(false)).unwrap_or_else(|_| escape(tex)),
                    Some(katex_message(&e)),
                ),
            };
            self.math.insert(key.clone(), entry);
        }
        let (html, error) = &self.math[&key];
        if let Some(message) = error {
            diags.push(Diagnostic {
                line,
                message: message.clone(),
            });
        }
        html.clone()
    }
}

/// The crate wraps KaTeX's message in a JS-value debug dump, and KaTeX appends
/// the source with combining underlines marking the error; keep the sentence.
fn katex_message(e: &katex::Error) -> String {
    let s = e.to_string();
    let msg = s.split("KaTeX parse error: ").last().unwrap_or(&s);
    let msg = msg.split(" at position ").next().unwrap_or(msg);
    msg.split("\")")
        .next()
        .unwrap_or(msg)
        .replace("\\\\", "\\")
        .replace("\\\"", "\"")
}

/// Splits the source into columns of `(first line, markdown)` slides.
/// Slides as `(first line, source)` per column. The source of every slide but
/// the first begins with the blank line after its separator; each ends just
/// before the next separator line.
pub fn split(src: &str) -> Vec<Vec<(usize, &str)>> {
    let lines: Vec<(usize, &str)> = {
        let mut off = 0;
        src.split('\n')
            .map(|l| {
                let at = off;
                off += l.len() + 1;
                (at, l)
            })
            .collect()
    };
    let blank = |i: usize| lines.get(i).is_none_or(|(_, l)| l.trim().is_empty());
    let (mut columns, mut column) = (Vec::new(), Vec::new());
    let (mut start, mut start_line, mut fence) = (0usize, 1usize, None);
    for (i, &(at, raw)) in lines.iter().enumerate() {
        let line = raw.trim_end_matches('\r');
        if fenced(&mut fence, line)
            || (line != "---" && line != "--")
            || i == 0
            || !blank(i - 1)
            || !blank(i + 1)
        {
            continue;
        }
        column.push((start_line, &src[start..at]));
        if line == "---" {
            columns.push(std::mem::take(&mut column));
        }
        (start, start_line) = ((at + raw.len() + 1).min(src.len()), i + 2);
    }
    column.push((start_line, &src[start..]));
    columns.push(column);
    columns
}

/// Tracks fenced code across lines: true while inside a fence (fence lines included).
fn fenced(fence: &mut Option<&'static str>, line: &str) -> bool {
    let t = line.trim_start();
    match *fence {
        Some(marker) => {
            if t.starts_with(marker) {
                *fence = None;
            }
            true
        }
        None => match ["```", "~~~"].into_iter().find(|m| t.starts_with(m)) {
            Some(marker) => {
                *fence = Some(marker);
                true
            }
            None => false,
        },
    }
}

/// A display-math span lifted out of the markdown by [`hoist`].
struct Hoisted {
    tex: String,
    line: usize,
    /// Byte offset of its placeholder in the hoisted text, and the newlines it removed.
    at: usize,
    newlines: usize,
}

/// Lifts out display math whose `$$` pair spans lines, before parsing. The
/// block parser runs before inline math, so a continuation line such as
/// `+ \mathbb{E}…` would become a list item and a lone `=` a heading underline.
/// Each span becomes a one-line `$$\u{E000}N$$` token the parser accepts as
/// display math; the renderer maps it back to the TeX and its source line.
fn hoist(md: &str, first_line: usize) -> (String, Vec<Hoisted>) {
    let mut out = String::with_capacity(md.len());
    let mut spans: Vec<Hoisted> = Vec::new();
    let (mut last, mut at, mut fence, mut open) = (0, 0, None, None);
    for (i, line) in md.split_inclusive('\n').enumerate() {
        if !fenced(&mut fence, line) {
            let marks: Vec<usize> = line.match_indices("$$").map(|(p, _)| at + p).collect();
            let opener = |from: usize| (marks.len() - from) % 2 == 1;
            match open {
                None => open = opener(0).then(|| (marks[marks.len() - 1], first_line + i)),
                Some((start, line_no)) => {
                    if let Some(&close) = marks.first() {
                        let tex = &md[start + 2..close];
                        out.push_str(&md[last..start]);
                        spans.push(Hoisted {
                            tex: tex.to_string(),
                            line: line_no,
                            at: out.len(),
                            newlines: tex.matches('\n').count(),
                        });
                        out.push_str(&format!("$$\u{E000}{}$$", spans.len() - 1));
                        last = close + 2;
                        open = opener(1).then(|| (marks[marks.len() - 1], first_line + i));
                    }
                }
            }
        }
        at += line.len();
    }
    out.push_str(&md[last..]);
    (out, spans)
}

/// Body and, if present, the speaker notes with their line offset within `md`.
fn split_notes(md: &str) -> (&str, Option<(&str, usize)>) {
    let (mut at, mut line) = (0, 0);
    for l in md.split_inclusive('\n') {
        if let Some(notes) = l.strip_prefix("Note:") {
            return (&md[..at], Some((&md[at + l.len() - notes.len()..], line)));
        }
        (at, line) = (at + l.len(), line + 1);
    }
    (md, None)
}

/// The trimmed text of every `<!-- ... -->` comment.
fn comments(src: &str) -> impl Iterator<Item = &str> {
    src.match_indices("<!--").filter_map(move |(i, _)| {
        let body = &src[i + 4..];
        body.find("-->").map(|end| body[..end].trim())
    })
}

/// Adds `data-line` to the first opening tag of a raw HTML block (a callout,
/// a column layout), so clicks inside it map to where the block starts.
fn stamp(html: &str, line: usize) -> String {
    static OPEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*<[a-zA-Z][\w-]*").unwrap());
    match OPEN.find(html) {
        Some(m) => format!(
            "{} data-line=\"{line}\"{}",
            &html[..m.end()],
            &html[m.end()..]
        ),
        None => html.to_string(),
    }
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(src: &str) -> Vec<Vec<usize>> {
        split(src)
            .iter()
            .map(|c| c.iter().map(|s| s.0).collect())
            .collect()
    }

    #[test]
    fn splits_horizontal_and_vertical() {
        assert_eq!(
            lines("a\n\n---\n\nb\n\n--\n\nc\n"),
            vec![vec![1], vec![4, 8]],
            "a slide starts right after its separator"
        );
        assert_eq!(
            lines("a\n---\nb"),
            vec![vec![1]],
            "separators need blank lines around them"
        );
        assert_eq!(
            lines("a\n\n```\n\n---\n\n```\n\n---\n\nb"),
            vec![vec![1], vec![10]],
            "not inside fences"
        );
    }

    #[test]
    fn blocks_carry_source_lines() {
        let deck = Renderer::default().render(
            "# H\n\npara\n\n- one\n- two\n\n---\n\n<div class=\"callout\">\n\ninside\n\n</div>\n\n```\nx\n```\n",
        );
        let a = &deck.columns[0][0].html;
        for tag in [
            "<h1 data-line=\"1\">",
            "<p data-line=\"3\">",
            "<li data-line=\"5\">",
            "<li data-line=\"6\">",
        ] {
            assert!(a.contains(tag), "{tag} in {a}");
        }
        let b = &deck.columns[1][0].html;
        for tag in [
            "<div data-line=\"10\" class=\"callout\">",
            "<p data-line=\"12\">",
            "<pre data-line=\"16\">",
        ] {
            assert!(b.contains(tag), "{tag} in {b}");
        }
        assert!(
            !b.contains("</div data-line"),
            "closing tags are left alone"
        );
    }

    #[test]
    fn slide_parts() {
        let mut r = Renderer::default();
        let deck = r.render("<!-- title: T -->\n<!-- theme: x -->\n<!-- .slide: class=\"big\" -->\n# Hi\nNote:\nremember *this*\n\n---\n\n```python\nx = 1\n```\n\n```\nplain\n```\n");
        assert_eq!(
            (deck.title.as_deref(), deck.theme.as_deref()),
            (Some("T"), Some("x"))
        );
        let s = &deck.columns[0][0];
        assert_eq!(s.attrs, "class=\"big\"");
        assert!(s.html.contains("<h1 data-line=\"4\">Hi</h1>"));
        assert!(s.notes.contains("<em>this</em>"));
        let code = &deck.columns[1][0].html;
        assert!(
            code.contains("<pre data-line=\"10\"><code class=\"python\">x = 1\n</code></pre>"),
            "{code}"
        );
        assert!(
            code.contains("<pre data-line=\"14\"><code>plain\n</code></pre>"),
            "{code}"
        );
    }

    #[test]
    fn math_renders_and_reports_errors() {
        let mut r = Renderer::default();
        let deck = r.render("a $x^2$ b\n\n---\n\nc\n\n$$\\foo$$\n\ncosts $5\n");
        assert!(deck.columns[0][0].html.contains("class=\"katex\""));
        let lines: Vec<usize> = deck.diagnostics.iter().map(|d| d.line).collect();
        assert_eq!(lines, vec![7, 9]);
        assert_eq!(
            deck.diagnostics[0].message,
            "Undefined control sequence: \\foo"
        );
        assert!(deck.diagnostics[1].message.starts_with("unmatched $"));
        let again = r.render("$$\\foo$$");
        assert_eq!(again.diagnostics.len(), 1, "cached errors are re-reported");
    }

    #[test]
    fn multi_line_display_math_survives_block_syntax() {
        let mut r = Renderer::default();
        let src = "- item\n\n$$\n\\hat{y}\n=\n\\mathrm{softmax}(z)\n$$\n\n$$V = a\n+ b$$ and then $x$\n\n```\n$$\nnot math\n$$\n```\n\n$$\\foo\n+ 1$$\n";
        let deck = r.render(src);
        let html = &deck.columns[0][0].html;
        assert_eq!(html.matches("katex-display").count(), 3, "{html}");
        assert!(!html.contains("<h1"), "{html}");
        assert_eq!(
            html.matches("<li ").count(),
            1,
            "continuation lines are not list items: {html}"
        );
        assert!(
            html.contains("<pre data-line=\"12\"><code>$$\nnot math\n$$\n</code></pre>"),
            "{html}"
        );
        assert_eq!(
            deck.diagnostics.iter().map(|d| d.line).collect::<Vec<_>>(),
            vec![18],
            "error line is the span's first line"
        );
    }

    #[test]
    fn math_inside_raw_html_blocks() {
        let mut r = Renderer::default();
        let deck = r.render("<div class=\"callout\">the term $c$, <code>$X</code>\n$$\\int_0^1 f$$\n<!-- $ -->\n</div>\n");
        let html = &deck.columns[0][0].html;
        assert_eq!(html.matches("class=\"katex\"").count(), 2, "{html}");
        assert!(
            html.contains("<code>$X</code>") && html.contains("<!-- $ -->"),
            "{html}"
        );
        assert!(deck.diagnostics.is_empty());
    }
}
