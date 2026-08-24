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
mod layout;
mod lock;
mod metadata;
mod mutation;
mod recovery;
mod store;
mod transaction;

pub use durability::{DirectoryDurability, StoreDoctorReport};
pub use error::{InvalidRecordReason, StoreError, StoreErrorCode};
pub use failpoint::{PersistenceBoundary, PersistenceFailpointInjector};
pub use lock::{LockOwner, LockOwnerDiagnostics};
pub use metadata::{STORE_FORMAT_VERSION, StoreId, StoreMetadata};
pub use mutation::{CreateMemoryInput, MutationCommit};
pub use store::{
    AuthorizedScope, AuthorizedScopes, CanonicalStore, QuarantineReceipt, StoreOptions, StoreRead,
    StoreWatermark,
};

#[cfg(test)]
mod tests;
