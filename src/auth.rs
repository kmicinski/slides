//! The password gate. One shared password (`SLIDES_PASSWORD`): the login form
//! sets a session cookie, tools send it as a bearer token. Sessions live in
//! memory and end with the process. Without a password the server is
//! read-only — players are public anyway; editing routes refuse.
//! TLS is the reverse proxy's job.

use crate::Shared;
use axum::Form;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use std::time::Duration;
use subtle::ConstantTimeEq;

pub fn authorized(app: &Shared, headers: &HeaderMap) -> bool {
    let Some(password) = &app.password else {
        return false;
    };
    let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
    if let Some(token) = header(header::AUTHORIZATION).and_then(|v| v.strip_prefix("Bearer ")) {
        return eq(token, password);
    }
    header(header::COOKIE)
        .and_then(|c| {
            c.split(';')
                .map(str::trim)
                .find_map(|c| c.strip_prefix("session="))
        })
        .is_some_and(|id| app.sessions.lock().unwrap().contains(id))
}

fn eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

/// Middleware for editing routes: browsers are sent to the login page, tools get 401.
pub async fn gate(State(app): State<Shared>, req: Request, next: Next) -> Response {
    if authorized(&app, req.headers()) {
        return next.run(req).await;
    }
    if app.password.is_none() {
        return (
            StatusCode::FORBIDDEN,
            "read-only: SLIDES_PASSWORD is not set",
        )
            .into_response();
    }
    let wants_html = req
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/html"));
    if wants_html {
        Redirect::to(&format!("/login?next={}", req.uri().path())).into_response()
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

#[derive(Deserialize)]
pub struct Login {
    password: String,
    #[serde(default)]
    next: String,
}

#[derive(Deserialize)]
pub struct NextParam {
    #[serde(default)]
    next: String,
}

fn page(next: &str, error: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>slides · login</title>
<link rel="stylesheet" href="/static/editor.css"></head>
<body class="page"><form method="post" action="/login" class="login">
<label>password <input type="password" name="password" autofocus></label>
<input type="hidden" name="next" value="{}"><button>log in</button><p class="error">{}</p></form></body></html>"#,
        crate::deck::escape(next),
        error
    ))
}

pub async fn login_form(Query(q): Query<NextParam>) -> Html<String> {
    page(&q.next, "")
}

pub async fn login(State(app): State<Shared>, Form(form): Form<Login>) -> Response {
    if app
        .password
        .as_deref()
        .is_some_and(|p| eq(&form.password, p))
    {
        let id: String = rand::random::<[u8; 32]>()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        app.sessions.lock().unwrap().insert(id.clone());
        let next = if form.next.starts_with('/') && !form.next.starts_with("//") {
            form.next.as_str()
        } else {
            "/"
        };
        return (
            [(
                header::SET_COOKIE,
                format!("session={id}; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000"),
            )],
            Redirect::to(next),
        )
            .into_response();
    }
    tokio::time::sleep(Duration::from_secs(1)).await; // blunt but effective against guessing
    (StatusCode::UNAUTHORIZED, page(&form.next, "wrong password")).into_response()
}

pub async fn logout(State(app): State<Shared>, headers: HeaderMap) -> Response {
    if let Some(id) = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| {
            c.split(';')
                .map(str::trim)
                .find_map(|c| c.strip_prefix("session="))
        })
    {
        app.sessions.lock().unwrap().remove(id);
    }
    (
        [(header::SET_COOKIE, "session=; Path=/; Max-Age=0")],
        Redirect::to("/"),
    )
        .into_response()
}
