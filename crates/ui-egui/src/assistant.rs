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
}

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
    app.assistant.poll();
    if app.assistant.busy() {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
    }
    egui::ScrollArea::vertical().id_salt("ai-assistant-panel").show(ui, |ui| {
        widgets::begin_dialog(ui);
        ui.label(tl!("Ask about selected text or the current page."));
        let busy = app.assistant.busy();
        ui.add_enabled_ui(!busy, |ui| {
            ui.add(widgets::checkbox(&mut app.assistant.enabled, tl!("Enable AI assistant")));
            ui.collapsing(tl!("Connection settings"), |ui| {
                let label = ui.label(tl!("Server address"));
                ui.add(widgets::line(&mut app.assistant.endpoint).char_limit(ai::MAX_ENDPOINT_CHARS).desired_width(f32::INFINITY)).labelled_by(label.id);
                let label = ui.label(tl!("Model"));
                ui.add(widgets::line(&mut app.assistant.model).char_limit(ai::MAX_MODEL_CHARS).desired_width(f32::INFINITY)).labelled_by(label.id);
                let label = ui.label(tl!("API key (optional, this session only)"));
                ui.add(widgets::line(&mut app.assistant.key).password(true).char_limit(ai::MAX_KEY_CHARS).desired_width(f32::INFINITY)).labelled_by(label.id);
            });
            ui.label(egui::RichText::new(tl!("Only the selected passage or current page is sent to your chosen model when you ask. Answers can be wrong; review them before saving.")).color(t.text_muted).size(12.0));
            ui.horizontal_wrapped(|ui| {
                if widgets::pill_button(ui, tl!("Summarize"), false).clicked() { app.ask_assistant(Task::Summarize); }
                if widgets::pill_button(ui, tl!("Explain"), false).clicked() { app.ask_assistant(Task::Ask { question: "Explain this passage clearly, citing the supplied page.".into() }); }
            });
            let label = ui.label(tl!("Ask about this document"));
            ui.add(widgets::block(&mut app.assistant.question).char_limit(ai::MAX_QUESTION_CHARS).desired_width(f32::INFINITY).desired_rows(3)).labelled_by(label.id);
            // The question remains normal text; agent commands and schemas are never shown here.
            if widgets::pill_button(ui, tl!("Ask"), true).clicked() { app.ask_assistant(Task::Ask { question: app.assistant.question.clone() }); }
            let label = ui.label(tl!("Translate into"));
            ui.add(widgets::line(&mut app.assistant.language).char_limit(ai::MAX_LANGUAGE_CHARS).desired_width(f32::INFINITY)).labelled_by(label.id);
            if widgets::pill_button(ui, tl!("Translate"), false).clicked() { app.ask_assistant(Task::Translate { language: app.assistant.language.clone() }); }
        });
        if app.assistant.busy() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(if app.assistant.cancelled { tl!("Stopping…") } else { tl!("Thinking…") });
                if !app.assistant.cancelled && widgets::pill_button(ui, tl!("Cancel"), false).clicked() { app.assistant.cancel(); }
            });
        }
        if let Some(e) = &app.assistant.error { ui.colored_label(t.text_muted, e); }
        if !app.assistant.answer.is_empty() {
            ui.separator();
            if app.assistant.truncated { ui.label(tl!("Only part of the text fitted in this request.")); }
            if let Some(source) = &app.assistant.source {
                ui.label(format!("{} · {} {} · {}", source.name, tl!("Page"), source.page + 1,
                    if source.selection { tl!("Selected text") } else { tl!("Current page") }));
            }
            ui.add(egui::Label::new(&app.assistant.answer).selectable(true).wrap());
            ui.horizontal_wrapped(|ui| {
                if widgets::pill_button(ui, tl!("Copy"), false).clicked() { ui.ctx().copy_text(app.assistant.answer.clone()); }
                if widgets::pill_button(ui, tl!("Add as comment"), false).clicked() { app.assistant_add_comment(); }
                if widgets::pill_button(ui, tl!("Clear"), false).clicked() { app.assistant.answer.clear(); app.assistant.source = None; }
            });
        }
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
    fn opening_the_native_panel_is_off_and_never_sends_until_asked() {
        let (app, sender) = app();
        let mut h = Harness::builder().with_size(egui::vec2(1400.0, 1000.0)).build_eframe(move |_| app);
        h.run_steps(3);
        assert!(sender.0.lock().unwrap().is_empty());
        h.get_by_label("Summarize").click();
        h.run_steps(2);
        assert!(sender.0.lock().unwrap().is_empty());
        assert!(!h.state().assistant.busy());
        h.get_by_label("Enable AI assistant").click();
        h.run_steps(1);
        h.get_by_label("Summarize").click();
        for _ in 0..200 {
            h.run_steps(1);
            if !h.state().assistant.busy() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(h.state().assistant.answer, "Synthetic answer (p. 1).");
        assert_eq!(sender.0.lock().unwrap().len(), 1);
        assert!(sender.0.lock().unwrap()[0].contains("BLUE BOX"));
        let saved = h.state().persist();
        assert!(!saved.contains("synthetic-session-only-key"));
        assert!(!saved.contains("Synthetic answer"));
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
