//! MCP endpoint — JSON-RPC 2.0 over HTTP at `/mcp`, bearer-token gated.
//!
//! `SLIDES_MCP_TOKEN` is the whole check (unset ⇒ the route answers 503); the
//! reverse proxy lets `/mcp` through without its own login so a remote LLM
//! client can connect. Same skeleton as the notes/recipes/fable servers.
//!
//! Tools work at two grains. Whole deck: `deck.md` in, `deck.md` out. Single
//! slide: slides are addressed by their 1-based position in presentation
//! order (columns left to right, each column top to bottom — the "n of m"
//! the player shows), and `replace_slide` / `insert_slide` / `delete_slide`
//! rewrite just that region of the source, under the document's lock, so a
//! tool never has to ship a 2,000-line deck to fix one bullet. Every write
//! goes through the live document like the editor's do: open editors and
//! previews update at once, the deck is rendered immediately and the render
//! diagnostics come back in the reply.
//!
//! In *review mode* (the default; `review.rs`) the write tools do not change the
//! deck: each call queues a proposal the author accepts or rejects in the
//! editor. The reply says so (`proposed: true`), `get_proposal` reports each
//! op's status and the author's comments, and `await_review` waits for them.
//! Writes are grouped into *changesets* the author can accept or reject as a
//! whole: `open_changeset` starts a named one for the writes that follow.
//! Writers are told apart by the `X-Slides-Thread` header the in-app agent's
//! runs send (`agent.rs`); a remote session has none. Two writers' ops on the
//! same slide *conflict* and wait for the author (`review.rs`).

use crate::deck::{self, Deck, Diagnostic, Renderer};
use crate::review::Kind;
use crate::theme::{self, Theme};
use crate::{Shared, api};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs;
use subtle::ConstantTimeEq;

const PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "slides-server";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(serde::Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(serde::Serialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

fn ok(id: Value, result: Value) -> Response {
    axum::Json(JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    })
    .into_response()
}

fn err(id: Value, code: i64, message: impl Into<String>) -> Response {
    axum::Json(JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.into(),
        }),
    })
    .into_response()
}

fn check_bearer(app: &Shared, headers: &HeaderMap) -> Result<(), (StatusCode, &'static str)> {
    let Some(token) = &app.mcp_token else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "MCP disabled: SLIDES_MCP_TOKEN is not set",
        ));
    };
    let provided = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    let same =
        provided.len() == token.len() && bool::from(provided.as_bytes().ct_eq(token.as_bytes()));
    if same {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "invalid bearer token"))
    }
}

/// The agent thread behind a request, from the `X-Slides-Thread: <deck>:<id>`
/// header its MCP config carries (`agent.rs`); remote sessions send none.
fn thread_header(headers: &HeaderMap) -> Option<(String, u32)> {
    let v = headers.get(crate::agent::THREAD_HEADER)?.to_str().ok()?;
    let (deck, id) = v.rsplit_once(':')?;
    Some((deck.to_string(), id.parse().ok()?))
}

/// The thread a write to `deck` is attributed to: the request's thread, if it is that deck's.
fn thread_for(thread: &Option<(String, u32)>, deck: &str) -> Option<u32> {
    thread.as_ref().filter(|(d, _)| d == deck).map(|(_, id)| *id)
}

pub async fn handler(State(app): State<Shared>, headers: HeaderMap, body: String) -> Response {
    if let Err(resp) = check_bearer(&app, &headers) {
        return resp.into_response();
    }
    let thread = thread_header(&headers);
    let req: JsonRpcRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => return err(Value::Null, -32700, format!("parse error: {e}")),
    };
    // Notifications (no id) — ack and return without a body.
    let Some(id) = req.id else {
        return (StatusCode::ACCEPTED, "").into_response();
    };
    match req.method.as_str() {
        "initialize" => ok(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
            }),
        ),
        "ping" => ok(id, json!({})),
        "tools/list" => ok(id, json!({ "tools": tool_catalog() })),
        "tools/call" => match tools_call(&app, req.params, thread).await {
            Ok(v) => ok(id, tool_result(v)),
            Err(msg) => ok(id, tool_error(&msg)),
        },
        "resources/list" => ok(id, json!({ "resources": [] })),
        "prompts/list" => ok(id, json!({ "prompts": [] })),
        m => err(id, -32601, format!("method not found: {m}")),
    }
}

fn tool_text(value: &Value) -> Value {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "<unserializable>".into());
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": value,
        "isError": false
    })
}

/// A tool reply; a `_preview_png` (base64) in the value becomes an image
/// content block the model can look at (see assets::render_page).
fn tool_result(mut value: Value) -> Value {
    let image = value
        .as_object_mut()
        .and_then(|o| o.remove("_preview_png"))
        .and_then(|p| p.as_str().map(String::from));
    let mut r = tool_text(&value);
    if let Some(data) = image {
        r["content"].as_array_mut().unwrap().push(json!({
            "type": "image", "data": data, "mimeType": "image/png"
        }));
    }
    r
}

fn tool_error(msg: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": msg }],
        "isError": true
    })
}

// ---------------------------------------------------------------------------
// Catalog

fn tool_catalog() -> Vec<Value> {
    let deck =
        json!({ "type": "string", "description": "Deck name (its directory under decks/)." });
    let slide = json!({
        "type": "integer",
        "description": "1-based slide position in presentation order (columns left to right, each column top to bottom), as list_slides reports."
    });
    let name = json!({ "type": "string", "description": "Theme name." });
    let schema =
        json!({ "type": "string", "description": "Schema name (the CSS class it styles)." });
    let note = json!({ "type": "string", "description": "One sentence for the author: what changed and why. Shown next to the diff in review mode." });
    let obj = |props: Value, required: &[&str]| json!({ "type": "object", "properties": props, "required": required });
    vec![
        json!({
            "name": "list_decks",
            "description": "Every deck with its title, theme, slide count and diagnostic count. Call this first to orient.",
            "inputSchema": obj(json!({}), &[])
        }),
        json!({
            "name": "get_deck",
            "description": "The full deck.md source of a deck plus its render summary (title, theme, slide count, diagnostics with line numbers). For one slide use get_slide instead.",
            "inputSchema": obj(json!({ "deck": deck }), &["deck"])
        }),
        json!({
            "name": "put_deck",
            "description": "Replace a deck's entire deck.md (creates the deck if it does not exist). Every open editor and preview updates at once. Returns the render diagnostics — read them. Prefer replace_slide for local edits. In review mode this queues a whole-deck proposal.",
            "inputSchema": obj(json!({ "deck": deck, "source": { "type": "string", "description": "Complete deck.md text." }, "note": note }), &["deck", "source"])
        }),
        json!({
            "name": "check_deck",
            "description": "Render deck.md text without saving anything: title, theme, slide count and diagnostics. Use it to validate a draft before put_deck.",
            "inputSchema": obj(json!({ "source": { "type": "string" } }), &["source"])
        }),
        json!({
            "name": "list_slides",
            "description": "Outline of a deck: one entry per slide with its position, column/row, first source line, heading, slide attributes (e.g. class=\"big-point\"), whether it has speaker notes, and its diagnostics.",
            "inputSchema": obj(json!({ "deck": deck }), &["deck"])
        }),
        json!({
            "name": "get_slide",
            "description": "The markdown source of one slide (speaker notes included), optionally with its rendered HTML.",
            "inputSchema": obj(json!({ "deck": deck, "slide": slide, "html": { "type": "boolean", "description": "Also return the rendered HTML and notes (default false)." } }), &["deck", "slide"])
        }),
        json!({
            "name": "replace_slide",
            "description": "Replace the source of one slide. `source` is the slide's markdown only — no `---`/`--` separators; include `<!-- .slide: … -->` attributes and any `Note:` section as part of it. Returns the deck's diagnostics. In review mode the change is queued as a proposal (reply has `proposed: true`).",
            "inputSchema": obj(json!({ "deck": deck, "slide": slide, "source": { "type": "string" }, "note": note }), &["deck", "slide", "source"])
        }),
        json!({
            "name": "insert_slide",
            "description": "Insert a new slide after position `after` (0 = before the first slide). `vertical: true` makes it a sub-slide (`--`) of the slide it follows; otherwise it starts a new column (`---`). `source` is the slide's markdown, without separators.",
            "inputSchema": obj(json!({ "deck": deck, "after": { "type": "integer", "description": "Position of the slide to insert after; 0 inserts at the top." }, "source": { "type": "string" }, "vertical": { "type": "boolean" }, "note": note }), &["deck", "after", "source"])
        }),
        json!({
            "name": "delete_slide",
            "description": "Delete one slide (and the separator that followed it). A deck keeps at least one slide.",
            "inputSchema": obj(json!({ "deck": deck, "slide": slide, "note": note }), &["deck", "slide"])
        }),
        json!({
            "name": "open_changeset",
            "description": "Start a changeset: the writes that follow (replace/insert/delete/put_deck) are grouped under this title, and the author can accept or reject the whole group at once or step through it slide by slide. Call it before a batch of related edits (one per task, not per slide); writes made without one land in an untitled changeset. The changeset closes when you open the next one or the author acts on it.",
            "inputSchema": obj(json!({ "deck": deck, "title": { "type": "string", "description": "A few words naming the batch, e.g. \"Tighten the intro\"." }, "note": { "type": "string", "description": "Optional sentence for the author about the batch as a whole." } }), &["deck", "title"])
        }),
        json!({
            "name": "get_proposal",
            "description": "Review state of a deck: whether review mode is on, its changesets (title, open, thread, counts), and every proposed op with its changeset, status (pending / accepted / rejected / merged), whether it went stale (the author edited that slide), the ids of other writers' pending ops it `conflicts` with (same slide; the author keeps one or has the agent merge them — not yours to resolve), and the author's comment asking for changes. Check this before revising work the author has commented on.",
            "inputSchema": obj(json!({ "deck": deck }), &["deck"])
        }),
        json!({
            "name": "await_review",
            "description": "Wait (up to `timeout` seconds, default 120) until the author acts on the proposal — accepts, rejects or comments — then return the review state. Use it after proposing changes when you want to respond to the author's decisions in the same session.",
            "inputSchema": obj(json!({ "deck": deck, "timeout": { "type": "integer" } }), &["deck"])
        }),
        json!({
            "name": "withdraw_proposal",
            "description": "Withdraw one pending op (`op`), every pending op of one changeset (`changeset`), or every pending op of the deck's proposal when both are omitted.",
            "inputSchema": obj(json!({ "deck": deck, "op": { "type": "integer" }, "changeset": { "type": "integer" } }), &["deck"])
        }),
        json!({
            "name": "fetch_asset",
            "description": "Download a public http(s) URL into the deck. Images (png, jpg, gif, webp, svg) land in the deck folder and the reply gives the markdown to put on a slide. PDFs land in the deck's sources (not served, not exported) for pdf_text / render_pdf_page. `name` is the file name to save as (extension added); default: the URL's last path segment.",
            "inputSchema": obj(json!({ "deck": deck, "url": { "type": "string" }, "name": { "type": "string" } }), &["deck", "url"])
        }),
        json!({
            "name": "pdf_text",
            "description": "Text of a fetched PDF: one page's text (`page`), or — with `query` — the pages the phrase occurs on with a snippet each (e.g. query \"Figure 1\" to find where a figure and its caption are). Without either, the first page.",
            "inputSchema": obj(json!({ "deck": deck, "file": { "type": "string", "description": "As fetch_asset / list_assets reported it." }, "page": { "type": "integer" }, "query": { "type": "string" } }), &["deck", "file"])
        }),
        json!({
            "name": "render_pdf_page",
            "description": "Look at a page of a fetched PDF, or cut a figure out of it. Returns a preview image of the page or region so you can see it. Without `name` nothing is saved — use that to find the figure and pick the crop. With `crop` (fractions of the page: x, y from the top-left corner, w, h of the region, all 0–1) and `name`, saves the region as a PNG in the deck folder (default 200 dpi) and returns the markdown for a slide. Check the preview and re-crop with the same name if it clipped the figure or caught neighbouring text.",
            "inputSchema": obj(json!({ "deck": deck, "file": { "type": "string" }, "page": { "type": "integer" }, "crop": { "type": "object", "properties": { "x": { "type": "number" }, "y": { "type": "number" }, "w": { "type": "number" }, "h": { "type": "number" } }, "required": ["x", "y", "w", "h"] }, "name": { "type": "string", "description": "File name for the PNG, e.g. fig1 or figures/fig1." }, "dpi": { "type": "number" } }), &["deck", "file", "page"])
        }),
        json!({
            "name": "list_assets",
            "description": "Images in the deck folder (with the markdown to use them) and PDFs in its sources.",
            "inputSchema": obj(json!({ "deck": deck }), &["deck"])
        }),
        json!({
            "name": "list_themes",
            "description": "Installed themes with their reveal.js options, stylesheets and schema names. A schema is a slide design (a CSS class plus an example of the markup it styles); get_theme returns the examples.",
            "inputSchema": obj(json!({}), &[])
        }),
        json!({
            "name": "get_theme",
            "description": "One theme in full: reveal options, stylesheets, and every schema with its example markdown — the vocabulary to write slides in that theme.",
            "inputSchema": obj(json!({ "theme": name }), &["theme"])
        }),
        json!({
            "name": "get_schema",
            "description": "One schema's CSS and example markdown.",
            "inputSchema": obj(json!({ "theme": name, "schema": schema }), &["theme", "schema"])
        }),
        json!({
            "name": "put_schema",
            "description": "Create or overwrite a schema: its CSS (styles for the class) and a self-contained example slide using it. Takes effect for players and previews on the next page load.",
            "inputSchema": obj(json!({ "theme": name, "schema": schema, "css": { "type": "string" }, "example": { "type": "string" } }), &["theme", "schema", "css", "example"])
        }),
    ]
}

// ---------------------------------------------------------------------------
// Slide addressing

/// One slide's region of the source. `start..end` is the byte range `deck::split`
/// hands the renderer: it begins with the blank line after the separator
/// (except for the first slide) and ends just before the next separator line
/// (or at the end of the text).
pub(crate) struct Region {
    pub index: usize,
    pub column: usize,
    pub row: usize,
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

pub(crate) fn regions(src: &str) -> Vec<Region> {
    let base = src.as_ptr() as usize;
    let mut out = Vec::new();
    for (c, column) in deck::split(src).into_iter().enumerate() {
        for (r, (line, md)) in column.into_iter().enumerate() {
            let start = md.as_ptr() as usize - base;
            out.push(Region {
                index: out.len() + 1,
                column: c + 1,
                row: r + 1,
                line,
                start,
                end: start + md.len(),
            });
        }
    }
    out
}

fn region(src: &str, slide: usize) -> Result<(Vec<Region>, usize), String> {
    let all = regions(src);
    if slide == 0 || slide > all.len() {
        return Err(format!(
            "no slide {slide}: the deck has {} slide(s)",
            all.len()
        ));
    }
    Ok((all, slide - 1))
}

/// The separator line (`---` or `--`) starting at byte `at`, with its newline.
fn separator(src: &str, at: usize) -> &str {
    let rest = &src[at..];
    &rest[..rest.find('\n').map_or(rest.len(), |n| n + 1)]
}

pub(crate) fn heading(md: &str) -> Option<String> {
    md.lines()
        .map(str::trim)
        .find(|l| l.starts_with('#'))
        .map(|l| l.trim_start_matches('#').trim().to_string())
}

fn diagnostics_in(deck: &Deck, first: usize, last: usize) -> Vec<Diagnostic> {
    deck.diagnostics
        .iter()
        .filter(|d| d.line >= first && d.line <= last)
        .cloned()
        .collect()
}

/// The theme a deck renders with: the one it names, else the first installed.
fn theme_of(app: &Shared, deck: &Deck) -> Option<String> {
    deck.theme.clone().or_else(|| {
        theme::list(&app.root)
            .ok()?
            .into_iter()
            .next()
            .map(|t| t.name)
    })
}

fn summary(deck: &Deck) -> Value {
    json!({
        "title": deck.title,
        "theme": deck.theme,
        "slides": deck.count(),
        "diagnostics": deck.diagnostics,
    })
}

// ---------------------------------------------------------------------------
// Tools

#[derive(Deserialize)]
struct DeckArg {
    deck: String,
}
#[derive(Deserialize)]
struct SlideArg {
    deck: String,
    slide: usize,
    #[serde(default)]
    html: bool,
    #[serde(default)]
    note: String,
}
#[derive(Deserialize)]
struct SourceArg {
    deck: String,
    source: String,
    #[serde(default)]
    note: String,
}
#[derive(Deserialize)]
struct ReplaceArg {
    deck: String,
    slide: usize,
    source: String,
    #[serde(default)]
    note: String,
}
#[derive(Deserialize)]
struct InsertArg {
    deck: String,
    after: usize,
    source: String,
    #[serde(default)]
    vertical: bool,
    #[serde(default)]
    note: String,
}
#[derive(Deserialize)]
struct ThemeArg {
    theme: String,
}
#[derive(Deserialize)]
struct SchemaArg {
    theme: String,
    schema: String,
}
#[derive(Deserialize)]
struct PutSchemaArg {
    theme: String,
    schema: String,
    css: String,
    example: String,
}

#[derive(Deserialize)]
struct FetchArg {
    deck: String,
    url: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct PdfTextArg {
    deck: String,
    file: String,
    #[serde(default)]
    page: Option<usize>,
    #[serde(default)]
    query: Option<String>,
}

#[derive(Deserialize)]
struct RenderArg {
    deck: String,
    file: String,
    page: usize,
    #[serde(default)]
    crop: Option<crate::assets::Crop>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    dpi: Option<f64>,
}

fn parse<T: serde::de::DeserializeOwned>(args: &Value) -> Result<T, String> {
    serde_json::from_value(args.clone()).map_err(|e| format!("bad arguments: {e}"))
}

fn doc(app: &Shared, name: &str) -> Result<crate::live::Doc, String> {
    app.doc(name).ok_or_else(|| format!("no such deck: {name}"))
}

/// A queued proposal, as the tool reply: the op plus the proposed deck's diagnostics.
fn proposed(doc: &crate::live::Doc, op: Value) -> Value {
    let diagnostics = doc
        .proposed()
        .map(|d| d.diagnostics.clone())
        .unwrap_or_default();
    json!({
        "proposed": true,
        "message": "review mode: queued in the current changeset for the author to accept or reject in the editor; nothing changed yet. Slide positions keep referring to the current deck — to add several slides in a row, insert each one after the same slide, in order.",
        "op": op,
        "diagnostics": diagnostics,
    })
}

async fn tools_call(app: &Shared, params: Value, thread: Option<(String, u32)>) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("missing 'name'")?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    match name {
        "list_decks" => {
            let docs = app.docs.lock().unwrap();
            Ok(json!(
                docs.iter()
                    .map(|(name, doc)| {
                        let d = doc.deck();
                        json!({
                            "name": name,
                            "title": d.title,
                            "theme": theme_of(app, &d),
                            "slides": d.count(),
                            "diagnostics": d.diagnostics.len(),
                        })
                    })
                    .collect::<Vec<_>>()
            ))
        }
        "get_deck" => {
            let a: DeckArg = parse(&args)?;
            let doc = doc(app, &a.deck)?;
            let d = doc.deck();
            let mut v = summary(&d);
            v["name"] = json!(a.deck);
            v["theme"] = json!(theme_of(app, &d));
            v["source"] = json!(doc.text());
            Ok(v)
        }
        "put_deck" => {
            let a: SourceArg = parse(&args)?;
            let deck = match app.doc(&a.deck) {
                Some(doc) if doc.review() => {
                    let t = thread_for(&thread, &a.deck);
                    let op = doc.propose(Kind::Deck, None, false, a.source, a.note, t)?;
                    return Ok(proposed(&doc, op));
                }
                Some(doc) => doc.replace(a.source, 0),
                None => app
                    .create(&a.deck, &a.source)
                    .map_err(|e| format!("{e:#}"))?
                    .deck(),
            };
            let mut v = summary(&deck);
            v["name"] = json!(a.deck);
            Ok(v)
        }
        "check_deck" => {
            let src = args
                .get("source")
                .and_then(|v| v.as_str())
                .ok_or("missing 'source'")?;
            Ok(summary(&Renderer::default().render(src)))
        }
        "list_slides" => {
            let a: DeckArg = parse(&args)?;
            let doc = doc(app, &a.deck)?;
            let (text, deck) = (doc.text(), doc.deck());
            let all = regions(&text);
            let slides: Vec<Value> = all
                .iter()
                .map(|r| {
                    let md = &text[r.start..r.end];
                    let last = r.line + md.matches('\n').count();
                    let slide = &deck.columns[r.column - 1][r.row - 1];
                    json!({
                        "slide": r.index,
                        "column": r.column,
                        "row": r.row,
                        "line": r.line,
                        "heading": heading(md),
                        "attrs": slide.attrs,
                        "notes": !slide.notes.is_empty(),
                        "diagnostics": diagnostics_in(&deck, r.line, last),
                    })
                })
                .collect();
            Ok(
                json!({ "name": a.deck, "title": deck.title, "theme": theme_of(app, &deck), "slides": slides }),
            )
        }
        "get_slide" => {
            let a: SlideArg = parse(&args)?;
            let doc = doc(app, &a.deck)?;
            let (text, deck) = (doc.text(), doc.deck());
            let (all, i) = region(&text, a.slide)?;
            let r = &all[i];
            let md = &text[r.start..r.end];
            let last = r.line + md.matches('\n').count();
            let slide = &deck.columns[r.column - 1][r.row - 1];
            let mut v = json!({
                "slide": r.index,
                "column": r.column,
                "row": r.row,
                "line": r.line,
                "source": md.trim_matches('\n'),
                "diagnostics": diagnostics_in(&deck, r.line, last),
            });
            if a.html {
                v["attrs"] = json!(slide.attrs);
                v["html"] = json!(slide.html);
                v["notes"] = json!(slide.notes);
            }
            Ok(v)
        }
        "replace_slide" => {
            let a: ReplaceArg = parse(&args)?;
            let d = doc(app, &a.deck)?;
            if d.review() {
                let t = thread_for(&thread, &a.deck);
                let op = d.propose(Kind::Replace, Some(a.slide), false, a.source, a.note, t)?;
                return Ok(proposed(&d, op));
            }
            let deck = d.update(0, |t| replace_slide(t, a.slide, &a.source))?;
            Ok(summary(&deck))
        }
        "insert_slide" => {
            let a: InsertArg = parse(&args)?;
            let d = doc(app, &a.deck)?;
            if d.review() {
                let t = thread_for(&thread, &a.deck);
                let op = d.propose(Kind::Insert, Some(a.after), a.vertical, a.source, a.note, t)?;
                return Ok(proposed(&d, op));
            }
            let deck = d.update(0, |t| insert_slide(t, a.after, &a.source, a.vertical))?;
            Ok(summary(&deck))
        }
        "delete_slide" => {
            let a: SlideArg = parse(&args)?;
            let d = doc(app, &a.deck)?;
            if d.review() {
                let t = thread_for(&thread, &a.deck);
                let op = d.propose(Kind::Delete, Some(a.slide), false, String::new(), a.note, t)?;
                return Ok(proposed(&d, op));
            }
            let deck = d.update(0, |t| delete_slide(t, a.slide))?;
            Ok(summary(&deck))
        }
        "get_proposal" => {
            let a: DeckArg = parse(&args)?;
            Ok(doc(app, &a.deck)?.state_view())
        }
        "await_review" => {
            let a: DeckArg = parse(&args)?;
            let secs = args
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(120)
                .clamp(1, 600);
            Ok(doc(app, &a.deck)?
                .await_review(std::time::Duration::from_secs(secs))
                .await)
        }
        "open_changeset" => {
            let a: DeckArg = parse(&args)?;
            let d = doc(app, &a.deck)?;
            let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let note = args.get("note").and_then(|v| v.as_str()).unwrap_or("");
            if title.trim().is_empty() {
                return Err("missing 'title'".into());
            }
            if !d.review() {
                return Ok(json!({ "changeset": Value::Null, "message": "review mode is off: writes apply directly, there is nothing to group" }));
            }
            let id = d.open_changeset(title, note, thread_for(&thread, &a.deck), Vec::new());
            Ok(json!({ "changeset": id, "message": "open: writes to this deck now join this changeset" }))
        }
        "withdraw_proposal" => {
            let a: DeckArg = parse(&args)?;
            let d = doc(app, &a.deck)?;
            if let Some(cs) = args.get("changeset").and_then(|v| v.as_u64()) {
                return d.resolve_changeset(cs as u32, crate::review::BatchAction::Withdraw);
            }
            let ids: Vec<u32> = match args.get("op").and_then(|v| v.as_u64()) {
                Some(id) => vec![id as u32],
                None => d.state_view()["ops"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|o| o["status"] == "pending")
                    .filter_map(|o| o["id"].as_u64().map(|i| i as u32))
                    .collect(),
            };
            for id in &ids {
                d.resolve(*id, crate::review::Action::Withdraw)?;
            }
            Ok(json!({ "withdrawn": ids }))
        }
        "fetch_asset" => {
            let a: FetchArg = parse(&args)?;
            doc(app, &a.deck)?;
            let dir = app.root.join("decks").join(&a.deck);
            crate::assets::fetch(&dir, &a.deck, &a.url, a.name.as_deref())
                .await
                .map_err(|e| format!("{e:#}"))
        }
        "pdf_text" => {
            let a: PdfTextArg = parse(&args)?;
            doc(app, &a.deck)?;
            let dir = app.root.join("decks").join(&a.deck);
            crate::assets::text(&dir, &a.file, a.page, a.query.as_deref())
                .await
                .map_err(|e| format!("{e:#}"))
        }
        "render_pdf_page" => {
            let a: RenderArg = parse(&args)?;
            doc(app, &a.deck)?;
            let dir = app.root.join("decks").join(&a.deck);
            crate::assets::render_page(&dir, &a.deck, &a.file, a.page, a.crop, a.name.as_deref(), a.dpi)
                .await
                .map_err(|e| format!("{e:#}"))
        }
        "list_assets" => {
            let a: DeckArg = parse(&args)?;
            doc(app, &a.deck)?;
            let dir = app.root.join("decks").join(&a.deck);
            crate::assets::list(&dir, &a.deck).map_err(|e| format!("{e:#}"))
        }
        "list_themes" => {
            let themes = theme::list(&app.root).map_err(|e| format!("{e:#}"))?;
            Ok(json!(
                themes
                    .iter()
                    .map(|t| json!({
                        "name": t.name,
                        "reveal": t.reveal,
                        "css": t.css,
                        "schemas": t.schemas.iter().map(|s| &s.name).collect::<Vec<_>>(),
                    }))
                    .collect::<Vec<_>>()
            ))
        }
        "get_theme" => {
            let a: ThemeArg = parse(&args)?;
            if !crate::valid_name(&a.theme) {
                return Err("invalid theme name".into());
            }
            let t = Theme::load(&app.root.join("themes").join(&a.theme)).map_err(|e| {
                let names: Vec<String> = theme::list(&app.root)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|t| t.name)
                    .collect();
                format!("{e:#}; installed themes: {}", names.join(", "))
            })?;
            serde_json::to_value(&t).map_err(|e| e.to_string())
        }
        "get_schema" => {
            let a: SchemaArg = parse(&args)?;
            let (css, md) = api::schema_paths(app, &a.theme, &a.schema).map_err(|(_, m)| m)?;
            let css = fs::read_to_string(css).map_err(|_| "no such schema".to_string())?;
            Ok(json!({
                "theme": a.theme,
                "schema": a.schema,
                "css": css,
                "example": fs::read_to_string(md).unwrap_or_default(),
            }))
        }
        "put_schema" => {
            let a: PutSchemaArg = parse(&args)?;
            let (css, md) = api::schema_paths(app, &a.theme, &a.schema).map_err(|(_, m)| m)?;
            fs::create_dir_all(css.parent().unwrap())
                .and_then(|()| fs::write(&css, a.css))
                .and_then(|()| fs::write(&md, a.example))
                .map_err(|e| e.to_string())?;
            Ok(json!({ "theme": a.theme, "schema": a.schema, "written": true }))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Source surgery. Each takes the deck text and returns the new text; the
// separators stay where `deck::split` put them, so a round trip through
// get_slide → replace_slide with the same source leaves the file unchanged.

pub(crate) fn replace_slide(text: &str, slide: usize, source: &str) -> Result<String, String> {
    let (all, i) = region(text, slide)?;
    let r = &all[i];
    let prefix = if r.start == 0 { "" } else { "\n" };
    let suffix = if r.end == text.len() { "\n" } else { "\n\n" };
    Ok(format!(
        "{}{prefix}{}{suffix}{}",
        &text[..r.start],
        source.trim_matches('\n'),
        &text[r.end..]
    ))
}

pub(crate) fn insert_slide(
    text: &str,
    after: usize,
    source: &str,
    vertical: bool,
) -> Result<String, String> {
    let sep = if vertical { "--" } else { "---" };
    let body = source.trim_matches('\n');
    if after == 0 {
        return Ok(format!(
            "{body}\n\n{sep}\n\n{}",
            text.trim_start_matches('\n')
        ));
    }
    let (all, i) = region(text, after)?;
    let r = &all[i];
    Ok(if r.end == text.len() {
        format!("{}\n\n{sep}\n\n{body}\n", text.trim_end_matches('\n'))
    } else {
        // text[..r.end] ends with the blank line before the next separator.
        format!("{}{sep}\n\n{body}\n\n{}", &text[..r.end], &text[r.end..])
    })
}

pub(crate) fn delete_slide(text: &str, slide: usize) -> Result<String, String> {
    let (all, i) = region(text, slide)?;
    if all.len() == 1 {
        return Err("a deck keeps at least one slide; use put_deck to rewrite it".into());
    }
    let r = &all[i];
    Ok(if r.row > 1 || r.end == text.len() {
        // A sub-slide goes with the `--` before it (taking the separator after
        // it would pull the next column under this one); the last slide has
        // nothing after it to take.
        let head = &text[..all[i - 1].end];
        if r.end == text.len() {
            format!("{}\n", head.trim_end_matches('\n'))
        } else {
            format!("{head}{}", &text[r.end..])
        }
    } else {
        // A column top goes with the separator after it, so its sub-slides (if
        // any) move up to the top of the column.
        let cut = r.end + separator(text, r.end).len();
        let rest = &text[cut..];
        if r.start == 0 {
            rest.trim_start_matches('\n').to_string()
        } else {
            format!("{}{rest}", &text[..r.start])
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DECK: &str =
        "<!-- title: T -->\n# A\n\n---\n\n# B\n\n--\n\n# B2\n\n---\n\n```\n---\n```\n# C\n";

    fn outline(text: &str) -> Vec<(usize, usize, Option<String>)> {
        regions(text)
            .iter()
            .map(|r| (r.column, r.row, heading(&text[r.start..r.end])))
            .collect()
    }
    fn h(s: &str) -> Option<String> {
        Some(s.into())
    }

    #[test]
    fn positions_follow_presentation_order() {
        assert_eq!(
            outline(DECK),
            vec![
                (1, 1, h("A")),
                (2, 1, h("B")),
                (2, 2, h("B2")),
                (3, 1, h("C"))
            ]
        );
        assert!(region(DECK, 0).is_err());
        assert!(region(DECK, 5).is_err());
    }

    #[test]
    fn replace_round_trips_and_edits() {
        for i in 1..=4 {
            let r = &regions(DECK)[i - 1];
            let src = DECK[r.start..r.end].trim_matches('\n');
            assert_eq!(replace_slide(DECK, i, src).unwrap(), DECK, "slide {i}");
        }
        let t = replace_slide(DECK, 3, "\n\n# B2 new\n").unwrap();
        assert_eq!(outline(&t)[2], (2, 2, h("B2 new")));
        assert_eq!(t, DECK.replace("# B2", "# B2 new"));
        let t = replace_slide(DECK, 1, "<!-- title: T -->\n# A1").unwrap();
        assert_eq!(t, DECK.replace("# A", "# A1"));
        let t = replace_slide(DECK, 4, "# C1").unwrap();
        assert!(t.ends_with("---\n\n# C1\n"));
    }

    #[test]
    fn insert_everywhere() {
        let t = insert_slide(DECK, 0, "# Z", false).unwrap();
        assert_eq!(outline(&t)[..2], [(1, 1, h("Z")), (2, 1, h("A"))]);
        let t = insert_slide(DECK, 1, "# N", true).unwrap();
        assert_eq!(
            outline(&t)[..3],
            [(1, 1, h("A")), (1, 2, h("N")), (2, 1, h("B"))]
        );
        let t = insert_slide(DECK, 2, "# N", false).unwrap();
        assert_eq!(
            outline(&t),
            vec![
                (1, 1, h("A")),
                (2, 1, h("B")),
                (3, 1, h("N")),
                (3, 2, h("B2")),
                (4, 1, h("C"))
            ]
        );
        let t = insert_slide(DECK, 4, "# N", false).unwrap();
        assert_eq!(outline(&t)[4], (4, 1, h("N")));
        assert!(t.ends_with("# C\n\n---\n\n# N\n"));
        let t = insert_slide(DECK, 4, "# N", true).unwrap();
        assert_eq!(outline(&t)[4], (3, 2, h("N")));
    }

    #[test]
    fn delete_everywhere() {
        let t = delete_slide(DECK, 1).unwrap();
        assert_eq!(
            outline(&t),
            vec![(1, 1, h("B")), (1, 2, h("B2")), (2, 1, h("C"))]
        );
        assert!(t.starts_with("# B\n"));
        let t = delete_slide(DECK, 2).unwrap();
        assert_eq!(
            outline(&t),
            vec![(1, 1, h("A")), (2, 1, h("B2")), (3, 1, h("C"))]
        );
        let t = delete_slide(DECK, 3).unwrap();
        assert_eq!(
            outline(&t),
            vec![(1, 1, h("A")), (2, 1, h("B")), (3, 1, h("C"))]
        );
        let t = delete_slide(DECK, 4).unwrap();
        assert_eq!(
            outline(&t),
            vec![(1, 1, h("A")), (2, 1, h("B")), (2, 2, h("B2"))]
        );
        assert!(t.ends_with("# B2\n"));
        assert!(delete_slide("# only\n", 1).is_err());
    }
}
