//! Experimental, headless HTML review. Original HTML is never DOM-serialized.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
use scraper::{ElementRef, Html, Selector};
#[cfg(feature = "native")]
pub mod native;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    ops::Range,
};

pub const MAX_HTML_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_ANNOTATIONS: usize = 10_000;
pub const MAX_TARGETS: usize = 10_000;
pub const MAX_TARGET_TEXT_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_REPLACEMENT_HTML_BYTES: usize = 64 * 1024;
const MAX_TEXT_TRAVERSAL_STEPS: usize = 4_000_000;
const FORMAT: &str = "pdfcraft-html-review/1.0";
const TARGET_SELECTOR: &str = "[data-pdfcraft-target-id],p,h1,h2,h3,h4,h5,h6,li,td,th,pre,blockquote,figure,img,div";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Invalid HTML review document: {0}")]
    Invalid(String),
    #[error("Invalid review JSON: {0}")]
    Json(#[from] serde_json::Error),
}
pub type Result<T> = std::result::Result<T, Error>;
fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnnotationStatus {
    #[default]
    Pending,
    Accepted,
    Rejected,
}

/// Offsets count Unicode scalar values in the target's concatenated DOM text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Annotation {
    pub id: String,
    pub target_id: String,
    pub exact: String,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub suffix: String,
    pub start: usize,
    pub end: usize,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub status: AnnotationStatus,
    /// Unknown annotation fields survive export, save and merge.
    #[serde(flatten)]
    pub metadata: BTreeMap<String, Value>,
}

impl Annotation {
    /// W3C Web Annotation representation; quote/position selectors refine the
    /// target-local CSS selection. Generic generated targets require the
    /// PdfCraft runtime target attributes, absent from the preserved source.
    /// The editable carrier retains all original fields and metadata separately.
    pub fn web_annotation(&self, document_id: &str) -> Value {
        let target_selector = data_selector(&self.target_id);
        json!({
            "@context": "http://www.w3.org/ns/anno.jsonld", "id": self.id,
            "type": "Annotation", "motivation": "commenting",
            "body": {"type": "TextualBody", "value": self.body, "format": "text/plain"},
            "target": {"type":"SpecificResource", "source":document_id,
                "pdfcraft:targetId":self.target_id,
                "selector":{"type":"CssSelector", "value":target_selector,
                    "refinedBy":[
                    {"type":"TextQuoteSelector", "exact":self.exact, "prefix":self.prefix, "suffix":self.suffix},
                    {"type":"TextPositionSelector", "start":self.start, "end":self.end}
                ]}},
            "created":self.created, "pdfcraft:status":self.status,
            "pdfcraft:color":self.color, "pdfcraft:replyTo":self.reply_to
        })
    }
}

fn css_escape(value: &str) -> String {
    value.chars().map(|ch| format!("\\{:x} ", u32::from(ch))).collect()
}
fn data_selector(target: &str) -> String {
    format!("[data-pdfcraft-target-id=\"{}\"]", css_escape(target))
}
fn structural_selector(mut node: ElementRef<'_>, source_elements: &BTreeSet<String>, steps: &mut usize) -> Result<String> {
    let mut parts = Vec::new();
    loop {
        *steps = steps.checked_sub(1).ok_or_else(|| invalid("HTML selector traversal budget exceeded"))?;
        let name = node.value().name();
        if !["html", "body", "head", "tbody"].contains(&name) || source_elements.contains(name) {
            let mut index = 1usize;
            for sibling in node.prev_siblings() {
                *steps = steps.checked_sub(1).ok_or_else(|| invalid("HTML selector traversal budget exceeded"))?;
                if sibling.value().as_element().is_some_and(|element| element.name() == name) {
                    index = index.saturating_add(1);
                }
            }
            parts.push(format!("{}:nth-of-type({index})", css_escape(name)));
        }
        let Some(parent) = node.parent().and_then(ElementRef::wrap) else {
            break;
        };
        node = parent;
    }
    parts.reverse();
    Ok(parts.join(" > "))
}

fn target_text(node: ElementRef<'_>, remaining_bytes: &mut usize, remaining_steps: &mut usize) -> Result<String> {
    let mut output = String::new();
    for child in node.descendants() {
        *remaining_steps = remaining_steps.checked_sub(1).ok_or_else(|| invalid("HTML target traversal budget exceeded"))?;
        let Some(text) = child.value().as_text() else {
            continue;
        };
        let mut excluded = false;
        for ancestor in child.ancestors() {
            *remaining_steps = remaining_steps.checked_sub(1).ok_or_else(|| invalid("HTML target traversal budget exceeded"))?;
            if ancestor.value().as_element().is_some_and(|element| ["script", "style"].contains(&element.name())) {
                excluded = true;
                break;
            }
        }
        if !excluded {
            let value: &str = text;
            *remaining_bytes = remaining_bytes.checked_sub(value.len()).ok_or_else(|| invalid("HTML cumulative target text exceeds 32 MiB"))?;
            output.push_str(value);
        }
    }
    Ok(output)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteSelector {
    pub exact: String,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub suffix: String,
    pub start: usize,
    pub end: usize,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Resolution {
    Anchored { start: usize, end: usize },
    Ambiguous,
    Orphan,
}

/// Resolve only within the supplied target. A valid original range wins;
/// otherwise quote/context must identify exactly one occurrence.
pub fn resolve(text: &str, quote: &QuoteSelector) -> Resolution {
    if quote.exact.is_empty() {
        return Resolution::Orphan;
    }
    let valid_context = |start: usize, end: usize| {
        text.get(..start).is_some_and(|s| s.ends_with(&quote.prefix)) && text.get(end..).is_some_and(|s| s.starts_with(&quote.suffix))
    };
    let scalar_byte = |offset: usize| text.char_indices().map(|(i, _)| i).chain(std::iter::once(text.len())).nth(offset);
    if let (Some(start), Some(end)) = (scalar_byte(quote.start), scalar_byte(quote.end))
        && start <= end
        && text.get(start..end) == Some(quote.exact.as_str())
        && valid_context(start, end)
    {
        return Resolution::Anchored { start: quote.start, end: quote.end };
    }
    let mut found = None;
    // char_indices includes overlapping matches, unlike str::match_indices.
    for (start, _) in text.char_indices() {
        let Some(end) = start.checked_add(quote.exact.len()) else {
            continue;
        };
        if text.get(start..end) == Some(quote.exact.as_str()) && valid_context(start, end) {
            if found.is_some() {
                return Resolution::Ambiguous;
            }
            let scalar_start = text.get(..start).map_or(0, |s| s.chars().count());
            found = Some(Resolution::Anchored { start: scalar_start, end: scalar_start.saturating_add(quote.exact.chars().count()) });
        }
    }
    found.unwrap_or(Resolution::Orphan)
}

#[derive(Debug, Clone)]
pub struct ReviewDocument {
    source: String,
    carrier: Option<Range<usize>>,
    manifest: Value,
    document_id: String,
    content_sha256: String,
    targets: BTreeMap<String, String>,
    source_selectors: BTreeMap<String, String>,
    annotations: Vec<Annotation>,
    review_metadata: BTreeMap<String, Value>,
}

#[derive(Serialize, Deserialize)]
struct Review {
    format: String,
    document_id: String,
    content_sha256: String,
    annotations: Vec<Annotation>,
    #[serde(default)]
    web_annotations: Option<Vec<Value>>,
    #[serde(default)]
    issue_id: Option<String>,
    #[serde(flatten)]
    metadata: BTreeMap<String, Value>,
}

impl ReviewDocument {
    pub fn parse(source: &str) -> Result<Self> {
        if source.len() > MAX_HTML_BYTES {
            return Err(invalid("HTML exceeds 16 MiB"));
        }
        let dom = Html::parse_document(source);
        let selector = |s| Selector::parse(s).map_err(|_| invalid("Internal selector invalid"));
        let manifest_selector = selector("[id=pdfcraft-manifest]")?;
        let mut manifests = dom.select(&manifest_selector);
        let manifest_node = manifests.next();
        if manifests.next().is_some() {
            return Err(invalid("Duplicate manifest"));
        }
        let manifest: Value = if let Some(node) = manifest_node {
            if node.value().name() != "script" || node.value().attr("type") != Some("application/json") {
                return Err(invalid("Manifest must be an application/json script"));
            }
            serde_json::from_str(&node.text().collect::<String>())?
        } else {
            Value::Null
        };
        let (carrier, source_elements) = find_carrier(source)?;
        let mut targets = BTreeMap::new();
        let mut source_selectors = BTreeMap::new();
        let mut remaining_bytes = MAX_TARGET_TEXT_BYTES;
        let mut remaining_steps = MAX_TEXT_TRAVERSAL_STEPS;
        for (index, node) in dom.select(&selector(TARGET_SELECTOR)?).enumerate() {
            if index >= MAX_TARGETS {
                return Err(invalid("More than 10000 HTML targets"));
            }
            let id = node.value().attr("data-pdfcraft-target-id").map(str::to_owned).unwrap_or_else(|| format!("html:{index}"));
            let source_selector = if node.value().attr("data-pdfcraft-target-id").is_some() {
                data_selector(&id)
            } else {
                structural_selector(node, &source_elements, &mut remaining_steps)?
            };
            remaining_bytes = remaining_bytes.checked_sub(source_selector.len()).ok_or_else(|| invalid("HTML target inspection exceeds 32 MiB"))?;
            source_selectors.insert(id.clone(), source_selector);
            if id.is_empty() || targets.insert(id, target_text(node, &mut remaining_bytes, &mut remaining_steps)?).is_some() {
                return Err(invalid("Empty or duplicate target id"));
            }
        }
        if let Some(body) = dom.select(&selector("body")?).next() {
            if body.value().attr("data-pdfcraft-target-id").is_none() {
                source_selectors.insert("html:body".into(), "body".into());
            }
            if body.value().attr("data-pdfcraft-target-id").is_none()
                && targets.insert("html:body".into(), target_text(body, &mut remaining_bytes, &mut remaining_steps)?).is_some()
            {
                return Err(invalid("Reserved target id html:body"));
            }
        }
        let carrier_selector = selector("[id=pdfcraft-annotations]")?;
        let carriers: Vec<_> = dom.select(&carrier_selector).collect();
        if carriers.len() != usize::from(carrier.is_some()) {
            return Err(invalid("Ambiguous annotation carrier"));
        }
        if let Some(node) = carriers.first()
            && (node.value().name() != "script" || node.value().attr("type") != Some("application/json"))
        {
            return Err(invalid("Annotations require an application/json script"));
        }
        let mut digest = Sha256::new();
        if let Some(range) = &carrier {
            digest.update(source.get(..range.start).ok_or_else(|| invalid("Invalid carrier range"))?);
            digest.update(source.get(range.end..).ok_or_else(|| invalid("Invalid carrier range"))?);
        } else {
            digest.update(source.as_bytes());
        }
        let content_sha256 = format!("{:x}", digest.finalize());
        let document_id = if manifest.is_null() {
            format!("sha256:{content_sha256}")
        } else {
            manifest
                .get("document_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| invalid("Missing manifest document_id"))?
                .to_owned()
        };
        let manifest = if manifest.is_null() { json!({"format":"pdfcraft-generic-html/1.0", "document_id":document_id}) } else { manifest };
        let mut document = Self {
            source: source.to_owned(),
            carrier,
            manifest,
            document_id,
            content_sha256,
            targets,
            source_selectors,
            annotations: Vec::new(),
            review_metadata: BTreeMap::new(),
        };
        if let Some(node) = carriers.first() {
            let value: Value = serde_json::from_str(&node.text().collect::<String>())?;
            document.merge_review(value)?;
        }
        Ok(document)
    }
    pub fn manifest(&self) -> &Value {
        &self.manifest
    }
    pub fn document_id(&self) -> &str {
        &self.document_id
    }
    pub fn content_sha256(&self) -> &str {
        &self.content_sha256
    }
    pub fn targets(&self) -> &BTreeMap<String, String> {
        &self.targets
    }
    pub fn annotations(&self) -> &[Annotation] {
        &self.annotations
    }

    /// Original editable source without the editor-owned annotation carrier.
    pub fn source_html(&self) -> String {
        match &self.carrier {
            Some(range) => self.source.get(..range.start).into_iter().chain(self.source.get(range.end..)).collect(),
            None => self.source.clone(),
        }
    }

    /// Replace one target's inner HTML while preserving other source bytes.
    /// The caller owns sanitization; this performs an explicit source edit.
    pub fn replace_target_html(&self, target_id: &str, html: &str) -> Result<Self> {
        if html.len() > MAX_REPLACEMENT_HTML_BYTES {
            return Err(invalid("Replacement HTML exceeds 64 KiB"));
        }
        let selector = self.source_selectors.get(target_id).ok_or_else(|| invalid(format!("Unknown source target {target_id}")))?;
        let parsed: lol_html::Selector = selector.parse().map_err(|error| invalid(format!("Unsupported source selector: {error}")))?;
        let mut matches = 0usize;
        let rewritten = lol_html::rewrite_str(
            &self.source_html(),
            lol_html::RewriteStrSettings {
                element_content_handlers: vec![(
                    Cow::Owned(parsed),
                    lol_html::ElementContentHandlers::default().element(
                        |element: &mut lol_html::html_content::Element<'_, '_>| -> lol_html::HandlerResult {
                            matches = matches.saturating_add(1);
                            if matches > 1 {
                                return Err(Box::new(invalid("Source selector matches multiple elements; edit source explicitly")));
                            }
                            if !element.can_have_content() {
                                return Err(Box::new(invalid("Target cannot contain rich HTML")));
                            }
                            element.set_inner_content(html, lol_html::html_content::ContentType::Html);
                            Ok(())
                        },
                    ),
                )],
                ..lol_html::RewriteStrSettings::new()
            },
        )
        .map_err(|error| invalid(format!("Cannot replace target HTML: {error}")))?;
        if matches != 1 {
            return Err(invalid("Source selector matched no element; edit source explicitly"));
        }
        if rewritten.len() > MAX_HTML_BYTES {
            return Err(invalid("Revised HTML exceeds 16 MiB"));
        }
        self.revised_source(&rewritten)
    }

    fn web_annotations(&self, annotations: &[Annotation]) -> Vec<Value> {
        annotations
            .iter()
            .map(|annotation| {
                let mut value = annotation.web_annotation(&self.document_id);
                if let Some(selector) = self.source_selectors.get(&annotation.target_id) {
                    value["target"]["selector"]["value"] = json!(selector);
                }
                value
            })
            .collect()
    }

    /// Explicit source revision with conservative target-scoped reanchoring.
    /// Returns a replacement document; any failure leaves this document intact.
    /// Generic document identities change with the revised source hash.
    pub fn revised_source(&self, source: &str) -> Result<Self> {
        let mut revised = Self::parse(source)?;
        if revised.carrier.is_some() {
            return Err(invalid("Source editor input must omit the annotation carrier; use source_html()"));
        }
        let mut annotations = self.annotations.clone();
        for annotation in &mut annotations {
            let text = revised.targets.get(&annotation.target_id).ok_or_else(|| {
                invalid(format!(
                    "Source edit removed target {} for annotation {}; keep the target or remove the annotation explicitly",
                    annotation.target_id, annotation.id
                ))
            })?;
            if text.is_empty() && annotation.exact.is_empty() && annotation.start == 0 && annotation.end == 0 {
                continue;
            }
            let quote = QuoteSelector {
                exact: annotation.exact.clone(),
                prefix: annotation.prefix.clone(),
                suffix: annotation.suffix.clone(),
                start: annotation.start,
                end: annotation.end,
            };
            match resolve(text, &quote) {
                Resolution::Anchored { start, end } => {
                    annotation.start = start;
                    annotation.end = end;
                    let before: String = text.chars().take(start).collect();
                    let prefix_len = annotation.prefix.chars().count();
                    annotation.prefix = before.chars().skip(before.chars().count().saturating_sub(prefix_len)).collect();
                    annotation.suffix = text.chars().skip(end).take(annotation.suffix.chars().count()).collect();
                }
                Resolution::Ambiguous => {
                    return Err(invalid(format!(
                        "Source edit makes annotation {} ambiguous in target {}; add unique quote context or adjust the edit",
                        annotation.id, annotation.target_id
                    )));
                }
                Resolution::Orphan => {
                    return Err(invalid(format!(
                        "Source edit orphans annotation {} in target {}; retain its quote/context or remove the annotation explicitly",
                        annotation.id, annotation.target_id
                    )));
                }
            }
        }
        revised.replace_annotations(annotations)?;
        revised.review_metadata = self.review_metadata.clone();
        Ok(revised)
    }

    /// DOM serialization for runtime display ONLY. Saving always uses source
    /// bytes. The caller must sanitize active content before rendering this.
    pub fn preview_html(&self) -> String {
        let mut dom = Html::parse_document(&self.source);
        let document_language = Selector::parse("html")
            .ok()
            .and_then(|selector| dom.select(&selector).next())
            .and_then(|node| node.value().attr("lang"))
            .map(str::to_owned);
        let Ok(selector) = Selector::parse(TARGET_SELECTOR) else {
            return String::new();
        };
        let mut assignments: Vec<_> = dom
            .select(&selector)
            .enumerate()
            .map(|(index, node)| {
                (node.id(), node.value().attr("data-pdfcraft-target-id").map(str::to_owned).unwrap_or_else(|| format!("html:{index}")))
            })
            .collect();
        if let Ok(body) = Selector::parse("body")
            && let Some(node) = dom.select(&body).next()
        {
            let body_id = node.id();
            let needs_language = node.value().attr("lang").is_none();
            if node.value().attr("data-pdfcraft-target-id").is_none() {
                assignments.push((node.id(), "html:body".into()));
            }
            if needs_language
                && let Some(language) = document_language
                && let Some(mut body_node) = dom.tree.get_mut(body_id)
                && let scraper::Node::Element(element) = body_node.value()
            {
                let mut name = element.name.clone();
                name.ns = "".into();
                name.prefix = None;
                name.local = "lang".into();
                element.attrs.insert(name, language.into());
            }
        }
        for (id, target) in assignments {
            if let Some(mut node) = dom.tree.get_mut(id)
                && let scraper::Node::Element(element) = node.value()
            {
                let mut name = element.name.clone();
                name.ns = "".into();
                name.prefix = None;
                name.local = "data-pdfcraft-target-id".into();
                element.attrs.insert(name, target.into());
            }
        }
        dom.html()
    }

    pub fn replace_annotations(&mut self, annotations: Vec<Annotation>) -> Result<()> {
        self.validate(&annotations)?;
        self.annotations = annotations;
        Ok(())
    }
    fn validate(&self, annotations: &[Annotation]) -> Result<()> {
        if annotations.len() > MAX_ANNOTATIONS {
            return Err(invalid("More than 10000 annotations"));
        }
        let mut ids = BTreeSet::new();
        for annotation in annotations {
            if annotation.id.is_empty() || !ids.insert(annotation.id.as_str()) {
                return Err(invalid("Empty or duplicate annotation id"));
            }
            let text = self.targets.get(&annotation.target_id).ok_or_else(|| invalid("Unknown annotation target"))?;
            if annotation.start > annotation.end
                || annotation.end > text.chars().count()
                || (annotation.start == annotation.end && !(text.is_empty() && annotation.start == 0 && annotation.exact.is_empty()))
            {
                return Err(invalid("Invalid annotation scalar offsets"));
            }
            let exact: String = text.chars().skip(annotation.start).take(annotation.end.saturating_sub(annotation.start)).collect();
            if exact != annotation.exact {
                return Err(invalid("Annotation exact quote differs at target offsets"));
            }
            let before: String = text.chars().take(annotation.start).collect();
            let after: String = text.chars().skip(annotation.end).collect();
            if !before.ends_with(&annotation.prefix) || !after.starts_with(&annotation.suffix) {
                return Err(invalid("Annotation quote context differs"));
            }
            if annotation.metadata.keys().any(|k| {
                ["id", "target_id", "exact", "prefix", "suffix", "start", "end", "body", "color", "created", "reply_to", "status"]
                    .contains(&k.as_str())
            }) {
                return Err(invalid("Reserved annotation metadata key"));
            }
        }
        let parents: BTreeMap<_, _> = annotations.iter().map(|annotation| (annotation.id.as_str(), annotation.reply_to.as_deref())).collect();
        let mut checked = BTreeSet::new();
        for annotation in annotations {
            let mut seen = BTreeSet::new();
            let mut next = Some(annotation.id.as_str());
            while let Some(id) = next {
                if checked.contains(id) {
                    break;
                }
                if !seen.insert(id) {
                    return Err(invalid("Cyclic annotation reply chain"));
                }
                next = *parents.get(id).ok_or_else(|| invalid("Reply references missing annotation"))?;
            }
            checked.extend(seen);
        }
        Ok(())
    }
    pub fn export_review(&self) -> Value {
        let web_annotations = self.web_annotations(&self.annotations);
        let issue_id = self.manifest.get("document_id").and_then(Value::as_str).unwrap_or(&self.document_id);
        let mut result = json!({"format": FORMAT, "document_id": self.document_id, "issue_id": issue_id, "content_sha256": self.content_sha256, "annotations": self.annotations, "web_annotations": web_annotations});
        if let Some(object) = result.as_object_mut() {
            for (key, value) in &self.review_metadata {
                object.insert(key.clone(), value.clone());
            }
        }
        result
    }
    /// Merge atomically. Identical IDs are idempotent; changed IDs are conflicts.
    pub fn merge_review(&mut self, value: Value) -> Result<usize> {
        let review: Review = serde_json::from_value(value)?;
        if review.format != FORMAT || review.document_id != self.document_id || review.content_sha256 != self.content_sha256 {
            return Err(invalid("Review format, document id or source hash mismatch"));
        }
        let issue_id = self.manifest.get("document_id").and_then(Value::as_str).unwrap_or(&self.document_id);
        if review.issue_id.as_deref().is_some_and(|id| id != issue_id) {
            return Err(invalid("Review issue id mismatch"));
        }
        if let Some(web_annotations) = &review.web_annotations {
            let derived = self.web_annotations(&review.annotations);
            if web_annotations != &derived {
                return Err(invalid("Derived W3C annotations differ from editable annotations"));
            }
        }
        if review.annotations.len() > MAX_ANNOTATIONS {
            return Err(invalid("More than 10000 imported annotations"));
        }
        let mut incoming_ids = BTreeSet::new();
        let mut merged = self.annotations.clone();
        let existing_by_id: BTreeMap<_, _> = self.annotations.iter().map(|annotation| (annotation.id.as_str(), annotation)).collect();
        for annotation in review.annotations {
            if !incoming_ids.insert(annotation.id.clone()) {
                return Err(invalid("Duplicate imported annotation id"));
            }
            if let Some(existing) = existing_by_id.get(annotation.id.as_str()) {
                if *existing != &annotation {
                    return Err(invalid(format!("Conflicting annotation id {}", annotation.id)));
                }
            } else {
                merged.push(annotation);
            }
        }
        self.validate(&merged)?;
        let mut metadata = self.review_metadata.clone();
        for (key, value) in review.metadata {
            if metadata.get(&key).is_some_and(|old| old != &value) {
                return Err(invalid(format!("Conflicting review metadata {key}")));
            }
            metadata.insert(key, value);
        }
        let added = merged.len().saturating_sub(self.annotations.len());
        self.annotations = merged;
        self.review_metadata = metadata;
        Ok(added)
    }
    pub fn saved_html(&self) -> Result<String> {
        let json = serde_json::to_string(&self.export_review())?
            .replace('&', "\\u0026")
            .replace('<', "\\u003c")
            .replace('>', "\\u003e")
            .replace('\u{2028}', "\\u2028")
            .replace('\u{2029}', "\\u2029");
        let script = format!("<script id=\"pdfcraft-annotations\" type=\"application/json\">{json}</script>");
        let mut output = String::new();
        if let Some(range) = &self.carrier {
            output.push_str(self.source.get(..range.start).ok_or_else(|| invalid("Invalid carrier start"))?);
            output.push_str(&script);
            output.push_str(self.source.get(range.end..).ok_or_else(|| invalid("Invalid carrier end"))?);
        } else {
            output.push_str(&self.source);
            output.push_str(&script);
        }
        if output.len() > MAX_HTML_BYTES {
            return Err(invalid("Saved HTML exceeds 16 MiB"));
        }
        Ok(output)
    }
}

// Lexical source ranges complement HTML5 DOM inspection. Never search for a
// script substring inside comments, quoted attributes or raw-text elements.
fn find_carrier(source: &str) -> Result<(Option<Range<usize>>, BTreeSet<String>)> {
    let lower = source.to_ascii_lowercase();
    let mut offset = 0usize;
    let mut carrier = None;
    let mut source_elements = BTreeSet::new();
    while let Some(relative) = lower.get(offset..).and_then(|s| s.find('<')) {
        let start = offset.checked_add(relative).ok_or_else(|| invalid("Offset overflow"))?;
        let tail = lower.get(start..).ok_or_else(|| invalid("Invalid source offset"))?;
        if tail.get(1..).and_then(|s| s.chars().next()).is_none_or(|c| !c.is_ascii_alphabetic() && c != '/' && c != '!' && c != '?') {
            offset = start.saturating_add(1);
            continue;
        }
        if tail.starts_with("<!--") {
            offset = tail.find("-->").and_then(|n| start.checked_add(n)?.checked_add(3)).ok_or_else(|| invalid("Unterminated HTML comment"))?;
            continue;
        }
        let end = tag_end(source, start)?;
        let tag = source.get(start..end).ok_or_else(|| invalid("Invalid tag range"))?;
        let name: String = tag.chars().skip(1).take_while(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
        if !name.is_empty() {
            source_elements.insert(name.clone());
        }
        offset = end;
        // plaintext has no closing delimiter; noscript tokenization depends on
        // browser scripting mode. Neither permits a reliably located carrier.
        if ["plaintext", "noscript"].contains(&name.as_str()) {
            return Err(invalid(format!("Unsupported <{name}> element: annotation carrier cannot be placed safely")));
        }
        if ["script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes"].contains(&name.as_str()) {
            let close_pattern = format!("</{name}");
            let mut search = end;
            let close_start = loop {
                let candidate = lower
                    .get(search..)
                    .and_then(|s| s.find(&close_pattern))
                    .and_then(|n| search.checked_add(n))
                    .ok_or_else(|| invalid("Unterminated raw text element"))?;
                let after = candidate.checked_add(close_pattern.len()).ok_or_else(|| invalid("Offset overflow"))?;
                let delimiter = lower.as_bytes().get(after).copied();
                if delimiter.is_some_and(|b| b.is_ascii_whitespace() || b == b'>' || b == b'/') {
                    break candidate;
                }
                search = after;
            };
            let close_end = tag_end(source, close_start)?;
            if name == "script" {
                let attrs = attributes(tag, name.len().saturating_add(1))?;
                if attrs.get("id").is_some_and(|id| id == "pdfcraft-annotations") {
                    if carrier.is_some() || attrs.get("type").map(String::as_str) != Some("application/json") || attrs.contains_key("src") {
                        return Err(invalid("Ambiguous or executable annotation carrier"));
                    }
                    carrier = Some(start..close_end);
                }
            }
            offset = close_end;
        }
    }
    Ok((carrier, source_elements))
}
fn tag_end(source: &str, start: usize) -> Result<usize> {
    let mut quote = None;
    for (relative, ch) in source.get(start..).ok_or_else(|| invalid("Invalid tag offset"))?.char_indices() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
        } else if ch == '>' {
            return start.checked_add(relative).and_then(|n| n.checked_add(1)).ok_or_else(|| invalid("Offset overflow"));
        }
    }
    Err(invalid("Unterminated tag"))
}
fn attributes(tag: &str, offset: usize) -> Result<BTreeMap<String, String>> {
    let mut rest = tag.get(offset..).ok_or_else(|| invalid("Invalid attributes"))?;
    let mut attrs = BTreeMap::new();
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
        if rest.is_empty() || rest.starts_with('>') || rest.starts_with("/>") {
            break;
        }
        let key_len = rest.find(|c: char| c.is_ascii_whitespace() || c == '=' || c == '>').unwrap_or(rest.len());
        if key_len == 0 {
            return Err(invalid("Malformed script attributes"));
        }
        let key = rest.get(..key_len).ok_or_else(|| invalid("Invalid attribute name"))?.to_ascii_lowercase();
        rest = rest.get(key_len..).ok_or_else(|| invalid("Invalid attribute range"))?.trim_start_matches(|c: char| c.is_ascii_whitespace());
        let mut value = String::new();
        if let Some(next) = rest.strip_prefix('=') {
            rest = next.trim_start_matches(|c: char| c.is_ascii_whitespace());
            if let Some(quote) = rest.chars().next().filter(|c| *c == '\'' || *c == '"') {
                let inner = rest.get(1..).ok_or_else(|| invalid("Malformed quoted attribute"))?;
                let len = inner.find(quote).ok_or_else(|| invalid("Unterminated quoted attribute"))?;
                value = inner.get(..len).ok_or_else(|| invalid("Invalid attribute value"))?.to_owned();
                rest = inner.get(len.saturating_add(1)..).ok_or_else(|| invalid("Invalid attribute end"))?;
            } else {
                let len = rest.find(|c: char| c.is_ascii_whitespace() || c == '>').unwrap_or(rest.len());
                value = rest.get(..len).ok_or_else(|| invalid("Invalid attribute value"))?.to_owned();
                rest = rest.get(len..).ok_or_else(|| invalid("Invalid attribute end"))?;
            }
        }
        if attrs.insert(key, value).is_some() {
            return Err(invalid("Duplicate script attribute"));
        }
    }
    Ok(attrs)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn document() -> ReviewDocument {
        ReviewDocument::parse("<!doctype html><html><head><script id=\"pdfcraft-manifest\" type=\"application/json\">{\"document_id\":\"synthetic\",\"unknown\":42}</script></head><body><p data-pdfcraft-target-id=\"a\">é🙂 same same</p><p data-pdfcraft-target-id=\"b\">same</p></body></html>").expect("synthetic fixture")
    }
    fn annotation(id: &str) -> Annotation {
        Annotation {
            id: id.into(),
            target_id: "a".into(),
            exact: "🙂".into(),
            prefix: "é".into(),
            suffix: " same".into(),
            start: 1,
            end: 2,
            body: "comment".into(),
            color: "yellow".into(),
            created: String::new(),
            reply_to: None,
            status: AnnotationStatus::Pending,
            metadata: BTreeMap::new(),
        }
    }
    #[test]
    fn unicode_offsets_are_scalars_and_target_scoped() {
        let mut doc = document();
        doc.replace_annotations(vec![annotation("1")]).expect("unicode selection");
        let mut wrong = annotation("2");
        wrong.target_id = "b".into();
        assert!(doc.replace_annotations(vec![wrong]).is_err());
        assert_eq!(doc.annotations().len(), 1);
    }
    #[test]
    fn duplicate_phrases_require_position_or_context() {
        let mut quote = QuoteSelector { exact: "same".into(), prefix: String::new(), suffix: String::new(), start: 9, end: 13 };
        assert_eq!(resolve("é🙂 same same", &quote), Resolution::Ambiguous);
        quote.start = 8;
        quote.end = 12;
        assert_eq!(resolve("é🙂 same same", &quote), Resolution::Anchored { start: 8, end: 12 });
        quote.start = 100;
        quote.end = 104;
        quote.prefix = "é🙂 ".into();
        assert_eq!(resolve("é🙂 same same", &quote), Resolution::Anchored { start: 3, end: 7 });
        assert_eq!(resolve("another target", &quote), Resolution::Orphan);
    }
    #[test]
    fn source_roundtrip_hash_and_script_injection() {
        let mut doc = document();
        let mut a = annotation("1");
        a.body = "</script><script>alert(1)</script> & \u{2028}🙂".into();
        a.metadata.insert("future".into(), json!({"x":true}));
        doc.replace_annotations(vec![a]).expect("annotation");
        let saved = doc.saved_html().expect("save");
        assert!(saved.starts_with(&doc.source));
        assert!(!saved.contains("<script>alert"));
        let reopened = ReviewDocument::parse(&saved).expect("reopen");
        assert_eq!(reopened.content_sha256(), doc.content_sha256());
        assert_eq!(reopened.manifest(), doc.manifest());
        assert_eq!(reopened.targets(), doc.targets());
        assert_eq!(reopened.annotations(), doc.annotations());
        assert_eq!(saved, reopened.saved_html().expect("second save"));
    }
    #[test]
    fn merge_is_idempotent_and_conflict_atomic() {
        let mut doc = document();
        let mut remote = document();
        remote.replace_annotations(vec![annotation("1")]).expect("remote annotation");
        assert_eq!(doc.merge_review(remote.export_review()).expect("first merge"), 1);
        assert_eq!(doc.merge_review(remote.export_review()).expect("duplicate merge"), 0);
        let before = doc.export_review();
        let mut changed = annotation("1");
        changed.body = "changed".into();
        remote.replace_annotations(vec![annotation("new"), changed]).expect("remote revision");
        assert!(doc.merge_review(remote.export_review()).is_err());
        assert_eq!(before, doc.export_review());
        let mut wrong = before.clone();
        wrong["document_id"] = json!("wrong");
        assert!(doc.merge_review(wrong).is_err());
        let mut wrong = before.clone();
        wrong["content_sha256"] = json!("wrong");
        assert!(doc.merge_review(wrong).is_err());
        let mut wrong = before.clone();
        wrong["format"] = json!("future/2");
        assert!(doc.merge_review(wrong).is_err());
        assert_eq!(before, doc.export_review());
    }
    #[test]
    fn malformed_or_ambiguous_carriers_are_rejected() {
        let source = document().source;
        let executable = "<script id=pdfcraft-annotations>alert(1)</script>";
        assert!(ReviewDocument::parse(&format!("{source}{executable}")).is_err());
        let duplicate = "<script id=pdfcraft-annotations id=other type=application/json>{}</script>";
        assert!(ReviewDocument::parse(&format!("{source}{duplicate}")).is_err());
        let mut doc = document();
        doc.replace_annotations(vec![annotation("1")]).expect("annotation");
        let saved = doc.saved_html().expect("save");
        assert!(ReviewDocument::parse(&format!("{saved}{executable}")).is_err());
        assert!(ReviewDocument::parse(&format!("{source}<!-- {executable} -->")).is_ok());
    }
    #[test]
    fn generic_html_and_empty_image_note_roundtrip() {
        let mut doc = ReviewDocument::parse("<p>Hello world</p><img src='data:image/png;base64,AAAA'>").expect("generic HTML");
        assert_eq!(doc.targets().get("html:0").map(String::as_str), Some("Hello world"));
        let a: Annotation = serde_json::from_value(json!({"id":"note","target_id":"html:1","exact":"","start":0,"end":0,"body":"image note"}))
            .expect("annotation JSON");
        doc.replace_annotations(vec![a]).expect("image note");
        let reopened = ReviewDocument::parse(&doc.saved_html().expect("save")).expect("reopen");
        assert_eq!(doc.document_id(), reopened.document_id());
        assert_eq!(doc.targets(), reopened.targets());
    }
    #[test]
    fn w3c_selectors_include_scoped_quote_and_position() {
        let value = annotation("1").web_annotation("document");
        assert_eq!(value["target"]["selector"]["type"], "CssSelector");
        assert_eq!(value["target"]["selector"]["value"], "[data-pdfcraft-target-id=\"\\61 \"]");
        assert_eq!(value["target"]["selector"]["refinedBy"][0]["exact"], "🙂");
        assert_eq!(value["target"]["selector"]["refinedBy"][1]["start"], 1);
        assert_eq!(value["target"]["pdfcraft:targetId"], "a");
    }
    #[test]
    fn preview_preserves_document_language_and_explicit_body_language() {
        let doc = ReviewDocument::parse("<html lang=it><body><p>Ciao</p></body></html>").expect("document");
        assert!(doc.preview_html().contains("lang=\"it\""));
        let preview = Html::parse_document(&doc.preview_html());
        let body = Selector::parse("body").expect("selector");
        assert_eq!(preview.select(&body).next().and_then(|node| node.value().attr("lang")), Some("it"));
        let doc = ReviewDocument::parse("<html lang=it><body lang=de><p>Hallo</p></body></html>").expect("document");
        let preview = Html::parse_document(&doc.preview_html());
        assert_eq!(preview.select(&body).next().and_then(|node| node.value().attr("lang")), Some("de"));
    }
    #[test]
    fn hostile_target_text_allocations_are_bounded() {
        let source = format!("{}{}{}", "<div>".repeat(40), "x".repeat(1024 * 1024), "</div>".repeat(40));
        assert!(ReviewDocument::parse(&source).is_err());
        let source = "<p>x</p>".repeat(MAX_TARGETS.saturating_add(1));
        assert!(ReviewDocument::parse(&source).is_err());
    }
    #[test]
    fn explicit_source_revision_reanchors_and_rejects_orphans_atomically() {
        let mut doc = ReviewDocument::parse("<p>Hello world</p>").expect("document");
        let a: Annotation =
            serde_json::from_value(json!({"id":"selection","target_id":"html:0","exact":"world","start":6,"end":11,"body":"keep me"}))
                .expect("annotation");
        doc.replace_annotations(vec![a]).expect("annotation");
        let revised = doc.revised_source("<p>Good morning Hello world</p>").expect("reanchored revision");
        assert_eq!(revised.annotations()[0].start, 19);
        assert_ne!(doc.document_id(), revised.document_id());
        assert_eq!(revised.annotations()[0].id, "selection");
        let saved = doc.saved_html().expect("saved");
        let reopened = ReviewDocument::parse(&saved).expect("reopened");
        assert_eq!(reopened.source_html(), "<p>Hello world</p>");
        let before = doc.export_review();
        assert!(doc.revised_source("<p>Hello everyone</p>").is_err());
        assert!(doc.revised_source("<p>world and world</p>").is_err());
        assert_eq!(before, doc.export_review());
        assert_eq!(doc.source_html(), "<p>Hello world</p>");
    }
    #[test]
    fn incremental_reply_import_and_cycles_are_checked_on_union() {
        let mut doc = document();
        doc.replace_annotations(vec![annotation("parent")]).expect("parent");
        let mut reply = annotation("reply");
        reply.reply_to = Some("parent".into());
        let mut imported = doc.export_review();
        imported["annotations"] = json!([reply]);
        imported.as_object_mut().expect("object").remove("web_annotations");
        assert_eq!(doc.merge_review(imported).expect("incremental reply"), 1);
        let mut cycle_parent = annotation("parent");
        cycle_parent.reply_to = Some("reply".into());
        let before = doc.export_review();
        assert!(doc.replace_annotations(vec![cycle_parent, reply]).is_err());
        assert_eq!(before, doc.export_review());
    }
    #[test]
    fn w3c_review_exports_roundtrip_and_reject_stale_derived_data() {
        let mut doc = document();
        doc.replace_annotations(vec![annotation("one")]).expect("annotation");
        let exported = doc.export_review();
        assert_eq!(exported["issue_id"], "synthetic");
        assert_eq!(exported["web_annotations"][0], annotation("one").web_annotation("synthetic"));
        let mut recipient = document();
        recipient.merge_review(exported.clone()).expect("merge");
        assert_eq!(recipient.export_review(), exported);
        let reopened = ReviewDocument::parse(&doc.saved_html().expect("save")).expect("reopen");
        assert_eq!(reopened.export_review(), exported);
        let mut stale = exported.clone();
        stale["web_annotations"][0]["body"]["value"] = json!("stale");
        assert!(recipient.merge_review(stale).is_err());
        assert_eq!(recipient.export_review(), exported);
    }
    #[test]
    fn consuming_markup_and_invalid_carriers_fail_before_save() {
        for source in [
            "<p>Before</p><plaintext>all remaining markup is text",
            "<p>Before</p><PLAINTEXT>text</PLAINTEXT>",
            "<noscript><p>Parser-mode-dependent content</p></noscript>",
            "<p>Before</p><script>unterminated",
            "<p>Before</p><script id=pdfcraft-annotations>{}</script>",
            "<p>Before</p><script id=pdfcraft-annotations type=text/javascript>{}</script>",
        ] {
            assert!(ReviewDocument::parse(source).is_err(), "unsafe save input: {source}");
        }
    }
    #[test]
    fn rich_target_edit_preserves_unrelated_source_bytes() {
        let source = "<!doctype html><html><head><script>const keep = '<b>verbatim</b>';</script></head><body><!--unknown--><p title='one'>First</p><img src='data:image/png;base64,AAAA'><p class='second'>Second</p></body></html>";
        let doc = ReviewDocument::parse(source).expect("generic source");
        let revised = doc.replace_target_html("html:2", "<strong>Second edited</strong>").expect("rich edit");
        assert_eq!(revised.source_html(), source.replace("Second</p>", "<strong>Second edited</strong></p>"));
        let exported = revised.export_review();
        assert!(exported["web_annotations"].as_array().expect("array").is_empty());
        let mut explicit = document();
        explicit.replace_annotations(vec![annotation("one")]).expect("annotation");
        let revised = explicit.replace_target_html("b", "<em>same updated</em>").expect("explicit target");
        assert!(revised.source_html().contains("<p data-pdfcraft-target-id=\"b\"><em>same updated</em></p>"));
        assert_eq!(explicit.annotations(), revised.annotations());
    }
    #[test]
    fn rich_edit_reanchors_and_refuses_unsafe_selection_or_selector() {
        let mut doc = ReviewDocument::parse("<p>Hello world</p><p>Other</p>").expect("document");
        let a: Annotation = serde_json::from_value(json!({"id":"one","target_id":"html:0","exact":"world","start":6,"end":11})).expect("annotation");
        doc.replace_annotations(vec![a]).expect("annotation");
        let revised = doc.replace_target_html("html:0", "<b>Good morning Hello world</b>").expect("anchored edit");
        assert_eq!(revised.annotations()[0].start, 19);
        assert!(!revised.export_review()["web_annotations"][0]["target"]["selector"]["value"].as_str().expect("selector").contains("data-pdfcraft"));
        assert!(doc.replace_target_html("html:0", "world and world").is_err());
        assert!(doc.replace_target_html("html:0", "no quote remains").is_err());
        assert!(doc.replace_target_html("unknown", "text").is_err());
        doc.source_selectors.insert("html:0".into(), "missing".into());
        assert!(doc.replace_target_html("html:0", "text").is_err());
        doc.source_selectors.insert("html:0".into(), "p".into());
        assert!(doc.replace_target_html("html:0", "text").is_err());
        assert_eq!(doc.source_html(), "<p>Hello world</p><p>Other</p>");
    }
    #[test]
    fn rich_edit_caps_replacement_and_final_document() {
        let doc = ReviewDocument::parse("<p>x</p>").expect("document");
        assert!(doc.replace_target_html("html:0", &"x".repeat(MAX_REPLACEMENT_HTML_BYTES.saturating_add(1))).is_err());
        let source = format!("<!--{}--><p>x</p>", "x".repeat(MAX_HTML_BYTES.saturating_sub(100)));
        let doc = ReviewDocument::parse(&source).expect("large comment");
        assert!(doc.replace_target_html("html:0", &"x".repeat(200)).is_err());
    }
}
