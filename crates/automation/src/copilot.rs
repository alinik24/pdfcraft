//! Opt-in headless access to the same document agent and reviewed native edits.
use super::*;
use pdfcraft_ai::copilot::{Conversation, DocumentContext, Turn, Update};
use pdfcraft_ai::workspace::DocumentIdentity;

pub(super) struct Chat {
    conversation: Box<dyn Conversation>,
    answer: String,
    activity: String,
    permission: Option<(Value, String, String)>,
    proposal: Option<(String, String)>,
    finished: bool,
    error: Option<String>,
}

impl Chat {
    fn drain(&mut self) -> Result<()> {
        for _ in 0..128 {
            let Some(update) = self.conversation.poll().map_err(failed)? else { break };
            match update {
                Update::Ready => {}
                Update::Text(text) => {
                    let remaining = 100_000usize.saturating_sub(self.answer.len());
                    self.answer.push_str(text.get(..text.floor_char_boundary(remaining.min(text.len()))).unwrap_or_default());
                }
                Update::Tool(title) => self.activity = title.chars().take(256).collect(),
                Update::Permission { id, title, details } if self.permission.is_none() => {
                    self.permission = Some((id, title.chars().take(256).collect(), details.chars().take(4000).collect()));
                }
                Update::Permission { id, .. } => self.conversation.decide(id, false).map_err(failed)?,
                Update::Proposal { path, summary } if path.len() <= 4096 => {
                    self.proposal = Some((path, summary.chars().take(12_000).collect()));
                }
                Update::Proposal { .. } => return Err(failed("the proposed change path exceeds the supported size")),
                Update::Finished => self.finished = true,
                Update::Failed(error) => {
                    self.error = Some(error.chars().take(4000).collect());
                    self.finished = true;
                }
            }
        }
        Ok(())
    }

    fn status(&self, id: DocId) -> Value {
        json!({"doc":id.0,"finished":self.finished,"answer":self.answer,"activity":self.activity,"error":self.error,
            "permission":self.permission.as_ref().map(|(id,title,details)|json!({"request_id":id,"title":title,"details":details})),
            "proposal":self.proposal.as_ref().map(|(_,summary)|json!({"summary":summary})),
            "workspace":self.conversation.workspace().to_string_lossy()})
    }
}

impl Automation {
    pub(super) fn copilot_request(&mut self, name: &str, a: &Args) -> Result<Value> {
        let id = self.doc(a)?.id;
        if name == "copilot_stop" {
            if let Some(mut chat) = self.copilots.remove(&id) {
                chat.conversation.cancel().map_err(failed)?;
            }
            return Ok(json!({"doc":id.0,"stopped":true}));
        }
        if name == "copilot_chat" {
            let message = a.str("message")?.to_owned();
            if message.trim().is_empty() || message.len() > 16_000 {
                return Err(failed("message must contain between 1 and 16000 bytes"));
            }
            let doc = self.doc_to_copy(a)?;
            let page = self.page(a)?;
            let revision = doc.edit_generation();
            let bytes = doc.bytes.clone();
            let document_name = doc.name.clone();
            let path = doc.path.clone();
            let text = self.page_texts(id, &[page])?.into_iter().next().ok_or_else(|| failed("this page has no readable text"))?.plain_text();
            if !self.copilots.contains_key(&id) {
                let backend = self.copilot_backend.clone().ok_or_else(|| failed("the Hermes document copilot is not configured"))?;
                // Check before connect: that operation creates the document workspace.
                if self.root.is_some() || self.roots.is_some() {
                    let root = backend.workspace_root().ok_or_else(|| failed("the copilot backend does not declare its workspace root"))?;
                    self.resolve(root.to_str().ok_or_else(|| failed("the copilot workspace path is not UTF-8"))?, true)?;
                }
                let identity = match path {
                    Some(path) => DocumentIdentity::source(Path::new(&path)),
                    None => DocumentIdentity::unsaved(
                        &format!("automation-{}-{}-{:?}", std::process::id(), id.0, std::time::SystemTime::now()),
                        &document_name,
                    ),
                }
                .map_err(failed)?;
                let conversation = backend.connect(DocumentContext { identity, name: document_name }).map_err(failed)?;
                self.copilots.insert(
                    id,
                    Chat {
                        conversation,
                        answer: String::new(),
                        activity: String::new(),
                        permission: None,
                        proposal: None,
                        finished: true,
                        error: None,
                    },
                );
            }
            let chat = self.copilots.get_mut(&id).ok_or_else(|| failed("the document copilot is unavailable"))?;
            chat.drain()?;
            if !chat.finished {
                return Err(failed("the document copilot is still working; check its status or stop it"));
            }
            chat.conversation.send(Turn { pdf: bytes, revision, page: page + 1, text, message }).map_err(failed)?;
            chat.answer.clear();
            chat.activity.clear();
            chat.permission = None;
            chat.proposal = None;
            chat.error = None;
            chat.finished = false;
        }
        if name == "copilot_apply_proposal" {
            let snapshot = pdfcraft_engine::review::ReviewSnapshot::from_document(self.doc(a)?).map_err(failed)?;
            let chat = self.copilots.get_mut(&id).ok_or_else(|| failed("no document copilot session exists"))?;
            chat.drain()?;
            if !chat.finished {
                return Err(failed("wait for the copilot to finish before applying changes"));
            }
            let (path, _) = chat.proposal.as_ref().ok_or_else(|| failed("no reviewed change proposal is available"))?;
            let bytes = chat.conversation.read_output(Path::new(path), pdfcraft_ai::review::MAX_PROPOSAL_BYTES).map_err(failed)?;
            let proposal = pdfcraft_ai::review::parse(&bytes, snapshot.revision()).map_err(failed)?;
            let edit =
                pdfcraft_engine::review::prepare_review(&snapshot, &proposal, |p, max| chat.conversation.read_output(p, max)).map_err(failed)?;
            let result = self.apply(a, edit)?;
            if let Some(chat) = self.copilots.get_mut(&id) {
                chat.proposal = None;
            }
            return Ok(result);
        }
        let chat = self.copilots.get_mut(&id).ok_or_else(|| failed("no document copilot session exists"))?;
        chat.drain()?;
        if name == "copilot_respond" {
            let supplied = a.0.get("request_id").ok_or_else(|| failed("request_id is required"))?;
            let (expected, _, _) = chat.permission.as_ref().ok_or_else(|| failed("no approval is pending"))?;
            if supplied != expected {
                return Err(failed("request_id does not match the pending approval"));
            }
            chat.conversation.decide(expected.clone(), a.opt_bool("allow_once")?.unwrap_or(false)).map_err(failed)?;
            chat.permission = None;
        }
        Ok(chat.status(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Canned {
        root: PathBuf,
        connects: Arc<AtomicUsize>,
    }
    struct Reply {
        root: PathBuf,
        queue: VecDeque<Update>,
        packet: Vec<u8>,
    }
    impl pdfcraft_ai::copilot::Backend for Canned {
        fn workspace_root(&self) -> Option<&Path> {
            Some(&self.root)
        }
        fn connect(&self, _: DocumentContext) -> std::result::Result<Box<dyn Conversation>, String> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Reply { root: self.root.clone(), queue: VecDeque::new(), packet: Vec::new() }))
        }
    }
    impl Conversation for Reply {
        fn send(&mut self, turn: Turn) -> std::result::Result<(), String> {
            self.packet=serde_json::to_vec(&json!({"format":pdfcraft_ai::review::FORMAT,"revision":turn.revision,"summary":"Add synthetic note","changes":[{"kind":"comment","page":1,"text":"Reviewed note"}]})).map_err(|e|e.to_string())?;
            self.queue.extend([
                Update::Text("Prepared a note.".into()),
                Update::Proposal { path: "outputs/review.json".into(), summary: "Add synthetic note".into() },
                Update::Finished,
            ]);
            Ok(())
        }
        fn poll(&mut self) -> std::result::Result<Option<Update>, String> {
            Ok(self.queue.pop_front())
        }
        fn cancel(&mut self) -> std::result::Result<(), String> {
            self.queue.clear();
            Ok(())
        }
        fn decide(&mut self, _: Value, _: bool) -> std::result::Result<(), String> {
            Ok(())
        }
        fn workspace(&self) -> &Path {
            &self.root
        }
        fn read_output(&self, _: &Path, max: usize) -> std::result::Result<Vec<u8>, String> {
            if self.packet.len() > max { Err("limit".into()) } else { Ok(self.packet.clone()) }
        }
    }
    fn fixture(root: PathBuf) -> (Automation, DocId, Arc<AtomicUsize>) {
        let mut a = Automation::new();
        let bytes = a.session.create_from_text("Review", "Synthetic review text.").unwrap();
        let id = a.session.open("review.pdf", None, bytes, None).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        a.copilot_backend = Some(Arc::new(Canned { root, connects: count.clone() }));
        (a, id, count)
    }
    #[test]
    fn workspace_root_is_checked_before_backend_connects() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let (a, id, count) = fixture(outside.path().to_owned());
        let mut a = a.with_root(dir.path()).unwrap();
        assert!(a.call("copilot_chat", &json!({"doc":id.0,"page":1,"message":"Review"})).is_err());
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn proposal_requires_explicit_apply_has_undo_and_refuses_stale_revision() {
        let dir = tempfile::tempdir().unwrap();
        let (mut a, id, count) = fixture(dir.path().to_owned());
        let args = json!({"doc":id.0,"page":1,"message":"Add note"});
        a.call("copilot_chat", &args).unwrap();
        a.call("copilot_status", &json!({"doc":id.0})).unwrap();
        assert!(a.session.get(id).unwrap().info.annotations.is_empty());
        a.call("copilot_apply_proposal", &json!({"doc":id.0})).unwrap();
        assert_eq!(a.session.get(id).unwrap().info.annotations.len(), 1);
        a.call("edit_undo", &json!({"doc":id.0})).unwrap();
        assert!(a.session.get(id).unwrap().info.annotations.is_empty());
        a.call("copilot_chat", &args).unwrap();
        a.call("page_rotate", &json!({"doc":id.0,"pages":[1],"degrees":90})).unwrap();
        assert!(a.call("copilot_apply_proposal", &json!({"doc":id.0})).is_err());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        a.call("copilot_stop", &json!({"doc":id.0})).unwrap();
        assert!(a.copilots.is_empty());
    }
}
