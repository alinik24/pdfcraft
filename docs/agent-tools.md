# Agent tools and structured document extraction

Development status, 2026-11-02. This describes the reuse choices and integration
boundaries; it is not a claim that every service is installed or accepted.

## Native tools first

PdfCraft already exposes a common automation/MCP tool table. Reuse
`doc_export_office` for its existing HTML export, `text_extract`, `text_lines`,
`text_paragraphs`, `page_images`, `image_save`, `doc_export_all_images`,
`comment_list`, and `doc_export_data` for XFDF/FDF comments. Native edits retain
the normal document permission, undo and save rules. HTML export is a conversion
with documented fidelity limits, not recovery of the original website design.

The development `comment_list` output also includes all markup `quads`, with
`quads_coordinate_space`. Display coordinates use points, a top-left origin and
downward y. Preserve each region: their union can include unmarked neighboring
lines. An explicit `pdf-user-space` fallback must be transformed before reusing
it with displayed-page tools. XFDF remains the interoperable annotation export.

## Connect the existing agent

Hermes provides ACP conversations, skills, memory, permissions and MCP tools.
PdfCraft should supply document context and native tool access, rather than
implement another agent loop. Explicit `mcp_servers` connection entries are
forwarded on both new and resumed ACP sessions in the development source.
Entries select an executable already installed inside the agent runtime; they
do not install packages. No server is selected or registered by default.

For example, a reviewed private runtime configuration can include:

```json
{
  "shared_project_runtime": true,
  "mcp_servers": [
    {
      "name": "document_conversion",
      "command": "/usr/bin/env",
      "args": [
        "DOCLING_MCP_CONVERSION_MODE=local",
        "DOCLING_MCP_FALLBACK_TO_LOCAL=false",
        "DOCLING_MCP_KEEP_IMAGES=true",
        "DOCLING_MCP_CACHE_MAX_DOCUMENTS=2",
        "HF_HOME=/project/.cache/huggingface",
        "TORCH_HOME=/project/.cache/torch",
        "/project/tools/docling/bin/docling-mcp-server",
        "--transport", "stdio"
      ]
    }
  ]
}
```

This is an extension to the complete runtime configuration in
[assistant.md](assistant.md), not a standalone connection file. The executable
must exist and its pinned version/options must be checked before enabling it. This
example selects local conversion with retained images and a small document cache;
it does not enable formula enrichment or picture-description models. Do not
put keys in arguments. MCP executables run with the selected agent's runtime
permissions; adding a server does not itself make its operations safe or give it
access to the visible PdfCraft document.

Full live-document tool access should reuse the authenticated running-window
bridge proposed in [upstream #2471](https://github.com/storytold/pdfcraft/pull/2471)
and the request/review ownership from
[#2340](https://github.com/storytold/pdfcraft/pull/2340). Both were open and
unmerged at review. They require porting and acceptance on current upstream;
the installed `.4` and candidate `.5` do not provide this bridge. A separate
headless MCP session must never be described as editing the visible GUI session.

Codex can be integrated through its documented
[App Server](https://learn.chatgpt.com/docs/app-server), including streaming and
native approval requests. Claude has a maintained
[ACP adapter](https://github.com/agentclientprotocol/claude-agent-acp).
These are future optional adapters, not configured accounts or tested product
features. Keep each agent's native authentication and approval controls.

## One managed runtime

The opt-in development shared-runtime mode uses one dedicated project agent
home, tool directory and cache under the configured workspace root, with
document-specific inputs, outputs and saved ACP session references. It does not
copy a profile or create a virtual environment per document. Existing document
folders and historical memories are preserved; they are not automatically merged.

Hermes warns against simultaneous processes sharing one home. A project owner
lease serializes that profile; disconnect the current conversation before using
it for another document. An uncertain or orphaned container blocks a new writer
instead of risking concurrent memory writes. The application must verify shutdown
before releasing ownership. This is a private PdfCraft agent profile, not a mount
of the user's entire home. Shared memory can contain document-derived information;
use a separate project profile where documents must not share that information.

## Optional semantic conversion

Reuse [Docling](https://github.com/docling-project/docling) and its official
[MCP](https://github.com/docling-project/docling-mcp) or
[Serve](https://github.com/docling-project/docling-serve) integration for layout,
tables, images and structured extraction. Reuse DoclingDocument JSON, which can
retain provenance and bounding boxes, alongside HTML and the original document.
One pinned shared runtime/model store is preferable to document-specific installs.
Install only on explicit setup; model licences and disk/memory requirements must
be checked for the selected pipeline. There is no implicit cloud fallback.

Formula enrichment is optional and needs the richer pipeline options: the
reviewed stock MCP local converter did not enable it. Retain source-region images
beside reconstructed LaTeX/MathML and compare them. Do not label a reconstruction
as an exact extract. [GROBID](https://github.com/grobidOrg/grobid) is an optional
specialist for scholarly references and citation contexts, not a general editor.
No semantic converter is bundled or accepted by this source change alone.

## Portable review output

An export should preserve the PDF/HTML and standard annotations, plus selected
region images, structured extraction, readable HTML/Markdown, and reconstructed
table/math files where requested. Record the source hash/revision, page, original
quads, coordinate convention, annotation/group IDs, tool/model/options and failed
or incomplete coverage. Group separate marks only on explicit selection or a
reviewed suggestion. Related captions and referenced objects must be identified
as related content, without silently enlarging the selection.

Dropping a PDF into another assistant does not guarantee that it will interpret
its annotation objects. Include readable extracts and a machine-readable manifest
when reliable interchange matters. Validate split highlights, nearby math lines,
cropped/rotated pages, tables, figure captions, references, scans and save/reopen
before claiming acceptance. Extraction can make mistakes; uncertain or incomplete
results must remain visible and reviewable.
