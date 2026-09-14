//! `slides` — a hosted slide-deck editor. Markdown in, a reveal.js deck out,
//! rendered live as you type; see README.md for the design.
//!
//! Everything lives under `SLIDES_ROOT` (default: the working directory):
//!
//! ```text
//! engine/   reveal.js + KaTeX, shared by every deck (vendored, never edited)
//! themes/   one directory per theme (`theme.rs`)
//! decks/    one directory per deck: `deck.md` (the source), its assets, and
//!           `index.html`, the generated player (`player.rs`)
//! ```
//!
//! Environment: `SLIDES_ROOT`, `SLIDES_BIND` (default `127.0.0.1:7100`),
//! `SLIDES_PASSWORD` (unset ⇒ read-only: players are served, editing refuses),
//! `TRUST_PROXY_AUTH=true` (the proxy's `Remote-User` header is the login;
//! see `auth.rs`), `SLIDES_MCP_TOKEN` (bearer token for the `/mcp` endpoint;
//! unset ⇒ disabled; see `mcp.rs`), `SLIDES_AGENT_MODEL` (the in-app agent's
//! model; see `agent.rs`), `SLIDES_DEFAULT_THEME` (see `theme.rs`).

mod agent;
mod assets;
mod api;
mod auth;
mod deck;
mod live;
mod mcp;
mod pages;
mod player;
mod review;
mod theme;

use axum::routing::{get, post, put};
use axum::response::IntoResponse;
use axum::{Router, middleware};
use live::Doc;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::{env, fs};
use tower_http::services::ServeDir;

pub struct App {
    pub root: PathBuf,
    pub password: Option<String>,
    pub trust_proxy: bool,
    pub mcp_token: Option<String>,
    pub agent: agent::Agent,
    pub sessions: Mutex<HashSet<String>>,
    pub docs: Mutex<BTreeMap<String, Doc>>,
}

pub type Shared = Arc<App>;

impl App {
    pub fn doc(&self, name: &str) -> Option<Doc> {
        self.docs.lock().unwrap().get(name).cloned()
    }

    pub fn create(&self, name: &str, text: &str) -> anyhow::Result<Doc> {
        anyhow::ensure!(valid_name(name), "invalid deck name {name:?}");
        let mut docs = self.docs.lock().unwrap();
        anyhow::ensure!(!docs.contains_key(name), "deck {name} exists");
        let doc = Doc::create(&self.root, name, text)?;
        docs.insert(name.to_string(), doc.clone());
        Ok(doc)
    }

    /// Opens every `decks/*/deck.md`, which also refreshes each player.
    fn load_decks(&self) -> anyhow::Result<()> {
        let mut docs = self.docs.lock().unwrap();
        let decks = self.root.join("decks");
        fs::create_dir_all(&decks)?;
        for entry in fs::read_dir(&decks)?.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if valid_name(&name) && entry.path().join("deck.md").is_file() {
                docs.insert(name.clone(), Doc::open(&self.root, &name)?);
            }
        }
        Ok(())
    }
}

/// Deck, theme and schema names double as directory names and URL segments.
/// 404 for any path with a dot-prefixed segment (see the deck router).
async fn no_dotfiles(req: axum::extract::Request, next: middleware::Next) -> axum::response::Response {
    if req.uri().path().split('/').any(|seg| seg.starts_with('.')) {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    }
    next.run(req).await
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root =
        PathBuf::from(env::var("SLIDES_ROOT").unwrap_or_else(|_| ".".into())).canonicalize()?;
    let bind = env::var("SLIDES_BIND").unwrap_or_else(|_| "127.0.0.1:7100".into());
    let mcp_token = env::var("SLIDES_MCP_TOKEN").ok().filter(|t| !t.is_empty());
    let app: Shared = Arc::new(App {
        root,
        password: env::var("SLIDES_PASSWORD").ok().filter(|p| !p.is_empty()),
        trust_proxy: env::var("TRUST_PROXY_AUTH").is_ok_and(|v| v == "true" || v == "1"),
        agent: agent::Agent::new(&bind, mcp_token.as_deref()),
        mcp_token,
        sessions: Default::default(),
        docs: Default::default(),
    });
    app.load_decks()?;

    let gate = || middleware::from_fn_with_state(app.clone(), auth::gate);
    let decks = Router::new()
        .route("/{name}/live", get(pages::live))
        .route("/{name}/thumb", get(pages::thumb))
        .route("/{name}/ws", get(pages::ws))
        .route_layer(gate())
        .route("/{name}/export.zip", get(pages::export))
        // ServeDir would redirect `/deck/x` to `/x/`: it does not know it is nested.
        .route(
            "/{name}",
            get(
                |axum::extract::Path(name): axum::extract::Path<String>| async move {
                    axum::response::Redirect::permanent(&format!("/deck/{name}/"))
                },
            ),
        )
        // Deck folders are public (players, images), but not their dotfiles:
        // `.slides.json` holds proposals and the agent transcript, `.sources/`
        // the PDFs fetch_asset downloaded.
        .fallback_service(
            Router::new()
                .fallback_service(ServeDir::new(app.root.join("decks")))
                .layer(middleware::from_fn(no_dotfiles)),
        );
    let editing = Router::new()
        .route("/new", axum::routing::post(pages::create))
        .route("/edit/{name}", get(pages::editor))
        .route("/api/decks", get(api::decks))
        .route("/api/decks/{name}", get(api::get_deck).put(api::put_deck))
        // Review mode + the in-app agent (the editor's drawer; see review.rs, agent.rs)
        .route("/api/decks/{name}/state", get(api::get_state))
        .route("/api/decks/{name}/review", put(api::put_review))
        .route(
            "/api/decks/{name}/proposal/clear",
            post(api::clear_resolved),
        )
        .route(
            "/api/decks/{name}/proposal/{op}/{action}",
            post(api::op_action),
        )
        .route(
            "/api/decks/{name}/proposal/{op}/conflict/{how}",
            post(api::conflict_action),
        )
        .route(
            "/api/decks/{name}/changeset/{id}/{action}",
            post(api::changeset_action),
        )
        .route("/api/agent/defaults", get(api::agent_defaults))
        .route("/api/decks/{name}/agent", post(api::agent_send))
        .route("/api/decks/{name}/agent/stop", post(api::agent_stop))
        .route("/api/decks/{name}/agent/reset", post(api::agent_reset))
        .route(
            "/api/decks/{name}/agent/thread/{thread}/close",
            post(api::thread_close),
        )
        .route("/api/themes", get(api::themes))
        .route(
            "/api/themes/{theme}/schemas/{schema}",
            get(api::get_schema).put(api::put_schema),
        )
        .route_layer(gate());
    let router = Router::new()
        .route("/", get(pages::index))
        .route("/login", get(auth::login_form).post(auth::login))
        .route("/logout", get(auth::logout))
        .route("/static/{file}", get(pages::static_file))
        // Outside the gate: mcp.rs checks its own bearer token.
        .route("/mcp", post(mcp::handler))
        .merge(editing)
        .nest("/deck", decks)
        .nest_service("/engine", ServeDir::new(app.root.join("engine")))
        .nest_service("/themes", ServeDir::new(app.root.join("themes")))
        .with_state(app.clone());

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    eprintln!(
        "slides: {} deck(s) under {}, {}, MCP {}, agent {} — http://{bind}/",
        app.docs.lock().unwrap().len(),
        app.root.display(),
        if app.trust_proxy {
            "editing enabled (proxy auth)"
        } else if app.password.is_some() {
            "editing enabled"
        } else {
            "read-only (set SLIDES_PASSWORD)"
        },
        if app.mcp_token.is_some() {
            "on"
        } else {
            "off (set SLIDES_MCP_TOKEN)"
        },
        match app.agent.available() {
            Ok(_) => format!("on ({})", app.agent.model),
            Err(e) => format!("off ({e})"),
        }
    );
    axum::serve(listener, router).await?;
    Ok(())
}
