//! Deck assets: files fetched from the web, and figures cut out of PDFs.
//!
//! Images live in the deck folder itself — served at `/deck/<name>/<file>`
//! and exported with the deck — so a slide refers to one as `![…](file.png)`.
//! Downloaded PDFs go to `decks/<name>/.sources/`, which is neither served
//! (dotfiles are 404 on the deck router) nor exported. The PDF work shells
//! out to poppler: `pdfinfo`, `pdftotext`, `pdftoppm`.
//!
//! These exist for the MCP tools (`fetch_asset`, `pdf_text`,
//! `render_pdf_page`, `list_assets`): the in-app agent has no web or shell
//! of its own, so "put Figure 1 of this paper on a slide" has to be
//! something the server can do for it. Fetching is limited to public
//! http(s) hosts — the agent must not become a way onto the LAN.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Largest download we accept.
const MAX_BYTES: u64 = 64 << 20;
/// Longest edge of the preview image a render returns inline.
const PREVIEW_PX: f64 = 1000.0;
const SOURCES: &str = ".sources";

// ---------------------------------------------------------------------------
// Names and paths

/// A relative file name safe to create under the deck folder: no `..`, no
/// absolute paths, no dot-prefixed segments, only `[A-Za-z0-9._-]` per
/// segment (anything else becomes `-`). Subdirectories are allowed.
pub fn clean_name(name: &str) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for seg in name.split(['/', '\\']) {
        let seg: String = seg
            .trim()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' })
            .collect();
        let seg = seg.trim_start_matches('.');
        if seg.is_empty() {
            continue;
        }
        out.push(seg);
    }
    if out.as_os_str().is_empty() {
        bail!("empty file name");
    }
    Ok(out)
}

fn with_ext(mut p: PathBuf, ext: &str) -> PathBuf {
    if p.extension().is_none_or(|e| !e.eq_ignore_ascii_case(ext)) {
        let stem = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
        p.set_file_name(format!("{stem}.{ext}"));
    }
    p
}

/// Where a PDF named by the agent lives: `.sources/<file>` (what fetch_asset
/// wrote), or a PDF the author dropped into the deck folder.
fn find_pdf(deck_dir: &Path, file: &str) -> Result<PathBuf> {
    let rel = clean_name(file.trim_start_matches(&format!("{SOURCES}/")))?;
    for candidate in [deck_dir.join(SOURCES).join(&rel), deck_dir.join(&rel)] {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("no such PDF in this deck: {file} (fetch_asset it first, or see list_assets)")
}

/// Paper pages that are viewers, not files: alphaXiv's and arXiv's abstract /
/// viewer URLs are turned into the arXiv PDF they show.
fn normalize(url: reqwest::Url) -> reqwest::Url {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let arxiv_id = |path: &str| -> Option<String> {
        let rest = path.trim_start_matches('/');
        let (kind, id) = rest.split_once('/')?;
        if !matches!(kind, "abs" | "pdf" | "overview" | "html") {
            return None;
        }
        let id = id.trim_end_matches('/').trim_end_matches(".pdf");
        (!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | '-' | '_'))).then(|| id.to_string())
    };
    if matches!(host, "alphaxiv.org" | "arxiv.org" | "export.arxiv.org") {
        if let Some(id) = arxiv_id(url.path()) {
            if let Ok(u) = format!("https://arxiv.org/pdf/{id}").parse() {
                return u;
            }
        }
    }
    url
}

fn base(url: &reqwest::Url) -> String {
    url.path_segments()
        .and_then(|mut s| s.next_back().map(String::from))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "download".into())
}

// ---------------------------------------------------------------------------
// Fetching

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Pdf,
    Image(&'static str),
}

fn sniff(head: &[u8], ctype: &str) -> Option<Kind> {
    if head.starts_with(b"%PDF-") {
        return Some(Kind::Pdf);
    }
    if head.starts_with(b"\x89PNG") {
        return Some(Kind::Image("png"));
    }
    if head.starts_with(b"\xFF\xD8\xFF") {
        return Some(Kind::Image("jpg"));
    }
    if head.starts_with(b"GIF8") {
        return Some(Kind::Image("gif"));
    }
    if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        return Some(Kind::Image("webp"));
    }
    match ctype {
        "application/pdf" => Some(Kind::Pdf),
        "image/svg+xml" => Some(Kind::Image("svg")),
        "image/png" => Some(Kind::Image("png")),
        "image/jpeg" => Some(Kind::Image("jpg")),
        _ if head.trim_ascii_start().starts_with(b"<svg") || head.trim_ascii_start().starts_with(b"<?xml") => Some(Kind::Image("svg")),
        _ => None,
    }
}

/// Only public addresses: nothing on the LAN, the host, or link-local.
async fn check_public(url: &reqwest::Url) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        bail!("only http(s) URLs can be fetched");
    }
    let host = url.host_str().ok_or_else(|| anyhow!("URL has no host"))?;
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".local") || host.ends_with(".internal") {
        bail!("refusing to fetch from {host}");
    }
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<IpAddr> = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("cannot resolve {host}"))?
        .map(|a| a.ip())
        .collect();
    if addrs.is_empty() {
        bail!("cannot resolve {host}");
    }
    for ip in addrs {
        let private = match ip {
            IpAddr::V4(v4) => {
                v4.is_private()
                    || v4.is_loopback()
                    || v4.is_link_local()
                    || v4.is_unspecified()
                    || v4.is_broadcast()
                    || v4.is_documentation()
                    || v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]) // CGNAT
                    || v4.octets()[0] == 0
            }
            IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique local
                    || (v6.segments()[0] & 0xffc0) == 0xfe80 // link local
                    || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_private() || v4.is_loopback() || v4.is_link_local())
            }
        };
        if private {
            bail!("refusing to fetch from {host}: it resolves to a private address ({ip})");
        }
    }
    Ok(())
}

/// Downloads `url` into the deck: images into the deck folder (public,
/// exported), PDFs into `.sources/`. Redirects are followed by hand so each
/// hop is checked against the same public-host rule.
pub async fn fetch(deck_dir: &Path, deck: &str, url: &str, name: Option<&str>) -> Result<Value> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(120))
        .user_agent("slides/1.0 (+https://github.com/kmicinski/slides)")
        .build()?;
    let asked = url.trim().to_string();
    let mut url: reqwest::Url = normalize(asked.parse().context("not a valid URL")?);
    let mut resp = None;
    for _ in 0..8 {
        check_public(&url).await?;
        let r = client.get(url.clone()).send().await.with_context(|| format!("GET {url}"))?;
        if r.status().is_redirection() {
            let loc = r
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow!("{url}: redirect without a Location"))?;
            url = url.join(loc).context("bad redirect target")?;
            continue;
        }
        resp = Some(r);
        break;
    }
    let r = resp.ok_or_else(|| anyhow!("too many redirects"))?;
    if !r.status().is_success() {
        bail!("{url}: HTTP {}", r.status());
    }
    if r.content_length().is_some_and(|n| n > MAX_BYTES) {
        bail!("{url}: larger than the {} MB limit", MAX_BYTES >> 20);
    }
    let ctype = r
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    // Stream to a temp file, capped.
    let tmp = std::env::temp_dir().join(format!("slides-fetch-{}-{:x}", std::process::id(), rand::random::<u64>()));
    let mut out = tokio::fs::File::create(&tmp).await?;
    let mut total: u64 = 0;
    let mut head = Vec::with_capacity(16);
    let mut stream = r;
    let result: Result<()> = async {
        while let Some(chunk) = stream.chunk().await? {
            total += chunk.len() as u64;
            if total > MAX_BYTES {
                bail!("{url}: larger than the {} MB limit", MAX_BYTES >> 20);
            }
            if head.len() < 16 {
                head.extend_from_slice(&chunk[..chunk.len().min(16 - head.len())]);
            }
            out.write_all(&chunk).await?;
        }
        out.flush().await?;
        Ok(())
    }
    .await;
    if let Err(e) = result {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    let Some(kind) = sniff(&head, &ctype) else {
        let _ = tokio::fs::remove_file(&tmp).await;
        bail!("{url} is {}, not a PDF or an image (png, jpg, gif, webp, svg)", if ctype.is_empty() { "of unknown type".to_string() } else { ctype });
    };

    let wanted = name.map(str::trim).filter(|n| !n.is_empty()).map(String::from).unwrap_or_else(|| base(&url));
    let rel = match kind {
        Kind::Pdf => Path::new(SOURCES).join(with_ext(clean_name(&wanted)?, "pdf")),
        Kind::Image(ext) => {
            let p = clean_name(&wanted)?;
            // Keep a matching extension the author chose; otherwise add the real one.
            let keep = p.extension().is_some_and(|e| {
                let e = e.to_ascii_lowercase();
                e == ext || (ext == "jpg" && e == "jpeg")
            });
            if keep { p } else { with_ext(p, ext) }
        }
    };
    let dest = deck_dir.join(&rel);
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if tokio::fs::rename(&tmp, &dest).await.is_err() {
        // Different filesystem: copy instead.
        tokio::fs::copy(&tmp, &dest).await?;
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    let file = rel.to_string_lossy().replace('\\', "/");
    let mut v = json!({ "file": file, "bytes": total, "from": url.as_str() });
    if url.as_str() != asked {
        v["note"] = json!(format!("{asked} is a viewer page; fetched the PDF it shows instead"));
    }
    match kind {
        Kind::Pdf => {
            v["kind"] = json!("pdf");
            v["pages"] = json!(page_count(&dest).await?);
            v["message"] = json!("saved to the deck's sources (not served, not exported). Next: pdf_text with a query to find the page, render_pdf_page to look at it and cut the figure out.");
        }
        Kind::Image(_) => {
            v["kind"] = json!("image");
            v["url"] = json!(format!("/deck/{deck}/{}", v["file"].as_str().unwrap_or_default()));
            v["markdown"] = json!(format!("![]({})", v["file"].as_str().unwrap_or_default()));
            v["message"] = json!("saved in the deck folder: use the markdown on a slide.");
        }
    }
    Ok(v)
}

// ---------------------------------------------------------------------------
// PDFs (poppler)

async fn run(cmd: &str, args: &[String]) -> Result<Vec<u8>> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .await
        .with_context(|| format!("running {cmd} (is poppler-utils installed?)"))?;
    if !out.status.success() {
        bail!("{cmd} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

async fn page_count(pdf: &Path) -> Result<usize> {
    let info = String::from_utf8_lossy(&run("pdfinfo", &[s(pdf)]).await?).into_owned();
    info.lines()
        .find_map(|l| l.strip_prefix("Pages:"))
        .and_then(|n| n.trim().parse().ok())
        .ok_or_else(|| anyhow!("pdfinfo did not report a page count"))
}

/// A page's size in points (1/72 in).
async fn page_size(pdf: &Path, page: usize) -> Result<(f64, f64)> {
    let args = ["-f".into(), page.to_string(), "-l".into(), page.to_string(), s(pdf)];
    let info = String::from_utf8_lossy(&run("pdfinfo", &args).await?).into_owned();
    let line = info
        .lines()
        .find(|l| l.starts_with("Page") && l.contains("size:"))
        .ok_or_else(|| anyhow!("pdfinfo did not report page {page}'s size"))?;
    let nums: Vec<f64> = line
        .split("size:")
        .nth(1)
        .unwrap_or("")
        .split_whitespace()
        .filter_map(|t| t.parse().ok())
        .collect();
    match nums.as_slice() {
        [w, h, ..] => Ok((*w, *h)),
        _ => bail!("cannot parse page size from: {line}"),
    }
}

/// Text of one page, or the pages a query occurs on (with a snippet each).
pub async fn text(deck_dir: &Path, file: &str, page: Option<usize>, query: Option<&str>) -> Result<Value> {
    let pdf = find_pdf(deck_dir, file)?;
    let pages = page_count(&pdf).await?;
    if let Some(p) = page {
        if p < 1 || p > pages {
            bail!("page {p} is out of range: the PDF has {pages} page(s)");
        }
        let args = ["-f".into(), p.to_string(), "-l".into(), p.to_string(), "-layout".into(), s(&pdf), "-".into()];
        let t = String::from_utf8_lossy(&run("pdftotext", &args).await?).into_owned();
        let (t, truncated) = clip(&t, 8000);
        return Ok(json!({ "file": file, "page": p, "pages": pages, "text": t, "truncated": truncated }));
    }
    let all = String::from_utf8_lossy(&run("pdftotext", &[s(&pdf), "-".into()]).await?).into_owned();
    let per_page: Vec<&str> = all.split('\x0c').collect();
    match query.map(str::trim).filter(|q| !q.is_empty()) {
        Some(q) => {
            let ql = q.to_lowercase();
            let hits: Vec<Value> = per_page
                .iter()
                .enumerate()
                .filter_map(|(i, t)| {
                    let tl = t.to_lowercase();
                    let at = tl.find(&ql)?;
                    let start = t[..at].char_indices().rev().nth(200).map(|(i, _)| i).unwrap_or(0);
                    let end = t[at..].char_indices().nth(300).map(|(i, _)| at + i).unwrap_or(t.len());
                    Some(json!({ "page": i + 1, "snippet": t[start..end].split_whitespace().collect::<Vec<_>>().join(" ") }))
                })
                .take(20)
                .collect();
            Ok(json!({ "file": file, "pages": pages, "query": q, "hits": hits }))
        }
        None => {
            let (t, truncated) = clip(per_page.first().copied().unwrap_or(""), 8000);
            Ok(json!({ "file": file, "pages": pages, "page": 1, "text": t, "truncated": truncated, "message": "first page only; pass `page`, or `query` to find where something is" }))
        }
    }
}

fn clip(t: &str, max: usize) -> (String, bool) {
    match t.char_indices().nth(max) {
        Some((i, _)) => (t[..i].to_string(), true),
        None => (t.to_string(), false),
    }
}

/// A region of a page, as fractions of its width and height.
#[derive(Deserialize, Clone, Copy, Debug)]
pub struct Crop {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Crop {
    fn check(&self) -> Result<()> {
        let ok = |v: f64| (0.0..=1.0).contains(&v);
        if !(ok(self.x) && ok(self.y) && ok(self.w) && ok(self.h)) || self.w <= 0.0 || self.h <= 0.0 {
            bail!("crop must be fractions of the page: 0 ≤ x, y < 1 and 0 < w, h ≤ 1");
        }
        if self.x + self.w > 1.0001 || self.y + self.h > 1.0001 {
            bail!("crop runs off the page: x + w and y + h must be ≤ 1");
        }
        Ok(())
    }
}

/// Renders (a region of) a page to PNG at `dpi` with pdftoppm; returns the file's bytes.
async fn render(pdf: &Path, page: usize, size: (f64, f64), crop: Option<Crop>, dpi: f64) -> Result<Vec<u8>> {
    let prefix = std::env::temp_dir().join(format!("slides-render-{}-{:x}", std::process::id(), rand::random::<u64>()));
    let mut args: Vec<String> = vec![
        "-png".into(), "-singlefile".into(),
        "-r".into(), format!("{dpi:.2}"),
        "-f".into(), page.to_string(), "-l".into(), page.to_string(),
    ];
    if let Some(c) = crop {
        let (pw, ph) = (size.0 * dpi / 72.0, size.1 * dpi / 72.0);
        let px = |v: f64| v.round().max(0.0) as u32;
        args.extend([
            "-x".into(), px(c.x * pw).to_string(),
            "-y".into(), px(c.y * ph).to_string(),
            "-W".into(), px(c.w * pw).max(1).to_string(),
            "-H".into(), px(c.h * ph).max(1).to_string(),
        ]);
    }
    args.push(s(pdf));
    args.push(s(&prefix));
    let res = run("pdftoppm", &args).await;
    let out = prefix.with_extension("png");
    let bytes = match res {
        Ok(_) => tokio::fs::read(&out).await.context("pdftoppm wrote no image"),
        Err(e) => Err(e),
    };
    let _ = tokio::fs::remove_file(&out).await;
    bytes
}

/// PNG dimensions from its header.
fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || !bytes.starts_with(b"\x89PNG") {
        return None;
    }
    let be = |i: usize| u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    Some((be(16), be(20)))
}

/// Looks at a page (no `name`), or cuts `crop` of it out to `name`.png in the
/// deck folder. Either way the reply carries a preview image of the result
/// (`_preview_png`, base64 — mcp.rs turns it into an image content block)
/// so the caller can check what it got.
pub async fn render_page(
    deck_dir: &Path,
    deck: &str,
    file: &str,
    page: usize,
    crop: Option<Crop>,
    name: Option<&str>,
    dpi: Option<f64>,
) -> Result<Value> {
    let pdf = find_pdf(deck_dir, file)?;
    let pages = page_count(&pdf).await?;
    if page < 1 || page > pages {
        bail!("page {page} is out of range: the PDF has {pages} page(s)");
    }
    if let Some(c) = &crop {
        c.check()?;
    }
    let size = page_size(&pdf, page).await?;
    let region = crop.map(|c| (size.0 * c.w, size.1 * c.h)).unwrap_or(size);
    // Preview: the longest edge of the region at about PREVIEW_PX pixels.
    let preview_dpi = (PREVIEW_PX * 72.0 / region.0.max(region.1)).clamp(24.0, 300.0);
    let preview = render(&pdf, page, size, crop, preview_dpi).await?;
    let (pw, ph) = png_size(&preview).unwrap_or((0, 0));
    let mut v = json!({
        "file": file,
        "page": page,
        "pages": pages,
        "page_size_pt": [size.0, size.1],
        "crop": crop.map(|c| json!({ "x": c.x, "y": c.y, "w": c.w, "h": c.h })),
        "preview_px": [pw, ph],
        "_preview_png": base64::engine::general_purpose::STANDARD.encode(&preview),
    });
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        None => {
            v["saved"] = json!(false);
            v["message"] = json!("preview only (no `name`). Coordinates for `crop` are fractions of the page: x, y from the top-left, w, h of the region. Call again with `crop` and `name` to save the figure.");
        }
        Some(n) => {
            let dpi = dpi.unwrap_or(200.0).clamp(50.0, 600.0);
            let bytes = render(&pdf, page, size, crop, dpi).await?;
            let rel = with_ext(clean_name(n)?, "png");
            let dest = deck_dir.join(&rel);
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&dest, &bytes).await?;
            let (w, h) = png_size(&bytes).unwrap_or((0, 0));
            let file = rel.to_string_lossy().replace('\\', "/");
            v["saved"] = json!(true);
            v["output"] = json!({ "file": file, "url": format!("/deck/{deck}/{file}"), "bytes": bytes.len(), "px": [w, h], "dpi": dpi });
            v["markdown"] = json!(format!("![]({file})"));
            v["message"] = json!("saved. Check the preview: if the crop clipped the figure or included too much, call again with an adjusted crop and the same name. Then put the markdown on a slide.");
        }
    }
    Ok(v)
}

/// Images in the deck folder and PDFs in its sources.
pub fn list(deck_dir: &Path, deck: &str) -> Result<Value> {
    let mut images = Vec::new();
    let mut pdfs = Vec::new();
    for entry in walkdir::WalkDir::new(deck_dir).max_depth(3).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(deck_dir)?.to_string_lossy().replace('\\', "/");
        let ext = entry.path().extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).unwrap_or_default();
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let hidden = rel.split('/').any(|seg| seg.starts_with('.'));
        if ext == "pdf" && (rel.starts_with(&format!("{SOURCES}/")) || !hidden) {
            pdfs.push(json!({ "file": rel, "bytes": size }));
        } else if !hidden && matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg") {
            images.push(json!({ "file": rel, "url": format!("/deck/{deck}/{rel}"), "markdown": format!("![]({rel})"), "bytes": size }));
        }
    }
    Ok(json!({ "images": images, "pdfs": pdfs }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_confined_to_the_deck_folder() {
        assert_eq!(clean_name("fig1.png").unwrap(), PathBuf::from("fig1.png"));
        assert_eq!(clean_name("figures/fig 1.png").unwrap(), PathBuf::from("figures/fig-1.png"));
        assert_eq!(clean_name("../../etc/passwd").unwrap(), PathBuf::from("etc/passwd"));
        assert_eq!(clean_name("/abs/.hidden/.slides.json").unwrap(), PathBuf::from("abs/hidden/slides.json"));
        assert!(clean_name("...").is_err());
        assert_eq!(with_ext(PathBuf::from("a"), "png"), PathBuf::from("a.png"));
        assert_eq!(with_ext(PathBuf::from("a.PNG"), "png"), PathBuf::from("a.PNG"));
        assert_eq!(with_ext(PathBuf::from("a.pdf"), "png"), PathBuf::from("a.pdf.png"));
    }

    #[test]
    fn sniffing_prefers_magic_bytes() {
        assert!(matches!(sniff(b"%PDF-1.7 ...", "text/html"), Some(Kind::Pdf)));
        assert!(matches!(sniff(b"\x89PNG\r\n", ""), Some(Kind::Image("png"))));
        assert!(matches!(sniff(b"<html>", "text/html"), None));
        assert!(matches!(sniff(b"  <svg xmlns", "text/plain"), Some(Kind::Image("svg"))));
    }

    #[tokio::test]
    async fn private_hosts_are_refused() {
        for u in ["http://localhost/x", "http://127.0.0.1/x", "http://192.168.68.115/x", "http://10.0.0.1/x", "ftp://example.com/x", "http://[::1]/x", "http://169.254.169.254/latest"] {
            assert!(check_public(&u.parse().unwrap()).await.is_err(), "{u}");
        }
    }

    #[test]
    fn viewer_urls_become_the_pdf() {
        let n = |u: &str| normalize(u.parse().unwrap()).to_string();
        assert_eq!(n("https://www.alphaxiv.org/pdf/2603.00991"), "https://arxiv.org/pdf/2603.00991");
        assert_eq!(n("https://www.alphaxiv.org/abs/2603.00991v2"), "https://arxiv.org/pdf/2603.00991v2");
        assert_eq!(n("https://arxiv.org/abs/2603.00991"), "https://arxiv.org/pdf/2603.00991");
        assert_eq!(n("https://arxiv.org/pdf/2603.00991.pdf"), "https://arxiv.org/pdf/2603.00991");
        assert_eq!(n("https://example.com/pdf/x.pdf"), "https://example.com/pdf/x.pdf");
        assert_eq!(n("https://arxiv.org/list/cs.PL/recent"), "https://arxiv.org/list/cs.PL/recent");
    }

    #[test]
    fn crop_bounds() {
        assert!(Crop { x: 0.1, y: 0.2, w: 0.5, h: 0.3 }.check().is_ok());
        assert!(Crop { x: 0.6, y: 0.2, w: 0.5, h: 0.3 }.check().is_err());
        assert!(Crop { x: 0.0, y: 0.0, w: 0.0, h: 0.3 }.check().is_err());
    }
}
