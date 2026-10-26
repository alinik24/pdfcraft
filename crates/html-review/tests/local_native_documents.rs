//! Opt-in local fidelity check. Private documents are never embedded in source.
#[cfg(feature = "native")]
#[test]
fn local_native_documents_render_and_keep_original_source() {
    let Some(directory) = std::env::var_os("PDFCRAFT_HTML_FIXTURE_DIR") else { return };
    let evidence = std::env::var_os("PDFCRAFT_HTML_NATIVE_EVIDENCE_DIR").map(std::path::PathBuf::from);
    if let Some(path) = &evidence {
        std::fs::create_dir_all(path).unwrap();
    }
    let mut count = 0;
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if !pdfcraft_html_review::native::is_html_name(&path.to_string_lossy()) {
            continue;
        }
        let source = std::fs::read(&path).unwrap();
        let (original, pdf) = pdfcraft_html_review::native::open_native(&source, path.to_str()).unwrap();
        assert_eq!(source, original.as_bytes());
        assert!(pdf.starts_with(b"%PDF-"));
        let saved = pdfcraft_html_review::native::save_native(&original, &pdf, serde_json::json!([])).unwrap();
        let (reopened_source, reopened_pdf) = pdfcraft_html_review::native::open_native(&saved, None).unwrap();
        assert_eq!(original, reopened_source);
        assert_eq!(pdf, reopened_pdf);
        if let Some(directory) = &evidence {
            let stem = path.file_stem().unwrap();
            std::fs::write(directory.join(stem).with_extension("pdf"), &pdf).unwrap();
            std::fs::write(directory.join(stem).with_extension("html"), &saved).unwrap();
        }
        count += 1;
    }
    assert!(count > 0, "fixture directory contained no HTML");
}
