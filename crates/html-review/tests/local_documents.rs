//! Opt-in private corpus check: paths/content never become build inputs or assets.
use pdfcraft_html_review::{Annotation, AnnotationStatus, ReviewDocument};
use std::collections::BTreeMap;

#[test]
fn local_documents_keep_source_images_and_scoped_annotations_after_reopen() {
    let Some(dir) = std::env::var_os("PDFCRAFT_HTML_FIXTURE_DIR") else {
        return;
    };
    let out =
        std::env::var_os("PDFCRAFT_HTML_EVIDENCE_DIR").map(std::path::PathBuf::from).expect("An evidence directory is required with local fixtures");
    std::fs::create_dir_all(&out).unwrap();
    let mut count = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("html") {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        let mut document = ReviewDocument::parse(&source).unwrap();
        let (target, text) = document.targets().iter().find(|(id, text)| id.as_str() != "html:body" && !text.trim().is_empty()).unwrap();
        let exact: String = text.chars().take(12).collect();
        let end = exact.chars().count();
        let annotation = Annotation {
            id: "urn:pdfcraft:private-corpus-check".into(),
            target_id: target.clone(),
            exact,
            prefix: String::new(),
            suffix: text.chars().skip(end).take(32).collect(),
            start: 0,
            end,
            body: "Local round-trip check; not accepted as a correction".into(),
            color: "yellow".into(),
            created: "2026-10-24T00:00:00Z".into(),
            reply_to: None,
            status: AnnotationStatus::Pending,
            metadata: BTreeMap::new(),
        };
        document.replace_annotations(vec![annotation.clone()]).unwrap();
        let saved = document.saved_html().unwrap();
        assert!(saved.starts_with(&source), "Original source bytes must remain unchanged");
        let reopened = ReviewDocument::parse(&saved).unwrap();
        assert_eq!(reopened.content_sha256(), document.content_sha256());
        assert_eq!(reopened.annotations(), &[annotation]);
        let mut merged = ReviewDocument::parse(&source).unwrap();
        assert_eq!(merged.merge_review(reopened.export_review()).unwrap(), 1);
        assert_eq!(merged.merge_review(reopened.export_review()).unwrap(), 0);
        let name = path.file_name().unwrap();
        std::fs::write(out.join(name), saved).unwrap();
        println!("verified document={} targets={} hash={}", document.document_id(), document.targets().len(), document.content_sha256());
        count += 1;
    }
    assert!(count >= 3, "Expect all supplied review fixtures");
}
