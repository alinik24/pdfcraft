//! Typed, portable proposals for changes to an open document.
//!
//! Parsing never executes or applies a change. The host must show the proposal, resolve exact
//! quoted text against the current document, check its permissions and revision again, and apply
//! an accepted proposal through its normal undoable edit pipeline. Saving remains a separate step.

use std::collections::HashSet;
use std::fmt;

use serde_json::{Map, Value};

pub const FORMAT: &str = "pdfcraft-review-proposal/1";
pub const MAX_PROPOSAL_BYTES: usize = 1024 * 1024;
pub const MAX_CHANGES: usize = 64;
pub const MAX_PAGES_PER_CHANGE: usize = 1000;
const MAX_PAGE_NUMBER: u64 = u32::MAX as u64;

/// One proposed transaction, bound to the host's monotonic document edit generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    pub revision: u64,
    pub summary: String,
    pub changes: Vec<Change>,
}

/// Supported operations have typed fields; unknown operations are never silently skipped.
/// All page numbers are one-based. A quote is retained exactly as supplied, without trimming.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Comment { page: usize, text: String },
    Highlight { page: usize, quote: String, comment: Option<String> },
    Underline { page: usize, quote: String, comment: Option<String> },
    Strikeout { page: usize, quote: String, comment: Option<String> },
    Rotate { pages: Vec<usize>, degrees: i16 },
    Metadata { key: MetadataKey, value: String },
    ReplacePages { pages: Vec<usize>, source_pages: Vec<usize>, source: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetadataKey {
    Title,
    Author,
    Subject,
    Keywords,
}

impl MetadataKey {
    /// The standard PDF information dictionary key.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Title => "Title",
            Self::Author => "Author",
            Self::Subject => "Subject",
            Self::Keywords => "Keywords",
        }
    }
}

/// Diagnostics describe the protocol problem without echoing private document text or paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewError {
    TooLarge,
    Invalid(String),
    StaleRevision { expected: u64, received: u64 },
}

impl fmt::Display for ReviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => f.write_str("the document change proposal exceeds 1 MiB"),
            Self::Invalid(message) => write!(f, "the document change proposal is invalid: {message}"),
            Self::StaleRevision { expected, received } => {
                write!(f, "the document has changed since this proposal was prepared (current revision {expected}, proposal revision {received})")
            }
        }
    }
}

impl std::error::Error for ReviewError {}

type Result<T> = std::result::Result<T, ReviewError>;

/// Parse an untrusted, bounded JSON packet and reject a proposal for any other document revision.
/// The default JSON recursion limit is retained. Floats, negative numbers and overflowing page
/// numbers/revisions are rejected instead of being rounded or cast.
pub fn parse(bytes: &[u8], expected_revision: u64) -> Result<Proposal> {
    if bytes.len() > MAX_PROPOSAL_BYTES {
        return Err(ReviewError::TooLarge);
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| invalid("expected a JSON object"))?;
    let packet = object(&value)?;
    known_fields(packet, &["format", "revision", "summary", "changes"])?;
    if packet.get("format").and_then(Value::as_str) != Some(FORMAT) {
        return Err(invalid("unsupported proposal format"));
    }
    let revision = packet.get("revision").and_then(Value::as_u64).ok_or_else(|| invalid("revision must be an unsigned integer"))?;
    if revision != expected_revision {
        return Err(ReviewError::StaleRevision { expected: expected_revision, received: revision });
    }
    let summary = text(packet, "summary", 2000, false)?;
    let changes = packet.get("changes").and_then(Value::as_array).ok_or_else(|| invalid("changes must be a list"))?;
    if changes.is_empty() || changes.len() > MAX_CHANGES {
        return Err(invalid("a proposal needs between 1 and 64 changes"));
    }
    let changes = changes.iter().map(parse_change).collect::<Result<Vec<_>>>()?;
    Ok(Proposal { revision, summary, changes })
}

fn parse_change(value: &Value) -> Result<Change> {
    let change = object(value)?;
    let kind = change.get("kind").and_then(Value::as_str).ok_or_else(|| invalid("each change needs a kind"))?;
    match kind {
        "comment" => {
            known_fields(change, &["kind", "page", "text"])?;
            Ok(Change::Comment { page: page(change.get("page"))?, text: text(change, "text", 32_000, false)? })
        }
        "highlight" | "underline" | "strikeout" => {
            known_fields(change, &["kind", "page", "quote", "comment"])?;
            let page = page(change.get("page"))?;
            let quote = text(change, "quote", 4000, false)?;
            let comment = change.get("comment").map(|_| text(change, "comment", 32_000, true)).transpose()?;
            Ok(match kind {
                "highlight" => Change::Highlight { page, quote, comment },
                "underline" => Change::Underline { page, quote, comment },
                _ => Change::Strikeout { page, quote, comment },
            })
        }
        "rotate" => {
            known_fields(change, &["kind", "pages", "degrees"])?;
            let pages = page_list(change.get("pages"), true)?;
            let degrees = change.get("degrees").and_then(Value::as_i64).ok_or_else(|| invalid("rotation degrees must be an integer"))?;
            if !matches!(degrees, -270 | -180 | -90 | 90 | 180 | 270) {
                return Err(invalid("rotation must be 90, 180 or 270 degrees in either direction"));
            }
            let degrees = i16::try_from(degrees).map_err(|_| invalid("rotation degrees overflow"))?;
            Ok(Change::Rotate { pages, degrees })
        }
        "metadata" => {
            known_fields(change, &["kind", "key", "value"])?;
            let key = match change.get("key").and_then(Value::as_str) {
                Some("Title") => MetadataKey::Title,
                Some("Author") => MetadataKey::Author,
                Some("Subject") => MetadataKey::Subject,
                Some("Keywords") => MetadataKey::Keywords,
                _ => return Err(invalid("metadata key must be Title, Author, Subject or Keywords")),
            };
            Ok(Change::Metadata { key, value: text(change, "value", 4000, true)? })
        }
        "replace_pages" => {
            known_fields(change, &["kind", "pages", "source_pages", "source"])?;
            let pages = page_list(change.get("pages"), true)?;
            // Reusing one source page for several distinct destination pages is intentional.
            let source_pages = page_list(change.get("source_pages"), false)?;
            if pages.len() != source_pages.len() {
                return Err(invalid("replacement and source page lists must have equal lengths"));
            }
            let source = text(change, "source", 4096, false)?;
            validate_source(&source)?;
            Ok(Change::ReplacePages { pages, source_pages, source })
        }
        _ => Err(invalid("unsupported change kind")),
    }
}

fn invalid(message: &str) -> ReviewError {
    ReviewError::Invalid(message.into())
}

fn object(value: &Value) -> Result<&Map<String, Value>> {
    value.as_object().ok_or_else(|| invalid("expected a JSON object"))
}

fn known_fields(object: &Map<String, Value>, allowed: &[&str]) -> Result<()> {
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(invalid("an unsupported field was supplied"));
    }
    Ok(())
}

fn text(object: &Map<String, Value>, key: &str, limit: usize, allow_empty: bool) -> Result<String> {
    let value = object.get(key).and_then(Value::as_str).ok_or_else(|| invalid("a text field is missing or has the wrong type"))?;
    if (!allow_empty && value.trim().is_empty())
        || value.chars().count() > limit
        || value.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(invalid("a text field is empty, too long, or contains unsupported control characters"));
    }
    Ok(value.to_string())
}

fn page(value: Option<&Value>) -> Result<usize> {
    let value = value.and_then(Value::as_u64).ok_or_else(|| invalid("page numbers must be positive integers"))?;
    if value == 0 || value > MAX_PAGE_NUMBER {
        return Err(invalid("page numbers must be between 1 and 4294967295"));
    }
    usize::try_from(value).map_err(|_| invalid("the page number exceeds this platform's limit"))
}

fn page_list(value: Option<&Value>, unique: bool) -> Result<Vec<usize>> {
    let values = value.and_then(Value::as_array).ok_or_else(|| invalid("page numbers must be a list"))?;
    if values.is_empty() || values.len() > MAX_PAGES_PER_CHANGE {
        return Err(invalid("each page list needs between 1 and 1000 pages"));
    }
    let mut result = Vec::with_capacity(values.len());
    let mut seen = HashSet::new();
    for value in values {
        let page = page(Some(value))?;
        if unique && !seen.insert(page) {
            return Err(invalid("destination page numbers must not repeat"));
        }
        result.push(page);
    }
    Ok(result)
}

fn validate_source(source: &str) -> Result<()> {
    if source.len() > 4096 || !source.starts_with("outputs/") || source.contains('\\') || source.split('/').count() > 64 {
        return Err(invalid("replacement PDFs must use a relative POSIX path under outputs"));
    }
    for component in source.split('/') {
        if component.is_empty()
            || component.len() > 240
            || component == "."
            || component == ".."
            || component.ends_with(['.', ' '])
            || component.chars().any(|c| c.is_control() || matches!(c, ':' | '<' | '>' | '"' | '|' | '?' | '*' | '~'))
        {
            return Err(invalid("the replacement path contains traversal, an alias, or an unusable name"));
        }
        let device = component.split('.').next().unwrap_or_default().trim_end_matches(' ').to_ascii_uppercase();
        let port = device.strip_prefix("COM").or_else(|| device.strip_prefix("LPT"));
        if matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$")
            || port.is_some_and(|s| matches!(s, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"))
        {
            return Err(invalid("Windows device names are not replacement PDF files"));
        }
    }
    if !source
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .is_some_and(|(stem, extension)| !stem.is_empty() && extension.eq_ignore_ascii_case("pdf"))
    {
        return Err(invalid("the replacement source must have a PDF extension"));
    }
    Ok(())
}

impl Proposal {
    /// Stable plain-language overview for a review panel, distinct from the portable JSON packet.
    /// Text previews are bounded; hosts should expose the typed change's complete text on demand.
    pub fn review_summary(&self) -> String {
        let mut output = self.summary.clone();
        for (index, change) in self.changes.iter().enumerate() {
            output.push_str(&format!("\n\n{}. {}", index + 1, change.review_summary()));
        }
        output
    }
}

impl Change {
    /// A bounded readable description. Exact text is retained separately in this change's fields.
    pub fn review_summary(&self) -> String {
        match self {
            Self::Comment { page, text } => format!("Add a comment on page {page}: {}", preview(text)),
            Self::Highlight { page, quote, comment } => markup_summary("Highlight", *page, quote, comment.as_deref()),
            Self::Underline { page, quote, comment } => markup_summary("Underline", *page, quote, comment.as_deref()),
            Self::Strikeout { page, quote, comment } => markup_summary("Strike out", *page, quote, comment.as_deref()),
            Self::Rotate { pages, degrees } => format!("Rotate pages {} by {degrees} degrees.", pages_summary(pages)),
            Self::Metadata { key, value } => format!("Set {}: {}", key.as_str(), if value.is_empty() { "(empty)".into() } else { preview(value) }),
            Self::ReplacePages { pages, source_pages, source } => {
                format!("Replace pages {} using pages {} from {}.", pages_summary(pages), pages_summary(source_pages), preview(source))
            }
        }
    }
}

fn markup_summary(action: &str, page: usize, quote: &str, comment: Option<&str>) -> String {
    let mut output = format!("{action} on page {page}: {}", preview(quote));
    if let Some(comment) = comment.filter(|s| !s.is_empty()) {
        output.push_str(&format!("\nComment: {}", preview(comment)));
    }
    output
}

fn preview(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut output: String = flat.chars().take(160).collect();
    if flat.chars().count() > 160 {
        output.push('…');
    }
    output
}

fn pages_summary(pages: &[usize]) -> String {
    let mut output = pages.iter().take(8).map(ToString::to_string).collect::<Vec<_>>().join(", ");
    if pages.len() > 8 {
        output.push_str(&format!(" and {} more", pages.len() - 8));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn packet(changes: Value) -> Value {
        json!({ "format": FORMAT, "revision": 7, "summary": "Review the selected passage.", "changes": changes })
    }

    fn read(value: Value) -> Result<Proposal> {
        parse(&serde_json::to_vec(&value).unwrap(), 7)
    }

    #[test]
    fn parses_every_operation_and_preserves_exact_quotes() {
        let quote = "  Exact\nquoted text with ä and <document>  ";
        let proposal = read(packet(json!([
            {"kind":"comment","page":1,"text":"Review comment"},
            {"kind":"highlight","page":2,"quote":quote,"comment":"A note"},
            {"kind":"underline","page":3,"quote":"underline"},
            {"kind":"strikeout","page":4,"quote":"strikeout"},
            {"kind":"rotate","pages":[2,1],"degrees":-90},
            {"kind":"metadata","key":"Title","value":"Revised title"},
            {"kind":"replace_pages","pages":[1,3],"source_pages":[2,2],"source":"outputs/revision.PDF"}
        ])))
        .unwrap();
        assert_eq!(proposal.changes.len(), 7);
        assert!(
            matches!(proposal.changes.get(1), Some(Change::Highlight { page: 2, quote: actual, comment: Some(comment) }) if actual == quote && comment == "A note")
        );
        assert!(matches!(proposal.changes.get(4), Some(Change::Rotate { pages, degrees: -90 }) if pages == &[2, 1]));
        let overview = proposal.review_summary();
        assert!(overview.starts_with("Review the selected passage."));
        assert!(overview.contains("2. Highlight on page 2:"));
        assert!(!overview.contains(FORMAT));
        assert!(!overview.contains("\"kind\""));
    }

    #[test]
    fn stale_revision_is_rejected_even_for_otherwise_valid_packet() {
        let bytes = serde_json::to_vec(&packet(json!([{"kind":"comment","page":1,"text":"Review"}]))).unwrap();
        assert_eq!(parse(&bytes, 8), Err(ReviewError::StaleRevision { expected: 8, received: 7 }));
        assert!(parse(&bytes, 8).unwrap_err().to_string().contains("document has changed"));
    }

    #[test]
    fn unknown_operations_and_fields_are_not_silently_ignored() {
        assert!(read(packet(json!([{"kind":"comment","page":1,"text":"Keep"},{"kind":"execute","command":"anything"}]))).is_err());
        assert!(read(packet(json!([{"kind":"comment","page":1,"text":"Keep","shell":"anything"}]))).is_err());
        let mut value = packet(json!([{"kind":"comment","page":1,"text":"Keep"}]));
        value["format"] = json!("future-format");
        assert!(read(value).is_err());
        assert!(read(packet(json!([]))).is_err());
        assert!(read(packet(json!([null]))).is_err());
    }

    #[test]
    fn rejects_wrong_types_nan_overflow_and_invalid_page_lists() {
        for page in [json!(0), json!(-1), json!(1.5), json!("1"), json!(null), json!(u64::MAX), json!(4_294_967_296_u64)] {
            assert!(read(packet(json!([{"kind":"comment","page":page,"text":"Keep"}]))).is_err());
        }
        for pages in [json!([]), json!([1, 1]), json!([0]), json!([1.0]), json!("1")] {
            assert!(read(packet(json!([{"kind":"rotate","pages":pages,"degrees":90}]))).is_err());
        }
        for degrees in [json!(0), json!(360), json!(-360), json!(45), json!(90.0), json!("90"), json!(i64::MAX)] {
            assert!(read(packet(json!([{"kind":"rotate","pages":[1],"degrees":degrees}]))).is_err());
        }
        assert!(
            parse(
                br#"{"format":"pdfcraft-review-proposal/1","revision":7,"summary":"x","changes":[{"kind":"rotate","pages":[1],"degrees":NaN}]}"#,
                7
            )
            .is_err()
        );
        for revision in [json!(-1), json!(7.0), json!("7"), json!(null)] {
            let mut value = packet(json!([{"kind":"comment","page":1,"text":"x"}]));
            value["revision"] = revision;
            assert!(read(value).is_err());
        }
        assert!(read(packet(json!([{"kind":"replace_pages","pages":[1,2],"source_pages":[1],"source":"outputs/review.pdf"}]))).is_err());
    }

    #[test]
    fn rejects_replacement_path_traversal_windows_aliases_and_non_pdfs() {
        for source in [
            "../outside.pdf",
            "/outputs/out.pdf",
            "outputs/../outside.pdf",
            "outputs/./review.pdf",
            "outputs//review.pdf",
            "outputs\\review.pdf",
            "outputs/CON.pdf",
            "outputs/CON .pdf",
            "outputs/LPT1.pdf",
            "outputs/COM¹.pdf",
            "outputs/review.pdf.",
            "outputs/review.pdf ",
            "outputs/review.pdf:stream",
            "outputs/DOCUME~1/review.pdf",
            "outputs/review.html",
            "outputs/.pdf",
            "C:/outputs/review.pdf",
            "outputs/review?.pdf",
        ] {
            assert!(read(packet(json!([{"kind":"replace_pages","pages":[1],"source_pages":[1],"source":source}]))).is_err(), "accepted {source}");
        }
        assert!(read(packet(json!([{"kind":"replace_pages","pages":[1],"source_pages":[1],"source":"outputs/reviews/revision 2.pdf"}]))).is_ok());
    }

    #[test]
    fn rejects_oversized_packets_fields_lists_and_control_characters() {
        assert_eq!(parse(&vec![b' '; MAX_PROPOSAL_BYTES + 1], 7), Err(ReviewError::TooLarge));
        assert!(read(packet(json!([{"kind":"comment","page":1,"text":"a".repeat(32_001)}]))).is_err());
        assert!(read(packet(json!([{"kind":"highlight","page":1,"quote":"a".repeat(4001)}]))).is_err());
        assert!(read(packet(json!([{"kind":"highlight","page":1,"quote":"x","comment":12}]))).is_err());
        assert!(read(packet(json!([{"kind":"comment","page":1,"text":"\u{0000}"}]))).is_err());
        assert!(read(packet(json!([{"kind":"comment","page":1,"text":"   "}]))).is_err());
        let mut value = packet(json!([{"kind":"comment","page":1,"text":"x"}]));
        value["summary"] = json!("a".repeat(2001));
        assert!(read(value).is_err());
        let changes = vec![json!({"kind":"comment","page":1,"text":"x"}); MAX_CHANGES + 1];
        assert!(read(packet(json!(changes))).is_err());
        assert!(read(packet(json!([{"kind":"rotate","pages":(1..=1001).collect::<Vec<_>>(),"degrees":90}]))).is_err());
        assert!(read(packet(json!([{"kind":"metadata","key":"Producer","value":"x"}]))).is_err());
        assert!(read(packet(json!([{"kind":"metadata","key":"Author","value":"a".repeat(4001)}]))).is_err());
    }
}
