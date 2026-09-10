// The editor page: Monaco on the left, the live player in an iframe on the
// right. One WebSocket carries edits out and the text, forwarded edits,
// slide line numbers and diagnostics in (protocol in src/live.rs). The iframe
// has its own socket for slide bodies; this page only tells it which slide
// the cursor is on, and it tells this page which source line was clicked.

import type { ClientMsg, Diagnostic, ServerMsg } from "./protocol.js";
import { socketUrl } from "./protocol.js";
import * as review from "./review.js";

const MONACO = "https://cdnjs.cloudflare.com/ajax/libs/monaco-editor/0.45.0/min/vs";
const EMACS = "https://cdn.jsdelivr.net/npm/monaco-emacs@0.3.0/dist/monaco-emacs";
const statusEl = document.getElementById("status")!;
const preview = document.getElementById("preview") as HTMLIFrameElement;
const status = (s: string) => { statusEl.textContent = s; };

// ---- player iframes -----------------------------------------------------------

/**
 * One player iframe. It remembers which deck and slide it was last told to
 * show and repeats that when the frame says it is `ready` — a message posted
 * before the frame's script runs (it loads reveal.js first, and the proposed
 * pane starts hidden) is simply dropped, and a reloaded frame starts over.
 */
class Player {
  proposed = false;
  at: { h: number; v: number } | null = null;
  constructor(readonly frame: HTMLIFrameElement) {}
  private post(m: unknown) { this.frame.contentWindow?.postMessage(m, location.origin); }
  view(proposed: boolean) { this.proposed = proposed; this.post({ type: "view", proposed }); }
  goto(h: number, v: number) { this.at = { h, v }; this.post({ type: "goto", h, v }); }
  repeat() {
    this.post({ type: "view", proposed: this.proposed });
    if (this.at) this.post({ type: "goto", ...this.at });
  }
}
const players = {
  current: new Player(preview),
  proposed: new Player(document.getElementById("preview2") as HTMLIFrameElement),
};
window.addEventListener("message", (e) => {
  if (e.origin !== location.origin || e.data?.type !== "ready") return;
  for (const p of Object.values(players)) if (p.frame.contentWindow === e.source) p.repeat();
});

/** 32-bit FNV-1a over code points; must match `fnv1a` in src/live.rs. */
function fnv1a(s: string): number {
  let h = 0x811c9dc5;
  for (const ch of s) {
    h ^= ch.codePointAt(0)!;
    h = Math.imul(h, 0x01000193);
  }
  return h >>> 0;
}

// ---- cursor → slide ---------------------------------------------------------

let cols: number[][] = []; // first source line of every slide, by column and row
let shown = "";
let previewProposed = false; // the preview shows the proposed deck: its slides are not ours to follow
let comparing = false; // compare mode: both players are parked on a proposed op

function slideAt(line: number): { h: number; v: number } {
  let h = 0;
  while (h + 1 < cols.length && cols[h + 1][0] <= line) h++;
  let v = 0;
  while (v + 1 < (cols[h]?.length ?? 0) && cols[h][v + 1] <= line) v++;
  return { h, v };
}

function follow(line: number) {
  if (previewProposed || comparing) return;
  const { h, v } = slideAt(line);
  const key = `${h},${v}`;
  if (key !== shown) {
    shown = key;
    players.current.goto(h, v);
  }
}

// ---- editor -----------------------------------------------------------------

require.config({ paths: { vs: MONACO, "monaco-emacs": EMACS } });
require(["vs/editor/editor.main", "monaco-emacs"], (_monaco, emacs) => {
  monaco.editor.defineTheme("solarized-light", {
    base: "vs",
    inherit: true,
    rules: [
      { token: "", foreground: "657b83", background: "fdf6e3" },
      { token: "comment", foreground: "93a1a1", fontStyle: "italic" },
      { token: "keyword", foreground: "859900" },
      { token: "string", foreground: "2aa198" },
      { token: "number", foreground: "d33682" },
      { token: "type", foreground: "b58900" },
      { token: "variable", foreground: "268bd2" },
      { token: "markup.heading", foreground: "cb4b16", fontStyle: "bold" },
      { token: "markup.bold", fontStyle: "bold" },
      { token: "markup.italic", fontStyle: "italic" },
    ],
    colors: {
      "editor.background": "#fdf6e3",
      "editor.foreground": "#657b83",
      "editor.lineHighlightBackground": "#eee8d5",
      "editor.selectionBackground": "#eee8d5",
      "editorCursor.foreground": "#657b83",
      "editorLineNumber.foreground": "#93a1a1",
      "editorLineNumber.activeForeground": "#657b83",
      "editorIndentGuide.background": "#eee8d5",
      "editorWhitespace.foreground": "#eee8d5",
    },
  });
  const editor = monaco.editor.create(document.getElementById("editor")!, {
    value: "",
    language: "markdown",
    theme: "solarized-light",
    fontSize: 14,
    fontFamily: '"JetBrains Mono", "SF Mono", Menlo, Consolas, monospace',
    lineHeight: 1.6,
    lineNumbers: "on",
    wordWrap: "on",
    minimap: { enabled: false },
    scrollBeyondLastLine: true,
    automaticLayout: true,
    tabSize: 2,
    insertSpaces: true,
    renderWhitespace: "selection",
    padding: { top: 12, bottom: 12 },
    cursorSmoothCaretAnimation: "on",
    smoothScrolling: true,
    occurrencesHighlight: "off",
    folding: false,
    quickSuggestions: false,
  });
  new (emacs as MonacoEmacsModule).EmacsExtension(editor).start();
  connect(editor);
});

function connect(editor: monaco.editor.IStandaloneCodeEditor) {
  const model = editor.getModel()!;
  let ws: WebSocket;
  let applyingRemote = false;
  const remote = (f: () => void) => { applyingRemote = true; try { f(); } finally { applyingRemote = false; } };
  const send = (m: ClientMsg) => { if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(m)); };
  const rangeOf = (offset: number, length: number) => {
    const a = model.getPositionAt(offset), b = model.getPositionAt(offset + length);
    return new monaco.Range(a.lineNumber, a.column, b.lineNumber, b.column);
  };
  const markers = (diags: Diagnostic[]) =>
    monaco.editor.setModelMarkers(model, "slides", diags.map((d) => {
      const line = Math.min(d.line, model.getLineCount());
      return { severity: monaco.MarkerSeverity.Error, message: d.message, startLineNumber: line, startColumn: 1, endLineNumber: line, endColumn: model.getLineMaxColumn(line) };
    }));

  const panel = review.init({
    deck: document.body.dataset.deck ?? "",
    gotoLine(line) {
      const last = model.getLineCount();
      const l = Math.min(Math.max(1, line), last);
      editor.setPosition({ lineNumber: l, column: 1 });
      editor.revealLineInCenter(l, monaco.editor.ScrollType.Smooth);
    },
    showProposed(on, at) {
      previewProposed = on;
      document.getElementById("preview-badge")!.hidden = !on;
      document.getElementById("preview-pane")!.classList.toggle("proposed", on);
      players.current.view(on);
      if (on && at) players.current.goto(at.h, at.v);
      if (!on) { shown = ""; follow(editor.getPosition()?.lineNumber ?? 1); }
    },
    compare(op) {
      const pane = document.getElementById("preview-pane")!;
      const bar = document.getElementById("compare-bar")!;
      const pane2 = document.getElementById("pane-proposed")!;
      const overlay = document.getElementById("delete-overlay")!;
      const label1 = document.getElementById("label-current")!;
      comparing = !!op;
      pane.classList.toggle("compare", comparing);
      bar.hidden = pane2.hidden = label1.hidden = !comparing;
      if (!op) {
        overlay.hidden = true;
        if (!previewProposed) { shown = ""; follow(editor.getPosition()?.lineNumber ?? 1); }
        return;
      }
      // Top: the real deck at the slide the op touches (for an insert, the one it follows).
      players.current.view(false);
      if (op.line) {
        const { h, v } = slideAt(op.line);
        shown = `${h},${v}`;
        players.current.goto(h, v);
      } else if (op.kind === "deck" || !op.slide) {
        players.current.goto(0, 0);
      }
      overlay.hidden = op.kind !== "delete";
      // Bottom: the forked deck at where the op's result landed.
      players.proposed.view(true);
      if (op.proposed_col) players.proposed.goto(op.proposed_col - 1, op.proposed_row - 1);
    },
    cursorSlide() {
      const line = editor.getPosition()?.lineNumber;
      if (!line || !cols.length) return null;
      const { h, v } = slideAt(line);
      let slide = v + 1;
      for (let i = 0; i < h; i++) slide += cols[i].length;
      const from = cols[h][v];
      let heading = "";
      for (let l = from; l <= Math.min(from + 6, model.getLineCount()); l++) {
        const t = model.getLineContent(l).trim();
        if (t.startsWith("#")) { heading = t.replace(/^#+\s*/, ""); break; }
      }
      return { slide, heading };
    },
  });

  const handle = (m: ServerMsg) => {
    switch (m.type) {
      case "text":
        if (fnv1a(model.getValue()) !== fnv1a(m.text)) {
          const pos = editor.getPosition();
          remote(() => model.pushEditOperations([], [{ range: model.getFullModelRange(), text: m.text }], () => null));
          if (pos) editor.setPosition(pos);
        }
        status("synced");
        break;
      case "edit":
        remote(() => model.applyEdits(m.changes.map((c) => ({ range: rangeOf(c.offset, c.length), text: c.text }))));
        break;
      case "patch":
        cols = m.cols.map((c) => c.map((s) => s.line));
        markers(m.diags);
        follow(editor.getPosition()?.lineNumber ?? 1);
        break;
      case "saved":
        status(m.error ? `save failed: ${m.error}` : "saved");
        break;
      case "state":
        panel.onState(m.state);
        break;
      case "agent":
        panel.onAgent(m.event);
        break;
    }
  };

  const open = () => {
    ws = new WebSocket(socketUrl());
    ws.onopen = () => send({ type: "sync" });
    ws.onclose = () => { status("disconnected, retrying…"); setTimeout(open, 1000); };
    ws.onmessage = (ev) => handle(JSON.parse(ev.data) as ServerMsg);
  };

  model.onDidChangeContent((e) => {
    if (applyingRemote) return;
    status("editing");
    send({
      type: "edit",
      changes: e.changes.map((c) => ({ offset: c.rangeOffset, length: c.rangeLength, text: c.text })),
      hash: fnv1a(model.getValue()),
    });
  });
  editor.onDidChangeCursorPosition((e) => follow(e.position.lineNumber));
  open();

  // ---- preview click → cursor ------------------------------------------------
  // A slide's own line is the blank line after its separator; land on the
  // first non-blank line at or after the target so the cursor sits on content.
  const flash = editor.createDecorationsCollection();
  let unflash = 0;
  window.addEventListener("message", (e) => {
    if (e.origin !== location.origin || e.data?.type !== "edit") return;
    const last = model.getLineCount();
    let line = Math.min(Math.max(1, Number(e.data.line) || 1), last);
    while (line < last && model.getLineContent(line).trim() === "") line++;
    editor.setPosition({ lineNumber: line, column: model.getLineFirstNonWhitespaceColumn(line) || 1 });
    editor.revealLineInCenter(line, monaco.editor.ScrollType.Smooth);
    editor.focus();
    flash.set([{ range: new monaco.Range(line, 1, line, 1), options: { isWholeLine: true, className: "jump-line" } }]);
    clearTimeout(unflash);
    unflash = window.setTimeout(() => flash.clear(), 900);
  });
}

// ---- pane divider -------------------------------------------------------------

const divider = document.getElementById("divider")!;
divider.addEventListener("pointerdown", (e) => {
  divider.setPointerCapture(e.pointerId);
  preview.style.pointerEvents = "none"; // or the iframe swallows the drag
  const move = (ev: PointerEvent) =>
    document.documentElement.style.setProperty("--editor-width", `${Math.max(240, ev.clientX)}px`);
  const up = () => {
    divider.removeEventListener("pointermove", move);
    divider.removeEventListener("pointerup", up);
    preview.style.pointerEvents = "";
  };
  divider.addEventListener("pointermove", move);
  divider.addEventListener("pointerup", up);
});
