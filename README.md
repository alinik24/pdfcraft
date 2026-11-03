# PdfCraft: PDF and HTML review with Hermes Copilot

An unofficial development fork of [storytold/pdfcraft](https://github.com/storytold/pdfcraft), maintained by [@alinik24](https://github.com/alinik24).
PdfCraft's original authors and contributors provide the Rust PDF editor. This fork adds native HTML annotation and editing through that existing editor.

Open HTML documents alongside PDFs, select text, highlight passages, add comments and replies, draw, edit the page revision, undo changes and save the review inside the HTML file. Agents can extract structured review JSON without an application-local database.

**Windows prerelease:** [v0.13.0-html-review.5](https://github.com/alinik24/pdfcraft/releases/tag/v0.13.0-html-review.5), source `f9e7dbd7e99712ea28c476b367ba923895949a5b`. Local packaged checks passed. The second-machine public download, checksum, installation and core checks passed, but its live whole-document Copilot test failed while attempting a parser dependency installation. Full `.5` acceptance is incomplete; existing `.4` installations are retained. These are automated checks, not an independent human or clean-OS installation test. Voice, embedded browsing and semantic design editing remain future work.

[Agent tools and structured extraction: development status](docs/agent-tools.md).

## Download and connect

Download the portable ZIP and `SHA256SUMS` from the release above, verify the checksum and extract the complete package. Run **pdfcraft.exe > All tools > AI assistant**. Use **Choose connection file**, then **Check connection**, and send a message about an open PDF or HTML. Docker and a reviewed private Hermes runtime/profile are optional requirements for Copilot; ordinary document editing remains available without them. Keys, profiles and memory are never bundled. See [the onboarding guide](docs/assistant.md) and [installation/upgrade instructions](docs/html-installation.md).

For edits, use **Review changes > Apply changes > Save**. Ask for whole-document summaries explicitly; selected text or the current page supplies immediate context, and Hermes can read the full PDF snapshot. Highlights and underlines use exact quoted text. Scanned documents need OCR for text anchors.

`main` mirrors upstream. The default `feat/html-review-distribution` branch carries this fork's release work; tags pin the exact packaged source. The HTML-only upstream PR and the fork Copilot are separate contributions.

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

The development branch connects the existing **AI assistant** entry to one
document-scoped Hermes copilot conversation. It streams answers, asks for individual
operation approvals and offers proposed native edits for review, apply and undo.
Its optional container keeps generated files in a separate folder for each document
under the project. Model routing and credentials remain local. Earlier portable
versions do not include this integration; check the exact release notes before
installing. See [copilot setup, automation and limits](docs/assistant.md).

[The researched architecture proposal](docs/agent-review-architecture.md) describes semantic HTML/SVG, stable object IDs, W3C annotation anchors, source-to-page geometry, optional diagram profiles, a selection-aware copilot, speech adapters and conversion fidelity reports. It is a proposal, not an implementation claim.

## Attribution and support

Original project: [PdfCraft by the ArtCraft team and contributors](https://github.com/storytold/pdfcraft). This fork is independent and is not endorsed or supported by the upstream team. Restricted ArtCraft marks are removed from the distribution branch. Open-source notices and contributor credits are retained.

Report fork-specific problems in [this fork's issue tracker](https://github.com/alinik24/pdfcraft/issues). Proposed upstream changes are submitted separately without fork distribution branding changes.

Code is licensed under [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE). See [NOTICE](NOTICE), [asset attribution](ATTRIBUTION.md) and the existing [contribution rules](AGENTS.md).
