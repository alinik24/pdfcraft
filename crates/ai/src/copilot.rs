//! Document-scoped conversational agent boundary. The native host supplies an
//! explicitly configured backend; merely opening a panel never launches one.
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use crate::workspace::DocumentIdentity;

pub struct DocumentContext {
    pub identity: DocumentIdentity,
    pub name: String,
}

pub struct Turn {
    pub pdf: Arc<Vec<u8>>,
    /// Monotonic native document edit generation, checked again before edits.
    pub revision: u64,
    /// One-based source page. The supplied passage may be a narrower selection.
    pub page: usize,
    pub text: String,
    pub message: String,
}

#[derive(Clone, Debug)]
pub enum Update {
    Ready,
    Text(String),
    Tool(String),
    Permission {
        id: Value,
        title: String,
        details: String,
    },
    /// A validated portable change packet. The UI shows a review before applying.
    Proposal {
        path: String,
        summary: String,
    },
    Finished,
    Failed(String),
}

/// Owns no UI or application document. File execution and lifetime confinement
/// are properties of the configured backend, not of the chat presentation.
pub trait Backend: Send + Sync {
    /// The host checks its write-root policy before connecting or creating files.
    fn workspace_root(&self) -> Option<&Path> {
        None
    }
    fn connect(&self, document: DocumentContext) -> Result<Box<dyn Conversation>, String>;
}

pub trait Conversation: Send {
    /// A backend may queue one initial turn while its protocol initializes.
    fn send(&mut self, turn: Turn) -> Result<(), String>;
    fn poll(&mut self) -> Result<Option<Update>, String>;
    fn cancel(&mut self) -> Result<(), String>;
    /// Approval is per operation. `false` rejects; no remembered approval exists.
    fn decide(&mut self, id: Value, allow_once: bool) -> Result<(), String>;
    fn workspace(&self) -> &Path;
    /// Bounded, mediated access to a generated output for an explicit review.
    fn read_output(&self, relative: &Path, max_bytes: usize) -> Result<Vec<u8>, String>;
}
