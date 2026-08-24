//! Exclusive canonical filesystem ownership, validated reads, atomic CAS, and
//! deterministic crash recovery for Jiandu.
//!
//! This crate owns storage paths internally. Its public APIs accept only opaque
//! Jiandu identities and authoritative scope grants; they never expose a
//! canonical path or derive Project identity from a path.

mod cursor;
mod document;
mod durability;
mod error;
mod failpoint;
mod idempotency;
mod inspection;
mod layout;
mod lock;
mod metadata;
mod mutation;
mod recovery;
mod store;
mod tombstone;
mod transaction;

pub use durability::{DirectoryDurability, StoreDoctorReport};
pub use error::{InvalidRecordReason, StoreError, StoreErrorCode};
pub use failpoint::{PersistenceBoundary, PersistenceFailpointInjector};
pub use idempotency::MutationOperation;
pub use inspection::{
    AuthorizedExportAdmin, AuthorizedValidationAdmin, ExportDigest, PORTABLE_EXPORT_FORMAT_VERSION,
    PORTABLE_TOMBSTONE_FORMAT_VERSION, PortableExportBundle, PortableMemoryRecord,
    PortableProvenance, PortableTombstone, ReadOnlyStoreInspector, SnapshotWatermark,
    VALIDATION_REPORT_FORMAT_VERSION, ValidationArtifact, ValidationCode, ValidationFinding,
    ValidationMode, ValidationReport, generated_inspection_schemas,
};
pub use lock::{LockOwner, LockOwnerDiagnostics};
pub use metadata::{AuditSequence, STORE_FORMAT_VERSION, StoreId, StoreMetadata};
pub use mutation::{ForgetCommit, MutationCommit};
pub use store::{
    AuthorizedAdmin, AuthorizedMutation, AuthorizedScope, AuthorizedScopes, CanonicalStore,
    QuarantineReceipt, StoreOptions, StoreRead, StoreWatermark,
};
pub use tombstone::{AdminAction, AdminActionPlan, AdminPlanTarget};

#[cfg(test)]
mod tests;
