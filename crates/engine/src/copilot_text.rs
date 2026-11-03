//! Bounded native text context shared by GUI and automation copilot turns.
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use pdfcraft_ai::copilot::{NativeTextIndex, NativeTextPage};
use pdfcraft_render::{PageRenderer, RenderConfig, RenderRequest, RequestKind};

use crate::Document;

const MAX_PAGES: usize = 1000;
const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_EXTRACTION_TIME: Duration = Duration::from_secs(3);

/// Movable native context. Deliberately not Debug/Serialize: configuration may contain a password.
pub struct TextSnapshot {
    bytes: Arc<Vec<u8>>,
    config: RenderConfig,
    revision: u64,
    total_pages: usize,
}

impl TextSnapshot {
    pub fn from_document(document: &Document) -> Result<Self, String> {
        document.check_copying().map_err(|e| e.to_string())?;
        Ok(Self {
            bytes: document.bytes.clone(),
            config: document.config.clone(),
            revision: document.edit_generation(),
            total_pages: document.info.pages.len(),
        })
    }

    /// Call on a worker for interactive hosts. The elapsed limit is checked between pages;
    /// one native page extraction is indivisible and may exceed that limit.
    pub fn extract(self) -> NativeTextIndex {
        self.extract_with_limits(MAX_PAGES, MAX_TEXT_BYTES, MAX_EXTRACTION_TIME)
    }

    /// Retain the requested current page even when the all-page index reaches a limit.
    pub fn extract_context(self, page: usize) -> (NativeTextIndex, Option<String>) {
        let bytes = self.bytes.clone();
        let config = self.config.clone();
        let index = self.extract();
        let text = index.pages.iter().find(|p| p.page == page.saturating_add(1) && !p.truncated).map(|p| p.text.clone()).or_else(|| {
            let mut renderer = PageRenderer::new(bytes, config);
            renderer
                .render(RenderRequest { page, kind: RequestKind::Text, tile: None, scale: 1.0, tag: 0, preview: false })
                .text
                .map(|text| text.plain_text())
        });
        let text = text.map(|text| cap_text(text, 100_000).0);
        (index, text)
    }

    fn extract_with_limits(self, max_pages: usize, max_bytes: usize, max_time: Duration) -> NativeTextIndex {
        let started = Instant::now();
        let mut index = NativeTextIndex {
            revision: self.revision,
            total_pages: self.total_pages,
            covered_pages: Vec::new(),
            unreadable_pages: Vec::new(),
            truncated: false,
            pages: Vec::new(),
        };
        let mut renderer = PageRenderer::new(self.bytes, self.config);
        let mut remaining = max_bytes;
        for page in 0..self.total_pages.min(max_pages) {
            if remaining == 0 || started.elapsed() >= max_time {
                break;
            }
            let rendered = renderer.render(RenderRequest { page, kind: RequestKind::Text, tile: None, scale: 1.0, tag: 0, preview: false });
            let number = page.saturating_add(1);
            match rendered.text {
                Some(text) => {
                    let plain = text.plain_text();
                    if plain.trim().is_empty() {
                        index.unreadable_pages.push(number);
                        continue;
                    }
                    let (text, capped) = cap_text(plain, remaining);
                    let truncated = capped || rendered.warnings.contains(&pdfcraft_render::RenderWarning::ContentTruncated);
                    remaining = remaining.saturating_sub(text.len());
                    index.covered_pages.push(number);
                    index.pages.push(NativeTextPage { page: number, text, truncated });
                    index.truncated |= truncated;
                }
                None => index.unreadable_pages.push(number),
            }
        }
        index.truncated |= index.covered_pages.len().saturating_add(index.unreadable_pages.len()) < index.total_pages;
        index
    }
}

fn cap_text(mut text: String, max_bytes: usize) -> (String, bool) {
    let truncated = text.len() > max_bytes;
    text.truncate(text.floor_char_boundary(max_bytes.min(text.len())));
    (text, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Session;

    fn snapshot() -> TextSnapshot {
        let mut session = Session::new();
        let id = session.open("synthetic.pdf", None, Arc::new(crate::tests::fixture(2)), None).unwrap();
        TextSnapshot::from_document(session.get(id).unwrap()).unwrap()
    }

    #[test]
    fn native_index_extracts_both_pages_with_revision_and_explicit_coverage() {
        let (index, focused) = snapshot().extract_context(1);
        assert_eq!(index.revision, 0);
        assert_eq!(index.total_pages, 2);
        assert_eq!(index.covered_pages, [1, 2]);
        assert!(index.unreadable_pages.is_empty());
        assert!(!index.truncated);
        assert!(index.pages[0].text.contains("Page 1"));
        assert!(index.pages[1].text.contains("Page 2"));
        assert!(focused.unwrap().contains("Page 2"));
    }

    #[test]
    fn native_index_reports_page_byte_time_and_unreadable_limits() {
        let index = snapshot().extract_with_limits(1, MAX_TEXT_BYTES, Duration::from_secs(3));
        assert_eq!(index.covered_pages, [1]);
        assert!(index.truncated);
        let index = snapshot().extract_with_limits(2, 2, Duration::from_secs(3));
        assert_eq!(index.covered_pages, [1]);
        assert!(index.pages[0].truncated);
        assert_eq!(index.pages[0].text.len(), 2);
        assert!(index.truncated);
        let index = snapshot().extract_with_limits(2, MAX_TEXT_BYTES, Duration::ZERO);
        assert!(index.covered_pages.is_empty());
        assert!(index.truncated);
        let mut source = snapshot();
        source.bytes = Arc::new(b"not a PDF".to_vec());
        let index = source.extract();
        assert!(index.covered_pages.is_empty());
        assert_eq!(index.unreadable_pages, [1, 2]);
    }

    #[test]
    fn native_index_reports_pages_without_extractable_text() {
        let mut source = snapshot();
        // Keep PDF stream lengths and xref offsets unchanged while blanking page 2.
        let bytes = String::from_utf8(source.bytes.as_ref().clone()).unwrap().replace("(Page 2)", "(      )");
        source.bytes = Arc::new(bytes.into_bytes());
        let index = source.extract();
        assert_eq!(index.covered_pages, [1]);
        assert_eq!(index.unreadable_pages, [2]);
        assert!(!index.truncated);
        assert_eq!(index.pages.len(), 1);
    }

    #[test]
    fn native_text_cap_preserves_unicode_boundaries() {
        assert_eq!(cap_text("é日本語".into(), 4), ("é".into(), true));
        assert_eq!(cap_text("é日本語".into(), 5), ("é日".into(), true));
        assert_eq!(cap_text("é".into(), 2), ("é".into(), false));
    }
}
