// The drawer beside the preview: "Ask" (talk to the in-app agent) and
// "Review" (accept / reject / comment on proposed slide changes). State comes
// over the editor's socket (`state` and `agent` messages); actions go over
// plain fetches to /api/decks/<name>/… (src/api.rs).
//
// A proposal is reviewed as a fork of the deck, not as text: each op card
// carries a rendered thumbnail of the proposed slide, and selecting one puts
// the preview into *compare* mode — the current deck above, the proposed deck
// below, both parked on that slide — with prev/next to step through the
// proposal. The text diff is there too, folded away.

import type { AgentEvent, AgentMsg, OpView, StateView } from "./protocol.js";

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

function where(op: OpView): string {
  if (op.kind === "deck") return "whole deck";
  if (op.kind === "insert") return (op.slide ? `after slide ${op.slide}` : "at the top") + (op.vertical ? " (vertical)" : "");
  return op.slide ? `slide ${op.slide}` : "slide (moved)";
}

/** Which slide a thumbnail should show, in which deck; `null` when there is nothing to draw. */
function thumbOf(op: OpView): { view: "current" | "proposed"; slide: number } | null {
  if (op.status !== "pending" || op.stale) return null;
  if (op.kind === "delete") return op.slide ? { view: "current", slide: op.slide } : null;
  if (op.kind === "deck") return { view: "proposed", slide: 1 };
  return op.proposed_slide ? { view: "proposed", slide: op.proposed_slide } : null;
}

// ---- the drawer ---------------------------------------------------------------

export function init(host: Host) {
  const drawer = $("drawer");
  const api = (path: string) => `/api/decks/${host.deck}/${path}`;
  let state: StateView | null = null;
  let running = false;
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

  // ---- Ask
  const transcript = $("transcript");
  const input = $<HTMLTextAreaElement>("ask-input");
  const stopBtn = $<HTMLButtonElement>("ask-stop");
  const sendBtn = $<HTMLButtonElement>("ask-send");
  let live: HTMLElement | null = null; // the "working…" line while a run is on
  let phase = "working…";
  let startedAt = 0;
  let draft: HTMLElement | null = null; // assistant text streaming in (deltas)
  const liveText = () => {
    if (!live) return;
    const secs = startedAt ? Math.round((Date.now() - startedAt) / 1000) : 0;
    live.textContent = `${phase} ${secs ? `(${secs}s)` : ""}`;
  };
  setInterval(liveText, 1000);

  // model / effort selects, remembered per browser
  const modelSel = $<HTMLSelectElement>("ask-model");
  const effortSel = $<HTMLSelectElement>("ask-effort");
  try {
    modelSel.value = localStorage.getItem("slides.ask.model") ?? "";
    effortSel.value = localStorage.getItem("slides.ask.effort") ?? "medium";
  } catch { /* storage may be unavailable */ }
  fetch("/api/agent/defaults").then((r) => r.json()).then((d: { model: string; effort: string }) => {
    modelSel.options[0].textContent = `default (${d.model.replace("claude-", "")})`;
    if (!effortSel.value) effortSel.value = d.effort;
  }).catch(() => {});
  modelSel.onchange = () => { try { localStorage.setItem("slides.ask.model", modelSel.value); } catch {} };
  effortSel.onchange = () => { try { localStorage.setItem("slides.ask.effort", effortSel.value); } catch {} };

  const bubble = (role: string, text: string) => {
    const el = document.createElement("div");
    el.className = `msg ${role}`;
    el.innerHTML = role === "tool" ? `🔧 ${esc(text)}` : esc(text).replace(/`([^`]+)`/g, "<code>$1</code>").replace(/\n/g, "<br>");
    transcript.append(el);
    transcript.scrollTop = transcript.scrollHeight;
    return el;
  };
  const renderTranscript = (msgs: AgentMsg[]) => {
    transcript.replaceChildren();
    for (const m of msgs) bubble(m.role, m.text);
    if (running) live = bubble("live", "working…");
  };
  const setRunning = (on: boolean) => {
    running = on;
    stopBtn.hidden = !on;
    sendBtn.disabled = on;
    if (on) { phase = "starting…"; startedAt = Date.now(); if (!live) live = bubble("live", phase); }
    if (!on && live) { live.remove(); live = null; startedAt = 0; }
    if (!on && draft) { draft.remove(); draft = null; }
  };
  const send = async () => {
    const message = input.value.trim();
    if (!message || running) return;
    const at = host.cursorSlide();
    try {
      await post(api("agent"), { message, slide: at?.slide ?? null, model: modelSel.value || null, effort: effortSel.value || null });
      input.value = "";
      bubble("user", message);
      setRunning(true);
    } catch (e) {
      bubble("error", (e as Error).message);
    }
  };
  sendBtn.onclick = (e) => { e.preventDefault(); void send(); };
  input.onkeydown = (e) => { if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); void send(); } };
  stopBtn.onclick = () => post(api("agent/stop")).catch(() => {});
  $("ask-new").onclick = () => { if (confirm("Start a fresh conversation with the agent? The transcript is cleared.")) void post(api("agent/reset")).catch((e) => alert(e.message)); };
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

  let selected: number | null = null; // op id shown in compare mode
  const pendingOps = () => state?.ops.filter((o) => o.status === "pending" && !o.stale) ?? [];
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
    const ps = pendingOps();
    $("cmp-pos").textContent = `${ps.findIndex((o) => o.id === op.id) + 1} / ${ps.length}`;
    $("cmp-title").textContent = `${op.kind} · ${where(op)}${op.note ? " — " + op.note : ""}`;
    $("label-current").textContent = op.kind === "insert" ? `current · slide ${op.slide ?? "—"} (new slide goes after this)` : `current · slide ${op.slide ?? "—"}`;
    $("label-proposed").textContent = op.kind === "delete" ? `proposed · slide ${op.proposed_slide ?? "—"} (what follows)` : `proposed · slide ${op.proposed_slide ?? "—"}`;
    for (const c of opsEl.children) c.classList.toggle("selected", (c as HTMLElement).dataset.op === String(op.id));
    if (op.line) host.gotoLine(op.line);
  };
  const select = (op: OpView | null) => { selected = op?.id ?? null; syncCompare(); };
  const step = (d: 1 | -1) => {
    const ps = pendingOps();
    if (!ps.length) return select(null);
    const i = ps.findIndex((o) => o.id === selected);
    select(ps[i < 0 ? (d > 0 ? 0 : ps.length - 1) : (i + d + ps.length) % ps.length]);
  };
  $("cmp-prev").onclick = () => step(-1);
  $("cmp-next").onclick = () => step(1);
  $("cmp-close").onclick = () => select(null);
  $("cmp-accept").onclick = () => { const op = current(); if (op) void act(op, "accept"); };
  $("cmp-reject").onclick = () => { const op = current(); if (op) void act(op, "reject"); };
  $("cmp-comment").onclick = () => { const op = current(); if (op) askComment(op); };
  document.addEventListener("keydown", (e) => {
    if (selected === null || bar.hidden) return;
    if ((e.target as HTMLElement | null)?.closest("input, textarea, select, .monaco-editor")) return;
    const op = current();
    switch (e.key) {
      case "n": case "]": step(1); break;
      case "p": case "[": step(-1); break;
      case "a": if (op) void act(op, "accept"); break;
      case "r": if (op) void act(op, "reject"); break;
      case "c": if (op) askComment(op); break;
      case "Escape": select(null); break;
      default: return;
    }
    e.preventDefault();
  });

  const card = (op: OpView): HTMLElement => {
    const el = document.createElement("div");
    el.className = `op ${op.status}${op.stale ? " stale" : ""}`;
    el.dataset.op = String(op.id);
    const pill = op.stale ? "stale" : op.status;
    const t = thumbOf(op);
    el.innerHTML = `
      <div class="op-head"><span class="kind ${op.kind}">${op.kind}</span> <span class="where">${esc(where(op))}</span> <span class="pill ${pill}">${pill}</span></div>
      ${t ? `<div class="thumb${op.kind === "delete" ? " del" : ""}"><iframe tabindex="-1" loading="lazy" src="/deck/${host.deck}/thumb?view=${t.view}&slide=${t.slide}&v=${fnv(op.source)}"></iframe></div>` : ""}
      ${op.note ? `<div class="note">${esc(op.note)}</div>` : ""}
      ${op.comment ? `<div class="comment">💬 ${esc(op.comment)}</div>` : ""}
      <details><summary>text diff</summary><div class="diff"></div></details>
      <div class="row actions"></div>`;
    el.querySelector<HTMLElement>(".diff")!.innerHTML = op.kind === "deck"
      ? `<div class="d">${esc(op.source.slice(0, 2000))}${op.source.length > 2000 ? "…" : ""}</div>`
      : diffHtml(op.current, op.source, op.kind);
    const actions = el.querySelector<HTMLElement>(".actions")!;
    const button = (label: string, cls: string, fn: () => void) => {
      const b = document.createElement("button");
      b.textContent = label; b.className = cls;
      b.onclick = (e) => { e.stopPropagation(); fn(); };
      actions.append(b);
    };
    if (op.status === "pending") {
      if (!op.stale) button("accept", "accept", () => void act(op, "accept"));
      button("reject", "", () => void act(op, "reject"));
      button(op.comment ? "edit comment" : "comment", "", () => askComment(op));
      if (op.stale) {
        const why = document.createElement("span"); why.className = "why"; why.textContent = "the slide changed since this was proposed";
        actions.append(why);
      }
    } else {
      button("dismiss", "", () => void act(op, "withdraw"));
    }
    el.onclick = () => { if (op.status === "pending" && !op.stale) select(op); else if (op.line) host.gotoLine(op.line); };
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
    for (const id of ["pcount", "pcount2"]) { const el = $(id); el.textContent = n ? String(n) : ""; el.hidden = !n; }
    opsEl.replaceChildren(...state.ops.map(card));
    if (!state.ops.length) opsEl.innerHTML = `<p class="empty">No proposals. ${state.review ? "Tool edits (MCP, the agent) will appear here for review." : "Review mode is off: tool edits apply directly."}</p>`;
    requestAnimationFrame(fitThumbs);
    if (!n && proposedShown) setProposed(false);
    // The selected op may have been resolved: move on to the next pending one.
    if (selected !== null && !current()?.stale && current()?.status === "pending") syncCompare();
    else if (selected !== null) { selected = null; if (pendingOps().length) step(1); else syncCompare(); }
  };

  return {
    onState(s: StateView) {
      const first = !state;
      state = s;
      render();
      if (first || !running) renderTranscript(s.agent.messages);
      if (first && s.pending) { show("review"); step(1); }
    },
    onAgent(ev: AgentEvent) {
      switch (ev.kind) {
        case "start":
          setRunning(true);
          if (ev.model) bubble("tool", `${ev.model.replace("claude-", "")} · effort ${ev.effort ?? "?"}`);
          if (live) transcript.append(live);
          break;
        case "phase": phase = ev.text ?? "working…"; liveText(); break;
        case "delta":
          if (!draft) { draft = bubble("assistant draft", ""); if (live) transcript.append(live); }
          draft.textContent += ev.text ?? "";
          break;
        case "text":
          if (draft) { draft.remove(); draft = null; }
          bubble("assistant", ev.text ?? "");
          if (live) transcript.append(live);
          break;
        case "tool": bubble("tool", ev.text ?? ""); if (live) transcript.append(live); break;
        case "error": bubble("error", ev.text ?? ""); break;
        case "done":
          setRunning(false);
          if (state?.pending) { show("review"); if (selected === null) step(1); }
          break;
      }
      transcript.scrollTop = transcript.scrollHeight;
    },
  };
}
