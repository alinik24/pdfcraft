# Document copilot

Status: development branch; completed release checks are recorded in release notes.
Older HTML prereleases have a placeholder or an earlier assistant.

Open **All tools > AI assistant** for one conversation about the active PDF or HTML.
Ask questions, summarize, translate, or request changes. Selected text accompanies
your message; without a selection, the current page accompanies it. Hermes can read
the current PDF snapshot for more context. Scans need OCR for exact text anchors.
Opening a document or panel starts no agent and sends no content.

Hermes is the actual agent behind the conversation, connected through its native
Agent Client Protocol. Runtime and model connections are optional, locally configured.
No account, key, personal profile, model weights or endpoint ships in the app.

The chat streams answers and activity. Operations needing approval offer **Allow
once** and **Decline**. **Stop** closes that document's runtime. **Add as comment**
uses normal comments and history. For proposed edits, choose **Review changes**,
then **Apply changes**. Application is one undoable native transaction; **Save**
remains separate. Stale proposals are refused. Closing the document closes its agent.

Supported proposals: comments; exact quoted highlights, underlines and strikeouts;
page rotation; document metadata; replacement PDF pages generated in the document
workspace. Replacement pages retain existing destination annotations. This does
not promise lossless semantic reconstruction. HTML saves embed annotations and the
edited PDF representation in the existing native review layer, preserving the
original DOM. See [HTML review](html-review.md).

## Connect your agent

1. Install the public portable package and open **All tools > AI assistant**.
   Connection controls are available before opening a document.
2. Use **Choose connection file** to select your private Hermes runtime JSON
   prepared using the setup below. This remembers its location for this user,
   so moving the application or installing a new version preserves the connection.
   Keep it outside tracked source or in an explicitly ignored private runtime folder.
3. Open your existing Docker Desktop and wait until its engine is ready. Choose
   **Check connection**. This checks local configuration, credential availability,
   Docker and the pinned runtime image; it makes no model request and sends no
   document. A ready local runtime does not prove that a provider login is valid.
4. Open a PDF or HTML, then send a message. A successful reply verifies your model
   connection. Start with a document you are comfortable sending to that model.
5. Ask for a change, then use **Review changes > Apply changes** and **Save**.
   You can decline agent operations, stop the conversation or undo applied edits.

Example requests: “Summarize this page”, “Highlight ‘the exact selected phrase’”,
“Underline ‘another exact phrase’ and add a note”, or “Add a comment explaining
this paragraph”. Quote exact text for markup. Read-only answers do not edit your
document. Saving is always separate from applying a proposal.

### Troubleshooting

| What you see | What to do |
| --- | --- |
| No agent configured | Choose your private connection file, or use Retry connection after repairing it. |
| Docker unavailable | Open the existing Docker Desktop and wait for its engine. Do not terminate unrelated work to reclaim memory. |
| Runtime image unavailable | Prepare the pinned runtime with the helper below. No image is downloaded automatically. |
| Credential unavailable | Repair the existing local credential reference. Do not paste keys into chat or the document. |
| Runtime ready but no model reply | Check your selected model, access and quota in your own Hermes setup. No alternate account or paid fallback is selected automatically. |
| Connection changed while using a document | Stop active conversations before switching. Existing documents and edit history are retained. |

**Check connection** starts only explicit local Docker diagnostic commands, each
with a ten-second deadline, using your existing Docker context (which can refer
to a remote Docker engine if you configured one). Diagnostic child processes do
not receive model keys. Ordinary startup does not start Docker, download a
runtime, run Hermes or send a document. The in-app selection stores a private
file path, not credential values, profiles, permissions or chat history.

## Private setup

The host reads an explicit `PDFCRAFT_COPILOT_CONFIG` override first, then the
connection selected in the assistant, then a private
`.document-workspaces/.runtime/copilot.json` beside the app or a project ancestor.
The selected connection reference is local to the current user:
`%LOCALAPPDATA%/PdfCraft/copilot-connection.json` on Windows, or
`$XDG_CONFIG_HOME/PdfCraft/copilot-connection.json` (falling back to
`~/.config/PdfCraft/copilot-connection.json`) on other desktops.
An absent configuration leaves Copilot offline and ordinary editing available.
Never publish this file or your private template.

```json
{
  "format": "pdfcraft-copilot-runtime/1",
  "docker": "C:\\Program Files\\Docker\\Docker\\resources\\bin\\docker.exe",
  "image": "sha256:<immutable image ID from docker image inspect>",
  "workspace_root": "C:\\Projects\\pdfcraft\\.document-workspaces",
  "profile_template": "C:\\Projects\\pdfcraft\\.document-workspaces\\.runtime\\profile-template",
  "credentials": {
    "env_file": "C:\\Private\\agent.env",
    "variables": ["YOUR_MODEL_API_KEY"]
  },
  "timeout_seconds": 180
}
```

Use your existing Docker installation and a Linux image running Hermes ACP.
[The runtime helper](../packaging/windows/copilot-runtime.ps1) builds a pinned
Hermes source archive with locked, hash-checked dependencies. Existing Git, uv,
Docker and Windows tar are required; the helper installs or activates none of them.
Use the resulting immutable image ID; mutable tags such as `latest` are refused.

The template is a reviewed Hermes profile: `config.yaml`, optionally `SOUL.md`,
`skills/`, and `memories/`. Select the model explicitly and disable unapproved
fallbacks. Refer to credentials by environment-variable name. Do not include `.env`,
authentication files, entire homes, discovered hooks or unrestricted host MCP
configurations. Preferences and skills do not grant additional permissions.

Each source gets a named, distinct folder under `workspace_root`. Unsaved documents
have a private session identity. Only that folder is mounted: read-only
`input/current.pdf`, `outputs/`, `.tmp/`, `.cache/`, `.home/`, `.hermes/`, and an
optional `.venv/`. Generated files and environments stay there. The container root,
snapshot, configuration and workspace manifest are read-only. No host socket or
other document is mounted. Each runtime has a 2 GiB limit, two CPUs and a bounded
lifetime. Cancellation removes only its owned container; Docker must be available
for cleanup. Container isolation protects host files; it is not a network firewall.
Messages and agent-selected content go to the configured model connection.

Keys come from the existing process environment or a referenced local environment
file. Values are excluded from command arguments and document metadata. Known key
values are redacted in protocol text and refused in imported outputs. Review
content before sharing it publicly.

## Automation and reuse

Host setup is also available headlessly: `pdfcraft-cli copilot connect <runtime.json>`
remembers the same private reference, and `pdfcraft-cli copilot check` performs the
same bounded local diagnostic. These are explicit host commands; they are not
exposed to document/MCP requests. Neither starts an agent or validates model access.


The same backend serves opt-in CLI/MCP tools `copilot_chat`, `copilot_status`,
`copilot_respond`, `copilot_stop`, and `copilot_apply_proposal`. Host write roots
are checked before workspace creation. Status never approves or applies changes.
Normal `doc_save` and undo tools remain the persistence/history interface. Ordinary
GUI launch starts no MCP server or listening port.

An explicit live check using a synthetic file and your configured model:

```text
cargo run -p pdfcraft-automation --features html-review --example copilot_check -- synthetic.html --allow-workspace-operations --apply
```

It checks an actual agent-created proposal, native application, undo/redo, save,
reopen and shutdown. It never runs automatically in unit tests. For another machine,
install the same public package, build or transfer the pinned runtime without
credentials, and reference that machine's existing local model credential. A reviewed
setup-only export can transfer reusable settings; logins, approval grants, binary
tools and environments need separate live checks.

## Provenance and limits

The earlier provider foundation retains attribution to upstream contribution
[#551](https://github.com/storytold/pdfcraft/pull/551). Related proposal
[#182](https://github.com/storytold/pdfcraft/pull/182) is separate. External
[Hermes Agent](https://github.com/NousResearch/hermes-agent) retains its own licence
and notices. This fork does not claim upstream adoption.

Voice/microphone controls, podcasts, an embedded browser, semantic design-object
editing and general structure-preserving PDF-to-HTML conversion remain future work.
