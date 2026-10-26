# Agent-readable documents and a selection-aware copilot

Status: design proposal, not implemented functionality. Reviewed 2026-10-26.
Current functionality is described in [HTML review](html-review.md). This proposal
does not change the existing `pdfcraft-native-html/1.0` file format.

## Goal and common format

Keep PdfCraft's existing editor and PDF workflows. Add a portable document contract
that both a person and an agent can inspect, select, review and revise. Use HTML for
semantic text, SVG for diagrams, JSON for metadata, and a PDF rendition for fixed
page fidelity. Do not require every document to be an entity-relationship diagram.
Presentations, notes, website designs and architecture diagrams need different
optional profiles on top of the same small core.

This is an application profile of existing formats, not a claim of a new universal
industry standard. Exporters may adopt it incrementally. Plain HTML and ordinary
PDFs must still open without it. Unknown extension fields must survive editing.

## Proposed exporter contract

An exporter should retain its editable source and assign persistent, unique IDs to
meaningful objects: headings, paragraphs, figures, table cells, slide objects,
diagram nodes and connections. IDs must not be regenerated merely because an
object moves or its text changes. Repeated phrases need separate object IDs.

The proposed package has these independently readable parts:

| Part | Purpose |
| --- | --- |
| Semantic HTML | Reading order, headings, tables, alternative text and stable `id` attributes |
| SVG and original assets | Scalable diagram shapes, connectors and figures with source IDs |
| Versioned JSON manifest | Object index, media types, relationships, asset hashes and capability declarations |
| PDF rendition and layout map | Fixed pages plus explicitly mapped object positions |
| W3C annotation records | Comment bodies, replies, status and text/region selectors |
| Revision records | Source revision, expected hashes, selected targets and applied changes |
| Optional editable sources | Mermaid, drawing source, slide source or other declared format |

Prefer a self-contained HTML file for small documents, with non-executable JSON
script blocks and inline assets. A future ZIP profile can support larger documents;
it needs bounded decompression and safe relative paths before it is enabled.
Package paths must never be commands or arbitrary filesystem access instructions.

The manifest must distinguish `source`, `rendition` and `extracted` objects. A
visually similar PDF text run is not proof that its semantic source is a table cell
or editable diagram node. Do not invent those relationships when extraction is
uncertain. Retain the original and record confidence, provenance and unresolved
mappings instead.

## Anchors and coordinate mapping

Use [W3C Web Annotation](https://www.w3.org/TR/annotation-model/) for reusable review
records. Keep an object ID plus text quote/context and text-position selectors
where applicable. Use SVG/region selectors for figures and diagrams. Include the
source revision/hash so a stale selector produces a conflict instead of editing a
different object. Context text is evidence for reanchoring, not permission to pick
the first matching phrase.

Every layout position needs its rendition ID, page index, page size, rotation,
coordinate units and origin. PDF user coordinates and HTML CSS pixels are different
spaces. Store the transform and source viewport used to render them, including crop
boxes and scale. Page indices must have one documented convention. A move between
pages or a reflow invalidates positions until the mapping is regenerated.

## Copilot interaction

### Product language and visual consistency

Extend the existing editor, menus, selection controls and Comments panel. Reuse
the existing theme, spacing, typography, Lucide icons and interaction patterns;
do not introduce a separate agent workspace or console for document editing.
Use the existing localized terminology for actions already supported by PdfCraft.
New labels should describe the user's action in plain language, for example
Explain, Summarize, Read aloud, Dictate and Review changes. These are proposed
labels, not controls included in the current release.

Do not expose implementation prompts, coding instructions, tool names, model
routing commands, JSON, schema syntax or package internals as normal editing
controls. Keep technical contracts in developer documentation and explicit
inspection tools. Exported documents must not require those commands to be read
or reviewed. Use icons with accessible labels and tooltips; do not substitute
emoji, an unrelated icon set or new symbols for established actions.

Hover or keyboard focus should show a readable object description and available
actions. Keep raw IDs, schema types and provenance details in an optional
inspection view. Selecting objects supplies that exact selection to the copilot.
An action bar in the existing editor can offer explain, summarize, comment,
rewrite, diagram edit, read aloud and dictate. It must also work without hover,
microphone or a model connection.

The action pipeline is selection -> intent -> proposed operations -> preview ->
existing document commands -> validation -> one undoable revision. Model output is
never executed as shell code, arbitrary script or an unvalidated file write.
Stale revisions, unsupported operations and ambiguous targets produce a useful
conflict. Preserve ordinary save prompts, recovery and undo/redo. A user may choose
to apply a clear scoped edit directly; destructive or broad changes need a visible
preview. Comments are feedback, not automatically accepted changes.

Keep document content separate from copilot instructions. A document asking the
agent to upload files, follow links or change settings does not authorize those
actions. User-selected provider settings belong outside portable documents.

## Browsing, speech and summaries

An optional embedded browser must be isolated from the document editing engine.
It should support clicked links and back/forward navigation, while keeping remote
pages separate from local package privileges. Cookies, downloads, microphone use
and external navigation need explicit product behavior. Static HTML import must
not begin executing arbitrary page scripts just because a browser exists.

Speech providers should be adapters, rather than hard-coded models. Discover and
probe explicitly configured speech capabilities, preserving credentials in the
user's local settings. A chat endpoint is not proof of speech support. Separately
verify model listing, transcription and synthesis capabilities, language, audio
format, timeouts, cancellation and resource use. Store model/probe provenance
without keys. Do not silently substitute a paid provider or a different language.

Voice edits use the same selection, proposal, command and undo pipeline as typed
edits. Show the transcript and microphone state. Read-aloud, page summaries and
podcast-style narration should cite source object/page IDs. Keep generated narration
and audio labeled as derived material; do not replace the original document.

## Components to evaluate

| Component | Intended role | Boundary |
| --- | --- | --- |
| [Docling](https://github.com/docling-project/docling) | PDF structure, reading order, tables, pictures and JSON extraction | Optional external adapter; evaluate actual fidelity and model licences |
| [MarkItDown](https://github.com/microsoft/markitdown) | Lightweight text/Markdown extraction for summaries | Does not serve as the visual editing master |
| [Mermaid](https://github.com/mermaid-js/mermaid) | Text sources for flowcharts, ER and sequence diagrams | Optional source format; preserve source and generated SVG, do not replace all diagrams with it |
| [Paper2Agent](https://github.com/jmiao24/Paper2Agent) | Optional research-paper tools and source-grounded interactions | Separate from conversion and deterministic document editing; inspect execution and cost before enabling |
| [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) / [whisper.cpp](https://github.com/ggml-org/whisper.cpp) | Possible local speech adapters if existing providers do not meet requirements | No bundled model until licence, language and machine-resource checks pass |

These are candidates, not installed integrations. Native product and build code
remain Rust under repository rules. External tools must have a versioned process
contract, bounded runtime and output, clear provenance and compatible licences.
Do not copy GPL/AGPL code into the product. Model weights have their own licences.

## Conversion fidelity and acceptance

Keep the original PDF as an authoritative rendition. PDF-to-HTML extraction must
retain image assets and geometry, reading order, links and available accessibility
structure, while recording missing fonts, unsupported graphics, OCR uncertainty
and unresolved semantics. An image of a page can preserve appearance but is not
equivalent to editable text or diagram structure. Missing semantic source in an
untagged PDF cannot be recovered with a universal lossless guarantee.

For HTML-to-PDF, preserve the semantic source and map IDs to the actual rendered
pages. For a round trip, compare page renders, object counts, asset hashes, text,
links, annotations and mapping consistency. Report losses explicitly. Separate
visual equivalence, semantic equivalence and editability in the test results.

Delivery stages and acceptance checks:

1. Ship the existing native HTML integration: published source, compliant portable
   package, documented install and real second-machine save/reopen test.
2. Define and validate the minimal exporter profile using synthetic notes, a slide,
   a diagram and a website design. Test duplicate IDs, stale revisions, missing
   assets and unknown extension retention. Add headless inspection/export tools.
3. Add native selection and hover/focus inspection. Test rotated pages, zoom,
   duplicate text and selection of diagram nodes/edges, without breaking PDF tools.
4. Add typed copilot proposals and command application, then verified speech
   adapters. Test cancellation, malformed output, undo/redo and save/reopen.
5. Add browser navigation and source-linked narration as independently opt-in
   capabilities. Test isolation and offline behavior.
6. Add evaluated PDF extraction and conversion adapters with honest fidelity
   reports. Validate the same package on both paired machines.

No stage is complete merely because a model produced a plausible screenshot or
summary. The app, headless tools and saved portable file must agree.

Revision history: 2026-10-26 — initial researched architecture and acceptance plan.
