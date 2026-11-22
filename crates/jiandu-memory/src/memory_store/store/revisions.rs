//! Immutable before-images of durable topics, written under the existing scope lock.

use std::{
    io,
    path::{Path, PathBuf},
};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::fs;

use super::{
    AuditLogEntry, CreatedBy, DurableMemoryDocument, MemoryScope, MemoryStore, WRITE_AUDIT_LOG,
    now_rfc3339, parse_markdown_document, validate_source_generation,
};

const HISTORY_PAGE_SIZE: usize = 20;

#[derive(Debug, Serialize)]
pub struct MemoryRevisionEntry {
    pub revision_id: String,
    pub updated_at: String,
    pub updated_by: CreatedBy,
}

#[derive(Debug, Serialize)]
pub struct MemoryRevisionHistory {
    pub current_revision: String,
    pub revisions: Vec<MemoryRevisionEntry>,
    pub next_before: Option<String>,
}

fn revision_dir(document: &DurableMemoryDocument) -> io::Result<PathBuf> {
    document
        .path
        .parent()
        .and_then(Path::parent)
        .map(|scope| scope.join("revisions").join(&document.frontmatter.id))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing topic scope directory"))
}

fn digest(raw: &str) -> String {
    format!("{:x}", Sha256::digest(raw.as_bytes()))
}

fn revision_parts(id: &str) -> io::Result<(u64, &str)> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "invalid memory revision ID");
    let (sequence, hash) = id.split_once('-').ok_or_else(invalid)?;
    if sequence.len() != 20 || !sequence.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    Ok((
        sequence.parse().map_err(|_| invalid())?,
        validate_source_generation(hash).map_err(|_| invalid())?,
    ))
}

async fn revision_ids(directory: &Path) -> io::Result<Vec<String>> {
    let mut entries = match fs::read_dir(directory).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut ids = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        if let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".md"))
            && revision_parts(id).is_ok()
            && entry.file_type().await?.is_file()
        {
            ids.push(id.to_owned());
        }
    }
    ids.sort_unstable_by(|left, right| right.cmp(left));
    Ok(ids)
}

pub(super) async fn retain_previous(
    document: &DurableMemoryDocument,
    previous: &str,
) -> io::Result<()> {
    let directory = revision_dir(document)?;
    let ids = revision_ids(&directory).await?;
    let hash = digest(previous);
    let next_sequence = match ids.first() {
        Some(latest) => {
            let (sequence, previous_hash) = revision_parts(latest)?;
            // An interrupted replacement had already saved the current version.
            if previous_hash == hash {
                return Ok(());
            }
            sequence
                .checked_add(1)
                .ok_or_else(|| io::Error::other("memory revision sequence overflow"))?
        }
        None => 1,
    };
    crate::atomic_fs::atomic_write(
        &directory.join(format!("{next_sequence:020}-{hash}.md")),
        previous.as_bytes(),
    )
    .await
}

async fn read_revision(
    document: &DurableMemoryDocument,
    id: &str,
) -> io::Result<DurableMemoryDocument> {
    let (_, hash) = revision_parts(id)?;
    let path = revision_dir(document)?.join(format!("{id}.md"));
    if !fs::symlink_metadata(&path).await?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "memory revision is not a file",
        ));
    }
    let raw = fs::read_to_string(&path).await?;
    let (frontmatter, body) = parse_markdown_document(&raw)?;
    let current = &document.frontmatter;
    if digest(&raw) != hash
        || frontmatter.id != current.id
        || frontmatter.scope != current.scope
        || frontmatter.project_key != current.project_key
        || frontmatter.section_id != current.section_id
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "memory revision does not match its canonical record",
        ));
    }
    Ok(DurableMemoryDocument {
        frontmatter,
        body,
        path,
    })
}

impl MemoryStore {
    /// List older versions in this exact scope, most recent first. Browse older
    /// pages by passing the returned `next_before`.
    pub async fn memory_revision_history(
        &self,
        scope: MemoryScope,
        project_key: Option<&str>,
        id: &str,
        before: Option<&str>,
    ) -> io::Result<Option<MemoryRevisionHistory>> {
        if let Some(before) = before {
            revision_parts(before)?;
        }
        let Some((document, current)) = self.read_memory_with_bytes(scope, project_key, id).await?
        else {
            return Ok(None);
        };
        #[cfg(test)]
        super::pause_after_initial_resolution_for_test(id).await;
        let current_revision = digest(&current);
        let mut ids = revision_ids(&revision_dir(&document)?).await?;
        // The snapshot is published first. Hide its pending entry if the topic
        // has not advanced yet, including after an interrupted replacement.
        if let Some(latest) = ids.first()
            && revision_parts(latest)?.1 == current_revision
        {
            ids.remove(0);
        }
        let mut remaining = ids
            .into_iter()
            .filter(|id| before.is_none_or(|before| id.as_str() < before));
        let mut revisions = Vec::new();
        for id in remaining.by_ref().take(HISTORY_PAGE_SIZE) {
            let previous = read_revision(&document, &id).await?;
            revisions.push(MemoryRevisionEntry {
                revision_id: id,
                updated_at: previous.frontmatter.updated_at,
                updated_by: previous.frontmatter.updated_by,
            });
        }
        let next_before = remaining
            .next()
            .and_then(|_| revisions.last())
            .map(|entry| entry.revision_id.clone());
        if fs::read_to_string(&document.path).await? != current {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "current memory revision changed while listing history; retry the read",
            ));
        }
        Ok(Some(MemoryRevisionHistory {
            current_revision,
            revisions,
            next_before,
        }))
    }

    /// Read one retained version through its current, exact-scope locator.
    pub async fn get_memory_revision(
        &self,
        scope: MemoryScope,
        project_key: Option<&str>,
        id: &str,
        revision_id: &str,
    ) -> io::Result<Option<DurableMemoryDocument>> {
        let Some(document) = self.get_memory_at_locator(scope, project_key, id).await? else {
            return Ok(None);
        };
        read_revision(&document, revision_id).await.map(Some)
    }

    /// Restore selected content as a new revision. The live lifecycle status
    /// and relationships are preserved, and confirmation is never inherited.
    #[allow(clippy::too_many_arguments)]
    pub async fn restore_memory_revision(
        &self,
        scope: MemoryScope,
        project_key: Option<&str>,
        id: &str,
        revision_id: &str,
        expected_current_revision: &str,
        session_id: Option<&str>,
        actor: &str,
    ) -> io::Result<Option<DurableMemoryDocument>> {
        revision_parts(revision_id)?;
        validate_source_generation(expected_current_revision)?;
        let project_key = self.require_project_key(scope, project_key)?;
        let _guard = self.acquire_scope_write_guard(scope, project_key).await?;
        let Some((mut document, raw)) = self.read_memory_with_bytes(scope, project_key, id).await?
        else {
            return Ok(None);
        };
        if digest(&raw) != expected_current_revision {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "current memory revision changed; reread history before restoring",
            ));
        }
        let previous = read_revision(&document, revision_id).await?;
        document.body = previous.body;
        let frontmatter = &mut document.frontmatter;
        frontmatter.title = previous.frontmatter.title;
        frontmatter.tags = previous.frontmatter.tags;
        frontmatter.granularity = previous.frontmatter.granularity;
        frontmatter.retrieval = previous.frontmatter.retrieval;
        frontmatter.sources = previous.frontmatter.sources;
        frontmatter.confidence = None;
        frontmatter.updated_at = now_rfc3339();
        frontmatter.updated_by = CreatedBy {
            kind: "memory_restore".to_owned(),
            id: Some(revision_id.to_owned()),
            actor: Some(actor.to_owned()),
        };
        self.write_document(&document).await?;
        self.append_audit(scope, project_key, WRITE_AUDIT_LOG, AuditLogEntry {
            timestamp: now_rfc3339(), action: "restore_revision".to_owned(), scope,
            memory_id: Some(document.frontmatter.id.clone()), session_id: session_id.map(str::to_owned),
            topic: None, summary: "Restored durable content from a previous revision.".to_owned(),
            metadata: Some(serde_json::json!({"revision_id": revision_id, "replaced_revision": expected_current_revision})),
        }).await?;
        self.refresh_scope_artifacts(scope, project_key).await?;
        Ok(Some(document))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::{DurableMemoryType, ResolveThenLockTestHook, resolve_then_lock_test_hooks};
    use super::*;

    #[tokio::test]
    async fn history_requires_a_consistent_revision_during_an_overlapping_write() {
        let directory = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(directory.path());
        let scope = MemoryScope::Global;
        let target = store
            .write_memory(
                scope,
                None,
                DurableMemoryType::Reference,
                "Revision history concurrent edit fixture",
                "Original version.",
                &[],
                None,
                "fixture",
                false,
                None,
            )
            .await
            .unwrap();
        let id = target.frontmatter.id;
        let original = fs::read_to_string(target.path).await.unwrap();
        let hook = Arc::new(ResolveThenLockTestHook {
            resolved: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
        });
        resolve_then_lock_test_hooks().insert(id.clone(), hook.clone());
        let read = {
            let store = store.clone();
            let id = id.clone();
            tokio::spawn(async move { store.memory_revision_history(scope, None, &id, None).await })
        };
        hook.resolved.notified().await;
        resolve_then_lock_test_hooks().remove(&id);
        MemoryStore::new(directory.path())
            .merge_memory(&id, None, "Concurrent version.", &[], None, "other", &[])
            .await
            .unwrap()
            .unwrap();
        hook.resume.notify_one();
        let error = read.await.unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        let history = store
            .memory_revision_history(scope, None, &id, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(history.revisions.len(), 1);
        assert_ne!(history.current_revision, digest(&original));
        assert_eq!(
            revision_parts(&history.revisions[0].revision_id).unwrap().1,
            digest(&original)
        );
    }
}
