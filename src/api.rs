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
//! Review mode and the in-app agent (used by the editor's drawer; JSON in, JSON out):
//!
//! ```text
//! GET  /api/decks/{name}/state                     → review state view (see review.rs)
//! PUT  /api/decks/{name}/review        ← {review}  toggle review mode for the deck
//! POST /api/decks/{name}/proposal/{op}/{action}    accept | reject | comment {comment} | withdraw
//! POST /api/decks/{name}/proposal/{op}/conflict/{how}
//!                                                  settle a conflict: keep (this op) | keep_other {other} |
//!                                                  merge {model?, effort?} — starts a merge thread → {thread}
//! POST /api/decks/{name}/changeset/{id}/{action}   accept | reject | withdraw every pending op of a changeset
//! POST /api/decks/{name}/proposal/clear            drop resolved ops
//! POST /api/decks/{name}/agent         ← {message, slide?, thread?, model?, effort?}
//!                                                  ask: a new thread, or a follow-up in `thread` → {thread}
//! POST /api/decks/{name}/agent/stop    ← {thread?} stop one thread's run (all of them without)
//! POST /api/decks/{name}/agent/reset               drop every thread
//! POST /api/decks/{name}/agent/thread/{id}/close   drop one thread (its proposals stay)
//! ```
//!
//! Authenticate with the session cookie or `Authorization: Bearer <SLIDES_PASSWORD>`.

use crate::deck::Diagnostic;
use crate::review::{Action, BatchAction, Kind};
use crate::theme::{self, Theme};
use crate::{Shared, agent, valid_name};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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
) -> ApiResult<Response> {
    let deck = match app.doc(&name) {
        Some(doc) if doc.review() => {
            // Review mode: the whole-deck write is queued for the author instead.
            let op = doc
                .propose(
                    Kind::Deck,
                    None,
                    false,
                    text,
                    "replaced the whole deck via PUT /api".into(),
                    None,
                )
                .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
            return Ok((
                StatusCode::ACCEPTED,
                Json(json!({ "proposed": true, "op": op })),
            )
                .into_response());
        }
        Some(doc) => doc.replace(text, 0),
        None => app
            .create(&name, &text)
            .map_err(bad(StatusCode::BAD_REQUEST))?
            .deck(),
    };
    Ok(Json(PutResult {
        slides: deck.count(),
        diagnostics: deck.diagnostics.clone(),
    })
    .into_response())
}

// ---- review mode + agent ----------------------------------------------------

fn doc_of(app: &Shared, name: &str) -> ApiResult<crate::live::Doc> {
    app.doc(name)
        .ok_or((StatusCode::NOT_FOUND, "no such deck".into()))
}

pub async fn get_state(
    State(app): State<Shared>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(doc_of(&app, &name)?.state_view()))
}

#[derive(Deserialize)]
pub struct ReviewFlag {
    review: bool,
}

pub async fn put_review(
    State(app): State<Shared>,
    Path(name): Path<String>,
    Json(f): Json<ReviewFlag>,
) -> ApiResult<StatusCode> {
    doc_of(&app, &name)?.set_review(f.review);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
pub struct OpBody {
    #[serde(default)]
    comment: String,
}

pub async fn op_action(
    State(app): State<Shared>,
    Path((name, op, action)): Path<(String, u32, String)>,
    Json(body): Json<OpBody>,
) -> ApiResult<Json<Value>> {
    let doc = doc_of(&app, &name)?;
    let action = match action.as_str() {
        "accept" => Action::Accept,
        "reject" => Action::Reject,
        "comment" => Action::Comment(body.comment),
        "withdraw" => Action::Withdraw,
        _ => return Err((StatusCode::NOT_FOUND, "no such action".into())),
    };
    doc.resolve(op, action)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    Ok(Json(doc.state_view()))
}

pub async fn changeset_action(
    State(app): State<Shared>,
    Path((name, id, action)): Path<(String, u32, String)>,
) -> ApiResult<Json<Value>> {
    let doc = doc_of(&app, &name)?;
    let action = match action.as_str() {
        "accept" => BatchAction::Accept,
        "reject" => BatchAction::Reject,
        "withdraw" => BatchAction::Withdraw,
        _ => return Err((StatusCode::NOT_FOUND, "no such action".into())),
    };
    let result = doc
        .resolve_changeset(id, action)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let mut v = doc.state_view();
    v["result"] = result;
    Ok(Json(v))
}

pub async fn clear_resolved(
    State(app): State<Shared>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let doc = doc_of(&app, &name)?;
    doc.clear_resolved();
    Ok(Json(doc.state_view()))
}

#[derive(Deserialize)]
pub struct AgentMessage {
    message: String,
    #[serde(default)]
    slide: Option<usize>,
    /// Follow up in this thread; without it the message starts a new one.
    #[serde(default)]
    thread: Option<u32>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
}

/// The Ask form's model/effort, checked.
fn run_options(model: Option<String>, effort: Option<String>) -> ApiResult<agent::RunOptions> {
    let model = model
        .filter(|s| !s.is_empty())
        .map(|s| {
            let ok = s
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._".contains(c));
            ok.then_some(s)
                .ok_or((StatusCode::BAD_REQUEST, "bad model id".to_string()))
        })
        .transpose()?;
    let effort = effort
        .filter(|s| !s.is_empty())
        .map(|s| {
            agent::EFFORTS
                .contains(&s.as_str())
                .then_some(s)
                .ok_or((StatusCode::BAD_REQUEST, "bad effort".to_string()))
        })
        .transpose()?;
    Ok(agent::RunOptions { model, effort })
}

pub async fn agent_send(
    State(app): State<Shared>,
    Path(name): Path<String>,
    Json(m): Json<AgentMessage>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let doc = doc_of(&app, &name)?;
    let message = m.message.trim().to_string();
    if message.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "empty message".into()));
    }
    let mut context = format!("- Date: {}\n", date_today());
    if let Some(s) = m.slide {
        let text = doc.text();
        let heading = crate::review::sources(&text)
            .get(s.wrapping_sub(1))
            .and_then(|src| crate::mcp::heading(src))
            .unwrap_or_default();
        context += &format!(
            "- The author's cursor is on slide {s}{}\n",
            if heading.is_empty() {
                String::new()
            } else {
                format!(" ({heading})")
            }
        );
    }
    let opts = run_options(m.model, m.effort)?;
    let thread = agent::start(app.clone(), doc, name, m.thread, message, context, opts)
        .map_err(|e| (StatusCode::CONFLICT, e))?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "thread": thread }))))
}

#[derive(Deserialize, Default)]
pub struct ConflictBody {
    #[serde(default)]
    other: Option<u32>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
}

/// Settles a conflict: `keep` this op, `keep_other` (body `other`), or
/// `merge` — a merge thread reconciles this op with everything it conflicts with.
pub async fn conflict_action(
    State(app): State<Shared>,
    Path((name, op, how)): Path<(String, u32, String)>,
    Json(body): Json<ConflictBody>,
) -> ApiResult<Json<Value>> {
    let doc = doc_of(&app, &name)?;
    let mut v = match how.as_str() {
        "keep" => doc.resolve_conflict(op, None),
        "keep_other" => {
            let other = body.other.ok_or((StatusCode::BAD_REQUEST, "missing 'other'".to_string()))?;
            doc.resolve_conflict(op, Some(other))
        }
        "merge" => {
            let opts = run_options(body.model, body.effort)?;
            agent::start_merge(app.clone(), doc.clone(), name, op, opts).map(|t| json!({ "thread": t }))
        }
        _ => return Err((StatusCode::NOT_FOUND, "no such action".into())),
    }
    .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    v["state"] = doc.state_view();
    Ok(Json(v))
}

/// Defaults for the Ask form's model/effort selects, and whether the agent is
/// configured at all (the editor hides ✦ Ask when it is not).
pub async fn agent_defaults(State(app): State<Shared>) -> Json<Value> {
    let unavailable = app.agent.available().err();
    Json(json!({
        "model": app.agent.model,
        "effort": app.agent.effort,
        "efforts": agent::EFFORTS,
        "available": unavailable.is_none(),
        "unavailable": unavailable,
    }))
}

/// Today as YYYY-MM-DD (UTC) for the agent's prompt; civil-from-days, no chrono.
pub fn date_today() -> String {
    let z = (crate::review::now() / 86400) as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

#[derive(Deserialize, Default)]
pub struct ThreadBody {
    #[serde(default)]
    thread: Option<u32>,
}

pub async fn agent_stop(
    State(app): State<Shared>,
    Path(name): Path<String>,
    body: Option<Json<ThreadBody>>,
) -> ApiResult<StatusCode> {
    doc_of(&app, &name)?;
    let stopped = match body.and_then(|Json(b)| b.thread) {
        Some(t) => app.agent.stop(&name, t),
        None => app.agent.stop_all(&name) > 0,
    };
    Ok(if stopped {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    })
}

pub async fn agent_reset(
    State(app): State<Shared>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let doc = doc_of(&app, &name)?;
    app.agent.stop_all(&name);
    doc.agent_reset();
    Ok(StatusCode::NO_CONTENT)
}

pub async fn thread_close(
    State(app): State<Shared>,
    Path((name, thread)): Path<(String, u32)>,
) -> ApiResult<StatusCode> {
    let doc = doc_of(&app, &name)?;
    app.agent.stop(&name, thread);
    doc.thread_close(thread);
    Ok(StatusCode::NO_CONTENT)
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

pub fn schema_paths(
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
