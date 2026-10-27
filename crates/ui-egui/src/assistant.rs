//! The existing AI assistant tool panel. Provider I/O is injected by the native app;
//! this frontend remains network-free. Keys and conversations are session-only.
use std::sync::{Arc, mpsc};

use pdfcraft_ai::{self as ai, Task, Transport};
#[cfg(not(target_arch = "wasm32"))]
use pdfcraft_ai::{Api, Page, Provider};
use pdfcraft_engine::{DocId, Edit, NewAnnotation};

use crate::{PdfCraftApp, comments::CommentTool, theme::Tokens, widgets};

pub type Sender = Arc<dyn Transport>;

#[derive(Default)]
pub struct Assistant {
    pub enabled: bool,
    pub endpoint: String,
    pub model: String,
    /// Never serialized, exposed through the control state, or written to a document.
    pub key: String,
    pub question: String,
    pub language: String,
    pub answer: String,
    pub error: Option<String>,
    source: Option<Source>,
    pending: Option<mpsc::Receiver<Result<ai::Answer, String>>>,
    cancelled: bool,
    truncated: bool,
    #[cfg(not(target_arch = "wasm32"))]
    chat: Chat,
}

#[derive(Clone)]
struct Source {
    doc: DocId,
    generation: u64,
    page: usize,
    name: String,
    selection: bool,
}

impl Assistant {
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }

    /// Discard the result; keep the one worker admitted until its bounded request ends.
    pub fn cancel(&mut self) {
        self.cancelled = true;
        self.answer.clear();
    }

    pub fn poll(&mut self) {
        let Some(rx) = &self.pending else { return };
        match rx.try_recv() {
            Ok(result) => {
                self.pending = None;
                if !self.cancelled {
                    match result {
                        Ok(answer) => {
                            self.truncated = answer.coverage.truncated;
                            self.answer = answer.text;
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pending = None;
                if !self.cancelled {
                    self.error = Some(tl!("The assistant stopped before answering. Please try again.").into());
                }
            }
        }
    }
}

impl PdfCraftApp {
    /// Drain every document's bounded event queue even while another tool is visible.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn poll_copilot(&mut self, ctx: &egui::Context) {
        let session = &self.session;
        // Dropping a closed document's connection also closes its owned agent runtime.
        self.assistant.chat.documents.retain(|document| session.get(document.id).is_some());
        for document in &mut self.assistant.chat.documents {
            document.poll();
        }
        if self.assistant.chat.documents.iter().any(ChatDocument::busy) {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    /// Shared entry for panel buttons and registered commands. No request on merely opening it.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn ask_assistant(&mut self, task: Task) {
        if self.assistant.busy() {
            return;
        }
        let fail = |app: &mut Self, message: &str| app.assistant.error = Some(tl!(message).into());
        if !self.assistant.enabled {
            fail(self, "Turn on the assistant and choose a model first.");
            return;
        }
        let Some(sender) = self.ai_transport.clone() else {
            fail(self, "The assistant needs the desktop app.");
            return;
        };
        let Some((i, id)) = self.active_ids() else {
            fail(self, "Open a document first.");
            return;
        };
        if !self.may_copy(id) {
            return;
        }
        let Some(doc) = self.session.get(id) else { return };
        let Some(view) = self.views.get(i) else { return };
        // Select All can span uncached pages. It is not a single-page selection.
        if view.all_text {
            fail(self, "Select a passage or clear the selection to use the current page.");
            return;
        }
        let selected = view.selection_quads(&doc.info).and_then(|(p, _)| view.selected_text().map(|t| (p, t)));
        let page = selected.as_ref().map_or(view.current, |(p, _)| *p);
        if doc.info.pages.get(page).is_none() {
            fail(self, "This page is not available.");
            return;
        }
        let provider =
            Provider::new(Api::OpenAi, self.assistant.endpoint.clone(), self.assistant.model.clone()).with_key(Some(self.assistant.key.clone()));
        if let Err(e) = provider.url() {
            self.assistant.error = Some(e.to_string());
            return;
        }
        let source = Source { doc: id, generation: doc.edit_generation(), page, name: doc.name.clone(), selection: selected.is_some() };
        let bytes = doc.bytes.clone();
        let password = doc.password.as_deref().map(Arc::from);
        let ctx = self.ctx.clone();
        let (tx, rx) = mpsc::channel();
        let worker = move || {
            let outcome = pdfcraft_engine::guard(|| {
                let text = match selected {
                    Some((_, text)) => text,
                    None => {
                        let mut renderer =
                            pdfcraft_render::PageRenderer::new(bytes, pdfcraft_render::RenderConfig { password, ..Default::default() });
                        let rendered = renderer.render(pdfcraft_render::RenderRequest {
                            page,
                            kind: pdfcraft_render::RequestKind::Text,
                            tile: None,
                            scale: 1.0,
                            tag: 0,
                            preview: false,
                        });
                        rendered.text.ok_or_else(|| tl!("This page has no readable text. Try Recognize text first.").to_string())?.plain_text()
                    }
                };
                ai::run(&provider, sender.as_ref(), &task, &[Page { number: page + 1, text }]).map_err(|e| e.to_string())
            })
            .unwrap_or_else(Err);
            let _ = tx.send(outcome);
            if let Some(ctx) = ctx {
                ctx.request_repaint();
            }
        };
        if std::thread::Builder::new().name("pdfcraft-assistant".into()).spawn(worker).is_err() {
            fail(self, "The assistant could not start. Please try again.");
            return;
        }
        {
            self.assistant.pending = Some(rx);
            self.assistant.cancelled = false;
            self.assistant.answer.clear();
            self.assistant.error = None;
            self.assistant.source = Some(source);
            self.assistant.truncated = false;
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn ask_assistant(&mut self, _task: Task) {
        self.assistant.error = Some(tl!("The assistant needs the desktop app.").into());
    }

    /// A visible, explicit action through the normal comment/history/save pipeline.
    pub fn assistant_add_comment(&mut self) -> bool {
        let Some(source) = &self.assistant.source else { return false };
        let Some((_, id)) = self.active_ids() else { return false };
        let Some(doc) = self.session.get(id) else { return false };
        if id != source.doc || doc.edit_generation() != source.generation {
            self.assistant.error = Some(tl!("The document changed. Ask again before adding this answer as a comment.").into());
            return false;
        }
        if self.assistant.answer.trim().is_empty() || self.assistant.busy() {
            return false;
        }
        let Some(page) = doc.info.pages.get(source.page) else { return false };
        let at = page.view_to_user((page.width - 44.0).max(0.0), 24.0);
        let edit = Edit::AddAnnotation(NewAnnotation {
            page: source.page,
            shape: pdfcraft_engine::Shape::Note { at: [at[0] as f64, at[1] as f64], icon: pdfcraft_engine::NoteIcon::Comment },
            style: self.comment_prefs.style(CommentTool::Note),
            author: self.comment_prefs.author.clone(),
            contents: format!("{}\n\n{}", tl!("AI assistant"), self.assistant.answer),
        });
        self.apply_edit(edit)
    }
}

pub(crate) fn panel(app: &mut PdfCraftApp, ui: &mut egui::Ui, t: &Tokens) {
    #[cfg(not(target_arch = "wasm32"))]
    chat_panel(app, ui, t);
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (app, t);
        ui.label(tl!("The copilot needs the desktop app."));
    }
}
#[cfg(not(target_arch = "wasm32"))]
use pdfcraft_ai::{
    copilot::{Conversation, DocumentContext, Turn, Update},
    workspace::DocumentIdentity,
};
#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
struct Chat {
    instance: String,
    documents: Vec<ChatDocument>,
}
#[cfg(not(target_arch = "wasm32"))]
type Connected = Result<(Box<dyn Conversation>, Source), String>;

#[cfg(not(target_arch = "wasm32"))]
type Reviewed = (Box<dyn Conversation>, Result<PreparedReview, String>);

#[cfg(not(target_arch = "wasm32"))]
struct ProposalNotice {
    path: String,
    summary: String,
    revision: u64,
}

#[cfg(not(target_arch = "wasm32"))]
struct PreparedReview {
    edit: Edit,
    revision: u64,
    summary: String,
    details: Vec<String>,
}

#[cfg(not(target_arch = "wasm32"))]
struct ChatDocument {
    id: DocId,
    draft: String,
    messages: Vec<ChatMessage>,
    connection: Option<Box<dyn Conversation>>,
    pending: Option<mpsc::Receiver<Connected>>,
    review_pending: Option<mpsc::Receiver<Reviewed>>,
    proposal: Option<ProposalNotice>,
    prepared: Option<PreparedReview>,
    source: Option<Source>,
    permission: Option<(serde_json::Value, String, String)>,
    running: bool,
    stopping: bool,
    status: String,
    error: Option<String>,
}
#[cfg(not(target_arch = "wasm32"))]
struct ChatMessage {
    user: bool,
    text: String,
    source: Option<Source>,
}
#[cfg(not(target_arch = "wasm32"))]
impl ChatDocument {
    fn new(id: DocId) -> Self {
        Self {
            id,
            draft: String::new(),
            messages: Vec::new(),
            connection: None,
            pending: None,
            review_pending: None,
            proposal: None,
            prepared: None,
            source: None,
            permission: None,
            running: false,
            stopping: false,
            status: String::new(),
            error: None,
        }
    }
    fn busy(&self) -> bool {
        self.running || self.pending.is_some() || self.review_pending.is_some()
    }

    fn poll(&mut self) {
        if let Some(rx) = &self.review_pending {
            match rx.try_recv() {
                Ok((connection, result)) => {
                    self.review_pending = None;
                    if self.stopping {
                        // The agent was idle while its bounded output was being read. Dropping
                        // this owned connection cancels its runtime and discards prepared edits.
                        self.prepared = None;
                        self.proposal = None;
                        self.stopping = false;
                    } else {
                        self.connection = Some(connection);
                        match result {
                            Ok(review) => self.prepared = Some(review),
                            Err(error) => self.error = Some(error.chars().take(4000).collect()),
                        }
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.review_pending = None;
                    self.fail(tl!("The changes could not be reviewed. Your document was not changed.").into());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(Ok((mut connection, source))) => {
                    self.pending = None;
                    self.source = Some(source);
                    if self.stopping
                        && let Err(e) = connection.cancel()
                    {
                        self.fail(e);
                        return;
                    }
                    self.connection = Some(connection);
                }
                Ok(Err(e)) => {
                    self.pending = None;
                    self.fail(e);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.fail(tl!("The copilot stopped before connecting. Please try again.").into());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        // Bound work per frame even if a backend produces an unbounded event stream.
        for _ in 0..64 {
            let Some(connection) = &mut self.connection else { break };
            let update = match connection.poll() {
                Ok(Some(update)) => update,
                Ok(None) => break,
                Err(e) => {
                    self.fail(e);
                    self.connection = None;
                    break;
                }
            };
            match update {
                Update::Ready => {}
                Update::Text(text) if self.running && !self.stopping => {
                    if self.messages.last().is_none_or(|m| m.user) {
                        self.messages.push(ChatMessage { user: false, text: String::new(), source: self.source.clone() });
                    }
                    if let Some(message) = self.messages.last_mut() {
                        let remaining = ai::MAX_ANSWER_CHARS.saturating_sub(message.text.chars().count());
                        message.text.extend(text.chars().take(remaining));
                    }
                }
                Update::Tool(title) if self.running && !self.stopping => self.status = title.chars().take(256).collect(),
                Update::Proposal { path, summary } if self.running && !self.stopping => {
                    if path.len() <= 4096
                        && let Some(source) = &self.source
                    {
                        // An agent may repeat its private interchange instructions. Keep
                        // the ordinary conversation about the review, not its packet format.
                        if let Some(message) = self.messages.last_mut().filter(|m| !m.user)
                            && (message.text.contains("pdfcraft-review-proposal/") || message.text.contains("/workspace/outputs/review-"))
                        {
                            message.text = summary.chars().take(2000).collect();
                        }
                        self.proposal = Some(ProposalNotice { path, summary: summary.chars().take(2000).collect(), revision: source.generation });
                        self.prepared = None;
                    }
                }
                Update::Permission { id, title, details } if self.running && !self.stopping && self.permission.is_none() => {
                    self.permission = Some((id, title.chars().take(256).collect(), details.chars().take(4000).collect()));
                }
                Update::Permission { id, .. } => {
                    if let Some(connection) = &mut self.connection
                        && let Err(e) = connection.decide(id, false)
                    {
                        self.fail(e);
                    }
                }
                Update::Finished => {
                    if self.stopping {
                        self.connection = None;
                    }
                    self.running = false;
                    self.stopping = false;
                    self.permission = None;
                    self.status.clear();
                }
                Update::Failed(e) => self.fail(e),
                Update::Text(_) | Update::Tool(_) | Update::Proposal { .. } => {}
            }
        }
    }
    fn fail(&mut self, error: String) {
        self.error = Some(error.chars().take(4000).collect());
        self.running = false;
        self.stopping = false;
        self.permission = None;
        self.proposal = None;
        self.prepared = None;
        self.status.clear();
        self.connection = None;
    }
    fn stop(&mut self) {
        self.stopping = true;
        self.permission = None;
        self.proposal = None;
        self.prepared = None;
        if let Some(connection) = &mut self.connection
            && let Err(e) = connection.cancel()
        {
            self.fail(e);
        }
    }
    fn decide(&mut self, allow: bool) {
        if let Some((id, _, _)) = self.permission.take()
            && let Some(connection) = &mut self.connection
            && let Err(e) = connection.decide(id, allow)
        {
            self.fail(e);
        }
    }
}
#[cfg(not(target_arch = "wasm32"))]
impl PdfCraftApp {
    /// Prepare a proposed native transaction on a worker. Opening a review never changes a PDF.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn review_copilot_changes(&mut self, origin: DocId) {
        if self.active_ids().map(|(_, id)| id) != Some(origin) {
            return;
        }
        let Some(doc) = self.session.get(origin) else { return };
        let Some(state) = self.assistant.chat.documents.iter_mut().find(|state| state.id == origin) else { return };
        if state.busy() {
            return;
        }
        let Some(notice) = &state.proposal else { return };
        if doc.edit_generation() != notice.revision {
            state.error = Some(tl!("The document changed. Ask for a new review before applying changes.").into());
            return;
        }
        let snapshot = match pdfcraft_engine::review::ReviewSnapshot::from_document(doc) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                state.error = Some(error);
                return;
            }
        };
        let Some(connection) = state.connection.take() else { return };
        let path = notice.path.clone();
        let ctx = self.ctx.clone();
        let (tx, rx) = mpsc::channel();
        let worker = move || {
            let result = pdfcraft_engine::guard(|| {
                let packet = connection.read_output(std::path::Path::new(&path), ai::review::MAX_PROPOSAL_BYTES)?;
                let proposal = ai::review::parse(&packet, snapshot.revision()).map_err(|e| e.to_string())?;
                let edit = pdfcraft_engine::review::prepare_review(&snapshot, &proposal, |path, limit| connection.read_output(path, limit))?;
                let details = proposal.changes.iter().map(describe_change).collect();
                Ok(PreparedReview { edit, revision: snapshot.revision(), summary: proposal.summary, details })
            })
            .unwrap_or_else(Err);
            let _ = tx.send((connection, result));
            if let Some(ctx) = ctx {
                ctx.request_repaint();
            }
        };
        if std::thread::Builder::new().name("pdfcraft-copilot-review".into()).spawn(worker).is_err() {
            state.error = Some(tl!("The review could not start. Your document was not changed.").into());
            return;
        }
        state.review_pending = Some(rx);
        state.prepared = None;
        state.error = None;
    }

    /// Apply only the reviewed document and exact live revision, as one ordinary undoable edit.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn apply_copilot_changes(&mut self, origin: DocId) -> bool {
        let Some(index) = self.assistant.chat.documents.iter().position(|state| state.id == origin) else { return false };
        let Some(state) = self.assistant.chat.documents.get(index) else { return false };
        let Some(prepared) = &state.prepared else { return false };
        if state.busy() {
            return false;
        }
        let current = self.session.get(origin).map(|doc| doc.edit_generation());
        if self.active_ids().map(|(_, id)| id) != Some(origin) || current != Some(prepared.revision) {
            if let Some(state) = self.assistant.chat.documents.get_mut(index) {
                state.error = Some(tl!("Return to the original document and request a new review if it changed.").into());
            }
            return false;
        }
        let Some(prepared) = self.assistant.chat.documents.get_mut(index).and_then(|state| state.prepared.take()) else { return false };
        if !self.apply_edit(prepared.edit) {
            return false;
        }
        if let Some(state) = self.assistant.chat.documents.get_mut(index) {
            state.proposal = None;
            state.error = None;
            if state.messages.len() < 64 {
                state.messages.push(ChatMessage {
                    user: false,
                    text: tl!("Changes applied. Review the document, then Save when you are ready.").into(),
                    source: None,
                });
            }
        }
        true
    }

    /// Starts only on an explicit Send. Extraction and agent startup never block an egui frame.
    pub fn send_copilot(&mut self, message: String) {
        let Some((view_index, id)) = self.active_ids() else { return };
        if !self.may_copy(id) {
            return;
        }
        let Some(backend) = self.copilot_backend.clone() else { return };
        let Some(doc) = self.session.get(id) else { return };
        let Some(view) = self.views.get(view_index) else { return };
        let chat = &mut self.assistant.chat;
        let index = match chat.documents.iter().position(|d| d.id == id) {
            Some(index) => index,
            None => {
                chat.documents.push(ChatDocument::new(id));
                chat.documents.len().saturating_sub(1)
            }
        };
        let Some(state) = chat.documents.get_mut(index) else { return };
        if state.busy() || message.trim().is_empty() {
            return;
        }
        if message.chars().count() > ai::MAX_QUESTION_CHARS {
            state.fail(tl!("Please shorten your message.").into());
            return;
        }
        if view.all_text {
            state.fail(tl!("Select a passage or clear the selection to use the current page.").into());
            return;
        }
        let selected = view.selection_quads(&doc.info).and_then(|(p, _)| view.selected_text().map(|t| (p, t)));
        let page = selected.as_ref().map_or(view.current, |(p, _)| *p);
        if doc.info.pages.get(page).is_none() {
            state.fail(tl!("This page is not available.").into());
            return;
        }
        if chat.instance.is_empty() {
            let mut bytes = [0u8; 16];
            if getrandom::fill(&mut bytes).is_err() {
                state.fail(tl!("A private document session could not be created.").into());
                return;
            }
            chat.instance = bytes.iter().map(|b| format!("{b:02x}")).collect();
        }
        let identity = match &doc.path {
            Some(path) => DocumentIdentity::source(std::path::Path::new(path)),
            None => DocumentIdentity::unsaved(&format!("{}-{}", chat.instance, id.0), &doc.name),
        };
        let identity = match identity {
            Ok(identity) => identity,
            Err(_) => {
                state.fail(tl!("The document location is unavailable. Save it or reopen it before asking the copilot.").into());
                return;
            }
        };
        // Refuse silently dropping conversation history. A future explicit New conversation can reset it.
        if state.messages.len() >= 64 {
            state.fail(tl!("This conversation is full. Reopen the app to start a new session.").into());
            return;
        }
        let source = Source { doc: id, generation: doc.edit_generation(), page, name: doc.name.clone(), selection: selected.is_some() };
        let source_for_worker = source.clone();
        let bytes = doc.bytes.clone();
        let password = doc.password.as_deref().map(Arc::from);
        let context = DocumentContext { identity, name: doc.name.clone() };
        let mut connection = state.connection.take();
        let ctx = self.ctx.clone();
        let user_message = message.clone();
        let (tx, rx) = mpsc::channel();
        let worker = move || {
            let outcome = pdfcraft_engine::guard(|| {
                let text = match selected {
                    Some((_, text)) => text,
                    None => {
                        let mut renderer =
                            pdfcraft_render::PageRenderer::new(bytes.clone(), pdfcraft_render::RenderConfig { password, ..Default::default() });
                        let rendered = renderer.render(pdfcraft_render::RenderRequest {
                            page,
                            kind: pdfcraft_render::RequestKind::Text,
                            tile: None,
                            scale: 1.0,
                            tag: 0,
                            preview: false,
                        });
                        rendered.text.ok_or_else(|| tl!("This page has no readable text. Try Recognize text first.").to_string())?.plain_text()
                    }
                };
                let mut conversation = match connection.take() {
                    Some(c) => c,
                    None => backend.connect(context)?,
                };
                conversation.send(Turn { pdf: bytes, revision: source_for_worker.generation, page: page.saturating_add(1), text, message })?;
                Ok((conversation, source_for_worker))
            })
            .unwrap_or_else(Err);
            let _ = tx.send(outcome);
            if let Some(ctx) = ctx {
                ctx.request_repaint();
            }
        };
        if std::thread::Builder::new().name("pdfcraft-copilot-connect".into()).spawn(worker).is_err() {
            state.fail(tl!("The copilot could not start. Please try again.").into());
            return;
        }
        state.pending = Some(rx);
        state.running = true;
        state.stopping = false;
        state.error = None;
        state.proposal = None;
        state.prepared = None;
        state.source = Some(source);
        state.draft.clear();
        state.messages.push(ChatMessage { user: true, text: user_message, source: None });
    }
}
#[cfg(not(target_arch = "wasm32"))]
fn describe_change(change: &ai::review::Change) -> String {
    use ai::review::Change;
    // The typed packet caps every list at 1000 pages and the complete packet at 1 MiB.
    // Show every destination so a review never hides additional affected pages.
    let pages = |numbers: &[usize]| numbers.iter().map(usize::to_string).collect::<Vec<_>>().join(", ");
    match change {
        Change::Comment { page, text } => format!("{} {} · {}: {}", tl!("Page"), page, tl!("Comment"), text),
        Change::Highlight { page, quote, comment } | Change::Underline { page, quote, comment } | Change::Strikeout { page, quote, comment } => {
            let action = match change {
                Change::Highlight { .. } => tl!("Highlight"),
                Change::Underline { .. } => tl!("Underline"),
                _ => tl!("Strike through"),
            };
            let note = comment.as_deref().unwrap_or_default();
            format!("{} {} · {}: {}{}{}", tl!("Page"), page, action, quote, if note.is_empty() { "" } else { "\n" }, note)
        }
        Change::Rotate { pages: numbers, degrees } => format!("{} {} · {} {}°", tl!("Pages"), pages(numbers), tl!("Rotate"), degrees),
        Change::Metadata { key, value } => {
            let key = match key {
                ai::review::MetadataKey::Title => tl!("Title"),
                ai::review::MetadataKey::Author => tl!("Author"),
                ai::review::MetadataKey::Subject => tl!("Subject"),
                ai::review::MetadataKey::Keywords => tl!("Keywords"),
            };
            format!("{key}: {value}")
        }
        Change::ReplacePages { pages: numbers, .. } => format!("{} {} · {}", tl!("Pages"), pages(numbers), tl!("Replace with generated pages")),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn chat_panel(app: &mut PdfCraftApp, ui: &mut egui::Ui, t: &Tokens) {
    widgets::begin_dialog(ui);
    ui.label(tl!("Copilot"));
    ui.label(egui::RichText::new(tl!("Ask, summarize, translate, or review your document in one conversation.")).color(t.text_muted));
    let Some((_, id)) = app.active_ids() else {
        ui.label(tl!("Open a document to start a conversation."));
        return;
    };
    if app.copilot_backend.is_none() {
        ui.label(tl!("Connect your local agent in the desktop setup to use Copilot."));
        return;
    }
    let index = match app.assistant.chat.documents.iter().position(|d| d.id == id) {
        Some(index) => index,
        None => {
            app.assistant.chat.documents.push(ChatDocument::new(id));
            app.assistant.chat.documents.len().saturating_sub(1)
        }
    };
    let Some(state) = app.assistant.chat.documents.get_mut(index) else { return };
    ui.label(
        egui::RichText::new(tl!("Your selected text or current page accompanies each message. Review answers before saving."))
            .color(t.text_muted)
            .size(12.0),
    );
    let mut comment = None;
    let review_height = if state.prepared.is_some() {
        300.0
    } else if state.proposal.is_some() {
        140.0
    } else {
        0.0
    };
    let permission_height = if state.permission.is_some() { 180.0 } else { 0.0 };
    egui::ScrollArea::vertical()
        .id_salt(("copilot-transcript", id.0))
        .max_height((ui.available_height() - 180.0 - review_height - permission_height).max(80.0))
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for message in &state.messages {
                ui.separator();
                ui.label(egui::RichText::new(if message.user { tl!("You") } else { tl!("Copilot") }).strong());
                ui.add(egui::Label::new(&message.text).selectable(true).wrap());
                if let Some(source) = &message.source {
                    ui.label(
                        egui::RichText::new(format!(
                            "{} · {} {} · {}",
                            source.name,
                            tl!("Page"),
                            source.page.saturating_add(1),
                            if source.selection { tl!("Selected text") } else { tl!("Current page") }
                        ))
                        .color(t.text_muted)
                        .size(12.0),
                    );
                    if !state.busy() && !message.text.is_empty() {
                        ui.horizontal_wrapped(|ui| {
                            if widgets::pill_button(ui, tl!("Copy"), false).clicked() {
                                ui.ctx().copy_text(message.text.clone());
                            }
                            if widgets::pill_button(ui, tl!("Add as comment"), false).clicked() {
                                comment = Some((source.clone(), message.text.clone()));
                            }
                        });
                    }
                }
            }
        });
    if let Some((_, title, details)) = &state.permission {
        ui.separator();
        ui.label(egui::RichText::new(title).strong());
        egui::ScrollArea::vertical().id_salt(("copilot-permission", id.0)).max_height(140.0).show(ui, |ui| {
            ui.label(details);
        });
        let mut decision = None;
        ui.horizontal_wrapped(|ui| {
            if widgets::pill_button(ui, tl!("Allow once"), true).clicked() {
                decision = Some(true);
            }
            if widgets::pill_button(ui, tl!("Decline"), false).clicked() {
                decision = Some(false);
            }
        });
        if let Some(allow) = decision {
            state.decide(allow);
        }
    }
    let mut review = false;
    let mut apply = false;
    let mut discard = false;
    if let Some(prepared) = &state.prepared {
        ui.separator();
        ui.label(egui::RichText::new(tl!("Review changes")).strong());
        egui::ScrollArea::vertical().id_salt(("copilot-review", id.0)).max_height(200.0).show(ui, |ui| {
            ui.label(&prepared.summary);
            for detail in &prepared.details {
                ui.label(detail);
            }
        });
        ui.label(egui::RichText::new(tl!("Apply changes adds one undo step. Save remains separate.")).color(t.text_muted).size(12.0));
        ui.horizontal_wrapped(|ui| {
            if widgets::pill_button(ui, tl!("Apply changes"), true).clicked() {
                apply = true;
            }
            if widgets::pill_button(ui, tl!("Discard"), false).clicked() {
                discard = true;
            }
        });
    } else if let Some(proposal) = &state.proposal {
        ui.separator();
        egui::ScrollArea::vertical().id_salt(("copilot-proposal", id.0)).max_height(100.0).show(ui, |ui| {
            ui.label(&proposal.summary);
        });
        ui.add_enabled_ui(!state.busy(), |ui| {
            if widgets::pill_button(ui, tl!("Review changes"), true).clicked() {
                review = true;
            }
            if widgets::pill_button(ui, tl!("Discard"), false).clicked() {
                discard = true;
            }
        });
    }
    if discard {
        state.proposal = None;
        state.prepared = None;
    }
    if let Some(e) = &state.error {
        ui.colored_label(t.text_muted, e);
    }
    if state.busy() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(if state.stopping {
                tl!("Stopping…")
            } else if state.review_pending.is_some() {
                tl!("Preparing review…")
            } else if state.status.is_empty() {
                tl!("Thinking…")
            } else {
                &state.status
            });
            if !state.stopping && widgets::pill_button(ui, tl!("Stop"), false).clicked() {
                state.stop();
            }
        });
    }
    let label = ui.label(tl!("Message Copilot"));
    ui.add_enabled(!state.busy(), widgets::block(&mut state.draft).char_limit(ai::MAX_QUESTION_CHARS).desired_width(f32::INFINITY).desired_rows(3))
        .labelled_by(label.id);
    let mut send = None;
    ui.add_enabled_ui(!state.busy() && !state.draft.trim().is_empty(), |ui| {
        if widgets::pill_button(ui, tl!("Send"), true).clicked() {
            send = Some(state.draft.clone());
        }
    });
    if let Some((source, answer)) = comment {
        app.assistant.source = Some(source);
        app.assistant.answer = answer;
        app.assistant_add_comment();
        if let Some(error) = app.assistant.error.take()
            && let Some(state) = app.assistant.chat.documents.get_mut(index)
        {
            state.error = Some(error);
        }
    }
    if let Some(message) = send {
        app.send_copilot(message);
    }
    if review {
        app.review_copilot_changes(id);
    }
    if apply {
        app.apply_copilot_changes(id);
    }
}

/// A compact native activity row keeps agent work and approval requests visible across tools.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn activity(app: &mut PdfCraftApp, ui: &mut egui::Ui) {
    let Some(state) = app.assistant.chat.documents.iter().find(|s| s.busy()) else { return };
    let id = state.id;
    let name = state.source.as_ref().map_or_else(String::new, |source| source.name.clone());
    let waiting = state.permission.is_some();
    let stopping = state.stopping;
    egui::Panel::top("copilot-activity").show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.spinner();
            ui.label(format!("{} · {}", if waiting { tl!("Copilot needs your decision") } else { tl!("Copilot is working") }, name));
            if widgets::pill_button(ui, tl!("Open Copilot"), false).clicked() {
                app.active = app.views.iter().position(|view| view.id == id);
                app.select_tool("ai");
            }
            if !stopping
                && widgets::pill_button(ui, tl!("Stop"), false).clicked()
                && let Some(state) = app.assistant.chat.documents.iter_mut().find(|state| state.id == id)
            {
                state.stop();
            }
        });
    });
}
#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    #[derive(Default)]
    struct Canned(Mutex<Vec<String>>);
    impl Transport for Canned {
        fn post(&self, request: &ai::Request) -> Result<ai::Response, ai::AiError> {
            self.0.lock().unwrap().push(request.body.clone());
            Ok(ai::Response { status: 200, body: r#"{"choices":[{"message":{"content":"Synthetic answer (p. 1)."}}]}"#.into() })
        }
    }
    fn app() -> (PdfCraftApp, Arc<Canned>) {
        let mut app = PdfCraftApp::new();
        app.set_option("language", "en").unwrap();
        let pdf = app.session.create_from_text("Synthetic fixture", "BLUE BOX connects to GREEN BOX.").unwrap();
        app.open_bytes("synthetic.pdf", None, pdf.as_ref().clone()).unwrap();
        let sender = Arc::new(Canned::default());
        app.ai_transport = Some(sender.clone());
        app.assistant.endpoint = "https://example.invalid/v1".into();
        app.assistant.model = "synthetic-model".into();
        app.assistant.key = "synthetic-session-only-key".into();
        app.select_tool("ai");
        (app, sender)
    }
    fn wait(app: &mut PdfCraftApp) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.assistant.busy() && Instant::now() < deadline {
            app.assistant.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!app.assistant.busy(), "assistant worker did not finish");
        assert!(app.assistant.error.is_none(), "{:?}", app.assistant.error);
    }
    #[test]
    fn opening_the_native_panel_never_connects_and_shows_one_composer() {
        let (mut app, sender) = app();
        let backend = Arc::new(FakeBackend::default());
        app.copilot_backend = Some(backend.clone());
        let mut h = Harness::builder().with_size(egui::vec2(1400.0, 1000.0)).build_eframe(move |_| app);
        h.run_steps(3);
        assert!(sender.0.lock().unwrap().is_empty());
        assert!(backend.calls.lock().unwrap().names.is_empty());
        assert!(h.query_by_label("Message Copilot").is_some());
        assert!(h.query_by_label("Server address").is_none());
        assert!(h.query_by_label("API key (optional, this session only)").is_none());
        assert!(h.query_by_label("Translate into").is_none());
        assert!(h.query_by_label("Summarize").is_none());
        assert!(h.query_by_label("Enable AI assistant").is_none());
        let saved = h.state().persist();
        assert!(!saved.contains("synthetic-session-only-key"));
    }
    #[derive(Default)]
    struct Calls {
        names: Vec<String>,
        turns: Vec<(String, String, usize)>,
        decisions: Vec<bool>,
    }
    #[derive(Default)]
    struct FakeBackend {
        calls: Arc<Mutex<Calls>>,
        approval: bool,
        proposal: bool,
    }
    struct FakeConversation {
        calls: Arc<Mutex<Calls>>,
        approval: bool,
        proposal: bool,
        revision: u64,
        updates: std::collections::VecDeque<Update>,
    }
    #[test]
    fn completed_proposals_show_review_wording_instead_of_internal_packet_instructions() {
        let id = DocId(1);
        let mut state = ChatDocument::new(id);
        state.running = true;
        state.source = Some(Source { doc: id, generation: 0, page: 0, name: "synthetic.pdf".into(), selection: false });
        state.messages.push(ChatMessage {
            user: false,
            text: "Wrote /workspace/outputs/review-private.json in pdfcraft-review-proposal/1 format".into(),
            source: None,
        });
        state.connection = Some(Box::new(FakeConversation {
            calls: Arc::new(Mutex::new(Calls::default())),
            approval: false,
            proposal: true,
            revision: 0,
            updates: std::collections::VecDeque::from([
                Update::Proposal { path: "outputs/review-private.json".into(), summary: "Add a review note on page 1.".into() },
                Update::Finished,
            ]),
        }));
        state.poll();
        assert_eq!(state.messages.last().unwrap().text, "Add a review note on page 1.");
        assert!(state.proposal.is_some());
        assert!(!state.running);
    }
    impl pdfcraft_ai::copilot::Backend for FakeBackend {
        fn connect(&self, context: DocumentContext) -> Result<Box<dyn Conversation>, String> {
            self.calls.lock().unwrap().names.push(context.name);
            Ok(Box::new(FakeConversation {
                calls: self.calls.clone(),
                approval: self.approval,
                proposal: self.proposal,
                revision: 0,
                updates: Default::default(),
            }))
        }
    }
    impl Conversation for FakeConversation {
        fn send(&mut self, turn: Turn) -> Result<(), String> {
            self.revision = turn.revision;
            self.calls.lock().unwrap().turns.push((turn.text, turn.message, turn.page));
            self.updates.push_back(Update::Ready);
            if self.proposal {
                self.updates.extend([
                    Update::Text("I prepared a document review.".into()),
                    Update::Proposal { path: "outputs/review-1.json".into(), summary: "Add a note, highlight the blue box, and set a title.".into() },
                    Update::Finished,
                ]);
            } else if self.approval {
                self.updates.push_back(Update::Permission {
                    id: serde_json::json!(1),
                    title: "Create a review note".into(),
                    details: "Write a new note in this document’s working folder.".into(),
                });
            } else {
                self.updates.extend([Update::Text("Synthetic ".into()), Update::Text("reply.".into()), Update::Finished]);
            }
            Ok(())
        }
        fn poll(&mut self) -> Result<Option<Update>, String> {
            Ok(self.updates.pop_front())
        }
        fn cancel(&mut self) -> Result<(), String> {
            self.updates.clear();
            self.updates.push_back(Update::Finished);
            Ok(())
        }
        fn decide(&mut self, _: serde_json::Value, allow: bool) -> Result<(), String> {
            self.calls.lock().unwrap().decisions.push(allow);
            self.updates.extend([Update::Text(if allow { "Allowed." } else { "Declined." }.into()), Update::Finished]);
            Ok(())
        }
        fn workspace(&self) -> &std::path::Path {
            std::path::Path::new(".")
        }
        fn read_output(&self, path: &std::path::Path, limit: usize) -> Result<Vec<u8>, String> {
            if !self.proposal || path != std::path::Path::new("outputs/review-1.json") {
                return Err("No output".into());
            }
            let packet = serde_json::to_vec(&serde_json::json!({
                "format": ai::review::FORMAT,
                "revision": self.revision,
                "summary": "Add a note, highlight the blue box, and set a title.",
                "changes": [
                    {"kind":"comment","page":1,"text":"Check this connection."},
                    {"kind":"highlight","page":1,"quote":"BLUE BOX","comment":"Review this component."},
                    {"kind":"metadata","key":"Title","value":"Reviewed diagram"}
                ]
            }))
            .unwrap();
            if packet.len() > limit {
                return Err("Output exceeds limit".into());
            }
            Ok(packet)
        }
    }
    fn wait_chat(app: &mut PdfCraftApp) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            for state in &mut app.assistant.chat.documents {
                state.poll();
            }
            if app.assistant.chat.documents.iter().all(|s| !s.busy()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("copilot did not finish");
    }
    #[test]
    fn copilot_streams_multiple_turns_without_reconnecting_or_mixing_documents() {
        let (mut app, _) = app();
        let backend = Arc::new(FakeBackend::default());
        app.copilot_backend = Some(backend.clone());
        let first = app.active_ids().unwrap().1;
        app.send_copilot("Summarize this page".into());
        // Switch while the first worker is still connecting.
        let pdf = app.session.create_from_text("Other", "Other page.").unwrap();
        app.open_bytes("other.pdf", None, pdf.as_ref().clone()).unwrap();
        let second = app.active_ids().unwrap().1;
        app.send_copilot("Translate this page".into());
        wait_chat(&mut app);
        assert_ne!(first, second);
        assert_eq!(app.assistant.chat.documents.len(), 2);
        for state in &app.assistant.chat.documents {
            assert_eq!(state.messages.len(), 2);
            assert_eq!(state.messages[1].text, "Synthetic reply.");
            assert_eq!(state.messages[1].source.as_ref().unwrap().doc, state.id);
        }
        app.send_copilot("Explain the translation".into());
        wait_chat(&mut app);
        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.names.len(), 2);
        assert_eq!(calls.turns.len(), 3);
        assert!(calls.turns.iter().all(|t| t.2 == 1));
        assert!(!app.persist().contains("Explain the translation"));
    }
    #[test]
    fn copilot_permission_is_per_operation_and_stop_does_not_apply_a_comment() {
        let (mut app, _) = app();
        let backend = Arc::new(FakeBackend { approval: true, ..Default::default() });
        app.copilot_backend = Some(backend.clone());
        app.send_copilot("Make a note".into());
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.assistant.chat.documents[0].permission.is_none() && Instant::now() < deadline {
            app.assistant.chat.documents[0].poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(app.assistant.chat.documents[0].permission.is_some());
        assert!(backend.calls.lock().unwrap().decisions.is_empty());
        app.assistant.chat.documents[0].decide(false);
        wait_chat(&mut app);
        app.send_copilot("Make another note".into());
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.assistant.chat.documents[0].permission.is_none() && Instant::now() < deadline {
            app.assistant.chat.documents[0].poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(app.assistant.chat.documents[0].permission.is_some());
        app.assistant.chat.documents[0].decide(true);
        wait_chat(&mut app);
        app.send_copilot("One more note".into());
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.assistant.chat.documents[0].permission.is_none() && Instant::now() < deadline {
            app.assistant.chat.documents[0].poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(app.assistant.chat.documents[0].permission.is_some());
        app.assistant.chat.documents[0].stop();
        wait_chat(&mut app);
        assert_eq!(backend.calls.lock().unwrap().decisions, [false, true]);
        assert!(app.session.docs().iter().all(|d| d.info.annotations.is_empty()));
    }
    #[test]
    fn copilot_reuses_native_comment_save_and_refuses_stale_sources() {
        let (mut app, _) = app();
        app.copilot_backend = Some(Arc::new(FakeBackend::default()));
        app.send_copilot("Review this page".into());
        wait_chat(&mut app);
        let message = &app.assistant.chat.documents[0].messages[1];
        app.assistant.source = message.source.clone();
        app.assistant.answer = message.text.clone();
        assert!(app.assistant_add_comment());
        let id = app.active_ids().unwrap().1;
        let saved = app.session.save_bytes(id).unwrap();
        let mut reopened = PdfCraftApp::new();
        reopened.open_bytes("reviewed.pdf", None, saved.as_ref().clone()).unwrap();
        assert_eq!(reopened.session.docs()[0].info.annotations.len(), 1);
        app.undo();
        assert!(app.session.get(id).unwrap().info.annotations.is_empty());
        assert!(!app.assistant_add_comment());
    }

    #[test]
    fn copilot_respects_copy_permissions_before_connecting() {
        let (mut app, _) = app();
        let backend = Arc::new(FakeBackend::default());
        app.copilot_backend = Some(backend.clone());
        let id = app.active_ids().unwrap().1;
        app.session
            .apply(id, Edit::Protect(pdfcraft_engine::Protection { permissions_password: Some("synthetic-owner".into()), ..Default::default() }))
            .unwrap();
        let saved = app.session.save_bytes(id).unwrap();
        app.open_bytes("restricted.pdf", None, saved.as_ref().clone()).unwrap();
        app.send_copilot("Summarize this page".into());
        assert!(backend.calls.lock().unwrap().names.is_empty());
        assert!(app.assistant.chat.documents.is_empty());
    }

    #[test]
    fn hidden_copilot_is_polled_and_closing_a_document_drops_its_session() {
        let (mut app, _) = app();
        app.copilot_backend = Some(Arc::new(FakeBackend::default()));
        app.send_copilot("Review this page".into());
        let id = app.active_ids().unwrap().1;
        app.select_tool("comment");
        let deadline = Instant::now() + Duration::from_secs(10);
        let ctx = egui::Context::default();
        while app.assistant.chat.documents[0].running && Instant::now() < deadline {
            app.poll_copilot(&ctx);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!app.assistant.chat.documents[0].running);
        assert_eq!(app.assistant.chat.documents[0].messages[1].text, "Synthetic reply.");
        app.session.close(id);
        app.poll_copilot(&ctx);
        assert!(app.assistant.chat.documents.is_empty());
    }
    fn propose(app: &mut PdfCraftApp) -> DocId {
        app.copilot_backend = Some(Arc::new(FakeBackend { proposal: true, ..Default::default() }));
        let id = app.active_ids().unwrap().1;
        app.send_copilot("Review the diagram and prepare changes".into());
        wait_chat(app);
        assert!(app.assistant.chat.documents.iter().find(|state| state.id == id).unwrap().proposal.is_some());
        id
    }

    #[test]
    fn proposed_changes_need_review_and_apply_then_save_as_one_undoable_transaction() {
        let (mut app, _) = app();
        let id = propose(&mut app);
        assert!(!app.apply_copilot_changes(id));
        assert!(app.session.get(id).unwrap().info.annotations.is_empty());
        app.review_copilot_changes(id);
        wait_chat(&mut app);
        let prepared = app.assistant.chat.documents[0].prepared.as_ref().unwrap();
        assert!(prepared.details.iter().any(|detail| detail.contains("BLUE BOX")));
        assert!(app.session.get(id).unwrap().info.annotations.is_empty());
        assert!(app.apply_copilot_changes(id));
        assert_eq!(app.session.get(id).unwrap().info.annotations.len(), 2);
        let saved = app.session.save_bytes(id).unwrap();
        let mut reopened = PdfCraftApp::new();
        reopened.open_bytes("reviewed.pdf", None, saved.as_ref().clone()).unwrap();
        assert_eq!(reopened.session.docs()[0].info.annotations.len(), 2);
        app.undo();
        assert!(app.session.get(id).unwrap().info.annotations.is_empty(), "the complete proposal is one undo step");
        app.redo();
        assert_eq!(app.session.get(id).unwrap().info.annotations.len(), 2);
        assert!(!app.apply_copilot_changes(id), "a review cannot apply twice");
    }

    #[test]
    fn prepared_changes_use_native_review_controls() {
        let (mut app, _) = app();
        let id = propose(&mut app);
        app.review_copilot_changes(id);
        wait_chat(&mut app);
        let mut h = Harness::builder().with_size(egui::vec2(1400.0, 1000.0)).build_eframe(move |_| app);
        h.run_steps(4);
        assert!(h.query_by_label("Apply changes").is_some());
        assert!(h.query_by_label("Discard").is_some());
        assert!(h.query_by_label("Message Copilot").is_some());
        assert!(h.query_by_label("Server address").is_none());
        assert!(h.query_by_label("Translate into").is_none());
        if let Ok(path) = std::env::var("PDFCRAFT_COPILOT_SHOT") {
            h.render().unwrap().save(path).unwrap();
        }
        h.get_by_label("Discard").click();
        h.run_steps(2);
        assert!(h.query_by_label("Apply changes").is_none());
        assert!(h.state().session.get(id).unwrap().info.annotations.is_empty());
    }

    #[test]
    fn proposed_changes_refuse_another_tab_and_a_stale_revision() {
        let (mut app, _) = app();
        let id = propose(&mut app);
        app.review_copilot_changes(id);
        wait_chat(&mut app);
        let pdf = app.session.create_from_text("Other", "Other page.").unwrap();
        app.open_bytes("other.pdf", None, pdf.as_ref().clone()).unwrap();
        assert!(!app.apply_copilot_changes(id));
        assert!(app.session.docs().iter().all(|doc| doc.info.annotations.is_empty()));
        app.active = app.views.iter().position(|view| view.id == id);
        app.apply_edit(Edit::RotatePages { pages: vec![0], degrees: 90 });
        assert!(!app.apply_copilot_changes(id));
        assert!(app.session.get(id).unwrap().info.annotations.is_empty());
    }

    #[test]
    fn stopping_review_or_starting_a_new_turn_never_applies_a_proposal() {
        let (mut app, _) = app();
        let id = propose(&mut app);
        app.review_copilot_changes(id);
        app.assistant.chat.documents[0].stop();
        wait_chat(&mut app);
        assert!(app.assistant.chat.documents[0].prepared.is_none());
        assert!(!app.apply_copilot_changes(id));
        app.send_copilot("Prepare a new review".into());
        wait_chat(&mut app);
        assert!(app.assistant.chat.documents[0].proposal.is_some());
        app.send_copilot("Summarize instead".into());
        assert!(app.assistant.chat.documents[0].proposal.is_none());
        assert!(app.session.get(id).unwrap().info.annotations.is_empty());
        wait_chat(&mut app);
    }

    #[cfg(feature = "html-review")]
    #[test]
    fn reviewed_changes_travel_inside_html_and_remain_undoable() {
        let (mut app, _) = app();
        let id = app.active_ids().unwrap().1;
        let pdf = app.session.get(id).unwrap().bytes.clone();
        let source = "<!doctype html><html><body><p>BLUE BOX connects to GREEN BOX.</p></body></html>";
        let html = pdfcraft_html_review::native::save_native(source, &pdf, serde_json::json!([])).unwrap();
        app.open_bytes("diagram.html", None, html).unwrap();
        let id = propose(&mut app);
        app.review_copilot_changes(id);
        wait_chat(&mut app);
        assert!(app.apply_copilot_changes(id));
        let saved = app.session.save_bytes(id).unwrap();
        assert!(std::str::from_utf8(&saved).unwrap().starts_with(source));
        let mut reopened = PdfCraftApp::new();
        reopened.open_bytes("reviewed.html", None, saved.as_ref().clone()).unwrap();
        let doc = &reopened.session.docs()[0];
        assert!(doc.is_html());
        assert_eq!(doc.info.annotations.len(), 2);
        app.undo();
        assert!(app.session.get(id).unwrap().info.annotations.is_empty());
    }

    #[test]
    fn an_answer_becomes_a_normal_undoable_comment_but_stale_answers_are_refused() {
        let (mut app, _) = app();
        app.assistant.enabled = true;
        app.ask_assistant(Task::Summarize);
        wait(&mut app);
        assert!(app.assistant_add_comment());
        let id = app.active_ids().unwrap().1;
        assert_eq!(app.session.get(id).unwrap().info.annotations.len(), 1);
        let saved = app.session.save_bytes(id).unwrap();
        let mut reopened = PdfCraftApp::new();
        reopened.open_bytes("saved.pdf", None, saved.as_ref().clone()).unwrap();
        assert_eq!(reopened.session.docs()[0].info.annotations.len(), 1);
        app.undo();
        assert_eq!(app.session.get(id).unwrap().info.annotations.len(), 0);
        app.redo();
        assert_eq!(app.session.get(id).unwrap().info.annotations.len(), 1);
        assert!(!app.assistant_add_comment());
        assert!(app.assistant.error.as_deref().unwrap().contains("changed"));
    }
    #[test]
    fn cancellation_discards_a_result_and_does_not_admit_another_worker() {
        let (mut app, sender) = app();
        app.assistant.enabled = true;
        app.ask_assistant(Task::Summarize);
        app.assistant.cancel();
        app.ask_assistant(Task::Summarize);
        wait(&mut app);
        assert!(app.assistant.answer.is_empty());
        assert_eq!(sender.0.lock().unwrap().len(), 1);
    }
    #[test]
    fn changing_tabs_cannot_put_an_answer_in_another_document() {
        let (mut app, _) = app();
        app.assistant.enabled = true;
        app.ask_assistant(Task::Summarize);
        wait(&mut app);
        let pdf = app.session.create_from_text("Other", "Other document.").unwrap();
        app.open_bytes("other.pdf", None, pdf.as_ref().clone()).unwrap();
        assert!(!app.assistant_add_comment());
        assert!(app.session.docs().iter().all(|d| d.info.annotations.is_empty()));
    }
    #[test]
    fn selection_context_sends_only_the_selected_passage() {
        let (mut app, sender) = app();
        let (i, id) = app.active_ids().unwrap();
        let doc = app.session.get(id).unwrap();
        let mut renderer = pdfcraft_render::PageRenderer::new(doc.bytes.clone(), Default::default());
        let rendered = renderer.render(pdfcraft_render::RenderRequest {
            page: 0,
            kind: pdfcraft_render::RequestKind::Text,
            tile: None,
            scale: 1.0,
            tag: 0,
            preview: false,
        });
        app.views[i].texts.insert(0, rendered.text.unwrap());
        app.views[i].select_text(0, 0, 4);
        app.assistant.enabled = true;
        app.ask_assistant(Task::Summarize);
        wait(&mut app);
        let seen = sender.0.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].contains("BLUE"));
        assert!(!seen[0].contains("GREEN"));
        assert!(app.assistant.source.as_ref().unwrap().selection);
    }
    #[test]
    fn panel_entry_cannot_send_text_that_the_pdf_forbids_copying() {
        let (mut app, sender) = app();
        let id = app.active_ids().unwrap().1;
        app.session
            .apply(id, Edit::Protect(pdfcraft_engine::Protection { permissions_password: Some("synthetic-owner".into()), ..Default::default() }))
            .unwrap();
        let bytes = app.session.save_bytes(id).unwrap();
        app.open_bytes("restricted.pdf", None, bytes.as_ref().clone()).unwrap();
        assert!(!app.session.get(app.active_ids().unwrap().1).unwrap().allows_copying());
        app.assistant.enabled = true;
        app.ask_assistant(Task::Summarize);
        assert!(!app.assistant.busy());
        assert!(sender.0.lock().unwrap().is_empty());
    }
    #[cfg(feature = "html-review")]
    #[test]
    fn assistant_comment_travels_inside_saved_html_and_reopens() {
        let (mut app, _) = app();
        let id = app.active_ids().unwrap().1;
        let pdf = app.session.get(id).unwrap().bytes.clone();
        let source = "<!doctype html><html><body><p>BLUE BOX connects to GREEN BOX.</p></body></html>";
        let html = pdfcraft_html_review::native::save_native(source, &pdf, serde_json::json!([])).unwrap();
        app.open_bytes("synthetic.html", None, html).unwrap();
        app.assistant.enabled = true;
        app.ask_assistant(Task::Summarize);
        wait(&mut app);
        assert!(app.assistant_add_comment());
        let id = app.active_ids().unwrap().1;
        let saved = app.session.save_bytes(id).unwrap();
        assert!(std::str::from_utf8(&saved).unwrap().starts_with(source));
        let mut reopened = PdfCraftApp::new();
        reopened.open_bytes("saved.html", None, saved.as_ref().clone()).unwrap();
        let doc = reopened.session.get(reopened.active_ids().unwrap().1).unwrap();
        assert!(doc.is_html());
        assert_eq!(doc.info.annotations.len(), 1);
    }
}
