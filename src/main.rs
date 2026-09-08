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
//! `SLIDES_PASSWORD` (unset ⇒ read-only: players are served, editing refuses).

mod api;
mod auth;
mod deck;
mod live;
mod pages;
mod player;
mod theme;

use axum::routing::get;
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
        for entry in fs::read_dir(self.root.join("decks"))?.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if valid_name(&name) && entry.path().join("deck.md").is_file() {
                docs.insert(name.clone(), Doc::open(&self.root, &name)?);
            }
        }
        Ok(())
    }
}

/// Deck, theme and schema names double as directory names and URL segments.
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
    let app: Shared = Arc::new(App {
        root,
        password: env::var("SLIDES_PASSWORD").ok().filter(|p| !p.is_empty()),
        sessions: Default::default(),
        docs: Default::default(),
    });
    app.load_decks()?;

    let gate = || middleware::from_fn_with_state(app.clone(), auth::gate);
    let decks = Router::new()
        .route("/{name}/live", get(pages::live))
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
        .fallback_service(ServeDir::new(app.root.join("decks")));
    let editing = Router::new()
        .route("/new", axum::routing::post(pages::create))
        .route("/edit/{name}", get(pages::editor))
        .route("/api/decks", get(api::decks))
        .route("/api/decks/{name}", get(api::get_deck).put(api::put_deck))
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
        .merge(editing)
        .nest("/deck", decks)
        .nest_service("/engine", ServeDir::new(app.root.join("engine")))
        .nest_service("/themes", ServeDir::new(app.root.join("themes")))
        .with_state(app.clone());

    let bind = env::var("SLIDES_BIND").unwrap_or_else(|_| "127.0.0.1:7100".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    eprintln!(
        "slides: {} deck(s) under {}, {} — http://{bind}/",
        app.docs.lock().unwrap().len(),
        app.root.display(),
        if app.password.is_some() {
            "editing enabled"
        } else {
            "read-only (set SLIDES_PASSWORD)"
        }
    );
    axum::serve(listener, router).await?;
    Ok(())
}
