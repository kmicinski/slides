//! JSON/text API for tools — an LLM driving a deck, scripts. It has the same
//! authority as the editor: writes go through the live document, so open
//! editors and previews update at once.
//!
//! ```text
//! GET  /api/decks                            → [{name, title}]
//! GET  /api/decks/{name}                     → deck.md
//! PUT  /api/decks/{name}      ← deck.md      → {slides, diagnostics}; creates the deck if new
//! GET  /api/themes                           → [{name, reveal, css, schemas: [{name, example}]}]
//! GET  /api/themes/{theme}/schemas/{schema}  → {css, example}
//! PUT  /api/themes/{theme}/schemas/{schema}  ← {css, example}   writes both files; reload pages to see it
//! ```
//!
//! Authenticate with the session cookie or `Authorization: Bearer <SLIDES_PASSWORD>`.

use crate::deck::Diagnostic;
use crate::theme::{self, Theme};
use crate::{Shared, valid_name};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use std::fs;

type ApiResult<T> = Result<T, (StatusCode, String)>;

fn bad(status: StatusCode) -> impl Fn(anyhow::Error) -> (StatusCode, String) {
    move |e| (status, format!("{e:#}"))
}

#[derive(Serialize)]
pub struct DeckInfo {
    pub name: String,
    pub title: Option<String>,
}

pub async fn decks(State(app): State<Shared>) -> Json<Vec<DeckInfo>> {
    let docs = app.docs.lock().unwrap();
    Json(
        docs.iter()
            .map(|(name, doc)| DeckInfo {
                name: name.clone(),
                title: doc.deck().title.clone(),
            })
            .collect(),
    )
}

pub async fn get_deck(State(app): State<Shared>, Path(name): Path<String>) -> ApiResult<Response> {
    let doc = app
        .doc(&name)
        .ok_or((StatusCode::NOT_FOUND, "no such deck".into()))?;
    Ok((
        [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
        doc.text(),
    )
        .into_response())
}

#[derive(Serialize)]
pub struct PutResult {
    slides: usize,
    diagnostics: Vec<Diagnostic>,
}

pub async fn put_deck(
    State(app): State<Shared>,
    Path(name): Path<String>,
    text: String,
) -> ApiResult<Json<PutResult>> {
    let deck = match app.doc(&name) {
        Some(doc) => doc.replace(text, 0),
        None => app
            .create(&name, &text)
            .map_err(bad(StatusCode::BAD_REQUEST))?
            .deck(),
    };
    Ok(Json(PutResult {
        slides: deck.count(),
        diagnostics: deck.diagnostics.clone(),
    }))
}

pub async fn themes(State(app): State<Shared>) -> ApiResult<Json<Vec<Theme>>> {
    theme::list(&app.root)
        .map(Json)
        .map_err(bad(StatusCode::INTERNAL_SERVER_ERROR))
}

#[derive(Serialize, Deserialize)]
pub struct SchemaFiles {
    css: String,
    example: String,
}

fn schema_paths(
    app: &Shared,
    theme: &str,
    schema: &str,
) -> ApiResult<(std::path::PathBuf, std::path::PathBuf)> {
    if !valid_name(theme) || !valid_name(schema) {
        return Err((StatusCode::BAD_REQUEST, "invalid name".into()));
    }
    let dir = app.root.join("themes").join(theme);
    if !dir.join("theme.toml").is_file() {
        return Err((StatusCode::NOT_FOUND, "no such theme".into()));
    }
    let schemas = dir.join("schemas");
    Ok((
        schemas.join(format!("{schema}.css")),
        schemas.join(format!("{schema}.md")),
    ))
}

pub async fn get_schema(
    State(app): State<Shared>,
    Path((theme, schema)): Path<(String, String)>,
) -> ApiResult<Json<SchemaFiles>> {
    let (css, md) = schema_paths(&app, &theme, &schema)?;
    let css =
        fs::read_to_string(css).map_err(|_| (StatusCode::NOT_FOUND, "no such schema".into()))?;
    Ok(Json(SchemaFiles {
        css,
        example: fs::read_to_string(md).unwrap_or_default(),
    }))
}

pub async fn put_schema(
    State(app): State<Shared>,
    Path((theme, schema)): Path<(String, String)>,
    Json(files): Json<SchemaFiles>,
) -> ApiResult<StatusCode> {
    let (css, md) = schema_paths(&app, &theme, &schema)?;
    fs::create_dir_all(css.parent().unwrap())
        .and_then(|()| fs::write(css, files.css))
        .and_then(|()| fs::write(md, files.example))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}
