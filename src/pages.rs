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
        let themes: String = theme::list(&app.root)
            .unwrap_or_default()
            .iter()
            .map(|t| format!(r#"<option>{}</option>"#, escape(&t.name)))
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
<a href="/deck/{name}/" target="_blank">player</a><a href="/deck/{name}/live" target="_blank">live view</a><a href="/deck/{name}/export.zip">export</a></header>
<main><div id="editor"></div><div id="divider"></div><iframe id="preview" src="/deck/{name}/live"></iframe></main>
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
