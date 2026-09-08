// The live preview: the player page plus this script. It keeps the reveal.js
// instance alive and patches slide bodies in place as the server sends them
// (protocol in src/live.rs), so the slide being edited never flickers. It is
// self-contained — open /deck/<name>/live on a second screen for a live view.

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
  cols.forEach((col, h) => col.forEach((s, v) => { if (s.body) fill(els[h][v], s.body); }));
  if (structural) {
    // sync() re-reads backgrounds and controls; slide() is what re-derives
    // past/present/future and which sections are vertical stacks.
    Reveal.sync();
    const { h = 0, v = 0 } = Reveal.getIndices(); // undefined until reveal has had a slide
    Reveal.slide(h, v);
  } else {
    Reveal.layout();
  }
}

function connect() {
  const ws = new WebSocket(socketUrl());
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data) as ServerMsg;
    if (m.type === "patch") apply(m.cols);
  };
  ws.onclose = () => setTimeout(connect, 1000);
}

window.addEventListener("message", (e) => {
  if (e.origin === location.origin && e.data?.type === "goto") Reveal.slide(e.data.h, e.data.v);
});

const ready = Reveal.isReady() ? Promise.resolve() : new Promise<void>((r) => Reveal.on("ready", () => r()));
ready.then(connect);
