//! HTML container for the normal, paged PdfCraft editor. The original source is
//! retained verbatim; the complete editable page revision and readable review
//! metadata travel inside the HTML, rather than in an application database.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    time::{Duration, Instant},
};

const START: &str = "\n<!-- pdfcraft-native-layer:start -->\n<script id=\"pdfcraft-native-layer\" type=\"application/json\">";
const END: &str = "</script>\n<!-- pdfcraft-native-layer:end -->\n";
const FORMAT: &str = "pdfcraft-native-html/1.0";
pub const MAX_PACKAGE_BYTES: usize = 128 * 1024 * 1024;
const MAX_PDF_BYTES: usize = 64 * 1024 * 1024;
const MAX_ASSET_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Layer {
    format: String,
    source_sha256: String,
    pdf_sha256: String,
    /// Every native edit, including page/content changes, is retained here.
    pdf_base64: String,
    /// Page coordinates are PDF points; pages and annotation indices are zero based.
    annotations: Value,
}

pub fn is_html_name(name: &str) -> bool {
    Path::new(name).extension().and_then(|s| s.to_str()).is_some_and(|s| ["html", "htm", "xhtml"].iter().any(|x| s.eq_ignore_ascii_case(x)))
}

pub fn has_layer(bytes: &[u8]) -> bool {
    bytes.windows(START.len()).any(|s| s == START.as_bytes())
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Decode only. Invalid, stale or incomplete layers are errors, never a request
/// to render again and silently throw away edits.
pub fn unpack_native(bytes: &[u8]) -> Result<(String, Vec<u8>), String> {
    if bytes.len() > MAX_PACKAGE_BYTES {
        return Err("HTML review file exceeds 128 MiB".into());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "HTML requires UTF-8 encoding")?;
    let (source, tail) = text.split_once(START).ok_or("No embedded PdfCraft native layer")?;
    if source.len() > super::MAX_HTML_BYTES || tail.contains(START) {
        return Err("HTML source is too large or its layer is ambiguous".into());
    }
    let json = tail.strip_suffix(END).ok_or("The embedded PdfCraft layer is incomplete; keep the file and recover its previous version")?;
    let layer: Layer = serde_json::from_str(json).map_err(|e| format!("Invalid embedded PdfCraft layer: {e}"))?;
    if layer.format != FORMAT || layer.source_sha256 != hash(source.as_bytes()) {
        return Err("HTML source or layer version changed outside PdfCraft; the saved edit layer was preserved, but cannot safely be reopened against different source".into());
    }
    if layer.pdf_base64.len() > MAX_PDF_BYTES.saturating_mul(4).saturating_div(3).saturating_add(4) {
        return Err("Embedded page layer exceeds 64 MiB".into());
    }
    let pdf = STANDARD.decode(&layer.pdf_base64).map_err(|_| "Invalid base64 page layer")?;
    if pdf.len() > MAX_PDF_BYTES || !pdf.starts_with(b"%PDF-") || layer.pdf_sha256 != hash(&pdf) {
        return Err("Embedded page layer is damaged".into());
    }
    Ok((source.to_owned(), pdf))
}

/// Validated, agent-readable review data, without duplicating the binary layer.
pub fn annotation_metadata(bytes: &[u8]) -> Result<Value, String> {
    let _ = unpack_native(bytes)?;
    let text = std::str::from_utf8(bytes).map_err(|_| "HTML requires UTF-8 encoding")?;
    let (_, tail) = text.split_once(START).ok_or("No embedded PdfCraft native layer")?;
    let json = tail.strip_suffix(END).ok_or("Incomplete embedded PdfCraft layer")?;
    let layer: Layer = serde_json::from_str(json).map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"format":layer.format,"source_sha256":layer.source_sha256,"pdf_sha256":layer.pdf_sha256,"review":layer.annotations}))
}

pub fn save_native(source: &str, pdf: &[u8], annotations: Value) -> Result<Vec<u8>, String> {
    if source.len() > super::MAX_HTML_BYTES || source.contains(START) {
        return Err("HTML source is too large or contains a reserved native layer marker".into());
    }
    if pdf.len() > MAX_PDF_BYTES || !pdf.starts_with(b"%PDF-") {
        return Err("A native HTML layer must contain a PDF of at most 64 MiB".into());
    }
    let layer =
        Layer { format: FORMAT.into(), source_sha256: hash(source.as_bytes()), pdf_sha256: hash(pdf), pdf_base64: STANDARD.encode(pdf), annotations };
    let json = serde_json::to_string(&layer).map_err(|e| e.to_string())?.replace('<', "\\u003c").replace('>', "\\u003e").replace('&', "\\u0026");
    let size = source
        .len()
        .checked_add(START.len())
        .and_then(|n| n.checked_add(json.len()))
        .and_then(|n| n.checked_add(END.len()))
        .ok_or("HTML package size overflow")?;
    if size > MAX_PACKAGE_BYTES {
        return Err("HTML review file exceeds 128 MiB".into());
    }
    let mut output = Vec::with_capacity(size);
    output.extend_from_slice(source.as_bytes());
    output.extend_from_slice(START.as_bytes());
    output.extend_from_slice(json.as_bytes());
    output.extend_from_slice(END.as_bytes());
    Ok(output)
}

pub fn open_native(bytes: &[u8], path: Option<&str>) -> Result<(String, Vec<u8>), String> {
    if has_layer(bytes) {
        return unpack_native(bytes);
    }
    if bytes.len() > super::MAX_HTML_BYTES {
        return Err("HTML source exceeds 16 MiB".into());
    }
    let source = std::str::from_utf8(bytes).map_err(|_| "HTML requires UTF-8 encoding")?.to_owned();
    let pdf = render(&source, path)?;
    Ok((source, pdf))
}

fn asset(root: Option<&Path>, reference: &str) -> Option<String> {
    if reference.starts_with("data:") {
        return Some(reference.to_owned());
    }
    let root = root?;
    let parsed = url::Url::parse("https://pdfcraft.invalid/").ok()?.join(reference).ok()?;
    if parsed.host_str() != Some("pdfcraft.invalid") || !parsed.username().is_empty() || parsed.port().is_some() {
        return None;
    }
    // URL decoding is performed by Url's filesystem conversion, never by a shell.
    let mut file = url::Url::from_directory_path(root).ok()?;
    let reference = reference.split(['?', '#']).next()?;
    if reference.contains(':') || reference.contains('\\') || reference.starts_with('/') {
        return None;
    }
    file = file.join(reference).ok()?;
    let path = file.to_file_path().ok()?.canonicalize().ok()?;
    if !path.starts_with(root) {
        return None;
    }
    let mime = match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "css" => "text/css",
        _ => return None,
    };
    let file = std::fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_ASSET_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_ASSET_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_ASSET_BYTES {
        return None;
    }
    Some(format!("data:{mime};base64,{}", STANDARD.encode(bytes)))
}

struct AssetBudget {
    bytes: Cell<usize>,
    reads: Cell<usize>,
    exhausted: Cell<bool>,
}

fn budget_asset(root: Option<&Path>, reference: &str, budget: &AssetBudget) -> Option<String> {
    if budget.exhausted.get() {
        return None;
    }
    if !reference.starts_with("data:") {
        let Some(left) = budget.reads.get().checked_sub(1) else {
            budget.exhausted.set(true);
            return None;
        };
        budget.reads.set(left);
    }
    let value = asset(root, reference)?;
    let Some(remaining) = budget.bytes.get().checked_sub(value.len()) else {
        budget.exhausted.set(true);
        return None;
    };
    budget.bytes.set(remaining);
    Some(value)
}

fn css_assets(css: &str, root: Option<&Path>, budget: &AssetBudget) -> String {
    css_assets_at(css, root, budget, None)
}

fn css_assets_at(css: &str, root: Option<&Path>, budget: &AssetBudget, stylesheet: Option<&str>) -> String {
    static PATTERN: OnceLock<Option<regex::Regex>> = OnceLock::new();
    let Some(pattern) = PATTERN.get_or_init(|| regex::Regex::new(r#"(?i)url\(\s*['\"]?([^'\")]+)['\"]?\s*\)"#).ok()) else { return String::new() };
    pattern
        .replace_all(css, |capture: &regex::Captures<'_>| {
            let reference = capture.get(1).map(|m| m.as_str().trim()).unwrap_or_default();
            let resolved = stylesheet.filter(|_| !reference.starts_with("data:")).and_then(|base| {
                let origin = url::Url::parse("https://pdfcraft.invalid/").ok()?;
                let joined = origin.join(base).ok()?.join(reference).ok()?;
                (joined.origin() == origin.origin()).then(|| joined.path().trim_start_matches('/').to_owned())
            });
            let value = budget_asset(root, resolved.as_deref().unwrap_or(reference), budget).unwrap_or_else(|| "data:,".into());
            format!("url(\"{value}\")")
        })
        .into_owned()
}

/// Build a static rendering input. Never execute document scripts or resolve
/// browser requests to local files/the network; permitted local assets are read
/// explicitly beneath the original file's directory and embedded as data URLs.
pub fn rendering_html(source: &str, path: Option<&str>) -> Result<String, String> {
    if source.len() > super::MAX_HTML_BYTES {
        return Err("HTML source exceeds 16 MiB".into());
    }
    let budget = AssetBudget {
        bytes: Cell::new(MAX_PACKAGE_BYTES.saturating_sub(source.len().saturating_mul(3))),
        reads: Cell::new(256),
        exhausted: Cell::new(false),
    };
    let root = path.and_then(|p| Path::new(p).parent()).and_then(|p| p.canonicalize().ok());
    let mut clean = ammonia::Builder::default();
    clean
        .add_tags(&[
            "style",
            "section",
            "article",
            "header",
            "footer",
            "nav",
            "main",
            "figure",
            "figcaption",
            "svg",
            "path",
            "g",
            "rect",
            "circle",
            "line",
            "polyline",
            "polygon",
            "text",
            "defs",
            "marker",
        ])
        .rm_clean_content_tags(&["style"])
        .add_generic_attributes(&[
            "id",
            "class",
            "style",
            "lang",
            "dir",
            "title",
            "viewBox",
            "viewbox",
            "d",
            "fill",
            "stroke",
            "stroke-width",
            "cx",
            "cy",
            "r",
            "x",
            "y",
            "x1",
            "x2",
            "y1",
            "y2",
            "width",
            "height",
            "points",
            "transform",
        ])
        .add_url_schemes(&["data"]);
    let dom = scraper::Html::parse_document(source);
    let selector = |s: &str| scraper::Selector::parse(s).map_err(|e| e.to_string());
    let styles = dom.select(&selector("style")?).map(|e| e.text().collect::<String>()).collect::<Vec<_>>().join("\n");
    let links = dom
        .select(&selector("link[rel=stylesheet][href]")?)
        .filter_map(|e| {
            let reference = e.value().attr("href")?;
            let encoded = budget_asset(root.as_deref(), reference, &budget)?;
            let payload = encoded.strip_prefix("data:text/css;base64,")?;
            let css = String::from_utf8(STANDARD.decode(payload).ok()?).ok()?;
            Some(css_assets_at(&css, root.as_deref(), &budget, Some(reference)))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let style = css_assets(&format!("{styles}\n{links}"), root.as_deref(), &budget).replace('<', "\\3c ");
    let body = dom.select(&selector("body")?).next();
    let body_html = body.map(|b| b.inner_html()).unwrap_or_default();
    let sanitized = clean.clean(&body_html).to_string();
    let rewritten = lol_html::rewrite_str(
        &sanitized,
        lol_html::RewriteStrSettings {
            element_content_handlers: vec![
                lol_html::element!("[src]", |el| {
                    if let Some(src) = el.get_attribute("src") {
                        el.set_attribute("src", &budget_asset(root.as_deref(), &src, &budget).unwrap_or_else(|| "data:,".into()))?;
                    }
                    Ok(())
                }),
                lol_html::element!("[style]", |el| {
                    if let Some(style) = el.get_attribute("style") {
                        el.set_attribute("style", &css_assets(&style, root.as_deref(), &budget))?;
                    }
                    Ok(())
                }),
                lol_html::element!("style", |el| {
                    el.remove();
                    Ok(())
                }),
            ],
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())?;
    let language =
        body.and_then(|b| b.value().attr("lang")).or_else(|| dom.select(&selector("html").ok()?).next()?.value().attr("lang")).unwrap_or_default();
    let attrs = body
        .map(|b| {
            b.value()
                .attrs()
                .filter(|(k, _)| ["class", "style", "dir"].contains(k))
                .map(|(k, v)| format!(" {k}=\"{}\"", ammonia::clean_text(&css_assets(v, root.as_deref(), &budget))))
                .collect::<String>()
        })
        .unwrap_or_default();
    let csp = "default-src 'none'; script-src 'none'; style-src 'unsafe-inline'; img-src data:; font-src data:; media-src 'none'; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'";
    let result = format!(
        "<!doctype html><html lang=\"{}\"><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"{csp}\"><style>{style}</style></head><body{attrs}>{rewritten}</body></html>",
        ammonia::clean_text(language)
    );
    if budget.exhausted.get() || result.len() > MAX_PACKAGE_BYTES {
        return Err("Expanded HTML rendering input exceeds 128 MiB".into());
    }
    Ok(result)
}

fn browser() -> Result<PathBuf, String> {
    #[cfg(target_os = "windows")]
    let candidates = [
        std::env::var_os("ProgramFiles(x86)").map(PathBuf::from).map(|p| p.join("Microsoft/Edge/Application/msedge.exe")),
        std::env::var_os("ProgramFiles").map(PathBuf::from).map(|p| p.join("Microsoft/Edge/Application/msedge.exe")),
        std::env::var_os("ProgramFiles").map(PathBuf::from).map(|p| p.join("Google/Chrome/Application/chrome.exe")),
    ];
    #[cfg(not(target_os = "windows"))]
    let candidates = [
        Some(PathBuf::from("/usr/bin/chromium")),
        Some(PathBuf::from("/usr/bin/chromium-browser")),
        Some(PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")),
    ];
    candidates.into_iter().flatten().find(|p| p.is_file()).ok_or_else(|| "Opening a new HTML document requires an installed Microsoft Edge or Chromium browser. Previously saved HTML edit layers reopen without it.".into())
}

fn render(source: &str, path: Option<&str>) -> Result<Vec<u8>, String> {
    let html = rendering_html(source, path)?;
    let temp = tempfile::Builder::new().prefix("pdfcraft-html-").tempdir().map_err(|e| e.to_string())?;
    let input = temp.path().join("document.html");
    let output = temp.path().join("pages.pdf");
    std::fs::write(&input, html).map_err(|e| e.to_string())?;
    let url = url::Url::from_file_path(&input).map_err(|_| "Cannot form local HTML rendering URL")?;
    let mut command = Command::new(browser()?);
    command
        .args([
            "--headless=new",
            "--disable-gpu",
            "--disable-extensions",
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-sync",
            "--disable-domain-reliability",
            "--no-first-run",
            "--no-default-browser-check",
            "--metrics-recording-only",
            "--no-pdf-header-footer",
            "--run-all-compositor-stages-before-draw",
            "--virtual-time-budget=3000",
            "--timeout=20000",
            "--host-resolver-rules=MAP * ~NOTFOUND",
            "--proxy-server=127.0.0.1:9",
        ])
        .arg(format!("--user-data-dir={}", temp.path().join("profile").display()))
        .arg(format!("--print-to-pdf={}", output.display()))
        .arg(url.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().map_err(|e| format!("Cannot start local HTML renderer: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(status)) => return Err(format!("Local HTML renderer exited with {status}")),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Local HTML rendering exceeded 45 seconds; the original file was preserved".into());
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Cannot wait for local HTML renderer: {e}"));
            }
        }
    }
    let file = std::fs::File::open(output).map_err(|e| format!("Local HTML renderer produced no pages: {e}"))?;
    if file.metadata().map_err(|e| e.to_string())?.len() > MAX_PDF_BYTES as u64 {
        return Err("Rendered HTML pages exceed 64 MiB".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_PDF_BYTES as u64 + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_PDF_BYTES || !bytes.starts_with(b"%PDF-") {
        return Err("Local HTML renderer produced invalid or oversized pages".into());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_layer_roundtrip_preserves_source_and_escapes_comment_script() {
        let source = "<!doctype html><h1>Ordinary café HTML</h1><script>original()</script>";
        let pdf = b"%PDF-1.7\nsynthetic layer";
        let package = save_native(source, pdf, serde_json::json!([{"contents":"</script><script>bad()</script>"}])).unwrap();
        assert!(package.starts_with(source.as_bytes()));
        let (restored, pages) = unpack_native(&package).unwrap();
        assert_eq!(source, restored);
        assert_eq!(pdf.as_slice(), pages);
        assert_eq!(open_native(&package, None).unwrap(), (source.into(), pdf.to_vec()));
        let text = String::from_utf8(package).unwrap();
        assert!(!text.contains("\"</script><script>bad"));
    }
    #[test]
    fn damaged_or_stale_layers_are_not_rerendered() {
        let package = save_native("<p>old</p>", b"%PDF-test", Value::Null).unwrap();
        let stale = String::from_utf8(package.clone()).unwrap().replace("<p>old</p>", "<p>new</p>");
        assert!(open_native(stale.as_bytes(), None).is_err());
        assert!(unpack_native(package.get(..package.len() - 3).unwrap()).is_err());
        let corrupt = String::from_utf8(package).unwrap().replace("JVBERi10ZXN0", "JVBERi1iYWQh");
        assert!(unpack_native(corrupt.as_bytes()).is_err());
    }
    #[test]
    fn static_renderer_drops_executable_and_remote_content() {
        let html = rendering_html("<html lang='de'><head><style>p{color:red;background:url(https://remote.invalid/a)}</style></head><body onload='bad()'><script>bad()</script><p>Hello</p><img src='https://remote.invalid/a'><iframe src='file:///secret'></iframe></body></html>", None).unwrap();
        assert!(!html.contains("<script"));
        assert!(!html.contains("onload"));
        assert!(!html.contains("https://remote"));
        assert!(!html.contains("file:///secret"));
        assert!(html.contains("color:red"));
        assert!(html.contains("lang=\"de\""));
    }
    #[test]
    fn local_asset_cannot_escape_document_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("inside");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(dir.path().join("outside.png"), b"private").unwrap();
        std::fs::write(root.join("inside.png"), b"ok").unwrap();
        let root = root.canonicalize().unwrap();
        assert!(asset(Some(&root), "inside.png").is_some());
        assert!(asset(Some(&root), "../outside.png").is_none());
        assert!(asset(Some(&root), "%2e%2e/outside.png").is_none());
        assert!(asset(Some(&root), "file:///outside.png").is_none());
    }

    #[test]
    fn resource_expansion_is_bounded_during_construction() {
        let budget = AssetBudget { bytes: Cell::new(3), reads: Cell::new(1), exhausted: Cell::new(false) };
        assert!(budget_asset(None, "data:larger-than-budget", &budget).is_none());
        assert!(budget.exhausted.get());
        let source = format!("<body>{}</body>", "<img src='missing.png'>".repeat(257));
        assert!(rendering_html(&source, None).is_err());
    }
}
