PdfCraft HTML review contribution - unofficial development fork by @alinik24
Based on https://github.com/storytold/pdfcraft
Source and releases: https://github.com/alinik24/pdfcraft

Extract the complete ZIP into a writable folder, then open pdfcraft.exe.
Keep models/, notices, licences and portable.txt beside the executable.
Settings and recovery data use PdfCraftData/ beside this portable copy.
No administrator installation, browser extension, account or background server is required.
Initial import of raw HTML needs installed Microsoft Edge or Chromium.
Saved native HTML layers reopen without a browser or local review database.

File > Open accepts PDF and UTF-8 HTML. Annotate using the existing controls.
Save writes the original HTML plus an editable PDF layer and readable review JSON.
An ordinary browser shows the original HTML, not the revised PDF layer.
This is fixed-page editing. Original source is retained, so export a PDF before
redaction, sanitizing, password protection or digital signing.

See docs/html-review.md and docs/html-installation.md for formats and limits.
Copilot, browser and semantic-design proposals in docs are not shipped features.
CLI use is optional; opening the GUI starts neither MCP nor a control server.
BUILD-INFO.json records the source revision, target and signing status.
Unsigned development binaries are not an official upstream installer.

Check release notes and SHA256SUMS before running a downloaded build.
Keep your documents and any existing installation backed up while testing.
Report fork-specific problems at https://github.com/alinik24/pdfcraft/issues
