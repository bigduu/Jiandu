use crate::{
    BAMBOO_CUTOVER_EVIDENCE_FORMAT, BAMBOO_IMPORT_PLAN_FORMAT, BAMBOO_MIGRATION_REPORT_FORMAT,
    BAMBOO_SOURCE_COMMIT, BAMBOO_SOURCE_REPOSITORY, BAMBOO_SOURCE_TREE, BambooImportError,
    MAX_EVIDENCE_BYTES, MAX_PLAN_BYTES, MAX_REPORT_BYTES, canonical_json, decode_canonical,
    digest_payload, portable_relative_path, valid_content_digest,
};
use jiandu_core::{MemoryId, MemoryScope};
use jiandu_store::{
    ExportDigest, ImportClassification, ImportCommit, ImportDigest, ImportDryRunPlan,
    PortableExportBundle, SnapshotWatermark, StoreId, ValidationReport,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;

/// Frozen #36 source-side mapping classification. Destination conflicts are
/// intentionally represented separately by [`ImportClassification`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BambooSourceOutcome {
    Accepted,
    Transformed,
    Unresolved,
    Skipped,
    Quarantined,
}

/// Whether a source case participates in the canonical portable projection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BambooMappingDisposition {
    NotEligible,
    Ready,
    UnresolvedIdentity,
    InvalidMapping,
}

/// Explicitly authorized actor evidence. No body or ambient path is present.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BambooActorEvidence {
    pub created_by_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by_id: Option<String>,
    pub updated_by_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by_actor: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<BambooRawSourceEvidence>,
}

/// Raw Bamboo source references retained only in the explicitly authorized
/// migration report.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BambooRawSourceEvidence {
    pub kind: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub message_range: Vec<String>,
}

/// One authorized source report row. `source_outcome` is frozen evidence;
/// `destination_classification` is the independent canonical-store overlay.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BambooReportCase {
    pub case_id: String,
    pub source_relative_path: String,
    pub source_sha256: String,
    pub source_outcome: BambooSourceOutcome,
    pub source_reason_code: String,
    pub mapping_disposition: BambooMappingDisposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_memory_id: Option<MemoryId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_scope: Option<MemoryScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_classification: Option<ImportClassification>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_evidence: Option<BambooActorEvidence>,
}

/// Exact totals for the frozen source classification axis.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BambooSourceCounts {
    pub accepted: u32,
    pub transformed: u32,
    pub unresolved: u32,
    pub skipped: u32,
    pub quarantined: u32,
}

/// Exact totals for canonical destination planning. This remains distinct
/// from [`BambooSourceCounts`].
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BambooDestinationCounts {
    pub accepted: u32,
    pub conflicting: u32,
    pub unauthorized: u32,
    pub tombstone_protected: u32,
    pub invalid: u32,
}

/// Source evidence bound by every report and reviewed plan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BambooSnapshotEvidence {
    pub repository: String,
    pub commit: String,
    pub tree: String,
    pub watermark_kind: String,
    pub native_watermark: Option<String>,
    pub snapshot_evidence_kind: String,
    pub aggregate_sha256: String,
    pub mapping_contract_sha256: String,
    pub compatibility_manifest_sha256: String,
    pub projection_store_id: StoreId,
    pub projection_snapshot: SnapshotWatermark,
}

/// Full authorized migration report. Relative logical source names and actor
/// evidence occur only here, never in errors or rollback evidence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BambooMigrationReport {
    pub format_version: String,
    pub source: BambooSnapshotEvidence,
    pub cases: Vec<BambooReportCase>,
    pub source_counts: BambooSourceCounts,
    pub destination_counts: BambooDestinationCounts,
    pub digest: String,
}

/// Body/path-free reviewed plan. Commit accepts its exact canonical bytes and
/// regenerates every bound artifact before starting the canonical import WAL.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BambooReviewedPlan {
    pub format_version: String,
    pub source: BambooSnapshotEvidence,
    pub target_store_id: StoreId,
    pub target_snapshot: SnapshotWatermark,
    pub target_validation_digest: ExportDigest,
    pub target_export_digest: ExportDigest,
    pub report_digest: String,
    pub portable_bundle_digest: String,
    pub portable_plan_digest: ImportDigest,
    pub eligible_record_count: u32,
    pub mapped_record_count: u32,
    pub destination_pristine: bool,
    pub eligible_mappings_complete: bool,
    pub portable_plan_committable: bool,
    pub committable: bool,
    pub digest: String,
}

/// Deterministic dry-run artifacts. The source bundle bytes remain private so
/// callers cannot substitute content after reviewing the bound plan.
#[derive(Clone)]
pub struct BambooDryRun {
    pub plan: BambooReviewedPlan,
    pub report: BambooMigrationReport,
    pub portable_plan: ImportDryRunPlan,
    pub(crate) bundle_bytes: Vec<u8>,
}

impl fmt::Debug for BambooDryRun {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BambooDryRun")
            .field("plan_digest", &self.plan.digest)
            .field("report_digest", &self.report.digest)
            .field("case_count", &self.report.cases.len())
            .field("bundle_digest", &self.plan.portable_bundle_digest)
            .finish_non_exhaustive()
    }
}

impl BambooDryRun {
    pub fn plan_bytes(&self) -> Result<Vec<u8>, BambooImportError> {
        self.plan.canonical_bytes()
    }

    pub fn report_bytes(&self) -> Result<Vec<u8>, BambooImportError> {
        self.report.canonical_bytes()
    }
}

/// Body/path-free rollback and Bamboo #940 cutover inputs. This is evidence,
/// not a restore or cutover executor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BambooCutoverEvidence {
    pub format_version: String,
    pub source_aggregate_sha256: String,
    pub mapping_contract_sha256: String,
    pub plan_digest: String,
    pub report_digest: String,
    pub portable_bundle_digest: String,
    pub portable_plan_digest: ImportDigest,
    pub target_store_id: StoreId,
    pub base_snapshot: SnapshotWatermark,
    pub target_snapshot: SnapshotWatermark,
    pub transaction_id: String,
    pub import_result_digest: ImportDigest,
    pub backup_metadata_digest: ImportDigest,
    pub validation_digest: ExportDigest,
    pub export_digest: ExportDigest,
    pub digest: String,
}

/// Successful all-or-none import followed by authoritative validation/export.
pub struct BambooImportCommit {
    pub import: ImportCommit,
    pub validation: ValidationReport,
    pub export: PortableExportBundle,
    pub evidence: BambooCutoverEvidence,
}

impl fmt::Debug for BambooImportCommit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BambooImportCommit")
            .field("transaction_id", &self.import.result.transaction_id)
            .field("validation_digest", &self.validation.digest)
            .field("export_digest", &self.export.digest)
            .field("evidence_digest", &self.evidence.digest)
            .finish_non_exhaustive()
    }
}

impl BambooMigrationReport {
    pub(crate) fn expected_digest(&self) -> Result<String, BambooImportError> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Unsigned<'a> {
            format_version: &'a str,
            source: &'a BambooSnapshotEvidence,
            cases: &'a [BambooReportCase],
            source_counts: BambooSourceCounts,
            destination_counts: BambooDestinationCounts,
        }
        digest_payload(
            b"jiandu/bamboo-migration-report/v1\0",
            &Unsigned {
                format_version: &self.format_version,
                source: &self.source,
                cases: &self.cases,
                source_counts: self.source_counts,
                destination_counts: self.destination_counts,
            },
        )
    }

    pub(crate) fn validate(&self) -> Result<(), BambooImportError> {
        let paths_are_sorted = self
            .cases
            .windows(2)
            .all(|pair| pair[0].source_relative_path < pair[1].source_relative_path);
        let ids_unique = self
            .cases
            .iter()
            .map(|case| case.case_id.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            == self.cases.len();
        let mut source_counts = BambooSourceCounts::default();
        let mut destination_counts = BambooDestinationCounts::default();
        for case in &self.cases {
            source_counts.observe(case.source_outcome)?;
            if let Some(classification) = case.destination_classification {
                destination_counts.observe(classification)?;
            }
            let eligible = matches!(
                case.source_outcome,
                BambooSourceOutcome::Accepted | BambooSourceOutcome::Transformed
            );
            let mapping_shape_is_valid = match case.mapping_disposition {
                BambooMappingDisposition::NotEligible => {
                    !eligible
                        && case.target_memory_id.is_none()
                        && case.target_scope.is_none()
                        && case.destination_classification.is_none()
                        && case.actor_evidence.is_none()
                }
                BambooMappingDisposition::Ready => {
                    eligible
                        && case.target_memory_id.is_some()
                        && case.target_scope.is_some()
                        && case.destination_classification.is_some()
                        && case.actor_evidence.is_some()
                }
                BambooMappingDisposition::UnresolvedIdentity
                | BambooMappingDisposition::InvalidMapping => {
                    eligible
                        && case.target_memory_id.is_none()
                        && case.target_scope.is_none()
                        && case.destination_classification.is_none()
                        && case.actor_evidence.is_some()
                }
            };
            if !portable_relative_path(&case.source_relative_path)
                || !valid_content_digest(&case.source_sha256)
                || case.case_id.is_empty()
                || case.source_reason_code.is_empty()
                || !mapping_shape_is_valid
            {
                return Err(BambooImportError::InvalidReviewedPlan);
            }
        }
        if self.format_version != BAMBOO_MIGRATION_REPORT_FORMAT
            || !paths_are_sorted
            || !ids_unique
            || self.cases.len() != 48
            || self.source_counts
                != (BambooSourceCounts {
                    accepted: 1,
                    transformed: 3,
                    unresolved: 8,
                    skipped: 30,
                    quarantined: 6,
                })
            || self.source.validate().is_err()
            || source_counts != self.source_counts
            || destination_counts != self.destination_counts
            || self.digest != self.expected_digest()?
        {
            return Err(BambooImportError::InvalidReviewedPlan);
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, BambooImportError> {
        self.validate()?;
        canonical_json(self, MAX_REPORT_BYTES)
    }

    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, BambooImportError> {
        decode_canonical(bytes, MAX_REPORT_BYTES, Self::validate)
    }
}

impl BambooReviewedPlan {
    pub(crate) fn expected_digest(&self) -> Result<String, BambooImportError> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Unsigned<'a> {
            format_version: &'a str,
            source: &'a BambooSnapshotEvidence,
            target_store_id: &'a StoreId,
            target_snapshot: SnapshotWatermark,
            target_validation_digest: &'a ExportDigest,
            target_export_digest: &'a ExportDigest,
            report_digest: &'a str,
            portable_bundle_digest: &'a str,
            portable_plan_digest: &'a ImportDigest,
            eligible_record_count: u32,
            mapped_record_count: u32,
            destination_pristine: bool,
            eligible_mappings_complete: bool,
            portable_plan_committable: bool,
            committable: bool,
        }
        digest_payload(
            b"jiandu/bamboo-import-plan/v1\0",
            &Unsigned {
                format_version: &self.format_version,
                source: &self.source,
                target_store_id: &self.target_store_id,
                target_snapshot: self.target_snapshot,
                target_validation_digest: &self.target_validation_digest,
                target_export_digest: &self.target_export_digest,
                report_digest: &self.report_digest,
                portable_bundle_digest: &self.portable_bundle_digest,
                portable_plan_digest: &self.portable_plan_digest,
                eligible_record_count: self.eligible_record_count,
                mapped_record_count: self.mapped_record_count,
                destination_pristine: self.destination_pristine,
                eligible_mappings_complete: self.eligible_mappings_complete,
                portable_plan_committable: self.portable_plan_committable,
                committable: self.committable,
            },
        )
    }

    pub(crate) fn validate(&self) -> Result<(), BambooImportError> {
        let complete = self.mapped_record_count == self.eligible_record_count;
        let expected_committable = self.destination_pristine
            && self.eligible_mappings_complete
            && self.portable_plan_committable;
        if self.format_version != BAMBOO_IMPORT_PLAN_FORMAT
            || self.source.validate().is_err()
            || !valid_content_digest(&self.report_digest)
            || !valid_content_digest(&self.portable_bundle_digest)
            || self.target_snapshot.audit_sequence.0 > self.target_snapshot.store_revision.0
            || (self.destination_pristine
                && (self.target_snapshot.store_revision.0 != 0
                    || self.target_snapshot.audit_sequence.0 != 0))
            || self.eligible_record_count != 4
            || self.mapped_record_count > self.eligible_record_count
            || self.eligible_mappings_complete != complete
            || self.committable != expected_committable
            || self.digest != self.expected_digest()?
        {
            return Err(BambooImportError::InvalidReviewedPlan);
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, BambooImportError> {
        self.validate()?;
        canonical_json(self, MAX_PLAN_BYTES)
    }

    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, BambooImportError> {
        decode_canonical(bytes, MAX_PLAN_BYTES, Self::validate)
    }
}

impl BambooSnapshotEvidence {
    fn validate(&self) -> Result<(), BambooImportError> {
        if self.repository != BAMBOO_SOURCE_REPOSITORY
            || self.commit != BAMBOO_SOURCE_COMMIT
            || self.tree != BAMBOO_SOURCE_TREE
            || self.watermark_kind != "sorted_relative_path_size_sha256"
            || self.native_watermark.is_some()
            || self.snapshot_evidence_kind != "immutable_file_manifest"
            || !valid_content_digest(&self.aggregate_sha256)
            || !valid_content_digest(&self.mapping_contract_sha256)
            || !valid_content_digest(&self.compatibility_manifest_sha256)
            || self.projection_snapshot.store_revision.0 != 1
            || self.projection_snapshot.audit_sequence.0 != 0
        {
            return Err(BambooImportError::InvalidReviewedPlan);
        }
        Ok(())
    }
}

impl BambooCutoverEvidence {
    pub(crate) fn expected_digest(&self) -> Result<String, BambooImportError> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Unsigned<'a> {
            format_version: &'a str,
            source_aggregate_sha256: &'a str,
            mapping_contract_sha256: &'a str,
            plan_digest: &'a str,
            report_digest: &'a str,
            portable_bundle_digest: &'a str,
            portable_plan_digest: &'a ImportDigest,
            target_store_id: &'a StoreId,
            base_snapshot: SnapshotWatermark,
            target_snapshot: SnapshotWatermark,
            transaction_id: &'a str,
            import_result_digest: &'a ImportDigest,
            backup_metadata_digest: &'a ImportDigest,
            validation_digest: &'a ExportDigest,
            export_digest: &'a ExportDigest,
        }
        digest_payload(
            b"jiandu/bamboo-cutover-evidence/v1\0",
            &Unsigned {
                format_version: &self.format_version,
                source_aggregate_sha256: &self.source_aggregate_sha256,
                mapping_contract_sha256: &self.mapping_contract_sha256,
                plan_digest: &self.plan_digest,
                report_digest: &self.report_digest,
                portable_bundle_digest: &self.portable_bundle_digest,
                portable_plan_digest: &self.portable_plan_digest,
                target_store_id: &self.target_store_id,
                base_snapshot: self.base_snapshot,
                target_snapshot: self.target_snapshot,
                transaction_id: &self.transaction_id,
                import_result_digest: &self.import_result_digest,
                backup_metadata_digest: &self.backup_metadata_digest,
                validation_digest: &self.validation_digest,
                export_digest: &self.export_digest,
            },
        )
    }

    pub(crate) fn validate(&self) -> Result<(), BambooImportError> {
        if self.format_version != BAMBOO_CUTOVER_EVIDENCE_FORMAT
            || !valid_content_digest(&self.source_aggregate_sha256)
            || !valid_content_digest(&self.mapping_contract_sha256)
            || !valid_content_digest(&self.plan_digest)
            || !valid_content_digest(&self.report_digest)
            || !valid_content_digest(&self.portable_bundle_digest)
            || !valid_canonical_uuid(&self.transaction_id)
            || self.base_snapshot.store_revision.0 != 0
            || self.base_snapshot.audit_sequence.0 != 0
            || self.target_snapshot.store_revision.0 != 1
            || self.target_snapshot.audit_sequence.0 != 1
            || self.digest != self.expected_digest()?
        {
            return Err(BambooImportError::InvalidReviewedPlan);
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, BambooImportError> {
        self.validate()?;
        canonical_json(self, MAX_EVIDENCE_BYTES)
    }

    /// Decode and verify exact canonical rollback/cutover evidence bytes.
    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, BambooImportError> {
        decode_canonical(bytes, MAX_EVIDENCE_BYTES, Self::validate)
    }
}

fn valid_canonical_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
}

impl BambooSourceCounts {
    pub(crate) fn observe(
        &mut self,
        outcome: BambooSourceOutcome,
    ) -> Result<(), BambooImportError> {
        let counter = match outcome {
            BambooSourceOutcome::Accepted => &mut self.accepted,
            BambooSourceOutcome::Transformed => &mut self.transformed,
            BambooSourceOutcome::Unresolved => &mut self.unresolved,
            BambooSourceOutcome::Skipped => &mut self.skipped,
            BambooSourceOutcome::Quarantined => &mut self.quarantined,
        };
        *counter = counter
            .checked_add(1)
            .ok_or(BambooImportError::InvalidSnapshot)?;
        Ok(())
    }
}

impl BambooDestinationCounts {
    pub(crate) fn observe(
        &mut self,
        classification: ImportClassification,
    ) -> Result<(), BambooImportError> {
        let counter = match classification {
            ImportClassification::Accepted => &mut self.accepted,
            ImportClassification::Conflicting => &mut self.conflicting,
            ImportClassification::Unauthorized => &mut self.unauthorized,
            ImportClassification::TombstoneProtected => &mut self.tombstone_protected,
            ImportClassification::Invalid => &mut self.invalid,
        };
        *counter = counter
            .checked_add(1)
            .ok_or(BambooImportError::InvalidSnapshot)?;
        Ok(())
    }
}
