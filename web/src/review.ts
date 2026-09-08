// The drawer beside the preview: "Ask" (talk to the in-app agent) and
// "Review" (accept / reject / comment on proposed slide changes). State comes
// over the editor's socket (`state` and `agent` messages); actions go over
// plain fetches to /api/decks/<name>/… (src/api.rs).

import type { AgentEvent, AgentMsg, OpView, StateView } from "./protocol.js";

export interface Host {
  deck: string;
  /** Move the editor cursor to a source line. */
  gotoLine(line: number): void;
  /** Show the proposed or the real deck in the preview, optionally at a slide. */
  showProposed(on: boolean, at?: { h: number; v: number }): void;
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
    if (on && !live) live = bubble("live", "working…");
    if (!on && live) { live.remove(); live = null; }
  };
  const send = async () => {
    const message = input.value.trim();
    if (!message || running) return;
    const at = host.cursorSlide();
    try {
      await post(api("agent"), { message, slide: at?.slide ?? null });
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

  // ---- Review
  const showBox = $<HTMLInputElement>("show-proposed");
  const setProposed = (on: boolean, at?: { h: number; v: number }) => {
    proposedShown = on;
    showBox.checked = on;
    host.showProposed(on, at);
  };
  showBox.onchange = () => setProposed(showBox.checked);
  $("clear-resolved").onclick = () => post(api("proposal/clear")).catch((e) => alert(e.message));

  const opsEl = $("ops");
  const act = (op: OpView, action: string, comment = "") =>
    post(api(`proposal/${op.id}/${action}`), { comment }).catch((e) => alert(e.message));

  const card = (op: OpView): HTMLElement => {
    const el = document.createElement("div");
    el.className = `op ${op.status}${op.stale ? " stale" : ""}`;
    const where = op.kind === "deck" ? "whole deck"
      : op.kind === "insert" ? (op.slide ? `after slide ${op.slide}` : "at the top") + (op.vertical ? " (vertical)" : "")
      : op.slide ? `slide ${op.slide}` : "slide (moved)";
    const pill = op.stale ? "stale" : op.status;
    el.innerHTML = `
      <div class="op-head"><span class="kind ${op.kind}">${op.kind}</span> <span class="where">${esc(where)}</span> <span class="pill ${pill}">${pill}</span></div>
      ${op.note ? `<div class="note">${esc(op.note)}</div>` : ""}
      <div class="diff"></div>
      ${op.comment ? `<div class="comment">💬 ${esc(op.comment)}</div>` : ""}
      <div class="row actions"></div>`;
    el.querySelector<HTMLElement>(".diff")!.innerHTML = op.kind === "deck"
      ? `<div class="d">${esc(op.source.slice(0, 2000))}${op.source.length > 2000 ? "…" : ""}</div>`
      : diffHtml(op.current, op.source, op.kind);
    const actions = el.querySelector<HTMLElement>(".actions")!;
    if (op.status === "pending") {
      if (!op.stale) {
        const accept = document.createElement("button"); accept.textContent = "accept"; accept.className = "accept";
        accept.onclick = (e) => { e.stopPropagation(); void act(op, "accept"); };
        actions.append(accept);
      }
      const reject = document.createElement("button"); reject.textContent = "reject";
      reject.onclick = (e) => { e.stopPropagation(); void act(op, "reject"); };
      const comment = document.createElement("button"); comment.textContent = op.comment ? "edit comment" : "comment";
      comment.onclick = (e) => {
        e.stopPropagation();
        const c = prompt("Tell the agent what to change about this proposal:", op.comment);
        if (c !== null) void act(op, "comment", c);
      };
      actions.append(reject, comment);
      if (op.stale) {
        const why = document.createElement("span"); why.className = "why"; why.textContent = "the slide changed since this was proposed";
        actions.append(why);
      }
    } else {
      const drop = document.createElement("button"); drop.textContent = "dismiss";
      drop.onclick = (e) => { e.stopPropagation(); void act(op, "withdraw"); };
      actions.append(drop);
    }
    el.onclick = () => {
      for (const o of opsEl.children) o.classList.toggle("selected", o === el);
      if (op.line) host.gotoLine(op.line);
      if (op.status === "pending" && !op.stale && op.proposed_col) setProposed(true, { h: op.proposed_col - 1, v: op.proposed_row - 1 });
    };
    return el;
  };

  const render = () => {
    if (!state) return;
    reviewBox.checked = state.review;
    const n = state.pending;
    for (const id of ["pcount", "pcount2"]) { const el = $(id); el.textContent = n ? String(n) : ""; el.hidden = !n; }
    opsEl.replaceChildren(...state.ops.map(card));
    if (!state.ops.length) opsEl.innerHTML = `<p class="empty">No proposals. ${state.review ? "Tool edits (MCP, the agent) will appear here for review." : "Review mode is off: tool edits apply directly."}</p>`;
    if (!n && proposedShown) setProposed(false);
  };

  return {
    onState(s: StateView) {
      const first = !state;
      state = s;
      render();
      if (first || !running) renderTranscript(s.agent.messages);
      if (first && s.pending) show("review");
    },
    onAgent(ev: AgentEvent) {
      switch (ev.kind) {
        case "start": setRunning(true); break;
        case "text": bubble("assistant", ev.text ?? ""); if (live) transcript.append(live); break;
        case "tool": bubble("tool", ev.text ?? ""); if (live) transcript.append(live); break;
        case "error": bubble("error", ev.text ?? ""); break;
        case "done": setRunning(false); if (state?.pending) show("review"); break;
      }
      transcript.scrollTop = transcript.scrollHeight;
    },
  };
}
