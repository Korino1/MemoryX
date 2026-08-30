//! Process-wide writer lease for a physical MemoryX base root.
//!
//! The lock file is only a stable OS locking target. Its presence never grants
//! ownership; ownership exists solely while the open file handle holds its lock.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use fs2::FileExt;
use parking_lot::Mutex;
use thiserror::Error;

const LEASE_FILE_NAME: &str = ".memoryx.writer.lock";

static LEASED_ROOTS: LazyLock<Mutex<HashSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
static NEXT_OWNER_EPOCH: AtomicU64 = AtomicU64::new(1);

/// Stable physical-root identity used by the production-v2 base binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PhysicalRootIdentity {
    pub(crate) platform_code: u8,
    pub(crate) canonical_root_key: Vec<u8>,
    pub(crate) stable_root_identity: Vec<u8>,
}

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
    // The directory handle pins the physical root and supplies the v2 binding.
    root_handle: File,
    physical_identity: PhysicalRootIdentity,
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

        let root_handle = open_root_directory(&canonical_root).map_err(|source| {
            let _ = file.unlock();
            leased_roots.remove(&canonical_root);
            BaseLeaseError::Io {
                root: canonical_root.clone(),
                source,
            }
        })?;
        let physical_identity =
            physical_root_identity(&root_handle, &canonical_root).map_err(|source| {
                let _ = file.unlock();
                leased_roots.remove(&canonical_root);
                BaseLeaseError::Io {
                    root: canonical_root.clone(),
                    source,
                }
            })?;

        drop(leased_roots);

        Ok(Self {
            canonical_root: canonical_root.clone(),
            file,
            root_handle,
            physical_identity,
        })
    }

    pub(crate) fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    pub(crate) fn physical_identity(&self) -> &PhysicalRootIdentity {
        &self.physical_identity
    }

    pub(crate) fn verify_physical_identity(&self) -> io::Result<()> {
        let current = physical_root_identity(&self.root_handle, &self.canonical_root)?;
        if current != self.physical_identity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "physical MemoryX root identity changed while its writer lease was held",
            ));
        }
        Ok(())
    }
}

/// The one owning authority retained by a live MemoryX instance.
pub(crate) struct LiveOwnerAuthority {
    lease: BaseLease,
    owner_epoch: u64,
    production_transaction_active: AtomicBool,
}

impl LiveOwnerAuthority {
    pub(crate) fn acquire(root: &Path) -> Result<Self, BaseLeaseError> {
        let lease = BaseLease::acquire(root)?;
        Ok(Self {
            lease,
            owner_epoch: NEXT_OWNER_EPOCH.fetch_add(1, Ordering::Relaxed),
            production_transaction_active: AtomicBool::new(false),
        })
    }

    pub(crate) fn canonical_root(&self) -> &Path {
        self.lease.canonical_root()
    }

    pub(crate) fn physical_identity(&self) -> &PhysicalRootIdentity {
        self.lease.physical_identity()
    }

    pub(crate) fn borrow_startup(
        &self,
    ) -> Result<BorrowedOwnerQuiescence<'_, StartupAdmission>, io::Error> {
        self.borrow_quiescence()
    }

    pub(crate) fn borrow_write(
        &self,
    ) -> Result<BorrowedOwnerQuiescence<'_, QuiescentWrite>, io::Error> {
        self.borrow_quiescence()
    }

    fn borrow_quiescence<Phase>(&self) -> Result<BorrowedOwnerQuiescence<'_, Phase>, io::Error> {
        self.lease.verify_physical_identity()?;
        self.production_transaction_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "nested production transaction is forbidden",
                )
            })?;
        Ok(BorrowedOwnerQuiescence {
            authority: self,
            owner_epoch: self.owner_epoch,
            _phase: PhantomData,
            _not_send_or_sync: PhantomData,
        })
    }
}

pub(crate) enum StartupAdmission {}
pub(crate) enum QuiescentWrite {}

/// Non-owning, non-cloneable authority borrowed from the one live owner.
pub(crate) struct BorrowedOwnerQuiescence<'owner, Phase> {
    authority: &'owner LiveOwnerAuthority,
    owner_epoch: u64,
    _phase: PhantomData<Phase>,
    // Rc is intentionally neither Send nor Sync. The token cannot cross threads.
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl<Phase> BorrowedOwnerQuiescence<'_, Phase> {
    pub(crate) fn canonical_root(&self) -> &Path {
        self.authority.canonical_root()
    }

    pub(crate) fn physical_identity(&self) -> &PhysicalRootIdentity {
        self.authority.physical_identity()
    }

    pub(crate) fn verify(&self) -> io::Result<()> {
        if self.owner_epoch != self.authority.owner_epoch
            || !self
                .authority
                .production_transaction_active
                .load(Ordering::Acquire)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "borrowed owner authority is no longer active",
            ));
        }
        self.authority.lease.verify_physical_identity()
    }
}

impl<Phase> Drop for BorrowedOwnerQuiescence<'_, Phase> {
    fn drop(&mut self) {
        self.authority
            .production_transaction_active
            .store(false, Ordering::Release);
    }
}

#[cfg(windows)]
fn open_root_directory(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_READ_ATTRIBUTES, FILE_TRAVERSE, SYNCHRONIZE,
    };

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    let mut options = OpenOptions::new();
    options
        .access_mode(FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "MemoryX physical root is not a non-reparse directory",
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn open_root_directory(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    options.open(path)
}

#[cfg(windows)]
fn physical_root_identity(
    root_handle: &File,
    _canonical_root: &Path,
) -> io::Result<PhysicalRootIdentity> {
    use std::mem::{MaybeUninit, size_of};
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FILE_NAME_NORMALIZED, FileIdInfo, GetFileInformationByHandleEx,
        GetFinalPathNameByHandleW, VOLUME_NAME_GUID,
    };

    let handle = root_handle.as_raw_handle() as _;
    let mut required = 512u32;
    let mut units = vec![0u16; required as usize];
    loop {
        // Safety: `handle` is borrowed from the live root File and `units`
        // exposes `required` writable UTF-16 code units for the duration of the call.
        let written = unsafe {
            GetFinalPathNameByHandleW(
                handle,
                units.as_mut_ptr(),
                required,
                FILE_NAME_NORMALIZED | VOLUME_NAME_GUID,
            )
        };
        if written == 0 {
            return Err(io::Error::last_os_error());
        }
        if written < required {
            units.truncate(written as usize);
            break;
        }
        if written > 32767 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "canonical Windows root exceeds the production-v2 limit",
            ));
        }
        required = written
            .checked_add(1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "root length overflow"))?;
        units.resize(required as usize, 0);
    }
    if units.is_empty() || units.len() > 32767 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "canonical Windows root length is outside the production-v2 limits",
        ));
    }
    if units.last() == Some(&(b'\\' as u16)) {
        let last_brace = units.iter().rposition(|unit| *unit == b'}' as u16);
        if last_brace.is_some_and(|index| index + 2 < units.len()) {
            units.pop();
        }
    }

    let mut info = MaybeUninit::<FILE_ID_INFO>::zeroed();
    // Safety: `handle` remains valid and `info` is correctly aligned writable
    // storage of exactly `FILE_ID_INFO` bytes.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            info.as_mut_ptr().cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // Safety: a successful call initialized the complete FILE_ID_INFO value.
    let info = unsafe { info.assume_init() };

    let count = u32::try_from(units.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "root length overflow"))?;
    let mut canonical_root_key = Vec::with_capacity(4 + units.len() * 2);
    canonical_root_key.extend_from_slice(&count.to_le_bytes());
    for unit in units {
        canonical_root_key.extend_from_slice(&unit.to_le_bytes());
    }
    let mut stable_root_identity = Vec::with_capacity(24);
    stable_root_identity.extend_from_slice(&info.VolumeSerialNumber.to_le_bytes());
    stable_root_identity.extend_from_slice(&info.FileId.Identifier);
    Ok(PhysicalRootIdentity {
        platform_code: 1,
        canonical_root_key,
        stable_root_identity,
    })
}

#[cfg(unix)]
fn physical_root_identity(
    root_handle: &File,
    canonical_root: &Path,
) -> io::Result<PhysicalRootIdentity> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let canonical_root_key = canonical_root.as_os_str().as_bytes().to_vec();
    if canonical_root_key.is_empty()
        || canonical_root_key.len() > 4096
        || canonical_root_key[0] != b'/'
        || canonical_root_key.contains(&0)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "canonical POSIX root is outside the production-v2 limits",
        ));
    }
    let metadata = root_handle.metadata()?;
    let mut stable_root_identity = Vec::with_capacity(16);
    stable_root_identity.extend_from_slice(&metadata.dev().to_le_bytes());
    stable_root_identity.extend_from_slice(&metadata.ino().to_le_bytes());
    Ok(PhysicalRootIdentity {
        platform_code: 2,
        canonical_root_key,
        stable_root_identity,
    })
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
