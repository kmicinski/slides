//! The player: the page students open. One reveal.js document with the slides
//! already rendered, linking the engine and theme by *relative* path so the
//! same file works served from this server and from a folder on disk:
//!
//! ```text
//! decks/<name>/index.html  →  ../../engine/…   ../../themes/<theme>/…
//! ```
//!
//! The export is therefore a plain copy of those three directories.
//! The live preview is the same page with an empty slide container and the
//! live script (see `web/src/live.ts`), which fills it over a WebSocket.

use crate::deck::{Deck, Slide, escape};
use crate::theme::Theme;
use anyhow::Result;
use std::fs;
use std::io::{Cursor, Write};
use std::path::Path;
use walkdir::WalkDir;

const TEMPLATE: &str = include_str!("player.html");

pub fn html(deck: &Deck, name: &str, theme: &Theme, live: bool) -> String {
    html_with(deck, name, theme, live, &Default::default())
}

/// One slide as a still: a player with no chrome, for the review panel's
/// thumbnails and the compare view. Served at `/deck/<name>/thumb` (same
/// depth as `live`, so the relative engine/theme links hold).
pub fn thumb(slide: &Slide, name: &str, theme: &Theme) -> String {
    let deck = Deck {
        columns: vec![vec![slide.clone()]],
        ..Default::default()
    };
    let mut extra = serde_json::Map::new();
    for (k, v) in [
        ("controls", false),
        ("progress", false),
        ("keyboard", false),
        ("touch", false),
        ("hash", false),
        ("history", false),
    ] {
        extra.insert(k.into(), v.into());
    }
    extra.insert("slideNumber".into(), false.into());
    extra.insert("transition".into(), "none".into());
    html_with(&deck, name, theme, false, &extra)
}

/// `extra` overrides the theme's reveal options.
pub fn html_with(
    deck: &Deck,
    name: &str,
    theme: &Theme,
    live: bool,
    extra: &serde_json::Map<String, serde_json::Value>,
) -> String {
    let css = theme
        .css
        .iter()
        .map(|f| {
            format!(
                "<link rel=\"stylesheet\" href=\"../../themes/{}/{f}\" />",
                theme.name
            )
        })
        .collect::<Vec<_>>()
        .join("\n    ");
    let mut options = match &theme.reveal {
        serde_json::Value::Object(o) => o.clone(),
        _ => Default::default(),
    };
    if live {
        // The preview lives in an iframe: keep it out of the page's URL and history.
        options.insert("hash".into(), false.into());
        options.insert("history".into(), false.into());
    }
    for (k, v) in extra {
        options.insert(k.clone(), v.clone());
    }
    let slides = if live {
        String::new()
    } else {
        deck.columns.iter().map(|c| column(c)).collect()
    };
    TEMPLATE
        .replace("{{TITLE}}", &escape(deck.title.as_deref().unwrap_or(name)))
        .replace("{{THEME_CSS}}", &css)
        .replace(
            "{{OPTIONS}}",
            &serde_json::Value::Object(options).to_string(),
        )
        .replace("{{SLIDES}}", &slides)
        .replace(
            "{{LIVE}}",
            if live {
                r#"<script type="module" src="/static/live.js"></script>"#
            } else {
                ""
            },
        )
}

fn column(slides: &[Slide]) -> String {
    match slides {
        [single] => section(single),
        stack => format!(
            "<section>\n{}</section>\n",
            stack.iter().map(section).collect::<String>()
        ),
    }
}

fn section(s: &Slide) -> String {
    let attrs = if s.attrs.is_empty() {
        String::new()
    } else {
        format!(" {}", s.attrs)
    };
    let notes = if s.notes.is_empty() {
        String::new()
    } else {
        format!("<aside class=\"notes\">\n{}</aside>\n", s.notes)
    };
    format!("<section{attrs}>\n{}{notes}</section>\n", s.html)
}

/// A zip of `decks/<name>`, `engine` and `themes/<theme>` under one `slides/` folder.
pub fn export(root: &Path, name: &str, theme: &Theme) -> Result<Vec<u8>> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    for dir in [
        format!("decks/{name}"),
        "engine".to_string(),
        format!("themes/{}", theme.name),
    ] {
        let files = WalkDir::new(root.join(&dir))
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_type().is_file() && !e.file_name().to_string_lossy().starts_with('.')
            });
        for entry in files {
            let rel = entry
                .path()
                .strip_prefix(root)?
                .to_string_lossy()
                .into_owned();
            zip.start_file(format!("slides/{rel}"), options)?;
            zip.write_all(&fs::read(entry.path())?)?;
        }
    }
    Ok(zip.finish()?.into_inner())
}
