//! Private native runtime configuration shared by the desktop and CLI hosts.
//! Reading it performs no process launch or network request. Secrets are only
//! selected from the owner's existing environment or a configured local env file.
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;

use crate::container::{ContainerBackend, Settings};
use crate::copilot::Backend;

const INVALID: &str = "The Hermes connection setup could not be read. Check the local copilot setup.";
const CREDENTIAL: &str = "Hermes needs an existing local model credential. Check the local copilot setup.";

/// Explicit environment override, or a private runtime configuration beside
/// the application/project. An absent configuration keeps the copilot offline.
pub fn configured_backend() -> Result<Option<Arc<dyn Backend>>, String> {
    let path = configured_path()?;
    path.map(|path| load_backend(&path)).transpose()
}

fn configured_path() -> Result<Option<PathBuf>, String> {
    let selection = connection_file();
    discover_path(std::env::var_os("PDFCRAFT_COPILOT_CONFIG").map(PathBuf::from), selection.as_deref(), std::env::current_exe().ok().as_deref())
}

/// Explicit local diagnostic. Starts neither an agent nor a model request.
/// Call on a worker: each existing Docker command has a ten-second deadline.
pub fn check_connection() -> Result<(), String> {
    let path = configured_path()?.ok_or_else(|| "Choose a Hermes connection file first.".to_string())?;
    let _backend = load_backend(&path)?;
    let value: Value = serde_json::from_slice(&read_small(&path, 64 * 1024)?).map_err(|_| INVALID.to_string())?;
    let docker = value.get("docker").and_then(Value::as_str).ok_or_else(|| INVALID.to_string())?;
    let image = value.get("image").and_then(Value::as_str).ok_or_else(|| INVALID.to_string())?;
    docker_check(Path::new(docker), &["version", "--format", "{{.Server.Version}}"])
        .map_err(|_| "Docker is unavailable. Open Docker Desktop, wait until it is ready, then check again.".to_string())?;
    docker_check(Path::new(docker), &["image", "inspect", image])
        .map_err(|_| "The configured Hermes runtime image is unavailable. Follow the setup guide to prepare it.".to_string())
}

fn docker_check(docker: &Path, args: &[&str]) -> Result<(), ()> {
    use std::process::{Command, Stdio};
    let mut command = Command::new(docker);
    command.args(args).env_clear().stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // Match the agent launcher's host-environment allowlist; model keys never
    // reach a diagnostic process. Docker keeps its existing user context.
    for name in ["PATH", "PATHEXT", "SYSTEMROOT", "WINDIR", "HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "PROGRAMDATA", "TEMP", "TMP", "TMPDIR"]
    {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().map_err(|_| ())?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return if status.success() { Ok(()) } else { Err(()) },
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(());
            }
        }
    }
}

fn connection_file() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(target_os = "windows"))]
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")));
    base.filter(|p| p.is_absolute()).map(|p| p.join("PdfCraft/copilot-connection.json"))
}

fn discover_path(explicit: Option<PathBuf>, selection: Option<&Path>, exe: Option<&Path>) -> Result<Option<PathBuf>, String> {
    let path = if let Some(path) = explicit {
        Some(path)
    } else if let Some(selection) = selection.filter(|p| p.exists()) {
        let value: Value = serde_json::from_slice(&read_small(selection, 64 * 1024)?).map_err(|_| INVALID.to_string())?;
        if value.get("format").and_then(Value::as_str) != Some("pdfcraft-copilot-connection/1") {
            return Err(INVALID.into());
        }
        let selected =
            value.get("runtime_config").and_then(Value::as_str).map(PathBuf::from).filter(|p| p.is_absolute()).ok_or_else(|| INVALID.to_string())?;
        Some(selected)
    } else {
        exe.and_then(|exe| {
            exe.parent()
                .and_then(|parent| parent.ancestors().take(4).map(|p| p.join(".document-workspaces/.runtime/copilot.json")).find(|p| p.is_file()))
        })
    };
    Ok(path)
}

/// Remember only the explicitly selected file path, never profile or credential contents.
/// Validation is offline: selecting a connection starts no agent or network request.
pub fn connect_and_remember(path: &Path) -> Result<Arc<dyn Backend>, String> {
    let path = fs::canonicalize(path).map_err(|_| INVALID.to_string())?;
    let backend = load_backend(&path)?;
    let selection = connection_file().ok_or_else(|| INVALID.to_string())?;
    remember_path(&selection, &path)?;
    Ok(backend)
}

fn remember_path(selection: &Path, path: &Path) -> Result<(), String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = selection.parent().ok_or_else(|| INVALID.to_string())?;
    fs::create_dir_all(parent).map_err(|_| INVALID.to_string())?;
    if fs::symlink_metadata(selection).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink()) {
        return Err(INVALID.into());
    }
    let data =
        serde_json::to_vec(&serde_json::json!({"format":"pdfcraft-copilot-connection/1", "runtime_config":path})).map_err(|_| INVALID.to_string())?;
    let temporary = parent.join(format!(".copilot-connection-{}-{}.tmp", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(|_| INVALID.to_string())?;
    let result = (|| {
        file.write_all(&data)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, selection)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|_| INVALID.to_string())
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
    let servers = configured_mcp_servers(&config, &environment)?;
    let project_runtime = match config.get("shared_project_runtime") {
        None => false,
        Some(value) => value.as_bool().ok_or_else(|| INVALID.to_string())?,
    };
    let settings = Settings::new(docker, image, workspace, template, environment)?
        .with_timeout(Duration::from_secs(timeout))?
        .with_mcp_servers(servers)?
        .with_project_runtime(project_runtime);
    Ok(Arc::new(ContainerBackend::new(settings)))
}

fn configured_mcp_servers(config: &Value, credentials: &BTreeMap<String, String>) -> Result<Vec<crate::acp::McpServer>, String> {
    let Some(value) = config.get("mcp_servers") else { return Ok(Vec::new()) };
    let definitions = value.as_array().ok_or_else(|| INVALID.to_string())?;
    if definitions.len() > 8 {
        return Err(INVALID.into());
    }
    let mut servers = Vec::new();
    for definition in definitions {
        let object = definition.as_object().ok_or_else(|| INVALID.to_string())?;
        if object.keys().any(|key| !["name", "command", "args"].contains(&key.as_str())) {
            return Err(INVALID.into());
        }
        let field = |key| object.get(key).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| INVALID.to_string());
        let args = match object.get("args") {
            None => Vec::new(),
            Some(value) => value
                .as_array()
                .filter(|a| a.len() <= 32)
                .ok_or_else(|| INVALID.to_string())?
                .iter()
                .map(|arg| arg.as_str().map(str::to_owned).ok_or_else(|| INVALID.to_string()))
                .collect::<Result<Vec<_>, _>>()?,
        };
        servers.push(crate::acp::McpServer { name: field("name")?, command: field("command")?, args });
    }
    let mut names = std::collections::HashSet::new();
    for server in &servers {
        server.validate().map_err(|_| INVALID.to_string())?;
        if !names.insert(&server.name)
            || credentials
                .values()
                .any(|secret| !secret.is_empty() && (server.command.contains(secret) || server.args.iter().any(|a| a.contains(secret))))
        {
            return Err(INVALID.into());
        }
    }
    Ok(servers)
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

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "pdfcraft-connection-test-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            for name in ["selection.json", "runtime.json", "other.json"] {
                let _ = fs::remove_file(self.0.join(name));
            }
            let _ = fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn remembered_connection_survives_application_moves_and_replacement() {
        let scratch = Scratch::new();
        let selection = scratch.0.join("selection.json");
        let runtime = scratch.0.join("runtime.json");
        fs::write(&runtime, b"private runtime contents never copied").unwrap();
        remember_path(&selection, &runtime).unwrap();
        let moved_exe = scratch.0.join("new-version/pdfcraft.exe");
        assert_eq!(discover_path(None, Some(&selection), Some(&moved_exe)).unwrap(), Some(runtime));
        assert!(!fs::read_to_string(&selection).unwrap().contains("contents never copied"));
        let other = scratch.0.join("other.json");
        remember_path(&selection, &other).unwrap();
        assert_eq!(discover_path(None, Some(&selection), None).unwrap(), Some(other));
    }

    #[test]
    fn invalid_saved_selection_is_reported_and_explicit_override_wins() {
        let scratch = Scratch::new();
        let selection = scratch.0.join("selection.json");
        fs::write(&selection, b"{broken").unwrap();
        assert!(discover_path(None, Some(&selection), None).is_err());
        let explicit = scratch.0.join("runtime.json");
        assert_eq!(discover_path(Some(explicit.clone()), Some(&selection), None).unwrap(), Some(explicit));
        assert!(discover_path(None, None, None).unwrap().is_none());
        assert!(connect_and_remember(&scratch.0.join("missing.json")).is_err());
        assert_eq!(fs::read(&selection).unwrap(), b"{broken");
    }

    #[test]
    fn selects_only_named_credential_without_execution_or_interpolation() {
        let text = "OTHER=private-other\nexport MODEL_KEY='synthetic=key#part' # comment\nMODEL_KEY=last-value\n";
        assert_eq!(env_value(text, "MODEL_KEY"), Some("last-value".into()));
        assert_eq!(env_value(text, "MISSING"), None);
        assert_eq!(env_value("MODEL_KEY=\"$(command)\"", "MODEL_KEY"), Some("$(command)".into()));
        assert_eq!(env_value("MODEL_KEY=\"a\\nb\"", "MODEL_KEY"), None);
        assert_eq!(env_value("MODEL_KEY='unterminated", "MODEL_KEY"), None);
    }

    #[test]
    fn explicitly_selected_mcp_servers_never_put_credentials_in_arguments() {
        let credentials = BTreeMap::from([("MODEL_KEY".into(), "synthetic-secret".into())]);
        assert!(configured_mcp_servers(&serde_json::json!({}), &credentials).unwrap().is_empty());
        let good =
            serde_json::json!({"mcp_servers":[{"name":"document_tools","command":"/opt/tools/bin/docling-mcp","args":["--transport","stdio"]}]});
        assert_eq!(configured_mcp_servers(&good, &credentials).unwrap().len(), 1);
        for bad in [
            serde_json::json!({"mcp_servers":[{"name":"tools","command":"/bin/python","args":["synthetic-secret"]}]}),
            serde_json::json!({"mcp_servers":[{"name":"tools","command":"/bin/python","env":[{"name":"KEY","value":"secret"}]}]}),
            serde_json::json!({"mcp_servers":[{"name":"tools","command":"relative-tool"}]}),
            serde_json::json!({"mcp_servers":[{"name":"same","command":"/bin/tool"},{"name":"same","command":"/bin/tool"}]}),
        ] {
            assert!(configured_mcp_servers(&bad, &credentials).is_err());
        }
    }
}
