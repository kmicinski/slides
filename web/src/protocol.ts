// Wire types shared by the editor and the live preview. The server side and
// the protocol's description are in src/live.rs.

export interface Edit { offset: number; length: number; text: string }
export interface Body { attrs: string; html: string; notes: string }
export interface PatchSlide { line: number; body?: Body }
export interface Diagnostic { line: number; message: string }

/** One proposed operation, as src/review.rs reports it. */
export interface OpView {
  id: number;
  kind: "replace" | "insert" | "delete" | "deck";
  slide: number | null;
  line: number | null;
  vertical: boolean;
  source: string;
  current: string | null;
  note: string;
  status: "pending" | "accepted" | "rejected";
  stale: boolean;
  comment: string;
  proposed_slide: number | null;
  proposed_col: number;
  proposed_row: number;
  /** The changeset this op belongs to. */
  changeset: number;
}
/** A batch of ops reviewed together (src/review.rs `Changeset`), with its counts. */
export interface ChangesetView {
  id: number;
  title: string;
  note: string;
  created: number;
  /** Still taking writes from the tool that opened it. */
  open: boolean;
  ops: number;
  /** Pending and mergeable. */
  pending: number;
  stale: number;
  accepted: number;
  rejected: number;
}
export interface AgentMsg { role: string; text: string; ts: number }
export interface StateView {
  review: boolean;
  proposal: { id: string; created: number } | null;
  changesets: ChangesetView[];
  ops: OpView[];
  pending: number;
  agent: { session: string | null; messages: AgentMsg[] };
}
export interface AgentEvent { kind: "start" | "text" | "tool" | "error" | "done" | "phase" | "delta"; text?: string; model?: string; effort?: string }

export type ServerMsg =
  | { type: "text"; text: string }
  | { type: "edit"; changes: Edit[] }
  | { type: "patch"; cols: PatchSlide[][]; diags: Diagnostic[] }
  | { type: "saved"; error?: string }
  | { type: "state"; state: StateView }
  | { type: "agent"; event: AgentEvent };

export type ClientMsg =
  | { type: "sync" }
  | { type: "edit"; changes: Edit[]; hash: number }
  | { type: "view"; proposed: boolean };

export function socketUrl(): string {
  const deck = location.pathname.split("/")[2]; // /deck/<name>/live or /edit/<name>
  const name = location.pathname.startsWith("/edit/") ? location.pathname.slice(6) : deck;
  return `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/deck/${name}/ws`;
}
