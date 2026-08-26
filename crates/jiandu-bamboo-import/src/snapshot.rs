use crate::{
    BambooImportError, MAX_SOURCE_BYTES, MAX_SOURCE_DEPTH, MAX_SOURCE_ENTRIES, MAX_SOURCE_FILES,
    portable_relative_path, sha256_hex,
};
use cap_fs_ext::{
    DirExt as _, FollowSymlinks, MetadataExt as IdentityMetadataExt, OpenOptionsFollowExt as _,
};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScannedEntry {
    pub(crate) relative_path: String,
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
}

pub(crate) struct SnapshotScan {
    pub(crate) entries: Vec<ScannedEntry>,
    pub(crate) directories: Vec<String>,
    pub(crate) files: BTreeMap<String, Vec<u8>>,
}

struct ScanBudget {
    entries: usize,
    total_bytes: usize,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    fn from_metadata(metadata: &cap_std::fs::Metadata) -> Self {
        Self {
            device: IdentityMetadataExt::dev(metadata),
            inode: IdentityMetadataExt::ino(metadata),
        }
    }

    fn from_file(file: &cap_std::fs::File) -> Result<Self, BambooImportError> {
        let metadata = file
            .metadata()
            .map_err(|_| BambooImportError::UnsafeSnapshot)?;
        Ok(Self::from_metadata(&metadata))
    }
}

pub(crate) struct ReadOnlySnapshotRoot {
    root: Dir,
    ambient_path: PathBuf,
    identity: FileIdentity,
}

impl ReadOnlySnapshotRoot {
    pub(crate) fn open(path: &Path) -> Result<Self, BambooImportError> {
        let metadata =
            std::fs::symlink_metadata(path).map_err(|_| BambooImportError::InvalidSnapshot)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(BambooImportError::UnsafeSnapshot);
        }
        let root = Dir::open_ambient_dir(path, ambient_authority())
            .map_err(|_| BambooImportError::InvalidSnapshot)?;
        let opened = root
            .dir_metadata()
            .map_err(|_| BambooImportError::UnsafeSnapshot)?;
        if opened.is_symlink() || !opened.is_dir() {
            return Err(BambooImportError::UnsafeSnapshot);
        }
        let identity = FileIdentity::from_metadata(&opened);
        Ok(Self {
            root,
            ambient_path: path.to_path_buf(),
            identity,
        })
    }

    pub(crate) fn recheck(&self) -> Result<(), BambooImportError> {
        let metadata = std::fs::symlink_metadata(&self.ambient_path)
            .map_err(|_| BambooImportError::SourceDrift)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(BambooImportError::SourceDrift);
        }
        let reopened = Dir::open_ambient_dir(&self.ambient_path, ambient_authority())
            .map_err(|_| BambooImportError::SourceDrift)?;
        let reopened_metadata = reopened
            .dir_metadata()
            .map_err(|_| BambooImportError::SourceDrift)?;
        let current = self
            .root
            .dir_metadata()
            .map_err(|_| BambooImportError::SourceDrift)?;
        if FileIdentity::from_metadata(&reopened_metadata) != self.identity
            || FileIdentity::from_metadata(&current) != self.identity
        {
            return Err(BambooImportError::SourceDrift);
        }
        Ok(())
    }

    pub(crate) fn read_regular(
        &self,
        relative: &str,
        maximum: usize,
    ) -> Result<Vec<u8>, BambooImportError> {
        if !portable_relative_path(relative) {
            return Err(BambooImportError::InvalidSnapshot);
        }
        let path = Path::new(relative);
        let mut directory = self
            .root
            .try_clone()
            .map_err(|_| BambooImportError::UnsafeSnapshot)?;
        let components = path.components().collect::<Vec<_>>();
        for component in &components[..components.len().saturating_sub(1)] {
            let Component::Normal(name) = component else {
                return Err(BambooImportError::InvalidSnapshot);
            };
            directory = directory
                .open_dir_nofollow(name)
                .map_err(|_| BambooImportError::UnsafeSnapshot)?;
        }
        let Some(Component::Normal(name)) = components.last() else {
            return Err(BambooImportError::InvalidSnapshot);
        };
        read_regular_in(&directory, name, maximum)
    }

    pub(crate) fn scan_source(&self) -> Result<SnapshotScan, BambooImportError> {
        self.recheck()?;
        let source = self
            .root
            .open_dir_nofollow("source")
            .map_err(|_| BambooImportError::UnsafeSnapshot)?;
        let mut entries = Vec::new();
        let mut directories = Vec::new();
        let mut files = BTreeMap::new();
        let mut budget = ScanBudget {
            entries: 0,
            total_bytes: 0,
        };
        scan_directory(
            &source,
            "",
            0,
            &mut entries,
            &mut directories,
            &mut files,
            &mut budget,
        )?;
        entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        directories.sort();
        if entries.len() > MAX_SOURCE_FILES
            || budget.entries > MAX_SOURCE_ENTRIES
            || budget.total_bytes > MAX_SOURCE_BYTES
        {
            return Err(BambooImportError::InvalidSnapshot);
        }
        self.recheck()?;
        Ok(SnapshotScan {
            entries,
            directories,
            files,
        })
    }
}

fn scan_directory(
    directory: &Dir,
    prefix: &str,
    depth: usize,
    entries: &mut Vec<ScannedEntry>,
    directories: &mut Vec<String>,
    files: &mut BTreeMap<String, Vec<u8>>,
    budget: &mut ScanBudget,
) -> Result<(), BambooImportError> {
    if depth > MAX_SOURCE_DEPTH {
        return Err(BambooImportError::InvalidSnapshot);
    }
    let identity = FileIdentity::from_metadata(
        &directory
            .dir_metadata()
            .map_err(|_| BambooImportError::UnsafeSnapshot)?,
    );
    let remaining_entries = MAX_SOURCE_ENTRIES
        .checked_sub(budget.entries)
        .ok_or(BambooImportError::InvalidSnapshot)?;
    let before = read_sorted_names(directory, remaining_entries)?;
    budget.entries = budget
        .entries
        .checked_add(before.len())
        .ok_or(BambooImportError::InvalidSnapshot)?;
    for name in &before {
        let metadata = directory
            .symlink_metadata(name)
            .map_err(|_| BambooImportError::UnsafeSnapshot)?;
        if metadata.is_symlink() {
            return Err(BambooImportError::UnsafeSnapshot);
        }
        let name_text = name.to_str().ok_or(BambooImportError::UnsafeSnapshot)?;
        if name_text.is_empty() || matches!(name_text, "." | "..") {
            return Err(BambooImportError::UnsafeSnapshot);
        }
        let relative = if prefix.is_empty() {
            name_text.to_owned()
        } else {
            format!("{prefix}/{name_text}")
        };
        if metadata.is_dir() {
            let child = directory
                .open_dir_nofollow(name)
                .map_err(|_| BambooImportError::UnsafeSnapshot)?;
            directories.push(relative.clone());
            scan_directory(
                &child,
                &relative,
                depth
                    .checked_add(1)
                    .ok_or(BambooImportError::InvalidSnapshot)?,
                entries,
                directories,
                files,
                budget,
            )?;
        } else if metadata.is_file() {
            let bytes = read_regular_in(directory, name, MAX_SOURCE_BYTES)?;
            budget.total_bytes = budget
                .total_bytes
                .checked_add(bytes.len())
                .ok_or(BambooImportError::InvalidSnapshot)?;
            let entry = ScannedEntry {
                relative_path: relative.clone(),
                bytes: u64::try_from(bytes.len())
                    .map_err(|_| BambooImportError::InvalidSnapshot)?,
                sha256: sha256_hex(&bytes),
            };
            if files.insert(relative, bytes).is_some() {
                return Err(BambooImportError::UnsafeSnapshot);
            }
            entries.push(entry);
        } else {
            return Err(BambooImportError::UnsafeSnapshot);
        }
        if entries.len() > MAX_SOURCE_FILES
            || budget.entries > MAX_SOURCE_ENTRIES
            || budget.total_bytes > MAX_SOURCE_BYTES
        {
            return Err(BambooImportError::InvalidSnapshot);
        }
    }
    let after = read_sorted_names(directory, before.len())?;
    let after_identity = FileIdentity::from_metadata(
        &directory
            .dir_metadata()
            .map_err(|_| BambooImportError::SourceDrift)?,
    );
    if before != after || identity != after_identity {
        return Err(BambooImportError::SourceDrift);
    }
    Ok(())
}

fn read_sorted_names(
    directory: &Dir,
    maximum: usize,
) -> Result<Vec<std::ffi::OsString>, BambooImportError> {
    let mut names = Vec::new();
    for entry in directory
        .read_dir(".")
        .map_err(|_| BambooImportError::UnsafeSnapshot)?
    {
        if names.len() >= maximum {
            return Err(BambooImportError::InvalidSnapshot);
        }
        names.push(
            entry
                .map_err(|_| BambooImportError::UnsafeSnapshot)?
                .file_name(),
        );
    }
    names.sort();
    if names.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(BambooImportError::UnsafeSnapshot);
    }
    Ok(names)
}

fn read_regular_in(
    directory: &Dir,
    name: &std::ffi::OsStr,
    maximum: usize,
) -> Result<Vec<u8>, BambooImportError> {
    let before = directory
        .symlink_metadata(name)
        .map_err(|_| BambooImportError::UnsafeSnapshot)?;
    if before.is_symlink()
        || !before.is_file()
        || IdentityMetadataExt::nlink(&before) != 1
        || before.len() > maximum as u64
    {
        return Err(BambooImportError::UnsafeSnapshot);
    }
    let before_identity = FileIdentity::from_metadata(&before);
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let mut file = directory
        .open_with(name, &options)
        .map_err(|_| BambooImportError::UnsafeSnapshot)?;
    if FileIdentity::from_file(&file)? != before_identity {
        return Err(BambooImportError::SourceDrift);
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(before.len()).map_err(|_| BambooImportError::InvalidSnapshot)?,
    );
    file.by_ref()
        .take((maximum as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| BambooImportError::UnsafeSnapshot)?;
    if bytes.len() > maximum {
        return Err(BambooImportError::InvalidSnapshot);
    }
    let after = directory
        .symlink_metadata(name)
        .map_err(|_| BambooImportError::SourceDrift)?;
    if after.is_symlink()
        || !after.is_file()
        || IdentityMetadataExt::nlink(&after) != 1
        || FileIdentity::from_metadata(&after) != before_identity
        || FileIdentity::from_file(&file)? != before_identity
        || after.len() != before.len()
    {
        return Err(BambooImportError::SourceDrift);
    }
    Ok(bytes)
}
