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

## Running it for real

```
SLIDES_PASSWORD=… docker compose up -d --build
```

binds 127.0.0.1:7100; put a reverse proxy with TLS in front. The password
gates editing and the API; players are public. Sessions live in memory, so a
restart logs everyone out. Environment: `SLIDES_ROOT` (default `.`),
`SLIDES_BIND` (default `127.0.0.1:7100`), `SLIDES_PASSWORD` (unset ⇒ read-only).

## Developing

`make test` runs the renderer's unit tests. The browser code is plain
TypeScript compiled by `tsc` (no bundler); Monaco and monaco-emacs load from
CDNs through Monaco's AMD loader, exactly as in the notes app. The Rust
binary embeds `web/dist/*.js`, so run `make web` after touching `web/src`.
