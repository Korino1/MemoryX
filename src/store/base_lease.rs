//! Process-wide writer lease for a physical MemoryX base root.
//!
//! The lock file is only a stable OS locking target. Its presence never grants
//! ownership; ownership exists solely while the open file handle holds its lock.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use fs2::FileExt;
use parking_lot::Mutex;
use thiserror::Error;

const LEASE_FILE_NAME: &str = ".memoryx.writer.lock";

static LEASED_ROOTS: LazyLock<Mutex<HashSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Error while acquiring a base-root writer lease.
#[derive(Debug, Error)]
pub(crate) enum BaseLeaseError {
    #[error("store base root is not a directory: {root}")]
    NotDirectory { root: PathBuf },

    #[error("store base root is already held by a MemoryX writer: {root}")]
    Busy { root: PathBuf },

    #[error("failed to acquire store base lease for {root}: {source}")]
    Io {
        root: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// An exclusive writer lease for one physical base root.
///
/// The process-local registry rejects duplicate `MemoryX` opens before a second
/// independent set of mutable store components can be constructed. The file
/// handle supplies the equivalent exclusion across processes.
pub(crate) struct BaseLease {
    canonical_root: PathBuf,
    // The handle must stay open for the full lifetime of this exclusive lease.
    file: File,
}

fn is_lock_contended(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::WouldBlock {
        return true;
    }

    #[cfg(windows)]
    {
        // LockFileEx may surface these Win32 errors as PermissionDenied.
        matches!(error.raw_os_error(), Some(32 | 33))
    }

    #[cfg(not(windows))]
    {
        false
    }
}

impl BaseLease {
    pub(crate) fn acquire(root: &Path) -> Result<Self, BaseLeaseError> {
        fs::create_dir_all(root).map_err(|source| BaseLeaseError::Io {
            root: root.to_path_buf(),
            source,
        })?;

        let canonical_root = fs::canonicalize(root).map_err(|source| BaseLeaseError::Io {
            root: root.to_path_buf(),
            source,
        })?;

        if !canonical_root.is_dir() {
            return Err(BaseLeaseError::NotDirectory {
                root: canonical_root,
            });
        }

        let mut leased_roots = LEASED_ROOTS.lock();
        if !leased_roots.insert(canonical_root.clone()) {
            return Err(BaseLeaseError::Busy {
                root: canonical_root,
            });
        }

        let lock_path = canonical_root.join(LEASE_FILE_NAME);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| {
                leased_roots.remove(&canonical_root);
                BaseLeaseError::Io {
                    root: canonical_root.clone(),
                    source,
                }
            })?;

        match file.try_lock_exclusive() {
            Ok(()) => {}
            Err(source) if is_lock_contended(&source) => {
                leased_roots.remove(&canonical_root);
                return Err(BaseLeaseError::Busy {
                    root: canonical_root,
                });
            }
            Err(source) => {
                leased_roots.remove(&canonical_root);
                return Err(BaseLeaseError::Io {
                    root: canonical_root,
                    source,
                });
            }
        }

        drop(leased_roots);

        Ok(Self {
            canonical_root: canonical_root.clone(),
            file,
        })
    }

    pub(crate) fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }
}

impl Drop for BaseLease {
    fn drop(&mut self) {
        let _ = self.file.unlock();
        LEASED_ROOTS.lock().remove(&self.canonical_root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::api::{MemoryX, StoreConfig, StoreError};
    use std::process::Command;

    const CHILD_ROOT_ENV: &str = "MEMORYX_BASE_LEASE_CHILD_ROOT";

    #[test]
    fn same_process_alias_is_rejected_for_a_canonical_root() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let root = temp_dir.path().join("base");

        let _first = BaseLease::acquire(&root).unwrap();
        let error = match BaseLease::acquire(&root.join(".")) {
            Err(error) => error,
            Ok(_) => panic!("canonical alias unexpectedly acquired a second lease"),
        };

        assert!(matches!(error, BaseLeaseError::Busy { .. }));
    }

    #[test]
    fn different_roots_can_be_opened_concurrently() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let first = BaseLease::acquire(&temp_dir.path().join("first")).unwrap();
        let second = BaseLease::acquire(&temp_dir.path().join("second")).unwrap();

        assert_ne!(first.canonical_root(), second.canonical_root());
    }

    #[test]
    fn lease_is_released_after_the_owner_drops() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let root = temp_dir.path().join("base");

        let first = BaseLease::acquire(&root).unwrap();
        drop(first);

        assert!(BaseLease::acquire(&root).is_ok());
    }

    #[test]
    fn child_process_reports_base_in_use_for_a_held_lease() {
        let Some(root) = std::env::var_os(CHILD_ROOT_ENV) else {
            return;
        };

        let error = match MemoryX::new(StoreConfig::new(Path::new(&root).to_path_buf())) {
            Err(error) => error,
            Ok(_) => panic!("second process unexpectedly acquired the base lease"),
        };
        assert!(matches!(error, StoreError::BaseInUse(_)));
        assert!(error.to_string().contains("exclusive writer lease is held"));
    }

    #[test]
    fn os_lock_rejects_a_second_process_for_the_same_root() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let root = temp_dir.path().join("base");
        let _lease = BaseLease::acquire(&root).unwrap();

        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "store::base_lease::tests::child_process_reports_base_in_use_for_a_held_lease",
                "--nocapture",
            ])
            .env(CHILD_ROOT_ENV, &root)
            .status()
            .unwrap();

        assert!(status.success());
    }
}
