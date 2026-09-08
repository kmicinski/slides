//! A theme is a directory. Nothing in it is compiled: the player links its
//! stylesheets directly and the export copies the directory verbatim.
//!
//! ```text
//! themes/<name>/
//!   theme.toml        [reveal] table, passed to Reveal.initialize() as-is
//!   *.css             linked in sorted order (base.css, highlight.css, ...)
//!   fonts/            referenced from the CSS by relative URL
//!   starter.md        what a new deck starts from
//!   schemas/<s>.css   one slide schema: the styles ...
//!   schemas/<s>.md    ... and a self-contained example of the markup it styles
//! ```
//!
//! A schema is the unit an author — or an LLM asked to reproduce a slide
//! design — adds: one stylesheet plus one example. The example is what the
//! API hands to tools so they know the theme's vocabulary.

use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Serialize)]
pub struct Schema {
    pub name: String,
    pub example: String,
}

#[derive(Serialize)]
pub struct Theme {
    pub name: String,
    #[serde(skip)]
    pub dir: PathBuf,
    pub reveal: serde_json::Value,
    /// Stylesheets relative to `dir`, in link order.
    pub css: Vec<String>,
    pub schemas: Vec<Schema>,
}

impl Theme {
    pub fn load(dir: &Path) -> Result<Theme> {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let toml_src = fs::read_to_string(dir.join("theme.toml"))
            .with_context(|| format!("theme {name}: theme.toml"))?;
        let table: toml::Table =
            toml::from_str(&toml_src).with_context(|| format!("theme {name}: theme.toml"))?;
        let reveal = serde_json::to_value(
            table
                .get("reveal")
                .cloned()
                .unwrap_or(toml::Value::Table(Default::default())),
        )?;
        let mut css = files(dir, "css")?;
        let schemas_dir = dir.join("schemas");
        let mut schemas = Vec::new();
        for file in files(&schemas_dir, "css")? {
            let name = file.trim_end_matches(".css").to_string();
            let example =
                fs::read_to_string(schemas_dir.join(format!("{name}.md"))).unwrap_or_default();
            css.push(format!("schemas/{file}"));
            schemas.push(Schema { name, example });
        }
        Ok(Theme {
            name,
            dir: dir.to_path_buf(),
            reveal,
            css,
            schemas,
        })
    }

    /// The named theme, or the first theme when a deck names none.
    pub fn resolve(root: &Path, name: Option<&str>) -> Result<Theme> {
        match name {
            Some(name) => Theme::load(&root.join("themes").join(name)),
            None => list(root)?
                .into_iter()
                .next()
                .context("no themes installed"),
        }
    }

    pub fn starter(&self) -> Result<String> {
        fs::read_to_string(self.dir.join("starter.md"))
            .with_context(|| format!("theme {}: starter.md", self.name))
    }
}

/// Every theme under `root/themes`, by name.
pub fn list(root: &Path) -> Result<Vec<Theme>> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(root.join("themes"))
        .context("themes/ directory")?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("theme.toml").is_file())
        .collect();
    dirs.sort();
    dirs.iter().map(|d| Theme::load(d)).collect()
}

/// Sorted file names with `ext` directly under `dir`; empty when `dir` is absent.
fn files(dir: &Path, ext: &str) -> Result<Vec<String>> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(&format!(".{ext}")) && !n.starts_with('.'))
        .collect();
    names.sort();
    Ok(names)
}
