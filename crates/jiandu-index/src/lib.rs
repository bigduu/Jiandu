//! Deterministic, disposable lexical retrieval derived from canonical Jiandu
//! records.
//!
//! Canonical storage remains authoritative. This crate receives only
//! path-free, authorization-resolved snapshots and can always be deleted and
//! rebuilt without changing a memory record.

use jiandu_store::{
    AuthorizedIndexAdmin, CanonicalIndexSnapshot, CanonicalStore, StoreError, StoreId,
    StoreWatermark,
};

mod cursor;
mod directory;
mod error;
mod format;
mod index;
mod tokenizer;

pub use cursor::CursorMacKey;
pub use error::{IndexDegradedReason, IndexError, IndexErrorCode};
pub use format::{
    BODY_WEIGHT, SCOPE_WEIGHT, STATUS_WEIGHT, SUMMARY_WEIGHT, TAG_WEIGHT, TITLE_WEIGHT,
    TYPE_WEIGHT, UPDATE_METADATA_WEIGHT,
};
pub use index::{
    IndexDiagnostic, IndexHealth, IndexReadiness, IndexRebuildReport, IndexWatermark, LexicalIndex,
};
pub use tokenizer::tokenize;

/// Current derived lexical-index format. It evolves independently from the
/// canonical store and public API formats.
pub const INDEX_FORMAT_VERSION: &str = "jiandu.index.lexical/v1alpha1";

/// Narrow read-only source used by rebuild and health checks.
pub trait CanonicalRecordReader {
    /// Read the complete authoritative record set for rebuilding the single
    /// all-store index. Implementations must return one stable watermark or
    /// fail without a partial snapshot.
    fn read_index_snapshot(
        &self,
        authorization: &AuthorizedIndexAdmin,
    ) -> Result<CanonicalIndexSnapshot, StoreError>;

    /// Return the current canonical store identity and revision while the
    /// source remains safe to serve.
    fn current_store_watermark(&self) -> Result<(StoreId, StoreWatermark), StoreError>;
}

impl CanonicalRecordReader for CanonicalStore {
    fn read_index_snapshot(
        &self,
        authorization: &AuthorizedIndexAdmin,
    ) -> Result<CanonicalIndexSnapshot, StoreError> {
        CanonicalStore::read_index_snapshot(self, authorization)
    }

    fn current_store_watermark(&self) -> Result<(StoreId, StoreWatermark), StoreError> {
        Ok((self.store_id().clone(), self.watermark()?))
    }
}

#[cfg(test)]
mod tests;
