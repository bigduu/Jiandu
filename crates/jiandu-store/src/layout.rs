//! Private canonical layout and containment checks.

use crate::StoreError;
use jiandu_core::{MemoryId, MemoryScope};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::path::{Component, Path, PathBuf};

pub(crate) const STORE_METADATA_FILE: &str = "store.json";
pub(crate) const STORE_METADATA_INIT_FILE: &str = ".store.json.init";
pub(crate) const STORE_LOCK_FILE: &str = "LOCK";
pub(crate) const QUARANTINE_DIR: &str = "quarantine";

const REQUIRED_DIRECTORIES: &[&str] = &[
    "records",
    "records/principal",
    "records/project",
    "records/session",
    "records/instance_global",
    "lineages",
    "tombstones",
    "transactions",
    "receipts",
    "audit",
    "index",
    QUARANTINE_DIR,
    "backups",
];

pub(crate) fn normalize_data_dir(path: &Path, create: bool) -> Result<PathBuf, StoreError> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(StoreError::InvalidDataDirectory);
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| StoreError::io("resolve data directory", source))?
            .join(path)
    };
    if create {
        fs::create_dir_all(&absolute)
            .map_err(|source| StoreError::io("create data directory", source))?;
    }
    let metadata = fs::symlink_metadata(&absolute).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            StoreError::NotInitialized
        } else {
            StoreError::io("inspect data directory", source)
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StoreError::InvalidDataDirectory);
    }
    let canonical = fs::canonicalize(&absolute)
        .map_err(|source| StoreError::io("resolve data directory", source))?;
    Ok(canonical)
}

pub(crate) fn create_layout(root: &Path) -> Result<(), StoreError> {
    for relative in REQUIRED_DIRECTORIES {
        let path = safe_join(root, Path::new(relative))?;
        ensure_no_symlink_components(&path)?;
        fs::create_dir_all(&path)
            .map_err(|source| StoreError::io("create store layout", source))?;
        ensure_directory(root, &path)?;
        set_private_directory_permissions(&path)?;
    }
    for relative in REQUIRED_DIRECTORIES.iter().rev() {
        sync_directory(&safe_join(root, Path::new(relative))?)?;
    }
    sync_directory(root)?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), StoreError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| StoreError::io("sync store layout directory", source))
}

/// Validate that an uncommitted directory is either empty or contains only
/// artifacts that an interrupted Jiandu initialization could have created.
///
/// The persistent lock is the ownership marker; a metadata-init file is only
/// accepted alongside it. Merely finding directories named `records` or
/// `index` is not enough to claim an existing directory, and any record/file
/// below the fixed empty layout makes initialization fail without adding a
/// Jiandu lock or metadata file.
pub(crate) fn validate_initialization_state(root: &Path) -> Result<(), StoreError> {
    ensure_directory(root, root)?;
    let mut pending = vec![root.to_path_buf()];
    let mut saw_entry = false;
    let mut saw_ownership_marker = false;

    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory)
            .map_err(|source| StoreError::io("inspect initialization directory", source))?;
        for entry in entries {
            let entry =
                entry.map_err(|source| StoreError::io("read initialization entry", source))?;
            saw_entry = true;
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|_| StoreError::InvalidDataDirectory)?;

            if relative == Path::new(STORE_LOCK_FILE) {
                ensure_regular_file(root, &path)?;
                saw_ownership_marker = true;
                continue;
            }
            if relative == Path::new(STORE_METADATA_INIT_FILE) {
                ensure_regular_file(root, &path)?;
                continue;
            }

            if REQUIRED_DIRECTORIES
                .iter()
                .any(|allowed| relative == Path::new(allowed))
            {
                ensure_directory(root, &path)?;
                pending.push(path);
                continue;
            }

            return Err(StoreError::InvalidDataDirectory);
        }
    }

    if saw_entry && !saw_ownership_marker {
        return Err(StoreError::InvalidDataDirectory);
    }
    Ok(())
}

pub(crate) fn validate_layout(root: &Path) -> Result<(), StoreError> {
    for relative in REQUIRED_DIRECTORIES {
        ensure_private_directory(root, &safe_join(root, Path::new(relative))?)?;
    }
    Ok(())
}

pub(crate) fn harden_data_directory(root: &Path) -> Result<(), StoreError> {
    ensure_directory(root, root)?;
    set_private_directory_permissions(root)
}

pub(crate) fn validate_private_data_directory(root: &Path) -> Result<(), StoreError> {
    ensure_directory(root, root)?;
    validate_private_directory_permissions(root, StoreError::InvalidDataDirectory)
}

pub(crate) fn safe_join(root: &Path, relative: &Path) -> Result<PathBuf, StoreError> {
    if relative.is_absolute()
        || relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(StoreError::UnsafePath);
    }
    let path = root.join(relative);
    if !path.starts_with(root) {
        return Err(StoreError::UnsafePath);
    }
    Ok(path)
}

pub(crate) fn ensure_directory(root: &Path, path: &Path) -> Result<(), StoreError> {
    ensure_contained(root, path)?;
    ensure_no_symlink_components(path)?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| StoreError::io("inspect store layout", source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StoreError::InvalidLayout);
    }
    Ok(())
}

pub(crate) fn ensure_regular_file(root: &Path, path: &Path) -> Result<(), StoreError> {
    ensure_contained(root, path)?;
    ensure_no_symlink_components(path)?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| StoreError::io("inspect store file", source))?;
    if metadata.file_type().is_symlink() {
        return Err(StoreError::UnsafePath);
    }
    if !metadata.is_file() {
        return Err(StoreError::InvalidLayout);
    }
    Ok(())
}

pub(crate) fn ensure_private_file(root: &Path, path: &Path) -> Result<(), StoreError> {
    ensure_regular_file(root, path)?;
    validate_private_file_permissions(path)
}

pub(crate) fn ensure_private_directory(root: &Path, path: &Path) -> Result<(), StoreError> {
    ensure_directory(root, path)?;
    validate_private_directory_permissions(path, StoreError::InvalidLayout)
}

pub(crate) fn set_private_file_permissions(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|source| StoreError::io("set private store file permissions", source))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn set_private_directory_permissions(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|source| StoreError::io("set private store directory permissions", source))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn validate_private_file_permissions(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::symlink_metadata(path)
            .map_err(|source| StoreError::io("inspect store file permissions", source))?
            .permissions()
            .mode()
            & 0o7777;
        if mode != 0o600 {
            return Err(StoreError::InvalidLayout);
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn validate_private_directory_permissions(
    path: &Path,
    error: StoreError,
) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::symlink_metadata(path)
            .map_err(|source| StoreError::io("inspect store directory permissions", source))?
            .permissions()
            .mode()
            & 0o7777;
        if mode != 0o700 {
            return Err(error);
        }
    }
    #[cfg(not(unix))]
    let _ = (path, error);
    Ok(())
}

pub(crate) fn ensure_regular_file_or_missing(root: &Path, path: &Path) -> Result<(), StoreError> {
    ensure_contained(root, path)?;
    ensure_no_symlink_components(path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(StoreError::UnsafePath),
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(StoreError::InvalidLayout),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(StoreError::io("inspect store file", source)),
    }
}

pub(crate) fn file_exists(root: &Path, path: &Path) -> Result<bool, StoreError> {
    ensure_contained(root, path)?;
    ensure_no_symlink_components(path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(StoreError::UnsafePath),
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(StoreError::InvalidLayout),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(StoreError::io("inspect memory record", source)),
    }
}

pub(crate) fn directory_exists(root: &Path, path: &Path) -> Result<bool, StoreError> {
    ensure_contained(root, path)?;
    ensure_no_symlink_components(path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(StoreError::UnsafePath),
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => Err(StoreError::InvalidLayout),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(StoreError::io("inspect scope directory", source)),
    }
}

pub(crate) fn scope_directory(root: &Path, scope: &MemoryScope) -> Result<PathBuf, StoreError> {
    let relative = match scope {
        MemoryScope::Principal { principal_id } => PathBuf::from("records")
            .join("principal")
            .join(storage_key("principal", principal_id.as_str())),
        MemoryScope::Project { project_id } => PathBuf::from("records")
            .join("project")
            .join(storage_key("project", project_id.as_str())),
        MemoryScope::Session { session_id } => PathBuf::from("records")
            .join("session")
            .join(storage_key("session", session_id.as_str())),
        MemoryScope::InstanceGlobal {} => PathBuf::from("records").join("instance_global"),
    };
    safe_join(root, &relative)
}

pub(crate) fn record_shard(id: &MemoryId) -> String {
    record_storage_key(id)[..2].to_owned()
}

pub(crate) fn record_storage_key(id: &MemoryId) -> String {
    storage_key("memory", id.as_str())
}

pub(crate) fn record_file_name(id: &MemoryId) -> String {
    format!("{}.md", record_storage_key(id))
}

pub(crate) fn record_path(
    root: &Path,
    scope: &MemoryScope,
    id: &MemoryId,
) -> Result<PathBuf, StoreError> {
    Ok(scope_directory(root, scope)?
        .join(record_shard(id))
        .join(record_file_name(id)))
}

pub(crate) fn validate_record_entry_name(name: &OsStr) -> Result<String, StoreError> {
    let name = name.to_str().ok_or(StoreError::UnsafePath)?;
    let key = name.strip_suffix(".md").ok_or(StoreError::InvalidLayout)?;
    if !valid_storage_key(key) {
        return Err(StoreError::InvalidLayout);
    }
    Ok(key.to_owned())
}

fn storage_key(domain: &str, value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"jiandu.store.path/v1\0");
    hasher.update(domain.as_bytes());
    hasher.update(b"\0");
    hasher.update(value.as_bytes());
    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(digest.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn valid_storage_key(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn ensure_no_symlink_components(path: &Path) -> Result<(), StoreError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(StoreError::UnsafePath);
            }
            Ok(_) => {}
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(StoreError::io("inspect store path", source)),
        }
    }
    Ok(())
}

fn ensure_contained(root: &Path, path: &Path) -> Result<(), StoreError> {
    if !root.is_absolute() || !path.is_absolute() || !path.starts_with(root) {
        return Err(StoreError::UnsafePath);
    }
    let relative = path
        .strip_prefix(root)
        .map_err(|_| StoreError::UnsafePath)?;
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(StoreError::UnsafePath);
    }
    Ok(())
}
