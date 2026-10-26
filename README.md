# PdfCraft: native HTML review contribution

An unofficial development fork of [storytold/pdfcraft](https://github.com/storytold/pdfcraft), maintained by [@alinik24](https://github.com/alinik24).
PdfCraft's original authors and contributors provide the Rust PDF editor. This fork adds native HTML annotation and editing through that existing editor.

Open HTML documents alongside PDFs, select text, highlight passages, add comments and replies, draw, edit the page revision, undo changes and save the review inside the HTML file. Agents can extract structured review JSON without an application-local database.

**Development status:** native HTML review passed local GUI checks and 23 HTML/PDF checks after public-download installation on a second machine. Interactive GUI use on that second machine remains unverified. See the [Windows portable releases](https://github.com/alinik24/pdfcraft/releases) for each version's assistant support, checksums and validation. Voice, embedded browsing and semantic design editing remain future work.

## Install from source

Windows requires the Rust MSVC toolchain, Visual Studio C++ Build Tools and a Windows SDK. Install Edge/Chromium for initial HTML import.

```powershell
git clone --branch feat/html-review-distribution https://github.com/alinik24/pdfcraft.git
cd pdfcraft
cargo build --release --locked -p pdfcraft -p pdfcraft-cli --features pdfcraft-cli/html-review -j 2
.\target\release\pdfcraft.exe C:\documents\review.html
```

See [installation and troubleshooting](docs/html-installation.md), [HTML review and file format](docs/html-review.md), and [agent automation](crates/automation/README.md).
Use this fork's [releases](https://github.com/alinik24/pdfcraft/releases) for any published packages. An upstream installer may not contain this contribution. Do not assume a development build is signed or that an installer is available.

## How HTML saving works

Saving preserves the original UTF-8 HTML and embeds a complete PDF page revision plus readable annotations in a versioned JSON layer. PdfCraft displays the revised pages; ordinary web browsers display the original HTML. Content edits do not rewrite or reflow the original DOM.

The native editor uses existing PDF tools and their limitations. Source-retaining HTML cannot provide whole-file redaction, sanitizing, password protection or digital signing: export a PDF first. Script-driven websites and remote resources do not run in the static editing view. Source, assets, subprocesses and package sizes are bounded; see the full feature documentation.

## Agent and design work

The CLI supports standard document/comment/edit/save operations, HTML review inspection/export and a separate PDF export. MCP and UI control are opt-in.

The existing **AI assistant** entry now supports
selected-text/current-page summaries, questions and translation, with an explicit
**Add as comment** action. Portable versions before `.3` do not include it.
It is off by default and uses a model you configure;
keys and answers are session-only. See [assistant behavior and limits](docs/assistant.md).

[The researched architecture proposal](docs/agent-review-architecture.md) describes semantic HTML/SVG, stable object IDs, W3C annotation anchors, source-to-page geometry, optional diagram profiles, a selection-aware copilot, speech adapters and conversion fidelity reports. It is a proposal, not an implementation claim.

## Attribution and support

Original project: [PdfCraft by the ArtCraft team and contributors](https://github.com/storytold/pdfcraft). This fork is independent and is not endorsed or supported by the upstream team. Restricted ArtCraft marks are removed from the distribution branch. Open-source notices and contributor credits are retained.

Report fork-specific problems in [this fork's issue tracker](https://github.com/alinik24/pdfcraft/issues). Proposed upstream changes are submitted separately without fork distribution branding changes.

Code is licensed under [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE). See [NOTICE](NOTICE), [asset attribution](ATTRIBUTION.md) and the existing [contribution rules](AGENTS.md).
