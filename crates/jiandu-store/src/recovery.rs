//! Deterministic startup reconciliation for interrupted store transactions.

use crate::failpoint::Failpoints;
use crate::idempotency::MutationArtifacts;
use crate::layout::{self, FileIdentity, StoreDirectory};
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
    let manifest = read_single_active_manifest(root, &metadata)?;
    let metadata = match manifest.as_ref().map(|manifest| &manifest.intent) {
        Some(TransactionIntent::Record(_)) => recover_record(
            root,
            metadata,
            manifest.as_ref().expect("manifest"),
            failpoints,
        )?,
        Some(TransactionIntent::Forget(_)) => recover_forget(
            root,
            metadata,
            manifest.as_ref().expect("manifest"),
            failpoints,
        )?,
        Some(TransactionIntent::Import(_)) => crate::portable_import::recover_import(
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
    reject_orphan_tombstone_temps(root)?;
    let quarantine_receipts = read_quarantine_receipts(root, &metadata.store_id)?;
    Ok(RecoveryOutcome {
        metadata,
        quarantine_receipts,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TombstoneState {
    Absent,
    Target,
    Ambiguous,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ForgetBodyState {
    Canonical,
    WitnessFull,
    WitnessErased,
    Absent,
    Ambiguous,
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
    metadata: &StoreMetadata,
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
        let manifest = TransactionManifest::decode(file, &transaction_id, &metadata.store_id)?;
        let expected_format = match metadata.format_version.as_str() {
            crate::metadata::LEGACY_STORE_FORMAT_VERSION => {
                transaction::LEGACY_TRANSACTION_FORMAT_VERSION
            }
            crate::metadata::PREVIOUS_STORE_FORMAT_VERSION => {
                transaction::PREVIOUS_TRANSACTION_FORMAT_VERSION
            }
            crate::metadata::V3_STORE_FORMAT_VERSION => transaction::V3_TRANSACTION_FORMAT_VERSION,
            crate::STORE_FORMAT_VERSION => transaction::TRANSACTION_FORMAT_VERSION,
            _ => return Err(StoreError::InvalidStoreMetadata),
        };
        if manifest.format_version != expected_format {
            return Err(StoreError::InvalidTransaction);
        }
        active.push(manifest);
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
    if record.idempotency.is_none() {
        return recover_legacy_record_state(
            root,
            manifest,
            failpoints,
            record_state,
            metadata_state,
        );
    }
    let published_artifact = mutation_artifact_published(root, manifest)?;
    match (record_state, metadata_state) {
        (RecordState::Base, MetadataState::Base) => {
            if published_artifact {
                return Err(StoreError::InvalidTransaction);
            }
            cleanup_transaction_temps(root, manifest, failpoints)?;
            cleanup_manifest(root, manifest, failpoints)?;
            Ok(record.base_store_metadata.clone())
        }
        (RecordState::Target, MetadataState::Base) => {
            let target = read_target_record(root, record)?;
            let artifacts =
                MutationArtifacts::from_record_intent(manifest.store_id.clone(), record, target)?;
            complete_mutation_artifacts(root, manifest, &artifacts, failpoints)?;
            recover_target_metadata(root, manifest, failpoints)?;
            cleanup_transaction_temps(root, manifest, failpoints)?;
            cleanup_manifest(root, manifest, failpoints)?;
            Ok(record.target_store_metadata.clone())
        }
        (RecordState::Target, MetadataState::Target) => {
            let target = read_target_record(root, record)?;
            let artifacts =
                MutationArtifacts::from_record_intent(manifest.store_id.clone(), record, target)?;
            complete_mutation_artifacts(root, manifest, &artifacts, failpoints)?;
            cleanup_transaction_temps(root, manifest, failpoints)?;
            cleanup_manifest(root, manifest, failpoints)?;
            Ok(record.target_store_metadata.clone())
        }
        (RecordState::Base, MetadataState::Target)
        | (RecordState::Ambiguous, _)
        | (_, MetadataState::Ambiguous) => Err(StoreError::InvalidTransaction),
    }
}

fn recover_forget(
    root: &StoreDirectory,
    metadata: StoreMetadata,
    manifest: &TransactionManifest,
    failpoints: &Failpoints,
) -> Result<StoreMetadata, StoreError> {
    let TransactionIntent::Forget(forget) = &manifest.intent else {
        return Err(StoreError::InvalidTransaction);
    };
    let metadata_state = if metadata == forget.base_store_metadata {
        MetadataState::Base
    } else if metadata == forget.target_store_metadata {
        MetadataState::Target
    } else {
        MetadataState::Ambiguous
    };
    let body_state = classify_forget_body(root, manifest, forget)?;
    let tombstone_state = classify_tombstone(root, manifest, forget)?;
    let published_artifact = mutation_artifact_published(root, manifest)?;

    match (body_state, tombstone_state, metadata_state) {
        (ForgetBodyState::Canonical, TombstoneState::Absent, MetadataState::Base)
            if !published_artifact =>
        {
            cleanup_transaction_temps(root, manifest, failpoints)?;
            cleanup_manifest(root, manifest, failpoints)?;
            Ok(forget.base_store_metadata.clone())
        }
        (ForgetBodyState::Canonical, TombstoneState::Target, MetadataState::Base)
            if !published_artifact =>
        {
            let tombstone = sync_recovered_tombstone(root, manifest, forget, failpoints)?;
            let source = transaction::record_relative(manifest)?;
            let file = root
                .try_open_regular(&source, true)?
                .ok_or(StoreError::InvalidTransaction)?;
            StoreDirectory::validate_private_open_file(&file)?;
            validate_forget_record(
                file.try_clone()
                    .map_err(|source| StoreError::io("clone recovered forget record", source))?,
                forget,
            )?;
            let identity = FileIdentity::from_file(&file)?;
            let witness =
                transaction::rename_forget_record(root, manifest, &file, identity, failpoints)?;
            transaction::erase_open_forget_witness(
                root,
                &witness,
                &file,
                identity,
                PersistenceBoundary::RecoveryForgottenBodyErased,
                PersistenceBoundary::RecoveryForgottenBodySynced,
                failpoints,
            )?;
            complete_recovered_forget(
                root,
                manifest,
                forget,
                &tombstone,
                metadata_state,
                failpoints,
            )
        }
        (
            ForgetBodyState::WitnessFull | ForgetBodyState::WitnessErased,
            TombstoneState::Target,
            MetadataState::Base | MetadataState::Target,
        ) => {
            let tombstone = sync_recovered_tombstone(root, manifest, forget, failpoints)?;
            erase_recovered_witness(root, manifest, forget, body_state, failpoints)?;
            complete_recovered_forget(
                root,
                manifest,
                forget,
                &tombstone,
                metadata_state,
                failpoints,
            )?;
            Ok(forget.target_store_metadata.clone())
        }
        _ => Err(StoreError::InvalidTransaction),
    }
}

fn classify_forget_body(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    forget: &transaction::ForgetTransaction,
) -> Result<ForgetBodyState, StoreError> {
    let canonical = root.try_open_regular(&transaction::record_relative(manifest)?, false)?;
    let witness =
        root.try_open_regular(&transaction::erasure_witness_relative(manifest)?, false)?;
    match (canonical, witness) {
        (Some(_), Some(_)) => Ok(ForgetBodyState::Ambiguous),
        (Some(file), None) => match validate_forget_record(file, forget) {
            Ok(()) => Ok(ForgetBodyState::Canonical),
            Err(error @ StoreError::Io { .. }) => Err(error),
            Err(_) => Ok(ForgetBodyState::Ambiguous),
        },
        (None, Some(file)) => {
            StoreDirectory::validate_private_open_file(&file)?;
            let length = file
                .metadata()
                .map_err(|source| StoreError::io("inspect forget erasure witness", source))?
                .len();
            if length == 0 {
                Ok(ForgetBodyState::WitnessErased)
            } else {
                match validate_forget_record(file, forget) {
                    Ok(()) => Ok(ForgetBodyState::WitnessFull),
                    Err(error @ StoreError::Io { .. }) => Err(error),
                    Err(_) => Ok(ForgetBodyState::Ambiguous),
                }
            }
        }
        (None, None) => Ok(ForgetBodyState::Absent),
    }
}

fn validate_forget_record(
    file: std::fs::File,
    forget: &transaction::ForgetTransaction,
) -> Result<(), StoreError> {
    let decoded = crate::CanonicalStore::read_record_file(
        file,
        &forget.scope,
        Some(&forget.memory_id),
        &layout::record_storage_key(&forget.memory_id),
        &layout::record_shard(&forget.memory_id),
    )?;
    if decoded.revision != forget.revision || decoded.etag != forget.etag {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(())
}

fn classify_tombstone(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    forget: &transaction::ForgetTransaction,
) -> Result<TombstoneState, StoreError> {
    let relative = transaction::tombstone_relative(manifest)?;
    let Some(file) = root.try_open_regular(&relative, false)? else {
        return Ok(TombstoneState::Absent);
    };
    let digest_file = file
        .try_clone()
        .map_err(|source| StoreError::io("clone protected tombstone", source))?;
    if transaction::raw_file_digest(digest_file)? != forget.tombstone_digest {
        return Ok(TombstoneState::Ambiguous);
    }
    match crate::tombstone::ProtectedTombstone::decode(file, &manifest.store_id) {
        Ok(tombstone) if tombstone_matches_forget(&tombstone, manifest, forget) => {
            Ok(TombstoneState::Target)
        }
        Ok(_) | Err(StoreError::InvalidTransaction) => Ok(TombstoneState::Ambiguous),
        Err(error) => Err(error),
    }
}

fn read_target_tombstone(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    forget: &transaction::ForgetTransaction,
) -> Result<crate::tombstone::ProtectedTombstone, StoreError> {
    let tombstone =
        crate::tombstone::read_exact(root, &manifest.store_id, &forget.scope, &forget.memory_id)?
            .ok_or(StoreError::InvalidTransaction)?;
    if !tombstone_matches_forget(&tombstone, manifest, forget)
        || crate::idempotency::content_digest(&tombstone.canonical_bytes()?)
            != forget.tombstone_digest
    {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(tombstone)
}

fn tombstone_matches_forget(
    tombstone: &crate::tombstone::ProtectedTombstone,
    manifest: &TransactionManifest,
    forget: &transaction::ForgetTransaction,
) -> bool {
    tombstone.store_id == manifest.store_id
        && tombstone.transaction_id == manifest.transaction_id
        && tombstone.memory_id == forget.memory_id
        && tombstone.scope == forget.scope
        && tombstone.revision == forget.revision
        && tombstone.etag == forget.etag
        && tombstone.store_revision == forget.target_store_metadata.store_revision
        && tombstone.audit_sequence == forget.target_store_metadata.audit_sequence
}

fn sync_recovered_tombstone(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    forget: &transaction::ForgetTransaction,
    failpoints: &Failpoints,
) -> Result<crate::tombstone::ProtectedTombstone, StoreError> {
    let tombstone = read_target_tombstone(root, manifest, forget)?;
    let relative = transaction::tombstone_relative(manifest)?;
    transaction::sync_parent(root, &relative, "sync recovered protected tombstone")?;
    failpoints.check(PersistenceBoundary::RecoveryTombstoneSynced)?;
    Ok(tombstone)
}

fn erase_recovered_witness(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    forget: &transaction::ForgetTransaction,
    body_state: ForgetBodyState,
    failpoints: &Failpoints,
) -> Result<(), StoreError> {
    let relative = transaction::erasure_witness_relative(manifest)?;
    let file = root
        .try_open_regular(&relative, true)?
        .ok_or(StoreError::InvalidTransaction)?;
    StoreDirectory::validate_private_open_file(&file)?;
    match body_state {
        ForgetBodyState::WitnessFull => validate_forget_record(
            file.try_clone()
                .map_err(|source| StoreError::io("clone recovered forget witness", source))?,
            forget,
        )?,
        ForgetBodyState::WitnessErased => {
            if file
                .metadata()
                .map_err(|source| StoreError::io("inspect recovered forget witness", source))?
                .len()
                != 0
            {
                return Err(StoreError::InvalidTransaction);
            }
        }
        _ => return Err(StoreError::InvalidTransaction),
    }
    let identity = FileIdentity::from_file(&file)?;
    transaction::sync_parent(root, &relative, "sync recovered forget witness namespace")?;
    failpoints.check(PersistenceBoundary::RecoveryForgetWitnessDirectorySynced)?;
    transaction::erase_open_forget_witness(
        root,
        &relative,
        &file,
        identity,
        PersistenceBoundary::RecoveryForgottenBodyErased,
        PersistenceBoundary::RecoveryForgottenBodySynced,
        failpoints,
    )
}

fn complete_recovered_forget(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    forget: &transaction::ForgetTransaction,
    tombstone: &crate::tombstone::ProtectedTombstone,
    metadata_state: MetadataState,
    failpoints: &Failpoints,
) -> Result<StoreMetadata, StoreError> {
    let artifacts =
        MutationArtifacts::from_forget_intent(manifest.store_id.clone(), forget, tombstone)?;
    complete_mutation_artifacts(root, manifest, &artifacts, failpoints)?;
    if metadata_state == MetadataState::Base {
        recover_target_metadata(root, manifest, failpoints)?;
    }
    cleanup_transaction_temps(root, manifest, failpoints)?;
    cleanup_manifest(root, manifest, failpoints)?;
    Ok(forget.target_store_metadata.clone())
}

fn recover_legacy_record_state(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    failpoints: &Failpoints,
    record_state: RecordState,
    metadata_state: MetadataState,
) -> Result<StoreMetadata, StoreError> {
    let TransactionIntent::Record(record) = &manifest.intent else {
        return Err(StoreError::InvalidTransaction);
    };
    match (record_state, metadata_state) {
        (RecordState::Base, MetadataState::Base) => {
            cleanup_transaction_temps(root, manifest, failpoints)?;
            cleanup_manifest(root, manifest, failpoints)?;
            Ok(record.base_store_metadata.clone())
        }
        (RecordState::Target, MetadataState::Base) => {
            recover_target_metadata(root, manifest, failpoints)?;
            cleanup_transaction_temps(root, manifest, failpoints)?;
            cleanup_manifest(root, manifest, failpoints)?;
            Ok(record.target_store_metadata.clone())
        }
        (RecordState::Target, MetadataState::Target) => {
            cleanup_transaction_temps(root, manifest, failpoints)?;
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

fn read_target_record(
    root: &StoreDirectory,
    record: &transaction::RecordTransaction,
) -> Result<jiandu_core::MemoryRecord, StoreError> {
    let relative = layout::record_relative_path(&record.scope, &record.memory_id);
    let file = root
        .try_open_regular(&relative, false)?
        .ok_or(StoreError::InvalidTransaction)?;
    let decoded = crate::CanonicalStore::read_record_file(
        file,
        &record.scope,
        Some(&record.memory_id),
        &layout::record_storage_key(&record.memory_id),
        &layout::record_shard(&record.memory_id),
    )?;
    if decoded.revision != record.target_revision || decoded.etag != record.target_etag {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(decoded)
}

fn mutation_artifact_published(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
) -> Result<bool, StoreError> {
    let idempotency = transaction::mutation_idempotency(manifest)?;
    for relative in [
        crate::idempotency::result_relative(&idempotency.binding.receipt_id)?,
        crate::idempotency::receipt_relative_for_binding(&idempotency.binding)?,
        crate::idempotency::audit_relative(idempotency.binding.audit_sequence)?,
    ] {
        if root.regular_file_exists(&relative)? {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Clone, Copy)]
enum MutationArtifactKind {
    Result,
    Receipt,
    Audit,
}

fn complete_mutation_artifacts(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    artifacts: &MutationArtifacts,
    failpoints: &Failpoints,
) -> Result<(), StoreError> {
    let binding = &transaction::mutation_idempotency(manifest)?.binding;
    for relative in [
        crate::idempotency::result_relative(&binding.receipt_id)?,
        crate::idempotency::receipt_relative_for_binding(binding)?,
        crate::idempotency::audit_relative(binding.audit_sequence)?,
    ] {
        let parent = relative.parent().ok_or(StoreError::InvalidTransaction)?;
        root.create_directory_all(parent)?;
        // Recovery can recreate several previously unseen directory entries
        // (principal, operation, and shard). Sync every ancestor, as the live
        // mutation path does, before any reconstructed artifact can become
        // committed by the metadata watermark.
        transaction::sync_directory_chain(
            root,
            parent,
            "sync recovered mutation artifact namespace",
        )?;
    }
    failpoints.check(PersistenceBoundary::RecoveryIdempotencyNamespacePrepared)?;
    for kind in [
        MutationArtifactKind::Result,
        MutationArtifactKind::Receipt,
        MutationArtifactKind::Audit,
    ] {
        complete_mutation_artifact(root, manifest, artifacts, kind, failpoints)?;
    }
    Ok(())
}

fn complete_mutation_artifact(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    artifacts: &MutationArtifacts,
    kind: MutationArtifactKind,
    failpoints: &Failpoints,
) -> Result<(), StoreError> {
    let binding = &transaction::mutation_idempotency(manifest)?.binding;
    let (staged, published) = match kind {
        MutationArtifactKind::Result => (
            crate::idempotency::result_temp_relative(binding)?,
            crate::idempotency::result_relative(&binding.receipt_id)?,
        ),
        MutationArtifactKind::Receipt => (
            crate::idempotency::receipt_temp_relative(binding)?,
            crate::idempotency::receipt_relative_for_binding(binding)?,
        ),
        MutationArtifactKind::Audit => (
            crate::idempotency::audit_temp_relative(binding)?,
            crate::idempotency::audit_relative(binding.audit_sequence)?,
        ),
    };

    if let Some(file) = root.try_open_regular(&published, false)? {
        validate_mutation_artifact(file, artifacts, kind)?;
        if root.remove_regular_file_if_exists(&staged)? {
            transaction::sync_parent(root, &staged, "sync recovered artifact temp cleanup")?;
        }
    } else {
        let staged_identity = if let Some(file) = root.try_open_regular(&staged, false)? {
            validate_mutation_artifact(
                file.try_clone()
                    .map_err(|source| StoreError::io("clone staged mutation artifact", source))?,
                artifacts,
                kind,
            )?;
            FileIdentity::from_file(&file)?
        } else {
            match kind {
                MutationArtifactKind::Result => transaction::stage_mutation_result(
                    root,
                    manifest,
                    &artifacts.result_bytes,
                    failpoints,
                )?,
                MutationArtifactKind::Receipt => transaction::stage_idempotency_receipt(
                    root,
                    manifest,
                    &artifacts.receipt_bytes,
                    failpoints,
                )?,
                MutationArtifactKind::Audit => transaction::stage_mutation_audit(
                    root,
                    manifest,
                    &artifacts.audit_bytes,
                    failpoints,
                )?,
            }
        };
        match kind {
            MutationArtifactKind::Result => {
                transaction::publish_mutation_result(root, manifest, staged_identity, failpoints)?
            }
            MutationArtifactKind::Receipt => transaction::publish_idempotency_receipt(
                root,
                manifest,
                staged_identity,
                failpoints,
            )?,
            MutationArtifactKind::Audit => {
                transaction::publish_mutation_audit(root, manifest, staged_identity, failpoints)?
            }
        }
    }

    transaction::sync_parent(root, &published, "sync recovered mutation artifact")?;
    failpoints.check(match kind {
        MutationArtifactKind::Result => PersistenceBoundary::RecoveryMutationResultDirectorySynced,
        MutationArtifactKind::Receipt => {
            PersistenceBoundary::RecoveryMutationReceiptDirectorySynced
        }
        MutationArtifactKind::Audit => PersistenceBoundary::RecoveryMutationAuditDirectorySynced,
    })
}

fn validate_mutation_artifact(
    file: std::fs::File,
    artifacts: &MutationArtifacts,
    kind: MutationArtifactKind,
) -> Result<(), StoreError> {
    StoreDirectory::validate_private_open_file(&file)?;
    let digest_file = file
        .try_clone()
        .map_err(|source| StoreError::io("clone mutation artifact for digest", source))?;
    let expected_digest = match kind {
        MutationArtifactKind::Result => &artifacts.result_digest,
        MutationArtifactKind::Receipt => &artifacts.receipt_digest,
        MutationArtifactKind::Audit => &artifacts.audit_digest,
    };
    if transaction::raw_file_digest(digest_file)? != *expected_digest {
        return Err(StoreError::InvalidTransaction);
    }
    match kind {
        MutationArtifactKind::Result => {
            let decoded = crate::idempotency::DurableMutationResult::decode(
                file,
                artifacts.result.store_id(),
                artifacts.result.binding(),
            )?;
            (decoded == artifacts.result)
                .then_some(())
                .ok_or(StoreError::InvalidTransaction)
        }
        MutationArtifactKind::Receipt => {
            let decoded = crate::idempotency::DurableIdempotencyReceipt::decode(
                file,
                &artifacts.receipt.store_id,
                &artifacts.receipt.binding.receipt_id,
            )?;
            (decoded == artifacts.receipt)
                .then_some(())
                .ok_or(StoreError::InvalidTransaction)
        }
        MutationArtifactKind::Audit => {
            let decoded = crate::idempotency::DurableAuditEvent::decode(
                file,
                &artifacts.audit.store_id,
                artifacts.audit.binding.audit_sequence,
            )?;
            (decoded == artifacts.audit)
                .then_some(())
                .ok_or(StoreError::InvalidTransaction)
        }
    }
}

pub(crate) fn recover_target_metadata(
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

fn cleanup_transaction_temps(
    root: &StoreDirectory,
    manifest: &TransactionManifest,
    failpoints: &Failpoints,
) -> Result<(), StoreError> {
    if matches!(manifest.intent, TransactionIntent::Record(_)) {
        let record_temp = transaction::record_temp_relative(manifest)?;
        if root.remove_regular_file_if_exists(&record_temp)? {
            transaction::sync_parent(root, &record_temp, "sync recovery record temp cleanup")?;
            failpoints.check(PersistenceBoundary::RecoveryRecordDirectorySynced)?;
        }
    } else if let TransactionIntent::Forget(forget) = &manifest.intent {
        let tombstone_temp = transaction::tombstone_temp_relative(manifest)?;
        if let Some(file) = root.try_open_regular(&tombstone_temp, false)? {
            StoreDirectory::validate_private_open_file(&file)?;
            let digest_file = file
                .try_clone()
                .map_err(|source| StoreError::io("clone staged tombstone", source))?;
            if transaction::raw_file_digest(digest_file)? != forget.tombstone_digest {
                return Err(StoreError::InvalidTransaction);
            }
            let tombstone = crate::tombstone::ProtectedTombstone::decode(file, &manifest.store_id)?;
            if !tombstone_matches_forget(&tombstone, manifest, forget) {
                return Err(StoreError::InvalidTransaction);
            }
            root.remove_regular_file(&tombstone_temp)?;
            transaction::sync_parent(
                root,
                &tombstone_temp,
                "sync recovery tombstone temp cleanup",
            )?;
            failpoints.check(PersistenceBoundary::RecoveryTombstoneSynced)?;
        }
        if let Some(file) =
            root.try_open_regular(&transaction::erasure_witness_relative(manifest)?, false)?
        {
            StoreDirectory::validate_private_open_file(&file)?;
            if file
                .metadata()
                .map_err(|source| StoreError::io("inspect committed forget witness", source))?
                .len()
                != 0
            {
                return Err(StoreError::InvalidTransaction);
            }
        }
    }
    let metadata_temp = transaction::metadata_temp_relative(manifest)?;
    if root.remove_regular_file_if_exists(&metadata_temp)? {
        root.sync_root("sync recovery metadata temp cleanup")?;
        failpoints.check(PersistenceBoundary::RecoveryMetadataDirectorySynced)?;
    }
    if let Ok(idempotency) = transaction::mutation_idempotency(manifest) {
        for (relative, boundary) in [
            (
                crate::idempotency::result_temp_relative(&idempotency.binding)?,
                PersistenceBoundary::RecoveryMutationResultDirectorySynced,
            ),
            (
                crate::idempotency::receipt_temp_relative(&idempotency.binding)?,
                PersistenceBoundary::RecoveryMutationReceiptDirectorySynced,
            ),
            (
                crate::idempotency::audit_temp_relative(&idempotency.binding)?,
                PersistenceBoundary::RecoveryMutationAuditDirectorySynced,
            ),
        ] {
            if root.remove_regular_file_if_exists(&relative)? {
                transaction::sync_parent(root, &relative, "sync recovery artifact temp cleanup")?;
                failpoints.check(boundary)?;
            }
        }
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
        if name == OsStr::new(layout::STORE_METADATA_MIGRATION_FILE)
            || name == OsStr::new(layout::V3_STORE_METADATA_MIGRATION_FILE)
            || name == OsStr::new(layout::PREVIOUS_STORE_METADATA_MIGRATION_FILE)
        {
            continue;
        }
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
        let erasure_witness = transaction::transaction_id_from_erasure_witness_name(&name);
        let resembles_erasure_witness = name
            .to_str()
            .is_some_and(|name| name.starts_with(".forgotten-"));
        if metadata.is_symlink() {
            if is_record_transaction_temp(&name) || resembles_erasure_witness {
                return Err(StoreError::UnsafePath);
            }
            continue;
        }
        if metadata.is_dir() {
            if resembles_erasure_witness {
                return Err(StoreError::InvalidTransaction);
            }
            let child = StoreDirectory::open_child_directory(directory, &name)?;
            reject_orphan_record_temps_in(&child, depth + 1)?;
            continue;
        }
        if erasure_witness.is_some() {
            let file = StoreDirectory::try_open_regular_in(directory, &name)?
                .ok_or(StoreError::InvalidTransaction)?;
            StoreDirectory::validate_private_open_file(&file)?;
            if file
                .metadata()
                .map_err(|source| StoreError::io("inspect forget erasure witness", source))?
                .len()
                != 0
            {
                return Err(StoreError::InvalidTransaction);
            }
            continue;
        }
        if resembles_erasure_witness {
            return Err(StoreError::InvalidTransaction);
        }
        if is_record_transaction_temp(&name) {
            let file = StoreDirectory::try_open_regular_in(directory, &name)?
                .ok_or(StoreError::InvalidTransaction)?;
            StoreDirectory::validate_private_open_file(&file)?;
            return Err(StoreError::InvalidTransaction);
        }
    }
    Ok(())
}

fn is_record_transaction_temp(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        (name.starts_with(".record-") && name.ends_with(".tmp"))
            || (name.starts_with(".import-record-") && name.ends_with(".tmp"))
            || (name.starts_with(".forgotten-") && name.ends_with(".body"))
    })
}

fn reject_orphan_tombstone_temps(root: &StoreDirectory) -> Result<(), StoreError> {
    let tombstones = root.open_directory(Path::new(layout::TOMBSTONES_DIR))?;
    reject_orphan_tombstone_temps_in(&tombstones, 0)
}

fn reject_orphan_tombstone_temps_in(
    directory: &cap_std::fs::Dir,
    depth: usize,
) -> Result<(), StoreError> {
    if depth > 4 {
        return Err(StoreError::InvalidLayout);
    }
    let entries = directory
        .entries()
        .map_err(|source| StoreError::io("inspect tombstone transaction artifacts", source))?;
    for entry in entries {
        let entry = entry
            .map_err(|source| StoreError::io("read tombstone transaction artifact", source))?;
        let name = entry.file_name();
        let metadata = directory
            .symlink_metadata(&name)
            .map_err(|source| StoreError::io("inspect tombstone namespace entry", source))?;
        let is_temp = name.to_str().is_some_and(|name| {
            (name.starts_with(".tombstone-") || name.starts_with(".import-tombstone-"))
                && name.ends_with(".tmp")
        });
        if metadata.is_symlink() {
            if is_temp {
                return Err(StoreError::UnsafePath);
            }
            continue;
        }
        if metadata.is_dir() {
            let child = StoreDirectory::open_child_directory(directory, &name)?;
            reject_orphan_tombstone_temps_in(&child, depth + 1)?;
        } else if is_temp {
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
