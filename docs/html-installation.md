# Install the PdfCraft HTML annotation contribution

Status: source build instructions; release packaging and installation on a second Windows machine remain pending.
Reviewed 2026-10-26.

This contribution by @alinik24 is based on [PdfCraft by the ArtCraft team and contributors](https://github.com/storytold/pdfcraft).
It adds HTML files to the existing PDF editor. It is not an official upstream release.
The upstream download does not necessarily include this contribution.

## Build on Windows

Use the Rust MSVC toolchain and Visual Studio C++ Build Tools with a Windows SDK.
An installed Microsoft Edge or Chromium browser is required for the first import of
ordinary HTML. Reopening a saved native HTML layer does not require the browser.
Build on a machine with sufficient free memory; `-j 2` limits parallel compilation.

```powershell
git clone --branch feat/html-review-distribution https://github.com/alinik24/pdfcraft.git
cd pdfcraft
cargo build --release --locked -p pdfcraft -p pdfcraft-cli --features pdfcraft-cli/html-review -j 2
.\target\release\pdfcraft.exe C:\documents\review.html
```

The GUI and CLI enable `html-review` by default on this distribution branch.
The explicit feature flag above also works when building the contribution branch.
Use a verified release commit rather than assuming a moving branch always has the
same contents. Release binaries, signatures and checksums must be checked on the
release page when available; this document does not claim an installer has shipped.
Other operating systems and the web build have not been validated for this feature.

Optional Japanese fonts use the existing upstream `CRAFT_FONTS_DIR` build input;
see the root README and asset policy. A build without that input has limited
Japanese font coverage. Models, font bundles and signing certificates are not
implicitly installed by the commands above.

## Open, annotate and move a file

1. Open an ordinary UTF-8 `.html`, `.htm` or `.xhtml` file using File > Open or the executable command above.
2. Use the existing selection, highlight, Comments and editing controls.
3. Save to a new HTML file for the first test, close it and reopen it.
4. Move the saved file to another machine with the same feature build. Its embedded page revision and review metadata travel with it.

The original HTML remains in the file. An ordinary web browser shows that source;
PdfCraft shows the saved fixed-page revision. Edits do not rewrite or reflow the
original DOM. Export a PDF before redaction, sanitizing, password protection or
digital signing. Do not use this HTML container to conceal its original contents.
See [the full limits and package format](html-review.md).

For Windows Open With, choose the newly built `target\release\pdfcraft.exe` explicitly.
An older installed copy may still own the file association. A source build does not
change Windows file associations or replace another installation automatically.

## Extract review metadata for an agent

Save this as `review-steps.json` in the document directory, with `review.html` present:

```json
[
  {"tool":"html_review_inspect","args":{"path":"review.html"}},
  {"tool":"html_review_export","args":{"path":"review.html","out":"review.json"}}
]
```

Run from that directory, replacing the executable path with the build location:

```powershell
C:\tools\pdfcraft\pdfcraft-cli.exe run --root . --script review-steps.json
```

`review.json` contains readable annotations and coordinates. Ordinary document,
comment and save tools operate on the embedded page revision. DOM edit/merge tools
intentionally refuse a native layer. `doc_export_pdf` creates a separate PDF copy.
MCP and GUI control remain opt-in; installing or opening the app starts neither.

## Troubleshooting

- **HTML is absent from Open:** verify that you launched this build, not an older installed executable.
- **No browser found:** install or use the supported Edge/Chromium browser before importing raw HTML.
- **Source changed / damaged layer:** recover an intact copy. The app refuses silent rerendering that could discard saved edits.
- **Missing web content:** scripts and remote resources do not run during static import. Keep supported local assets inside the source directory.
- **Build executable is locked:** save work and close that exact running build before replacing it.

Revision history: 2026-10-26 — initial source installation and review extraction guide.
