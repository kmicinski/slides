//! Live documents: the in-memory text of every deck, the WebSocket protocol the
//! editor and preview speak, and persistence.
//!
//! One [`Doc`] per deck holds the authoritative text. Every writer — the browser
//! editor (deltas over its socket) or a tool (whole text over the API) — changes
//! it through [`Doc::edit`] / [`Doc::replace`], which re-render the deck and
//! broadcast an [`Update`] to all connections of that deck. A connection turns
//! updates into messages:
//!
//! ```text
//! server → client
//!   {"type":"text","text"}                          whole text: answer to `sync`, after an external
//!                                                   replace, or when an edit was refused (desync)
//!   {"type":"edit","changes"}                       another editor's delta, to apply locally
//!   {"type":"patch","cols":[[{"line","body"?}]],"diags"}
//!                                                   every slide's first line; bodies only where the
//!                                                   slide differs from what this connection last got
//!   {"type":"saved","error"?}                       the debounced write to disk finished
//!   {"type":"state","state"}                        review state (proposal ops, agent transcript) —
//!                                                   on `sync` and whenever it changes (`review.rs`)
//!   {"type":"agent","event"}                        a live event from the in-app agent (`agent.rs`)
//! client → server
//!   {"type":"sync"}                                 become an editor: get the text, forwarded edits,
//!                                                   and body-less patches (the editor only needs lines)
//!   {"type":"edit","changes":[{offset,length,text}],"hash"}
//!                                                   Monaco's content changes (UTF-16 offsets, each
//!                                                   relative to the text before the whole batch) and
//!                                                   an FNV-1a hash of the resulting text
//!   {"type":"view","proposed"}                      preview switch: patch from the proposed deck
//!                                                   (current text + pending proposal) or the real one
//! ```
//!
//! Patches are positional: a connection remembers the deck it last sent and a
//! body travels only where (column, row) changed. The preview keeps one
//! reveal.js instance alive and swaps changed bodies in place, so the slide
//! being edited is never replaced — that is what makes updates flicker-free.
//! An edit whose hash does not match is refused and answered with the full
//! text, which heals any drift between a client and the server.
//!
//! `deck.md` and `index.html` are written one second after the last change.

use crate::deck::{Deck, Diagnostic, Renderer, Slide};
use crate::player;
use crate::review::{self, DeckState};
use crate::theme::Theme;
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use std::{fs, io};
use tokio::sync::{broadcast, watch};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edit {
    pub offset: usize,
    pub length: usize,
    pub text: String,
}

pub enum Change {
    Delta(Vec<Edit>),
    Text(Arc<str>),
}

pub enum Update {
    Changed {
        deck: Arc<Deck>,
        proposed: Option<Arc<Deck>>,
        change: Change,
        origin: u64,
    },
    Saved(Option<String>),
    /// The proposal changed (or the text under it did): the proposed deck, if
    /// any, and the review state view for editors.
    Proposal {
        deck: Option<Arc<Deck>>,
        view: Arc<Value>,
    },
    Agent(Arc<Value>),
}

/// Handle to a deck's live state; clones share it.
#[derive(Clone)]
pub struct Doc(Arc<Mutex<Inner>>);

pub(crate) struct Inner {
    root: PathBuf,
    pub(crate) name: String,
    pub(crate) text: String,
    version: u64,
    renderer: Renderer,
    deck: Arc<Deck>,
    pub(crate) tx: broadcast::Sender<Arc<Update>>,
    /// Review mode, the open proposal and the agent transcript (`review.rs`).
    pub(crate) state: DeckState,
    /// Current text with the pending proposal applied; `None` when nothing is pending.
    pub(crate) proposed: Option<Arc<Deck>>,
    /// Bumped on every review-state change; `await_review` waits on it.
    pub(crate) rev: watch::Sender<u64>,
}

impl Doc {
    /// Loads `decks/<name>/deck.md` and (re)writes its player.
    pub fn open(root: &Path, name: &str) -> io::Result<Doc> {
        let dir = root.join("decks").join(name);
        let text = fs::read_to_string(dir.join("deck.md"))?;
        let mut renderer = Renderer::default();
        let deck = Arc::new(renderer.render(&text));
        let inner = Inner {
            root: root.into(),
            name: name.into(),
            text,
            version: 0,
            renderer,
            deck,
            tx: broadcast::channel(64).0,
            state: review::load(&dir),
            proposed: None,
            rev: watch::channel(0).0,
        };
        if let Err(e) = inner.write_player() {
            eprintln!("deck {name}: {e:#}");
        }
        let doc = Doc(Arc::new(Mutex::new(inner)));
        let mut g = doc.lock();
        doc.recompute_proposed(&mut g);
        drop(g);
        Ok(doc)
    }

    pub fn create(root: &Path, name: &str, text: &str) -> io::Result<Doc> {
        let dir = root.join("decks").join(name);
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("deck.md"), text)?;
        Doc::open(root, name)
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap()
    }

    pub fn deck(&self) -> Arc<Deck> {
        self.0.lock().unwrap().deck.clone()
    }

    pub fn proposed(&self) -> Option<Arc<Deck>> {
        self.0.lock().unwrap().proposed.clone()
    }

    pub fn broadcast_agent(&self, event: Value) {
        let g = self.0.lock().unwrap();
        let _ = g.tx.send(Arc::new(Update::Agent(Arc::new(event))));
    }

    pub fn text(&self) -> String {
        self.0.lock().unwrap().text.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Update>> {
        self.0.lock().unwrap().tx.subscribe()
    }

    /// Applies an editor's delta. Returns `false`, changing nothing, when `hash`
    /// is not the hash of the result — the client's text was not the server's.
    pub fn edit(&self, changes: &[Edit], hash: u32, origin: u64) -> bool {
        let mut g = self.0.lock().unwrap();
        let mut text = g.text.clone();
        for c in changes {
            let a = byte_at(&text, c.offset);
            let b = a + byte_at(&text[a..], c.length);
            text.replace_range(a..b, &c.text);
        }
        if fnv1a(&text) != hash {
            return false;
        }
        g.text = text;
        self.commit(&mut g, Change::Delta(changes.to_vec()), origin);
        true
    }

    /// Replaces the whole text (tools, API).
    pub fn replace(&self, text: String, origin: u64) -> Arc<Deck> {
        self.update(origin, |_| Ok::<_, std::convert::Infallible>(text))
            .unwrap_or_else(|e| match e {})
    }

    /// Rewrites the text under the lock — `f` sees the current text and returns
    /// the new one — so a tool editing one slide cannot race an editor's delta.
    pub fn update<E>(
        &self,
        origin: u64,
        f: impl FnOnce(&str) -> Result<String, E>,
    ) -> Result<Arc<Deck>, E> {
        let mut g = self.0.lock().unwrap();
        let text = f(&g.text)?;
        let change = Change::Text(text.as_str().into());
        g.text = text;
        self.commit(&mut g, change, origin);
        Ok(g.deck.clone())
    }

    pub(crate) fn commit(&self, g: &mut Inner, change: Change, origin: u64) {
        g.deck = Arc::new(g.renderer.render(&g.text));
        g.version += 1;
        // The proposed deck is derived from the text, so it moves with it.
        self.recompute_proposed(g);
        let _ = g.tx.send(Arc::new(Update::Changed {
            deck: g.deck.clone(),
            proposed: g.proposed.clone(),
            change,
            origin,
        }));
        if g.state.proposal.is_some() {
            // Positions and staleness in the review panel depend on the text.
            let proposed = review::proposed_text(&g.text, g.ops());
            let view = Arc::new(review::view(&g.text, &g.state, &proposed));
            let _ = g.tx.send(Arc::new(Update::Proposal {
                deck: g.proposed.clone(),
                view,
            }));
        }
        let (doc, version) = (self.clone(), g.version);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let g = doc.0.lock().unwrap();
            if g.version == version {
                let error = g
                    .write_source()
                    .and_then(|()| g.write_player())
                    .err()
                    .map(|e| format!("{e:#}"));
                let _ = g.tx.send(Arc::new(Update::Saved(error)));
            }
        });
    }
}

impl Inner {
    pub(crate) fn dir(&self) -> PathBuf {
        self.root.join("decks").join(&self.name)
    }

    fn write_source(&self) -> anyhow::Result<()> {
        Ok(fs::write(self.dir().join("deck.md"), &self.text)?)
    }

    fn write_player(&self) -> anyhow::Result<()> {
        let theme = Theme::resolve(&self.root, self.deck.theme.as_deref())?;
        Ok(fs::write(
            self.dir().join("index.html"),
            player::html(&self.deck, &self.name, &theme, false),
        )?)
    }
}

/// Byte index of the `utf16`-th UTF-16 code unit (Monaco's offsets).
fn byte_at(s: &str, utf16: usize) -> usize {
    let mut n = 0;
    for (i, c) in s.char_indices() {
        if n >= utf16 {
            return i;
        }
        n += c.len_utf16();
    }
    s.len()
}

/// 32-bit FNV-1a over Unicode scalar values; `fnv1a` in `web/src/editor.ts` must match.
pub fn fnv1a(s: &str) -> u32 {
    s.chars().fold(0x811c9dc5u32, |h, c| {
        (h ^ c as u32).wrapping_mul(0x01000193)
    })
}

// ---- WebSocket session ------------------------------------------------------

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ClientMsg {
    Sync,
    Edit { changes: Vec<Edit>, hash: u32 },
    View { proposed: bool },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ServerMsg<'a> {
    Text {
        text: &'a str,
    },
    Edit {
        changes: &'a [Edit],
    },
    Patch {
        cols: Vec<Vec<PatchSlide<'a>>>,
        diags: &'a [Diagnostic],
    },
    Saved {
        error: &'a Option<String>,
    },
    State {
        state: &'a Value,
    },
    Agent {
        event: &'a Value,
    },
}

#[derive(Serialize)]
struct PatchSlide<'a> {
    line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<&'a Slide>,
}

fn patch<'a>(prev: &Deck, next: &'a Deck, bodies: bool) -> ServerMsg<'a> {
    let cols = next
        .columns
        .iter()
        .enumerate()
        .map(|(h, col)| {
            col.iter()
                .enumerate()
                .map(|(v, slide)| {
                    let same = prev
                        .columns
                        .get(h)
                        .and_then(|c| c.get(v))
                        .is_some_and(|p| p == slide);
                    PatchSlide {
                        line: slide.line,
                        body: (bodies && !same).then_some(slide),
                    }
                })
                .collect()
        })
        .collect();
    ServerMsg::Patch {
        cols,
        diags: &next.diagnostics,
    }
}

fn message(msg: &ServerMsg) -> Message {
    Message::Text(serde_json::to_string(msg).unwrap().into())
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub async fn session(socket: WebSocket, doc: Doc) {
    let _ = run(socket, doc).await;
}

async fn run(socket: WebSocket, doc: Doc) -> Result<(), axum::Error> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut rx = doc.subscribe();
    let (mut sink, mut stream) = socket.split();
    let mut editor = false;
    let mut proposed_view = false;
    let mut prev = Arc::new(Deck::default());
    let deck = doc.deck();
    sink.send(message(&patch(&prev, &deck, true))).await?;
    prev = deck;
    loop {
        tokio::select! {
            msg = stream.next() => {
                let Some(Ok(Message::Text(text))) = msg else {
                    if matches!(msg, Some(Ok(_))) { continue } else { break }
                };
                match serde_json::from_str(&text) {
                    Ok(ClientMsg::Sync) => {
                        editor = true;
                        sink.send(message(&ServerMsg::Text { text: &doc.text() })).await?;
                        sink.send(message(&ServerMsg::State { state: &doc.state_view() })).await?;
                    }
                    Ok(ClientMsg::View { proposed }) => {
                        proposed_view = proposed;
                        let deck = if proposed { doc.proposed().unwrap_or_else(|| doc.deck()) } else { doc.deck() };
                        sink.send(message(&patch(&prev, &deck, !editor))).await?;
                        prev = deck;
                    }
                    Ok(ClientMsg::Edit { changes, hash }) => {
                        if !doc.edit(&changes, hash, id) {
                            sink.send(message(&ServerMsg::Text { text: &doc.text() })).await?;
                        }
                    }
                    Err(_) => {}
                }
            }
            update = rx.recv() => match update {
                Ok(update) => match &*update {
                    Update::Changed { deck, proposed, change, origin } => {
                        if editor && *origin != id {
                            sink.send(message(&match change {
                                Change::Delta(changes) => ServerMsg::Edit { changes },
                                Change::Text(text) => ServerMsg::Text { text },
                            })).await?;
                        }
                        let shown = if proposed_view { proposed.as_ref().unwrap_or(deck) } else { deck };
                        sink.send(message(&patch(&prev, shown, !editor))).await?;
                        prev = shown.clone();
                    }
                    Update::Saved(error) => sink.send(message(&ServerMsg::Saved { error })).await?,
                    Update::Proposal { deck, view } => {
                        if editor {
                            sink.send(message(&ServerMsg::State { state: view })).await?;
                        }
                        if proposed_view {
                            let shown = deck.clone().unwrap_or_else(|| doc.deck());
                            sink.send(message(&patch(&prev, &shown, !editor))).await?;
                            prev = shown;
                        }
                    }
                    Update::Agent(event) => {
                        if editor {
                            sink.send(message(&ServerMsg::Agent { event })).await?;
                        }
                    }
                },
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Missed updates: resend the current state wholesale.
                    let deck = doc.deck();
                    if editor {
                        sink.send(message(&ServerMsg::Text { text: &doc.text() })).await?;
                    }
                    sink.send(message(&patch(&prev, &deck, !editor))).await?;
                    prev = deck;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_offsets() {
        let s = "a😀b"; // the emoji is one char, two UTF-16 units, four bytes
        assert_eq!(byte_at(s, 0), 0);
        assert_eq!(byte_at(s, 1), 1);
        assert_eq!(byte_at(s, 3), 5);
        assert_eq!(byte_at(s, 9), s.len());
    }

    #[test]
    fn fnv_matches_reference_values() {
        assert_eq!(fnv1a(""), 0x811c9dc5);
        assert_eq!(fnv1a("a"), 0xe40c292c);
        assert_eq!(fnv1a("foobar"), 0xbf9cf968);
    }

    #[tokio::test]
    async fn edits_apply_in_batch_order_and_verify() {
        let dir = std::env::temp_dir().join(format!("slides-test-{}", std::process::id()));
        fs::create_dir_all(dir.join("themes/t")).unwrap();
        fs::write(dir.join("themes/t/theme.toml"), "[reveal]\n").unwrap();
        let doc = Doc::create(&dir, "d", "hello world").unwrap();
        // Monaco reports a batch with later changes first, each against the original text.
        let changes = vec![
            Edit {
                offset: 6,
                length: 5,
                text: "there".into(),
            },
            Edit {
                offset: 0,
                length: 5,
                text: "hi".into(),
            },
        ];
        assert!(!doc.edit(&changes, 0, 1), "wrong hash is refused");
        assert_eq!(doc.text(), "hello world");
        assert!(doc.edit(&changes, fnv1a("hi there"), 1));
        assert_eq!(doc.text(), "hi there");
        fs::remove_dir_all(dir).unwrap();
    }
}
