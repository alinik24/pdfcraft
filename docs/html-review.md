# HTML in the native PdfCraft editor

Last reviewed: 2026-10-26. Status: locally validated; public release and second-machine validation pending.

This HTML annotation contribution builds on [storytold/pdfcraft](https://github.com/storytold/pdfcraft).
See [installation](html-installation.md) for building this feature branch and
[the proposed agent review architecture](agent-review-architecture.md) for future work.
The proposed browser, voice copilot and semantic design editing are not implemented features.

The desktop `html-review` feature is enabled in normal builds. HTML opens through
existing File > Open, command-line/Open With, drag/drop and recent-file paths.
It uses `PdfCraftApp`, its ordinary document tabs, canvas, toolbars, Comments panel,
commands, edit history and save/recovery system. There is no separate HTML workspace,
WebView toolbar, application or console. PDF input keeps its original code paths.

## Editing and saving

First opening a UTF-8 `.html`, `.htm` or `.xhtml` file creates a fixed page layout
with the installed Edge/Chromium browser, then opens those pages in the existing
PDF engine. Selection, highlights, notes/replies, drawing, content editing, page
operations, search, zoom, printing and export use that engine's existing capabilities
and limitations. This is paged editing, not a live DOM/reflow editor. Document
JavaScript, interactive HTML widgets and speech buttons do not execute in this
static editing view. They remain in the retained original source.

**Save and Save As write HTML.** Each save retains the original HTML bytes and
appends one versioned `script#pdfcraft-native-layer[type="application/json"]`:

- `format: "pdfcraft-native-html/1.0"`;
- `source_sha256` and `pdf_sha256` integrity hashes;
- `pdf_base64`: the complete editable page revision, including native edits;
- `annotations`: readable review metadata including text, author, reply references,
  status, colors, page indices, rectangles, markup quads and drawing points.

Agents can extract the JSON without the app. Native annotations describe fixed
page coordinates, not DOM text offsets. Reopening saved HTML validates and opens
its embedded revision without rendering the source again or needing a local database.
The original source remains unchanged; content edits affect the embedded page revision.
An ordinary browser displays the original HTML; PdfCraft displays the edited page
revision and annotations. Moving the HTML file carries its edits.

Corrupt/incomplete layers or externally changed source are rejected rather than
silently regenerating pages and discarding edits. Standard dirty-document prompts,
atomic save validation, undo/redo and recovery snapshots apply.

## Limits

The original source remains in the HTML container. Whole-file redaction, sanitizing,
hidden-information removal and password protection are therefore blocked for HTML;
export a PDF first for those confidentiality operations. A redacted/encrypted page
layer cannot conceal original HTML. Ordinary PDFs retain their existing operations.

Source is limited to 16 MiB, local assets individually to 16 MiB, local asset lookups
to 256, and cumulative resource expansion is bounded. The editable PDF layer is at
most 64 MiB and saved HTML at most 128 MiB. Source scripts, frames, forms, remote
resources and browser requests for arbitrary files are blocked in rendering input.
Supported local assets are read only beneath the source directory and embedded as
data URLs. Rendering uses a disposable browser profile and a 45 second process
deadline. Background-network capture remains a validation item; zero background
traffic has not been audited.

Non-UTF-8 input, script-generated content, unsupported CSS/assets and resources
outside the source directory require separate fidelity work. Initial rendering
requires installed Edge/Chromium; reopening saved layers does not.

## Agent access

Build the existing CLI with `--features html-review`. Standard `doc_open`, `comment_*`,
page/content operations and `doc_save` use native HTML containers.
`doc_export_pdf` writes a PDF-only copy for workflows that require one, without
changing the source document or marking its unsaved HTML edits as saved.
`html_review_inspect`/`html_review_export` extract native review metadata.
DOM-oriented `html_review_edit`/`html_review_merge` refuse native containers because
rewriting source invalidates page anchors. Use the standard document tools instead.

The headless `ReviewDocument` API also supports source-preserving DOM annotation
transport (`pdfcraft-html-review/1.0`) for original HTML without a native layer.
It is not another GUI. See `crates/html-review/README.md` for that schema.
Automation retains read/write root confinement; no service/MCP server starts by default.

## Local build

```powershell
git clone --branch feat/html-learning-review https://github.com/alinik24/pdfcraft.git
cd pdfcraft
cargo build --release -p pdfcraft -p pdfcraft-cli --features pdfcraft-cli/html-review -j 2
.\target\release\pdfcraft.exe C:\path\document.html
```

Revision history: 2026-10-26 — added public installation guidance and separated future copilot work from current capabilities.
2026-10-25 — replaced the separate experimental HTML host with
existing native document/editor/save routes and an embedded portable page layer.
2026-10-24 — initial headless HTML review schema and experimental host.
