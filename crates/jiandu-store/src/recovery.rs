//! Deterministic startup reconciliation for interrupted store transactions.

use crate::failpoint::Failpoints;
use crate::layout::{self, StoreDirectory};
use crate::transaction::{self, TransactionIntent, TransactionManifest};
use crate::{PersistenceBoundary, QuarantineReceipt, StoreError, StoreId, StoreMetadata};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub(crate) struct RecoveryOutcome {
    pub(crate) metadata: StoreMetadata,
    pub(crate) quarantine_receipts: Vec<QuarantineReceipt>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordState {
    Base,
    Target,
    Ambiguous,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MetadataState {
    Base,
    Target,
    Ambiguous,
}

pub(crate) fn recover(
    root: &StoreDirectory,
    metadata: StoreMetadata,
    failpoints: &Failpoints,
) -> Result<RecoveryOutcome, StoreError> {
    cleanup_interrupted_manifest_publish(root)?;
    let manifest = read_single_active_manifest(root, &metadata.store_id)?;
    let metadata = match manifest.as_ref().map(|manifest| &manifest.intent) {
        Some(TransactionIntent::Record(_)) => recover_record(
            root,
            metadata,
            manifest.as_ref().expect("manifest"),
            failpoints,
        )?,
        Some(TransactionIntent::Quarantine(_)) => {
            recover_quarantine(root, manifest.as_ref().expect("manifest"), failpoints)?;
            metadata
        }
        None => metadata,
    };
    reject_orphan_metadata_temp(root, None)?;
    reject_orphan_record_temps(root)?;
    let quarantine_receipts = read_quarantine_receipts(root, &metadata.store_id)?;
    Ok(RecoveryOutcome {
        metadata,
        quarantine_receipts,
    })
}

fn cleanup_interrupted_manifest_publish(root: &StoreDirectory) -> Result<(), StoreError> {
    let transactions = root.open_directory(Path::new("transactions"))?;
    let mut staged = Vec::new();
    let entries = transactions
        .entries()
        .map_err(|source| StoreError::io("list transaction directory", source))?;
    for entry in entries {
        let entry = entry.map_err(|source| StoreError::io("read transaction entry", source))?;
        let name = entry.file_name();
        if transaction::transaction_id_from_manifest_name(&name).is_some() {
            continue;
        }
        if transaction::transaction_id_from_manifest_temp_name(&name).is_some() {
            let file = StoreDirectory::try_open_regular_in(&transactions, &name)?
                .ok_or(StoreError::InvalidTransaction)?;
            StoreDirectory::validate_private_open_file(&file)?;
            staged.push(PathBuf::from("transactions").join(&name));
            continue;
        }
        if durability_probe_name(&name) {
            let file = StoreDirectory::try_open_regular_in(&transactions, &name)?
                .ok_or(StoreError::InvalidTransaction)?;
            StoreDirectory::validate_private_open_file(&file)?;
            staged.push(PathBuf::from("transactions").join(&name));
            continue;
        }
        return Err(StoreError::InvalidTransaction);
    }
    // A canonical manifest is published only after its staging name has been
    // renamed away. Multiple staging intents therefore contradict the single
    // owner protocol and are not guessed away.
    let manifest_stage_count = staged
        .iter()
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                transaction::transaction_id_from_manifest_temp_name(name).is_some()
            })
        })
        .count();
    if manifest_stage_count > 1 {
        return Err(StoreError::InvalidTransaction);
    }
    let removed_any = !staged.is_empty();
    for path in staged {
        root.remove_regular_file(&path)?;
    }
    if removed_any {
        root.sync_directory(
            Path::new("transactions"),
            "sync interrupted manifest rollback",
        )?;
    }
    Ok(())
}

fn read_single_active_manifest(
    root: &StoreDirectory,
    store_id: &StoreId,
) -> Result<Option<TransactionManifest>, StoreError> {
    let transactions = root.open_directory(Path::new("transactions"))?;
    let mut active = Vec::new();
    let entries = transactions
        .entries()
        .map_err(|source| StoreError::io("list active transactions", source))?;
    for entry in entries {
        let entry = entry.map_err(|source| StoreError::io("read active transaction", source))?;
        let name = entry.file_name();
        let Some(transaction_id) = transaction::transaction_id_from_manifest_name(&name) else {
            return Err(StoreError::InvalidTransaction);
        };
        let file = StoreDirectory::try_open_regular_in(&transactions, &name)?
            .ok_or(StoreError::InvalidTransaction)?;
        StoreDirectory::validate_private_open_file(&file)?;
        active.push(TransactionManifest::decode(
            file,
            &transaction_id,
            store_id,
        )?);
    }
    if active.len() > 1 {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(active.pop())
}

fn recover_record(
    root: &StoreDirectory,
    metadata: StoreMetadata,
    manifest: &TransactionManifest,
    failpoints: &Failpoints,
) -> Result<StoreMetadata, StoreError> {
    let TransactionIntent::Record(record) = &manifest.intent else {
        return Err(StoreError::InvalidTransaction);
    };
    let record_state = classify_record(root, record)?;
    let metadata_state = if metadata == record.base_store_metadata {
        MetadataState::Base
    } else if metadata == record.target_store_metadata {
        MetadataState::Target
    } else {
        MetadataState::Ambiguous
    };
    match (record_state, metadata_state) {
        (RecordState::Base, MetadataState::Base) => {
            cleanup_record_temps(root, manifest, failpoints)?;
            cleanup_manifest(root, manifest, failpoints)?;
            Ok(record.base_store_metadata.clone())
        }
        (RecordState::Target, MetadataState::Base) => {
            recover_target_metadata(root, manifest, failpoints)?;
            cleanup_record_temps(root, manifest, failpoints)?;
            cleanup_manifest(root, manifest, failpoints)?;
            Ok(record.target_store_metadata.clone())
        }
        (RecordState::Target, MetadataState::Target) => {
            cleanup_record_temps(root, manifest, failpoints)?;
            cleanup_manifest(root, manifest, failpoints)?;
            Ok(record.target_store_metadata.clone())
        }
        (RecordState::Base, MetadataState::Target)
        | (RecordState::Ambiguous, _)
        | (_, MetadataState::Ambiguous) => Err(StoreError::InvalidTransaction),
    }
}

fn classify_record(
    root: &StoreDirectory,
    record: &transaction::RecordTransaction,
) -> Result<RecordState, StoreError> {
    let relative = layout::record_relative_path(&record.scope, &record.memory_id);
    let Some(file) = root.try_open_regular(&relative, false)? else {
        return Ok(if record.base_revision.is_none() {
            RecordState::Base
        } else {
            RecordState::Ambiguous
        });
    };
    let decoded = crate::CanonicalStore::read_record_file(
        file,
        &record.scope,
        Some(&record.memory_id),
        &layout::record_storage_key(&record.memory_id),
        &layout::record_shard(&record.memory_id),
    );
    let decoded = match decoded {
        Ok(decoded) => decoded,
        Err(StoreError::InvalidRecord { .. }) => return Ok(RecordState::Ambiguous),
        Err(error) => return Err(error),
    };
    if decoded.revision == record.target_revision && decoded.etag == record.target_etag {
        return Ok(RecordState::Target);
    }
    if record
        .base_revision
        .is_some_and(|base| decoded.revision == base)
        && record
            .base_etag
            .as_ref()
            .is_some_and(|base| &decoded.etag == base)
    {
        return Ok(RecordState::Base);
    }
    Ok(RecordState::Ambiguous)
}

fn recover_target_metadata(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    failpoints: &Failpoints,
) -> Result<(), StoreError> {
    let staged = transaction::metadata_temp_relative(manifest)?;
    if root.remove_regular_file_if_exists(&staged)? {
        root.sync_root("sync discarded recovery metadata temp")?;
    }
    let identity = transaction::stage_metadata(root, manifest, failpoints)?;
    transaction::publish_metadata(root, manifest, identity, failpoints)?;
    failpoints.check(PersistenceBoundary::RecoveryMetadataDirectorySynced)
}

fn cleanup_record_temps(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    failpoints: &Failpoints,
) -> Result<(), StoreError> {
    let record_temp = transaction::record_temp_relative(manifest)?;
    if root.remove_regular_file_if_exists(&record_temp)? {
        transaction::sync_parent(root, &record_temp, "sync recovery record temp cleanup")?;
        failpoints.check(PersistenceBoundary::RecoveryRecordDirectorySynced)?;
    }
    let metadata_temp = transaction::metadata_temp_relative(manifest)?;
    if root.remove_regular_file_if_exists(&metadata_temp)? {
        root.sync_root("sync recovery metadata temp cleanup")?;
        failpoints.check(PersistenceBoundary::RecoveryMetadataDirectorySynced)?;
    }
    Ok(())
}

fn cleanup_manifest(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    failpoints: &Failpoints,
) -> Result<(), StoreError> {
    transaction::remove_manifest(root, manifest, failpoints)?;
    failpoints.check(PersistenceBoundary::RecoveryManifestDirectorySynced)
}

fn recover_quarantine(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    failpoints: &Failpoints,
) -> Result<(), StoreError> {
    let TransactionIntent::Quarantine(quarantine) = &manifest.intent else {
        return Err(StoreError::InvalidTransaction);
    };
    let source = layout::record_relative_path(&quarantine.scope, &quarantine.memory_id);
    let destination = transaction::quarantine_relative(quarantine)?;
    let source_digest = digest_if_present(root, &source)?;
    let destination_digest = digest_if_present(root, &destination)?;
    let intended = &quarantine.source_digest;
    match (source_digest.as_deref(), destination_digest.as_deref()) {
        (Some(source), None) if source == intended => {
            reject_existing_receipt(root, manifest)?;
            cleanup_manifest(root, manifest, failpoints)
        }
        (None, Some(destination)) if destination == intended => {
            complete_quarantine(root, manifest, failpoints)
        }
        (Some(source), Some(destination)) if source == intended && destination == intended => {
            root.remove_regular_file(&source_path(manifest)?)?;
            transaction::sync_parent(
                root,
                &source_path(manifest)?,
                "sync duplicate quarantine source cleanup",
            )?;
            failpoints.check(PersistenceBoundary::RecoveryQuarantineSourceDirectorySynced)?;
            complete_quarantine(root, manifest, failpoints)
        }
        _ => Err(StoreError::InvalidTransaction),
    }
}

fn complete_quarantine(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    failpoints: &Failpoints,
) -> Result<(), StoreError> {
    let TransactionIntent::Quarantine(quarantine) = &manifest.intent else {
        return Err(StoreError::InvalidTransaction);
    };
    let destination = transaction::quarantine_relative(quarantine)?;
    root.sync_directory(
        Path::new(layout::QUARANTINE_DIR),
        "sync recovered quarantine destination",
    )?;
    failpoints.check(PersistenceBoundary::RecoveryQuarantineDirectorySynced)?;
    transaction::sync_parent(
        root,
        &source_path(manifest)?,
        "sync recovered quarantine source",
    )?;
    failpoints.check(PersistenceBoundary::RecoveryQuarantineSourceDirectorySynced)?;
    if digest_if_present(root, &destination)?.as_deref() != Some(&quarantine.source_digest) {
        return Err(StoreError::InvalidTransaction);
    }
    let receipt = transaction::DurableQuarantineReceipt::from_manifest(manifest)?;
    transaction::persist_quarantine_receipt(root, &receipt, failpoints)?;
    // `persist_quarantine_receipt` can discover an exact receipt whose rename
    // survived a crash before its parent directory sync. Re-sync explicitly
    // before the recovery boundary and before deleting the manifest.
    root.sync_directory(
        Path::new(layout::QUARANTINE_RECEIPTS_DIR),
        "sync recovered quarantine receipt",
    )?;
    failpoints.check(PersistenceBoundary::RecoveryReceiptDirectorySynced)?;
    cleanup_manifest(root, manifest, failpoints)
}

fn source_path(manifest: &TransactionManifest) -> Result<PathBuf, StoreError> {
    let TransactionIntent::Quarantine(quarantine) = &manifest.intent else {
        return Err(StoreError::InvalidTransaction);
    };
    Ok(layout::record_relative_path(
        &quarantine.scope,
        &quarantine.memory_id,
    ))
}

fn digest_if_present(root: &StoreDirectory, relative: &Path) -> Result<Option<String>, StoreError> {
    root.try_open_regular(relative, false)?
        .map(transaction::raw_file_digest)
        .transpose()
}

fn reject_existing_receipt(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
) -> Result<(), StoreError> {
    if root.regular_file_exists(&transaction::receipt_relative(&manifest.transaction_id)?)? {
        Err(StoreError::InvalidTransaction)
    } else {
        Ok(())
    }
}

fn reject_orphan_metadata_temp(
    root: &StoreDirectory,
    manifest: Option<&TransactionManifest>,
) -> Result<(), StoreError> {
    let expected = manifest
        .and_then(|manifest| transaction::metadata_temp_relative(manifest).ok())
        .and_then(|path| path.file_name().map(OsStr::to_owned));
    let directory = root.root_directory()?;
    let entries = directory
        .entries()
        .map_err(|source| StoreError::io("inspect store control entries", source))?;
    for entry in entries {
        let entry = entry.map_err(|source| StoreError::io("read store control entry", source))?;
        let name = entry.file_name();
        let Some(name_string) = name.to_str() else {
            continue;
        };
        if name_string.starts_with(".store-") && name_string.ends_with(".tmp") {
            if expected.as_ref() == Some(&name) {
                continue;
            }
            return Err(StoreError::InvalidTransaction);
        }
    }
    Ok(())
}

fn reject_orphan_record_temps(root: &StoreDirectory) -> Result<(), StoreError> {
    let records = root.open_directory(Path::new("records"))?;
    reject_orphan_record_temps_in(&records, 0)
}

fn reject_orphan_record_temps_in(
    directory: &cap_std::fs::Dir,
    depth: usize,
) -> Result<(), StoreError> {
    if depth > 4 {
        return Err(StoreError::InvalidLayout);
    }
    let entries = directory
        .entries()
        .map_err(|source| StoreError::io("inspect record transaction artifacts", source))?;
    for entry in entries {
        let entry =
            entry.map_err(|source| StoreError::io("read record transaction artifact", source))?;
        let name = entry.file_name();
        let metadata = directory
            .symlink_metadata(&name)
            .map_err(|source| StoreError::io("inspect record namespace entry", source))?;
        if metadata.is_symlink() {
            if name
                .to_str()
                .is_some_and(|name| name.starts_with(".record-") && name.ends_with(".tmp"))
            {
                return Err(StoreError::UnsafePath);
            }
            continue;
        }
        if metadata.is_dir() {
            let child = StoreDirectory::open_child_directory(directory, &name)?;
            reject_orphan_record_temps_in(&child, depth + 1)?;
            continue;
        }
        if name
            .to_str()
            .is_some_and(|name| name.starts_with(".record-") && name.ends_with(".tmp"))
        {
            let file = StoreDirectory::try_open_regular_in(directory, &name)?
                .ok_or(StoreError::InvalidTransaction)?;
            StoreDirectory::validate_private_open_file(&file)?;
            return Err(StoreError::InvalidTransaction);
        }
    }
    Ok(())
}

fn read_quarantine_receipts(
    root: &StoreDirectory,
    store_id: &StoreId,
) -> Result<Vec<QuarantineReceipt>, StoreError> {
    let directory = root.open_directory(Path::new(layout::QUARANTINE_RECEIPTS_DIR))?;
    let mut receipts = Vec::new();
    let entries = directory
        .entries()
        .map_err(|source| StoreError::io("list quarantine receipts", source))?;
    for entry in entries {
        let entry = entry.map_err(|source| StoreError::io("read quarantine receipt", source))?;
        let name = entry.file_name();
        if transaction::transaction_id_from_receipt_temp_name(&name).is_some() {
            return Err(StoreError::InvalidTransaction);
        }
        let Some(transaction_id) = transaction::transaction_id_from_receipt_name(&name) else {
            return Err(StoreError::InvalidTransaction);
        };
        let file = StoreDirectory::try_open_regular_in(&directory, &name)?
            .ok_or(StoreError::InvalidTransaction)?;
        StoreDirectory::validate_private_open_file(&file)?;
        let receipt =
            transaction::DurableQuarantineReceipt::decode(file, &transaction_id, store_id)?;
        receipts.push(QuarantineReceipt {
            memory_id: receipt.memory_id,
            quarantine_token: receipt.quarantine_token,
        });
    }
    receipts.sort_by(|left, right| {
        left.memory_id
            .cmp(&right.memory_id)
            .then_with(|| left.quarantine_token.cmp(&right.quarantine_token))
    });
    Ok(receipts)
}

fn durability_probe_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(rest) = name.strip_prefix(".durability-") else {
        return false;
    };
    let Some((transaction_id, suffix)) = rest.rsplit_once('-') else {
        return false;
    };
    transaction::valid_transaction_id(transaction_id) && matches!(suffix, "source" | "target")
}
