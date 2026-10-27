//! Prepare explicit copilot proposals as one normal, undoable engine transaction.
//!
//! Preparation never applies an edit, writes a file, saves a document or starts a process. The
//! host must retain the originating document ID, recheck its live revision when the user presses
//! Apply, and then pass the returned batch to [`crate::Session::apply`]. Normal Save stays separate.

use std::collections::{HashMap, HashSet, hash_map::Entry};
use std::path::{Component, Path};
use std::sync::Arc;

use pdfcraft_ai::review::{Change, MAX_CHANGES, MAX_PAGES_PER_CHANGE, Proposal};
use pdfcraft_render::{PageInfo, PageRenderer, RenderConfig, RenderRequest, RequestKind};

use crate::{DocId, Document, Edit, Markup, NewAnnotation, NoteIcon, Shape, Style};

/// Total replacement PDF bytes retained by one prepared review (shared sources count once).
pub const MAX_REPLACEMENT_BYTES: usize = 128 * 1024 * 1024;

/// An immutable snapshot that can be moved to a worker without borrowing a UI-owned document.
/// Intentionally has no Debug implementation: render configuration can hold an opening password.
#[derive(Clone)]
pub struct ReviewSnapshot {
    document_id: DocId,
    revision: u64,
    bytes: Arc<Vec<u8>>,
    config: RenderConfig,
    pages: Vec<PageInfo>,
    permissions: Option<pdfcraft_cos::Permissions>,
}

impl ReviewSnapshot {
    /// Capture only after the native copying permission and editability checks succeed.
    pub fn from_document(document: &Document) -> Result<Self, String> {
        document.check_copying().map_err(|e| e.to_string())?;
        if !document.editable() {
            return Err(document.read_only_reason.clone().unwrap_or_else(|| "the document cannot be edited".into()));
        }
        Ok(Self {
            document_id: document.id,
            revision: document.edit_generation(),
            bytes: document.bytes.clone(),
            config: document.config.clone(),
            pages: document.info.pages.clone(),
            permissions: document.permissions(),
        })
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn document_id(&self) -> DocId {
        self.document_id
    }
}

/// Resolve a previously parsed proposal against its snapshot. `read_output` must enforce the
/// document workspace root, reject links/escapes, and honour the requested byte limit before
/// allocation. This adapter never reads the host filesystem itself.
///
/// Exact markup quotes refer to the snapshot's content, so replacing and marking the same page
/// in one proposal is refused. A second reviewed proposal can mark the replacement's new text.
pub fn prepare_review(
    snapshot: &ReviewSnapshot,
    proposal: &Proposal,
    mut read_output: impl FnMut(&Path, usize) -> Result<Vec<u8>, String>,
) -> Result<Edit, String> {
    crate::guard(|| prepare(snapshot, proposal, &mut read_output))
        .map_err(|_| "the document review could not be prepared safely; the document was not changed".to_string())?
}

fn prepare(
    snapshot: &ReviewSnapshot,
    proposal: &Proposal,
    read_output: &mut impl FnMut(&Path, usize) -> Result<Vec<u8>, String>,
) -> Result<Edit, String> {
    if proposal.revision != snapshot.revision {
        return Err("the document changed after this proposal was prepared; request a new review".into());
    }
    if proposal.changes.is_empty() || proposal.changes.len() > MAX_CHANGES {
        return Err("a review must contain between 1 and 64 changes".into());
    }
    // Validate every destination before invoking a reader or preparing any native operation.
    let mut marked = HashSet::new();
    let mut replaced = HashSet::new();
    for change in &proposal.changes {
        match change {
            Change::Comment { page, text } => {
                page_index(snapshot, *page)?;
                bounded_text(text, 32_000, false)?;
            }
            Change::Highlight { page, quote, comment } | Change::Underline { page, quote, comment } | Change::Strikeout { page, quote, comment } => {
                page_index(snapshot, *page)?;
                bounded_text(quote, 4000, false)?;
                if let Some(comment) = comment {
                    bounded_text(comment, 32_000, true)?;
                }
                marked.insert(*page);
            }
            Change::Rotate { pages, degrees } => {
                destination_indices(snapshot, pages)?;
                if !matches!(degrees, -270 | -180 | -90 | 90 | 180 | 270) {
                    return Err("review rotation must be 90, 180 or 270 degrees in either direction".into());
                }
            }
            Change::Metadata { value, .. } => bounded_text(value, 4000, true)?,
            Change::ReplacePages { pages, source_pages, source } => {
                destination_indices(snapshot, pages)?;
                if pages.len() != source_pages.len() || source_pages.contains(&0) {
                    return Err("replacement and source page lists must be equal-length, positive page numbers".into());
                }
                checked_source_path(source)?;
                replaced.extend(pages.iter().copied());
            }
        }
    }
    if marked.iter().any(|page| replaced.contains(page)) {
        return Err("replace a page before requesting text markup on its new content".into());
    }

    let mut renderer = PageRenderer::new(snapshot.bytes.clone(), snapshot.config.clone());
    let mut text_cache = HashMap::new();
    let mut source_cache: HashMap<String, (Arc<Vec<u8>>, usize)> = HashMap::new();
    let mut remaining_bytes = MAX_REPLACEMENT_BYTES;
    let mut edits = Vec::with_capacity(proposal.changes.len());
    for change in &proposal.changes {
        let edit = match change {
            Change::Comment { page, text } => {
                let page = page_index(snapshot, *page)?;
                let info = snapshot.pages.get(page).ok_or("the comment page no longer exists")?;
                valid_geometry(info)?;
                let at = info.view_to_user((info.width - 44.0).max(0.0), 24.0_f32.min(info.height));
                annotation(page, Shape::Note { at: at.map(f64::from), icon: NoteIcon::Comment }, text.clone())
            }
            Change::Highlight { page, quote, comment } | Change::Underline { page, quote, comment } | Change::Strikeout { page, quote, comment } => {
                let page = page_index(snapshot, *page)?;
                let info = snapshot.pages.get(page).ok_or("the markup page no longer exists")?;
                valid_geometry(info)?;
                if let Entry::Vacant(entry) = text_cache.entry(page) {
                    let rendered = renderer.render(RenderRequest { page, kind: RequestKind::Text, scale: 1.0, ..Default::default() });
                    if rendered.error.is_some() || rendered.warnings.contains(&pdfcraft_render::RenderWarning::ContentTruncated) {
                        return Err(format!("page {} could not be read completely for the quoted markup", page.saturating_add(1)));
                    }
                    let text = rendered.text.ok_or_else(|| format!("page {} has no readable text for markup", page.saturating_add(1)))?;
                    entry.insert(text);
                }
                let text = text_cache.get(&page).ok_or("the quoted page text is unavailable")?;
                // Native Find normally ignores case and normalises whitespace. Review proposals
                // require the exact quoted text: a case-sensitive first hit is checked verbatim.
                let hit = text
                    .find_opts_limited(quote, true, false, 1)
                    .into_iter()
                    .next()
                    .ok_or_else(|| format!("the exact quoted text was not found on page {}", page.saturating_add(1)))?;
                if text.text_of(hit.clone()) != *quote {
                    return Err(format!("the quoted text does not match page {} exactly", page.saturating_add(1)));
                }
                let quads: Vec<[f64; 8]> = text.line_rects(hit).into_iter().map(|rect| info.view_rect_to_quad(rect)).collect();
                if quads.is_empty() || quads.len() > 4000 || quads.iter().flatten().any(|value| !value.is_finite()) {
                    return Err("the quoted text has no usable native markup geometry".into());
                }
                let kind = match change {
                    Change::Highlight { .. } => Markup::Highlight,
                    Change::Underline { .. } => Markup::Underline,
                    _ => Markup::StrikeOut,
                };
                annotation(page, Shape::TextMarkup { kind, quads }, comment.clone().unwrap_or_default())
            }
            Change::Rotate { pages, degrees } => Edit::RotatePages { pages: destination_indices(snapshot, pages)?, degrees: i64::from(*degrees) },
            Change::Metadata { key, value } => Edit::SetInfo { key: key.as_str().into(), value: value.clone() },
            Change::ReplacePages { pages, source_pages, source } => {
                if let Entry::Vacant(entry) = source_cache.entry(source.clone()) {
                    let bytes = read_output(Path::new(source), remaining_bytes)?;
                    if bytes.is_empty() || bytes.len() > remaining_bytes {
                        return Err("replacement PDFs exceed the review's 128 MiB total byte limit or are empty".into());
                    }
                    remaining_bytes = remaining_bytes.checked_sub(bytes.len()).ok_or("replacement PDF byte budget overflow")?;
                    let bytes = Arc::new(bytes);
                    let source_document = crate::open_source(source, &bytes).map_err(|e| e.to_string())?;
                    let count = pdfcraft_organize::page_count(&source_document).map_err(|e| e.to_string())?;
                    entry.insert((bytes, count));
                }
                let (bytes, count) = source_cache.get(source).ok_or("the replacement PDF is unavailable")?;
                let src_pages = source_pages
                    .iter()
                    .map(|page| page.checked_sub(1).filter(|page| page < count).ok_or_else(|| "a replacement source page does not exist".to_string()))
                    .collect::<Result<Vec<_>, _>>()?;
                Edit::ReplacePages { pages: destination_indices(snapshot, pages)?, name: source.clone(), bytes: bytes.clone(), src_pages }
            }
        };
        edits.push(edit);
    }
    let edit = Edit::Batch { label: "Apply copilot review".into(), edits };
    if let Some(permissions) = &snapshot.permissions {
        crate::check_permission(&edit, permissions).map_err(|e| e.to_string())?;
    }
    Ok(edit)
}

fn annotation(page: usize, shape: Shape, contents: String) -> Edit {
    Edit::AddAnnotation(NewAnnotation { page, style: Style::default_for(&shape), shape, contents, author: "PdfCraft".into() })
}

fn page_index(snapshot: &ReviewSnapshot, one_based: usize) -> Result<usize, String> {
    one_based.checked_sub(1).filter(|index| snapshot.pages.get(*index).is_some()).ok_or_else(|| "a proposed document page does not exist".into())
}

fn destination_indices(snapshot: &ReviewSnapshot, pages: &[usize]) -> Result<Vec<usize>, String> {
    if pages.is_empty() || pages.len() > MAX_PAGES_PER_CHANGE {
        return Err("a review page list needs between 1 and 1000 pages".into());
    }
    let mut seen = HashSet::new();
    pages
        .iter()
        .map(|page| {
            if !seen.insert(*page) {
                return Err("a review destination page must not repeat".into());
            }
            page_index(snapshot, *page)
        })
        .collect()
}

fn bounded_text(text: &str, limit: usize, allow_empty: bool) -> Result<(), String> {
    if (!allow_empty && text.trim().is_empty())
        || text.chars().count() > limit
        || text.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err("a proposed text field is empty, too long, or contains unsupported controls".into());
    }
    Ok(())
}

fn valid_geometry(info: &PageInfo) -> Result<(), String> {
    if !info.width.is_finite() || !info.height.is_finite() || info.width <= 0.0 || info.height <= 0.0 || info.crop.iter().any(|n| !n.is_finite()) {
        return Err("the proposed annotation page has unusable geometry".into());
    }
    Ok(())
}

fn checked_source_path(source: &str) -> Result<(), String> {
    // The parser owns the complete protocol validation. Repeat lexical confinement here even
    // for a directly constructed Rust proposal, before invoking a host-provided file reader.
    let path = Path::new(source);
    if source.len() > 4096
        || !source.starts_with("outputs/")
        || source.contains(['\\', ':', '~'])
        || !source.to_ascii_lowercase().ends_with(".pdf")
        || source
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == ".." || part.ends_with(['.', ' ']) || part.chars().any(char::is_control))
        || path.components().any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("replacement PDFs must stay under the document workspace outputs folder".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Session;

    fn document() -> (Session, DocId, ReviewSnapshot) {
        let mut session = Session::new().with_clock(|| 1_700_000_000);
        let id = session.open("review.pdf", None, Arc::new(crate::tests::fixture(2)), None).unwrap();
        let snapshot = ReviewSnapshot::from_document(session.get(id).unwrap()).unwrap();
        (session, id, snapshot)
    }

    fn proposal(snapshot: &ReviewSnapshot, changes: Vec<Change>) -> Proposal {
        Proposal { revision: snapshot.revision(), summary: "Review the document.".into(), changes }
    }

    fn no_files(_: &Path, _: usize) -> Result<Vec<u8>, String> {
        Err("unexpected file read".into())
    }

    #[test]
    fn comment_and_exact_highlight_are_one_undoable_saved_transaction() {
        fn send_and_clone<T: Send + Clone>() {}
        send_and_clone::<ReviewSnapshot>();
        let (mut session, id, snapshot) = document();
        assert_eq!(snapshot.document_id(), id);
        let packet = proposal(
            &snapshot,
            vec![
                Change::Comment { page: 1, text: "Check the first page.".into() },
                Change::Highlight { page: 1, quote: "Page 1".into(), comment: Some("Relevant passage".into()) },
            ],
        );
        let edit = prepare_review(&snapshot, &packet, no_files).unwrap();
        assert!(matches!(&edit, Edit::Batch { label, edits } if label == "Apply copilot review" && edits.len() == 2));
        assert!(session.get(id).unwrap().info.annotations.is_empty());
        session.apply(id, edit).unwrap();
        let annotations = &session.get(id).unwrap().info.annotations;
        assert_eq!(annotations.len(), 2);
        assert!(annotations.iter().any(|a| a.subtype == "Highlight" && !a.quads.is_empty()));
        let saved = session.save_bytes(id).unwrap();
        let reopened = session.open("saved.pdf", None, saved, None).unwrap();
        assert_eq!(session.get(reopened).unwrap().info.annotations.len(), 2);
        session.undo(id).unwrap();
        assert!(session.get(id).unwrap().info.annotations.is_empty());
        assert!(session.get(id).unwrap().can_undo().is_none());
        session.redo(id).unwrap();
        assert_eq!(session.get(id).unwrap().info.annotations.len(), 2);
    }

    #[cfg(feature = "html-review")]
    #[test]
    fn reviewed_annotations_survive_inside_html_source_on_save_and_reopen() {
        let source = "<!doctype html><p>Retained original source.</p>";
        let packaged = pdfcraft_html_review::native::save_native(source, &crate::tests::fixture(1), serde_json::json!([])).unwrap();
        let mut session = Session::new().with_clock(|| 1_700_000_000);
        let id = session.open("review.html", None, Arc::new(packaged), None).unwrap();
        let snapshot = ReviewSnapshot::from_document(session.get(id).unwrap()).unwrap();
        let packet = proposal(
            &snapshot,
            vec![
                Change::Comment { page: 1, text: "Embedded review comment".into() },
                Change::Highlight { page: 1, quote: "Page 1".into(), comment: None },
            ],
        );
        session.apply(id, prepare_review(&snapshot, &packet, no_files).unwrap()).unwrap();
        let saved = session.save_bytes(id).unwrap();
        let (kept_source, _) = pdfcraft_html_review::native::unpack_native(&saved).unwrap();
        assert_eq!(kept_source, source);
        let reopened = session.open("saved.html", None, saved, None).unwrap();
        let document = session.get(reopened).unwrap();
        assert!(document.is_html());
        assert_eq!(document.info.annotations.len(), 2);
        assert!(document.info.annotations.iter().any(|annotation| annotation.contents.as_deref() == Some("Embedded review comment")));
    }

    #[test]
    fn missing_or_case_changed_quote_refuses_entire_prepared_batch() {
        let (session, id, snapshot) = document();
        for quote in ["missing passage", "page 1", " Page 1 "] {
            let packet = proposal(
                &snapshot,
                vec![
                    Change::Comment { page: 1, text: "Must not apply alone".into() },
                    Change::Highlight { page: 1, quote: quote.into(), comment: None },
                ],
            );
            assert!(prepare_review(&snapshot, &packet, no_files).is_err());
            assert!(session.get(id).unwrap().info.annotations.is_empty());
            assert!(session.get(id).unwrap().can_undo().is_none());
        }
    }

    #[test]
    fn underline_strikeout_rotation_and_metadata_use_native_page_indices() {
        let (mut session, id, snapshot) = document();
        let packet = proposal(
            &snapshot,
            vec![
                Change::Underline { page: 1, quote: "Page 1".into(), comment: None },
                Change::Strikeout { page: 2, quote: "Page 2".into(), comment: Some("Review this".into()) },
                Change::Rotate { pages: vec![2], degrees: 90 },
                Change::Metadata { key: pdfcraft_ai::review::MetadataKey::Subject, value: "Reviewed document".into() },
            ],
        );
        session.apply(id, prepare_review(&snapshot, &packet, no_files).unwrap()).unwrap();
        let document = session.get(id).unwrap();
        assert!(document.info.annotations.iter().any(|annotation| annotation.page == 0 && annotation.subtype == "Underline"));
        assert!(document.info.annotations.iter().any(|annotation| annotation.page == 1 && annotation.subtype == "StrikeOut"));
        assert_eq!(document.info.pages.get(1).unwrap().rotation, 90);
        assert_eq!(document.info.subject.as_deref(), Some("Reviewed document"));
        session.undo(id).unwrap();
        let document = session.get(id).unwrap();
        assert!(document.info.annotations.is_empty());
        assert_eq!(document.info.pages.get(1).unwrap().rotation, 0);
        assert_eq!(document.info.subject, None);
    }

    #[test]
    fn stale_revision_bad_pages_and_unreadable_sources_are_refused() {
        let (session, id, snapshot) = document();
        let mut packet = proposal(&snapshot, vec![Change::Comment { page: 1, text: "Comment".into() }]);
        packet.revision = packet.revision.saturating_add(1);
        assert!(prepare_review(&snapshot, &packet, no_files).unwrap_err().contains("document changed"));
        for page in [0, 3, usize::MAX] {
            assert!(prepare_review(&snapshot, &proposal(&snapshot, vec![Change::Comment { page, text: "Comment".into() }]), no_files).is_err());
        }
        let packet = proposal(&snapshot, vec![Change::ReplacePages { pages: vec![1], source_pages: vec![1], source: "outputs/revised.pdf".into() }]);
        assert!(prepare_review(&snapshot, &packet, |_, _| Err("source unavailable".into())).unwrap_err().contains("source unavailable"));
        assert!(prepare_review(&snapshot, &packet, |_, _| Ok(b"not a PDF".to_vec())).is_err());
        assert!(
            prepare_review(&snapshot, &packet, |_, limit| {
                assert_eq!(limit, MAX_REPLACEMENT_BYTES);
                Ok(crate::tests::fixture(1))
            })
            .is_ok()
        );
        assert!(session.get(id).unwrap().info.annotations.is_empty());
        assert!(!session.get(id).unwrap().dirty);
    }

    #[test]
    fn replacement_validates_indices_caches_sources_and_preserves_existing_comments() {
        let (mut session, id, snapshot) = document();
        let note = proposal(&snapshot, vec![Change::Comment { page: 1, text: "Keep this comment".into() }]);
        session.apply(id, prepare_review(&snapshot, &note, no_files).unwrap()).unwrap();
        let snapshot = ReviewSnapshot::from_document(session.get(id).unwrap()).unwrap();
        let packet = proposal(
            &snapshot,
            vec![
                Change::ReplacePages { pages: vec![1], source_pages: vec![1], source: "outputs/revised.pdf".into() },
                Change::ReplacePages { pages: vec![2], source_pages: vec![1], source: "outputs/revised.pdf".into() },
            ],
        );
        let mut reads = 0;
        let edit = prepare_review(&snapshot, &packet, |path, _| {
            assert_eq!(path, Path::new("outputs/revised.pdf"));
            reads += 1;
            Ok(crate::tests::fixture(1))
        })
        .unwrap();
        assert_eq!(reads, 1);
        session.apply(id, edit).unwrap();
        assert!(session.get(id).unwrap().info.annotations.iter().any(|a| a.contents.as_deref() == Some("Keep this comment")));
        let bad = proposal(&snapshot, vec![Change::ReplacePages { pages: vec![1], source_pages: vec![2], source: "outputs/revised.pdf".into() }]);
        assert!(prepare_review(&snapshot, &bad, |_, _| Ok(crate::tests::fixture(1))).unwrap_err().contains("source page"));
    }

    #[test]
    fn replacement_markup_conflict_and_unsafe_source_refuse_before_reading() {
        let (_, _, snapshot) = document();
        let conflict = proposal(
            &snapshot,
            vec![
                Change::Highlight { page: 1, quote: "Page 1".into(), comment: None },
                Change::ReplacePages { pages: vec![1], source_pages: vec![1], source: "outputs/revised.pdf".into() },
            ],
        );
        let mut reads = 0;
        assert!(
            prepare_review(&snapshot, &conflict, |_, _| {
                reads += 1;
                Ok(crate::tests::fixture(1))
            })
            .is_err()
        );
        assert_eq!(reads, 0);
        let escape =
            proposal(&snapshot, vec![Change::ReplacePages { pages: vec![1], source_pages: vec![1], source: "outputs/../outside.pdf".into() }]);
        assert!(
            prepare_review(&snapshot, &escape, |_, _| {
                reads += 1;
                Ok(crate::tests::fixture(1))
            })
            .is_err()
        );
        assert_eq!(reads, 0);
    }

    #[test]
    fn snapshots_honour_copying_permission() {
        let mut cos = pdfcraft_cos::Document::open(Arc::new(crate::tests::fixture(1))).unwrap();
        cos.set_encryption(&pdfcraft_cos::NewEncryption {
            algorithm: pdfcraft_cos::Algorithm::Aes256,
            user_password: "",
            owner_password: "owner",
            permissions: -1 ^ 0b1_0000,
            encrypt_metadata: true,
            seed: [7; 32],
        })
        .unwrap();
        let bytes = Arc::new(pdfcraft_cos::write_full(&cos, &Default::default()).unwrap());
        let mut session = Session::new();
        let id = session.open("protected.pdf", None, bytes, None).unwrap();
        assert!(!session.get(id).unwrap().allows_copying());
        assert!(ReviewSnapshot::from_document(session.get(id).unwrap()).is_err());
    }

    #[test]
    fn preparation_honours_native_annotation_permission() {
        let mut cos = pdfcraft_cos::Document::open(Arc::new(crate::tests::fixture(1))).unwrap();
        cos.set_encryption(&pdfcraft_cos::NewEncryption {
            algorithm: pdfcraft_cos::Algorithm::Aes256,
            user_password: "",
            owner_password: "owner",
            permissions: -1 ^ 0b10_0000,
            encrypt_metadata: true,
            seed: [8; 32],
        })
        .unwrap();
        let bytes = Arc::new(pdfcraft_cos::write_full(&cos, &Default::default()).unwrap());
        let mut session = Session::new();
        let id = session.open("protected.pdf", None, bytes, None).unwrap();
        let document = session.get(id).unwrap();
        assert!(document.allows_copying());
        assert!(!document.allows_annotation());
        let snapshot = ReviewSnapshot::from_document(document).unwrap();
        let packet = proposal(&snapshot, vec![Change::Comment { page: 1, text: "Not allowed".into() }]);
        assert!(prepare_review(&snapshot, &packet, no_files).is_err());
        assert!(session.get(id).unwrap().info.annotations.is_empty());
    }
}
