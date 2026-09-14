// The live preview: the player page plus this script. It keeps the reveal.js
// instance alive and patches slide bodies in place as the server sends them
// (protocol in src/live.rs), so the slide being edited never flickers. It is
// self-contained — open /deck/<name>/live on a second screen for a live view.
// Inside the editor's iframe, a click on a slide tells the editor which source
// line it was (block elements carry `data-line` from the renderer).

import type { Body, PatchSlide, ServerMsg } from "./protocol.js";
import { socketUrl } from "./protocol.js";

const root = document.querySelector<HTMLElement>(".reveal .slides")!;
let shape: number[] = []; // rows per column currently in the DOM

/** Slide elements by (column, row), reading the DOM by `shape`. */
function slides(): HTMLElement[][] {
  return shape.map((rows, h) => {
    const col = root.children[h] as HTMLElement;
    return rows === 1 ? [col] : (Array.from(col.children) as HTMLElement[]);
  });
}

/** Re-creates the column skeleton, keeping every slide element that still has a position. */
function rebuild(cols: PatchSlide[][]) {
  const old = slides();
  const section = () => document.createElement("section");
  root.replaceChildren(...cols.map((col, h) => {
    const els = col.map((_, v) => old[h]?.[v] ?? section());
    if (els.length === 1) return els[0];
    const stack = section();
    stack.append(...els);
    return stack;
  }));
}

function attributes(attrs: string): Attr[] {
  const t = document.createElement("template");
  t.innerHTML = `<section ${attrs}></section>`;
  return Array.from(t.content.firstElementChild!.attributes);
}

function fill(el: HTMLElement, body: Body) {
  // Drop what the previous body set (reveal's own classes and attributes stay), then set the new.
  const classes = (v: string) => v.split(/\s+/).filter(Boolean);
  for (const a of attributes(el.dataset.attrs ?? "")) a.name === "class" ? el.classList.remove(...classes(a.value)) : el.removeAttribute(a.name);
  for (const a of attributes(body.attrs)) a.name === "class" ? el.classList.add(...classes(a.value)) : el.setAttribute(a.name, a.value);
  el.dataset.attrs = body.attrs;
  el.innerHTML = body.html + (body.notes ? `<aside class="notes">${body.notes}</aside>` : "");
  const highlight = Reveal.getPlugin("highlight");
  el.querySelectorAll<HTMLElement>("pre code").forEach((code) => {
    code.parentElement!.classList.add("code-wrapper"); // as the plugin does on load
    highlight.highlightBlock(code);
  });
}

function apply(cols: PatchSlide[][]) {
  const structural = cols.length !== shape.length || cols.some((c, h) => c.length !== shape[h]);
  if (structural) rebuild(cols);
  shape = cols.map((c) => c.length);
  const els = slides();
  cols.forEach((col, h) => col.forEach((s, v) => {
    els[h][v].dataset.line = String(s.line); // the fallback for clicks on bare slide chrome
    if (s.body) fill(els[h][v], s.body);
  }));
  if (structural) {
    // sync() re-reads backgrounds and controls; slide() is what re-derives
    // past/present/future and which sections are vertical stacks.
    Reveal.sync();
    const { h = 0, v = 0 } = Reveal.getIndices(); // undefined until reveal has had a slide
    Reveal.slide(h, v);
  } else {
    Reveal.layout();
  }
  if (awaiting > 0) awaiting--;
  if (awaiting === 0 && parked) {
    const { h, v } = parked;
    parked = null;
    Reveal.slide(h, v);
  }
}

let ws: WebSocket | null = null;
let proposed = false; // which deck this preview shows (see `view` in src/live.rs)
let viewOp: number | undefined; // …or the deck with just this pending op applied (a conflicted op)
// Patches we asked for and have not applied yet: the one every connection
// opens with, and one per `view` switch. A `goto` that arrives meanwhile is
// parked until they land — its indices refer to the deck we asked for, which
// may not have the slide yet (or have it somewhere else).
let awaiting = 1;
let parked: { h: number; v: number } | null = null;

function goto(h: number, v: number) {
  if (awaiting > 0) parked = { h, v };
  else Reveal.slide(h, v);
}

function askView() {
  if (ws?.readyState !== WebSocket.OPEN) return; // onopen sends it
  awaiting++;
  ws.send(JSON.stringify({ type: "view", proposed, op: viewOp }));
}

function connect() {
  awaiting = 1;
  ws = new WebSocket(socketUrl());
  ws.onopen = () => { if (proposed || viewOp !== undefined) askView(); };
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data) as ServerMsg;
    if (m.type === "patch") apply(m.cols);
  };
  ws.onclose = () => setTimeout(connect, 1000);
}

window.addEventListener("message", (e) => {
  if (e.origin !== location.origin) return;
  if (e.data?.type === "goto") goto(Number(e.data.h) || 0, Number(e.data.v) || 0);
  if (e.data?.type === "view") {
    proposed = !!e.data.proposed;
    viewOp = typeof e.data.op === "number" ? e.data.op : undefined;
    askView();
  }
});
// The editor may have posted `view`/`goto` before this script ran (it enters
// compare mode as soon as its first state arrives, while the proposed pane is
// still loading) — those messages are lost. Tell it we are listening now, and
// it repeats what it wants (see `Player` in editor.ts).
if (window.parent !== window) window.parent.postMessage({ type: "ready" }, location.origin);

// ---- click → source line ------------------------------------------------------

root.addEventListener("click", (e) => {
  if (window.parent === window) return; // standalone live view: nothing to tell
  const target = e.target as Element | null;
  if (!target || target.closest("a, button, .controls, .progress")) return;
  if (getSelection()?.toString()) return; // the user was selecting text, not pointing
  const line = Number(target.closest<HTMLElement>("[data-line]")?.dataset.line);
  if (line > 0) window.parent.postMessage({ type: "edit", line }, location.origin);
});

const ready = Reveal.isReady() ? Promise.resolve() : new Promise<void>((r) => Reveal.on("ready", () => r()));
ready.then(connect);
