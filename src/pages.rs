//! The app's own pages — deck list, editor, live preview — and the static
//! assets they use. HTML lives here as Rust strings; the browser code is
//! TypeScript in `web/src`, compiled by `tsc` into `web/dist` and embedded
//! at build time (`make web`).

use crate::deck::escape;
use crate::theme::{self, Theme};
use crate::{Shared, auth, live, player, valid_name};
use axum::Form;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;

pub async fn static_file(Path(file): Path<String>) -> Response {
    let (mime, body) = match file.as_str() {
        "editor.js" => ("text/javascript", include_str!("../web/dist/editor.js")),
        "live.js" => ("text/javascript", include_str!("../web/dist/live.js")),
        "protocol.js" => ("text/javascript", include_str!("../web/dist/protocol.js")),
        "review.js" => ("text/javascript", include_str!("../web/dist/review.js")),
        "editor.css" => ("text/css", include_str!("../web/editor.css")),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

pub async fn index(State(app): State<Shared>, headers: HeaderMap) -> Html<String> {
    let editing = auth::authorized(&app, &headers);
    let rows: String = app.docs.lock().unwrap().iter().map(|(name, doc)| {
        let title = escape(doc.deck().title.as_deref().unwrap_or(name));
        let edit = if editing { format!(r#" <a class="edit" href="/edit/{name}">edit</a>"#) } else { String::new() };
        format!(r#"<li><a href="/deck/{name}/">{title}</a> <span class="name">{name}</span>{edit}</li>"#)
    }).collect();
    let form = if editing {
        let default = theme::default(&app.root).map(|t| t.name).unwrap_or_default();
        let themes: String = theme::list(&app.root)
            .unwrap_or_default()
            .iter()
            .map(|t| {
                let sel = if t.name == default { " selected" } else { "" };
                format!(r#"<option{sel}>{}</option>"#, escape(&t.name))
            })
            .collect();
        format!(
            r#"<form method="post" action="/new"><input name="name" placeholder="new-deck-name" pattern="[A-Za-z0-9][A-Za-z0-9._-]*" required> <select name="theme">{themes}</select> <button>create</button></form>"#
        )
    } else if app.password.is_some() {
        r#"<p><a href="/login">log in</a> to edit</p>"#.into()
    } else {
        String::new()
    };
    Html(format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>slides</title>
<link rel="stylesheet" href="/static/editor.css"></head>
<body class="page"><h1>slides</h1><ul class="decks">{rows}</ul>{form}</body></html>"#
    ))
}

#[derive(Deserialize)]
pub struct NewDeck {
    name: String,
    theme: String,
}

pub async fn create(
    State(app): State<Shared>,
    Form(form): Form<NewDeck>,
) -> Result<Redirect, (StatusCode, String)> {
    if app.doc(&form.name).is_none() {
        let starter = Theme::resolve(&app.root, Some(&form.theme)).and_then(|t| t.starter());
        let starter = starter.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
        app.create(&form.name, &starter)
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    }
    Ok(Redirect::to(&format!("/edit/{}", form.name)))
}

pub async fn editor(
    State(app): State<Shared>,
    Path(name): Path<String>,
) -> Result<Html<String>, StatusCode> {
    if !valid_name(&name) || app.doc(&name).is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Html(format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>{name} · slides</title>
<link rel="stylesheet" href="/static/editor.css">
<script src="https://cdnjs.cloudflare.com/ajax/libs/monaco-editor/0.45.0/min/vs/loader.min.js"></script>
</head>
<body data-deck="{name}">
<header><a href="/">decks</a><strong>{name}</strong><span id="status"></span><span class="spacer"></span>
<label class="toggle" title="When on, edits made by tools (MCP clients, the agent) are queued as proposals you accept or reject"><input type="checkbox" id="review-mode" checked> review tool edits</label>
<button id="open-ask" class="ask">✦ Ask</button><button id="open-review">proposals <span id="pcount2" class="count" hidden></span></button>
<a href="/deck/{name}/" target="_blank">player</a><a href="/deck/{name}/live" target="_blank">live view</a><a href="/deck/{name}/export.zip">export</a></header>
<main>
<div id="editor"></div><div id="divider"></div>
<div id="preview-pane">
  <div id="compare-bar" hidden title="keys: n / p next and previous · a accept · A accept the whole changeset · r reject · c comment · esc close">
    <button id="cmp-prev" title="previous (p)">◀</button><span id="cmp-pos" class="pos"></span><button id="cmp-next" title="next (n)">▶</button>
    <span id="cmp-title" class="title"></span><span class="spacer"></span>
    <button id="cmp-accept" class="accept" title="accept this slide (a)">accept</button><button id="cmp-accept-all" class="accept" title="accept every pending slide in this changeset (A)">accept all</button><button id="cmp-reject" title="reject (r)">reject</button><button id="cmp-comment" title="comment (c)">comment</button><button id="cmp-close" title="back to the single preview (esc)">×</button>
  </div>
  <div id="panes">
    <div class="pane" id="pane-current"><div class="pane-label" id="label-current" hidden>current</div><iframe id="preview" src="/deck/{name}/live"></iframe><div id="delete-overlay" hidden>removed in this proposal</div></div>
    <div class="pane" id="pane-proposed" hidden><div class="pane-label" id="label-proposed">proposed</div><iframe id="preview2" src="/deck/{name}/live"></iframe></div>
  </div>
  <div id="preview-badge" hidden>proposed</div>
</div>
<aside id="drawer" hidden>
  <nav><button data-tab="ask" class="active">✦ Ask</button><button data-tab="review">Review <span id="pcount" class="count" hidden></span></button><span class="spacer"></span><button id="drawer-close" title="close">×</button></nav>
  <section id="tab-ask">
    <div id="transcript"></div>
    <form id="ask-form">
      <textarea id="ask-input" rows="3" placeholder="What should change? Enter sends, Shift+Enter for a newline."></textarea>
      <div class="row"><span id="ask-context" class="muted"></span><span class="spacer"></span>
        <select id="ask-model" title="model"><option value="">default model</option><option value="claude-fable-5-1">fable 5.1</option><option value="claude-opus-5">opus 5</option><option value="claude-sonnet-5">sonnet 5</option></select>
        <select id="ask-effort" title="effort: how long the model thinks"><option value="low">quick</option><option value="medium">normal</option><option value="high">careful</option><option value="xhigh">thorough</option></select>
        <button type="button" id="ask-stop" hidden>stop</button><button type="button" id="ask-new" title="start a fresh conversation">new</button><button type="submit" id="ask-send">send</button></div>
    </form>
  </section>
  <section id="tab-review" hidden>
    <div class="row"><label><input type="checkbox" id="show-proposed"> show proposed deck in the preview</label><span class="spacer"></span><button id="clear-resolved">clear resolved</button></div>
    <div id="ops"></div>
  </section>
</aside>
</main>
<script type="module" src="/static/editor.js"></script>
</body></html>"#
    )))
}

/// The player with an empty slide container; `live.js` fills it over the socket.
pub async fn live(
    State(app): State<Shared>,
    Path(name): Path<String>,
) -> Result<Html<String>, (StatusCode, String)> {
    let doc = app
        .doc(&name)
        .ok_or((StatusCode::NOT_FOUND, "no such deck".into()))?;
    let deck = doc.deck();
    let theme = Theme::resolve(&app.root, deck.theme.as_deref())
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    Ok(Html(player::html(&deck, &name, &theme, true)))
}

#[derive(Deserialize)]
pub struct ThumbQuery {
    #[serde(default)]
    view: String,
    slide: usize,
}

/// One slide, current or proposed deck, as a chrome-less player (`player::thumb`).
pub async fn thumb(
    State(app): State<Shared>,
    Path(name): Path<String>,
    axum::extract::Query(q): axum::extract::Query<ThumbQuery>,
) -> Result<Response, (StatusCode, String)> {
    let doc = app
        .doc(&name)
        .ok_or((StatusCode::NOT_FOUND, "no such deck".into()))?;
    let deck = match q.view.as_str() {
        "proposed" => doc.proposed().unwrap_or_else(|| doc.deck()),
        _ => doc.deck(),
    };
    let slide = deck
        .columns
        .iter()
        .flatten()
        .nth(q.slide.wrapping_sub(1))
        .ok_or((StatusCode::NOT_FOUND, "no such slide".into()))?;
    let theme = Theme::resolve(&app.root, deck.theme.as_deref())
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Html(player::thumb(slide, &name, &theme)),
    )
        .into_response())
}

pub async fn export(
    State(app): State<Shared>,
    Path(name): Path<String>,
) -> Result<Response, (StatusCode, String)> {
    let doc = app
        .doc(&name)
        .ok_or((StatusCode::NOT_FOUND, "no such deck".into()))?;
    let theme = Theme::resolve(&app.root, doc.deck().theme.as_deref())
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let zip = player::export(&app.root, &name, &theme)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}.zip\""),
            ),
        ],
        zip,
    )
        .into_response())
}

pub async fn ws(
    State(app): State<Shared>,
    Path(name): Path<String>,
    headers: HeaderMap,
    upgrade: axum::extract::WebSocketUpgrade,
) -> Response {
    // Cookies ride along on cross-site WebSocket handshakes; only our own pages may open one.
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .and_then(|o| o.split("://").nth(1));
    if origin.is_none() || origin != host {
        return StatusCode::FORBIDDEN.into_response();
    }
    match app.doc(&name) {
        Some(doc) => upgrade.on_upgrade(move |socket| live::session(socket, doc)),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
