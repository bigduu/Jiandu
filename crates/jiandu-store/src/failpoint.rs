//! Deterministic persistence-boundary fault injection.
//!
//! Production stores use the no-op injector. Tests can provide an injector to
//! stop an operation immediately after any named filesystem boundary, then
//! reopen the store and exercise the same startup recovery path used after a
//! process or power failure.

use std::sync::Arc;

/// A completed persistence boundary at which a crash may be simulated.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum PersistenceBoundary {
    ManifestTempWritten,
    ManifestTempSynced,
    ManifestTempDirectorySynced,
    ManifestPublished,
    ManifestDirectorySynced,
    RecordNamespacePrepared,
    RecordTempWritten,
    RecordTempSynced,
    RecordTempDirectorySynced,
    MetadataTempWritten,
    MetadataTempSynced,
    MetadataTempDirectorySynced,
    IdempotencyNamespacePrepared,
    MutationResultTempWritten,
    MutationResultTempSynced,
    MutationResultTempDirectorySynced,
    MutationReceiptTempWritten,
    MutationReceiptTempSynced,
    MutationReceiptTempDirectorySynced,
    MutationAuditTempWritten,
    MutationAuditTempSynced,
    MutationAuditTempDirectorySynced,
    RecordRenamed,
    RecordDirectorySynced,
    MutationResultPublished,
    MutationResultDirectorySynced,
    MutationReceiptPublished,
    MutationReceiptDirectorySynced,
    MutationAuditPublished,
    MutationAuditDirectorySynced,
    MetadataRenamed,
    MetadataDirectorySynced,
    QuarantineRenamed,
    QuarantineDirectorySynced,
    QuarantineSourceDirectorySynced,
    QuarantineReceiptTempWritten,
    QuarantineReceiptTempSynced,
    QuarantineReceiptTempDirectorySynced,
    QuarantineReceiptPublished,
    QuarantineReceiptDirectorySynced,
    QuarantineReceiptAcknowledgementRemoved,
    QuarantineReceiptAcknowledgementDirectorySynced,
    QuarantineReceiptLayoutCreated,
    QuarantineReceiptLayoutDirectorySynced,
    ManifestRemoved,
    ManifestRemovalDirectorySynced,
    RecoveryRecordDirectorySynced,
    RecoveryMetadataDirectorySynced,
    RecoveryIdempotencyNamespacePrepared,
    RecoveryMutationResultDirectorySynced,
    RecoveryMutationReceiptDirectorySynced,
    RecoveryMutationAuditDirectorySynced,
    RecoveryQuarantineDirectorySynced,
    RecoveryQuarantineSourceDirectorySynced,
    RecoveryReceiptDirectorySynced,
    RecoveryManifestDirectorySynced,
    MigrationLayoutSynced,
    MigrationGenesisTempWritten,
    MigrationGenesisTempSynced,
    MigrationGenesisTempDirectorySynced,
    MigrationGenesisPublished,
    MigrationGenesisDirectorySynced,
    MigrationMetadataTempWritten,
    MigrationMetadataTempSynced,
    MigrationMetadataTempDirectorySynced,
    MigrationMetadataPublished,
    MigrationMetadataDirectorySynced,
    DurabilityProbeFilesSynced,
    DurabilityProbeRenamed,
    DurabilityProbeDirectorySynced,
}

impl PersistenceBoundary {
    /// Exhaustive internal conformance list. Tests compare their exercised
    /// boundary set against this slice so new persistence steps cannot be
    /// introduced silently.
    pub const ALL: &'static [Self] = &[
        Self::ManifestTempWritten,
        Self::ManifestTempSynced,
        Self::ManifestTempDirectorySynced,
        Self::ManifestPublished,
        Self::ManifestDirectorySynced,
        Self::RecordNamespacePrepared,
        Self::RecordTempWritten,
        Self::RecordTempSynced,
        Self::RecordTempDirectorySynced,
        Self::MetadataTempWritten,
        Self::MetadataTempSynced,
        Self::MetadataTempDirectorySynced,
        Self::IdempotencyNamespacePrepared,
        Self::MutationResultTempWritten,
        Self::MutationResultTempSynced,
        Self::MutationResultTempDirectorySynced,
        Self::MutationReceiptTempWritten,
        Self::MutationReceiptTempSynced,
        Self::MutationReceiptTempDirectorySynced,
        Self::MutationAuditTempWritten,
        Self::MutationAuditTempSynced,
        Self::MutationAuditTempDirectorySynced,
        Self::RecordRenamed,
        Self::RecordDirectorySynced,
        Self::MutationResultPublished,
        Self::MutationResultDirectorySynced,
        Self::MutationReceiptPublished,
        Self::MutationReceiptDirectorySynced,
        Self::MutationAuditPublished,
        Self::MutationAuditDirectorySynced,
        Self::MetadataRenamed,
        Self::MetadataDirectorySynced,
        Self::QuarantineRenamed,
        Self::QuarantineDirectorySynced,
        Self::QuarantineSourceDirectorySynced,
        Self::QuarantineReceiptTempWritten,
        Self::QuarantineReceiptTempSynced,
        Self::QuarantineReceiptTempDirectorySynced,
        Self::QuarantineReceiptPublished,
        Self::QuarantineReceiptDirectorySynced,
        Self::QuarantineReceiptAcknowledgementRemoved,
        Self::QuarantineReceiptAcknowledgementDirectorySynced,
        Self::QuarantineReceiptLayoutCreated,
        Self::QuarantineReceiptLayoutDirectorySynced,
        Self::ManifestRemoved,
        Self::ManifestRemovalDirectorySynced,
        Self::RecoveryRecordDirectorySynced,
        Self::RecoveryMetadataDirectorySynced,
        Self::RecoveryIdempotencyNamespacePrepared,
        Self::RecoveryMutationResultDirectorySynced,
        Self::RecoveryMutationReceiptDirectorySynced,
        Self::RecoveryMutationAuditDirectorySynced,
        Self::RecoveryQuarantineDirectorySynced,
        Self::RecoveryQuarantineSourceDirectorySynced,
        Self::RecoveryReceiptDirectorySynced,
        Self::RecoveryManifestDirectorySynced,
        Self::MigrationLayoutSynced,
        Self::MigrationGenesisTempWritten,
        Self::MigrationGenesisTempSynced,
        Self::MigrationGenesisTempDirectorySynced,
        Self::MigrationGenesisPublished,
        Self::MigrationGenesisDirectorySynced,
        Self::MigrationMetadataTempWritten,
        Self::MigrationMetadataTempSynced,
        Self::MigrationMetadataTempDirectorySynced,
        Self::MigrationMetadataPublished,
        Self::MigrationMetadataDirectorySynced,
        Self::DurabilityProbeFilesSynced,
        Self::DurabilityProbeRenamed,
        Self::DurabilityProbeDirectorySynced,
    ];
}

/// Test seam for deterministic crash-boundary coverage.
///
/// Implementations must not perform store I/O themselves. Returning `true`
/// asks the store to stop with a secret-safe `InjectedFailure` and poison the
/// current handle until it is dropped and reopened.
pub trait PersistenceFailpointInjector: Send + Sync {
    fn should_fail(&self, boundary: PersistenceBoundary) -> bool;
}

#[derive(Debug)]
struct NoFailpoints;

impl PersistenceFailpointInjector for NoFailpoints {
    fn should_fail(&self, _boundary: PersistenceBoundary) -> bool {
        false
    }
}

#[derive(Clone)]
pub(crate) struct Failpoints(Arc<dyn PersistenceFailpointInjector>);

impl Default for Failpoints {
    fn default() -> Self {
        Self(Arc::new(NoFailpoints))
    }
}

impl Failpoints {
    pub(crate) fn new(injector: Arc<dyn PersistenceFailpointInjector>) -> Self {
        Self(injector)
    }

    pub(crate) fn check(&self, boundary: PersistenceBoundary) -> Result<(), crate::StoreError> {
        if self.0.should_fail(boundary) {
            Err(crate::StoreError::InjectedFailure { boundary })
        } else {
            Ok(())
        }
    }
}
