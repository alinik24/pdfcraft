# Experimental HTML review core

`ReviewDocument::parse` accepts UTF-8 HTML up to 16 MiB. A learning document's
`script#pdfcraft-manifest[type="application/json"]` is retained verbatim and its
`document_id` supplies the identity. Ordinary HTML needs no manifest: its identity
is `sha256:<source hash>`. No document scripts execute in this crate.

`targets()` maps target IDs to concatenated DOM text, excluding script/style text.
The document-order selector is
`[data-pdfcraft-target-id],p,h1,h2,h3,h4,h5,h6,li,td,th,pre,blockquote,figure,img,div`.
Each match uses its existing data ID, otherwise `html:<zero-based match index>`.
`html:body` is also available. These indices count existing data-ID matches too.
Offsets are Unicode scalar values in this exact text, without whitespace
normalization. Empty targets support a block note at `start=end=0`.
Target inspection is limited to 10000 matches, 32 MiB cumulative target text,
and four million traversal steps to bound nested hostile markup. Runtime preview
copies the HTML document language onto the body unless the body has its own.

Use `replace_annotations` for validated atomic add/update/delete, `saved_html`
for a new file's bytes, and `export_review`/`merge_review` for transport. The caller
owns atomic disk writes, undo/redo and user approval of reviewed changes.
The original HTML is never serialized from the parsed DOM: saving replaces only
the exact annotation carrier or appends one at EOF. All other source bytes,
including images and manifest, survive unchanged. Its content hash omits that
entire carrier and stays stable across repeated saves. JSON escapes prevent an
annotation body from closing its script element.

The carrier uses `format: "pdfcraft-html-review/1.0"`, `document_id`,
`issue_id`, `content_sha256`, and an editable `annotations` array. The source
content SHA-256 excludes the annotation carrier. A derived `web_annotations`
array supplies W3C representations; when provided during merge it must exactly
match the editable annotations. It is regenerated on every export, never retained
as unknown metadata. `issue_id` is the manifest document ID or generic document
ID. No changing clock timestamp is generated during save. Every annotation has `id`,
`target_id`, `exact`, scalar `start`/`end`, optional quote `prefix`/`suffix`,
`body`, `color`, `created`, `reply_to`, and `status` (pending/accepted/rejected).
Unknown annotation and envelope metadata survives. Unknown statuses, conflicting
IDs, wrong identities/hashes/versions, invalid ranges and cyclic replies fail
without changing the document. Merging identical annotations is idempotent.
`Annotation::web_annotation` provides a W3C CssSelector refined by target-local
TextQuoteSelector/TextPositionSelector alternatives, plus `pdfcraft:targetId`.
`ReviewDocument` exports replace generated target CSS values with in-memory
original-source structural selectors; existing explicit data target attributes
retain their selectors. The standalone annotation helper lacks source context
and therefore uses runtime attributes. Structural selectors in malformed markup
can differ between parsers; rich editing requires exactly one source match and
fails safely otherwise. No external service is contacted.

`resolve` tries the supplied original range and context, then requires a unique
quote/context match in the supplied target. Ambiguous and orphaned selections
are explicit; it never selects an arbitrary global occurrence. Changed source
hashes require explicit reanchoring by the caller and cannot be silently merged.
The source editor uses `source_html()` to remove the carrier from editable text,
then `revised_source(&str)` to create a replacement document. Existing annotations
are retained only when their target and quote can be resolved uniquely; orphaned
or ambiguous edits return an actionable error and leave the original unchanged.
A generic document's identity changes with this explicit source revision. An
embedded carrier with an obsolete hash is rejected; it is never silently dropped.
`replace_target_html(target_id, html)` performs a streaming source edit through
Cloudflare's `lol_html`, preserving bytes outside that element's inner content,
then applies the same conservative annotation reanchoring. Replacements are
limited to 64 KiB and the result to 16 MiB. The caller sanitizes rich HTML before
rendering; editing this way does not imply that the HTML is safe to execute.

This is a foundation, not a complete HTML editor or browser sandbox. Preview
rendering must sanitize active content independently. Private user fixtures are
not included in this crate; unit tests use contributor-original synthetic HTML.
Documents containing `plaintext`, parser-mode-dependent `noscript`, unterminated
raw-text elements, or malformed annotation carriers are refused before editing
or saving: the annotation carrier cannot be located and reopened safely.

## Native desktop document integration

With `native`, `native::open_native` uses installed Edge/Chromium to produce a
static paged working revision for the existing PdfCraft engine. `save_native`
retains the original source verbatim and embeds the complete editable PDF layer
and readable annotation metadata inside the HTML. `unpack_native` validates and
opens that layer without executing source or rendering again. `annotation_metadata`
extracts validated review data without returning the PDF base64 field.

This is a fixed-page editor backend, not DOM reflow editing. The desktop uses its
ordinary tabs, tools, Comments panel and save/recovery flow. Original HTML source
stays unchanged; native content/page edits affect the embedded working revision.
An ordinary browser displays original source, while PdfCraft displays its saved
page revision. Confidentiality and digital-signing operations require explicit PDF
export because the retained original HTML lies outside the PDF layer.

The native carrier `script#pdfcraft-native-layer[type="application/json"]` uses
`pdfcraft-native-html/1.0`, source/PDF SHA-256 hashes, `pdf_base64` and `annotations`.
Native annotation coordinates refer to fixed PDF pages; the DOM source-review
schema above remains separate. Never apply DOM review merge/edit to native packages.
Source is capped at 16 MiB, PDF layer at 64 MiB and package at 128 MiB. Resources
are restricted to data URLs and bounded local assets beneath the source directory.
See `docs/html-review.md` for limitations and the actual desktop build command.
