use super::*;

/// Produce a deterministic, zero-write migration plan and authorized report.
///
/// `snapshot_root` is an operator-provided isolated copy containing the #36
/// `manifest.json`, `source/`, and exact canonical identity map. The function
/// never reads `BAMBOO_DATA_DIR` and has no source or destination write API.
pub fn plan_bamboo_snapshot(
    snapshot_root: impl AsRef<Path>,
    store: &CanonicalStore,
    authority: &jiandu_store::AuthorizedScopes,
    context: &TrustedRequestContext,
) -> Result<BambooDryRun, BambooImportError> {
    let validation_authority = authority
        .authorize_store_validation(context)
        .map_err(store_error)?;
    let export_authority = authority
        .authorize_all_scope_export(context)
        .map_err(store_error)?;
    let target_validation = store
        .validate_all(&validation_authority)
        .map_err(store_error)?;
    let target_export = store.export_all(&export_authority).map_err(store_error)?;

    let prepared = prepare_snapshot(snapshot_root.as_ref())?;
    let bundle = PortableExportBundle::from_canonical_records(
        prepared.evidence.projection_store_id.clone(),
        prepared.evidence.projection_snapshot,
        prepared.records.clone(),
    )
    .map_err(store_error)?;
    let bundle_bytes = bundle.canonical_bytes().map_err(store_error)?;
    let portable_plan = store
        .plan_import(authority, context, &bundle_bytes)
        .map_err(store_error)?;

    let classifications = portable_plan
        .entries
        .iter()
        .map(|entry| (entry.memory_id.clone(), entry.classification))
        .collect::<BTreeMap<_, _>>();
    let mut report_cases = prepared.cases;
    let mut destination_counts = BambooDestinationCounts::default();
    for case in &mut report_cases {
        if let Some(memory_id) = &case.target_memory_id {
            let classification = classifications
                .get(memory_id)
                .copied()
                .ok_or(BambooImportError::InvalidSnapshot)?;
            case.destination_classification = Some(classification);
            destination_counts.observe(classification)?;
        }
    }
    let mut report = BambooMigrationReport {
        format_version: BAMBOO_MIGRATION_REPORT_FORMAT.to_owned(),
        source: prepared.evidence.clone(),
        cases: report_cases,
        source_counts: prepared.source_counts,
        destination_counts,
        digest: String::new(),
    };
    report.digest = report.expected_digest()?;
    report.validate()?;
    let _ = report.canonical_bytes()?;

    let target_snapshot = target_export.snapshot;
    if portable_plan.target_store_id != *store.store_id()
        || portable_plan.target_snapshot != target_snapshot
        || target_export.source_store_id != *store.store_id()
        || target_export.source_store_format != jiandu_store::STORE_FORMAT_VERSION
        || target_validation.source_store_id.as_ref() != Some(store.store_id())
        || target_validation.snapshot != Some(target_snapshot)
    {
        return Err(BambooImportError::Store(StoreErrorCode::RecoveryRequired));
    }
    let destination_pristine = target_validation.findings.is_empty()
        && !target_validation.truncated
        && target_snapshot.store_revision.0 == 0
        && target_snapshot.audit_sequence.0 == 0
        && target_export.scopes.is_empty()
        && target_export.records.is_empty()
        && target_export.tombstones.is_empty();
    let eligible_mappings_complete = prepared.eligible_record_count == prepared.records.len();
    let mut plan = BambooReviewedPlan {
        format_version: BAMBOO_IMPORT_PLAN_FORMAT.to_owned(),
        source: prepared.evidence,
        target_store_id: store.store_id().clone(),
        target_snapshot,
        target_validation_digest: target_validation.digest.clone(),
        target_export_digest: target_export.digest.clone(),
        report_digest: report.digest.clone(),
        portable_bundle_digest: bundle.digest.as_str().to_owned(),
        portable_plan_digest: portable_plan.digest.clone(),
        eligible_record_count: u32::try_from(prepared.eligible_record_count)
            .map_err(|_| BambooImportError::InvalidSnapshot)?,
        mapped_record_count: u32::try_from(prepared.records.len())
            .map_err(|_| BambooImportError::InvalidSnapshot)?,
        destination_pristine,
        eligible_mappings_complete,
        portable_plan_committable: portable_plan.committable,
        committable: destination_pristine
            && eligible_mappings_complete
            && portable_plan.committable,
        digest: String::new(),
    };
    plan.digest = plan.expected_digest()?;
    plan.validate()?;
    let _ = plan.canonical_bytes()?;
    Ok(BambooDryRun {
        plan,
        report,
        portable_plan,
        bundle_bytes,
    })
}

/// Commit exactly one unchanged reviewed plan through Jiandu's canonical
/// portable-import WAL, then validate and export the resulting store.
pub fn commit_bamboo_snapshot(
    snapshot_root: impl AsRef<Path>,
    store: &mut CanonicalStore,
    authority: &jiandu_store::AuthorizedScopes,
    context: &TrustedRequestContext,
    reviewed_plan_bytes: &[u8],
    idempotency_key: &IdempotencyKey,
) -> Result<BambooImportCommit, BambooImportError> {
    commit_bamboo_snapshot_with_hook(
        snapshot_root.as_ref(),
        store,
        authority,
        context,
        reviewed_plan_bytes,
        idempotency_key,
        |_| Ok(()),
    )
}

pub(crate) fn commit_bamboo_snapshot_with_hook<F>(
    snapshot_root: &Path,
    store: &mut CanonicalStore,
    authority: &jiandu_store::AuthorizedScopes,
    context: &TrustedRequestContext,
    reviewed_plan_bytes: &[u8],
    idempotency_key: &IdempotencyKey,
    post_commit_hook: F,
) -> Result<BambooImportCommit, BambooImportError>
where
    F: FnOnce(&ImportCommit) -> Result<(), StoreErrorCode>,
{
    let reviewed = BambooReviewedPlan::decode_canonical(reviewed_plan_bytes)?;
    let validation_authority = authority
        .authorize_store_validation(context)
        .map_err(store_error)?;
    let export_authority = authority
        .authorize_all_scope_export(context)
        .map_err(store_error)?;
    if reviewed.target_store_id != *store.store_id() {
        return Err(BambooImportError::StaleReviewedPlan);
    }

    // A changed watermark may be an acknowledgement/evidence-loss retry. The
    // canonical import API resolves the exact receipt before target conflicts,
    // so regenerate only source-bound material and give replay precedence.
    if store.watermark().map_err(store_error)? != reviewed.target_snapshot.store_revision {
        let (bundle_bytes, report) = replay_material(snapshot_root, &reviewed)?;
        let replay = match store.replay_portable_import(
            authority,
            context,
            &bundle_bytes,
            &reviewed.portable_plan_digest,
            idempotency_key,
        ) {
            Ok(Some(replay)) => replay,
            Err(StoreError::IdempotencyConflict) => {
                return Err(BambooImportError::StaleReviewedPlan);
            }
            Err(error) => return Err(store_error(error)),
            Ok(None) => {
                let validation = store
                    .validate_all(&validation_authority)
                    .map_err(store_error)?;
                if validation.truncated || !validation.findings.is_empty() {
                    return Err(BambooImportError::Store(StoreErrorCode::ValidationFailed));
                }
                let export = store.export_all(&export_authority).map_err(store_error)?;
                if export.tombstones.iter().any(|tombstone| {
                    report.cases.iter().any(|case| {
                        case.mapping_disposition == BambooMappingDisposition::Ready
                            && case.target_memory_id.as_ref() == Some(&tombstone.memory_id)
                    })
                }) {
                    return Err(BambooImportError::ProtectedTombstoneResurrection);
                }
                return Err(BambooImportError::DestinationNotPristine);
            }
        };
        if !replay.idempotent_replay {
            return Err(BambooImportError::StaleReviewedPlan);
        }
        return finalize_committed_import(
            store,
            &validation_authority,
            &export_authority,
            replay,
            &reviewed,
            &report,
        );
    }

    let current = plan_bamboo_snapshot(snapshot_root, store, authority, context)?;
    let current_bytes = current.plan.canonical_bytes()?;

    if current.plan.source != reviewed.source {
        return Err(BambooImportError::SourceDrift);
    }
    if current
        .portable_plan
        .entries
        .iter()
        .any(|entry| entry.classification == ImportClassification::TombstoneProtected)
    {
        return Err(BambooImportError::ProtectedTombstoneResurrection);
    }
    if !current.plan.destination_pristine {
        return Err(BambooImportError::DestinationNotPristine);
    }
    if !current.plan.eligible_mappings_complete {
        return Err(BambooImportError::UnresolvedEligibleIdentity);
    }
    if current_bytes != reviewed_plan_bytes {
        return Err(BambooImportError::StaleReviewedPlan);
    }
    if !reviewed.committable || !current.plan.portable_plan_committable {
        return Err(BambooImportError::PlanNotCommittable);
    }

    let import = store
        .import_portable(
            authority,
            context,
            &current.bundle_bytes,
            &current.portable_plan.digest,
            idempotency_key,
        )
        .map_err(store_error)?;
    if let Err(failure) = post_commit_hook(&import) {
        return Err(committed_evidence_error(&import, failure));
    }
    finalize_committed_import(
        store,
        &validation_authority,
        &export_authority,
        import,
        &current.plan,
        &current.report,
    )
}

fn replay_material(
    snapshot_root: &Path,
    reviewed: &BambooReviewedPlan,
) -> Result<(Vec<u8>, BambooMigrationReport), BambooImportError> {
    if !reviewed.destination_pristine
        || !reviewed.eligible_mappings_complete
        || !reviewed.portable_plan_committable
        || !reviewed.committable
        || reviewed.target_snapshot.store_revision.0 != 0
        || reviewed.target_snapshot.audit_sequence.0 != 0
    {
        return Err(BambooImportError::InvalidReviewedPlan);
    }
    verify_pristine_target_digests(reviewed)?;
    let prepared = prepare_snapshot(snapshot_root)?;
    if prepared.evidence != reviewed.source {
        return Err(BambooImportError::SourceDrift);
    }
    if prepared.records.len() != prepared.eligible_record_count
        || u32::try_from(prepared.eligible_record_count).ok()
            != Some(reviewed.eligible_record_count)
        || u32::try_from(prepared.records.len()).ok() != Some(reviewed.mapped_record_count)
    {
        return Err(BambooImportError::UnresolvedEligibleIdentity);
    }
    let bundle = PortableExportBundle::from_canonical_records(
        prepared.evidence.projection_store_id.clone(),
        prepared.evidence.projection_snapshot,
        prepared.records,
    )
    .map_err(store_error)?;
    if bundle.digest.as_str() != reviewed.portable_bundle_digest {
        return Err(BambooImportError::StaleReviewedPlan);
    }
    let bundle_bytes = bundle.canonical_bytes().map_err(store_error)?;
    let mut cases = prepared.cases;
    let mut destination_counts = BambooDestinationCounts::default();
    for case in &mut cases {
        if case.mapping_disposition == BambooMappingDisposition::Ready {
            case.destination_classification = Some(ImportClassification::Accepted);
            destination_counts.observe(ImportClassification::Accepted)?;
        }
    }
    let mut report = BambooMigrationReport {
        format_version: BAMBOO_MIGRATION_REPORT_FORMAT.to_owned(),
        source: prepared.evidence,
        cases,
        source_counts: prepared.source_counts,
        destination_counts,
        digest: String::new(),
    };
    report.digest = report.expected_digest()?;
    report.validate()?;
    if report.digest != reviewed.report_digest {
        return Err(BambooImportError::StaleReviewedPlan);
    }
    Ok((bundle_bytes, report))
}

fn verify_pristine_target_digests(reviewed: &BambooReviewedPlan) -> Result<(), BambooImportError> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct PristineValidationPayload<'a> {
        format_version: &'static str,
        mode: &'static str,
        source_store_id: Option<&'a StoreId>,
        snapshot: Option<SnapshotWatermark>,
        inspected_scopes: &'a [MemoryScope],
        findings: &'a [()],
        truncated: bool,
    }

    let payload = serde_json::to_vec(&PristineValidationPayload {
        format_version: jiandu_store::VALIDATION_REPORT_FORMAT_VERSION,
        mode: "all_scopes",
        source_store_id: Some(&reviewed.target_store_id),
        snapshot: Some(reviewed.target_snapshot),
        inspected_scopes: &[],
        findings: &[],
        truncated: false,
    })
    .map_err(|_| BambooImportError::InvalidReviewedPlan)?;
    let mut validation_hasher = Sha256::new();
    validation_hasher.update(b"jiandu/validation-report/v1\0");
    validation_hasher.update(payload);
    let validation_digest = format!("sha256:{}", hex_digest(&validation_hasher.finalize()));
    let empty_export = PortableExportBundle::from_canonical_records(
        reviewed.target_store_id.clone(),
        reviewed.target_snapshot,
        Vec::new(),
    )
    .map_err(store_error)?;
    if reviewed.target_validation_digest.as_str() != validation_digest
        || reviewed.target_export_digest != empty_export.digest
    {
        return Err(BambooImportError::StaleReviewedPlan);
    }
    Ok(())
}

fn finalize_committed_import(
    store: &CanonicalStore,
    validation_authority: &jiandu_store::AuthorizedValidationAdmin,
    export_authority: &jiandu_store::AuthorizedExportAdmin,
    import: ImportCommit,
    plan: &BambooReviewedPlan,
    report: &BambooMigrationReport,
) -> Result<BambooImportCommit, BambooImportError> {
    let validation = store
        .validate_all(validation_authority)
        .map_err(|error| committed_evidence_error(&import, error.code()))?;
    if validation.truncated || !validation.findings.is_empty() {
        return Err(committed_evidence_error(
            &import,
            StoreErrorCode::ValidationFailed,
        ));
    }
    let export = store
        .export_all(export_authority)
        .map_err(|error| committed_evidence_error(&import, error.code()))?;
    if export.snapshot != import.result.target_snapshot
        || export.source_store_id != import.result.target_store_id
    {
        return Err(committed_evidence_error(
            &import,
            StoreErrorCode::RecoveryRequired,
        ));
    }
    let mut evidence = BambooCutoverEvidence {
        format_version: BAMBOO_CUTOVER_EVIDENCE_FORMAT.to_owned(),
        source_aggregate_sha256: plan.source.aggregate_sha256.clone(),
        mapping_contract_sha256: plan.source.mapping_contract_sha256.clone(),
        plan_digest: plan.digest.clone(),
        report_digest: report.digest.clone(),
        portable_bundle_digest: plan.portable_bundle_digest.clone(),
        portable_plan_digest: plan.portable_plan_digest.clone(),
        target_store_id: import.result.target_store_id.clone(),
        base_snapshot: import.result.base_snapshot,
        target_snapshot: import.result.target_snapshot,
        transaction_id: import.result.transaction_id.clone(),
        import_result_digest: import.result.digest.clone(),
        backup_metadata_digest: import.backup_metadata.digest.clone(),
        validation_digest: validation.digest.clone(),
        export_digest: export.digest.clone(),
        digest: String::new(),
    };
    evidence.digest = evidence
        .expected_digest()
        .map_err(|_| committed_evidence_error(&import, StoreErrorCode::InvalidRequest))?;
    evidence
        .validate()
        .map_err(|_| committed_evidence_error(&import, StoreErrorCode::InvalidRequest))?;
    let _ = evidence
        .canonical_bytes()
        .map_err(|_| committed_evidence_error(&import, StoreErrorCode::InvalidRequest))?;
    Ok(BambooImportCommit {
        import,
        validation,
        export,
        evidence,
    })
}

fn committed_evidence_error(import: &ImportCommit, failure: StoreErrorCode) -> BambooImportError {
    BambooImportError::CommittedEvidenceUnavailable {
        transaction_id: import.result.transaction_id.clone(),
        import_result_digest: import.result.digest.as_str().to_owned(),
        backup_metadata_digest: import.backup_metadata.digest.as_str().to_owned(),
        failure,
    }
}
