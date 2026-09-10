//! The in-app agent: a headless `claude` run that edits the deck through this
//! server's own `/mcp` endpoint. It is deliberately just another MCP client —
//! in review mode its writes become proposals like anyone else's, so there is
//! one code path for "a tool changed the deck" and the author reviews it in the
//! same panel. Events stream to the editor over the deck's WebSocket
//! (`Update::Agent`); the transcript persists per deck (`review.rs`).
//!
//! Needs `SLIDES_MCP_TOKEN` (the loopback MCP config is written at startup) and
//! a `claude` binary with OAuth credentials in `$HOME` — bind-mounted in
//! docker-compose, notes-style. `SLIDES_AGENT_MODEL` picks the model.

use crate::Shared;
use crate::live::Doc;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};

pub struct Agent {
    config: Option<PathBuf>,
    pub model: String,
    pub effort: String,
    running: Mutex<HashMap<String, Child>>,
}

/// Per-message knobs from the Ask form (validated in `api.rs`).
#[derive(Default, Clone)]
pub struct RunOptions {
    pub model: Option<String>,
    pub effort: Option<String>,
}

pub const EFFORTS: [&str; 4] = ["low", "medium", "high", "xhigh"];

impl Agent {
    /// Writes the MCP config pointing at ourselves (mode 0600) when the token is set.
    pub fn new(bind: &str, token: Option<&str>) -> Agent {
        let model = std::env::var("SLIDES_AGENT_MODEL").unwrap_or_else(|_| "claude-opus-5".into());
        // Slide edits are routine work: medium effort keeps turns short. The Ask
        // form can raise it per message.
        let effort = std::env::var("SLIDES_AGENT_EFFORT")
            .ok()
            .filter(|e| EFFORTS.contains(&e.as_str()))
            .unwrap_or_else(|| "medium".into());
        let config = token.and_then(|token| {
            let port = bind.rsplit(':').next().unwrap_or("7100");
            let cfg = json!({ "mcpServers": { "slides": {
                "type": "http",
                "url": format!("http://127.0.0.1:{port}/mcp"),
                "headers": { "Authorization": format!("Bearer {token}") },
            }}});
            let path = std::env::temp_dir().join(format!("slides-mcp-{}.json", std::process::id()));
            std::fs::write(&path, cfg.to_string()).ok()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            Some(path)
        });
        Agent {
            config,
            model,
            effort,
            running: Mutex::new(HashMap::new()),
        }
    }

    pub fn available(&self) -> Result<&PathBuf, String> {
        let cfg = self
            .config
            .as_ref()
            .ok_or("the agent needs SLIDES_MCP_TOKEN (it drives the deck through /mcp)")?;
        if which("claude").is_none() {
            return Err("no `claude` binary on PATH (mount it into the container)".into());
        }
        Ok(cfg)
    }

    pub fn is_running(&self, deck: &str) -> bool {
        self.running.lock().unwrap().contains_key(deck)
    }

    pub fn stop(&self, deck: &str) -> bool {
        self.running
            .lock()
            .unwrap()
            .get_mut(deck)
            .is_some_and(|c| c.start_kill().is_ok())
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(bin))
            .find(|p| p.is_file())
    })
}

const PERSONA: &str = "\
You are the slide-editing agent inside the `slides` app, working on a reveal.js deck with its author, \
who is watching the editor next to you. The `slides` MCP tools are your only way to read or change the deck.

How to work — and work fast; the author is waiting:
- Read once, then write: list_slides for the outline, get_slide only for the slides you will touch. Call get_theme only when you need a layout or schema you have not seen (its examples are the theme's vocabulary). Do not re-read what you already have.
- Make slide-sized changes: replace_slide, insert_slide, delete_slide. Use put_deck only when the author asks for a whole-deck rewrite. When you have several slides to add or change, issue all the write calls in one turn (parallel tool calls) instead of one per turn.
- Finish your turn as soon as the writes are in. Do not call await_review unless the author explicitly asks you to wait; their decisions and comments reach you in their next message (and via get_proposal).
- Every write returns render diagnostics (math errors, stray `$`). Fix anything you introduced.
- Images and PDFs from the web: fetch_asset downloads a URL into the deck (images become `![…](name.png)` right away; PDFs go to the deck's sources). To put a figure from a paper on a slide: pdf_text with `query` to find the page, render_pdf_page without `name` to look at it, then render_pdf_page with `crop` (fractions of the page: x, y, w, h) and a `name` to save the PNG — check the preview it returns, adjust the crop if it clipped the figure — and put `![caption](name.png)` on the slide. Include the figure's caption or a source line.
- Deck syntax: `---` between blank lines starts a slide, `--` a vertical sub-slide, `Note:` starts speaker notes, `<!-- .slide: class=\"…\" -->` sets slide attributes, `$…$` / `$$…$$` are LaTeX (escape `%` as `\\%`).
- Keep the author's voice and structure. Do what was asked; do not restyle or reorganise unasked.

Review mode: when it is on, each write is queued as a *proposal* the author accepts or rejects in the editor — it is not applied until they do. Your writes in one turn form one *changeset* (already opened for you, titled with the request) that the author can accept all at once or step through slide by slide; if a turn does two unrelated things, call open_changeset between them so each can be judged on its own. Give every write a one-sentence `note` saying what changed and why; the author reads it next to the diff. If get_proposal shows comments from the author on earlier proposals, address those first. Re-proposing the same slide replaces your earlier pending proposal for it.

Reply when done with a short summary of what you proposed or changed and anything you want the author to decide. No preamble, no restating the request, no announcing what you are about to do.";

fn label(block: &Value) -> String {
    let name = block["name"].as_str().unwrap_or("tool");
    let tool = name.rsplit("__").next().unwrap_or(name);
    let input = &block["input"];
    let mut bits = Vec::new();
    for key in ["deck", "slide", "after", "theme", "schema"] {
        if let Some(v) = input.get(key) {
            let v = match v {
                Value::String(s) => s.clone(),
                v => v.to_string(),
            };
            bits.push(format!("{key} {v}"));
        }
    }
    if bits.is_empty() {
        tool.to_string()
    } else {
        format!("{tool} · {}", bits.join(", "))
    }
}

/// Starts a run for `deck`; events reach the editor over the socket.
pub fn start(
    app: Shared,
    doc: Doc,
    deck: String,
    message: String,
    context: String,
    opts: RunOptions,
) -> Result<(), String> {
    app.agent.available()?;
    if app.agent.is_running(&deck) {
        return Err("the agent is already working on this deck".into());
    }
    doc.agent_push("user", &message);
    if doc.review() {
        // This turn's writes form one changeset, named after the request.
        doc.open_changeset(&changeset_title(&message), "");
    }
    tokio::spawn(run(app, doc, deck, message, context, opts));
    Ok(())
}

/// The first line of the request, cut to a title's length.
fn changeset_title(message: &str) -> String {
    let line = message.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let mut t: String = line.chars().take(72).collect();
    if t.len() < line.len() {
        t = t.trim_end().to_string() + "…";
    }
    t
}

async fn run(
    app: Shared,
    doc: Doc,
    deck: String,
    message: String,
    context: String,
    opts: RunOptions,
) {
    let emit = |event: Value| doc.broadcast_agent(event);
    let mut resume = doc.agent_session();
    loop {
        let cfg = app.agent.config.clone().unwrap();
        let system = format!(
            "{PERSONA}\n\n## Runtime context (from the slides server)\n- Deck: `{deck}`\n- Review mode: {}\n{context}",
            if doc.review() {
                "on — your writes become proposals"
            } else {
                "off — your writes apply directly"
            }
        );
        let model = opts
            .model
            .clone()
            .unwrap_or_else(|| app.agent.model.clone());
        let effort = opts
            .effort
            .clone()
            .unwrap_or_else(|| app.agent.effort.clone());
        let mut args: Vec<String> = vec![
            "-p".into(), message.clone(),
            "--output-format".into(), "stream-json".into(), "--verbose".into(),
            // Partial events let the panel show "thinking…" / "writing <tool>…" while a turn runs.
            "--include-partial-messages".into(),
            "--model".into(), model.clone(),
            "--effort".into(), effort.clone(),
            // No skills: this run only ever talks to our MCP. (`--bare` would be
            // leaner still, but it refuses OAuth credentials — API key only.)
            "--disable-slash-commands".into(),
            "--mcp-config".into(), cfg.to_string_lossy().into_owned(), "--strict-mcp-config".into(),
            "--allowedTools".into(), "mcp__slides".into(),
            "--disallowedTools".into(), "Bash,Task,Agent,Skill,TodoWrite,NotebookEdit,Write,Edit,Read,Glob,Grep,WebSearch,WebFetch,KillShell,BashOutput".into(),
            "--max-turns".into(), "40".into(),
            "--append-system-prompt".into(), system,
        ];
        if let Some(s) = &resume {
            args.push("--resume".into());
            args.push(s.clone());
        }
        emit(json!({ "kind": "start", "model": model, "effort": effort }));
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let child = Command::new("claude")
            .args(&args)
            .current_dir(&home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("could not start claude: {e}");
                doc.agent_push("error", &msg);
                emit(json!({ "kind": "error", "text": msg }));
                break;
            }
        };
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        app.agent
            .running
            .lock()
            .unwrap()
            .insert(deck.clone(), child);
        let stderr_task = tokio::spawn(async move {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s).await;
            s
        });

        let mut lines = BufReader::new(stdout).lines();
        let mut got_result = false;
        let mut last_text = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(ev) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match ev["type"].as_str() {
                Some("system") if ev["subtype"] == "init" => {
                    if let Some(sid) = ev["session_id"].as_str() {
                        doc.set_agent_session(Some(sid.into()));
                    }
                }
                // Partial chunks (--include-partial-messages): the raw API stream events.
                Some("stream_event") => {
                    let e = &ev["event"];
                    match e["type"].as_str() {
                        Some("content_block_start") => {
                            let b = &e["content_block"];
                            match b["type"].as_str() {
                                Some("thinking") => {
                                    emit(json!({ "kind": "phase", "text": "thinking…" }))
                                }
                                Some("tool_use") => {
                                    let name = b["name"].as_str().unwrap_or("tool");
                                    let tool = name.rsplit("__").next().unwrap_or(name);
                                    emit(
                                        json!({ "kind": "phase", "text": format!("writing {tool}…") }),
                                    );
                                }
                                Some("text") => {
                                    emit(json!({ "kind": "phase", "text": "replying…" }))
                                }
                                _ => {}
                            }
                        }
                        Some("content_block_delta") => {
                            if let Some(t) = e["delta"]["text"].as_str() {
                                emit(json!({ "kind": "delta", "text": t }));
                            }
                        }
                        _ => {}
                    }
                }
                Some("assistant") => {
                    for block in ev["message"]["content"].as_array().into_iter().flatten() {
                        match block["type"].as_str() {
                            Some("text") => {
                                let text = block["text"].as_str().unwrap_or("").trim().to_string();
                                if !text.is_empty() {
                                    last_text = text.clone();
                                    doc.agent_push("assistant", &text);
                                    emit(json!({ "kind": "text", "text": text }));
                                }
                            }
                            Some("tool_use") => {
                                let l = label(block);
                                doc.agent_push("tool", &l);
                                emit(json!({ "kind": "tool", "text": l }));
                            }
                            _ => {}
                        }
                    }
                }
                Some("result") => {
                    got_result = true;
                    if let Some(sid) = ev["session_id"].as_str() {
                        doc.set_agent_session(Some(sid.into()));
                    }
                    let text = ev["result"].as_str().unwrap_or("").trim().to_string();
                    if ev["is_error"].as_bool().unwrap_or(false) {
                        let msg = if text.is_empty() {
                            ev["subtype"].as_str().unwrap_or("error").to_string()
                        } else {
                            text
                        };
                        doc.agent_push("error", &msg);
                        emit(json!({ "kind": "error", "text": msg }));
                    } else if !text.is_empty() && text != last_text {
                        doc.agent_push("assistant", &text);
                        emit(json!({ "kind": "text", "text": text }));
                    }
                }
                _ => {}
            }
        }
        let child = app.agent.running.lock().unwrap().remove(&deck);
        let status = match child {
            Some(mut c) => c.wait().await.ok(),
            None => None,
        };
        let err = stderr_task.await.unwrap_or_default();
        let failed = !got_result && !status.is_some_and(|s| s.success());
        if failed && resume.is_some() {
            // A stale session id (transcript gone) is the usual cause: start fresh once.
            doc.set_agent_session(None);
            resume = None;
            emit(
                json!({ "kind": "tool", "text": "could not resume the conversation; starting a new one" }),
            );
            continue;
        }
        if failed {
            let msg = format!(
                "claude exited without a result{}",
                err.lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .map(|l| format!(": {l}"))
                    .unwrap_or_default()
            );
            doc.agent_push("error", &msg);
            emit(json!({ "kind": "error", "text": msg }));
        }
        break;
    }
    // Whatever this turn proposed is in; the next turn starts its own changeset.
    doc.seal_changesets();
    emit(json!({ "kind": "done" }));
}
