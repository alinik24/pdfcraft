//! One optional project-owned Hermes home and tool cache, with one writer at a time.
//!
//! The lease is an OS file lock, not a directory-exists convention. Its file is never
//! unlinked: deleting it could let a second process lock a different inode. This is
//! cooperative ownership, not a filesystem sandbox; the container supplies isolation.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use crate::workspace::DocumentWorkspace;

pub(crate) struct ProjectRuntime {
    workspace: DocumentWorkspace,
    // Kept open throughout the agent process's lifetime, including shutdown.
    _lease: File,
}

impl ProjectRuntime {
    pub(crate) fn acquire(workspace_root: &Path) -> Result<Self, String> {
        let workspace = DocumentWorkspace::open_project_runtime(workspace_root)
            .map_err(|_| "The shared agent workspace could not be prepared safely.".to_string())?;
        let path =
            workspace.resolve_child(Path::new(".home/pdfcraft-owner.lock")).map_err(|_| "The shared agent owner file is invalid.".to_string())?;
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|_| "The shared agent owner file could not be opened.".to_string())?;
        // Validate again after opening; normal app operations do not replace ancestors.
        workspace.resolve_child(Path::new(".home/pdfcraft-owner.lock")).map_err(|_| "The shared agent owner file is invalid.".to_string())?;
        if !lease.metadata().map_err(|_| "The shared agent owner file is invalid.".to_string())?.is_file() {
            return Err("The shared agent owner file must be an ordinary file.".into());
        }
        match lease.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err("The project agent is connected to another document. Disconnect that conversation before connecting this one.".into());
            }
            Err(TryLockError::Error(_)) => return Err("The shared agent owner lock could not be acquired.".into()),
        }
        // Only the owner creates runtime working areas; no tools are installed here.
        workspace.ensure_directory(Path::new("tools")).map_err(|_| "The shared agent tools folder could not be prepared safely.".to_string())?;
        Ok(Self { workspace, _lease: lease })
    }

    pub(crate) fn workspace(&self) -> &DocumentWorkspace {
        &self.workspace
    }

    pub(crate) fn root(&self) -> &Path {
        self.workspace.root()
    }

    pub(crate) fn tools(&self) -> PathBuf {
        self.root().join("tools")
    }

    /// Stable per-project name lets a new host detect an orphan after a crash.
    /// The hash is only a naming aid; the owner file is the serialization boundary.
    pub(crate) fn container_name(&self) -> String {
        let text = self.root().to_string_lossy();
        #[cfg(windows)]
        let text = text.to_lowercase();
        let mut hash = 0xcbf29ce484222325u64;
        for byte in text.as_bytes() {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
        }
        format!("pdfcraft-project-agent-{hash:016x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn project_owner_is_exclusive_and_reopen_preserves_memory() -> Result<(), Box<dyn std::error::Error>> {
        let base =
            std::env::temp_dir().canonicalize()?.join(format!("pdfcraft-runtime-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&base)?;
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            let runtime = ProjectRuntime::acquire(&base).map_err(io::Error::other)?;
            let root = runtime.root().to_owned();
            let name = runtime.container_name();
            runtime.workspace().ensure_directory(Path::new(".hermes/memories"))?;
            runtime.workspace().write_file(Path::new(".hermes/memories/MEMORY.md"), b"Synthetic retained preference")?;
            assert!(ProjectRuntime::acquire(&base).is_err(), "another document/process must not share a live writer");
            let mut child = std::process::Command::new(std::env::current_exe()?);
            child
                .args(["--exact", "project_runtime::tests::lease_probe_child", "--ignored", "--nocapture"])
                .env("PDFCRAFT_RUNTIME_LOCK_PROBE", &base);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                child.creation_flags(0x0800_0000);
            }
            let mut child = child.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn()?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let status = loop {
                if let Some(status) = child.try_wait()? {
                    break status;
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(io::Error::other("cross-process lease probe timed out").into());
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            assert!(status.success(), "a distinct process must be refused while this owner is live");
            assert!(runtime.tools().is_dir());
            drop(runtime);
            let reopened = ProjectRuntime::acquire(&base).map_err(io::Error::other)?;
            assert_eq!(reopened.root(), root);
            assert_eq!(reopened.container_name(), name);
            assert_eq!(reopened.workspace().read_file(Path::new(".hermes/memories/MEMORY.md"), 100)?, b"Synthetic retained preference");
            Ok(())
        })();
        fs::remove_dir_all(base)?;
        result
    }

    #[test]
    #[ignore = "only launched by the bounded parent lease test"]
    fn lease_probe_child() {
        let Some(base) = std::env::var_os("PDFCRAFT_RUNTIME_LOCK_PROBE") else {
            return;
        };
        assert!(ProjectRuntime::acquire(Path::new(&base)).is_err(), "cross-process lock was not enforced");
    }

    #[test]
    fn projects_do_not_share_profiles_or_locks() -> Result<(), Box<dyn std::error::Error>> {
        let base = std::env::temp_dir().canonicalize()?.join(format!(
            "pdfcraft-runtime-projects-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&base)?;
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            let first = ProjectRuntime::acquire(&base.join("one")).map_err(io::Error::other)?;
            let second = ProjectRuntime::acquire(&base.join("two")).map_err(io::Error::other)?;
            assert_ne!(first.root(), second.root());
            assert_ne!(first.container_name(), second.container_name());
            assert!(first.root().ends_with(Path::new(".runtime/agent")));
            Ok(())
        })();
        fs::remove_dir_all(base)?;
        result
    }
}
