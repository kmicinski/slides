//! Review mode: tool edits become *proposals* the author accepts or rejects
//! slide by slide, instead of landing in the deck.
//!
//! A proposal is a list of per-slide operations. Each op remembers the slide
//! it targets by an *anchor*: the slide's position when the op was made plus a
//! hash of its source. Accepting an op finds the slide by that hash — so edits
//! elsewhere in the deck don't invalidate it — and splices the change in with
//! the same code the direct tools use. If the author has since edited that
//! slide, the hash no longer matches and the op is *stale*: it can be rejected
//! or re-proposed, never merged. The proposed deck (current text with every
//! pending op applied) is rendered alongside the real one so the preview can
//! show either.
//!
//! Per-deck state — the review flag, the open proposal and the in-app agent's
//! transcript — persists in `decks/<name>/.slides.json`.

use crate::deck::Renderer;
use crate::live::{Doc, Inner, Update, fnv1a};
use crate::mcp::{delete_slide, insert_slide, regions, replace_slide};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, io};

#[derive(Serialize, Deserialize, Clone)]
pub struct DeckState {
    #[serde(default = "yes")]
    pub review: bool,
    #[serde(default)]
    pub proposal: Option<Proposal>,
    #[serde(default)]
    pub agent: AgentState,
}

/// Review mode is on until the author turns it off (a derived `Default`
/// would say `false`).
impl Default for DeckState {
    fn default() -> Self {
        DeckState {
            review: true,
            proposal: None,
            agent: AgentState::default(),
        }
    }
}

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct AgentState {
    pub session: Option<String>,
    #[serde(default)]
    pub messages: Vec<AgentMsg>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct AgentMsg {
    pub role: String,
    pub text: String,
    pub ts: u64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Proposal {
    pub id: String,
    pub created: u64,
    pub ops: Vec<Op>,
    #[serde(default)]
    pub next_op: u32,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Op {
    pub id: u32,
    pub kind: Kind,
    /// The slide this op targets (replace/delete: itself; insert: the slide it
    /// follows, `None` = insert at the top). `Deck` ops hash the whole text.
    pub anchor: Option<Anchor>,
    #[serde(default)]
    pub vertical: bool,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub status: Status,
    #[serde(default)]
    pub comment: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Replace,
    Insert,
    Delete,
    Deck,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Pending,
    Accepted,
    Rejected,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct Anchor {
    pub slide: usize,
    pub hash: u32,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Pure logic

/// The trimmed source of every slide, in presentation order.
pub fn sources(text: &str) -> Vec<&str> {
    regions(text)
        .iter()
        .map(|r| text[r.start..r.end].trim_matches('\n'))
        .collect()
}

pub fn anchor_for(text: &str, slide: usize) -> Option<Anchor> {
    let src = *sources(text).get(slide.checked_sub(1)?)?;
    Some(Anchor {
        slide,
        hash: fnv1a(src),
    })
}

/// Current 1-based position of the anchored slide: its original position if
/// the source still matches there, else the first slide with that source.
pub fn locate(text: &str, anchor: &Anchor) -> Option<usize> {
    let srcs = sources(text);
    let at = |i: usize| srcs.get(i).is_some_and(|s| fnv1a(s) == anchor.hash);
    if anchor.slide >= 1 && at(anchor.slide - 1) {
        return Some(anchor.slide);
    }
    (0..srcs.len()).find(|&i| at(i)).map(|i| i + 1)
}

/// Applies one op to `text`. `Err` when its anchor no longer matches (stale).
pub fn apply(text: &str, op: &Op) -> Result<String, String> {
    match op.kind {
        Kind::Deck => {
            let hash = op.anchor.map(|a| a.hash).unwrap_or(0);
            if fnv1a(text) != hash {
                return Err("the deck changed since this was proposed".into());
            }
            Ok(op.source.clone())
        }
        Kind::Insert => {
            let after = match &op.anchor {
                None => 0,
                Some(a) => locate(text, a).ok_or("the slide it follows was edited")?,
            };
            insert_slide(text, after, &op.source, op.vertical)
        }
        Kind::Replace | Kind::Delete => {
            let a = op.anchor.as_ref().ok_or("op has no anchor")?;
            let slide = locate(text, a).ok_or("the slide was edited since this was proposed")?;
            if op.kind == Kind::Replace {
                replace_slide(text, slide, &op.source)
            } else {
                delete_slide(text, slide)
            }
        }
    }
}

/// Current text with every pending, non-stale op applied, in order.
pub fn proposed_text(text: &str, ops: &[Op]) -> String {
    let mut t = text.to_string();
    for op in ops.iter().filter(|o| o.status == Status::Pending) {
        if let Ok(next) = apply(&t, op) {
            t = next;
        }
    }
    t
}

/// What the editor and the agent see: each op with where it lands now.
pub fn view(text: &str, state: &DeckState, proposed: &str) -> Value {
    let cur = regions(text);
    let prop = regions(proposed);
    let prop_srcs = sources(proposed);
    let ops: Vec<Value> = state
        .proposal
        .as_ref()
        .map(|p| p.ops.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|op| {
            let slide = match (&op.kind, &op.anchor) {
                (Kind::Deck, _) | (_, None) => None,
                (_, Some(a)) => locate(text, a).or_else(|| {
                    // Resolved ops keep the position they were made at, for the record.
                    (op.status != Status::Pending).then_some(a.slide)
                }),
            };
            let stale = op.status == Status::Pending
                && match op.kind {
                    Kind::Deck => op.anchor.map(|a| a.hash) != Some(fnv1a(text)),
                    Kind::Insert => op.anchor.is_some() && slide.is_none(),
                    _ => slide.is_none(),
                };
            let current = slide
                .and_then(|s| cur.get(s - 1))
                .map(|r| text[r.start..r.end].trim_matches('\n'));
            // Where the op's result sits in the proposed deck (to point the preview at it).
            let proposed_slide = match op.kind {
                Kind::Replace | Kind::Insert if op.status == Status::Pending && !stale => {
                    let h = fnv1a(op.source.trim_matches('\n'));
                    prop_srcs.iter().position(|s| fnv1a(s) == h).map(|i| i + 1)
                }
                Kind::Delete => slide.map(|s| s.saturating_sub(1).max(1)),
                _ => None,
            };
            let (pcol, prow) = proposed_slide
                .and_then(|s| prop.get(s - 1))
                .map(|r| (r.column, r.row))
                .unwrap_or((0, 0));
            json!({
                "id": op.id,
                "kind": op.kind,
                "slide": slide,
                "line": slide.and_then(|s| cur.get(s - 1)).map(|r| r.line),
                "vertical": op.vertical,
                "source": op.source,
                "current": current,
                "note": op.note,
                "status": op.status,
                "stale": stale,
                "comment": op.comment,
                "proposed_slide": proposed_slide,
                "proposed_col": pcol,
                "proposed_row": prow,
            })
        })
        .collect();
    let pending = ops.iter().filter(|o| o["status"] == "pending").count();
    json!({
        "review": state.review,
        "proposal": state.proposal.as_ref().map(|p| json!({ "id": p.id, "created": p.created })),
        "ops": ops,
        "pending": pending,
        "agent": {
            "session": state.agent.session,
            "messages": state.agent.messages,
        },
    })
}

// ---------------------------------------------------------------------------
// Persistence

pub fn load(dir: &Path) -> DeckState {
    fs::read_to_string(dir.join(".slides.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(dir: &Path, state: &DeckState) -> io::Result<()> {
    let tmp = dir.join(".slides.json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    fs::rename(tmp, dir.join(".slides.json"))
}

// ---------------------------------------------------------------------------
// Doc operations

pub enum Action {
    Accept,
    Reject,
    Comment(String),
    Withdraw,
}

impl Doc {
    pub fn review(&self) -> bool {
        self.lock().state.review
    }

    pub fn set_review(&self, on: bool) {
        let mut g = self.lock();
        g.state.review = on;
        self.after_review_change(&mut g);
    }

    /// The editor's and the agent's view of the proposal.
    pub fn state_view(&self) -> Value {
        let g = self.lock();
        let proposed = proposed_text(&g.text, g.ops());
        view(&g.text, &g.state, &proposed)
    }

    /// Queues an op (from a tool, in review mode). Re-proposing a pending op on
    /// the same slide replaces it. Returns the op's view.
    pub fn propose(
        &self,
        kind: Kind,
        slide: Option<usize>,
        vertical: bool,
        source: String,
        note: String,
    ) -> Result<Value, String> {
        let mut g = self.lock();
        let anchor = match kind {
            Kind::Deck => Some(Anchor {
                slide: 0,
                hash: fnv1a(&g.text),
            }),
            Kind::Insert if slide == Some(0) || slide.is_none() => None,
            _ => {
                let s = slide.ok_or("missing slide")?;
                Some(anchor_for(&g.text, s).ok_or_else(|| {
                    format!(
                        "no slide {s}: the deck has {} slide(s)",
                        sources(&g.text).len()
                    )
                })?)
            }
        };
        // Validate against the current proposed text so the agent hears about mistakes now.
        let staged = proposed_text(&g.text, g.ops());
        let probe = Op {
            id: 0,
            kind,
            anchor,
            vertical,
            source: source.clone(),
            note: String::new(),
            status: Status::Pending,
            comment: String::new(),
        };
        apply(&staged, &probe).or_else(|_| apply(&g.text, &probe))?;
        let all_resolved = g
            .state
            .proposal
            .as_ref()
            .is_some_and(|p| p.ops.iter().all(|o| o.status != Status::Pending));
        if g.state.proposal.is_none() || all_resolved {
            g.state.proposal = Some(Proposal {
                id: format!("{:x}", now()),
                created: now(),
                ops: Vec::new(),
                next_op: 1,
            });
        }
        let p = g.state.proposal.as_mut().unwrap();
        let same = |o: &Op| {
            o.status == Status::Pending
                && o.kind == kind
                && o.anchor.map(|a| a.hash) == anchor.map(|a| a.hash)
        };
        let id = match p.ops.iter().position(same) {
            Some(i) => {
                let o = &mut p.ops[i];
                o.source = source;
                o.note = note;
                o.vertical = vertical;
                o.comment.clear();
                o.id
            }
            None => {
                let id = p.next_op;
                p.next_op += 1;
                p.ops.push(Op {
                    id,
                    kind,
                    anchor,
                    vertical,
                    source,
                    note,
                    status: Status::Pending,
                    comment: String::new(),
                });
                id
            }
        };
        self.after_review_change(&mut g);
        let v = self.view_locked(&g);
        Ok(v["ops"]
            .as_array()
            .and_then(|ops| ops.iter().find(|o| o["id"] == id).cloned())
            .unwrap_or(Value::Null))
    }

    pub fn resolve(&self, op_id: u32, action: Action) -> Result<(), String> {
        let mut g = self.lock();
        let p = g.state.proposal.as_mut().ok_or("no open proposal")?;
        let i = p
            .ops
            .iter()
            .position(|o| o.id == op_id)
            .ok_or("no such op")?;
        match action {
            Action::Accept => {
                let op = p.ops[i].clone();
                if op.status != Status::Pending {
                    return Err("already resolved".into());
                }
                let text = apply(&g.text, &op)?;
                g.text = text;
                let change = crate::live::Change::Text(g.text.as_str().into());
                self.commit(&mut g, change, 0);
                g.state.proposal.as_mut().unwrap().ops[i].status = Status::Accepted;
            }
            Action::Reject => p.ops[i].status = Status::Rejected,
            Action::Comment(c) => p.ops[i].comment = c,
            Action::Withdraw => {
                p.ops.remove(i);
            }
        }
        self.after_review_change(&mut g);
        Ok(())
    }

    /// Drops resolved ops (and the proposal when nothing is left).
    pub fn clear_resolved(&self) {
        let mut g = self.lock();
        if let Some(p) = g.state.proposal.as_mut() {
            p.ops.retain(|o| o.status == Status::Pending);
            if p.ops.is_empty() {
                g.state.proposal = None;
            }
        }
        self.after_review_change(&mut g);
    }

    /// Waits until the proposal changes (accept/reject/comment/…), or `timeout`.
    pub async fn await_review(&self, timeout: Duration) -> Value {
        let mut rx = self.lock().rev.subscribe();
        let _ = tokio::time::timeout(timeout, rx.changed()).await;
        self.state_view()
    }

    // -- agent transcript --

    pub fn agent_push(&self, role: &str, text: &str) {
        let mut g = self.lock();
        g.state.agent.messages.push(AgentMsg {
            role: role.into(),
            text: text.into(),
            ts: now(),
        });
        if g.state.agent.messages.len() > 400 {
            let n = g.state.agent.messages.len() - 400;
            g.state.agent.messages.drain(..n);
        }
        let _ = save(&g.dir(), &g.state);
    }

    pub fn agent_session(&self) -> Option<String> {
        self.lock().state.agent.session.clone()
    }

    pub fn set_agent_session(&self, session: Option<String>) {
        let mut g = self.lock();
        g.state.agent.session = session;
        let _ = save(&g.dir(), &g.state);
    }

    pub fn agent_reset(&self) {
        let mut g = self.lock();
        g.state.agent = AgentState::default();
        let _ = save(&g.dir(), &g.state);
        self.after_review_change(&mut g);
    }

    // -- internals --

    fn view_locked(&self, g: &Inner) -> Value {
        let proposed = proposed_text(&g.text, g.ops());
        view(&g.text, &g.state, &proposed)
    }

    /// Persists, re-renders the proposed deck and tells every connection.
    pub(crate) fn after_review_change(&self, g: &mut Inner) {
        if let Err(e) = save(&g.dir(), &g.state) {
            eprintln!("deck {}: saving .slides.json: {e}", g.name);
        }
        self.recompute_proposed(g);
        g.rev.send_modify(|v| *v += 1);
        let view = Arc::new(self.view_locked(g));
        let _ = g.tx.send(Arc::new(Update::Proposal {
            deck: g.proposed.clone(),
            view,
        }));
    }

    /// The proposed deck exists only while review mode has pending ops.
    pub(crate) fn recompute_proposed(&self, g: &mut Inner) {
        let pending = g.state.review && g.ops().iter().any(|o| o.status == Status::Pending);
        g.proposed = pending.then(|| {
            let text = proposed_text(&g.text, g.ops());
            Arc::new(Renderer::default().render(&text))
        });
    }
}

impl Inner {
    pub(crate) fn ops(&self) -> &[Op] {
        self.state
            .proposal
            .as_ref()
            .map(|p| p.ops.as_slice())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DECK: &str = "# A\n\n---\n\n# B\n\n---\n\n# C\n";

    fn op(kind: Kind, anchor: Option<Anchor>, source: &str) -> Op {
        Op {
            id: 1,
            kind,
            anchor,
            vertical: false,
            source: source.into(),
            note: String::new(),
            status: Status::Pending,
            comment: String::new(),
        }
    }

    #[test]
    fn review_is_on_by_default() {
        assert!(DeckState::default().review);
        let parsed: DeckState = serde_json::from_str("{}").unwrap();
        assert!(parsed.review);
    }

    #[test]
    fn anchors_follow_slides_around() {
        let a = anchor_for(DECK, 2).unwrap();
        assert_eq!(locate(DECK, &a), Some(2));
        let moved = insert_slide(DECK, 0, "# Z", false).unwrap();
        assert_eq!(
            locate(&moved, &a),
            Some(3),
            "found by hash after an insert above"
        );
        let edited = replace_slide(DECK, 2, "# B edited").unwrap();
        assert_eq!(
            locate(&edited, &a),
            None,
            "stale once the slide itself changed"
        );
    }

    #[test]
    fn apply_and_stage_in_order() {
        let replace = op(Kind::Replace, anchor_for(DECK, 2), "# B2");
        let insert = Op {
            id: 2,
            ..op(Kind::Insert, anchor_for(DECK, 3), "# D")
        };
        let delete = Op {
            id: 3,
            ..op(Kind::Delete, anchor_for(DECK, 1), "")
        };
        let t = proposed_text(DECK, &[replace.clone(), insert.clone(), delete.clone()]);
        assert_eq!(sources(&t), vec!["# B2", "# C", "# D"]);
        // A stale op is skipped, the rest still apply.
        let stale = op(Kind::Replace, Some(Anchor { slide: 1, hash: 1 }), "# nope");
        let t = proposed_text(DECK, &[stale.clone(), insert]);
        assert_eq!(sources(&t), vec!["# A", "# B", "# C", "# D"]);
        assert!(apply(DECK, &stale).is_err());
        assert!(apply(DECK, &delete).is_ok());
    }

    #[test]
    fn view_reports_positions_and_staleness() {
        let mut state = DeckState::default();
        state.proposal = Some(Proposal {
            id: "p".into(),
            created: 0,
            ops: vec![
                op(Kind::Replace, anchor_for(DECK, 2), "# B2"),
                Op {
                    id: 2,
                    ..op(Kind::Replace, Some(Anchor { slide: 3, hash: 7 }), "# C2")
                },
            ],
            next_op: 3,
        });
        let proposed = proposed_text(DECK, &state.proposal.as_ref().unwrap().ops);
        let v = view(DECK, &state, &proposed);
        assert_eq!(v["pending"], 2);
        assert_eq!(v["ops"][0]["slide"], 2);
        assert_eq!(v["ops"][0]["stale"], false);
        assert_eq!(v["ops"][0]["proposed_slide"], 2);
        assert_eq!(v["ops"][0]["current"], "# B");
        assert_eq!(v["ops"][1]["stale"], true);
        assert_eq!(v["ops"][1]["slide"], Value::Null);
    }
}
