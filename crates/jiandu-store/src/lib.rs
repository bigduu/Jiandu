//! Exclusive filesystem ownership and validated canonical reads for Jiandu.
//!
//! This crate owns storage paths internally. Its public read API accepts only
//! opaque Jiandu identities and authoritative scope grants; it never exposes a
//! canonical path or derives Project identity from a path.

mod cursor;
mod document;
mod error;
mod layout;
mod lock;
mod metadata;
mod store;

pub use error::{InvalidRecordReason, StoreError, StoreErrorCode};
pub use lock::{LockOwner, LockOwnerDiagnostics};
pub use metadata::{STORE_FORMAT_VERSION, StoreId, StoreMetadata};
pub use store::{AuthorizedScopes, CanonicalStore, QuarantineReceipt, StoreRead, StoreWatermark};

#[cfg(test)]
mod tests;
