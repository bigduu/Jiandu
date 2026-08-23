//! Exclusive owning lock and secret-safe owner metadata.

use jiandu_core::Timestamp;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
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
    file: File,
}

impl StoreLock {
    pub(crate) fn acquire(root: &Path, create: bool) -> Result<Self, crate::StoreError> {
        let path = crate::layout::safe_join(root, Path::new(crate::layout::STORE_LOCK_FILE))?;
        crate::layout::ensure_regular_file_or_missing(root, &path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(create);
        let mut file = options
            .open(&path)
            .map_err(|source| crate::StoreError::io("open store lock", source))?;
        crate::layout::ensure_regular_file(root, &path)?;

        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => {}
            Err(source) if source.kind() == std::io::ErrorKind::WouldBlock => {
                let diagnostics = read_owner(&mut file);
                return Err(crate::StoreError::StoreLocked { owner: diagnostics });
            }
            Err(source) => return Err(crate::StoreError::io("acquire store lock", source)),
        }

        Ok(Self { file })
    }

    pub(crate) fn validate_initialization_marker(&mut self) -> Result<(), crate::StoreError> {
        let metadata = self
            .file
            .metadata()
            .map_err(|source| crate::StoreError::io("inspect initialization lock", source))?;
        if metadata.len() > 4_096 {
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            self.file
                .set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|source| {
                    crate::StoreError::io("set private store lock permissions", source)
                })?;
        }
        Ok(())
    }

    pub(crate) fn publish_owner(&mut self, owner: &LockOwner) -> Result<(), crate::StoreError> {
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
        Ok(())
    }
}

fn read_owner(file: &mut File) -> Option<LockOwnerDiagnostics> {
    if file.metadata().ok()?.len() > 4_096 {
        return None;
    }
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let owner: LockOwner = serde_json::from_slice(&bytes).ok()?;
    LockOwner::new(owner.instance_id, owner.process_id, owner.started_at).ok()
}
