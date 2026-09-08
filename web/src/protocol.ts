// Wire types shared by the editor and the live preview. The server side and
// the protocol's description are in src/live.rs.

export interface Edit { offset: number; length: number; text: string }
export interface Body { attrs: string; html: string; notes: string }
export interface PatchSlide { line: number; body?: Body }
export interface Diagnostic { line: number; message: string }

export type ServerMsg =
  | { type: "text"; text: string }
  | { type: "edit"; changes: Edit[] }
  | { type: "patch"; cols: PatchSlide[][]; diags: Diagnostic[] }
  | { type: "saved"; error?: string };

export type ClientMsg =
  | { type: "sync" }
  | { type: "edit"; changes: Edit[]; hash: number };

export function socketUrl(): string {
  const deck = location.pathname.split("/")[2]; // /deck/<name>/live or /edit/<name>
  const name = location.pathname.startsWith("/edit/") ? location.pathname.slice(6) : deck;
  return `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/deck/${name}/ws`;
}
