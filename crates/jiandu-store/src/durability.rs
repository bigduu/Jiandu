//! Runtime verification of filesystem assumptions required by transactions.

use crate::failpoint::Failpoints;
use crate::layout::StoreDirectory;
use crate::{PersistenceBoundary, StoreError};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Directory-entry durability available on the current platform.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectoryDurability {
    ExplicitSync,
    PlatformDocumentedBestEffort,
}

/// Path-free result of the startup/doctor durability probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreDoctorReport {
    pub file_sync: bool,
    pub same_filesystem_atomic_replace: bool,
    pub directory_durability: DirectoryDurability,
}

pub(crate) fn probe(
    root: &StoreDirectory,
    failpoints: &Failpoints,
    forced_unsupported: Option<&'static str>,
) -> Result<StoreDoctorReport, StoreError> {
    if let Some(capability) = forced_unsupported {
        return Err(StoreError::UnsupportedDurability { capability });
    }
    run_probe(root, failpoints).map_err(|error| match error {
        StoreError::InjectedFailure { .. } => error,
        StoreError::UnsupportedDurability { .. } => error,
        _ => StoreError::UnsupportedDurability {
            capability: "same-filesystem atomic replace and sync",
        },
    })?;
    Ok(StoreDoctorReport {
        file_sync: true,
        same_filesystem_atomic_replace: true,
        directory_durability: directory_durability(),
    })
}

fn run_probe(root: &StoreDirectory, failpoints: &Failpoints) -> Result<(), StoreError> {
    let token = Uuid::new_v4().hyphenated().to_string();
    let source = probe_path(&token, "source");
    let target = probe_path(&token, "target");
    write_probe_file(root, &source, b"replacement\n")?;
    write_probe_file(root, &target, b"original\n")?;
    root.sync_directory(Path::new("transactions"), "sync durability probe files")?;
    failpoints.check(PersistenceBoundary::DurabilityProbeFilesSynced)?;
    root.rename(&source, &target)?;
    failpoints.check(PersistenceBoundary::DurabilityProbeRenamed)?;
    root.sync_directory(
        Path::new("transactions"),
        "sync durability probe replacement",
    )?;
    failpoints.check(PersistenceBoundary::DurabilityProbeDirectorySynced)?;

    let mut bytes = Vec::new();
    root.open_existing_regular(&target, false)?
        .read_to_end(&mut bytes)
        .map_err(|source| StoreError::io("read durability probe", source))?;
    if bytes != b"replacement\n" || root.regular_file_exists(&source)? {
        return Err(StoreError::UnsupportedDurability {
            capability: "atomic replacement",
        });
    }
    root.remove_regular_file(&target)?;
    root.sync_directory(Path::new("transactions"), "sync durability probe cleanup")
}

fn write_probe_file(
    root: &StoreDirectory,
    relative: &Path,
    bytes: &[u8],
) -> Result<(), StoreError> {
    let mut file = root.create_new_regular(relative)?;
    StoreDirectory::set_private_file(&file)?;
    file.write_all(bytes)
        .map_err(|source| StoreError::io("write durability probe", source))?;
    file.sync_all()
        .map_err(|source| StoreError::io("sync durability probe", source))
}

fn probe_path(token: &str, suffix: &str) -> PathBuf {
    PathBuf::from("transactions").join(format!(".durability-{token}-{suffix}"))
}

const fn directory_durability() -> DirectoryDurability {
    #[cfg(windows)]
    {
        DirectoryDurability::PlatformDocumentedBestEffort
    }
    #[cfg(not(windows))]
    {
        DirectoryDurability::ExplicitSync
    }
}
