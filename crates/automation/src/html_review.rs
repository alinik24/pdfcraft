//! Feature-gated, local-file adapters for the experimental HTML review engine.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use pdfcraft_html_review::ReviewDocument;
use pdfcraft_html_review::native;
use serde_json::{Value, json};

use crate::{Args, Automation, Result, ToolError, failed, write_atomic};

const MAX_HTML: u64 = 16 * 1024 * 1024;
const MAX_REVIEW: u64 = 4 * 1024 * 1024;
const MAX_REPLACEMENT: usize = 60 * 1024;

struct LoadedDocument {
    source: Option<ReviewDocument>,
    native_annotations: Option<Value>,
}

impl LoadedDocument {
    fn document_id(&self) -> String {
        self.source.as_ref().map_or_else(|| format!("sha256:{}", self.content_sha256()), |source| source.document_id().to_owned())
    }

    fn content_sha256(&self) -> &str {
        self.source.as_ref().map_or_else(
            || self.native_annotations.as_ref().and_then(|metadata| metadata.get("source_sha256")).and_then(Value::as_str).unwrap_or_default(),
            ReviewDocument::content_sha256,
        )
    }

    fn native_annotation_array(&self) -> Option<&Vec<Value>> {
        let metadata = self.native_annotations.as_ref()?;
        metadata.pointer("/review/annotations").or_else(|| metadata.get("review")).and_then(Value::as_array)
    }

    fn annotation_count(&self) -> usize {
        if self.native_annotations.is_some() {
            self.native_annotation_array().map_or(0, Vec::len)
        } else {
            self.source.as_ref().map_or(0, |source| source.annotations().len())
        }
    }

    fn export_review(&self) -> Value {
        match &self.native_annotations {
            Some(metadata) => json!({
                "format": "pdfcraft-native-html-review/1.0",
                "document_id": self.document_id(),
                "content_sha256": self.content_sha256(),
                "annotations": self.native_annotation_array(),
                "native_metadata": metadata,
                "source_review": self.source.as_ref().map(ReviewDocument::export_review),
                "merge_supported": false
            }),
            None => self.source.as_ref().map_or(Value::Null, ReviewDocument::export_review),
        }
    }

    fn editable_source(self) -> Result<ReviewDocument> {
        if self.native_annotations.is_some() {
            return Err(ToolError::InvalidArgs("This HTML contains a native PdfCraft page-edit layer. html_review_edit/html_review_merge cannot safely rewrite that layer. Use doc_open, the standard comment/annotation or editing tools, and doc_save to preserve native edits.".into()));
        }
        self.source.ok_or_else(|| failed("The source HTML is unavailable for structured source editing"))
    }
}

impl Automation {
    pub(super) fn doc_export_pdf(&self, args: &Args) -> Result<Value> {
        let document = self.doc(args)?;
        let source = document.path.as_deref().map(Path::new).map(Path::to_path_buf);
        let inputs: Vec<&Path> = source.iter().map(PathBuf::as_path).collect();
        let output = self.html_review_output(args.str("out")?, &inputs)?;
        let pdf = self.session.save_pdf_bytes(document.id).map_err(failed)?;
        pdfcraft_render::check_pages(pdf.clone(), document.password.as_deref(), document.info.pages.len()).map_err(failed)?;
        write_atomic(&output, &pdf)?;
        Ok(json!({"path":output,"bytes":pdf.len(),"format":"pdf","source_doc":document.id.0,"source_dirty":document.dirty}))
    }

    pub(super) fn html_review_inspect(&self, args: &Args) -> Result<Value> {
        let path = self.resolve(args.str("path")?, false)?;
        let loaded = read_document(&path)?;
        let document = &loaded.source;
        Ok(json!({
            "mode": if loaded.native_annotations.is_some() { "native" } else { "source" },
            "document_id": loaded.document_id(),
            "content_sha256": loaded.content_sha256(),
            "manifest": document.as_ref().map(ReviewDocument::manifest),
            "target_count": document.as_ref().map(|source| source.targets().len()),
            "annotation_count": loaded.annotation_count(),
            "source_annotation_count": document.as_ref().map(|source| source.annotations().len()),
            "native_annotations": loaded.native_annotation_array(),
            "native_metadata": loaded.native_annotations
        }))
    }

    pub(super) fn html_review_export(&self, args: &Args) -> Result<Value> {
        let source = self.resolve(args.str("path")?, false)?;
        let output = self.html_review_output(args.str("out")?, &[&source])?;
        let document = read_document(&source)?;
        let bytes = serde_json::to_vec_pretty(&document.export_review()).map_err(failed)?;
        write_atomic(&output, &bytes)?;
        Ok(json!({"path": output, "document_id": document.document_id(), "annotation_count": document.annotation_count()}))
    }

    pub(super) fn html_review_merge(&self, args: &Args) -> Result<Value> {
        let source = self.resolve(args.str("path")?, false)?;
        let review = self.resolve(args.str("review")?, false)?;
        let output = self.html_review_output(args.str("out")?, &[&source, &review])?;
        let mut document = read_document(&source)?.editable_source()?;
        let review: Value = serde_json::from_slice(&read_bounded(&review, MAX_REVIEW, "review JSON")?).map_err(failed)?;
        let merged = document.merge_review(review).map_err(failed)?;
        let html = document.saved_html().map_err(failed)?;
        write_atomic(&output, html.as_bytes())?;
        Ok(json!({"path": output, "document_id": document.document_id(), "merged": merged, "annotation_count": document.annotations().len()}))
    }

    pub(super) fn html_review_edit(&self, args: &Args) -> Result<Value> {
        let source = self.resolve(args.str("path")?, false)?;
        let output = self.html_review_output(args.str("out")?, &[&source])?;
        let html = args.str("html")?;
        if html.len() > MAX_REPLACEMENT {
            return Err(ToolError::InvalidArgs("replacement HTML exceeds 60 KiB UTF-8".into()));
        }
        let document = read_document(&source)?.editable_source()?;
        let revised = document.replace_target_html(args.str("target_id")?, html).map_err(failed)?;
        let saved = revised.saved_html().map_err(failed)?;
        write_atomic(&output, saved.as_bytes())?;
        Ok(json!({"path": output, "document_id": revised.document_id(), "annotation_count": revised.annotations().len()}))
    }

    /// Check write authority before resolving file identities. Atomic replacement must never
    /// publish over an input, including a differently spelled path or a symlink to that input.
    fn html_review_output(&self, path: &str, inputs: &[&Path]) -> Result<PathBuf> {
        let output = self.resolve(path, true)?;
        if output.exists() {
            let identity = output.canonicalize().map_err(failed)?;
            for input in inputs {
                if identity == input.canonicalize().map_err(failed)? {
                    return Err(ToolError::InvalidArgs("out must name a new HTML/review output, distinct from every input; source overwrite is disabled for experimental HTML review".into()));
                }
            }
        }
        Ok(output)
    }
}

fn read_document(path: &Path) -> Result<LoadedDocument> {
    let bytes = read_bounded(path, native::MAX_PACKAGE_BYTES as u64, "HTML package")?;
    if native::has_layer(&bytes) {
        let (source, _) = native::unpack_native(&bytes).map_err(failed)?;
        let annotations = native::annotation_metadata(&bytes).map_err(failed)?;
        // Native page annotations remain inspectable even when the source-review parser
        // cannot safely anchor this otherwise valid HTML (for example noscript content).
        return Ok(LoadedDocument { source: ReviewDocument::parse(&source).ok(), native_annotations: Some(annotations) });
    }
    if bytes.len() as u64 > MAX_HTML {
        return Err(failed(format!("{}: HTML input exceeds 16 MiB", path.display())));
    }
    let html = std::str::from_utf8(&bytes).map_err(|e| failed(format!("{}: HTML must be UTF-8: {e}", path.display())))?;
    Ok(LoadedDocument { source: Some(ReviewDocument::parse(html).map_err(failed)?), native_annotations: None })
}

/// Metadata catches oversized regular files before allocation; take also bounds a file that
/// grows after this check. Devices, pipes and directories are not HTML review inputs.
fn read_bounded(path: &Path, max: u64, kind: &str) -> Result<Vec<u8>> {
    let mut file = std::fs::File::open(path).map_err(|e| failed(format!("{}: {e}", path.display())))?;
    let metadata = file.metadata().map_err(failed)?;
    if !metadata.is_file() {
        return Err(failed(format!("{}: {kind} input must be a regular file", path.display())));
    }
    let oversized = || failed(format!("{}: {kind} input exceeds {} MiB", path.display(), max / (1024 * 1024)));
    if metadata.len() > max {
        return Err(oversized());
    }
    let mut bytes = Vec::new();
    if max > MAX_HTML {
        // A native marker follows at most 16 MiB of source. Read only that source plus a
        // small marker lookahead before granting the larger package budget.
        (&mut file).take(MAX_HTML.saturating_add(1024)).read_to_end(&mut bytes).map_err(failed)?;
        if bytes.len() as u64 > MAX_HTML && !native::has_layer(&bytes) {
            return Err(failed(format!("{}: HTML input exceeds 16 MiB", path.display())));
        }
    }
    let remaining = max.saturating_add(1).saturating_sub(bytes.len() as u64);
    file.take(remaining).read_to_end(&mut bytes).map_err(failed)?;
    if bytes.len() as u64 > max {
        return Err(oversized());
    }
    Ok(bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{Content, file_access::AutomationRoots};

    fn html() -> String {
        format!(
            "<!doctype html><html><body><script id=\"pdfcraft-manifest\" type=\"application/json\">{}</script><p data-pdfcraft-target-id=\"p1\">Hello café</p></body></html>",
            json!({"document_id": "synthetic-automation-review"})
        )
    }

    fn call(automation: &mut Automation, tool: &str, args: Value) -> Value {
        let result = automation.call(tool, &args).unwrap();
        match result.first().unwrap() {
            Content::Json(value) => value.clone(),
            other => panic!("expected JSON, got {other:?}"),
        }
    }

    #[test]
    fn html_review_tools_export_native_pdf_without_replacing_source() {
        let directory = tempfile::tempdir().unwrap();
        let mut automation = Automation::new().with_root(directory.path()).unwrap();
        let created = call(&mut automation, "doc_create", json!({"from":"blank"}));
        let id = pdfcraft_engine::DocId(created["doc"].as_u64().unwrap());
        let pdf = automation.session.save_pdf_bytes(id).unwrap();
        let package = native::save_native("<p>Original HTML source</p>", &pdf, json!([])).unwrap();
        std::fs::write(directory.path().join("source.html"), &package).unwrap();
        let opened = call(&mut automation, "doc_open", json!({"path":"source.html"}));
        let id = opened["doc"].clone();
        assert_eq!(opened["format"], "html");
        call(&mut automation, "comment_add", json!({"doc":id,"page":1,"type":"note","at":[30,30],"contents":"Kept in exported page layer"}));
        call(&mut automation, "doc_export_pdf", json!({"doc":id,"out":"copy.pdf"}));
        assert!(std::fs::read(directory.path().join("copy.pdf")).unwrap().starts_with(b"%PDF-"));
        assert_eq!(std::fs::read(directory.path().join("source.html")).unwrap(), package);
        assert!(automation.session.get(pdfcraft_engine::DocId(id.as_u64().unwrap())).unwrap().dirty);
        assert!(automation.call("doc_export_pdf", &json!({"doc":id,"out":"source.html"})).is_err());
        assert!(automation.call("doc_export_pdf", &json!({"doc":id,"out":"../outside.pdf"})).is_err());
        let reopened = call(&mut automation, "doc_open", json!({"path":"copy.pdf"}));
        assert_eq!(reopened["format"], "pdf");
        assert_eq!(automation.session.get(pdfcraft_engine::DocId(reopened["doc"].as_u64().unwrap())).unwrap().info.annotations.len(), 1);
    }

    #[test]
    fn html_review_tools_roundtrip_and_preserve_source() {
        let directory = tempfile::tempdir().unwrap();
        let source = html();
        std::fs::write(directory.path().join("source.html"), &source).unwrap();
        let mut automation = Automation::new().with_root(directory.path()).unwrap();
        let inspected = call(&mut automation, "html_review_inspect", json!({"path": "source.html"}));
        assert_eq!(inspected["manifest"]["document_id"], "synthetic-automation-review");
        assert!(inspected["target_count"].as_u64().unwrap() >= 1);
        assert_eq!(inspected["annotation_count"], 0);
        call(&mut automation, "html_review_export", json!({"path": "source.html", "out": "review.json"}));
        let review_path = directory.path().join("review.json");
        let mut review: Value = serde_json::from_slice(&std::fs::read(&review_path).unwrap()).unwrap();
        review["annotations"] = json!([{
            "id": "note1", "target_id": "p1", "exact": "café", "start": 6, "end": 10,
            "body": "Synthetic reviewer note <script> & Unicode", "status": "accepted"
        }]);
        // The editable flat annotations are authoritative when no derived W3C view is supplied.
        review.as_object_mut().unwrap().remove("web_annotations");
        std::fs::write(&review_path, serde_json::to_vec(&review).unwrap()).unwrap();
        let merged = call(&mut automation, "html_review_merge", json!({"path": "source.html", "review": "review.json", "out": "reviewed.html"}));
        assert_eq!(merged["merged"], 1);
        assert_eq!(merged["annotation_count"], 1);
        let reopened = call(&mut automation, "html_review_inspect", json!({"path": "reviewed.html"}));
        assert_eq!(reopened["annotation_count"], 1);
        assert_eq!(reopened["content_sha256"], inspected["content_sha256"]);
        assert_eq!(std::fs::read_to_string(directory.path().join("source.html")).unwrap(), source);
        call(&mut automation, "html_review_export", json!({"path": "reviewed.html", "out": "reexport.json"}));
        let exported: Value = serde_json::from_slice(&std::fs::read(directory.path().join("reexport.json")).unwrap()).unwrap();
        let normalized: Vec<pdfcraft_html_review::Annotation> = serde_json::from_value(review["annotations"].clone()).unwrap();
        assert_eq!(exported["annotations"], serde_json::to_value(normalized).unwrap());
    }

    #[test]
    fn html_review_tools_accept_generic_html() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("generic.html"), "<html><body><p>Hello café</p></body></html>").unwrap();
        let mut automation = Automation::new().with_root(directory.path()).unwrap();
        let inspected = call(&mut automation, "html_review_inspect", json!({"path": "generic.html"}));
        assert!(!inspected["document_id"].as_str().unwrap().is_empty());
        assert!(inspected["target_count"].as_u64().unwrap() > 0);
    }

    #[test]
    fn html_review_tools_edit_target_preserves_other_content_and_original() {
        let directory = tempfile::tempdir().unwrap();
        let original = "<!doctype html><html><body><p>Hello café</p><p id='untouched'>Keep &amp; preserve</p><!-- original spacing --></body></html>";
        let source = directory.path().join("source.html");
        std::fs::write(&source, original).unwrap();
        let mut automation = Automation::new().with_root(directory.path()).unwrap();
        call(
            &mut automation,
            "html_review_edit",
            json!({"path": "source.html", "target_id": "html:0", "html": "Revised <strong>café</strong>", "out": "edited.html"}),
        );
        let edited = std::fs::read_to_string(directory.path().join("edited.html")).unwrap();
        assert!(edited.contains("<p>Revised <strong>café</strong></p>"));
        assert!(edited.contains("<p id='untouched'>Keep &amp; preserve</p><!-- original spacing -->"));
        assert_eq!(std::fs::read_to_string(&source).unwrap(), original);
        let parsed = ReviewDocument::parse(&edited).unwrap();
        assert_eq!(parsed.targets().get("html:0").unwrap(), "Revised café");
        assert!(
            automation
                .call("html_review_edit", &json!({"path": "source.html", "target_id": "html:0", "html": "changed", "out": "source.html"}))
                .is_err()
        );
        let error = automation
            .call(
                "html_review_edit",
                &json!({"path": "source.html", "target_id": "html:0", "html": "é".repeat(MAX_REPLACEMENT), "out": "oversized.html"}),
            )
            .unwrap_err();
        assert!(error.to_string().contains("exceeds 60 KiB"));
        assert!(!directory.path().join("oversized.html").exists());
        assert_eq!(std::fs::read_to_string(source).unwrap(), original);
    }

    #[test]
    fn html_review_tools_refuse_overwrite_and_invalid_merge_without_output() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.html");
        let source = html();
        std::fs::write(&path, &source).unwrap();
        let mut automation = Automation::new().with_root(directory.path()).unwrap();
        call(&mut automation, "html_review_export", json!({"path": "source.html", "out": "review.json"}));
        for (tool, args) in [
            ("html_review_export", json!({"path": "source.html", "out": "./source.html"})),
            ("html_review_merge", json!({"path": "source.html", "review": "review.json", "out": "source.html"})),
            ("html_review_merge", json!({"path": "source.html", "review": "review.json", "out": "review.json"})),
        ] {
            let error = automation.call(tool, &args).unwrap_err();
            assert!(error.to_string().contains("source overwrite is disabled"), "{error}");
        }
        let mut wrong_review = ReviewDocument::parse(&source).unwrap().export_review();
        wrong_review["document_id"] = json!("another-document");
        std::fs::write(directory.path().join("wrong.json"), serde_json::to_vec(&wrong_review).unwrap()).unwrap();
        assert!(automation.call("html_review_merge", &json!({"path": "source.html", "review": "wrong.json", "out": "refused.html"})).is_err());
        assert!(!directory.path().join("refused.html").exists());
        assert_eq!(std::fs::read_to_string(path).unwrap(), source);
    }

    #[test]
    fn html_review_tools_honor_both_root_modes_for_every_path() {
        let directory = tempfile::tempdir().unwrap();
        let inside = directory.path().join("inside");
        let outside = directory.path().join("outside");
        std::fs::create_dir(&inside).unwrap();
        std::fs::create_dir(&outside).unwrap();
        for folder in [&inside, &outside] {
            std::fs::write(folder.join("source.html"), html()).unwrap();
            let review = ReviewDocument::parse(&html()).unwrap().export_review();
            std::fs::write(folder.join("review.json"), serde_json::to_vec(&review).unwrap()).unwrap();
        }
        let source = inside.join("source.html");
        let review = inside.join("review.json");
        let outside_source = outside.join("source.html");
        let outside_review = outside.join("review.json");
        let output = outside.join("output.html");
        let allowed_output = inside.join("output.html");
        for mut automation in [
            Automation::new().with_root(&inside).unwrap(),
            Automation::new().with_automation_roots(AutomationRoots::new(Some(&inside), Some(&inside)).unwrap()),
        ] {
            for (tool, args) in [
                ("html_review_inspect", json!({"path": outside_source})),
                ("html_review_export", json!({"path": outside_source, "out": allowed_output})),
                ("html_review_export", json!({"path": source, "out": output})),
                ("html_review_merge", json!({"path": outside_source, "review": review, "out": allowed_output})),
                ("html_review_merge", json!({"path": source, "review": outside_review, "out": allowed_output})),
                ("html_review_merge", json!({"path": source, "review": review, "out": output})),
                ("html_review_edit", json!({"path": outside_source, "target_id": "p1", "html": "new", "out": allowed_output})),
                ("html_review_edit", json!({"path": source, "target_id": "p1", "html": "new", "out": output})),
            ] {
                let error = automation.call(tool, &args).unwrap_err().to_string();
                assert!(error.contains("outside"), "{tool}: {error}");
            }
            assert!(!output.exists());
            assert!(!allowed_output.exists());
        }
        let mut read_only = Automation::new().with_automation_roots(AutomationRoots::new(Some(&inside), None).unwrap());
        call(&mut read_only, "html_review_inspect", json!({"path": source}));
        let error = read_only.call("html_review_export", &json!({"path": source, "out": allowed_output})).unwrap_err();
        assert!(error.to_string().contains("write authority is absent"));
        let mut write_only = Automation::new().with_automation_roots(AutomationRoots::new(None, Some(&inside)).unwrap());
        let error = write_only.call("html_review_inspect", &json!({"path": source})).unwrap_err();
        assert!(error.to_string().contains("read authority is absent"));
    }

    #[test]
    fn html_review_tools_cap_reads_before_parsing() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("source.html"), html()).unwrap();
        for (name, max) in [("huge.html", MAX_HTML), ("huge.json", MAX_REVIEW)] {
            std::fs::File::create(directory.path().join(name)).unwrap().set_len(max + 1).unwrap();
        }
        let mut automation = Automation::new().with_root(directory.path()).unwrap();
        let error = automation.call("html_review_inspect", &json!({"path": "huge.html"})).unwrap_err();
        assert!(error.to_string().contains("exceeds 16 MiB"));
        let error = automation.call("html_review_merge", &json!({"path": "source.html", "review": "huge.json", "out": "refused.html"})).unwrap_err();
        assert!(error.to_string().contains("exceeds 4 MiB"));
        assert!(!directory.path().join("refused.html").exists());
    }

    #[test]
    fn html_review_tools_native_inspect_export_and_refuse_source_mutations() {
        let directory = tempfile::tempdir().unwrap();
        let source = html();
        let annotations = json!([{"page": 0, "index": 0, "subtype": "Text", "contents": "Native reviewer note"}]);
        let review = json!({"format": "pdfcraft-native-annotations/1.0", "coordinate_space": "PDF user space", "page_index_base": 0, "annotations": annotations});
        let package = native::save_native(&source, b"%PDF-1.7\n%%EOF\n", review.clone()).unwrap();
        let path = directory.path().join("native.html");
        std::fs::write(&path, &package).unwrap();
        let mut automation = Automation::new().with_root(directory.path()).unwrap();
        let inspected = call(&mut automation, "html_review_inspect", json!({"path": "native.html"}));
        assert_eq!(inspected["mode"], "native");
        assert_eq!(inspected["annotation_count"], 1);
        assert_eq!(inspected["source_annotation_count"], 0);
        assert_eq!(inspected["native_annotations"], annotations);
        assert_eq!(inspected["native_metadata"]["review"], review);
        call(&mut automation, "html_review_export", json!({"path": "native.html", "out": "native.json"}));
        let exported: Value = serde_json::from_slice(&std::fs::read(directory.path().join("native.json")).unwrap()).unwrap();
        assert_eq!(exported["format"], "pdfcraft-native-html-review/1.0");
        assert_eq!(exported["annotations"], annotations);
        assert_eq!(exported["native_metadata"]["review"]["coordinate_space"], "PDF user space");
        assert_eq!(exported["merge_supported"], false);
        assert_eq!(exported["source_review"]["document_id"], "synthetic-automation-review");
        for (tool, args) in [
            ("html_review_edit", json!({"path": "native.html", "target_id": "p1", "html": "Changed source", "out": "edited.html"})),
            ("html_review_merge", json!({"path": "native.html", "review": "native.json", "out": "merged.html"})),
        ] {
            let error = automation.call(tool, &args).unwrap_err().to_string();
            assert!(error.contains("doc_open") && error.contains("doc_save"), "{error}");
        }
        assert!(!directory.path().join("edited.html").exists());
        assert!(!directory.path().join("merged.html").exists());
        assert_eq!(std::fs::read(&path).unwrap(), package);
        let corrupted = String::from_utf8(package).unwrap().replacen("Hello café", "Changed café", 1);
        std::fs::write(&path, corrupted).unwrap();
        assert!(automation.call("html_review_export", &json!({"path": "native.html", "out": "corrupt.json"})).is_err());
        assert!(!directory.path().join("corrupt.json").exists());
    }

    #[test]
    fn html_review_tools_native_package_can_exceed_plain_source_limit() {
        let directory = tempfile::tempdir().unwrap();
        // Synthetic encoded page data grows the package above 16 MiB; only the original
        // source is parsed as HTML. Native decoding validates the page bytes' hash/magic.
        let mut pdf = b"%PDF-1.7\n".to_vec();
        pdf.resize(13 * 1024 * 1024, b' ');
        let package = native::save_native(&html(), &pdf, json!([])).unwrap();
        assert!(package.len() as u64 > MAX_HTML);
        std::fs::write(directory.path().join("large-native.html"), package).unwrap();
        let mut automation = Automation::new().with_root(directory.path()).unwrap();
        let inspected = call(&mut automation, "html_review_inspect", json!({"path": "large-native.html"}));
        assert_eq!(inspected["mode"], "native");
        let oversized = std::fs::File::create(directory.path().join("oversized.html")).unwrap();
        oversized.set_len(native::MAX_PACKAGE_BYTES as u64 + 1).unwrap();
        let error = automation.call("html_review_inspect", &json!({"path": "oversized.html"})).unwrap_err().to_string();
        assert!(error.contains("exceeds 128 MiB"), "{error}");
    }

    #[test]
    fn html_review_tools_native_inspection_does_not_require_source_review_parser() {
        let directory = tempfile::tempdir().unwrap();
        let source = "<!doctype html><html><body><noscript><p>Fallback content</p></noscript><p>Native HTML</p></body></html>";
        assert!(ReviewDocument::parse(source).is_err());
        let annotations = json!([{"page": 0, "index": 0, "body": "Preserved native note"}]);
        let package = native::save_native(source, b"%PDF-1.7\n%%EOF\n", json!({"annotations": annotations})).unwrap();
        std::fs::write(directory.path().join("native.html"), &package).unwrap();
        let mut automation = Automation::new().with_root(directory.path()).unwrap();
        let inspected = call(&mut automation, "html_review_inspect", json!({"path": "native.html"}));
        assert_eq!(inspected["mode"], "native");
        assert_eq!(inspected["annotation_count"], 1);
        assert_eq!(inspected["native_annotations"], annotations);
        assert!(inspected["manifest"].is_null());
        assert!(inspected["target_count"].is_null());
        assert!(inspected["document_id"].as_str().unwrap().starts_with("sha256:"));
        assert_eq!(inspected["content_sha256"], inspected["native_metadata"]["source_sha256"]);
        call(&mut automation, "html_review_export", json!({"path": "native.html", "out": "native.json"}));
        let exported: Value = serde_json::from_slice(&std::fs::read(directory.path().join("native.json")).unwrap()).unwrap();
        assert_eq!(exported["annotations"], annotations);
        assert!(exported["source_review"].is_null());
        assert_eq!(std::fs::read(directory.path().join("native.html")).unwrap(), package);
    }
}
