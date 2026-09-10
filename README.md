# slides

A hosted editor for markdown slide decks. You type on the left, the deck
renders on the right as you type, and what you see is exactly the reveal.js
player your students open. One Rust binary; the browser side is a little
TypeScript around Monaco.

```
make            # compiles web/src with tsc, then cargo build
SLIDES_PASSWORD=secret cargo run     # http://127.0.0.1:7100/
```

Decks live in `decks/<name>/deck.md`. Create one from the deck list, or drop a
directory in and restart. Everything on the server is a file you can read and
edit; nothing is in a database.

## How it fits together

```
decks/<name>/deck.md ──▶ deck.rs ──▶ Deck (slides as HTML) ──▶ player.rs ──▶ decks/<name>/index.html
                          ▲                 │                                   ▲ ../../engine, ../../themes/<t>
        edits over WS ────┘                 └──▶ live.rs: positional patches ──▶ browser preview (same page, patched in place)
```

| Layer | Where | What it is |
|---|---|---|
| Engine | `engine/` | reveal.js 5.2 (core, highlight, notes plugins) and KaTeX's CSS + fonts. Vendored, shared by every deck, never edited. |
| Theme | `themes/<name>/` | Stylesheets, fonts, reveal options, a starter deck and **schemas**. See `src/theme.rs`. |
| Deck | `decks/<name>/` | `deck.md` (the source of truth), its images, and the generated `index.html`. |
| Renderer | `src/deck.rs` | `deck.md` → slides. pulldown-cmark for markdown, KaTeX (server-side, cached) for math, diagnostics with line numbers. |
| Player | `src/player.rs`, `src/player.html` | The page students open. Relative links to engine and theme, so the same file works served and from a folder. |
| Live | `src/live.rs`, `web/src/*.ts` | In-memory document per deck, the WebSocket protocol, autosave. |
| App | `src/main.rs`, `pages.rs`, `auth.rs`, `api.rs` | Routes, the three pages, the password gate, the tool API. |

Each source file starts with a comment that explains its part; read those
before the code. Line counts are small on purpose — about 1,800 lines of Rust
and 300 of TypeScript for the whole thing.

### Why the server renders

The course's previous pipeline let reveal's markdown plugin render in the
browser and hid math from it with a base64 trick. Here one renderer, in Rust,
produces the HTML for both the live preview and the exported deck, so the
preview *is* the artifact. A real parser knows where code is, so math never
needs hiding; KaTeX runs on the server, so formulas never pop in after the
slide appears; and the player needs no markdown plugin at all. Rendering a
whole 2,000-line deck takes a few milliseconds, so every keystroke re-renders
everything — no incremental bookkeeping to get wrong.

Parity with the old pipeline was checked slide by slide across all twenty
course decks (818 slides). Three conventions differ, each flagged by a
diagnostic in the editor or cosmetic:

- inside a table cell, a `|` in math must be written `\|`;
- no space just inside `$…$` (`$x = $` is not math);
- bare URLs are not auto-linked — use `<https://…>` or `[text](url)`.

### Why the preview never flickers

The preview iframe is the player page with an empty slide container plus
`live.js`. It keeps one reveal.js instance alive for the life of the page. The
server sends *positional* patches — for each (column, row) the slide's first
source line, and a body only where that position's HTML changed since the
last patch to that connection. The client swaps changed bodies in place, so
the `<section>` you are editing is the same DOM element before and after
every keystroke; reveal only re-syncs when the number of slides changes.
The preview follows your cursor: the editor maps the cursor line to a slide
using the line numbers in each patch. Open `/deck/<name>/live` on its own
for a live view on a second screen.

### Text ownership

The server holds the authoritative text of every deck. The editor sends
Monaco's deltas with a hash of the result; a mismatch is refused and answered
with the full text, which heals any drift. Tools replace the whole text
through the API and every open editor and preview updates at once. The deck
is written to disk one second after the last change; the status in the
header says so.

## Themes and schemas

```
themes/cis400/
  theme.toml        [reveal] options, passed to Reveal.initialize() as-is
  base.css          what markdown produces on its own: type, lists, code, tables, chrome
  highlight.css     code colours (highlight.js classes)
  fonts/            referenced from the CSS by relative URL
  starter.md        what a new deck starts from
  schemas/
    callout.css     one slide schema: the styles …
    callout.md      … and a self-contained example of the markup it styles
```

A **schema** is any class an author writes by hand — `title-slide`,
`section-divider`, `big-point`, `two-col`, `callout`, `stat`, `source`,
`playground`, `footer` in the CIS400 theme. Adding a slide design means adding
one CSS file and one example; nothing is compiled. The API lists schemas with
their examples so a tool driving the deck knows the vocabulary, and can write
new ones (say, from a photo of a Keynote slide). A deck picks its theme with
`<!-- theme: cis400 -->`; without it the first theme is used. Reload the
editor page after changing a theme.

## Writing decks

reveal's markdown conventions, unchanged from the course decks:
`---` between blank lines starts a slide, `--` a vertical sub-slide, `Note:`
starts speaker notes, `<!-- .slide: class="big-point" -->` sets attributes on
the slide, `<!-- title: … -->` names the deck. `$…$` and `$$…$$` are LaTeX,
anywhere but code — including inside raw HTML blocks such as callouts, and
across lines even when a continuation line starts with `+` or `=`. Fenced code
is highlighted by language; untagged fences are auto-detected, as before.
Math errors and stray `$` show up as markers in the editor.

## API for tools

Authenticate with the session cookie or `Authorization: Bearer <SLIDES_PASSWORD>`.

```
GET  /api/decks                            [{name, title}]
GET  /api/decks/{name}                     deck.md
PUT  /api/decks/{name}      ← deck.md      {slides, diagnostics}   creates the deck if new
GET  /api/themes                           [{name, reveal, css, schemas: [{name, example}]}]
GET  /api/themes/{t}/schemas/{s}           {css, example}
PUT  /api/themes/{t}/schemas/{s} ← {css, example}
```

A refactoring session with an LLM is: `GET` the deck, edit, `PUT` it back,
read the diagnostics, look at the preview. Players are public at
`/deck/<name>/`; `/deck/<name>/export.zip` is the self-contained folder
(deck + engine + theme) — open `slides/decks/<name>/index.html` from it, and
print to PDF with `?print-pdf` in Chrome, as with any reveal deck.

## MCP endpoint

`POST /mcp` is a JSON-RPC 2.0 [MCP](https://modelcontextprotocol.io) server —
the same API for a Claude Code session on another machine. It is gated by
`Authorization: Bearer $SLIDES_MCP_TOKEN` alone (unset ⇒ the route answers
503), so a proxy that logs users in for everything else should pass `/mcp`
through untouched. Tools (`src/mcp.rs`):

| Tool | What it does |
|---|---|
| `list_decks` | every deck: title, theme, slide and diagnostic counts |
| `get_deck` / `put_deck` | whole `deck.md` in and out (`put_deck` creates a deck too) |
| `check_deck` | render a draft without saving: slide count + diagnostics |
| `list_slides` | outline: position, column/row, first line, heading, attrs, notes?, diagnostics |
| `get_slide` | one slide's source (and, on request, its HTML) |
| `replace_slide` / `insert_slide` / `delete_slide` | edit one slide by position; separators are managed for you |
| `list_themes` / `get_theme` | themes and their schemas with examples — the vocabulary |
| `get_schema` / `put_schema` | one schema's CSS + example; write to add a slide design |
| `fetch_asset` | download a public URL into the deck: images go on slides as `![](file)`, PDFs into the deck's sources |
| `pdf_text` / `render_pdf_page` | find where a figure is in a fetched PDF, look at the page (the reply carries a preview image), and cut a region out to a PNG in the deck folder |
| `list_assets` | images in the deck folder and PDFs in its sources |
| `open_changeset` | review mode: group the writes that follow under a title the author can accept or reject in one go |
| `get_proposal` / `await_review` / `withdraw_proposal` | review mode: see each changeset and op's status and the author's comments, wait for a decision, take an op or a changeset back |

Assets: a deck folder is public, so images the tools save there are served
at `/deck/<name>/<file>` and go into `export.zip`; slides use them as
`![caption](file.png)`. Downloaded PDFs live in `decks/<name>/.sources/`,
which is neither served (dotfiles are 404 on the deck router — that also
keeps `.slides.json` private) nor exported. Fetching is limited to public
http(s) hosts: nothing on the LAN or the host itself. The PDF tools need
poppler (`pdfinfo`, `pdftotext`, `pdftoppm`; the image installs
`poppler-utils`). The in-app agent knows the routine — fetch the paper,
`pdf_text` for "Figure 1", look at the page, crop, check the preview, put the
markdown on a slide.

Slides are addressed by their 1-based position in presentation order (the
"n of m" the player shows). Slide edits are spliced into the source under the
document's lock, so they cannot race an editor's keystrokes; like `PUT
/api/decks/{name}`, every write re-renders, pushes to open editors and
previews, and returns the diagnostics. Connect from Claude Code with

```
claude mcp add --transport http slides https://slides.example.com/mcp \
  --header "Authorization: Bearer $SLIDES_MCP_TOKEN"
```

## Review mode and the in-app agent

Tool edits — over the API, the MCP endpoint, or from the agent — are
**proposals** until the author accepts them. Every deck has a *review tool
edits* switch in the editor header (on by default; `src/review.rs`):

- A proposal is a list of per-slide operations (replace / insert / delete, or a
  whole-deck replace). Each is anchored to the slide it targets by position
  **and** a hash of that slide's source, so edits elsewhere in the deck leave it
  valid, while editing the targeted slide makes it *stale* (reject or
  re-propose; never merged).
- Operations are grouped into **changesets** — one batch of related edits.
  Each ✦ Ask turn is a changeset titled with the request; a remote session
  calls `open_changeset` before a batch (writes made without one land in an
  untitled changeset). A changeset is *open* while its tool is still writing
  and seals when the author acts on it, when the tool opens the next one, or
  when the Ask run ends. The author can **accept all** or **reject all** from
  the changeset's header, or **review** it slide by slide. Accepting all
  applies the ops in order and commits once; any that went stale stay pending
  and are reported.
- A proposal is reviewed as a **fork of the deck**, not as text. The editor's
  **Review** tab lists each changeset's operations as cards with the tool's
  `note` and a rendered thumbnail of the proposed slide (`/deck/<name>/thumb`,
  a chrome-less one-slide player); the line diff is there too, folded away.
  Selecting a card puts the preview into **compare** mode: the current deck
  above and the proposed deck below, both parked on that slide, with prev /
  next (within the changeset) / accept / accept all / reject / comment in a
  bar (keys `n` `p` `a` `A` `r` `c`, `esc` to leave). The lower player is the whole forked deck — arrow around it; its
  added and changed slides carry a dashed outline and a "proposed" badge
  (`data-proposed` on the section, set only on the live proposed rendering).
  A deletion shows as a red overlay on the current slide. "Show proposed deck
  in the preview" browses the fork in the single preview instead.
  Comments are for the agent: `get_proposal` returns them, `await_review`
  blocks until the author acts, `withdraw_proposal` takes an op or a whole
  changeset back.
- Accepting splices the change through the same code the direct tools use;
  nothing else in the proposal moves. State lives in `decks/<name>/.slides.json`.

**✦ Ask** opens a chat with an agent that edits the deck for you
(`src/agent.rs`). It is a headless `claude -p` run whose only tools are this
server's own MCP endpoint over loopback, so in review mode its edits arrive as
proposals in the same panel; the cursor's slide is passed as context. One
conversation per deck, resumed across messages (`--resume`; transcripts under
`$HOME/.claude`). It needs `SLIDES_MCP_TOKEN`, a `claude` binary on `PATH`
with OAuth credentials in `$HOME`, and picks its model from
`SLIDES_AGENT_MODEL` (default `claude-opus-5`) and its thinking effort from
`SLIDES_AGENT_EFFORT` (default `medium`; slide edits are routine work and
higher effort mostly buys minutes of thinking). Both can be overridden per
message from the Ask form. The run passes `--include-partial-messages` so the
panel can show what the model is doing (thinking, writing which tool,
replying) with an elapsed timer, and streams the reply as it is written.
Endpoints in `src/api.rs`.

Two things that keep agent turns short: consecutive `insert_slide` calls
after the *same* slide chain in order (so "add three slides after 14" is three
inserts after 14, no arithmetic on positions that do not exist yet), and
`list_slides` / `get_deck` report the theme the deck actually renders with,
so the agent can go straight to `get_theme`.

## Running it for real

```
SLIDES_PASSWORD=… docker compose up -d --build
```

binds 127.0.0.1:7100; put a reverse proxy with TLS in front. The password
gates editing and the API; players are public. Sessions live in memory, so a
restart logs everyone out. Environment: `SLIDES_ROOT` (default `.`),
`SLIDES_BIND` (default `127.0.0.1:7100`), `SLIDES_PASSWORD` (unset ⇒ read-only),
`SLIDES_MCP_TOKEN` (unset ⇒ no MCP), `SLIDES_AGENT_MODEL` (the ✦ Ask agent's model).

Behind a proxy that does its own login (Authelia, oauth2-proxy, …), set
`TRUST_PROXY_AUTH=true` instead of a password: any request carrying a
`Remote-User` header is treated as logged in, and `/login` is never shown. The
proxy must then (a) require login for everything except the public player
paths — `/deck/<name>/` and its assets, `/engine/*`, `/themes/*` — but
including `/deck/<name>/live` and `/deck/<name>/ws`, and (b) strip any
client-supplied `Remote-User` on the paths it lets through anonymously.
Tools reach `/api` with whatever session the proxy accepts.

## Developing

`make test` runs the renderer's unit tests. The browser code is plain
TypeScript compiled by `tsc` (no bundler); Monaco and monaco-emacs load from
CDNs through Monaco's AMD loader, exactly as in the notes app. The Rust
binary embeds `web/dist/*.js`, so run `make web` after touching `web/src`.
