use super::*;
use serde_json::json;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Canned(Mutex<Vec<String>>);
impl pdfcraft_ai::Transport for Canned {
    fn post(&self, r: &pdfcraft_ai::Request) -> std::result::Result<pdfcraft_ai::Response, pdfcraft_ai::AiError> {
        self.0.lock().unwrap().push(r.body.clone());
        Ok(pdfcraft_ai::Response { status: 200, body: r#"{"choices":[{"message":{"content":"Synthetic page answer."}}]}"#.into() })
    }
}

fn fixture() -> (Automation, DocId, Arc<Canned>) {
    let mut a = Automation::new();
    let bytes = a.session.create_from_text("Synthetic", "The blue box connects to the green box.").unwrap();
    let id = a.session.open("synthetic.pdf", None, bytes, None).unwrap();
    let sender = Arc::new(Canned::default());
    a.ai_provider = Some(pdfcraft_ai::Provider::new(pdfcraft_ai::Api::OpenAi, "https://example.invalid/v1", "synthetic-model"));
    a.ai_transport = Some(sender.clone());
    (a, id, sender)
}

#[test]
fn assistant_tools_share_document_permissions_and_leave_the_file_unchanged() {
    let (mut a, id, sender) = fixture();
    let before = a.session.get(id).unwrap().bytes.clone();
    for (name, extra) in
        [("ai_summarize", json!({})), ("ai_ask", json!({"question":"What connects?"})), ("ai_translate", json!({"language":"German"}))]
    {
        let mut args = json!({"doc":id.0,"page":1});
        args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let out = a.call(name, &args).unwrap();
        let Content::Json(v) = &out[0] else { panic!("expected structured answer") };
        assert_eq!(v["page"], 1);
        assert_eq!(v["answer"], "Synthetic page answer.");
    }
    assert_eq!(sender.0.lock().unwrap().len(), 3);
    assert!(Arc::ptr_eq(&before, &a.session.get(id).unwrap().bytes));
    assert!(a.session.get(id).unwrap().info.annotations.is_empty());
    let count = sender.0.lock().unwrap().len();
    assert!(a.call("ai_summarize", &json!({"doc":id.0,"page":0})).is_err());
    assert!(a.call("ai_summarize", &json!({"doc":id.0,"page":999})).is_err());
    assert_eq!(sender.0.lock().unwrap().len(), count);
}

#[test]
fn unconfigured_or_restricted_documents_never_reach_the_provider() {
    let (mut a, id, sender) = fixture();
    let provider = a.ai_provider.take();
    assert!(a.call("ai_summarize", &json!({"doc":id.0,"page":1})).is_err());
    a.ai_provider = provider;
    a.session
        .apply(id, Edit::Protect(pdfcraft_engine::Protection { permissions_password: Some("synthetic-owner".into()), ..Default::default() }))
        .unwrap();
    let protected = a.session.save_bytes(id).unwrap();
    let restricted = a.session.open("restricted.pdf", None, protected, None).unwrap();
    assert!(!a.session.get(restricted).unwrap().allows_copying());
    assert!(a.call("ai_summarize", &json!({"doc":restricted.0,"page":1})).is_err());
    assert!(sender.0.lock().unwrap().is_empty());
}
