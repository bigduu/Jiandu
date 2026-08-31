//! One-shot import of Bamboo's current canonical durable-memory topics.
//!
//! This is deliberately not a generic migration framework. The source is read
//! only, every topic is validated before any destination staging begins, and a
//! fully rebuilt sibling staging directory is renamed into an absent or empty
//! Jiandu data root only after source and staged content identities match.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::fs;

use crate::ProjectId;
use crate::memory_store::{MemoryScope, MemoryStore, parse_markdown_document, validate_memory_id};

const TOPIC_BATCH_SIZE: usize = 128;

/// Successful one-shot Bamboo import summary.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BambooImportReport {
    pub source_data_dir: PathBuf,
    pub destination_data_dir: PathBuf,
    pub scanned: usize,
    pub imported: usize,
    pub failed: usize,
    pub global_topics: usize,
    pub project_topics: usize,
    pub project_scopes: usize,
    pub rebuilt_scopes: usize,
    pub content_identity_sha256: String,
}

#[derive(Debug, Clone)]
struct ImportTopic {
    relative_path: PathBuf,
    bytes: Vec<u8>,
    id: String,
    project_id: Option<ProjectId>,
}

#[derive(Debug)]
struct ValidatedSource {
    topics: Vec<ImportTopic>,
    project_counts: BTreeMap<ProjectId, usize>,
    global_topics: usize,
    content_identity_sha256: String,
}

/// Seed an independent Jiandu data root from Bamboo's current canonical
/// Global and typed-Project durable topics.
///
/// The caller must stop Bamboo memory writes or pass a static snapshot. Jiandu
/// verifies that the selected source topics did not change while staging, but
/// never writes to or locks the Bamboo source.
pub async fn import_bamboo_durable_memory(
    source_data_dir: impl AsRef<Path>,
    destination_data_dir: impl AsRef<Path>,
) -> io::Result<BambooImportReport> {
    let source_data_dir = fs::canonicalize(source_data_dir.as_ref())
        .await
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "failed to resolve Bamboo source '{}': {error}",
                    source_data_dir.as_ref().display()
                ),
            )
        })?;
    if !fs::metadata(&source_data_dir).await?.is_dir() {
        return Err(invalid_input(format!(
            "Bamboo source is not a directory: {}",
            source_data_dir.display()
        )));
    }

    let destination_data_dir = resolve_destination_path(destination_data_dir.as_ref()).await?;
    if source_data_dir == destination_data_dir
        || source_data_dir.starts_with(&destination_data_dir)
        || destination_data_dir.starts_with(&source_data_dir)
    {
        return Err(invalid_input(
            "Bamboo source and Jiandu destination must be distinct, non-nested data directories",
        ));
    }
    let destination_was_empty = destination_is_absent_or_empty(&destination_data_dir).await?;

    // Complete source validation happens before a staging directory exists.
    let source = scan_bamboo_topics(&source_data_dir).await?;

    let destination_parent = destination_data_dir.parent().ok_or_else(|| {
        invalid_input(format!(
            "Jiandu destination has no parent directory: {}",
            destination_data_dir.display()
        ))
    })?;
    let staging = tempfile::Builder::new()
        .prefix(".jiandu-bamboo-import-")
        .tempdir_in(destination_parent)?;

    for batch in source.topics.chunks(TOPIC_BATCH_SIZE) {
        let writes = batch
            .iter()
            .map(|topic| {
                (
                    staging.path().join(&topic.relative_path),
                    topic.bytes.clone(),
                )
            })
            .collect();
        crate::atomic_fs::atomic_write_batch(writes).await?;
    }

    let staging_store = MemoryStore::new(staging.path());
    let mut rebuilt_scopes = 0;
    if source.global_topics > 0 {
        staging_store
            .rebuild_scope(MemoryScope::Global, None)
            .await?;
        rebuilt_scopes += 1;
    }
    for project_id in source.project_counts.keys() {
        staging_store
            .for_project(project_id)
            .rebuild_scope(MemoryScope::Project, Some(project_id.as_str()))
            .await?;
        rebuilt_scopes += 1;
    }

    let staged = scan_bamboo_topics(staging.path()).await?;
    if staged.topics.len() != source.topics.len()
        || staged.content_identity_sha256 != source.content_identity_sha256
    {
        return Err(invalid_data(
            "staged Jiandu topics do not match the validated Bamboo source",
        ));
    }

    // Refuse publication if Bamboo changed while the import was being built.
    let source_after_staging = scan_bamboo_topics(&source_data_dir).await?;
    if source_after_staging.topics.len() != source.topics.len()
        || source_after_staging.content_identity_sha256 != source.content_identity_sha256
    {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "Bamboo source changed during import; stop memory writes or import a static snapshot",
        ));
    }

    if destination_was_empty {
        fs::remove_dir(&destination_data_dir)
            .await
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "Jiandu destination stopped being empty before publication '{}': {error}",
                        destination_data_dir.display()
                    ),
                )
            })?;
    }
    if let Err(error) = fs::rename(staging.path(), &destination_data_dir).await {
        if destination_was_empty {
            let _ = fs::create_dir(&destination_data_dir).await;
        }
        return Err(io::Error::new(
            error.kind(),
            format!(
                "failed to publish staged Jiandu store '{}': {error}",
                destination_data_dir.display()
            ),
        ));
    }

    Ok(BambooImportReport {
        source_data_dir,
        destination_data_dir,
        scanned: source.topics.len(),
        imported: source.topics.len(),
        failed: 0,
        global_topics: source.global_topics,
        project_topics: source.topics.len() - source.global_topics,
        project_scopes: source.project_counts.len(),
        rebuilt_scopes,
        content_identity_sha256: source.content_identity_sha256,
    })
}

async fn resolve_destination_path(path: &Path) -> io::Result<PathBuf> {
    if path.exists() {
        return fs::canonicalize(path).await;
    }
    let file_name = path.file_name().ok_or_else(|| {
        invalid_input(format!(
            "Jiandu destination must name a data directory: {}",
            path.display()
        ))
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok(fs::canonicalize(parent).await?.join(file_name))
}

/// Returns true when an existing empty directory must be replaced at publish.
async fn destination_is_absent_or_empty(path: &Path) -> io::Result<bool> {
    match fs::metadata(path).await {
        Ok(metadata) if metadata.is_dir() => {
            let mut entries = fs::read_dir(path).await?;
            if entries.next_entry().await?.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "Jiandu destination must be absent or empty: {}",
                        path.display()
                    ),
                ));
            }
            Ok(true)
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "Jiandu destination exists and is not an empty directory: {}",
                path.display()
            ),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

async fn scan_bamboo_topics(data_dir: &Path) -> io::Result<ValidatedSource> {
    let mut topics = Vec::new();
    let global_relative = PathBuf::from("memory")
        .join("v1")
        .join("scopes")
        .join("global")
        .join("topics");
    scan_topic_directory(
        data_dir,
        &global_relative,
        MemoryScope::Global,
        None,
        &mut topics,
    )
    .await?;

    let projects_root = data_dir.join("projects");
    if projects_root.exists() {
        let metadata = fs::symlink_metadata(&projects_root).await?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(invalid_data(format!(
                "Bamboo projects root is not a real directory: {}",
                projects_root.display()
            )));
        }
        let mut entries = fs::read_dir(&projects_root).await?;
        let mut projects = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                projects.push(entry);
            }
        }
        projects.sort_by_key(|entry| entry.file_name());
        for entry in projects {
            let topics_dir = entry.path().join("memory").join("v1").join("topics");
            if !topics_dir.exists() {
                continue;
            }
            let project_name = entry.file_name().into_string().map_err(|_| {
                invalid_data(format!(
                    "Bamboo Project directory is not UTF-8: {}",
                    entry.path().display()
                ))
            })?;
            let project_id = ProjectId::parse(project_name).map_err(|error| {
                invalid_data(format!(
                    "invalid Bamboo Project directory '{}': {error}",
                    entry.path().display()
                ))
            })?;
            let relative = PathBuf::from("projects")
                .join(project_id.as_str())
                .join("memory")
                .join("v1")
                .join("topics");
            scan_topic_directory(
                data_dir,
                &relative,
                MemoryScope::Project,
                Some(&project_id),
                &mut topics,
            )
            .await?;
        }
    }

    topics.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    let mut seen_ids = HashSet::new();
    let mut global_topics = 0;
    let mut project_counts = BTreeMap::new();
    for topic in &topics {
        if !seen_ids.insert(topic.id.clone()) {
            return Err(invalid_data(format!(
                "duplicate Bamboo memory id: {}",
                topic.id
            )));
        }
        match &topic.project_id {
            Some(project_id) => *project_counts.entry(project_id.clone()).or_insert(0) += 1,
            None => global_topics += 1,
        }
    }
    let content_identity_sha256 = content_identity(&topics);

    Ok(ValidatedSource {
        topics,
        project_counts,
        global_topics,
        content_identity_sha256,
    })
}

async fn scan_topic_directory(
    data_dir: &Path,
    relative_dir: &Path,
    expected_scope: MemoryScope,
    project_id: Option<&ProjectId>,
    topics: &mut Vec<ImportTopic>,
) -> io::Result<()> {
    let topic_dir = data_dir.join(relative_dir);
    match fs::symlink_metadata(&topic_dir).await {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(invalid_data(format!(
                "Bamboo topic path is not a real directory: {}",
                topic_dir.display()
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    let canonical_data_dir = fs::canonicalize(data_dir).await?;
    let canonical_topic_dir = fs::canonicalize(&topic_dir).await?;
    if !canonical_topic_dir.starts_with(&canonical_data_dir) {
        return Err(invalid_data(format!(
            "Bamboo topic directory escapes the source data root: {}",
            topic_dir.display()
        )));
    }

    let mut entries = fs::read_dir(&topic_dir).await?;
    let mut topic_entries = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "md")
        {
            topic_entries.push(entry);
        }
    }
    topic_entries.sort_by_key(|entry| entry.file_name());

    for entry in topic_entries {
        if !entry.file_type().await?.is_file() {
            return Err(invalid_data(format!(
                "Bamboo topic is not a regular file: {}",
                entry.path().display()
            )));
        }
        let file_name = entry.file_name().into_string().map_err(|_| {
            invalid_data(format!(
                "Bamboo topic filename is not UTF-8: {}",
                entry.path().display()
            ))
        })?;
        let file_id = file_name
            .strip_suffix(".md")
            .ok_or_else(|| invalid_data(format!("invalid Bamboo topic filename: {file_name}")))?;
        let file_id = validate_memory_id(file_id).map_err(|error| {
            invalid_data(format!(
                "invalid Bamboo topic filename '{}': {error}",
                entry.path().display()
            ))
        })?;
        let bytes = fs::read(entry.path()).await?;
        let raw = std::str::from_utf8(&bytes).map_err(|error| {
            invalid_data(format!(
                "Bamboo topic is not UTF-8 '{}': {error}",
                entry.path().display()
            ))
        })?;
        let (frontmatter, _) = parse_markdown_document(raw).map_err(|error| {
            invalid_data(format!(
                "invalid Bamboo topic '{}': {error}",
                entry.path().display()
            ))
        })?;
        if frontmatter.id != file_id {
            return Err(invalid_data(format!(
                "Bamboo topic id '{}' does not match filename '{}': {}",
                frontmatter.id,
                file_id,
                entry.path().display()
            )));
        }
        if frontmatter.scope != expected_scope {
            return Err(invalid_data(format!(
                "Bamboo topic scope '{}' does not match canonical path scope '{}': {}",
                frontmatter.scope.as_str(),
                expected_scope.as_str(),
                entry.path().display()
            )));
        }
        let expected_project_key = project_id.map(ProjectId::as_str);
        if frontmatter.project_key.as_deref() != expected_project_key {
            return Err(invalid_data(format!(
                "Bamboo topic Project identity does not match its canonical path: {}",
                entry.path().display()
            )));
        }

        topics.push(ImportTopic {
            relative_path: relative_dir.join(file_name),
            bytes,
            id: frontmatter.id,
            project_id: project_id.cloned(),
        });
    }
    Ok(())
}

fn content_identity(topics: &[ImportTopic]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"jiandu-bamboo-current-topics\0");
    for topic in topics {
        let path = topic.relative_path.to_string_lossy();
        digest.update((path.len() as u64).to_be_bytes());
        digest.update(path.as_bytes());
        digest.update((topic.bytes.len() as u64).to_be_bytes());
        digest.update(&topic.bytes);
    }
    let mut hex = String::with_capacity(64);
    for byte in digest.finalize() {
        write!(&mut hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    hex
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
