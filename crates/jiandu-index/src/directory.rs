//! Capability-relative access to the private derived-index directory.

use crate::error::{IndexDegradedReason, IndexError};
use crate::format::MAX_INDEX_FILE_BYTES;
use cap_fs_ext::{
    DirExt as _, FollowSymlinks, MetadataExt as IdentityMetadataExt, OpenOptionsFollowExt as _,
};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Write as _};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

pub(crate) const INDEX_FILE_NAME: &str = "lexical.sqlite";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DirectoryOpenError {
    Missing,
    Unsafe,
    Io,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    fn from_cap_metadata(metadata: &cap_std::fs::Metadata) -> Self {
        Self {
            device: IdentityMetadataExt::dev(metadata),
            inode: IdentityMetadataExt::ino(metadata),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReplacementTarget {
    Missing,
    Existing(FileIdentity),
}

impl ReplacementTarget {
    pub(crate) const fn existed(self) -> bool {
        matches!(self, Self::Existing(_))
    }
}

pub(crate) struct OpenedIndexFile {
    pub(crate) file: File,
    pub(crate) length: u64,
    identity: FileIdentity,
}

/// An opened directory capability. The ambient pathname is retained only to
/// detect namespace replacement; all index I/O is relative to `handle`.
pub(crate) struct IndexDirectory {
    handle: Dir,
    ambient_path: PathBuf,
    identity: FileIdentity,
}

impl IndexDirectory {
    pub(crate) fn open(path: &Path) -> Result<Self, DirectoryOpenError> {
        let ambient_path = absolute_directory_path(path)?;
        let handle = walk_ambient_directory(&ambient_path)?;
        validate_private_directory(&handle)?;
        let metadata = handle.dir_metadata().map_err(|_| DirectoryOpenError::Io)?;
        let identity = FileIdentity::from_cap_metadata(&metadata);
        Ok(Self {
            handle,
            ambient_path,
            identity,
        })
    }

    pub(crate) fn validate_ambient_identity(&self) -> Result<(), DirectoryOpenError> {
        let current = walk_ambient_directory(&self.ambient_path)?;
        validate_private_directory(&current)?;
        let metadata = current.dir_metadata().map_err(|_| DirectoryOpenError::Io)?;
        if FileIdentity::from_cap_metadata(&metadata) != self.identity {
            return Err(DirectoryOpenError::Unsafe);
        }
        Ok(())
    }

    pub(crate) fn open_image(&self) -> Result<OpenedIndexFile, IndexDegradedReason> {
        match open_regular_at(
            &self.handle,
            OsStr::new(INDEX_FILE_NAME),
            MAX_INDEX_FILE_BYTES,
        ) {
            Ok(Some((file, identity, length))) => Ok(OpenedIndexFile {
                file,
                length,
                identity,
            }),
            Ok(None) => Err(IndexDegradedReason::Missing),
            Err(_) => Err(IndexDegradedReason::Corrupt),
        }
    }

    pub(crate) fn revalidate_open_image(
        &self,
        opened: &OpenedIndexFile,
    ) -> Result<(), IndexDegradedReason> {
        let metadata = validate_open_regular(&opened.file, MAX_INDEX_FILE_BYTES)
            .map_err(|_| IndexDegradedReason::Corrupt)?;
        if FileIdentity::from_cap_metadata(&metadata) != opened.identity
            || metadata.len() != opened.length
        {
            return Err(IndexDegradedReason::Corrupt);
        }
        let Some((current, identity, length)) = open_regular_at(
            &self.handle,
            OsStr::new(INDEX_FILE_NAME),
            MAX_INDEX_FILE_BYTES,
        )
        .map_err(|_| IndexDegradedReason::Corrupt)?
        else {
            return Err(IndexDegradedReason::Corrupt);
        };
        drop(current);
        if identity != opened.identity || length != opened.length {
            return Err(IndexDegradedReason::Corrupt);
        }
        Ok(())
    }

    pub(crate) fn replacement_target(&self) -> Result<ReplacementTarget, IndexError> {
        self.validate_publish_namespace()?;
        match open_regular_at(
            &self.handle,
            OsStr::new(INDEX_FILE_NAME),
            MAX_INDEX_FILE_BYTES,
        ) {
            Ok(Some((file, identity, _))) => {
                drop(file);
                Ok(ReplacementTarget::Existing(identity))
            }
            Ok(None) => Ok(ReplacementTarget::Missing),
            Err(DirectoryOpenError::Unsafe) => Err(IndexError::Degraded {
                reason: IndexDegradedReason::Corrupt,
            }),
            Err(DirectoryOpenError::Missing) => Err(IndexError::Degraded {
                reason: IndexDegradedReason::Missing,
            }),
            Err(DirectoryOpenError::Io) => Err(IndexError::io("inspect index target")),
        }
    }

    pub(crate) fn publish(
        &self,
        source: &Path,
        expected_target: ReplacementTarget,
    ) -> Result<(), IndexError> {
        self.validate_publish_namespace()?;
        let source_length = source
            .metadata()
            .map_err(|_| IndexError::io("inspect rebuilt index"))?
            .len();
        if source_length > MAX_INDEX_FILE_BYTES {
            return Err(IndexError::InvalidRequest);
        }
        let temporary_name = format!(".lexical-{}.tmp", Uuid::new_v4().simple());
        let result = (|| {
            let mut input = File::open(source)
                .map_err(|_| IndexError::io("open rebuilt index for publication"))?;
            let mut options = OpenOptions::new();
            options
                .read(true)
                .write(true)
                .create_new(true)
                .follow(FollowSymlinks::No);
            #[cfg(unix)]
            {
                use cap_std::fs::OpenOptionsExt as _;
                options.mode(0o600).custom_flags(libc::O_NONBLOCK);
            }
            let mut output = self
                .handle
                .open_with(&temporary_name, &options)
                .map_err(|_| IndexError::io("create capability-relative index temporary"))?
                .into_std();
            set_private_file(&output)?;
            let copied = io::copy(&mut input, &mut output)
                .map_err(|_| IndexError::io("copy rebuilt index for publication"))?;
            if copied != source_length {
                return Err(IndexError::io("copy complete rebuilt index"));
            }
            output
                .flush()
                .and_then(|()| output.sync_all())
                .map_err(|_| IndexError::io("sync capability-relative index temporary"))?;
            let temporary_metadata = validate_open_regular(&output, MAX_INDEX_FILE_BYTES)
                .map_err(|_| IndexError::io("validate index temporary"))?;
            let temporary_identity = FileIdentity::from_cap_metadata(&temporary_metadata);
            drop(output);

            #[cfg(test)]
            run_test_hook(TestHookPoint::BeforePublish);

            self.validate_publish_namespace()?;
            if self.replacement_target()? != expected_target {
                return Err(IndexError::InvalidRequest);
            }
            let Some((temporary, reopened_identity, reopened_length)) = open_regular_at(
                &self.handle,
                OsStr::new(&temporary_name),
                MAX_INDEX_FILE_BYTES,
            )
            .map_err(|_| IndexError::io("reopen index temporary"))?
            else {
                return Err(IndexError::io("reopen index temporary"));
            };
            drop(temporary);
            if reopened_identity != temporary_identity || reopened_length != source_length {
                return Err(IndexError::InvalidRequest);
            }

            self.handle
                .rename(&temporary_name, &self.handle, INDEX_FILE_NAME)
                .map_err(|_| IndexError::io("publish rebuilt index"))?;
            let Some((published, published_identity, published_length)) = open_regular_at(
                &self.handle,
                OsStr::new(INDEX_FILE_NAME),
                MAX_INDEX_FILE_BYTES,
            )
            .map_err(|_| IndexError::io("validate published index"))?
            else {
                return Err(IndexError::io("validate published index"));
            };
            drop(published);
            if published_identity != temporary_identity || published_length != source_length {
                return Err(IndexError::InvalidRequest);
            }
            sync_directory_handle(&self.handle)?;
            self.validate_ambient_identity()
                .map_err(|_| IndexError::InvalidRequest)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = self.handle.remove_file(&temporary_name);
        }
        result
    }

    fn validate_publish_namespace(&self) -> Result<(), IndexError> {
        self.validate_ambient_identity()
            .map_err(|error| match error {
                DirectoryOpenError::Missing | DirectoryOpenError::Unsafe => {
                    IndexError::InvalidRequest
                }
                DirectoryOpenError::Io => IndexError::io("validate private index directory"),
            })
    }
}

fn open_regular_at(
    directory: &Dir,
    name: &OsStr,
    maximum_length: u64,
) -> Result<Option<(File, FileIdentity, u64)>, DirectoryOpenError> {
    let observed = match directory.symlink_metadata(name) {
        Ok(metadata) if metadata.is_symlink() || !metadata.is_file() => {
            return Err(DirectoryOpenError::Unsafe);
        }
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(DirectoryOpenError::Io),
    };
    let observed_identity = FileIdentity::from_cap_metadata(&observed);
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = directory
        .open_with(name, &options)
        .map_err(|error| secure_open_error(directory, name, error))?
        .into_std();
    let metadata = validate_open_regular(&file, maximum_length)?;
    let identity = FileIdentity::from_cap_metadata(&metadata);
    if identity != observed_identity {
        return Err(DirectoryOpenError::Unsafe);
    }
    let length = metadata.len();
    Ok(Some((file, identity, length)))
}

fn validate_open_regular(
    file: &File,
    maximum_length: u64,
) -> Result<cap_std::fs::Metadata, DirectoryOpenError> {
    let metadata = cap_std::fs::Metadata::from_file(file).map_err(|_| DirectoryOpenError::Io)?;
    if !metadata.is_file()
        || IdentityMetadataExt::nlink(&metadata) != 1
        || metadata.len() > maximum_length
    {
        return Err(DirectoryOpenError::Unsafe);
    }
    #[cfg(unix)]
    {
        use cap_std::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(DirectoryOpenError::Unsafe);
        }
    }
    Ok(metadata)
}

fn absolute_directory_path(path: &Path) -> Result<PathBuf, DirectoryOpenError> {
    if path.as_os_str().is_empty()
        || path.components().any(|component| {
            matches!(component, Component::CurDir | Component::ParentDir)
                || matches!(component, Component::Normal(name) if name == OsStr::new(".") || name == OsStr::new(".."))
        })
    {
        return Err(DirectoryOpenError::Unsafe);
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| DirectoryOpenError::Io)?
            .join(path)
    };
    absolute
        .is_absolute()
        .then_some(absolute)
        .ok_or(DirectoryOpenError::Unsafe)
}

fn walk_ambient_directory(path: &Path) -> Result<Dir, DirectoryOpenError> {
    let (anchor, components) = split_absolute_path(path)?;
    let mut current =
        Dir::open_ambient_dir(&anchor, ambient_authority()).map_err(|_| DirectoryOpenError::Io)?;
    let mut trusted_system_chain = trusted_system_directory(&current);

    for (index, component) in components.iter().enumerate() {
        let is_final = index + 1 == components.len();
        let metadata = match current.symlink_metadata(component) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(DirectoryOpenError::Missing);
            }
            Err(_) => return Err(DirectoryOpenError::Io),
        };
        let next = if metadata.is_symlink() {
            if is_final || !trusted_system_chain || !trusted_system_symlink(&metadata) {
                return Err(DirectoryOpenError::Unsafe);
            }
            let followed = current
                .open_dir(component)
                .map_err(|_| DirectoryOpenError::Io)?;
            if !trusted_system_directory(&followed) {
                return Err(DirectoryOpenError::Unsafe);
            }
            followed
        } else if metadata.is_dir() {
            let expected = FileIdentity::from_cap_metadata(&metadata);
            let opened = current
                .open_dir_nofollow(component)
                .map_err(|error| secure_open_error(&current, component, error))?;
            let opened_metadata = opened.dir_metadata().map_err(|_| DirectoryOpenError::Io)?;
            if FileIdentity::from_cap_metadata(&opened_metadata) != expected {
                return Err(DirectoryOpenError::Unsafe);
            }
            opened
        } else {
            return Err(DirectoryOpenError::Unsafe);
        };
        trusted_system_chain = trusted_system_chain && trusted_system_directory(&next);
        current = next;
    }
    Ok(current)
}

fn split_absolute_path(path: &Path) -> Result<(PathBuf, Vec<OsString>), DirectoryOpenError> {
    let mut anchor = PathBuf::new();
    let mut components = Vec::new();
    let mut saw_normal = false;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir if !saw_normal => {
                anchor.push(component.as_os_str());
            }
            Component::Normal(name) if name != OsStr::new(".") && name != OsStr::new("..") => {
                saw_normal = true;
                components.push(name.to_os_string());
            }
            Component::Normal(_)
            | Component::CurDir
            | Component::ParentDir
            | Component::Prefix(_)
            | Component::RootDir => return Err(DirectoryOpenError::Unsafe),
        }
    }
    if anchor.as_os_str().is_empty() || components.is_empty() {
        return Err(DirectoryOpenError::Unsafe);
    }
    Ok((anchor, components))
}

fn secure_open_error(directory: &Dir, name: &OsStr, _error: io::Error) -> DirectoryOpenError {
    if directory
        .symlink_metadata(name)
        .is_ok_and(|metadata| metadata.is_symlink())
    {
        DirectoryOpenError::Unsafe
    } else {
        DirectoryOpenError::Io
    }
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

fn validate_private_directory(directory: &Dir) -> Result<(), DirectoryOpenError> {
    let metadata = directory
        .dir_metadata()
        .map_err(|_| DirectoryOpenError::Io)?;
    if !metadata.is_dir() {
        return Err(DirectoryOpenError::Unsafe);
    }
    #[cfg(unix)]
    {
        use cap_std::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(DirectoryOpenError::Unsafe);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_private_file(file: &File) -> Result<(), IndexError> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|_| IndexError::io("set index file permissions"))
}

#[cfg(not(unix))]
fn set_private_file(_file: &File) -> Result<(), IndexError> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory_handle(directory: &Dir) -> Result<(), IndexError> {
    use rustix::fs::{Mode, OFlags, openat};

    openat(
        directory,
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(io::Error::from)
    .and_then(|file| file.sync_all())
    .map_err(|_| IndexError::io("sync index directory"))
}

#[cfg(not(unix))]
fn sync_directory_handle(_directory: &Dir) -> Result<(), IndexError> {
    Ok(())
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TestHookPoint {
    AfterDirectoryOpen,
    BeforePublish,
}

#[cfg(test)]
type TestHook = (TestHookPoint, Box<dyn FnOnce()>);

#[cfg(test)]
thread_local! {
    static TEST_HOOK: std::cell::RefCell<Option<TestHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn install_test_hook(point: TestHookPoint, action: impl FnOnce() + 'static) {
    TEST_HOOK.with(|slot| {
        let previous = slot.replace(Some((point, Box::new(action))));
        assert!(
            previous.is_none(),
            "an index filesystem hook is already installed"
        );
    });
}

#[cfg(test)]
pub(crate) fn run_test_hook(point: TestHookPoint) {
    let action = TEST_HOOK.with(|slot| {
        let mut hook = slot.borrow_mut();
        if hook.as_ref().is_some_and(|hook| hook.0 == point) {
            hook.take().map(|hook| hook.1)
        } else {
            None
        }
    });
    if let Some(action) = action {
        action();
    }
}
