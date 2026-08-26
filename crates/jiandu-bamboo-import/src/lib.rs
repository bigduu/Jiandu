//! Deterministic, read-only planning and reviewed commit for Bamboo filesystem
//! memory snapshots.
//!
//! This adapter deliberately lives outside `jiandu-core` and `jiandu-store`.
//! Bamboo source DTOs never become Jiandu domain or persistence contracts.

use jiandu_core::{
    Confidence, ContentDigest, CreationActor, Etag, ExtractionMethod, ExtractionProvenance,
    IdempotencyKey, MemoryFrontmatterV1Alpha1, MemoryId, MemoryRecord, MemoryRelation,
    MemorySchema, MemoryScope, MemoryStatus, MemoryType, PrincipalId, ProjectId, Provenance,
    RelationKind, Revision, SessionId, SourceUri, Tag, Timestamp, TrustedRequestContext, Validate,
};
use jiandu_store::{
    AuditSequence, CanonicalStore, ImportClassification, ImportCommit, PortableExportBundle,
    SnapshotWatermark, StoreError, StoreErrorCode, StoreId, canonical_record_from_document_parts,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Strict reviewed-plan format owned by this adapter.
pub const BAMBOO_IMPORT_PLAN_FORMAT: &str = "jiandu.bamboo-import-plan/v1alpha1";
/// Strict authorized migration-report format owned by this adapter.
pub const BAMBOO_MIGRATION_REPORT_FORMAT: &str = "jiandu.bamboo-migration-report/v1alpha1";
/// Body/path-free cutover and rollback evidence format.
pub const BAMBOO_CUTOVER_EVIDENCE_FORMAT: &str = "jiandu.bamboo-cutover-evidence/v1alpha1";

const CORPUS_SCHEMA: &str = "jiandu.dev/compatibility/bamboo-memory-fixture-corpus/v1alpha1";
const IDENTITY_MAP_SCHEMA: &str = "jiandu.dev/compatibility/bamboo-host-identity-map/v1alpha1";
const BAMBOO_SOURCE_REPOSITORY: &str = "bigduu/Bamboo-agent";
const BAMBOO_SOURCE_COMMIT: &str = "f362cc151a2ac097db01178449b9a733929a440f";
const BAMBOO_SOURCE_TREE: &str = "a824dfe7e7263c69304ff26ef5826b7abcb0085f";
const FROZEN_MANIFEST_NORMALIZED_SHA256: &str =
    "a10b2f02edaa4babe34bac2ad33e26e4563b63f817e0b472ced2a70b20900f1e";
const MAX_MANIFEST_BYTES: usize = 1_048_576;
const MAX_REPORT_BYTES: usize = 1_048_576;
const MAX_PLAN_BYTES: usize = 262_144;
const MAX_EVIDENCE_BYTES: usize = 262_144;
const MAX_SOURCE_FILES: usize = 1_000;
const MAX_SOURCE_ENTRIES: usize = 1_000;
const MAX_SOURCE_DEPTH: usize = 16;
const MAX_SOURCE_BYTES: usize = 67_108_864;
const PROJECTION_REVISION: u64 = 1;

/// Stable, path-free migration failure categories.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BambooImportError {
    InvalidSnapshot,
    UnsafeSnapshot,
    SourceDrift,
    InvalidReviewedPlan,
    StaleReviewedPlan,
    DestinationNotPristine,
    UnresolvedEligibleIdentity,
    ProtectedTombstoneResurrection,
    PlanNotCommittable,
    CommittedEvidenceUnavailable {
        transaction_id: String,
        import_result_digest: String,
        backup_metadata_digest: String,
        failure: StoreErrorCode,
    },
    Store(StoreErrorCode),
}

impl fmt::Display for BambooImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidSnapshot => "invalid Bamboo migration snapshot",
            Self::UnsafeSnapshot => "unsafe Bamboo migration snapshot entry",
            Self::SourceDrift => "Bamboo migration source changed",
            Self::InvalidReviewedPlan => "invalid reviewed Bamboo migration plan",
            Self::StaleReviewedPlan => "reviewed Bamboo migration plan is stale",
            Self::DestinationNotPristine => "Jiandu migration destination is not pristine",
            Self::UnresolvedEligibleIdentity => {
                "an import-eligible Bamboo record has unresolved identity or mapping"
            }
            Self::ProtectedTombstoneResurrection => {
                "Bamboo migration would resurrect a protected tombstone"
            }
            Self::PlanNotCommittable => "Bamboo migration plan is not committable",
            Self::CommittedEvidenceUnavailable { .. } => {
                "Bamboo migration committed, but validation/export evidence is temporarily unavailable"
            }
            Self::Store(_) => "Jiandu store rejected the Bamboo migration operation",
        })
    }
}

impl std::error::Error for BambooImportError {}

fn store_error(error: StoreError) -> BambooImportError {
    BambooImportError::Store(error.code())
}

mod contract;
pub use contract::*;
mod engine;
#[cfg(test)]
pub(crate) use engine::commit_bamboo_snapshot_with_hook;
pub use engine::{commit_bamboo_snapshot, plan_bamboo_snapshot};

#[derive(Clone)]
struct PreparedSnapshot {
    evidence: BambooSnapshotEvidence,
    cases: Vec<BambooReportCase>,
    source_counts: BambooSourceCounts,
    records: Vec<MemoryRecord>,
    eligible_record_count: usize,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CorpusManifest {
    schema: String,
    corpus_version: u32,
    source_snapshot: SourceSnapshotManifest,
    format_evidence: serde_json::Value,
    host_identity_mapping: SnapshotEntry,
    expected_artifacts: Vec<SnapshotEntry>,
    cases: Vec<ManifestCase>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceSnapshotManifest {
    repository: String,
    commit: String,
    tree: String,
    watermark_kind: String,
    native_watermark: Option<String>,
    snapshot_evidence_kind: String,
    mapping_contract_sha256: String,
    aggregate_sha256: String,
    entries: Vec<SnapshotEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SnapshotEntry {
    relative_path: String,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManifestCase {
    id: String,
    source_relative_path: String,
    artifact_kind: String,
    format_variant: String,
    encoding: String,
    outcome: BambooSourceOutcome,
    reason_code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expected_mapping: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HostIdentityMap {
    mapping_schema: String,
    principal_slots: Vec<PrincipalSlot>,
    projects: Vec<ProjectBinding>,
    sessions: Vec<SessionBinding>,
    record_type_overrides: Vec<RecordTypeOverride>,
    created_by_mappings: Vec<CreatedByMapping>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PrincipalSlot {
    source: String,
    target_principal_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProjectBinding {
    bamboo_project_id: String,
    declared_legacy_project_keys: Vec<String>,
    target_project_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionBinding {
    bamboo_session_id: String,
    target_session_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecordTypeOverride {
    source_memory_id: String,
    target_type: String,
    reason: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreatedByMapping {
    bamboo_kind: String,
    target_actor: String,
    #[serde(default)]
    requires_session_binding: bool,
    preserve_as_migration_evidence: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BambooDurableFrontmatter {
    id: String,
    title: String,
    #[serde(rename = "type")]
    memory_type: String,
    scope: String,
    #[serde(default)]
    project_key: Option<String>,
    #[serde(default)]
    granularity: Option<String>,
    status: String,
    #[serde(default)]
    freshness: Option<String>,
    #[serde(default)]
    confidence: Option<String>,
    created_at: String,
    updated_at: String,
    created_by: BambooActor,
    updated_by: BambooActor,
    #[serde(default)]
    sources: Vec<BambooSource>,
    #[serde(default)]
    relations: BambooRelations,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(rename = "retrieval", default)]
    _retrieval: BambooRetrieval,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BambooActor {
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    actor: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BambooSource {
    kind: String,
    id: String,
    #[serde(default)]
    message_range: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct BambooRelations {
    #[serde(default)]
    supersedes: Vec<String>,
    #[serde(default)]
    contradicted_by: Vec<String>,
    #[serde(default)]
    related: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct BambooRetrieval {
    #[serde(rename = "keywords", default)]
    _keywords: Vec<String>,
    #[serde(rename = "entities", default)]
    _entities: Vec<String>,
    #[serde(rename = "embedding_ready", default)]
    _embedding_ready: bool,
    #[serde(rename = "last_accessed_at", default)]
    _last_accessed_at: Option<String>,
}

struct CandidateTopic {
    case_id: String,
    source_relative_path: String,
    source_sha256: String,
    frontmatter: BambooDurableFrontmatter,
    body: String,
}

fn prepare_snapshot(root_path: &Path) -> Result<PreparedSnapshot, BambooImportError> {
    let root = ReadOnlySnapshotRoot::open(root_path)?;
    let manifest_bytes = root.read_regular("manifest.json", MAX_MANIFEST_BYTES)?;
    let manifest: CorpusManifest = decode_canonical_json(&manifest_bytes, MAX_MANIFEST_BYTES)?;
    validate_frozen_manifest_bytes(&manifest_bytes)?;
    validate_manifest(&manifest)?;
    let identity_bytes = root.read_regular(
        &manifest.host_identity_mapping.relative_path,
        MAX_MANIFEST_BYTES,
    )?;
    if sha256_hex(&identity_bytes) != manifest.host_identity_mapping.sha256
        || u64::try_from(identity_bytes.len()).ok() != Some(manifest.host_identity_mapping.bytes)
        || format!("sha256:{}", sha256_hex(&identity_bytes))
            != manifest.source_snapshot.mapping_contract_sha256
    {
        return Err(BambooImportError::SourceDrift);
    }
    let identity: HostIdentityMap = decode_canonical_json(&identity_bytes, MAX_MANIFEST_BYTES)?;
    validate_identity_map(&identity)?;

    let first_scan = root.scan_source()?;
    validate_source_scan(&manifest, &first_scan)?;
    let mut source_counts = BambooSourceCounts::default();
    let entries_by_path = manifest
        .source_snapshot
        .entries
        .iter()
        .map(|entry| {
            (
                entry
                    .relative_path
                    .strip_prefix("source/")
                    .unwrap_or_default(),
                entry,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut report_cases = Vec::with_capacity(manifest.cases.len());
    let mut candidates = Vec::new();
    for case in &manifest.cases {
        source_counts.observe(case.outcome)?;
        let entry = entries_by_path
            .get(case.source_relative_path.as_str())
            .ok_or(BambooImportError::InvalidSnapshot)?;
        let eligible = matches!(
            case.outcome,
            BambooSourceOutcome::Accepted | BambooSourceOutcome::Transformed
        );
        report_cases.push(BambooReportCase {
            case_id: case.id.clone(),
            source_relative_path: case.source_relative_path.clone(),
            source_sha256: format!("sha256:{}", entry.sha256),
            source_outcome: case.outcome,
            source_reason_code: case.reason_code.clone(),
            mapping_disposition: if eligible {
                BambooMappingDisposition::UnresolvedIdentity
            } else {
                BambooMappingDisposition::NotEligible
            },
            target_memory_id: None,
            target_scope: None,
            destination_classification: None,
            actor_evidence: None,
        });
        if eligible {
            let bytes = first_scan
                .files
                .get(case.source_relative_path.as_str())
                .ok_or(BambooImportError::InvalidSnapshot)?;
            let text =
                std::str::from_utf8(bytes).map_err(|_| BambooImportError::InvalidSnapshot)?;
            let (yaml, body) = split_bamboo_frontmatter(text)?;
            let frontmatter = serde_yaml_ng::from_str::<BambooDurableFrontmatter>(yaml)
                .map_err(|_| BambooImportError::InvalidSnapshot)?;
            candidates.push(CandidateTopic {
                case_id: case.id.clone(),
                source_relative_path: case.source_relative_path.clone(),
                source_sha256: entry.sha256.clone(),
                frontmatter,
                body: body.trim().to_owned(),
            });
        }
    }

    let eligible_record_count = candidates.len();
    let (records, mapping_rows) = map_candidates(&candidates, &identity, &manifest)?;
    let rows = mapping_rows
        .into_iter()
        .map(|row| (row.case_id.clone(), row))
        .collect::<BTreeMap<_, _>>();
    for report_case in &mut report_cases {
        if let Some(row) = rows.get(&report_case.case_id) {
            report_case.mapping_disposition = row.disposition;
            report_case.target_memory_id = row.memory_id.clone();
            report_case.target_scope = row.scope.clone();
            report_case.actor_evidence = row.actor_evidence.clone();
        }
    }
    report_cases.sort_by(|left, right| left.source_relative_path.cmp(&right.source_relative_path));

    let second_scan = root.scan_source()?;
    let second_manifest = root.read_regular("manifest.json", MAX_MANIFEST_BYTES)?;
    let second_identity = root.read_regular(
        &manifest.host_identity_mapping.relative_path,
        MAX_MANIFEST_BYTES,
    )?;
    root.recheck()?;
    if first_scan.entries != second_scan.entries
        || first_scan.directories != second_scan.directories
        || first_scan.files != second_scan.files
        || manifest_bytes != second_manifest
        || identity_bytes != second_identity
    {
        return Err(BambooImportError::SourceDrift);
    }
    let compatibility_manifest_sha256 = format!("sha256:{}", sha256_hex(&manifest_bytes));
    let projection_store_id = projection_store_id(
        &manifest.source_snapshot.aggregate_sha256,
        &manifest.source_snapshot.mapping_contract_sha256,
        &compatibility_manifest_sha256,
    )?;
    let evidence = BambooSnapshotEvidence {
        repository: manifest.source_snapshot.repository,
        commit: manifest.source_snapshot.commit,
        tree: manifest.source_snapshot.tree,
        watermark_kind: manifest.source_snapshot.watermark_kind,
        native_watermark: manifest.source_snapshot.native_watermark,
        snapshot_evidence_kind: manifest.source_snapshot.snapshot_evidence_kind,
        aggregate_sha256: manifest.source_snapshot.aggregate_sha256,
        mapping_contract_sha256: manifest.source_snapshot.mapping_contract_sha256,
        compatibility_manifest_sha256,
        projection_store_id,
        projection_snapshot: SnapshotWatermark {
            store_revision: jiandu_core::StoreRevision(PROJECTION_REVISION),
            audit_sequence: AuditSequence(0),
        },
    };
    Ok(PreparedSnapshot {
        evidence,
        cases: report_cases,
        source_counts,
        records,
        eligible_record_count,
    })
}

#[derive(Clone)]
struct MappingRow {
    case_id: String,
    disposition: BambooMappingDisposition,
    memory_id: Option<MemoryId>,
    scope: Option<MemoryScope>,
    actor_evidence: Option<BambooActorEvidence>,
}

struct MappedCandidate {
    case_id: String,
    source: BambooDurableFrontmatter,
    record: MemoryRecord,
    actor_evidence: BambooActorEvidence,
}

fn map_candidates(
    candidates: &[CandidateTopic],
    identity: &HostIdentityMap,
    manifest: &CorpusManifest,
) -> Result<(Vec<MemoryRecord>, Vec<MappingRow>), BambooImportError> {
    let mut id_counts = BTreeMap::<&str, usize>::new();
    for candidate in candidates {
        *id_counts
            .entry(candidate.frontmatter.id.as_str())
            .or_default() += 1;
    }
    let mut mapped = Vec::new();
    let mut rows = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let result = if id_counts.get(candidate.frontmatter.id.as_str()).copied() != Some(1) {
            Err(BambooMappingDisposition::InvalidMapping)
        } else {
            map_candidate(candidate, identity, manifest)
        };
        match result {
            Ok(value) => mapped.push(value),
            Err(disposition) => rows.push(MappingRow {
                case_id: candidate.case_id.clone(),
                disposition,
                memory_id: None,
                scope: None,
                actor_evidence: Some(actor_evidence(&candidate.frontmatter)),
            }),
        }
    }

    let mapped_ids = mapped
        .iter()
        .map(|candidate| {
            (
                candidate.record.id.as_str().to_owned(),
                candidate.record.id.clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut reverse_relations = BTreeMap::<String, Vec<MemoryRelation>>::new();
    for candidate in &mapped {
        for contradicting_id in &candidate.source.relations.contradicted_by {
            let source_id = mapped_ids
                .get(contradicting_id)
                .ok_or(BambooImportError::InvalidSnapshot)?;
            reverse_relations
                .entry(source_id.as_str().to_owned())
                .or_default()
                .push(MemoryRelation {
                    kind: RelationKind::Contradicts,
                    target_memory_id: candidate.record.id.clone(),
                });
        }
    }
    let mut records = Vec::with_capacity(mapped.len());
    for mut candidate in mapped {
        if let Some(extra) = reverse_relations.remove(candidate.record.id.as_str()) {
            candidate.record.relations.extend(extra);
        }
        candidate.record.relations.sort();
        if candidate
            .record
            .relations
            .windows(2)
            .any(|pair| pair[0] == pair[1])
        {
            rows.push(MappingRow {
                case_id: candidate.case_id,
                disposition: BambooMappingDisposition::InvalidMapping,
                memory_id: None,
                scope: None,
                actor_evidence: Some(candidate.actor_evidence),
            });
            continue;
        }
        let frontmatter = MemoryFrontmatterV1Alpha1::from_record(&candidate.record);
        match canonical_record_from_document_parts(&frontmatter, &candidate.record.body) {
            Ok(record) => {
                rows.push(MappingRow {
                    case_id: candidate.case_id,
                    disposition: BambooMappingDisposition::Ready,
                    memory_id: Some(record.id.clone()),
                    scope: Some(record.scope.clone()),
                    actor_evidence: Some(candidate.actor_evidence),
                });
                records.push(record);
            }
            Err(_) => rows.push(MappingRow {
                case_id: candidate.case_id,
                disposition: BambooMappingDisposition::InvalidMapping,
                memory_id: None,
                scope: None,
                actor_evidence: Some(candidate.actor_evidence),
            }),
        }
    }
    records.sort_by(|left, right| left.id.cmp(&right.id));
    rows.sort_by(|left, right| left.case_id.cmp(&right.case_id));
    Ok((records, rows))
}

fn map_candidate(
    candidate: &CandidateTopic,
    identity: &HostIdentityMap,
    manifest: &CorpusManifest,
) -> Result<MappedCandidate, BambooMappingDisposition> {
    let source = &candidate.frontmatter;
    let expected_filename = format!("{}.md", source.id);
    if candidate
        .source_relative_path
        .rsplit('/')
        .next()
        .is_none_or(|name| name != expected_filename)
    {
        return Err(BambooMappingDisposition::InvalidMapping);
    }
    let id =
        MemoryId::new(source.id.clone()).map_err(|_| BambooMappingDisposition::InvalidMapping)?;
    let scope = resolve_scope(source, identity)?;
    let memory_type = resolve_memory_type(source, identity)?;
    let status = resolve_status(&source.status)?;
    let tags = map_tags(source)?;
    let created_at = canonical_timestamp(&source.created_at)?;
    let updated_at = canonical_timestamp(&source.updated_at)?;
    let session_id = resolve_session_provenance(source, identity)?;
    validate_actor_policy(source, identity)?;
    let confidence = match source.confidence.as_deref() {
        None => None,
        Some("high") => {
            Some(Confidence::new(1.0).map_err(|_| BambooMappingDisposition::InvalidMapping)?)
        }
        Some("medium") => {
            Some(Confidence::new(0.5).map_err(|_| BambooMappingDisposition::InvalidMapping)?)
        }
        Some("low") => {
            Some(Confidence::new(0.25).map_err(|_| BambooMappingDisposition::InvalidMapping)?)
        }
        Some(_) => return Err(BambooMappingDisposition::InvalidMapping),
    };
    let mut relations = Vec::new();
    for target in &source.relations.supersedes {
        relations.push(MemoryRelation {
            kind: RelationKind::Supersedes,
            target_memory_id: MemoryId::new(target.clone())
                .map_err(|_| BambooMappingDisposition::InvalidMapping)?,
        });
    }
    for target in &source.relations.related {
        relations.push(MemoryRelation {
            kind: RelationKind::RelatedTo,
            target_memory_id: MemoryId::new(target.clone())
                .map_err(|_| BambooMappingDisposition::InvalidMapping)?,
        });
    }
    relations.sort();
    let extractor_prefix = manifest
        .source_snapshot
        .commit
        .get(..7)
        .ok_or(BambooMappingDisposition::InvalidMapping)?;
    let record = MemoryRecord {
        schema: MemorySchema::V1Alpha1,
        id,
        revision: Revision::new(1).map_err(|_| BambooMappingDisposition::InvalidMapping)?,
        etag: Etag::new("pending-canonical-etag")
            .map_err(|_| BambooMappingDisposition::InvalidMapping)?,
        scope,
        memory_type,
        status,
        title: source.title.clone(),
        summary: None,
        body: candidate.body.clone(),
        tags,
        created_at,
        updated_at,
        provenance: Provenance {
            created_by: CreationActor::Import,
            agent_id: None,
            session_id,
            branch_id: None,
            message_ids: Vec::new(),
            message_range: None,
            source_uri: Some(
                SourceUri::new(format!(
                    "bamboo-memory://snapshot-v1/{}",
                    candidate.source_relative_path
                ))
                .map_err(|_| BambooMappingDisposition::InvalidMapping)?,
            ),
            content_digest: Some(
                ContentDigest::new(format!("sha256:{}", candidate.source_sha256))
                    .map_err(|_| BambooMappingDisposition::InvalidMapping)?,
            ),
            extraction: Some(ExtractionProvenance {
                method: ExtractionMethod::Import,
                extractor_version: Some(format!("bamboo-{extractor_prefix}")),
            }),
            confidence,
        },
        relations,
    };
    record
        .validate()
        .map_err(|_| BambooMappingDisposition::InvalidMapping)?;
    Ok(MappedCandidate {
        case_id: candidate.case_id.clone(),
        source: source.clone(),
        record,
        actor_evidence: actor_evidence(source),
    })
}

fn resolve_scope(
    source: &BambooDurableFrontmatter,
    identity: &HostIdentityMap,
) -> Result<MemoryScope, BambooMappingDisposition> {
    match source.scope.as_str() {
        "global" => {
            let matches = identity
                .principal_slots
                .iter()
                .filter(|slot| slot.source == "bamboo-single-user-global")
                .collect::<Vec<_>>();
            if matches.len() != 1 {
                return Err(BambooMappingDisposition::UnresolvedIdentity);
            }
            Ok(MemoryScope::Principal {
                principal_id: PrincipalId::new(matches[0].target_principal_id.clone())
                    .map_err(|_| BambooMappingDisposition::UnresolvedIdentity)?,
            })
        }
        "project" => {
            let project_key = source
                .project_key
                .as_deref()
                .ok_or(BambooMappingDisposition::UnresolvedIdentity)?;
            let matches = identity
                .projects
                .iter()
                .filter(|binding| {
                    binding.bamboo_project_id == project_key
                        || binding
                            .declared_legacy_project_keys
                            .iter()
                            .any(|key| key == project_key)
                })
                .collect::<Vec<_>>();
            if matches.len() != 1 {
                return Err(BambooMappingDisposition::UnresolvedIdentity);
            }
            Ok(MemoryScope::Project {
                project_id: ProjectId::new(matches[0].target_project_id.clone())
                    .map_err(|_| BambooMappingDisposition::UnresolvedIdentity)?,
            })
        }
        _ => Err(BambooMappingDisposition::UnresolvedIdentity),
    }
}

fn resolve_memory_type(
    source: &BambooDurableFrontmatter,
    identity: &HostIdentityMap,
) -> Result<MemoryType, BambooMappingDisposition> {
    match source.memory_type.as_str() {
        "feedback" => Ok(MemoryType::Feedback),
        "project" => Ok(MemoryType::Project),
        "reference" => Ok(MemoryType::Reference),
        "user" => {
            let matches = identity
                .record_type_overrides
                .iter()
                .filter(|mapping| mapping.source_memory_id == source.id)
                .collect::<Vec<_>>();
            if matches.len() != 1 {
                return Err(BambooMappingDisposition::UnresolvedIdentity);
            }
            match matches[0].target_type.as_str() {
                "preference" => Ok(MemoryType::Preference),
                "decision" => Ok(MemoryType::Decision),
                "fact" => Ok(MemoryType::Fact),
                _ => Err(BambooMappingDisposition::UnresolvedIdentity),
            }
        }
        _ => Err(BambooMappingDisposition::InvalidMapping),
    }
}

fn resolve_status(value: &str) -> Result<MemoryStatus, BambooMappingDisposition> {
    match value {
        "active" => Ok(MemoryStatus::Active),
        "stale" => Ok(MemoryStatus::Stale),
        "superseded" => Ok(MemoryStatus::Superseded),
        "contradicted" => Ok(MemoryStatus::Contradicted),
        "archived" => Ok(MemoryStatus::Archived),
        _ => Err(BambooMappingDisposition::InvalidMapping),
    }
}

fn resolve_session_provenance(
    source: &BambooDurableFrontmatter,
    identity: &HostIdentityMap,
) -> Result<Option<SessionId>, BambooMappingDisposition> {
    let source_id = source
        .created_by
        .id
        .as_deref()
        .filter(|_| source.created_by.kind == "session")
        .or_else(|| {
            source
                .sources
                .iter()
                .find(|source| source.kind == "session")
                .map(|source| source.id.as_str())
        });
    let Some(source_id) = source_id else {
        return Ok(None);
    };
    let matches = identity
        .sessions
        .iter()
        .filter(|binding| binding.bamboo_session_id == source_id)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(BambooMappingDisposition::UnresolvedIdentity);
    }
    Ok(Some(
        SessionId::new(matches[0].target_session_id.clone())
            .map_err(|_| BambooMappingDisposition::UnresolvedIdentity)?,
    ))
}

fn validate_actor_policy(
    source: &BambooDurableFrontmatter,
    identity: &HostIdentityMap,
) -> Result<(), BambooMappingDisposition> {
    let matches = identity
        .created_by_mappings
        .iter()
        .filter(|mapping| mapping.bamboo_kind == source.created_by.kind)
        .collect::<Vec<_>>();
    if matches.len() != 1
        || matches[0].target_actor != "import"
        || !matches[0].preserve_as_migration_evidence
    {
        return Err(BambooMappingDisposition::UnresolvedIdentity);
    }
    if matches[0].requires_session_binding && source.created_by.id.is_some() {
        let id = source.created_by.id.as_deref().unwrap_or_default();
        if identity
            .sessions
            .iter()
            .filter(|binding| binding.bamboo_session_id == id)
            .count()
            != 1
        {
            return Err(BambooMappingDisposition::UnresolvedIdentity);
        }
    }
    Ok(())
}

fn map_tags(source: &BambooDurableFrontmatter) -> Result<Vec<Tag>, BambooMappingDisposition> {
    let mut values = BTreeSet::new();
    for source_tag in &source.tags {
        let normalized = normalize_tag(source_tag)?;
        if !values.insert(normalized) {
            return Err(BambooMappingDisposition::InvalidMapping);
        }
    }
    if let Some(granularity) = &source.granularity {
        let normalized = normalize_tag(granularity)?;
        if !values.insert(format!("bamboo:granularity:{normalized}")) {
            return Err(BambooMappingDisposition::InvalidMapping);
        }
    }
    if let Some(freshness) = &source.freshness {
        let normalized = normalize_tag(freshness)?;
        if !values.insert(format!("bamboo:freshness:{normalized}")) {
            return Err(BambooMappingDisposition::InvalidMapping);
        }
    }
    values
        .into_iter()
        .map(|value| Tag::new(value).map_err(|_| BambooMappingDisposition::InvalidMapping))
        .collect()
}

fn normalize_tag(value: &str) -> Result<String, BambooMappingDisposition> {
    let value = value.trim();
    if value.is_empty() || !value.is_ascii() {
        return Err(BambooMappingDisposition::InvalidMapping);
    }
    let mut result = String::new();
    let mut separator = false;
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':') {
            if separator && !result.is_empty() {
                result.push('-');
            }
            separator = false;
            result.push(char::from(byte.to_ascii_lowercase()));
        } else if byte == b'-' || byte.is_ascii_whitespace() {
            separator = true;
        } else {
            return Err(BambooMappingDisposition::InvalidMapping);
        }
    }
    if result.is_empty() {
        return Err(BambooMappingDisposition::InvalidMapping);
    }
    Ok(result)
}

fn canonical_timestamp(value: &str) -> Result<Timestamp, BambooMappingDisposition> {
    let parsed = OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|_| BambooMappingDisposition::InvalidMapping)?;
    let canonical = parsed
        .to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)
        .map_err(|_| BambooMappingDisposition::InvalidMapping)?;
    Timestamp::new(canonical).map_err(|_| BambooMappingDisposition::InvalidMapping)
}

fn actor_evidence(source: &BambooDurableFrontmatter) -> BambooActorEvidence {
    BambooActorEvidence {
        created_by_kind: source.created_by.kind.clone(),
        created_by_id: source.created_by.id.clone(),
        updated_by_kind: source.updated_by.kind.clone(),
        updated_by_actor: source.updated_by.actor.clone(),
        sources: source
            .sources
            .iter()
            .map(|source| BambooRawSourceEvidence {
                kind: source.kind.clone(),
                id: source.id.clone(),
                message_range: source.message_range.clone(),
            })
            .collect(),
    }
}

fn split_bamboo_frontmatter(text: &str) -> Result<(&str, &str), BambooImportError> {
    let text = text.trim_start_matches('\u{feff}');
    if text.contains('\r') {
        return Err(BambooImportError::InvalidSnapshot);
    }
    let rest = text
        .strip_prefix("---\n")
        .ok_or(BambooImportError::InvalidSnapshot)?;
    let (yaml, body) = rest
        .split_once("\n---\n")
        .ok_or(BambooImportError::InvalidSnapshot)?;
    if body.trim().is_empty() {
        return Err(BambooImportError::InvalidSnapshot);
    }
    Ok((yaml, body))
}

fn validate_manifest(manifest: &CorpusManifest) -> Result<(), BambooImportError> {
    if manifest.schema != CORPUS_SCHEMA
        || manifest.corpus_version != 1
        || manifest.source_snapshot.repository != BAMBOO_SOURCE_REPOSITORY
        || manifest.source_snapshot.commit != BAMBOO_SOURCE_COMMIT
        || manifest.source_snapshot.tree != BAMBOO_SOURCE_TREE
        || manifest.source_snapshot.native_watermark.is_some()
        || manifest.source_snapshot.snapshot_evidence_kind != "immutable_file_manifest"
        || manifest.source_snapshot.watermark_kind != "sorted_relative_path_size_sha256"
        || !valid_content_digest(&manifest.source_snapshot.aggregate_sha256)
        || !valid_content_digest(&manifest.source_snapshot.mapping_contract_sha256)
        || manifest.source_snapshot.entries.is_empty()
        || manifest.source_snapshot.entries.len() > MAX_SOURCE_FILES
        || manifest.cases.len() != manifest.source_snapshot.entries.len()
        || manifest.host_identity_mapping.relative_path != "expected/host-identity-map.json"
    {
        return Err(BambooImportError::InvalidSnapshot);
    }
    let mut previous_entry = None;
    let mut total_bytes = 0u64;
    for entry in &manifest.source_snapshot.entries {
        if !entry.relative_path.starts_with("source/")
            || !portable_relative_path(&entry.relative_path)
            || !valid_hex_digest(&entry.sha256)
            || previous_entry.is_some_and(|previous: &str| previous >= entry.relative_path.as_str())
        {
            return Err(BambooImportError::InvalidSnapshot);
        }
        total_bytes = total_bytes
            .checked_add(entry.bytes)
            .ok_or(BambooImportError::InvalidSnapshot)?;
        previous_entry = Some(entry.relative_path.as_str());
    }
    if total_bytes > MAX_SOURCE_BYTES as u64 {
        return Err(BambooImportError::InvalidSnapshot);
    }
    let expected_identity = manifest
        .expected_artifacts
        .iter()
        .filter(|entry| entry.relative_path == "expected/host-identity-map.json")
        .collect::<Vec<_>>();
    if expected_identity.as_slice() != [&manifest.host_identity_mapping]
        || !valid_hex_digest(&manifest.host_identity_mapping.sha256)
        || manifest.host_identity_mapping.bytes > MAX_MANIFEST_BYTES as u64
    {
        return Err(BambooImportError::InvalidSnapshot);
    }
    let case_paths = manifest
        .cases
        .iter()
        .map(|case| case.source_relative_path.as_str())
        .collect::<BTreeSet<_>>();
    let entry_paths = manifest
        .source_snapshot
        .entries
        .iter()
        .filter_map(|entry| entry.relative_path.strip_prefix("source/"))
        .collect::<BTreeSet<_>>();
    let case_ids = manifest
        .cases
        .iter()
        .map(|case| case.id.as_str())
        .collect::<BTreeSet<_>>();
    if case_paths != entry_paths || case_ids.len() != manifest.cases.len() {
        return Err(BambooImportError::InvalidSnapshot);
    }
    for case in &manifest.cases {
        if case.id.is_empty()
            || !portable_relative_path(&case.source_relative_path)
            || case.reason_code.is_empty()
            || (matches!(
                case.outcome,
                BambooSourceOutcome::Accepted | BambooSourceOutcome::Transformed
            ) && (case.artifact_kind != "durable_topic" || case.expected_mapping.is_none()))
        {
            return Err(BambooImportError::InvalidSnapshot);
        }
    }
    let aggregate = aggregate_manifest_entries(&manifest.source_snapshot.entries)?;
    if aggregate != manifest.source_snapshot.aggregate_sha256 {
        return Err(BambooImportError::InvalidSnapshot);
    }
    Ok(())
}

fn validate_frozen_manifest_bytes(bytes: &[u8]) -> Result<(), BambooImportError> {
    const ZERO_DIGEST: &str =
        "sha256:0000000000000000000000000000000000000000000000000000000000000000";
    const ZERO_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    let mut value = serde_json::from_slice::<serde_json::Value>(bytes)
        .map_err(|_| BambooImportError::InvalidSnapshot)?;
    *value
        .pointer_mut("/sourceSnapshot/mappingContractSha256")
        .ok_or(BambooImportError::InvalidSnapshot)? =
        serde_json::Value::String(ZERO_DIGEST.to_owned());
    *value
        .pointer_mut("/hostIdentityMapping/bytes")
        .ok_or(BambooImportError::InvalidSnapshot)? = serde_json::Value::Number(0_u64.into());
    *value
        .pointer_mut("/hostIdentityMapping/sha256")
        .ok_or(BambooImportError::InvalidSnapshot)? =
        serde_json::Value::String(ZERO_HEX.to_owned());
    let expected = value
        .get_mut("expectedArtifacts")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or(BambooImportError::InvalidSnapshot)?;
    let mut matches = expected.iter_mut().filter(|entry| {
        entry
            .get("relativePath")
            .and_then(serde_json::Value::as_str)
            == Some("expected/host-identity-map.json")
    });
    let mapping = matches.next().ok_or(BambooImportError::InvalidSnapshot)?;
    if matches.next().is_some() {
        return Err(BambooImportError::InvalidSnapshot);
    }
    *mapping
        .get_mut("bytes")
        .ok_or(BambooImportError::InvalidSnapshot)? = serde_json::Value::Number(0_u64.into());
    *mapping
        .get_mut("sha256")
        .ok_or(BambooImportError::InvalidSnapshot)? =
        serde_json::Value::String(ZERO_HEX.to_owned());
    let mut normalized =
        serde_json::to_vec_pretty(&value).map_err(|_| BambooImportError::InvalidSnapshot)?;
    normalized.push(b'\n');
    if sha256_hex(&normalized) != FROZEN_MANIFEST_NORMALIZED_SHA256 {
        return Err(BambooImportError::InvalidSnapshot);
    }
    Ok(())
}

fn validate_identity_map(identity: &HostIdentityMap) -> Result<(), BambooImportError> {
    if identity.mapping_schema != IDENTITY_MAP_SCHEMA {
        return Err(BambooImportError::InvalidSnapshot);
    }
    let unique_principals = identity
        .principal_slots
        .iter()
        .map(|slot| slot.source.as_str())
        .collect::<BTreeSet<_>>()
        .len()
        == identity.principal_slots.len();
    let unique_projects = identity
        .projects
        .iter()
        .map(|binding| binding.bamboo_project_id.as_str())
        .collect::<BTreeSet<_>>()
        .len()
        == identity.projects.len();
    let unique_sessions = identity
        .sessions
        .iter()
        .map(|binding| binding.bamboo_session_id.as_str())
        .collect::<BTreeSet<_>>()
        .len()
        == identity.sessions.len();
    let unique_overrides = identity
        .record_type_overrides
        .iter()
        .map(|mapping| mapping.source_memory_id.as_str())
        .collect::<BTreeSet<_>>()
        .len()
        == identity.record_type_overrides.len();
    if !unique_principals || !unique_projects || !unique_sessions || !unique_overrides {
        return Err(BambooImportError::InvalidSnapshot);
    }
    for slot in &identity.principal_slots {
        PrincipalId::new(slot.target_principal_id.clone())
            .map_err(|_| BambooImportError::InvalidSnapshot)?;
    }
    for project in &identity.projects {
        ProjectId::new(project.target_project_id.clone())
            .map_err(|_| BambooImportError::InvalidSnapshot)?;
        if project
            .declared_legacy_project_keys
            .iter()
            .any(|key| key.is_empty())
        {
            return Err(BambooImportError::InvalidSnapshot);
        }
    }
    for session in &identity.sessions {
        SessionId::new(session.target_session_id.clone())
            .map_err(|_| BambooImportError::InvalidSnapshot)?;
    }
    if identity
        .record_type_overrides
        .iter()
        .any(|mapping| mapping.reason.is_empty())
        || identity.created_by_mappings.iter().any(|mapping| {
            mapping.bamboo_kind.is_empty()
                || mapping.target_actor != "import"
                || !mapping.preserve_as_migration_evidence
        })
    {
        return Err(BambooImportError::InvalidSnapshot);
    }
    Ok(())
}

fn validate_source_scan(
    manifest: &CorpusManifest,
    scan: &SnapshotScan,
) -> Result<(), BambooImportError> {
    if scan.entries.len() != manifest.source_snapshot.entries.len() {
        return Err(BambooImportError::SourceDrift);
    }
    for (actual, expected) in scan.entries.iter().zip(&manifest.source_snapshot.entries) {
        if format!("source/{}", actual.relative_path) != expected.relative_path
            || actual.bytes != expected.bytes
            || actual.sha256 != expected.sha256
        {
            return Err(BambooImportError::SourceDrift);
        }
    }
    let prefixed = scan
        .entries
        .iter()
        .map(|entry| SnapshotEntry {
            relative_path: format!("source/{}", entry.relative_path),
            bytes: entry.bytes,
            sha256: entry.sha256.clone(),
        })
        .collect::<Vec<_>>();
    if aggregate_manifest_entries(&prefixed)? != manifest.source_snapshot.aggregate_sha256 {
        return Err(BambooImportError::SourceDrift);
    }
    let mut expected_directories = BTreeSet::new();
    for entry in &manifest.source_snapshot.entries {
        let mut relative = entry
            .relative_path
            .strip_prefix("source/")
            .ok_or(BambooImportError::InvalidSnapshot)?;
        while let Some((parent, _)) = relative.rsplit_once('/') {
            expected_directories.insert(parent.to_owned());
            relative = parent;
        }
    }
    if scan.directories != expected_directories.into_iter().collect::<Vec<_>>() {
        return Err(BambooImportError::SourceDrift);
    }
    Ok(())
}

mod snapshot;
use snapshot::*;

#[cfg(test)]
mod tests;

fn aggregate_manifest_entries(entries: &[SnapshotEntry]) -> Result<String, BambooImportError> {
    let mut hasher = Sha256::new();
    for entry in entries {
        hasher.update(entry.relative_path.as_bytes());
        hasher.update([0]);
        hasher.update(entry.bytes.to_string().as_bytes());
        hasher.update([0]);
        hasher.update(entry.sha256.as_bytes());
        hasher.update(b"\n");
    }
    Ok(format!("sha256:{}", hex_digest(&hasher.finalize())))
}

fn projection_store_id(
    aggregate: &str,
    mapping: &str,
    manifest: &str,
) -> Result<StoreId, BambooImportError> {
    let mut hasher = Sha256::new();
    hasher.update(b"jiandu/bamboo-portable-projection/v1\0");
    hasher.update(aggregate.as_bytes());
    hasher.update([0]);
    hasher.update(mapping.as_bytes());
    hasher.update([0]);
    hasher.update(manifest.as_bytes());
    let mut bytes: [u8; 16] = hasher.finalize()[..16]
        .try_into()
        .map_err(|_| BambooImportError::InvalidSnapshot)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let value = format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    );
    StoreId::new(value).map_err(store_error)
}

fn digest_payload(domain: &[u8], value: &impl Serialize) -> Result<String, BambooImportError> {
    let payload = serde_json::to_vec(value).map_err(|_| BambooImportError::InvalidReviewedPlan)?;
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(payload);
    Ok(format!("sha256:{}", hex_digest(&hasher.finalize())))
}

fn canonical_json(value: &impl Serialize, maximum: usize) -> Result<Vec<u8>, BambooImportError> {
    let mut bytes =
        serde_json::to_vec_pretty(value).map_err(|_| BambooImportError::InvalidReviewedPlan)?;
    bytes.push(b'\n');
    if bytes.len() > maximum {
        return Err(BambooImportError::InvalidReviewedPlan);
    }
    Ok(bytes)
}

fn decode_canonical<T>(
    bytes: &[u8],
    maximum: usize,
    validate: fn(&T) -> Result<(), BambooImportError>,
) -> Result<T, BambooImportError>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
    if bytes.len() > maximum {
        return Err(BambooImportError::InvalidReviewedPlan);
    }
    let value =
        serde_json::from_slice::<T>(bytes).map_err(|_| BambooImportError::InvalidReviewedPlan)?;
    validate(&value)?;
    if canonical_json(&value, maximum)? != bytes {
        return Err(BambooImportError::InvalidReviewedPlan);
    }
    Ok(value)
}

fn decode_canonical_json<T>(bytes: &[u8], maximum: usize) -> Result<T, BambooImportError>
where
    T: for<'de> Deserialize<'de>,
{
    if bytes.len() > maximum {
        return Err(BambooImportError::InvalidSnapshot);
    }
    let value = serde_json::from_slice::<serde_json::Value>(bytes)
        .map_err(|_| BambooImportError::InvalidSnapshot)?;
    let mut canonical =
        serde_json::to_vec_pretty(&value).map_err(|_| BambooImportError::InvalidSnapshot)?;
    canonical.push(b'\n');
    if canonical != bytes {
        return Err(BambooImportError::InvalidSnapshot);
    }
    serde_json::from_value(value).map_err(|_| BambooImportError::InvalidSnapshot)
}

fn portable_relative_path(value: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && !path.is_absolute()
        && !value.contains('\\')
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn valid_content_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(valid_hex_digest)
}

fn valid_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex_digest(&Sha256::digest(bytes))
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(char::from(HEX[usize::from(byte >> 4)]));
        result.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    result
}
