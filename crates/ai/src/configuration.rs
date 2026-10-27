//! Private native runtime configuration shared by the desktop and CLI hosts.
//! Reading it performs no process launch or network request. Secrets are only
//! selected from the owner's existing environment or a configured local env file.
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::container::{ContainerBackend, Settings};
use crate::copilot::Backend;

const INVALID: &str = "The Hermes connection setup could not be read. Check the local copilot setup.";
const CREDENTIAL: &str = "Hermes needs an existing local model credential. Check the local copilot setup.";

/// Explicit environment override, or a private runtime configuration beside
/// the application/project. An absent configuration keeps the copilot offline.
pub fn configured_backend() -> Result<Option<Arc<dyn Backend>>, String> {
    let path = if let Some(path) = std::env::var_os("PDFCRAFT_COPILOT_CONFIG") {
        Some(PathBuf::from(path))
    } else {
        std::env::current_exe().ok().and_then(|exe| {
            exe.parent()
                .and_then(|parent| parent.ancestors().take(4).map(|p| p.join(".document-workspaces/.runtime/copilot.json")).find(|p| p.is_file()))
        })
    };
    path.map(|path| load_backend(&path)).transpose()
}

pub fn load_backend(path: &Path) -> Result<Arc<dyn Backend>, String> {
    let bytes = read_small(path, 64 * 1024)?;
    let config: Value = serde_json::from_slice(&bytes).map_err(|_| INVALID.to_string())?;
    if config.get("format").and_then(Value::as_str) != Some("pdfcraft-copilot-runtime/1") {
        return Err(INVALID.into());
    }
    let field = |name| config.get(name).and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| INVALID.to_string());
    let docker = PathBuf::from(field("docker")?);
    let image = field("image")?.to_owned();
    let workspace = PathBuf::from(field("workspace_root")?);
    let template = PathBuf::from(field("profile_template")?);
    let credentials = config.get("credentials").ok_or_else(|| INVALID.to_string())?;
    let names =
        credentials.get("variables").and_then(Value::as_array).filter(|v| !v.is_empty() && v.len() <= 16).ok_or_else(|| INVALID.to_string())?;
    let env_file = credentials.get("env_file").and_then(Value::as_str).map(PathBuf::from);
    let env_text = env_file.as_ref().map(|p| read_small(p, 256 * 1024)).transpose()?;
    let env_text = env_text.as_ref().map(|b| std::str::from_utf8(b).map_err(|_| CREDENTIAL.to_string())).transpose()?;
    let mut environment = BTreeMap::new();
    for name in names {
        let name = name.as_str().filter(|s| !s.is_empty() && s.len() <= 128).ok_or_else(|| INVALID.to_string())?;
        let value = std::env::var(name).ok().filter(|v| !v.is_empty()).or_else(|| env_text.and_then(|text| env_value(text, name)));
        let value = value.filter(|v| !v.is_empty() && v.len() <= 16_384 && !v.chars().any(char::is_control)).ok_or_else(|| CREDENTIAL.to_string())?;
        environment.insert(name.to_owned(), value);
    }
    let timeout = match config.get("timeout_seconds") {
        Some(value) => value.as_u64().filter(|n| (1..=600).contains(n)).ok_or_else(|| INVALID.to_string())?,
        None => 180,
    };
    let settings = Settings::new(docker, image, workspace, template, environment)?.with_timeout(Duration::from_secs(timeout))?;
    Ok(Arc::new(ContainerBackend::new(settings)))
}

fn read_small(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path).map_err(|_| INVALID.to_string())?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit {
        return Err(INVALID.into());
    }
    // The owner configures this trusted path; document paths never reach it.
    let mut bytes = Vec::new();
    fs::File::open(path).map_err(|_| INVALID.to_string())?.take(limit.saturating_add(1)).read_to_end(&mut bytes).map_err(|_| INVALID.to_string())?;
    if bytes.len() as u64 > limit {
        return Err(INVALID.into());
    }
    Ok(bytes)
}

/// Conservative dotenv subset for a single API credential. No interpolation,
/// command evaluation, multiline values, or mutation of process environment.
fn env_value(text: &str, wanted: &str) -> Option<String> {
    let mut found = None;
    for line in text.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((name, value)) = line.split_once('=') else { continue };
        if name.trim() != wanted {
            continue;
        }
        let value = value.trim();
        let parsed = if let Some(quote) = value.chars().next().filter(|c| *c == '"' || *c == '\'') {
            let rest = value.get(1..)?;
            let end = rest.find(quote)?;
            let tail = rest.get(end + 1..)?.trim();
            if !tail.is_empty() && !tail.starts_with('#') {
                return None;
            }
            let value = rest.get(..end)?;
            if value.contains('\\') {
                return None;
            }
            value
        } else {
            value.split_once(" #").map_or(value, |(v, _)| v).trim()
        };
        found = Some(parsed.to_owned());
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_only_named_credential_without_execution_or_interpolation() {
        let text = "OTHER=private-other\nexport MODEL_KEY='synthetic=key#part' # comment\nMODEL_KEY=last-value\n";
        assert_eq!(env_value(text, "MODEL_KEY"), Some("last-value".into()));
        assert_eq!(env_value(text, "MISSING"), None);
        assert_eq!(env_value("MODEL_KEY=\"$(command)\"", "MODEL_KEY"), Some("$(command)".into()));
        assert_eq!(env_value("MODEL_KEY=\"a\\nb\"", "MODEL_KEY"), None);
        assert_eq!(env_value("MODEL_KEY='unterminated", "MODEL_KEY"), None);
    }
}
