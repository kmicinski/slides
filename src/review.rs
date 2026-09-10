//! Review mode: tool edits become *proposals* the author accepts or rejects
//! slide by slide, instead of landing in the deck.
//!
//! A proposal is a list of per-slide operations grouped into *changesets*: one
//! per batch of related edits (an Ask turn, or whatever a remote session put
//! between `open_changeset` calls). The author can accept or reject a
//! changeset in one go or step through its ops. A changeset is *open* while
//! its tool is still adding to it; it seals when the author acts on it, when
//! the tool opens the next one, or when the Ask run that made it ends.
//!
//! Each op remembers the slide
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
    #[serde(default)]
    pub changesets: Vec<Changeset>,
    #[serde(default)]
    pub next_changeset: u32,
}

impl Proposal {
    fn new() -> Self {
        Proposal {
            id: format!("{:x}", now()),
            created: now(),
            ops: Vec::new(),
            next_op: 1,
            changesets: Vec::new(),
            next_changeset: 1,
        }
    }

    /// Nothing left for the author to do: every op resolved, no changeset open.
    fn done(&self) -> bool {
        self.ops.iter().all(|o| o.status != Status::Pending)
            && self.changesets.iter().all(|c| c.sealed)
    }

    /// The changeset new ops join, if a tool left one open.
    fn open(&self) -> Option<u32> {
        self.changesets.iter().rev().find(|c| !c.sealed).map(|c| c.id)
    }

    /// Seals every open changeset; an open one nothing was written into is dropped.
    fn seal_all(&mut self) {
        for c in &mut self.changesets {
            c.sealed = true;
        }
        self.prune();
    }

    /// Seals one changeset (the author touched it, or the tool moved on).
    fn seal(&mut self, id: u32) {
        if let Some(c) = self.changesets.iter_mut().find(|c| c.id == id) {
            c.sealed = true;
        }
        self.prune();
    }

    /// Drops sealed changesets with no ops left.
    fn prune(&mut self) {
        let ops = &self.ops;
        self.changesets
            .retain(|c| !c.sealed || ops.iter().any(|o| o.changeset == c.id));
    }

    fn add_changeset(&mut self, title: String, note: String) -> u32 {
        self.seal_all();
        let id = self.next_changeset;
        self.next_changeset += 1;
        self.changesets.push(Changeset {
            id,
            title,
            note,
            created: now(),
            sealed: false,
        });
        id
    }

    /// Files older than changesets: every op is in changeset 0, which does not
    /// exist. Give such ops a sealed changeset so the view has a group for them.
    fn migrate(&mut self) {
        let orphans: Vec<u32> = self
            .ops
            .iter()
            .map(|o| o.changeset)
            .filter(|id| !self.changesets.iter().any(|c| c.id == *id))
            .collect();
        for id in orphans {
            if !self.changesets.iter().any(|c| c.id == id) {
                self.changesets.push(Changeset {
                    id,
                    title: String::new(),
                    note: String::new(),
                    created: self.created,
                    sealed: true,
                });
            }
        }
        let max = self.changesets.iter().map(|c| c.id).max().unwrap_or(0);
        self.next_changeset = self.next_changeset.max(max + 1);
    }
}

/// A batch of ops reviewed together.
#[derive(Serialize, Deserialize, Clone)]
pub struct Changeset {
    pub id: u32,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub note: String,
    pub created: u64,
    /// Sealed: no tool adds to it any more (see the module doc).
    #[serde(default)]
    pub sealed: bool,
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
    /// The changeset this op belongs to.
    #[serde(default)]
    pub changeset: u32,
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

/// `locate`, but following the proposal's own rewrites of the anchored slide:
/// an op made against slide N still finds N after another op (not `skip`,
/// the op itself) replaced it — in the staged fork, or once that replace was
/// accepted. So "rewrite 2, then add slides after 2" holds together as a batch.
fn locate_through(text: &str, anchor: &Anchor, prior: &[Op], skip: u32) -> Option<usize> {
    let mut a = *anchor;
    for _ in 0..8 {
        if let Some(pos) = locate(text, &a) {
            return Some(pos);
        }
        let rewrite = prior.iter().find(|o| {
            o.id != skip
                && o.kind == Kind::Replace
                && o.status != Status::Rejected
                && o.anchor.map(|x| x.hash) == Some(a.hash)
        })?;
        a.hash = fnv1a(rewrite.source.trim_matches('\n'));
    }
    None
}

/// Where an insert lands. Several inserts "after slide N" chain in proposal
/// order — each goes after the previous one's slide, if that slide is in
/// `text` (pending, in the staged text; accepted, in the real one) — so a tool
/// can add a run of slides by inserting them all after the same anchor.
fn insert_after(text: &str, op: &Op, prior: &[Op]) -> Result<usize, String> {
    let Some(anchor) = &op.anchor else {
        return Ok(0);
    };
    let base = locate_through(text, anchor, prior, op.id).ok_or("the slide it follows was edited")?;
    let chained = prior
        .iter()
        .filter(|o| {
            o.id != op.id
                && o.kind == Kind::Insert
                && o.status != Status::Rejected
                && o.anchor.map(|a| a.hash) == Some(anchor.hash)
        })
        .filter_map(|o| {
            let h = fnv1a(o.source.trim_matches('\n'));
            sources(text)
                .iter()
                .position(|s| fnv1a(s) == h)
                .map(|i| i + 1)
        })
        .filter(|&pos| pos > base)
        .max();
    Ok(chained.unwrap_or(base))
}

/// Applies one op to `text`. `Err` when its anchor no longer matches (stale).
/// `prior` is the proposal's op list (for insert chaining, see `insert_after`).
pub fn apply(text: &str, op: &Op, prior: &[Op]) -> Result<String, String> {
    match op.kind {
        Kind::Deck => {
            let hash = op.anchor.map(|a| a.hash).unwrap_or(0);
            if fnv1a(text) != hash {
                return Err("the deck changed since this was proposed".into());
            }
            Ok(op.source.clone())
        }
        Kind::Insert => {
            let after = insert_after(text, op, prior)?;
            insert_slide(text, after, &op.source, op.vertical)
        }
        Kind::Replace | Kind::Delete => {
            let a = op.anchor.as_ref().ok_or("op has no anchor")?;
            let slide = locate_through(text, a, prior, op.id)
                .ok_or("the slide was edited since this was proposed")?;
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
        if let Ok(next) = apply(&t, op, ops) {
            t = next;
        }
    }
    t
}

/// Where a pending op's result sits in the proposed deck (1-based), to point
/// the preview and thumbnails at it. A delete points at the slide that now
/// occupies its place.
pub fn proposed_position(op: &Op, ops: &[Op], text: &str, proposed: &str, stale: bool) -> Option<usize> {
    if op.status != Status::Pending || stale {
        return None;
    }
    let prop_srcs = sources(proposed);
    match op.kind {
        Kind::Replace | Kind::Insert => {
            let h = fnv1a(op.source.trim_matches('\n'));
            prop_srcs.iter().position(|s| fnv1a(s) == h).map(|i| i + 1)
        }
        Kind::Delete => {
            let at = locate_through(text, op.anchor.as_ref()?, ops, op.id)?;
            Some(at.min(prop_srcs.len()).max(1))
        }
        Kind::Deck => Some(1),
    }
}

/// Stamps the proposed deck's added/changed slides with `data-proposed`, which
/// the player shows as a badge (see `player.html`).
pub fn mark_proposed(deck: &mut crate::deck::Deck, text: &str, proposed: &str, ops: &[Op]) {
    let prop = regions(proposed);
    for op in ops {
        let stale = op.status == Status::Pending && apply(text, op, ops).is_err();
        let Some(pos) = proposed_position(op, ops, text, proposed, stale) else {
            continue;
        };
        let Some(r) = prop.get(pos - 1) else { continue };
        let label = match op.kind {
            Kind::Insert => "new",
            Kind::Replace => "changed",
            _ => continue,
        };
        if let Some(slide) = deck
            .columns
            .get_mut(r.column - 1)
            .and_then(|c| c.get_mut(r.row - 1))
        {
            slide.attrs = format!("{} data-proposed=\"{label}\"", slide.attrs)
                .trim()
                .to_string();
        }
    }
}

/// What the editor and the agent see: each op with where it lands now.
pub fn view(text: &str, state: &DeckState, proposed: &str) -> Value {
    let cur = regions(text);
    let prop = regions(proposed);
    let all = state
        .proposal
        .as_ref()
        .map(|p| p.ops.as_slice())
        .unwrap_or_default();
    let ops: Vec<Value> = all
        .iter()
        .map(|op| {
            let slide = match (&op.kind, &op.anchor) {
                (Kind::Deck, _) | (_, None) => None,
                (_, Some(a)) => locate_through(text, a, all, op.id).or_else(|| {
                    // Resolved ops keep the position they were made at, for the record.
                    (op.status != Status::Pending).then_some(a.slide)
                }),
            };
            // Stale = accepting it now would fail (the same check `accept` makes).
            let stale = op.status == Status::Pending && apply(text, op, all).is_err();
            let current = slide
                .and_then(|s| cur.get(s - 1))
                .map(|r| text[r.start..r.end].trim_matches('\n'));
            let proposed_slide = proposed_position(op, all, text, proposed, stale);
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
                "changeset": op.changeset,
            })
        })
        .collect();
    let pending = ops.iter().filter(|o| o["status"] == "pending").count();
    let changesets: Vec<Value> = state
        .proposal
        .as_ref()
        .map(|p| p.changesets.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|c| {
            let mine = || ops.iter().filter(|o| o["changeset"] == c.id);
            let count = |f: &dyn Fn(&Value) -> bool| mine().filter(|o| f(o)).count();
            json!({
                "id": c.id,
                "title": c.title,
                "note": c.note,
                "created": c.created,
                "open": !c.sealed,
                "ops": count(&|_| true),
                "pending": count(&|o| o["status"] == "pending" && o["stale"] == false),
                "stale": count(&|o| o["stale"] == true),
                "accepted": count(&|o| o["status"] == "accepted"),
                "rejected": count(&|o| o["status"] == "rejected"),
            })
        })
        .collect();
    json!({
        "review": state.review,
        "proposal": state.proposal.as_ref().map(|p| json!({ "id": p.id, "created": p.created })),
        "changesets": changesets,
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
    let mut state: DeckState = fs::read_to_string(dir.join(".slides.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if let Some(p) = state.proposal.as_mut() {
        p.migrate();
    }
    state
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

/// What the author can do to a whole changeset.
pub enum BatchAction {
    /// Accept every pending op, in order; stale ones are skipped and reported.
    Accept,
    Reject,
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
            changeset: 0,
        };
        apply(&staged, &probe, g.ops()).or_else(|_| apply(&g.text, &probe, g.ops()))?;
        let p = Self::proposal_mut(&mut g);
        // Writes join the open changeset; without one, each fresh batch gets an untitled one.
        let cs = match p.open() {
            Some(id) => id,
            None => p.add_changeset(String::new(), String::new()),
        };
        // A pending op on the same slide is *replaced* by a new one — except
        // inserts, which may legitimately stack after the same slide: those
        // only replace an earlier insert that starts with the same line.
        let first_line = |src: &str| {
            src.lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let new_head = first_line(&source);
        let same = |o: &Op| {
            o.status == Status::Pending
                && o.kind == kind
                && o.anchor.map(|a| a.hash) == anchor.map(|a| a.hash)
                && (kind != Kind::Insert || first_line(&o.source) == new_head)
        };
        let id = match p.ops.iter().position(same) {
            Some(i) => {
                let o = &mut p.ops[i];
                o.source = source;
                o.note = note;
                o.vertical = vertical;
                o.comment.clear();
                // The latest batch owns it now (its old changeset may empty out).
                o.changeset = cs;
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
                    changeset: cs,
                });
                id
            }
        };
        p.prune();
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
                let text = apply(&g.text, &op, g.ops())?;
                g.text = text;
                let change = crate::live::Change::Text(g.text.as_str().into());
                self.commit(&mut g, change, 0);
                g.state.proposal.as_mut().unwrap().ops[i].status = Status::Accepted;
            }
            Action::Reject => p.ops[i].status = Status::Rejected,
            Action::Comment(c) => p.ops[i].comment = c,
            Action::Withdraw => {
                let cs = p.ops.remove(i).changeset;
                p.seal(cs);
                self.after_review_change(&mut g);
                return Ok(());
            }
        }
        // The author is on this batch now: it stops taking new ops.
        let p = g.state.proposal.as_mut().unwrap();
        let cs = p.ops[i].changeset;
        p.seal(cs);
        self.after_review_change(&mut g);
        Ok(())
    }

    /// Opens a changeset for the writes that follow (sealing any open one).
    pub fn open_changeset(&self, title: &str, note: &str) -> u32 {
        let mut g = self.lock();
        let id = Self::proposal_mut(&mut g).add_changeset(title.trim().into(), note.trim().into());
        self.after_review_change(&mut g);
        id
    }

    /// Seals every open changeset: the tool that was writing has finished.
    pub fn seal_changesets(&self) {
        let mut g = self.lock();
        if let Some(p) = g.state.proposal.as_mut() {
            if p.open().is_none() {
                return;
            }
            p.seal_all();
        }
        self.after_review_change(&mut g);
    }

    /// Accepts, rejects or withdraws every pending op of one changeset.
    /// Accepting applies them in order and commits once; ops that went stale
    /// (or that the earlier ones made unmergeable) stay pending and are
    /// reported. Returns `{accepted|rejected|withdrawn: [ids], skipped: [{op, why}]}`.
    pub fn resolve_changeset(&self, id: u32, action: BatchAction) -> Result<Value, String> {
        let mut g = self.lock();
        let mine: Vec<usize> = {
            let p = g.state.proposal.as_ref().ok_or("no open proposal")?;
            if !p.changesets.iter().any(|c| c.id == id) {
                return Err("no such changeset".into());
            }
            (0..p.ops.len())
                .filter(|&i| p.ops[i].changeset == id && p.ops[i].status == Status::Pending)
                .collect()
        };
        let mut done = Vec::new();
        let mut skipped = Vec::new();
        let result = match action {
            BatchAction::Accept => {
                let mut text = g.text.clone();
                let p = g.state.proposal.as_mut().unwrap();
                for i in mine {
                    let op = p.ops[i].clone();
                    match apply(&text, &op, &p.ops) {
                        Ok(next) => {
                            text = next;
                            p.ops[i].status = Status::Accepted;
                            done.push(op.id);
                        }
                        Err(why) => skipped.push(json!({ "op": op.id, "why": why })),
                    }
                }
                if !done.is_empty() {
                    g.text = text;
                    let change = crate::live::Change::Text(g.text.as_str().into());
                    self.commit(&mut g, change, 0);
                }
                json!({ "accepted": done, "skipped": skipped })
            }
            BatchAction::Reject => {
                let p = g.state.proposal.as_mut().unwrap();
                for i in mine {
                    p.ops[i].status = Status::Rejected;
                    done.push(p.ops[i].id);
                }
                json!({ "rejected": done, "skipped": skipped })
            }
            BatchAction::Withdraw => {
                let p = g.state.proposal.as_mut().unwrap();
                for i in mine.iter().rev() {
                    done.push(p.ops.remove(*i).id);
                }
                done.reverse();
                json!({ "withdrawn": done, "skipped": skipped })
            }
        };
        g.state.proposal.as_mut().unwrap().seal(id);
        self.after_review_change(&mut g);
        Ok(result)
    }

    /// Drops resolved ops (and the proposal when nothing is left).
    pub fn clear_resolved(&self) {
        let mut g = self.lock();
        if let Some(p) = g.state.proposal.as_mut() {
            p.ops.retain(|o| o.status == Status::Pending);
            p.prune();
            if p.ops.is_empty() && p.changesets.is_empty() {
                g.state.proposal = None;
            }
        }
        self.after_review_change(&mut g);
    }

    /// The proposal to add to: the current one, or a fresh one once the
    /// author has resolved everything in it (its history is dropped then).
    fn proposal_mut(g: &mut Inner) -> &mut Proposal {
        if g.state.proposal.as_ref().is_none_or(|p| p.done()) {
            g.state.proposal = Some(Proposal::new());
        }
        g.state.proposal.as_mut().unwrap()
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
            let mut deck = Renderer::default().render(&text);
            mark_proposed(&mut deck, &g.text, &text, g.ops());
            Arc::new(deck)
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
            changeset: 1,
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
        assert!(apply(DECK, &stale, &[]).is_err());
        assert!(apply(DECK, &delete, &[]).is_ok());
    }

    #[test]
    fn inserts_after_the_same_slide_chain_in_order() {
        let a = Op {
            id: 1,
            ..op(Kind::Insert, anchor_for(DECK, 1), "# X")
        };
        let b = Op {
            id: 2,
            ..op(Kind::Insert, anchor_for(DECK, 1), "# Y")
        };
        let ops = [a.clone(), b.clone()];
        assert_eq!(
            sources(&proposed_text(DECK, &ops)),
            vec!["# A", "# X", "# Y", "# B", "# C"]
        );
        // Accepting one at a time keeps the order too: after X is in, Y goes after X.
        let after_a = apply(DECK, &a, &ops).unwrap();
        let mut accepted = ops.clone();
        accepted[0].status = Status::Accepted;
        let t = apply(&after_a, &b, &accepted).unwrap();
        assert_eq!(sources(&t), vec!["# A", "# X", "# Y", "# B", "# C"]);
    }

    fn temp_doc(tag: &str) -> Doc {
        let dir = std::env::temp_dir().join(format!("slides-review-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("themes/t")).unwrap();
        fs::write(dir.join("themes/t/theme.toml"), "[reveal]\n").unwrap();
        Doc::create(&dir, "d", DECK).unwrap()
    }

    #[tokio::test]
    async fn writes_join_the_open_changeset_and_seal_on_review() {
        let doc = temp_doc("group");
        // No changeset open: the first write opens an untitled one, the next joins it.
        doc.propose(Kind::Replace, Some(1), false, "# A1".into(), "".into()).unwrap();
        doc.propose(Kind::Replace, Some(2), false, "# B1".into(), "".into()).unwrap();
        let v = doc.state_view();
        assert_eq!(v["changesets"].as_array().unwrap().len(), 1);
        assert_eq!(v["changesets"][0]["title"], "");
        assert_eq!(v["changesets"][0]["open"], true);
        assert_eq!(v["changesets"][0]["pending"], 2);
        // A named changeset seals the untitled one and takes the writes that follow.
        let cs = doc.open_changeset("Add a summary", "one slide at the end");
        doc.propose(Kind::Insert, Some(3), false, "# D".into(), "".into()).unwrap();
        let v = doc.state_view();
        assert_eq!(v["changesets"].as_array().unwrap().len(), 2);
        assert_eq!(v["changesets"][0]["open"], false);
        assert_eq!(v["changesets"][1]["id"], cs);
        assert_eq!(v["changesets"][1]["title"], "Add a summary");
        assert_eq!(v["ops"][2]["changeset"], cs);
        // The author acting on a changeset seals it; the next write starts a new one.
        doc.seal_changesets();
        assert_eq!(doc.state_view()["changesets"][1]["open"], false);
        doc.resolve(1, Action::Reject).unwrap();
        doc.propose(Kind::Delete, Some(3), false, String::new(), "".into()).unwrap();
        let v = doc.state_view();
        assert_eq!(v["changesets"].as_array().unwrap().len(), 3);
        assert_eq!(v["changesets"][2]["open"], true);
        // Re-proposing a pending slide moves it into the current changeset.
        doc.propose(Kind::Replace, Some(2), false, "# B2".into(), "".into()).unwrap();
        let v = doc.state_view();
        let b = v["ops"].as_array().unwrap().iter().find(|o| o["source"] == "# B2").unwrap();
        assert_eq!(b["id"], 2, "replaced in place, same op id");
        assert_eq!(b["changeset"], v["changesets"][2]["id"]);
        // An open changeset nobody wrote into disappears when sealed.
        doc.open_changeset("nothing", "");
        assert_eq!(doc.state_view()["changesets"].as_array().unwrap().len(), 4);
        doc.seal_changesets();
        assert_eq!(doc.state_view()["changesets"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn accept_all_applies_in_order_and_reports_stale_ops() {
        let doc = temp_doc("batch");
        let cs = doc.open_changeset("Batch", "");
        doc.propose(Kind::Replace, Some(1), false, "# A1".into(), "".into()).unwrap();
        doc.propose(Kind::Insert, Some(3), false, "# D".into(), "".into()).unwrap();
        doc.propose(Kind::Insert, Some(3), false, "# E".into(), "".into()).unwrap();
        doc.propose(Kind::Delete, Some(2), false, String::new(), "".into()).unwrap();
        // The author edits slide 2 under the delete: that op goes stale.
        doc.replace(DECK.replace("# B", "# B edited"), 0);
        let r = doc.resolve_changeset(cs, BatchAction::Accept).unwrap();
        assert_eq!(r["accepted"], json!([1, 2, 3]));
        assert_eq!(r["skipped"][0]["op"], 4);
        assert_eq!(
            sources(&doc.text()),
            vec!["# A1", "# B edited", "# C", "# D", "# E"]
        );
        let v = doc.state_view();
        assert_eq!(v["changesets"][0]["accepted"], 3);
        assert_eq!(v["changesets"][0]["stale"], 1);
        assert_eq!(v["changesets"][0]["open"], false);
        // Reject all clears the stale one too.
        doc.resolve_changeset(cs, BatchAction::Reject).unwrap();
        assert_eq!(doc.state_view()["pending"], 0);
        // Everything resolved and nothing open: the next write starts a fresh proposal.
        doc.propose(Kind::Replace, Some(1), false, "# A2".into(), "".into()).unwrap();
        let v = doc.state_view();
        assert_eq!(v["ops"].as_array().unwrap().len(), 1);
        assert_eq!(v["changesets"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn inserts_follow_a_rewrite_of_their_anchor() {
        let doc = temp_doc("through");
        let cs = doc.open_changeset("Rewrite B and expand it", "");
        doc.propose(Kind::Replace, Some(2), false, "# B1".into(), "".into()).unwrap();
        doc.propose(Kind::Insert, Some(2), false, "# B-more".into(), "".into()).unwrap();
        doc.propose(Kind::Insert, Some(2), true, "# B-detail".into(), "".into()).unwrap();
        let v = doc.state_view();
        assert!(v["ops"].as_array().unwrap().iter().all(|o| o["stale"] == false));
        assert_eq!(v["ops"][1]["slide"], 2);
        assert_eq!(v["ops"][1]["proposed_slide"], 3, "staged fork has the insert after the rewrite");
        assert_eq!(v["ops"][2]["proposed_slide"], 4);
        let r = doc.resolve_changeset(cs, BatchAction::Accept).unwrap();
        assert_eq!(r["accepted"], json!([1, 2, 3]));
        assert_eq!(r["skipped"].as_array().unwrap().len(), 0);
        assert_eq!(
            sources(&doc.text()),
            vec!["# A", "# B1", "# B-more", "# B-detail", "# C"]
        );
        // One at a time works too: the accepted rewrite still resolves the anchor.
        let doc = temp_doc("through2");
        doc.propose(Kind::Replace, Some(2), false, "# B1".into(), "".into()).unwrap();
        doc.propose(Kind::Insert, Some(2), false, "# B-more".into(), "".into()).unwrap();
        doc.resolve(1, Action::Accept).unwrap();
        assert_eq!(doc.state_view()["ops"][1]["stale"], false);
        doc.resolve(2, Action::Accept).unwrap();
        assert_eq!(sources(&doc.text()), vec!["# A", "# B1", "# B-more", "# C"]);
    }

    #[test]
    fn old_state_files_get_a_changeset() {
        let old = r##"{"review":true,"proposal":{"id":"p","created":5,"next_op":2,
            "ops":[{"id":1,"kind":"replace","anchor":{"slide":1,"hash":1},"source":"# X"}]}}"##;
        let mut state: DeckState = serde_json::from_str(old).unwrap();
        state.proposal.as_mut().unwrap().migrate();
        let p = state.proposal.as_ref().unwrap();
        assert_eq!(p.ops[0].changeset, 0);
        assert_eq!(p.changesets.len(), 1);
        assert_eq!(p.changesets[0].id, 0);
        assert!(p.changesets[0].sealed);
        assert_eq!(p.next_changeset, 1);
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
            changesets: vec![Changeset {
                id: 1,
                title: "batch".into(),
                note: String::new(),
                created: 0,
                sealed: true,
            }],
            next_changeset: 2,
        });
        let proposed = proposed_text(DECK, &state.proposal.as_ref().unwrap().ops);
        let v = view(DECK, &state, &proposed);
        assert_eq!(v["pending"], 2);
        assert_eq!(v["changesets"][0]["title"], "batch");
        assert_eq!(v["changesets"][0]["ops"], 2);
        assert_eq!(v["changesets"][0]["pending"], 1);
        assert_eq!(v["changesets"][0]["stale"], 1);
        assert_eq!(v["ops"][0]["changeset"], 1);
        assert_eq!(v["ops"][0]["slide"], 2);
        assert_eq!(v["ops"][0]["stale"], false);
        assert_eq!(v["ops"][0]["proposed_slide"], 2);
        assert_eq!(v["ops"][0]["current"], "# B");
        assert_eq!(v["ops"][1]["stale"], true);
        assert_eq!(v["ops"][1]["slide"], Value::Null);
    }
}
