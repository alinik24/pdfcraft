//! Private, per-document working folders for the native copilot.
//!
//! All file operations below reject traversal, Windows aliases and existing links/reparse points,
//! and validate the complete ancestor chain again before use. These checks mediate cooperative
//! app file tools; they are **not an operating-system sandbox**. A path or a process's current
//! directory cannot confine arbitrary subprocesses, hard links, or a concurrent hostile process
//! replacing ancestors between checks. External agents must use a separate sandbox or route file
//! access through the host's mediated tools, without unrestricted shell/filesystem tools.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

/// Maximum snapshot or generated file accepted by the mediated writer (128 MiB).
pub const MAX_FILE_BYTES: usize = 128 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_IDENTITY_BYTES: usize = 16 * 1024;
const MANIFEST: &str = "workspace.json";
const FORMAT: &str = "pdfcraft-document-workspace/1";
const DIRECTORIES: &[&str] = &["input", "outputs", ".tmp", ".cache", ".home", ".hermes"];
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// A source's complete private identity, checked against the manifest on every reopen.
#[derive(Clone)]
pub struct DocumentIdentity {
    value: String,
    stem: String,
    extension: String,
}

impl DocumentIdentity {
    /// Identifies an existing source by its canonical path, without reading its contents.
    pub fn source(source: &Path) -> io::Result<Self> {
        let canonical = source.canonicalize()?;
        if !canonical.is_file() {
            return Err(invalid("the document source must be a file"));
        }
        let value = canonical.to_str().ok_or_else(|| invalid("the document path is not Unicode"))?;
        Self::build(
            format!("source:{value}"),
            canonical.file_stem().and_then(|s| s.to_str()).unwrap_or("document"),
            canonical.extension().and_then(|s| s.to_str()).unwrap_or("bin"),
        )
    }

    /// `stable_id` must be unique and retained by the caller for the lifetime of an unsaved document.
    pub fn unsaved(stable_id: &str, display_name: &str) -> io::Result<Self> {
        if stable_id.trim().is_empty() || stable_id.len() > MAX_IDENTITY_BYTES {
            return Err(invalid("an unsaved document needs a stable identity"));
        }
        let name = Path::new(display_name);
        Self::build(
            format!("unsaved:{stable_id}"),
            name.file_stem().and_then(|s| s.to_str()).unwrap_or("document"),
            name.extension().and_then(|s| s.to_str()).unwrap_or("bin"),
        )
    }

    fn build(value: String, stem: &str, extension: &str) -> io::Result<Self> {
        if value.len() > MAX_IDENTITY_BYTES || value.chars().any(char::is_control) {
            return Err(invalid("the document identity is too long or contains control characters"));
        }
        let stem = readable_stem(stem);
        let extension = if !extension.is_empty() && extension.len() <= 12 && extension.bytes().all(|b| b.is_ascii_alphanumeric()) {
            extension.to_ascii_lowercase()
        } else {
            "bin".into()
        };
        Ok(Self { value, stem, extension })
    }

    fn directory_name(&self) -> String {
        // Two independent deterministic FNV-1a accumulators are naming aids, not authentication.
        // The complete identity in the manifest detects even a suffix collision.
        let first = hash(self.value.as_bytes(), 0xcbf29ce484222325);
        let second = hash(self.value.as_bytes(), 0x84222325cbf29ce4);
        format!("{}-{first:016x}{second:016x}", self.stem)
    }
}

/// Owns no process and installs no tool. Paths and the manifest remain under the caller's root.
#[derive(Clone)]
pub struct DocumentWorkspace {
    root: PathBuf,
    snapshot: PathBuf,
}

impl DocumentWorkspace {
    /// Creates/reopens one workspace. Reopening never replaces a snapshot or generated file.
    /// An existing directory without a valid matching manifest is refused, never adopted.
    pub fn open(configured_root: &Path, identity: DocumentIdentity) -> io::Result<Self> {
        create_checked_directory(configured_root)?;
        let base = configured_root.canonicalize()?;
        let root = base.join(identity.directory_name());
        let created = match fs::create_dir(&root) {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
            Err(e) => return Err(e),
        };
        check_directory(&root)?;
        let manifest_path = root.join(MANIFEST);
        if created {
            let manifest = json!({ "format": FORMAT, "identity": identity.value, "snapshot": format!("input/current.{}", identity.extension) });
            let bytes = serde_json::to_vec_pretty(&manifest).map_err(io::Error::other)?;
            // A private newly created directory has no prior contents to replace. A crash while
            // writing leaves a refused manifest rather than silently adopting an unknown folder.
            let mut file = OpenOptions::new().write(true).create_new(true).open(&manifest_path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        check_plain_file(&manifest_path)?;
        let manifest: Value = serde_json::from_slice(&read_bounded(&manifest_path, MAX_MANIFEST_BYTES)?)
            .map_err(|_| invalid("the document workspace manifest is malformed"))?;
        let expected_snapshot = format!("input/current.{}", identity.extension);
        if manifest.get("format").and_then(Value::as_str) != Some(FORMAT)
            || manifest.get("identity").and_then(Value::as_str) != Some(identity.value.as_str())
            || manifest.get("snapshot").and_then(Value::as_str) != Some(expected_snapshot.as_str())
        {
            return Err(invalid("the document workspace belongs to a different identity or format"));
        }
        for name in DIRECTORIES {
            create_checked_directory(&root.join(name))?;
        }
        let workspace = Self { snapshot: root.join(expected_snapshot), root };
        workspace.check_root()?;
        workspace.resolve_child(workspace.snapshot.strip_prefix(&workspace.root).map_err(io::Error::other)?)?;
        Ok(workspace)
    }

    /// The document's working directory, suitable as a *cwd*, never as a sandbox claim.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Current input snapshot. Only [`Self::write_snapshot`] replaces it, never [`Self::open`].
    pub fn snapshot_path(&self) -> &Path {
        &self.snapshot
    }

    /// Resolve an allowed relative child whose parent already exists. The final child may be new.
    /// This deliberately excludes root metadata and unrecognised top-level directories.
    pub fn resolve_child(&self, relative: &Path) -> io::Result<PathBuf> {
        let names = child_components(relative)?;
        self.check_root()?;
        let mut path = self.root.clone();
        for (index, name) in names.iter().enumerate() {
            path.push(name);
            match fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    reject_link(&metadata)?;
                    if index + 1 < names.len() && !metadata.is_dir() {
                        return Err(invalid("a workspace parent is not a directory"));
                    }
                    let canonical = path.canonicalize()?;
                    if !canonical.starts_with(&self.root) {
                        return Err(invalid("the workspace path escapes its document folder"));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound && index + 1 == names.len() => {}
                Err(e) => return Err(e),
            }
        }
        Ok(path)
    }

    /// Create a nested directory under one of the recognised working areas.
    pub fn ensure_directory(&self, relative: &Path) -> io::Result<PathBuf> {
        let names = child_components(relative)?;
        self.check_root()?;
        let mut current = PathBuf::new();
        for name in names {
            current.push(name);
            let path = self.resolve_child(&current)?;
            match fs::create_dir(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
            check_directory(&path)?;
        }
        self.resolve_child(relative)
    }

    /// Reserves a lazily created venv directory. Does not execute Python or install anything.
    pub fn ensure_venv(&self) -> io::Result<PathBuf> {
        self.ensure_directory(Path::new(".venv"))
    }

    /// Bounded file read for mediated agent tools. Directory/link/special-file reads are refused.
    pub fn read_file(&self, relative: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
        if max_bytes > MAX_FILE_BYTES {
            return Err(invalid("the requested file read exceeds the workspace limit"));
        }
        let path = self.resolve_child(relative)?;
        check_plain_file(&path)?;
        read_bounded(&path, max_bytes as u64)
    }

    /// Atomically replace one generated file. Input snapshots and private metadata are read-only
    /// to agent file tools; the caller must explicitly use [`Self::write_snapshot`] for input.
    pub fn write_file(&self, relative: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        let names = child_components(relative)?;
        if names.first().is_some_and(|s| s == "input") {
            return Err(invalid("input snapshots are read-only to agent file tools"));
        }
        self.atomic_write(relative, bytes)
    }

    /// Replace the current context snapshot explicitly. Existing output files are untouched.
    pub fn write_snapshot(&self, bytes: &[u8]) -> io::Result<PathBuf> {
        let relative = self.snapshot.strip_prefix(&self.root).map_err(io::Error::other)?;
        self.atomic_write(relative, bytes)
    }

    /// Write an additional input snapshot with its actual format, e.g. `"pdf"` for the native
    /// PDF representation of an HTML source. Returns its path; [`Self::snapshot_path`] continues
    /// to identify the source-format snapshot. No original source is read or modified here.
    pub fn write_snapshot_as(&self, extension: &str, bytes: &[u8]) -> io::Result<PathBuf> {
        if extension.is_empty() || extension.len() > 12 || !extension.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(invalid("the snapshot extension must contain at most 12 letters or digits"));
        }
        self.atomic_write(Path::new(&format!("input/current.{}", extension.to_ascii_lowercase())), bytes)
    }

    fn check_root(&self) -> io::Result<()> {
        check_directory(&self.root)?;
        if self.root.canonicalize()? != self.root {
            return Err(invalid("the document workspace has moved or changed identity"));
        }
        Ok(())
    }

    fn atomic_write(&self, relative: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
        if bytes.len() > MAX_FILE_BYTES {
            return Err(invalid("the workspace file exceeds 128 MiB"));
        }
        let destination = self.resolve_child(relative)?;
        if destination.exists() {
            check_plain_file(&destination)?;
        }
        let parent = destination.parent().ok_or_else(|| invalid("the workspace file has no parent"))?;
        let mut temporary = None;
        for _ in 0..32 {
            let counter = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("._pdfcraft-write-{}-{counter:016x}", std::process::id()));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    temporary = Some((path, file));
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        let (temporary_path, mut file) =
            temporary.ok_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "no unused workspace temporary name was available"))?;
        let result = (|| {
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            // Recheck after I/O, before publication. Rename replaces a hard-linked destination's
            // directory entry instead of opening/truncating its external target.
            self.resolve_child(relative)?;
            fs::rename(&temporary_path, &destination)?;
            Ok(destination)
        })();
        if result.is_err() {
            // Remove only the exact temporary file created by this call, never a directory/tree.
            if self.check_root().is_ok() && check_plain_file(&temporary_path).is_ok() {
                let _ = fs::remove_file(&temporary_path);
            }
        }
        result
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn hash(bytes: &[u8], mut state: u64) -> u64 {
    for byte in bytes {
        state ^= u64::from(*byte);
        state = state.wrapping_mul(0x100000001b3);
    }
    state
}

fn readable_stem(name: &str) -> String {
    let mut candidate = String::new();
    for character in name.chars().take(64) {
        let character = if character.is_alphanumeric() || matches!(character, '-' | '_') { character } else { '-' };
        if candidate.len().saturating_add(character.len_utf8()) > 64 {
            break;
        }
        candidate.push(character);
    }
    let candidate = candidate.trim_matches('-');
    if candidate.is_empty() { "document".into() } else { format!("document-{candidate}") }
}

fn valid_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 240
        || name == "."
        || name == ".."
        || name.ends_with(['.', ' '])
        || name.starts_with("._pdfcraft-write-")
        || name.chars().any(|c| c.is_control() || matches!(c, ':' | '/' | '\\' | '<' | '>' | '"' | '|' | '?' | '*' | '~'))
    {
        return Err(invalid("the workspace path contains traversal, an alias, or an unusable name"));
    }
    let device = name.split('.').next().unwrap_or_default().trim_end_matches(' ').to_ascii_uppercase();
    let port = device.strip_prefix("COM").or_else(|| device.strip_prefix("LPT"));
    if matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$")
        || port.is_some_and(|s| matches!(s, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"))
    {
        return Err(invalid("Windows device names are not workspace files"));
    }
    Ok(())
}

fn child_components(path: &Path) -> io::Result<Vec<String>> {
    // Parse both separators on every platform, so Linux-created manifests/tools cannot smuggle
    // Windows paths through a later Windows run. Empty and dot components are refused too.
    let text = path.to_str().ok_or_else(|| invalid("workspace paths must be Unicode"))?;
    let mut names = Vec::new();
    for name in text.split(['/', '\\']) {
        valid_name(name)?;
        names.push(name.to_string());
        if names.len() > 64 {
            return Err(invalid("the workspace path is nested too deeply"));
        }
    }
    if !names.first().is_some_and(|s| DIRECTORIES.contains(&s.as_str()) || s == ".venv") {
        return Err(invalid("use an input, outputs, temporary, cache, home, Hermes or venv folder"));
    }
    Ok(names)
}

fn reject_link(metadata: &fs::Metadata) -> io::Result<()> {
    if metadata.file_type().is_symlink() {
        return Err(invalid("links are not allowed in document workspaces"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(invalid("Windows reparse points are not allowed in document workspaces"));
        }
    }
    Ok(())
}

fn check_ancestors(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(invalid("the configured workspace root must be absolute"));
    }
    let mut ancestor = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => ancestor.push(component.as_os_str()),
            Component::Normal(name) => {
                valid_name(name.to_str().ok_or_else(|| invalid("workspace roots must be Unicode"))?)?;
                ancestor.push(name);
                reject_link(&fs::symlink_metadata(&ancestor)?)?;
            }
            _ => return Err(invalid("the workspace root contains traversal")),
        }
    }
    Ok(())
}

fn check_directory(path: &Path) -> io::Result<()> {
    check_ancestors(path)?;
    if !fs::metadata(path)?.is_dir() {
        return Err(invalid("the workspace path must be a directory"));
    }
    Ok(())
}

fn create_checked_directory(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(invalid("the configured workspace root must be absolute"));
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => current.push(component.as_os_str()),
            Component::Normal(name) => {
                valid_name(name.to_str().ok_or_else(|| invalid("workspace roots must be Unicode"))?)?;
                current.push(name);
                match fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e),
                }
                check_directory(&current)?;
            }
            _ => return Err(invalid("the workspace root contains traversal")),
        }
    }
    Ok(())
}

fn check_plain_file(path: &Path) -> io::Result<()> {
    check_ancestors(path)?;
    let metadata = fs::symlink_metadata(path)?;
    reject_link(&metadata)?;
    if !metadata.is_file() {
        return Err(invalid("the workspace path must be an ordinary file"));
    }
    Ok(())
}

fn read_bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    if file.metadata()?.len() > limit {
        return Err(invalid("the workspace file exceeds the permitted read size"));
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid("the workspace file grew beyond the permitted read size"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!("pdfcraft-workspace-test-{}-{id}", std::process::id()));
            fs::create_dir(&root).unwrap();
            Self(root.canonicalize().unwrap())
        }
        fn workspace(&self) -> DocumentWorkspace {
            DocumentWorkspace::open(&self.0.join("workspaces"), DocumentIdentity::unsaved("stable-id", "review.html").unwrap()).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn same_name_sources_get_distinct_stable_workspaces() {
        let fixture = Fixture::new();
        let base = fixture.0.join("workspaces");
        let mut roots = Vec::new();
        for name in ["one", "two"] {
            let directory = fixture.0.join(name);
            fs::create_dir(&directory).unwrap();
            let source = directory.join("review.html");
            fs::write(&source, b"synthetic document").unwrap();
            let workspace = DocumentWorkspace::open(&base, DocumentIdentity::source(&source).unwrap()).unwrap();
            let reopened = DocumentWorkspace::open(&base, DocumentIdentity::source(&source).unwrap()).unwrap();
            assert_eq!(workspace.root(), reopened.root());
            roots.push(workspace.root().to_path_buf());
        }
        assert_ne!(roots.first(), roots.get(1));
    }

    #[test]
    fn reopen_preserves_input_outputs_and_creates_venv_only_on_request() {
        let fixture = Fixture::new();
        let workspace = fixture.workspace();
        assert!(!workspace.root().join(".venv").exists());
        workspace.write_snapshot(b"current input").unwrap();
        let native_pdf = workspace.write_snapshot_as("PDF", b"synthetic PDF snapshot").unwrap();
        assert_eq!(native_pdf.extension().unwrap(), "pdf");
        assert_eq!(fs::read(native_pdf).unwrap(), b"synthetic PDF snapshot");
        assert!(workspace.write_snapshot_as("../escape", b"escape").is_err());
        workspace.write_file(Path::new("outputs/review.txt"), b"generated review").unwrap();
        let reopened = fixture.workspace();
        assert_eq!(fs::read(reopened.snapshot_path()).unwrap(), b"current input");
        assert_eq!(reopened.read_file(Path::new("outputs/review.txt"), 100).unwrap(), b"generated review");
        assert!(reopened.ensure_venv().unwrap().is_dir());
        assert!(reopened.write_file(Path::new("input/current.html"), b"overwrite").is_err());
        assert!(reopened.read_file(Path::new("outputs/review.txt"), 2).is_err());
        reopened.write_file(Path::new("outputs/review.txt"), b"revised").unwrap();
        assert_eq!(reopened.read_file(Path::new("outputs/review.txt"), 100).unwrap(), b"revised");
    }

    #[test]
    fn traversal_devices_aliases_and_metadata_are_refused() {
        let fixture = Fixture::new();
        let workspace = fixture.workspace();
        for path in [
            "outputs/../outside",
            "outputs\\..\\outside",
            "../outputs/file",
            "/outputs/file",
            "C:\\outputs\\file",
            "outputs/CON.txt",
            "outputs/CON .txt",
            "outputs/LPT1",
            "outputs/a.",
            "outputs/a ",
            "outputs/a:b",
            "outputs/DOCUME~1",
            "outputs/./file",
            "outputs//file",
            "workspace.json",
            "outputs/._pdfcraft-write-1",
            "outputs/COM¹.txt",
        ] {
            assert!(workspace.resolve_child(Path::new(path)).is_err(), "accepted {path}");
        }
        assert!(workspace.ensure_directory(Path::new("outputs/a/b")).unwrap().is_dir());
        assert!(DocumentWorkspace::open(Path::new("relative-root"), DocumentIdentity::unsaved("id", "x").unwrap()).is_err());
    }

    #[test]
    fn malformed_and_collision_manifests_are_refused_without_changing_files() {
        let fixture = Fixture::new();
        let workspace = fixture.workspace();
        workspace.write_file(Path::new("outputs/keep.txt"), b"keep").unwrap();
        let manifest = workspace.root().join(MANIFEST);
        let original = fs::read(&manifest).unwrap();
        fs::write(&manifest, b"{").unwrap();
        assert!(DocumentWorkspace::open(&fixture.0.join("workspaces"), DocumentIdentity::unsaved("stable-id", "review.html").unwrap()).is_err());
        let mut collision: Value = serde_json::from_slice(&original).unwrap();
        collision["identity"] = json!("unsaved:someone-else");
        fs::write(&manifest, serde_json::to_vec(&collision).unwrap()).unwrap();
        assert!(DocumentWorkspace::open(&fixture.0.join("workspaces"), DocumentIdentity::unsaved("stable-id", "review.html").unwrap()).is_err());
        assert_eq!(fs::read(workspace.root().join("outputs/keep.txt")).unwrap(), b"keep");
    }

    #[test]
    fn unknown_existing_directory_is_never_adopted() {
        let fixture = Fixture::new();
        let identity = DocumentIdentity::unsaved("stable-id", "review.html").unwrap();
        let base = fixture.0.join("workspaces");
        let directory = base.join(identity.directory_name());
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("keep.txt"), b"unrelated").unwrap();
        assert!(DocumentWorkspace::open(&base, identity).is_err());
        assert_eq!(fs::read(directory.join("keep.txt")).unwrap(), b"unrelated");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_and_symlink_root_are_refused() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let workspace = fixture.workspace();
        let outside = fixture.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), b"outside").unwrap();
        symlink(&outside, workspace.root().join("outputs/link")).unwrap();
        assert!(workspace.read_file(Path::new("outputs/link/secret"), 100).is_err());
        assert!(workspace.write_file(Path::new("outputs/link/new"), b"escape").is_err());
        assert!(DocumentWorkspace::open(&workspace.root().join("outputs/link"), DocumentIdentity::unsaved("other", "other.pdf").unwrap()).is_err());
        symlink(&outside, fixture.0.join("root-link")).unwrap();
        assert!(DocumentWorkspace::open(&fixture.0.join("root-link"), DocumentIdentity::unsaved("id", "x").unwrap()).is_err());
        assert!(!outside.join("new").exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_junction_escape_is_refused() {
        // Junctions need no developer-mode symlink privilege. The fixed fixture paths contain
        // no shell metacharacters; cmd's mklink is used only to create this test's junction.
        use std::os::windows::process::CommandExt;
        let fixture = Fixture::new();
        let workspace = fixture.workspace();
        let outside = fixture.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), b"outside").unwrap();
        let junction = workspace.root().join("outputs/link");
        let result = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(result.status.success(), "junction fixture creation failed");
        assert!(workspace.read_file(Path::new("outputs/link/secret"), 100).is_err());
        assert!(workspace.write_file(Path::new("outputs/link/new"), b"escape").is_err());
        assert!(DocumentWorkspace::open(&junction, DocumentIdentity::unsaved("other", "other.pdf").unwrap()).is_err());
        // Remove only the link before Fixture's cleanup, never its target.
        fs::remove_dir(&junction).unwrap();
        assert_eq!(fs::read(outside.join("secret")).unwrap(), b"outside");
    }
}
