# Optional assistant provider

The provider/request/parser/HTTP foundation is adapted from FBaldo98's PdfCraft
[contribution #551](https://github.com/storytold/pdfcraft/pull/551), source commit
`de1750b80d0dc5032d169fd0ef418867fc7a5041`, under the repository's MIT OR Apache-2.0
licences. Original contributors retain their credit. Related open contribution:
[#182](https://github.com/storytold/pdfcraft/pull/182). This fork does not claim those
contributions have been merged upstream.

The crate has no network transport unless its native-only `http` feature is enabled.
Hosts supply both provider settings and a transport. No endpoint, model, key,
download, background probe or automatic request is included. HTTPS is required
except on loopback. HTTP redirects are refused, requests have a 90-second deadline
and response bodies are capped. Keys are omitted from debug output and redacted
from returned answers/errors. Neither documents nor model responses are executed.

The native panel uses selected text or the current page, shows its source, and
offers explicit copying or adding a review comment. Adding a comment uses the
existing engine permissions, undo history and save pipeline, with a stale-document
check. Selection is a page/text anchor; stable semantic HTML object IDs and DOM
editing are not implemented by this module.

The CLI tools `ai_summarize`, `ai_ask` and `ai_translate` take a document ID and one
1-based page. They require explicit `PDFCRAFT_AI_ENDPOINT` (base URL),
`PDFCRAFT_AI_MODEL` and optionally `PDFCRAFT_AI_API_KEY` in the process environment.
Do not put credentials in tool arguments, scripts, saved HTML, public examples or
packages. GUI connection settings and keys are session-only in this version.
No speech, autonomous editing or model installer is included.
