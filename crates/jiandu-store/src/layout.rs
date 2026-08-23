//! Private canonical layout and capability-relative filesystem access.

use crate::StoreError;
use cap_fs_ext::{
    DirExt as _, FollowSymlinks, MetadataExt as IdentityMetadataExt, OpenOptionsFollowExt as _,
};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use jiandu_core::{MemoryId, MemoryScope};
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};

#[cfg(all(test, unix))]
use std::cell::RefCell;

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

#[cfg(all(test, unix))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TestHookPoint {
    DirectoryOpen,
    DirectoryEntries,
    RegularOpen,
    Rename,
}

#[cfg(all(test, unix))]
struct TestHook {
    point: TestHookPoint,
    name: OsString,
    action: Option<Box<dyn FnOnce()>>,
}

#[cfg(all(test, unix))]
thread_local! {
    static TEST_HOOK: RefCell<Option<TestHook>> = const { RefCell::new(None) };
}

#[cfg(all(test, unix))]
pub(crate) fn install_test_hook(
    point: TestHookPoint,
    name: impl Into<OsString>,
    action: impl FnOnce() + 'static,
) {
    TEST_HOOK.with(|slot| {
        let previous = slot.replace(Some(TestHook {
            point,
            name: name.into(),
            action: Some(Box::new(action)),
        }));
        assert!(
            previous.is_none(),
            "a filesystem test hook is already installed"
        );
    });
}

#[cfg(all(test, unix))]
pub(crate) fn run_test_hook(point: TestHookPoint, name: &OsStr) {
    let action = TEST_HOOK.with(|slot| {
        let mut hook = slot.borrow_mut();
        let matches = hook
            .as_ref()
            .is_some_and(|hook| hook.point == point && hook.name == name);
        if matches {
            hook.take().and_then(|mut hook| hook.action.take())
        } else {
            None
        }
    });
    if let Some(action) = action {
        action();
    }
}

/// Stable identity taken from an opened filesystem object, never a pathname.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    pub(crate) fn from_file(file: &File) -> Result<Self, StoreError> {
        let metadata = cap_std::fs::Metadata::from_file(file)
            .map_err(|source| StoreError::io("inspect opened store file", source))?;
        Ok(Self::from_cap_metadata(&metadata))
    }

    fn from_cap_metadata(metadata: &cap_std::fs::Metadata) -> Self {
        Self {
            device: IdentityMetadataExt::dev(metadata),
            inode: IdentityMetadataExt::ino(metadata),
        }
    }
}

/// An opened, fixed store root. The ambient path is retained only to detect a
/// later namespace replacement; all store I/O is relative to `root`.
pub(crate) struct StoreDirectory {
    root: Dir,
    ambient_path: PathBuf,
    identity: FileIdentity,
}

impl StoreDirectory {
    pub(crate) fn open(path: &Path, create: bool) -> Result<Self, StoreError> {
        let absolute = absolute_data_path(path)?;
        let root = walk_ambient_directory(&absolute, create)?;
        let metadata = root
            .dir_metadata()
            .map_err(|source| StoreError::io("inspect opened data directory", source))?;
        if !metadata.is_dir() {
            return Err(StoreError::InvalidDataDirectory);
        }
        let identity = FileIdentity::from_cap_metadata(&metadata);
        Ok(Self {
            root,
            ambient_path: absolute,
            identity,
        })
    }

    /// Fail closed if the configured pathname has been renamed or replaced.
    /// The comparison is diagnostic only: subsequent I/O still uses `root`.
    pub(crate) fn validate_ambient_identity(&self) -> Result<(), StoreError> {
        let current = walk_ambient_directory(&self.ambient_path, false)
            .map_err(|_| StoreError::UnsafePath)?;
        let metadata = current.dir_metadata().map_err(|_| StoreError::UnsafePath)?;
        if FileIdentity::from_cap_metadata(&metadata) != self.identity {
            return Err(StoreError::UnsafePath);
        }
        Ok(())
    }

    pub(crate) fn root_lock_file(&self) -> Result<File, StoreError> {
        self.root
            .try_clone()
            .map(Dir::into_std_file)
            .map_err(|source| StoreError::io("clone store root handle", source))
    }

    pub(crate) fn sync_root(&self, operation: &'static str) -> Result<(), StoreError> {
        sync_directory_handle(&self.root, operation)
    }

    pub(crate) fn sync_directory(
        &self,
        relative: &Path,
        operation: &'static str,
    ) -> Result<(), StoreError> {
        let directory = self.open_directory(relative)?;
        sync_directory_handle(&directory, operation)
    }

    pub(crate) fn open_directory(&self, relative: &Path) -> Result<Dir, StoreError> {
        self.try_open_directory(relative)?
            .ok_or(StoreError::InvalidLayout)
    }

    pub(crate) fn try_open_directory(&self, relative: &Path) -> Result<Option<Dir>, StoreError> {
        validate_relative_path(relative)?;
        let mut current = self
            .root
            .try_clone()
            .map_err(|source| StoreError::io("clone store directory handle", source))?;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(StoreError::UnsafePath);
            };
            let metadata = match current.symlink_metadata(name) {
                Ok(metadata) => metadata,
                Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(source) => {
                    return Err(StoreError::io("inspect store directory component", source));
                }
            };
            if metadata.is_symlink() {
                return Err(StoreError::UnsafePath);
            }
            if !metadata.is_dir() {
                return Err(StoreError::InvalidLayout);
            }
            #[cfg(all(test, unix))]
            run_test_hook(TestHookPoint::DirectoryOpen, name);
            current = current.open_dir_nofollow(name).map_err(|source| {
                secure_open_error(&current, name, "open store directory component", source)
            })?;
        }
        Ok(Some(current))
    }

    pub(crate) fn create_directory_all(&self, relative: &Path) -> Result<(), StoreError> {
        validate_relative_path(relative)?;
        let mut current = self
            .root
            .try_clone()
            .map_err(|source| StoreError::io("clone store directory handle", source))?;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(StoreError::UnsafePath);
            };
            match current.symlink_metadata(name) {
                Ok(metadata) if metadata.is_symlink() => return Err(StoreError::UnsafePath),
                Ok(metadata) if !metadata.is_dir() => return Err(StoreError::InvalidLayout),
                Ok(_) => {}
                Err(source) if source.kind() == io::ErrorKind::NotFound => {
                    match current.create_dir(name) {
                        Ok(()) => {}
                        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(source) => {
                            return Err(StoreError::io("create store layout directory", source));
                        }
                    }
                }
                Err(source) => {
                    return Err(StoreError::io("inspect store layout directory", source));
                }
            }
            let next = current.open_dir_nofollow(name).map_err(|source| {
                secure_open_error(&current, name, "open created store directory", source)
            })?;
            set_private_directory_handle(&next)?;
            current = next;
        }
        Ok(())
    }

    pub(crate) fn try_open_regular(
        &self,
        relative: &Path,
        write: bool,
    ) -> Result<Option<File>, StoreError> {
        let (parent, name) = split_relative_file(relative)?;
        let Some(directory) = self.try_open_parent(&parent)? else {
            return Ok(None);
        };
        try_open_regular_at(&directory, &name, write)
    }

    pub(crate) fn open_existing_regular(
        &self,
        relative: &Path,
        write: bool,
    ) -> Result<File, StoreError> {
        self.try_open_regular(relative, write)?
            .ok_or(StoreError::NotFound)
    }

    pub(crate) fn open_or_create_lock(&self) -> Result<File, StoreError> {
        let directory = self.open_parent(Path::new(""))?;
        open_regular_at(
            &directory,
            OsStr::new(STORE_LOCK_FILE),
            true,
            true,
            false,
            true,
        )
    }

    pub(crate) fn open_existing_lock(&self) -> Result<File, StoreError> {
        let directory = self.open_parent(Path::new(""))?;
        open_regular_at(
            &directory,
            OsStr::new(STORE_LOCK_FILE),
            true,
            false,
            false,
            true,
        )
        .map_err(|error| match error {
            StoreError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound => {
                StoreError::NotFound
            }
            other => other,
        })
    }

    pub(crate) fn create_new_regular(&self, relative: &Path) -> Result<File, StoreError> {
        let (parent, name) = split_relative_file(relative)?;
        let directory = self.open_parent(&parent)?;
        match directory.symlink_metadata(&name) {
            Ok(metadata) if metadata.is_symlink() => return Err(StoreError::UnsafePath),
            Ok(_) => return Err(StoreError::InvalidStoreMetadata),
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(StoreError::io("inspect new store file", source));
            }
        }
        open_regular_at(&directory, &name, true, true, true, false).map_err(|error| match error {
            StoreError::Io { source, .. } if source.kind() == io::ErrorKind::AlreadyExists => {
                StoreError::InvalidStoreMetadata
            }
            other => other,
        })
    }

    pub(crate) fn regular_file_exists(&self, relative: &Path) -> Result<bool, StoreError> {
        self.try_open_regular(relative, false)
            .map(|file| file.is_some())
    }

    pub(crate) fn remove_regular_file(&self, relative: &Path) -> Result<(), StoreError> {
        let (parent, name) = split_relative_file(relative)?;
        let directory = self.open_parent(&parent)?;
        let _opened =
            try_open_regular_at(&directory, &name, false)?.ok_or(StoreError::NotInitialized)?;
        directory
            .remove_file(&name)
            .map_err(|source| StoreError::io("remove store file", source))
    }

    pub(crate) fn rename(&self, from: &Path, to: &Path) -> Result<(), StoreError> {
        let (from_parent, from_name) = split_relative_file(from)?;
        let (to_parent, to_name) = split_relative_file(to)?;
        let from_directory = self.open_parent(&from_parent)?;
        let to_directory = self.open_parent(&to_parent)?;
        from_directory
            .rename(&from_name, &to_directory, &to_name)
            .map_err(|source| StoreError::io("rename store entry", source))
    }

    pub(crate) fn file_identity_matches(
        &self,
        relative: &Path,
        expected: FileIdentity,
    ) -> Result<bool, StoreError> {
        let Some(file) = self.try_open_regular(relative, false)? else {
            return Ok(false);
        };
        Ok(FileIdentity::from_file(&file)? == expected)
    }

    pub(crate) fn validate_private_file(&self, relative: &Path) -> Result<File, StoreError> {
        let file = self
            .try_open_regular(relative, false)?
            .ok_or(StoreError::InvalidLayout)?;
        validate_private_file_permissions(&file)?;
        Ok(file)
    }

    pub(crate) fn set_private_file(file: &File) -> Result<(), StoreError> {
        set_private_file_permissions(file)
    }

    pub(crate) fn has_single_link(file: &File) -> Result<bool, StoreError> {
        let metadata = cap_std::fs::Metadata::from_file(file)
            .map_err(|source| StoreError::io("inspect opened store file links", source))?;
        Ok(IdentityMetadataExt::nlink(&metadata) == 1)
    }

    pub(crate) fn validate_private_open_file(file: &File) -> Result<(), StoreError> {
        validate_private_file_permissions(file)
    }

    pub(crate) fn validate_private_root(&self) -> Result<(), StoreError> {
        validate_private_directory_handle(&self.root, StoreError::InvalidDataDirectory)
    }

    pub(crate) fn harden_root(&self) -> Result<(), StoreError> {
        set_private_directory_handle(&self.root)
    }

    pub(crate) fn validate_private_directory(&self, relative: &Path) -> Result<(), StoreError> {
        let directory = self.open_directory(relative)?;
        validate_private_directory_handle(&directory, StoreError::InvalidLayout)
    }

    pub(crate) fn try_open_regular_in(
        directory: &Dir,
        name: &OsStr,
    ) -> Result<Option<File>, StoreError> {
        validate_normal_name(name)?;
        try_open_regular_at(directory, name, false)
    }

    pub(crate) fn open_child_directory(directory: &Dir, name: &OsStr) -> Result<Dir, StoreError> {
        validate_normal_name(name)?;
        let metadata = directory
            .symlink_metadata(name)
            .map_err(|source| StoreError::io("inspect child store directory", source))?;
        if metadata.is_symlink() {
            return Err(StoreError::UnsafePath);
        }
        if !metadata.is_dir() {
            return Err(StoreError::InvalidLayout);
        }
        #[cfg(all(test, unix))]
        run_test_hook(TestHookPoint::DirectoryOpen, name);
        directory.open_dir_nofollow(name).map_err(|source| {
            secure_open_error(directory, name, "open child store directory", source)
        })
    }

    pub(crate) fn rename_between(
        from_directory: &Dir,
        from_name: &OsStr,
        to_directory: &Dir,
        to_name: &OsStr,
    ) -> Result<(), StoreError> {
        validate_normal_name(from_name)?;
        validate_normal_name(to_name)?;
        #[cfg(all(test, unix))]
        run_test_hook(TestHookPoint::Rename, from_name);
        from_directory
            .rename(from_name, to_directory, to_name)
            .map_err(|source| StoreError::io("rename store entry", source))
    }

    pub(crate) fn sync_open_directory(
        directory: &Dir,
        operation: &'static str,
    ) -> Result<(), StoreError> {
        sync_directory_handle(directory, operation)
    }

    fn try_open_parent(&self, relative: &Path) -> Result<Option<Dir>, StoreError> {
        if relative.as_os_str().is_empty() {
            return self
                .root
                .try_clone()
                .map(Some)
                .map_err(|source| StoreError::io("clone store root handle", source));
        }
        self.try_open_directory(relative)
    }

    fn open_parent(&self, relative: &Path) -> Result<Dir, StoreError> {
        self.try_open_parent(relative)?
            .ok_or(StoreError::InvalidLayout)
    }
}

pub(crate) fn create_layout(root: &StoreDirectory) -> Result<(), StoreError> {
    for relative in REQUIRED_DIRECTORIES {
        root.create_directory_all(Path::new(relative))?;
        root.validate_private_directory(Path::new(relative))?;
    }
    for relative in REQUIRED_DIRECTORIES.iter().rev() {
        root.sync_directory(Path::new(relative), "sync store layout directory")?;
    }
    root.sync_root("sync store root layout")
}

/// Validate that an uncommitted directory is either empty or contains only
/// artifacts that an interrupted Jiandu initialization could have created.
pub(crate) fn validate_initialization_state(root: &StoreDirectory) -> Result<(), StoreError> {
    let mut pending = vec![PathBuf::new()];
    let mut saw_entry = false;
    let mut saw_ownership_marker = false;

    while let Some(relative_directory) = pending.pop() {
        let directory = root.open_parent(&relative_directory)?;
        let entries = directory
            .entries()
            .map_err(|source| StoreError::io("inspect initialization directory", source))?;
        for entry in entries {
            let entry =
                entry.map_err(|source| StoreError::io("read initialization entry", source))?;
            saw_entry = true;
            let name = entry.file_name();
            validate_normal_name(&name)?;
            let relative = relative_directory.join(&name);
            let metadata = directory
                .symlink_metadata(&name)
                .map_err(|source| StoreError::io("inspect initialization entry", source))?;
            if metadata.is_symlink() {
                return Err(StoreError::UnsafePath);
            }

            if relative == Path::new(STORE_LOCK_FILE)
                || relative == Path::new(STORE_METADATA_INIT_FILE)
            {
                if !metadata.is_file() || IdentityMetadataExt::nlink(&metadata) != 1 {
                    return Err(StoreError::UnsafePath);
                }
                if relative == Path::new(STORE_LOCK_FILE) {
                    saw_ownership_marker = true;
                }
                continue;
            }

            if REQUIRED_DIRECTORIES
                .iter()
                .any(|allowed| relative == Path::new(allowed))
            {
                if !metadata.is_dir() {
                    return Err(StoreError::InvalidDataDirectory);
                }
                let _opened = StoreDirectory::open_child_directory(&directory, &name)?;
                pending.push(relative);
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

pub(crate) fn validate_layout(root: &StoreDirectory) -> Result<(), StoreError> {
    for relative in REQUIRED_DIRECTORIES {
        root.validate_private_directory(Path::new(relative))?;
    }
    Ok(())
}

pub(crate) fn scope_relative_directory(scope: &MemoryScope) -> PathBuf {
    match scope {
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
    }
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

pub(crate) fn record_relative_path(scope: &MemoryScope, id: &MemoryId) -> PathBuf {
    scope_relative_directory(scope)
        .join(record_shard(id))
        .join(record_file_name(id))
}

pub(crate) fn validate_record_entry_name(name: &OsStr) -> Result<String, StoreError> {
    let name = name.to_str().ok_or(StoreError::UnsafePath)?;
    let key = name.strip_suffix(".md").ok_or(StoreError::InvalidLayout)?;
    if !valid_storage_key(key) {
        return Err(StoreError::InvalidLayout);
    }
    Ok(key.to_owned())
}

#[cfg(test)]
pub(crate) fn safe_join(root: &Path, relative: &Path) -> Result<PathBuf, StoreError> {
    validate_relative_path(relative)?;
    Ok(root.join(relative))
}

#[cfg(test)]
pub(crate) fn scope_directory(root: &Path, scope: &MemoryScope) -> Result<PathBuf, StoreError> {
    safe_join(root, &scope_relative_directory(scope))
}

#[cfg(test)]
pub(crate) fn record_path(
    root: &Path,
    scope: &MemoryScope,
    id: &MemoryId,
) -> Result<PathBuf, StoreError> {
    safe_join(root, &record_relative_path(scope, id))
}

fn absolute_data_path(path: &Path) -> Result<PathBuf, StoreError> {
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
    if !absolute.is_absolute() {
        return Err(StoreError::InvalidDataDirectory);
    }
    Ok(absolute)
}

fn walk_ambient_directory(path: &Path, create: bool) -> Result<Dir, StoreError> {
    let (anchor, components) = split_absolute_path(path)?;
    let mut current = Dir::open_ambient_dir(&anchor, ambient_authority())
        .map_err(|source| StoreError::io("open filesystem anchor", source))?;
    let mut trusted_system_chain = trusted_system_directory(&current);

    for (index, component) in components.iter().enumerate() {
        let is_final = index + 1 == components.len();
        let metadata = match current.symlink_metadata(component) {
            Ok(metadata) => Some(metadata),
            Err(source) if source.kind() == io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(StoreError::io("inspect data directory component", source));
            }
        };

        let next = match metadata {
            Some(metadata) if metadata.is_symlink() => {
                if is_final || !trusted_system_chain || !trusted_system_symlink(&metadata) {
                    return Err(StoreError::InvalidDataDirectory);
                }
                let followed = current.open_dir(component).map_err(|source| {
                    StoreError::io("open trusted system directory link", source)
                })?;
                if !trusted_system_directory(&followed) {
                    return Err(StoreError::InvalidDataDirectory);
                }
                followed
            }
            Some(metadata) if metadata.is_dir() => {
                current.open_dir_nofollow(component).map_err(|source| {
                    secure_open_error(&current, component, "open data directory component", source)
                })?
            }
            Some(_) => return Err(StoreError::InvalidDataDirectory),
            None if create => {
                match current.create_dir(component) {
                    Ok(()) => {}
                    Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(source) => {
                        return Err(StoreError::io("create data directory component", source));
                    }
                }
                let opened = current.open_dir_nofollow(component).map_err(|source| {
                    secure_open_error(
                        &current,
                        component,
                        "open created data directory component",
                        source,
                    )
                })?;
                set_private_directory_handle(&opened)?;
                opened
            }
            None => return Err(StoreError::NotInitialized),
        };
        trusted_system_chain = trusted_system_chain && trusted_system_directory(&next);
        current = next;
    }
    Ok(current)
}

fn split_absolute_path(path: &Path) -> Result<(PathBuf, Vec<OsString>), StoreError> {
    let mut anchor = PathBuf::new();
    let mut components = Vec::new();
    let mut saw_normal = false;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir if !saw_normal => {
                anchor.push(component.as_os_str());
            }
            Component::Normal(name) => {
                saw_normal = true;
                components.push(name.to_os_string());
            }
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                return Err(StoreError::InvalidDataDirectory);
            }
        }
    }
    if anchor.as_os_str().is_empty() || components.is_empty() {
        return Err(StoreError::InvalidDataDirectory);
    }
    Ok((anchor, components))
}

fn trusted_system_directory(directory: &Dir) -> bool {
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt as _;
        directory
            .dir_metadata()
            .is_ok_and(|metadata| metadata.uid() == 0 && metadata.mode() & 0o022 == 0)
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
        false
    }
}

fn trusted_system_symlink(metadata: &cap_std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt as _;
        metadata.uid() == 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

fn try_open_regular_at(
    directory: &Dir,
    name: &OsStr,
    write: bool,
) -> Result<Option<File>, StoreError> {
    validate_normal_name(name)?;
    match directory.symlink_metadata(name) {
        Ok(metadata) if metadata.is_symlink() => return Err(StoreError::UnsafePath),
        Ok(metadata) if !metadata.is_file() => return Err(StoreError::InvalidLayout),
        Ok(_) => {}
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(StoreError::io("inspect store file", source)),
    }
    #[cfg(all(test, unix))]
    run_test_hook(TestHookPoint::RegularOpen, name);
    match open_regular_at(directory, name, write, false, false, false) {
        Ok(file) => Ok(Some(file)),
        Err(StoreError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn open_regular_at(
    directory: &Dir,
    name: &OsStr,
    write: bool,
    create: bool,
    create_new: bool,
    deny_delete: bool,
) -> Result<File, StoreError> {
    validate_normal_name(name)?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(write)
        .create(create)
        .create_new(create_new)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    if deny_delete {
        use cap_std::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    }
    #[cfg(not(windows))]
    let _ = deny_delete;
    let file = directory.open_with(name, &options).map_err(|source| {
        secure_open_error(
            directory,
            name,
            "open capability-relative store file",
            source,
        )
    })?;
    let file = file.into_std();
    let metadata = cap_std::fs::Metadata::from_file(&file)
        .map_err(|source| StoreError::io("inspect opened store file", source))?;
    if !metadata.is_file() {
        return Err(StoreError::InvalidLayout);
    }
    if IdentityMetadataExt::nlink(&metadata) != 1 {
        return Err(StoreError::UnsafePath);
    }
    Ok(file)
}

fn secure_open_error(
    directory: &Dir,
    name: &OsStr,
    operation: &'static str,
    source: io::Error,
) -> StoreError {
    if directory
        .symlink_metadata(name)
        .is_ok_and(|metadata| metadata.is_symlink())
    {
        return StoreError::UnsafePath;
    }
    #[cfg(unix)]
    if source.raw_os_error() == Some(libc::ELOOP) {
        return StoreError::UnsafePath;
    }
    StoreError::io(operation, source)
}

fn split_relative_file(relative: &Path) -> Result<(PathBuf, OsString), StoreError> {
    validate_relative_path(relative)?;
    let name = relative
        .file_name()
        .ok_or(StoreError::UnsafePath)?
        .to_os_string();
    validate_normal_name(&name)?;
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    Ok((parent.to_path_buf(), name))
}

fn validate_relative_path(relative: &Path) -> Result<(), StoreError> {
    if relative.is_absolute()
        || relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(StoreError::UnsafePath);
    }
    Ok(())
}

fn validate_normal_name(name: &OsStr) -> Result<(), StoreError> {
    let path = Path::new(name);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err(StoreError::UnsafePath);
    }
    Ok(())
}

fn set_private_file_permissions(file: &File) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|source| StoreError::io("set private store file permissions", source))?;
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

fn validate_private_file_permissions(file: &File) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = file
            .metadata()
            .map_err(|source| StoreError::io("inspect store file permissions", source))?
            .permissions()
            .mode()
            & 0o7777;
        if mode != 0o600 {
            return Err(StoreError::InvalidLayout);
        }
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

fn set_private_directory_handle(directory: &Dir) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        directory
            .try_clone()
            .map(Dir::into_std_file)
            .and_then(|file| file.set_permissions(std::fs::Permissions::from_mode(0o700)))
            .map_err(|source| StoreError::io("set private store directory permissions", source))?;
    }
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

fn validate_private_directory_handle(directory: &Dir, error: StoreError) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use cap_std::fs::PermissionsExt as _;
        let mode = directory
            .dir_metadata()
            .map_err(|source| StoreError::io("inspect store directory permissions", source))?
            .permissions()
            .mode()
            & 0o7777;
        if mode != 0o700 {
            return Err(error);
        }
    }
    #[cfg(not(unix))]
    let _ = (directory, error);
    Ok(())
}

fn sync_directory_handle(directory: &Dir, operation: &'static str) -> Result<(), StoreError> {
    directory
        .try_clone()
        .map(Dir::into_std_file)
        .and_then(|file| file.sync_all())
        .map_err(|source| StoreError::io(operation, source))
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
