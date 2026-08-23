//! Exclusive owning lock and secret-safe owner metadata.

use crate::layout::{FileIdentity, STORE_LOCK_FILE, StoreDirectory};
use jiandu_core::Timestamp;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use uuid::Uuid;

/// Identity written into the held `LOCK` inode.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LockOwner {
    pub instance_id: String,
    pub process_id: u32,
    pub started_at: Timestamp,
}

impl LockOwner {
    pub fn new(
        instance_id: impl Into<String>,
        process_id: u32,
        started_at: Timestamp,
    ) -> Result<Self, crate::StoreError> {
        let instance_id = instance_id.into();
        let parsed =
            Uuid::parse_str(&instance_id).map_err(|_| crate::StoreError::InvalidStoreMetadata)?;
        if parsed.hyphenated().to_string() != instance_id || process_id == 0 {
            return Err(crate::StoreError::InvalidStoreMetadata);
        }
        Ok(Self {
            instance_id,
            process_id,
            started_at,
        })
    }

    pub fn for_current_process() -> Result<Self, crate::StoreError> {
        Self::new(
            Uuid::new_v4().hyphenated().to_string(),
            std::process::id(),
            crate::metadata::timestamp_now()?,
        )
    }

    fn canonical_bytes(&self) -> Result<Vec<u8>, crate::StoreError> {
        let mut bytes =
            serde_json::to_vec_pretty(self).map_err(|_| crate::StoreError::InvalidStoreMetadata)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

/// Public path-free diagnostics returned when the lock is already owned.
pub type LockOwnerDiagnostics = LockOwner;

#[derive(Debug)]
pub(crate) struct StoreLock {
    // Unix locks the directory inode. Windows holds a directory handle without
    // delete sharing. Both keep ownership bound to the same fixed root.
    _root_file: File,
    file: File,
    identity: FileIdentity,
}

impl StoreLock {
    pub(crate) fn acquire(root: &StoreDirectory, create: bool) -> Result<Self, crate::StoreError> {
        let root_file = root.root_lock_file()?;
        #[cfg(unix)]
        match fs2::FileExt::try_lock_exclusive(&root_file) {
            Ok(()) => {}
            Err(source) if lock_is_contended(&source) => {
                return Err(crate::StoreError::StoreLocked {
                    owner: owner_diagnostics(root),
                });
            }
            Err(source) => return Err(crate::StoreError::io("acquire store root lock", source)),
        }

        let opened = if create {
            root.open_or_create_lock()
        } else {
            root.open_existing_lock().map_err(|error| match error {
                crate::StoreError::NotFound => crate::StoreError::InvalidLayout,
                other => other,
            })
        };
        let file = match opened {
            Ok(file) => file,
            #[cfg(windows)]
            Err(error) if lock_writer_is_contended(&error) => {
                return Err(crate::StoreError::StoreLocked {
                    owner: owner_diagnostics(root),
                });
            }
            Err(error) => return Err(error),
        };
        #[cfg(not(windows))]
        let mut file = file;
        let identity = FileIdentity::from_file(&file)?;

        #[cfg(not(windows))]
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => {}
            Err(source) if lock_is_contended(&source) => {
                let diagnostics = read_owner(&mut file);
                return Err(crate::StoreError::StoreLocked { owner: diagnostics });
            }
            Err(source) => return Err(crate::StoreError::io("acquire store lock", source)),
        }

        let lock = Self {
            _root_file: root_file,
            file,
            identity,
        };
        lock.validate_ownership(root)?;
        Ok(lock)
    }

    pub(crate) fn validate_ownership(
        &self,
        root: &StoreDirectory,
    ) -> Result<(), crate::StoreError> {
        let metadata = self
            .file
            .metadata()
            .map_err(|source| crate::StoreError::io("inspect held store lock", source))?;
        if !metadata.is_file() || !StoreDirectory::has_single_link(&self.file)? {
            return Err(crate::StoreError::UnsafePath);
        }
        if !root.file_identity_matches(Path::new(STORE_LOCK_FILE), self.identity)? {
            return Err(crate::StoreError::UnsafePath);
        }
        Ok(())
    }

    pub(crate) fn validate_initialization_marker(&mut self) -> Result<(), crate::StoreError> {
        let metadata = self
            .file
            .metadata()
            .map_err(|source| crate::StoreError::io("inspect initialization lock", source))?;
        if metadata.len() > 4_096 || !StoreDirectory::has_single_link(&self.file)? {
            return Err(crate::StoreError::InvalidDataDirectory);
        }
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|source| crate::StoreError::io("seek initialization lock", source))?;
        let mut bytes = Vec::new();
        self.file
            .read_to_end(&mut bytes)
            .map_err(|source| crate::StoreError::io("read initialization lock", source))?;
        if bytes.is_empty() {
            return Ok(());
        }
        let owner: LockOwner =
            serde_json::from_slice(&bytes).map_err(|_| crate::StoreError::InvalidDataDirectory)?;
        let owner = LockOwner::new(owner.instance_id, owner.process_id, owner.started_at)
            .map_err(|_| crate::StoreError::InvalidDataDirectory)?;
        if owner
            .canonical_bytes()
            .map_err(|_| crate::StoreError::InvalidDataDirectory)?
            != bytes
        {
            return Err(crate::StoreError::InvalidDataDirectory);
        }
        Ok(())
    }

    pub(crate) fn harden_permissions(&self) -> Result<(), crate::StoreError> {
        StoreDirectory::set_private_file(&self.file)
    }

    pub(crate) fn publish_owner(
        &mut self,
        root: &StoreDirectory,
        owner: &LockOwner,
    ) -> Result<(), crate::StoreError> {
        self.validate_ownership(root)?;
        let owner = LockOwner::new(
            owner.instance_id.clone(),
            owner.process_id,
            owner.started_at.clone(),
        )?;
        let bytes = owner.canonical_bytes()?;
        self.file
            .set_len(0)
            .map_err(|source| crate::StoreError::io("truncate store lock", source))?;
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|source| crate::StoreError::io("seek store lock", source))?;
        self.file
            .write_all(&bytes)
            .map_err(|source| crate::StoreError::io("write store lock owner", source))?;
        self.file
            .sync_all()
            .map_err(|source| crate::StoreError::io("sync store lock owner", source))?;
        self.validate_ownership(root)
    }
}

#[cfg(not(windows))]
fn lock_is_contended(source: &std::io::Error) -> bool {
    source.kind() == std::io::ErrorKind::WouldBlock
        || source.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

#[cfg(windows)]
fn lock_writer_is_contended(error: &crate::StoreError) -> bool {
    use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

    matches!(
        error,
        crate::StoreError::Io { source, .. }
            if source.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32)
    )
}

fn owner_diagnostics(root: &StoreDirectory) -> Option<LockOwnerDiagnostics> {
    let mut file = root
        .open_existing_regular(Path::new(STORE_LOCK_FILE), false)
        .ok()?;
    read_owner(&mut file)
}

fn read_owner(file: &mut File) -> Option<LockOwnerDiagnostics> {
    let metadata = file.metadata().ok()?;
    if metadata.len() > 4_096 || !StoreDirectory::has_single_link(file).ok()? {
        return None;
    }
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let owner: LockOwner = serde_json::from_slice(&bytes).ok()?;
    LockOwner::new(owner.instance_id, owner.process_id, owner.started_at).ok()
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::lock_is_contended;

    #[test]
    fn only_platform_lock_contention_is_classified_as_store_contention() {
        assert!(lock_is_contended(&fs2::lock_contended_error()));
        assert!(!lock_is_contended(&std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "ordinary I/O failure",
        )));
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::lock_writer_is_contended;

    #[test]
    fn only_writer_sharing_violations_are_classified_as_store_contention() {
        use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION};

        let sharing = crate::StoreError::io(
            "test lock open",
            std::io::Error::from_raw_os_error(ERROR_SHARING_VIOLATION as i32),
        );
        assert!(lock_writer_is_contended(&sharing));

        let denied = crate::StoreError::io(
            "test lock open",
            std::io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32),
        );
        assert!(!lock_writer_is_contended(&denied));
    }
}
