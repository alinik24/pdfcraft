# AI assistant

Implementation under validation on this fork; not included in portable prerelease
`v0.13.0-html-review.2`.

The existing **All tools > AI assistant** entry is the home for this feature.
It uses the current editor and theme. There is no separate document workspace,
coding console, prompt editor or new document tab.

Enable the assistant and enter your chosen model's connection settings. Nothing
is sent just by opening a file or the panel. Choose **Summarize**, **Explain**,
**Ask** or **Translate** to send the selected passage, or the current page when
no text is selected. Select All is refused rather than silently sending only
some cached pages. Scanned pages need Recognize text before text-based questions.
This version does not interpret page images or diagram topology.

The answer shows its source document and page. **Copy** copies it; **Add as comment**
adds it through the existing Comments, undo and save workflow. It does not rewrite
document content automatically. An answer from a changed or different document
cannot be inserted as a comment; ask again against the current version.
For HTML, saving embeds that comment in the existing native HTML review layer.
The original HTML DOM remains unchanged.

The model must speak the OpenAI-compatible chat-completions protocol. This is a
protocol choice, not a requirement to use a particular company's models or buy a
subscription. The app ships no endpoint, account or model. HTTPS is required
except for a server on this computer. Redirects are refused. Requests run off the
UI thread, with a 90-second network deadline, a one-megabyte response cap and
bounded text/output. Cancel discards the response and waits for that one bounded
worker to finish; it does not cancel computation at the provider.

Connection details, keys and answers are session-only. Keys are not saved in app
settings, documents or packages. A generated answer becomes document content only
through an explicit comment action. Model answers are untrusted plain text, with
no shell, script execution, automatic link following or autonomous document edits.
The same explicit process environment as the CLI can prefill the desktop panel;
the assistant still starts disabled until the user enables it.

Headless tools are documented in [the provider module](https://github.com/alinik24/pdfcraft/blob/feat/html-review-distribution/crates/ai/README.md).
They use the same request/parser contract and copying permissions, and advertise
external access to MCP clients. CLI/MCP configuration never starts either service
on normal GUI launch.

## Reused work and scouting

Provider code is adapted, with attribution, from
[upstream contribution #551](https://github.com/storytold/pdfcraft/pull/551).
The open [assistant proposal #182](https://github.com/storytold/pdfcraft/pull/182)
is related work; this fork does not open a competing assistant contribution or
claim either proposal is merged.

The supplied architecture review correctly distinguishes static review from
semantic source editing. Its manifest, stable identity, selection context,
revision checks and reversible action recommendations inform the next stage.
Current anchors are document/page/text, not stable semantic design-object IDs.

[Tiptap](https://tiptap.dev/docs/editor/getting-started/overview) is a web editor;
[draw.io embed mode](https://www.drawio.com/docs/reference/embed-mode/) exchanges
diagram XML through a browser host. Both require an isolated integration study
before adoption in this native Rust application. Neither is installed here.
[Docling](https://docling-project.github.io/docling/) remains a candidate external
structure-extraction adapter, with output fidelity and model licences to evaluate.
No new editor framework, inference engine or model weights are bundled.

Voice, continuous conversation, podcasts, embedded browsing, source-object editing
and general structure-preserving PDF-to-HTML conversion remain future work.
