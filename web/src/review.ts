// The drawer beside the preview: "Ask" (talk to the in-app agent) and
// "Review" (accept / reject / comment on proposed slide changes). State comes
// over the editor's socket (`state` and `agent` messages); actions go over
// plain fetches to /api/decks/<name>/… (src/api.rs).
//
// Ask is an inbox of *threads*: every question starts one, several may run at
// once, and each has its own transcript to follow up in. The inbox shows each
// thread's live status; opening one shows the conversation and sends follow-
// ups into it. Send from the inbox to ask something new.
//
// A proposal is reviewed as a fork of the deck, not as text: each op card
// carries a rendered thumbnail of the proposed slide, and selecting one puts
// the preview into *compare* mode — the current deck above, the proposed deck
// below, both parked on that slide — with prev/next to step through the
// changeset. The text diff is there too, folded away.
//
// Ops are grouped by *changeset* — one batch of related edits (an Ask turn,
// or what a remote session put under one `open_changeset`). A changeset can
// be accepted or rejected whole from its header, or stepped through op by op;
// prev/next in the compare bar stay inside the changeset being reviewed.
//
// Two threads that change the same slide put their ops in *conflict*: neither
// is in the shared fork (each is previewed on its own, `view=op`) and neither
// can be accepted until the author keeps one or — the usual choice — has the
// agent merge them, which starts a merge thread whose proposal replaces both.

import type { AgentEvent, ChangesetView, OpView, StateView, ThreadView } from "./protocol.js";

export interface Host {
  deck: string;
  /** Move the editor cursor to a source line. */
  gotoLine(line: number): void;
  /** Show the proposed or the real deck in the (single) preview, optionally at a slide. */
  showProposed(on: boolean, at?: { h: number; v: number }): void;
  /** Split the preview into current/proposed players parked on `op`; `null` restores the single preview. */
  compare(op: OpView | null): void;
  /** 1-based position of the slide under the cursor, and its heading, if known. */
  cursorSlide(): { slide: number; heading: string } | null;
}

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const esc = (s: string) => s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]!);

async function post(url: string, body: unknown = {}): Promise<Response> {
  const r = await fetch(url, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });
  if (!r.ok) throw new Error(await r.text());
  return r;
}

/** 32-bit FNV-1a, for cache-busting thumbnail URLs when an op's source changes. */
function fnv(s: string): number {
  let h = 0x811c9dc5;
  for (const ch of s) { h ^= ch.codePointAt(0)!; h = Math.imul(h, 0x01000193); }
  return h >>> 0;
}

// ---- line diff ----------------------------------------------------------------

type Row = { t: " " | "-" | "+"; s: string };

/** Line diff by LCS; small inputs only (a slide), so quadratic is fine. */
export function diffLines(a: string[], b: string[]): Row[] {
  if (a.length * b.length > 400_000) return [...a.map((s): Row => ({ t: "-", s })), ...b.map((s): Row => ({ t: "+", s }))];
  const n = a.length, m = b.length;
  const dp: Uint16Array[] = Array.from({ length: n + 1 }, () => new Uint16Array(m + 1));
  for (let i = n - 1; i >= 0; i--) for (let j = m - 1; j >= 0; j--)
    dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
  const out: Row[] = [];
  let i = 0, j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) { out.push({ t: " ", s: a[i] }); i++; j++; }
    else if (dp[i + 1][j] >= dp[i][j + 1]) out.push({ t: "-", s: a[i++] });
    else out.push({ t: "+", s: b[j++] });
  }
  while (i < n) out.push({ t: "-", s: a[i++] });
  while (j < m) out.push({ t: "+", s: b[j++] });
  return out;
}

function diffHtml(current: string | null, source: string, kind: OpView["kind"]): string {
  const a = kind === "insert" ? [] : (current ?? "").split("\n");
  const b = kind === "delete" ? [] : source.split("\n");
  return diffLines(a, b).map((r) => `<div class="d${r.t === " " ? "" : r.t === "-" ? " del" : " add"}">${esc(r.s) || "&nbsp;"}</div>`).join("");
}

// ---- describing an op ---------------------------------------------------------

function titleOf(cs: ChangesetView): string {
  return cs.title || "untitled changeset";
}

function where(op: OpView): string {
  if (op.kind === "deck") return "whole deck";
  if (op.kind === "insert") return (op.slide ? `after slide ${op.slide}` : "at the top") + (op.vertical ? " (vertical)" : "");
  return op.slide ? `slide ${op.slide}` : "slide (moved)";
}

/** Which slide a thumbnail should show, in which deck; `null` when there is nothing to draw. */
function thumbOf(op: OpView): { view: "current" | "proposed" | "op"; slide: number } | null {
  if (op.status !== "pending" || op.stale) return null;
  if (op.kind === "delete") return op.slide ? { view: "current", slide: op.slide } : null;
  const view = op.conflicts.length ? "op" : "proposed";
  if (op.kind === "deck") return { view, slide: 1 };
  return op.proposed_slide ? { view, slide: op.proposed_slide } : null;
}

// ---- the drawer ---------------------------------------------------------------

export function init(host: Host) {
  const drawer = $("drawer");
  const api = (path: string) => `/api/decks/${host.deck}/${path}`;
  let state: StateView | null = null;
  let proposedShown = false;

  // tabs
  const show = (tab: "ask" | "review") => {
    drawer.hidden = false;
    document.documentElement.style.setProperty("--drawer-width", "400px");
    for (const b of drawer.querySelectorAll<HTMLButtonElement>("nav button[data-tab]")) b.classList.toggle("active", b.dataset.tab === tab);
    $("tab-ask").hidden = tab !== "ask";
    $("tab-review").hidden = tab !== "review";
    if (tab === "ask") $("ask-input").focus();
  };
  const close = () => {
    drawer.hidden = true;
    document.documentElement.style.setProperty("--drawer-width", "0px");
  };
  for (const b of drawer.querySelectorAll<HTMLButtonElement>("nav button[data-tab]")) b.onclick = () => show(b.dataset.tab as "ask" | "review");
  $("drawer-close").onclick = close;
  $("open-ask").onclick = () => show("ask");
  $("open-review").onclick = () => show("review");

  // review mode toggle (header)
  const reviewBox = $<HTMLInputElement>("review-mode");
  reviewBox.onchange = () => post(api("review"), { review: reviewBox.checked }).catch((e) => alert(e.message));

  // ---- Ask: the inbox of threads, and the open thread
  const threadsEl = $("threads");
  const threadEl = $("thread");
  const transcript = $("transcript");
  const input = $<HTMLTextAreaElement>("ask-input");
  const sendBtn = $<HTMLButtonElement>("ask-send");
  let open: number | null = null; // the thread whose transcript is shown; null = inbox
  /** What a running thread is doing right now (from agent events), by thread id. */
  type Run = { phase: string; startedAt: number; draft: string; error?: string };
  const runs = new Map<number, Run>();
  const threads = () => state?.agent.threads ?? [];
  const threadOf = (id: number | null) => threads().find((t) => t.id === id) ?? null;
  const elapsed = (r: Run) => { const s = Math.round((Date.now() - r.startedAt) / 1000); return s ? ` (${s}s)` : ""; };

  // model / effort selects, remembered per browser
  const modelSel = $<HTMLSelectElement>("ask-model");
  const effortSel = $<HTMLSelectElement>("ask-effort");
  try {
    modelSel.value = localStorage.getItem("slides.ask.model") ?? "";
    effortSel.value = localStorage.getItem("slides.ask.effort") ?? "medium";
  } catch { /* storage may be unavailable */ }
  let agentAvailable = true;
  fetch("/api/agent/defaults").then((r) => r.json()).then((d: { model: string; effort: string; available: boolean; unavailable?: string }) => {
    modelSel.options[0].textContent = `default (${d.model.replace("claude-", "")})`;
    if (!effortSel.value) effortSel.value = d.effort;
    if (!d.available) {
      // No agent on this server: the drawer opens on Review, and Ask explains
      // itself instead of failing on send.
      agentAvailable = false;
      $("open-ask").hidden = true;
      drawer.querySelector<HTMLButtonElement>('nav button[data-tab="ask"]')!.hidden = true;
      $<HTMLFormElement>("ask-form").hidden = true;
      threadsEl.insertAdjacentHTML("afterend", `<p class="empty">The in-app agent is not configured on this server (${esc(d.unavailable ?? "")}). See README.md, "Review mode and the in-app agent".</p>`);
      if (!drawer.hidden && !$("tab-ask").hidden) show("review");
      render();
    }
  }).catch(() => {});
  modelSel.onchange = () => { try { localStorage.setItem("slides.ask.model", modelSel.value); } catch {} };
  effortSel.onchange = () => { try { localStorage.setItem("slides.ask.effort", effortSel.value); } catch {} };

  const runOpts = () => ({ model: modelSel.value || null, effort: effortSel.value || null });

  /** Pending ops a thread has on the table (through its changesets), and how many are in conflict. */
  const threadPending = (t: ThreadView) => {
    const mine = new Set(state?.changesets.filter((c) => c.thread === t.id).map((c) => c.id));
    const ops = state?.ops.filter((o) => mine.has(o.changeset) && o.status === "pending" && !o.stale) ?? [];
    return { pending: ops.length, conflicts: ops.filter((o) => o.conflicts.length).length };
  };
  const statusOf = (t: ThreadView): { text: string; cls: string } => {
    const r = runs.get(t.id);
    if (t.running) return { text: (r?.phase ?? "working…") + (r ? elapsed(r) : ""), cls: "running" };
    const last = t.messages[t.messages.length - 1];
    if (last?.role === "error") return { text: "failed", cls: "error" };
    return { text: t.messages.some((m) => m.role === "assistant") ? "done" : "", cls: "" };
  };
  const snippetOf = (t: ThreadView) => {
    const r = runs.get(t.id);
    if (t.running && r?.draft) return r.draft;
    const last = [...t.messages].reverse().find((m) => m.role === "assistant" || m.role === "error");
    return last?.text ?? "";
  };

  const renderInbox = () => {
    const ts = [...threads()].sort((a, b) => Number(b.running) - Number(a.running) || b.created - a.created);
    threadsEl.replaceChildren(...ts.map((t) => {
      const el = document.createElement("div");
      const st = statusOf(t);
      const p = threadPending(t);
      el.className = `thread ${st.cls}${t.merge ? " merge" : ""}`;
      el.dataset.thread = String(t.id);
      el.innerHTML = `
        <div class="t-head"><span class="t-title">${esc(t.title || "untitled")}</span>
          ${p.pending ? `<span class="t-pending${p.conflicts ? " conflict" : ""}" title="${p.conflicts ? `${p.conflicts} in conflict` : "pending proposals"}">${p.pending}</span>` : ""}
          <span class="t-status">${esc(st.text)}</span></div>
        ${snippetOf(t) ? `<div class="t-snippet">${esc(snippetOf(t))}</div>` : ""}`;
      el.onclick = () => openThread(t.id);
      return el;
    }));
    if (!ts.length && agentAvailable) threadsEl.innerHTML = `<p class="empty">Ask for a change below. Each question runs on its own — ask the next one right away; they run at the same time, and you review every proposal in one place.</p>`;
  };

  const bubble = (role: string, text: string) => {
    const el = document.createElement("div");
    el.className = `msg ${role}`;
    el.innerHTML = role === "tool" ? `🔧 ${esc(text)}` : esc(text).replace(/`([^`]+)`/g, "<code>$1</code>").replace(/\n/g, "<br>");
    transcript.append(el);
    return el;
  };
  let live: HTMLElement | null = null; // the "working…" line of the open thread
  let draft: HTMLElement | null = null; // its assistant text streaming in
  const renderThread = () => {
    const t = threadOf(open);
    if (!t) { if (open !== null) { open = null; syncAsk(); } return; }
    $("thread-title").textContent = t.title || "untitled";
    $("thread-title").title = new Date(t.created * 1000).toLocaleString();
    $("thread-stop").hidden = !t.running;
    transcript.replaceChildren();
    live = draft = null;
    for (const m of t.messages) {
      // A merge thread's request is long and mechanical: fold it.
      if (t.merge && m.role === "user") {
        const d = document.createElement("details");
        d.className = "msg user";
        d.innerHTML = `<summary>merge request</summary>${esc(m.text).replace(/\n/g, "<br>")}`;
        transcript.append(d);
      } else bubble(m.role, m.text);
    }
    const r = runs.get(t.id);
    if (t.running) {
      if (r?.draft) { draft = bubble("assistant draft", ""); draft.textContent = r.draft; }
      live = bubble("live", (r?.phase ?? "working…") + (r ? elapsed(r) : ""));
    }
    transcript.scrollTop = transcript.scrollHeight;
  };
  const ts_id = (el: HTMLElement) => (el.dataset.thread ? Number(el.dataset.thread) : null);
  const tick = () => {
    const t = threadOf(open);
    const r = t && runs.get(t.id);
    if (live && r) live.textContent = r.phase + elapsed(r);
    for (const el of threadsEl.querySelectorAll<HTMLElement>(".thread.running")) {
      // Refresh the seconds on running rows without a full re-render.
      const id = ts_id(el);
      const rr = id !== null && runs.get(id);
      const st = el.querySelector<HTMLElement>(".t-status");
      if (rr && st) st.textContent = rr.phase + elapsed(rr);
    }
  };
  setInterval(tick, 1000);

  /** Inbox or open thread: which one shows, and what the composer does. */
  const syncAsk = () => {
    const t = threadOf(open);
    threadsEl.hidden = !!t;
    threadEl.hidden = !t;
    input.placeholder = t
      ? (t.running ? "This thread is still working — a follow-up waits for it. Or ask something new from ‹ all." : "Follow up in this thread. Enter sends, Shift+Enter for a newline.")
      : "Ask for a change — several questions can run at once. Enter sends, Shift+Enter for a newline.";
    sendBtn.disabled = !!t?.running;
    if (t) renderThread(); else renderInbox();
  };
  const openThread = (id: number | null) => { open = id; syncAsk(); if (id !== null) input.focus(); };
  $("thread-back").onclick = () => openThread(null);
  $("thread-stop").onclick = () => { if (open !== null) void post(api("agent/stop"), { thread: open }).catch(() => {}); };
  $("thread-close").onclick = () => {
    const t = threadOf(open);
    if (t && (!t.running || confirm("This thread is still working. Stop it and drop the thread?"))) {
      void post(api(`agent/thread/${t.id}/close`)).then(() => openThread(null)).catch((e) => alert(e.message));
    }
  };

  const send = async () => {
    const message = input.value.trim();
    if (!message) return;
    const t = threadOf(open);
    if (t?.running) return;
    const at = host.cursorSlide();
    try {
      const r = await post(api("agent"), { message, slide: at?.slide ?? null, thread: t?.id ?? null, ...runOpts() });
      const { thread } = (await r.json()) as { thread: number };
      input.value = "";
      runs.set(thread, { phase: "starting…", startedAt: Date.now(), draft: "" });
      // Asked from the inbox: stay there, so the next question can go straight out.
      if (t) renderThread();
    } catch (e) {
      alert((e as Error).message);
    }
  };
  sendBtn.onclick = (e) => { e.preventDefault(); void send(); };
  input.onkeydown = (e) => { if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); void send(); } };
  $("ask-new").onclick = () => { if (confirm("Drop every conversation with the agent? Proposals stay.")) void post(api("agent/reset")).then(() => openThread(null)).catch((e) => alert(e.message)); };
  const updateContext = () => {
    const at = host.cursorSlide();
    $("ask-context").textContent = at ? `cursor: slide ${at.slide}${at.heading ? " · " + at.heading : ""}` : "";
  };
  setInterval(updateContext, 800);

  // ---- Review: browse the fork, or compare op by op
  const showBox = $<HTMLInputElement>("show-proposed");
  const setProposed = (on: boolean, at?: { h: number; v: number }) => {
    proposedShown = on;
    showBox.checked = on;
    host.showProposed(on, at);
  };
  showBox.onchange = () => { if (showBox.checked) select(null); setProposed(showBox.checked); };
  $("clear-resolved").onclick = () => post(api("proposal/clear")).catch((e) => alert(e.message));

  const opsEl = $("ops");
  const act = (op: OpView, action: string, comment = "") =>
    post(api(`proposal/${op.id}/${action}`), { comment }).catch((e) => alert(e.message));
  const askComment = (op: OpView) => {
    const c = prompt("Tell the agent what to change about this proposal:", op.comment);
    if (c !== null) void act(op, "comment", c);
  };
  /** Settle a conflict: keep this op, keep one rival, or have a merge thread reconcile them all. */
  const settle = (op: OpView, how: "keep" | "keep_other" | "merge", other?: number) =>
    post(api(`proposal/${op.id}/conflict/${how}`), how === "merge" ? runOpts() : { other })
      .then(async (r) => {
        if (how !== "merge") return;
        const { thread } = (await r.json()) as { thread: number };
        runs.set(thread, { phase: "starting…", startedAt: Date.now(), draft: "" });
      })
      .catch((e) => alert(e.message));
  const rivalsOf = (op: OpView) => op.conflicts.map((id) => state?.ops.find((o) => o.id === id)).filter((o): o is OpView => !!o);
  const csOf = (op: OpView) => state?.changesets.find((c) => c.id === op.changeset) ?? null;
  const rivalLabel = (r: OpView) => { const cs = csOf(r); return `#${r.id}${cs ? " · " + titleOf(cs) : ""}`; };

  const batch = (cs: ChangesetView, action: "accept" | "reject" | "withdraw") =>
    post(api(`changeset/${cs.id}/${action}`)).then(async (r) => {
      const v = (await r.json()) as StateView & { result?: { skipped?: { op: number; why: string }[] } };
      const skipped = v.result?.skipped ?? [];
      if (action === "accept" && skipped.length) alert(`${skipped.length} change${skipped.length > 1 ? "s" : ""} could not be merged and stay${skipped.length > 1 ? "" : "s"} pending:\n` + skipped.map((s) => `· ${s.why}`).join("\n"));
    }).catch((e) => alert(e.message));
  const confirmBatch = (cs: ChangesetView, action: "accept" | "reject") => {
    const n = cs.pending + (action === "reject" ? cs.stale : 0);
    if (n <= 1 || confirm(`${action === "accept" ? "Accept" : "Reject"} all ${n} pending changes in “${titleOf(cs)}”?`)) void batch(cs, action);
  };

  let selected: number | null = null; // op id shown in compare mode
  /** Pending, mergeable ops — of one changeset, or of the whole proposal. */
  const pendingOps = (cs?: number) => state?.ops.filter((o) => o.status === "pending" && !o.stale && (cs === undefined || o.changeset === cs)) ?? [];
  const current = () => state?.ops.find((o) => o.id === selected) ?? null;

  const bar = $("compare-bar");
  const syncCompare = () => {
    const op = current();
    if (!op || op.status !== "pending" || op.stale) {
      selected = null;
      host.compare(null);
      for (const c of opsEl.children) c.classList.remove("selected");
      return;
    }
    if (proposedShown) setProposed(false);
    host.compare(op);
    const cs = csOf(op);
    const ps = pendingOps(op.changeset);
    const conflicted = op.conflicts.length > 0;
    $("cmp-pos").textContent = `${ps.findIndex((o) => o.id === op.id) + 1} / ${ps.length}`;
    $("cmp-title").textContent = `${cs ? titleOf(cs) + " · " : ""}${op.kind} · ${where(op)}${conflicted ? ` — ⚡ conflicts with ${rivalsOf(op).map(rivalLabel).join(", ")}` : op.note ? " — " + op.note : ""}`;
    $("cmp-accept").hidden = conflicted;
    $("cmp-accept-all").hidden = conflicted || ps.length < 2;
    $("cmp-merge").hidden = $("cmp-keep").hidden = $("cmp-keep-other").hidden = !conflicted;
    $("label-current").textContent = op.kind === "insert" ? `current · slide ${op.slide ?? "—"} (new slide goes after this)` : `current · slide ${op.slide ?? "—"}`;
    $("label-proposed").textContent = (op.kind === "delete" ? `proposed · slide ${op.proposed_slide ?? "—"} (what follows)` : `proposed · slide ${op.proposed_slide ?? "—"}`) + (conflicted ? " · this proposal alone" : "");
    for (const c of opsEl.querySelectorAll<HTMLElement>(".op")) c.classList.toggle("selected", c.dataset.op === String(op.id));
    if (op.line) host.gotoLine(op.line);
  };
  const select = (op: OpView | null) => { selected = op?.id ?? null; syncCompare(); };
  /** Prev/next within the selected op's changeset; with nothing selected, the first pending op anywhere. */
  const step = (d: 1 | -1) => {
    const cur = current();
    const ps = pendingOps(cur?.changeset);
    if (!ps.length) return select(null);
    const i = ps.findIndex((o) => o.id === selected);
    select(ps[i < 0 ? (d > 0 ? 0 : ps.length - 1) : (i + d + ps.length) % ps.length]);
  };
  const acceptRest = () => { const op = current(); const cs = op && csOf(op); if (cs) confirmBatch(cs, "accept"); };
  $("cmp-prev").onclick = () => step(-1);
  $("cmp-next").onclick = () => step(1);
  $("cmp-close").onclick = () => select(null);
  $("cmp-accept").onclick = () => { const op = current(); if (op) void act(op, "accept"); };
  $("cmp-accept-all").onclick = acceptRest;
  $("cmp-reject").onclick = () => { const op = current(); if (op) void act(op, "reject"); };
  $("cmp-comment").onclick = () => { const op = current(); if (op) askComment(op); };
  $("cmp-merge").onclick = () => { const op = current(); if (op?.conflicts.length) void settle(op, "merge"); };
  $("cmp-keep").onclick = () => { const op = current(); if (op?.conflicts.length) void settle(op, "keep"); };
  $("cmp-keep-other").onclick = () => { const op = current(); if (op?.conflicts.length) void settle(op, "keep_other", op.conflicts[0]); };
  document.addEventListener("keydown", (e) => {
    if (selected === null || bar.hidden) return;
    if ((e.target as HTMLElement | null)?.closest("input, textarea, select, .monaco-editor")) return;
    const op = current();
    const conflicted = !!op?.conflicts.length;
    switch (e.key) {
      case "n": case "]": step(1); break;
      case "p": case "[": step(-1); break;
      case "a": if (op && !conflicted) void act(op, "accept"); break;
      case "A": acceptRest(); break;
      case "r": if (op) void act(op, "reject"); break;
      case "c": if (op) askComment(op); break;
      case "m": if (op && conflicted) void settle(op, "merge"); break;
      case "k": if (op && conflicted) void settle(op, "keep"); break;
      case "o": if (op && conflicted) void settle(op, "keep_other", op.conflicts[0]); break;
      case "Escape": select(null); break;
      default: return;
    }
    e.preventDefault();
  });

  const card = (op: OpView): HTMLElement => {
    const el = document.createElement("div");
    const conflicted = op.status === "pending" && !op.stale && op.conflicts.length > 0;
    el.className = `op ${op.status}${op.stale ? " stale" : ""}${conflicted ? " conflict" : ""}`;
    el.dataset.op = String(op.id);
    const pill = op.stale ? "stale" : conflicted ? "conflict" : op.status;
    const t = thumbOf(op);
    el.innerHTML = `
      <div class="op-head"><span class="kind ${op.kind}">${op.kind}</span> <span class="where">${esc(where(op))}</span> <span class="pill ${pill}">${pill}</span></div>
      ${t ? `<div class="thumb${op.kind === "delete" ? " del" : ""}"><iframe tabindex="-1" loading="lazy" src="/deck/${host.deck}/thumb?view=${t.view}&slide=${t.slide}${t.view === "op" ? `&op=${op.id}` : ""}&v=${fnv(op.source)}"></iframe></div>` : ""}
      ${op.note ? `<div class="note">${esc(op.note)}</div>` : ""}
      ${op.comment ? `<div class="comment">💬 ${esc(op.comment)}</div>` : ""}
      ${conflicted ? `<div class="conflict-box">⚡ another proposal changes this slide too: ${rivalsOf(op).map((r) => `<span class="rival" data-op="${r.id}">${esc(rivalLabel(r))}</span>`).join(", ")}<div class="row conflict-actions"></div></div>` : ""}
      <details><summary>text diff</summary><div class="diff"></div></details>
      <div class="row actions"></div>`;
    el.querySelector<HTMLElement>(".diff")!.innerHTML = op.kind === "deck"
      ? `<div class="d">${esc(op.source.slice(0, 2000))}${op.source.length > 2000 ? "…" : ""}</div>`
      : diffHtml(op.current, op.source, op.kind);
    const button = (into: HTMLElement, label: string, cls: string, fn: () => void, title = "") => {
      const b = document.createElement("button");
      b.textContent = label; b.className = cls; b.title = title;
      b.onclick = (e) => { e.stopPropagation(); fn(); };
      into.append(b);
    };
    if (conflicted) {
      const box = el.querySelector<HTMLElement>(".conflict-actions")!;
      button(box, "✦ merge with AI", "merge", () => void settle(op, "merge"), "a merge thread combines the proposals into one; usually what you want");
      button(box, "keep this", "", () => void settle(op, "keep"), "drop the other proposal(s)");
      for (const r of rivalsOf(op)) button(box, op.conflicts.length > 1 ? `keep ${rivalLabel(r)}` : "keep other", "", () => void settle(op, "keep_other", r.id), "drop this proposal");
      for (const s of el.querySelectorAll<HTMLElement>(".rival")) s.onclick = (e) => { e.stopPropagation(); const r = state?.ops.find((o) => o.id === Number(s.dataset.op)); if (r) select(r); };
    }
    const actions = el.querySelector<HTMLElement>(".actions")!;
    if (op.status === "pending") {
      if (!op.stale && !conflicted) button(actions, "accept", "accept", () => void act(op, "accept"));
      button(actions, "reject", "", () => void act(op, "reject"));
      button(actions, op.comment ? "edit comment" : "comment", "", () => askComment(op));
      if (op.stale) {
        const why = document.createElement("span"); why.className = "why"; why.textContent = "the slide changed since this was proposed";
        actions.append(why);
      }
    } else {
      button(actions, "dismiss", "", () => void act(op, "withdraw"));
    }
    el.onclick = () => { if (op.status === "pending" && !op.stale) select(op); else if (op.line) host.gotoLine(op.line); };
    return el;
  };

  // ---- changeset groups
  const folded = new Map<number, boolean>(); // author's explicit fold/unfold, by changeset id
  const group = (cs: ChangesetView, ops: OpView[]): HTMLElement => {
    const el = document.createElement("section");
    const live = cs.pending + cs.stale;
    const done = !live && !cs.open;
    const isFolded = folded.get(cs.id) ?? done;
    el.className = `cs${cs.open ? " open" : ""}${done ? " done" : ""}${isFolded ? " folded" : ""}`;
    el.dataset.cs = String(cs.id);
    const counts: string[] = [];
    if (cs.conflicted) counts.push(`<span class="conflict">${cs.conflicted} in conflict</span>`);
    if (cs.pending - cs.conflicted) counts.push(`<span class="pend">${cs.pending - cs.conflicted} pending</span>`);
    if (cs.stale) counts.push(`<span class="stale">${cs.stale} stale</span>`);
    if (cs.accepted) counts.push(`${cs.accepted} accepted`);
    if (cs.rejected) counts.push(`${cs.rejected} rejected`);
    if (cs.merged) counts.push(`${cs.merged} merged`);
    const merges = cs.merges.length ? `<div class="cs-note">✦ merge of proposals ${cs.merges.map((m) => `#${m}`).join(", ")}</div>` : "";
    el.innerHTML = `
      <div class="cs-head" title="${esc(new Date(cs.created * 1000).toLocaleString())}">
        <button class="fold" title="fold / unfold">${isFolded ? "▸" : "▾"}</button>
        <span class="cs-title${cs.title ? "" : " untitled"}">${esc(titleOf(cs))}</span>
        ${cs.open ? `<span class="writing" title="still taking writes from the tool that opened it">open</span>` : ""}
        <span class="cs-counts">${counts.join(" · ")}</span>
      </div>
      ${merges}
      ${cs.note ? `<div class="cs-note">${esc(cs.note)}</div>` : ""}
      <div class="row cs-actions"></div>
      <div class="cs-ops"></div>`;
    el.querySelector<HTMLElement>(".cs-head")!.onclick = () => { folded.set(cs.id, !isFolded); render(); };
    const actions = el.querySelector<HTMLElement>(".cs-actions")!;
    const button = (label: string, cls: string, title: string, fn: () => void) => {
      const b = document.createElement("button");
      b.textContent = label; b.className = cls; b.title = title;
      b.onclick = (e) => { e.stopPropagation(); fn(); };
      actions.append(b);
    };
    if (cs.pending) {
      button("review", "", "step through this changeset slide by slide", () => { const first = pendingOps(cs.id)[0]; if (first) select(first); });
      const n = cs.pending - cs.conflicted;
      if (n) button(n > 1 ? `accept all ${n}` : "accept", "accept", cs.conflicted ? "accept every pending change that is not in conflict" : "accept every pending change in this changeset", () => confirmBatch(cs, "accept"));
    }
    if (live) button(live > 1 ? "reject all" : "reject", "", "reject every pending change in this changeset", () => confirmBatch(cs, "reject"));
    if (!actions.children.length) actions.remove();
    const opsBox = el.querySelector<HTMLElement>(".cs-ops")!;
    opsBox.replaceChildren(...ops.map(card));
    if (!ops.length) opsBox.innerHTML = `<p class="empty">${cs.open ? "nothing proposed yet" : "no changes"}</p>`;
    return el;
  };

  /** Thumbnails are full-size players scaled to the card; size them once laid out. */
  const fitThumbs = () => {
    for (const th of opsEl.querySelectorAll<HTMLElement>(".thumb")) {
      const f = th.querySelector<HTMLIFrameElement>("iframe");
      if (f && th.clientWidth) f.style.transform = `scale(${th.clientWidth / 1280})`;
    }
  };

  const render = () => {
    if (!state) return;
    reviewBox.checked = state.review;
    const n = state.pending;
    for (const id of ["pcount", "pcount2"]) {
      const el = $(id);
      el.textContent = n ? String(n) : "";
      el.hidden = !n;
      el.classList.toggle("warn", state.conflicts > 0);
      el.title = state.conflicts ? `${state.conflicts} in conflict` : "";
    }
    const groups = state.changesets.map((cs) => group(cs, state!.ops.filter((o) => o.changeset === cs.id)));
    // Ops whose changeset is gone should not happen (the server migrates old files), but never hide one.
    const orphans = state.ops.filter((o) => !state!.changesets.some((c) => c.id === o.changeset));
    if (orphans.length) groups.push(group({ id: -1, title: "", note: "", created: 0, open: false, thread: null, merges: [], ops: orphans.length, pending: orphans.filter((o) => o.status === "pending" && !o.stale).length, stale: orphans.filter((o) => o.stale).length, conflicted: orphans.filter((o) => o.conflicts.length).length, accepted: orphans.filter((o) => o.status === "accepted").length, rejected: orphans.filter((o) => o.status === "rejected").length, merged: orphans.filter((o) => o.status === "merged").length }, orphans));
    opsEl.replaceChildren(...groups);
    if (!groups.length) opsEl.innerHTML = `<p class="empty">No proposals. ${state.review ? "Tool edits (MCP, the agent) will appear here for review." : "Review mode is off: tool edits apply directly."}</p>`;
    requestAnimationFrame(fitThumbs);
    if (!n && proposedShown) setProposed(false);
    // The selected op may have been resolved: move on to the next pending one,
    // in its changeset if it has any left, else anywhere.
    const cur = current();
    if (selected !== null && cur && !cur.stale && cur.status === "pending") syncCompare();
    else if (selected !== null) {
      const next = (cur && pendingOps(cur.changeset)[0]) ?? pendingOps()[0] ?? null;
      selected = null;
      if (next) select(next); else syncCompare();
    }
    // Threads that stopped running have no live status any more.
    for (const t of threads()) if (!t.running) runs.delete(t.id);
    syncAsk();
  };

  return {
    onState(s: StateView) {
      const first = !state;
      state = s;
      render();
      if (first && s.pending) { show("review"); step(1); }
    },
    onAgent(ev: AgentEvent) {
      const r = runs.get(ev.thread) ?? { phase: "working…", startedAt: Date.now(), draft: "" };
      runs.set(ev.thread, r);
      const shown = open === ev.thread;
      switch (ev.kind) {
        case "start":
          r.phase = "starting…"; r.startedAt = Date.now(); r.draft = "";
          if (shown) { if (ev.model) bubble("tool", `${ev.model.replace("claude-", "")} · effort ${ev.effort ?? "?"}`); if (live) transcript.append(live); }
          break;
        case "phase": r.phase = ev.text ?? "working…"; if (shown) tick(); break;
        case "delta":
          r.draft += ev.text ?? "";
          if (shown) {
            if (!draft) { draft = bubble("assistant draft", ""); if (live) transcript.append(live); }
            draft.textContent = r.draft;
          }
          break;
        case "text":
          r.draft = "";
          if (shown) { if (draft) { draft.remove(); draft = null; } bubble("assistant", ev.text ?? ""); if (live) transcript.append(live); }
          break;
        case "tool": if (shown) { bubble("tool", ev.text ?? ""); if (live) transcript.append(live); } break;
        case "error": r.error = ev.text; if (shown) bubble("error", ev.text ?? ""); break;
        case "done":
          runs.delete(ev.thread);
          // The state message that follows re-renders; until then, drop the live line.
          if (shown && live) { live.remove(); live = null; }
          if (shown && draft) { draft.remove(); draft = null; }
          if (state?.pending && !state.agent.threads.some((t) => t.running && t.id !== ev.thread)) { show("review"); if (selected === null) step(1); }
          break;
      }
      if (shown) transcript.scrollTop = transcript.scrollHeight;
      else renderInbox();
    },
  };
}
