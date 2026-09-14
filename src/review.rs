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
//! Several tools may write at once — the in-app agent runs one *thread* per
//! question, in parallel (`agent.rs`), and remote MCP sessions come and go.
//! Each thread's writes go to its own changesets (the MCP request carries the
//! thread), and a thread re-proposing a slide replaces its own earlier op for
//! it. Ops from *different* changesets that touch the same slide (replace or
//! delete it, insert after a slide the other deletes, or rewrite the whole
//! deck) are in **conflict**: neither is merged into the proposed deck or can
//! be accepted until the author keeps one, or asks the agent to merge them —
//! a merge thread whose changeset lists the ops it supersedes; its first
//! write marks them `merged`. Ops on different slides compose as usual.
//!
//! Per-deck state — the review flag, the open proposal and the in-app agent's
//! threads — persists in `decks/<name>/.slides.json`.

use crate::deck::Renderer;
use crate::live::{Doc, Inner, Update, fnv1a};
use crate::mcp::{delete_slide, insert_slide, regions, replace_slide};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
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

/// The in-app agent's conversations with this deck: one thread per question,
/// each its own `claude` session (`agent.rs`), so several can run at once.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct AgentState {
    #[serde(default)]
    pub threads: Vec<Thread>,
    #[serde(default)]
    pub next_thread: u32,
    // Before threads there was one conversation per deck; `load` moves it into thread 1.
    #[serde(default, skip_serializing)]
    session: Option<String>,
    #[serde(default, skip_serializing)]
    messages: Vec<AgentMsg>,
}

impl AgentState {
    fn migrate(&mut self) {
        if self.threads.is_empty() && (self.session.is_some() || !self.messages.is_empty()) {
            let title = self
                .messages
                .iter()
                .find(|m| m.role == "user")
                .map(|m| m.text.clone())
                .unwrap_or_default();
            let created = self.messages.first().map(|m| m.ts).unwrap_or_else(now);
            self.threads.push(Thread {
                id: 1,
                title: crate::agent::title_of(&title),
                session: self.session.take(),
                messages: std::mem::take(&mut self.messages),
                created,
                merge: false,
                running: false,
            });
        }
        let max = self.threads.iter().map(|t| t.id).max().unwrap_or(0);
        self.next_thread = self.next_thread.max(max + 1).max(1);
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Thread {
    pub id: u32,
    #[serde(default)]
    pub title: String,
    pub session: Option<String>,
    #[serde(default)]
    pub messages: Vec<AgentMsg>,
    #[serde(default)]
    pub created: u64,
    /// A merge thread: started to reconcile conflicting proposals.
    #[serde(default)]
    pub merge: bool,
    /// A `claude` run is on for this thread now (not persisted: nothing runs after a restart).
    #[serde(default, skip_serializing)]
    pub running: bool,
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

    /// The changeset a writer's new ops join: the one it left open, if any.
    /// Writers are told apart by thread (`None` = a remote MCP session).
    fn open_for(&self, thread: Option<u32>) -> Option<u32> {
        self.changesets
            .iter()
            .rev()
            .find(|c| !c.sealed && c.thread == thread)
            .map(|c| c.id)
    }

    /// Seals one writer's open changesets (it finished, or moved on to the
    /// next one); an open one nothing was written into is dropped.
    fn seal_thread(&mut self, thread: Option<u32>) {
        for c in self.changesets.iter_mut().filter(|c| c.thread == thread) {
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

    fn add_changeset(
        &mut self,
        title: String,
        note: String,
        thread: Option<u32>,
        merges: Vec<u32>,
    ) -> u32 {
        self.seal_thread(thread);
        let id = self.next_changeset;
        self.next_changeset += 1;
        self.changesets.push(Changeset {
            id,
            title,
            note,
            created: now(),
            sealed: false,
            thread,
            merges,
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
                    thread: None,
                    merges: Vec::new(),
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
    /// The agent thread writing it; `None` for a remote MCP session.
    #[serde(default)]
    pub thread: Option<u32>,
    /// A merge changeset: the conflicting ops it reconciles. Its first write
    /// marks them `merged`.
    #[serde(default)]
    pub merges: Vec<u32>,
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
    /// Superseded by a merge of conflicting proposals (see the module doc).
    Merged,
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

/// Whether two ops from different changesets cannot both go in: they
/// replace or delete the same slide, one inserts after a slide the other
/// deletes, or one rewrites the whole deck. Two inserts after the same slide
/// merely chain; a replace and an insert after it hold together (the insert
/// follows the rewrite, see `locate_through`).
fn clash(a: &Op, b: &Op) -> bool {
    if a.changeset == b.changeset {
        return false;
    }
    if a.kind == Kind::Deck || b.kind == Kind::Deck {
        return true;
    }
    let (Some(x), Some(y)) = (a.anchor, b.anchor) else {
        return false;
    };
    if x.hash != y.hash {
        return false;
    }
    match (a.kind, b.kind) {
        (Kind::Insert, Kind::Insert) => false,
        (Kind::Insert, k) | (k, Kind::Insert) => k == Kind::Delete,
        _ => true,
    }
}

/// Every pending, mergeable op that conflicts with another, with the ids of
/// the ops it conflicts with. Stale ops cannot be accepted anyway, so they
/// do not take part.
pub fn conflicts(text: &str, ops: &[Op]) -> BTreeMap<u32, Vec<u32>> {
    let live: Vec<&Op> = ops
        .iter()
        .filter(|o| o.status == Status::Pending && apply(text, o, ops).is_ok())
        .collect();
    let mut out = BTreeMap::new();
    for a in &live {
        let rivals: Vec<u32> = live
            .iter()
            .filter(|b| b.id != a.id && clash(a, b))
            .map(|b| b.id)
            .collect();
        if !rivals.is_empty() {
            out.insert(a.id, rivals);
        }
    }
    out
}

/// Current text with every pending, non-stale op applied, in order — except
/// ops in conflict, which stay out until the author resolves them.
pub fn proposed_text(text: &str, ops: &[Op]) -> String {
    let conflicted = conflicts(text, ops);
    let mut t = text.to_string();
    for op in ops
        .iter()
        .filter(|o| o.status == Status::Pending && !conflicted.contains_key(&o.id))
    {
        if let Ok(next) = apply(&t, op, ops) {
            t = next;
        }
    }
    t
}

/// The deck with just this op applied — what a conflicted op looks like on
/// its own, since the shared proposed deck leaves it out. `None` when the
/// op is not pending or would not merge.
pub fn alone_text(text: &str, ops: &[Op], op_id: u32) -> Option<String> {
    let op = ops.iter().find(|o| o.id == op_id && o.status == Status::Pending)?;
    apply(text, op, ops).ok()
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
    let conflicted = conflicts(text, ops);
    for op in ops.iter().filter(|o| !conflicted.contains_key(&o.id)) {
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
    let conflicted = conflicts(text, all);
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
            let rivals = conflicted.get(&op.id).cloned().unwrap_or_default();
            // A conflicted op is not in the shared fork: its positions refer to
            // the deck with just this op applied (`alone_text`, `view=op`).
            let alone = (!rivals.is_empty())
                .then(|| alone_text(text, all, op.id))
                .flatten();
            let fork = alone.as_deref().unwrap_or(proposed);
            let fork_regions = if alone.is_some() { regions(fork) } else { Vec::new() };
            let fork_regions = if alone.is_some() { &fork_regions } else { &prop };
            let proposed_slide = proposed_position(op, all, text, fork, stale);
            let (pcol, prow) = proposed_slide
                .and_then(|s| fork_regions.get(s - 1))
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
                "conflicts": rivals,
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
                "thread": c.thread,
                "merges": c.merges,
                "ops": count(&|_| true),
                "pending": count(&|o| o["status"] == "pending" && o["stale"] == false),
                "stale": count(&|o| o["stale"] == true),
                "conflicted": count(&|o| !o["conflicts"].as_array().is_none_or(|a| a.is_empty())),
                "accepted": count(&|o| o["status"] == "accepted"),
                "rejected": count(&|o| o["status"] == "rejected"),
                "merged": count(&|o| o["status"] == "merged"),
            })
        })
        .collect();
    let threads: Vec<Value> = state
        .agent
        .threads
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "title": t.title,
                "session": t.session,
                "messages": t.messages,
                "created": t.created,
                "merge": t.merge,
                "running": t.running,
            })
        })
        .collect();
    json!({
        "review": state.review,
        "proposal": state.proposal.as_ref().map(|p| json!({ "id": p.id, "created": p.created })),
        "changesets": changesets,
        "ops": ops,
        "pending": pending,
        "conflicts": conflicted.len(),
        "agent": { "threads": threads },
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
    state.agent.migrate();
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

    /// Queues an op (from a tool, in review mode). `thread` says which agent
    /// thread is writing (`None`: a remote session); a writer re-proposing a
    /// pending op of its own on the same slide replaces it, while another
    /// writer's op on that slide stands beside it, in conflict. Returns the
    /// op's view.
    pub fn propose(
        &self,
        kind: Kind,
        slide: Option<usize>,
        vertical: bool,
        source: String,
        note: String,
        thread: Option<u32>,
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
        // Writes join the writer's open changeset; without one, each fresh batch gets an untitled one.
        let cs = match p.open_for(thread) {
            Some(id) => id,
            None => p.add_changeset(String::new(), String::new(), thread, Vec::new()),
        };
        // The writer's own pending op on the same slide is *replaced* by the
        // new one — except inserts, which may legitimately stack after the
        // same slide: those only replace an earlier insert that starts with
        // the same line. Another writer's op on that slide is left alone: the
        // two are then in conflict for the author to resolve.
        let first_line = |src: &str| {
            src.lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let new_head = first_line(&source);
        let threads: std::collections::HashMap<u32, Option<u32>> =
            p.changesets.iter().map(|c| (c.id, c.thread)).collect();
        let same = |o: &Op| {
            o.status == Status::Pending
                && o.kind == kind
                && o.anchor.map(|a| a.hash) == anchor.map(|a| a.hash)
                && (kind != Kind::Insert || first_line(&o.source) == new_head)
                && threads.get(&o.changeset).copied().flatten() == thread
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
        // A merge changeset's first write supersedes the ops it reconciles.
        let merges: Vec<u32> = p
            .changesets
            .iter()
            .find(|c| c.id == cs)
            .map(|c| c.merges.clone())
            .unwrap_or_default();
        for o in p.ops.iter_mut() {
            if merges.contains(&o.id) && o.status == Status::Pending && o.id != id {
                o.status = Status::Merged;
            }
        }
        p.prune();
        self.after_review_change(&mut g);
        let v = self.view_locked(&g);
        Ok(v["ops"]
            .as_array()
            .and_then(|ops| ops.iter().find(|o| o["id"] == id).cloned())
            .unwrap_or(Value::Null))
    }

    /// Why an op cannot be accepted as it stands: the other pending ops it conflicts with.
    fn conflict_reason(text: &str, ops: &[Op], op_id: u32) -> Option<String> {
        let rivals = conflicts(text, ops).remove(&op_id)?;
        let list: Vec<String> = rivals.iter().map(|r| format!("#{r}")).collect();
        Some(format!(
            "conflicts with proposal {}: keep one of them or have the agent merge them first",
            list.join(", ")
        ))
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
                if let Some(why) = Self::conflict_reason(&g.text, g.ops(), op_id) {
                    return Err(why);
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

    /// Opens a changeset for a writer's writes that follow (sealing its open
    /// one). `merges` makes it a merge changeset (see the module doc).
    pub fn open_changeset(&self, title: &str, note: &str, thread: Option<u32>, merges: Vec<u32>) -> u32 {
        let mut g = self.lock();
        let id = Self::proposal_mut(&mut g).add_changeset(
            title.trim().into(),
            note.trim().into(),
            thread,
            merges,
        );
        self.after_review_change(&mut g);
        id
    }

    /// Seals a writer's open changesets: it has finished.
    pub fn seal_changesets(&self, thread: Option<u32>) {
        let mut g = self.lock();
        if let Some(p) = g.state.proposal.as_mut() {
            if p.open_for(thread).is_none() {
                return;
            }
            p.seal_thread(thread);
        }
        self.after_review_change(&mut g);
    }

    /// Settles a conflict by choice: keep `op` (rejecting every op it conflicts
    /// with) or keep one rival, `other` (rejecting `op`). Merging is the third
    /// way, and is the agent's job (`agent::start_merge`).
    pub fn resolve_conflict(&self, op_id: u32, keep_other: Option<u32>) -> Result<Value, String> {
        let mut g = self.lock();
        let rivals = conflicts(&g.text, g.ops())
            .remove(&op_id)
            .ok_or("this proposal is not in conflict")?;
        let losers: Vec<u32> = match keep_other {
            None => rivals,
            Some(other) if rivals.contains(&other) => vec![op_id],
            Some(other) => return Err(format!("proposal #{other} does not conflict with #{op_id}")),
        };
        let p = g.state.proposal.as_mut().ok_or("no open proposal")?;
        let mut touched = Vec::new();
        for o in p.ops.iter_mut().filter(|o| losers.contains(&o.id)) {
            o.status = Status::Rejected;
            touched.push(o.changeset);
        }
        for cs in touched {
            p.seal(cs);
        }
        self.after_review_change(&mut g);
        Ok(json!({ "rejected": losers, "kept": keep_other.unwrap_or(op_id) }))
    }

    /// What a merge needs to know about a conflicted op and its rivals: each
    /// op's view plus the title and request of the changeset/thread behind
    /// it, and the current source of the slide they fight over.
    pub fn conflict_group(&self, op_id: u32) -> Result<Value, String> {
        let g = self.lock();
        let ops = g.ops();
        let rivals = conflicts(&g.text, ops)
            .remove(&op_id)
            .ok_or("this proposal is not in conflict")?;
        let view = self.view_locked(&g);
        let p = g.state.proposal.as_ref().ok_or("no open proposal")?;
        let describe = |id: u32| -> Option<Value> {
            let mut v = view["ops"].as_array()?.iter().find(|o| o["id"] == id)?.clone();
            let cs = p.changesets.iter().find(|c| Some(c.id) == v["changeset"].as_u64().map(|x| x as u32));
            let thread = cs.and_then(|c| c.thread).and_then(|t| g.state.agent.threads.iter().find(|x| x.id == t));
            v["title"] = json!(cs.map(|c| c.title.as_str()).unwrap_or(""));
            v["request"] = json!(thread
                .and_then(|t| t.messages.iter().find(|m| m.role == "user"))
                .map(|m| m.text.as_str())
                .unwrap_or(""));
            Some(v)
        };
        let me = describe(op_id).ok_or("no such op")?;
        let others: Vec<Value> = rivals.iter().filter_map(|r| describe(*r)).collect();
        Ok(json!({ "op": me, "rivals": others, "all": std::iter::once(op_id).chain(rivals).collect::<Vec<_>>() }))
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
                let conflicted = conflicts(&g.text, g.ops());
                let p = g.state.proposal.as_mut().unwrap();
                for i in mine {
                    let op = p.ops[i].clone();
                    if let Some(rivals) = conflicted.get(&op.id) {
                        let list: Vec<String> = rivals.iter().map(|r| format!("#{r}")).collect();
                        skipped.push(json!({ "op": op.id, "why": format!("in conflict with proposal {}", list.join(", ")) }));
                        continue;
                    }
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

    // -- agent threads --

    /// Starts a thread (a conversation with the agent); returns its id.
    pub fn thread_new(&self, title: &str, merge: bool) -> u32 {
        let mut g = self.lock();
        let a = &mut g.state.agent;
        let id = a.next_thread.max(1);
        a.next_thread = id + 1;
        a.threads.push(Thread {
            id,
            title: title.into(),
            session: None,
            messages: Vec::new(),
            created: now(),
            merge,
            running: false,
        });
        if a.threads.len() > 40 {
            // Oldest idle threads go first; the transcript of a busy deck is not an archive.
            let mut n = a.threads.len() - 40;
            a.threads.retain(|t| {
                if n > 0 && !t.running {
                    n -= 1;
                    false
                } else {
                    true
                }
            });
        }
        self.after_review_change(&mut g);
        id
    }

    pub fn thread(&self, id: u32) -> Option<Thread> {
        self.lock().state.agent.threads.iter().find(|t| t.id == id).cloned()
    }

    pub fn thread_push(&self, id: u32, role: &str, text: &str) {
        let mut g = self.lock();
        let Some(t) = g.state.agent.threads.iter_mut().find(|t| t.id == id) else {
            return;
        };
        t.messages.push(AgentMsg {
            role: role.into(),
            text: text.into(),
            ts: now(),
        });
        if t.messages.len() > 400 {
            let n = t.messages.len() - 400;
            t.messages.drain(..n);
        }
        let _ = save(&g.dir(), &g.state);
    }

    pub fn set_thread_session(&self, id: u32, session: Option<String>) {
        let mut g = self.lock();
        if let Some(t) = g.state.agent.threads.iter_mut().find(|t| t.id == id) {
            t.session = session;
        }
        let _ = save(&g.dir(), &g.state);
    }

    /// Marks a thread running or not; editors learn of it through the state view.
    pub fn set_thread_running(&self, id: u32, running: bool) {
        let mut g = self.lock();
        if let Some(t) = g.state.agent.threads.iter_mut().find(|t| t.id == id) {
            t.running = running;
        }
        self.after_review_change(&mut g);
    }

    /// Drops one thread (its proposals stay, in their changesets).
    pub fn thread_close(&self, id: u32) {
        let mut g = self.lock();
        g.state.agent.threads.retain(|t| t.id != id);
        self.after_review_change(&mut g);
    }

    pub fn agent_reset(&self) {
        let mut g = self.lock();
        g.state.agent = AgentState::default();
        g.state.agent.migrate();
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

    /// The deck with just one pending op applied (`alone_text`) — how a
    /// conflicted op is previewed, since the shared proposed deck leaves it out.
    pub fn deck_for_op(&self, op: u32) -> Option<Arc<crate::deck::Deck>> {
        let g = self.lock();
        let text = alone_text(&g.text, g.ops(), op)?;
        Some(Arc::new(Renderer::default().render(&text)))
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
        doc.propose(Kind::Replace, Some(1), false, "# A1".into(), "".into(), None).unwrap();
        doc.propose(Kind::Replace, Some(2), false, "# B1".into(), "".into(), None).unwrap();
        let v = doc.state_view();
        assert_eq!(v["changesets"].as_array().unwrap().len(), 1);
        assert_eq!(v["changesets"][0]["title"], "");
        assert_eq!(v["changesets"][0]["open"], true);
        assert_eq!(v["changesets"][0]["pending"], 2);
        // A named changeset seals the untitled one and takes the writes that follow.
        let cs = doc.open_changeset("Add a summary", "one slide at the end", None, vec![]);
        doc.propose(Kind::Insert, Some(3), false, "# D".into(), "".into(), None).unwrap();
        let v = doc.state_view();
        assert_eq!(v["changesets"].as_array().unwrap().len(), 2);
        assert_eq!(v["changesets"][0]["open"], false);
        assert_eq!(v["changesets"][1]["id"], cs);
        assert_eq!(v["changesets"][1]["title"], "Add a summary");
        assert_eq!(v["ops"][2]["changeset"], cs);
        // The author acting on a changeset seals it; the next write starts a new one.
        doc.seal_changesets(None);
        assert_eq!(doc.state_view()["changesets"][1]["open"], false);
        doc.resolve(1, Action::Reject).unwrap();
        doc.propose(Kind::Delete, Some(3), false, String::new(), "".into(), None).unwrap();
        let v = doc.state_view();
        assert_eq!(v["changesets"].as_array().unwrap().len(), 3);
        assert_eq!(v["changesets"][2]["open"], true);
        // Re-proposing a pending slide moves it into the current changeset.
        doc.propose(Kind::Replace, Some(2), false, "# B2".into(), "".into(), None).unwrap();
        let v = doc.state_view();
        let b = v["ops"].as_array().unwrap().iter().find(|o| o["source"] == "# B2").unwrap();
        assert_eq!(b["id"], 2, "replaced in place, same op id");
        assert_eq!(b["changeset"], v["changesets"][2]["id"]);
        // An open changeset nobody wrote into disappears when sealed.
        doc.open_changeset("nothing", "", None, vec![]);
        assert_eq!(doc.state_view()["changesets"].as_array().unwrap().len(), 4);
        doc.seal_changesets(None);
        assert_eq!(doc.state_view()["changesets"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn accept_all_applies_in_order_and_reports_stale_ops() {
        let doc = temp_doc("batch");
        let cs = doc.open_changeset("Batch", "", None, vec![]);
        doc.propose(Kind::Replace, Some(1), false, "# A1".into(), "".into(), None).unwrap();
        doc.propose(Kind::Insert, Some(3), false, "# D".into(), "".into(), None).unwrap();
        doc.propose(Kind::Insert, Some(3), false, "# E".into(), "".into(), None).unwrap();
        doc.propose(Kind::Delete, Some(2), false, String::new(), "".into(), None).unwrap();
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
        doc.propose(Kind::Replace, Some(1), false, "# A2".into(), "".into(), None).unwrap();
        let v = doc.state_view();
        assert_eq!(v["ops"].as_array().unwrap().len(), 1);
        assert_eq!(v["changesets"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn inserts_follow_a_rewrite_of_their_anchor() {
        let doc = temp_doc("through");
        let cs = doc.open_changeset("Rewrite B and expand it", "", None, vec![]);
        doc.propose(Kind::Replace, Some(2), false, "# B1".into(), "".into(), None).unwrap();
        doc.propose(Kind::Insert, Some(2), false, "# B-more".into(), "".into(), None).unwrap();
        doc.propose(Kind::Insert, Some(2), true, "# B-detail".into(), "".into(), None).unwrap();
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
        doc.propose(Kind::Replace, Some(2), false, "# B1".into(), "".into(), None).unwrap();
        doc.propose(Kind::Insert, Some(2), false, "# B-more".into(), "".into(), None).unwrap();
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
                thread: None,
                merges: vec![],
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

    #[tokio::test]
    async fn parallel_writers_conflict_on_a_slide_and_compose_elsewhere() {
        let doc = temp_doc("conflict");
        // Two threads: both rewrite slide 2; only thread 1 touches slide 3.
        doc.open_changeset("Tighten B", "", Some(1), vec![]);
        doc.propose(Kind::Replace, Some(2), false, "# B from 1".into(), "".into(), Some(1)).unwrap();
        doc.propose(Kind::Replace, Some(3), false, "# C from 1".into(), "".into(), Some(1)).unwrap();
        doc.open_changeset("Expand B", "", Some(2), vec![]);
        doc.propose(Kind::Replace, Some(2), false, "# B from 2".into(), "".into(), Some(2)).unwrap();
        let v = doc.state_view();
        assert_eq!(v["ops"].as_array().unwrap().len(), 3, "the second writer's op did not replace the first's");
        assert_eq!(v["changesets"].as_array().unwrap().len(), 2);
        assert_eq!(v["changesets"][0]["open"], true, "thread 1 is still open");
        assert_eq!(v["changesets"][1]["thread"], 2);
        assert_eq!(v["ops"][0]["conflicts"], json!([3]));
        assert_eq!(v["ops"][1]["conflicts"], json!([]));
        assert_eq!(v["ops"][2]["conflicts"], json!([1]));
        assert_eq!(v["conflicts"], 2);
        // The shared fork has the uncontested change only; each side alone shows its own.
        assert_eq!(sources(&proposed_text(&doc.text(), &doc.lock().ops().to_vec())), vec!["# A", "# B", "# C from 1"]);
        assert_eq!(v["ops"][0]["proposed_slide"], 2);
        assert_eq!(v["ops"][2]["proposed_slide"], 2);
        assert_eq!(sources(&alone_text(&doc.text(), doc.lock().ops(), 3).unwrap()), vec!["# A", "# B from 2", "# C"]);
        // Neither side can be accepted until the conflict is settled; accept-all skips it.
        assert!(doc.resolve(1, Action::Accept).unwrap_err().contains("conflicts"));
        let r = doc.resolve_changeset(1, BatchAction::Accept).unwrap();
        assert_eq!(r["accepted"], json!([2]));
        assert_eq!(r["skipped"][0]["op"], 1);
        // Keep the second: the first is rejected and the second is free to go in.
        doc.resolve_conflict(1, Some(3)).unwrap();
        let v = doc.state_view();
        assert_eq!(v["ops"][0]["status"], "rejected");
        assert_eq!(v["ops"][2]["conflicts"], json!([]));
        doc.resolve(3, Action::Accept).unwrap();
        assert_eq!(sources(&doc.text()), vec!["# A", "# B from 2", "# C from 1"]);
    }

    #[tokio::test]
    async fn a_thread_replaces_its_own_op_but_not_anothers() {
        let doc = temp_doc("own");
        doc.propose(Kind::Replace, Some(1), false, "# A1".into(), "".into(), Some(1)).unwrap();
        doc.propose(Kind::Replace, Some(1), false, "# A1 again".into(), "".into(), Some(1)).unwrap();
        assert_eq!(doc.state_view()["ops"].as_array().unwrap().len(), 1);
        // A remote session (no thread) on the same slide: a conflict, not a replacement.
        doc.propose(Kind::Replace, Some(1), false, "# A remote".into(), "".into(), None).unwrap();
        let v = doc.state_view();
        assert_eq!(v["ops"].as_array().unwrap().len(), 2);
        assert_eq!(v["ops"][1]["conflicts"], json!([1]));
        // Deleting a slide someone inserts after is a conflict; two inserts after it are not.
        doc.propose(Kind::Insert, Some(2), false, "# after B".into(), "".into(), Some(1)).unwrap();
        doc.propose(Kind::Insert, Some(2), false, "# also after B".into(), "".into(), Some(2)).unwrap();
        assert_eq!(doc.state_view()["conflicts"], 2);
        doc.propose(Kind::Delete, Some(2), false, String::new(), "".into(), Some(3)).unwrap();
        let v = doc.state_view();
        assert_eq!(v["conflicts"], 5);
        assert_eq!(v["ops"][4]["conflicts"], json!([3, 4]));
    }

    #[tokio::test]
    async fn a_merge_supersedes_the_ops_it_reconciles() {
        let doc = temp_doc("merge");
        doc.open_changeset("one", "", Some(1), vec![]);
        doc.propose(Kind::Replace, Some(2), false, "# B1".into(), "".into(), Some(1)).unwrap();
        doc.open_changeset("two", "", Some(2), vec![]);
        doc.propose(Kind::Replace, Some(2), false, "# B2".into(), "".into(), Some(2)).unwrap();
        let group = doc.conflict_group(1).unwrap();
        assert_eq!(group["all"], json!([1, 2]));
        assert_eq!(group["rivals"][0]["title"], "two");
        // The merge thread's changeset names both; its write resolves them.
        doc.open_changeset("Merge", "", Some(3), vec![1, 2]);
        doc.propose(Kind::Replace, Some(2), false, "# B1+B2".into(), "".into(), Some(3)).unwrap();
        let v = doc.state_view();
        assert_eq!(v["ops"][0]["status"], "merged");
        assert_eq!(v["ops"][1]["status"], "merged");
        assert_eq!(v["ops"][2]["conflicts"], json!([]));
        assert_eq!(v["changesets"][2]["merges"], json!([1, 2]));
        assert_eq!(v["pending"], 1);
        doc.resolve(3, Action::Accept).unwrap();
        assert_eq!(sources(&doc.text()), vec!["# A", "# B1+B2", "# C"]);
    }

    #[test]
    fn old_agent_state_becomes_thread_one() {
        let old = r##"{"agent":{"session":"s","messages":[{"role":"user","text":"Fix slide 2\nplease","ts":5}]}}"##;
        let mut state: DeckState = serde_json::from_str(old).unwrap();
        state.agent.migrate();
        assert_eq!(state.agent.threads.len(), 1);
        assert_eq!(state.agent.threads[0].id, 1);
        assert_eq!(state.agent.threads[0].title, "Fix slide 2");
        assert_eq!(state.agent.threads[0].session.as_deref(), Some("s"));
        assert_eq!(state.agent.threads[0].messages.len(), 1);
        assert_eq!(state.agent.next_thread, 2);
        let out = serde_json::to_string(&state).unwrap();
        assert!(!out.contains("\"session\":\"s\",\"messages\"") || out.contains("threads"));
    }
}
