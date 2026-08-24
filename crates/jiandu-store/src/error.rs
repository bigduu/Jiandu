//! Stable, path-free storage diagnostics.

use jiandu_core::{MemoryId, Revision};
use std::fmt;
use std::io;

/// Stable storage failure categories suitable for adapter mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum StoreErrorCode {
    InvalidDataDirectory,
    AlreadyInitialized,
    NotInitialized,
    InvalidStoreMetadata,
    UnsupportedStoreFormat,
    StoreLocked,
    InvalidLayout,
    UnsafePath,
    Unauthenticated,
    Forbidden,
    InvalidRequest,
    InvalidRecord,
    DuplicateMemoryId,
    NotFound,
    InvalidCursor,
    StaleCursor,
    RecordIsValid,
    AlreadyExists,
    RevisionConflict,
    RevisionOverflow,
    IdempotencyConflict,
    RecoveryRequired,
    InvalidTransaction,
    UnsupportedDurability,
    InjectedFailure,
    Io,
}

/// Stable reason attached to an invalid canonical record without its path/body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum InvalidRecordReason {
    InvalidUtf8,
    Truncated,
    MalformedFrontmatter,
    NonCanonicalEncoding,
    ValidationFailed,
    IdFilenameMismatch,
    ScopePathMismatch,
    ShardMismatch,
}

/// Secret-safe store error. Paths and record bodies are intentionally absent.
#[derive(Debug)]
#[non_exhaustive]
pub enum StoreError {
    InvalidDataDirectory,
    AlreadyInitialized,
    NotInitialized,
    InvalidStoreMetadata,
    UnsupportedStoreFormat {
        found: String,
    },
    StoreLocked {
        owner: Option<crate::LockOwnerDiagnostics>,
    },
    InvalidLayout,
    UnsafePath,
    Unauthenticated,
    Forbidden,
    InvalidRequest,
    InvalidRecord {
        id: Option<MemoryId>,
        reason: InvalidRecordReason,
    },
    DuplicateMemoryId {
        id: MemoryId,
    },
    NotFound,
    InvalidCursor,
    StaleCursor,
    RecordIsValid {
        id: MemoryId,
    },
    AlreadyExists {
        id: MemoryId,
    },
    RevisionConflict {
        current_revision: Revision,
    },
    RevisionOverflow,
    IdempotencyConflict,
    RecoveryRequired,
    InvalidTransaction,
    UnsupportedDurability {
        capability: &'static str,
    },
    InjectedFailure {
        boundary: crate::PersistenceBoundary,
    },
    Io {
        operation: &'static str,
        source: io::Error,
    },
}

impl StoreError {
    #[must_use]
    pub const fn code(&self) -> StoreErrorCode {
        match self {
            Self::InvalidDataDirectory => StoreErrorCode::InvalidDataDirectory,
            Self::AlreadyInitialized => StoreErrorCode::AlreadyInitialized,
            Self::NotInitialized => StoreErrorCode::NotInitialized,
            Self::InvalidStoreMetadata => StoreErrorCode::InvalidStoreMetadata,
            Self::UnsupportedStoreFormat { .. } => StoreErrorCode::UnsupportedStoreFormat,
            Self::StoreLocked { .. } => StoreErrorCode::StoreLocked,
            Self::InvalidLayout => StoreErrorCode::InvalidLayout,
            Self::UnsafePath => StoreErrorCode::UnsafePath,
            Self::Unauthenticated => StoreErrorCode::Unauthenticated,
            Self::Forbidden => StoreErrorCode::Forbidden,
            Self::InvalidRequest => StoreErrorCode::InvalidRequest,
            Self::InvalidRecord { .. } => StoreErrorCode::InvalidRecord,
            Self::DuplicateMemoryId { .. } => StoreErrorCode::DuplicateMemoryId,
            Self::NotFound => StoreErrorCode::NotFound,
            Self::InvalidCursor => StoreErrorCode::InvalidCursor,
            Self::StaleCursor => StoreErrorCode::StaleCursor,
            Self::RecordIsValid { .. } => StoreErrorCode::RecordIsValid,
            Self::AlreadyExists { .. } => StoreErrorCode::AlreadyExists,
            Self::RevisionConflict { .. } => StoreErrorCode::RevisionConflict,
            Self::RevisionOverflow => StoreErrorCode::RevisionOverflow,
            Self::IdempotencyConflict => StoreErrorCode::IdempotencyConflict,
            Self::RecoveryRequired => StoreErrorCode::RecoveryRequired,
            Self::InvalidTransaction => StoreErrorCode::InvalidTransaction,
            Self::UnsupportedDurability { .. } => StoreErrorCode::UnsupportedDurability,
            Self::InjectedFailure { .. } => StoreErrorCode::InjectedFailure,
            Self::Io { .. } => StoreErrorCode::Io,
        }
    }

    pub(crate) fn io(operation: &'static str, source: io::Error) -> Self {
        Self::Io { operation, source }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDataDirectory => formatter.write_str("invalid Jiandu data directory"),
            Self::AlreadyInitialized => formatter.write_str("Jiandu store is already initialized"),
            Self::NotInitialized => formatter.write_str("Jiandu store is not initialized"),
            Self::InvalidStoreMetadata => formatter.write_str("invalid Jiandu store metadata"),
            Self::UnsupportedStoreFormat { found } => {
                write!(formatter, "unsupported Jiandu store format: {found}")
            }
            Self::StoreLocked { owner: Some(owner) } => write!(
                formatter,
                "Jiandu store is owned by instance {} (pid {}, started {})",
                owner.instance_id, owner.process_id, owner.started_at
            ),
            Self::StoreLocked { owner: None } => {
                formatter.write_str("Jiandu store is owned by another instance")
            }
            Self::InvalidLayout => formatter.write_str("invalid Jiandu store layout"),
            Self::UnsafePath => formatter.write_str("unsafe Jiandu store path"),
            Self::Unauthenticated => formatter.write_str("trusted mutation identity is invalid"),
            Self::Forbidden => formatter.write_str("mutation is not authorized"),
            Self::InvalidRequest => formatter.write_str("invalid Jiandu store request"),
            Self::InvalidRecord {
                id: Some(id),
                reason,
            } => {
                write!(formatter, "invalid memory record {id}: {reason:?}")
            }
            Self::InvalidRecord { id: None, reason } => {
                write!(formatter, "invalid memory record: {reason:?}")
            }
            Self::DuplicateMemoryId { id } => write!(formatter, "duplicate memory ID {id}"),
            Self::NotFound => formatter.write_str("memory record not found"),
            Self::InvalidCursor => formatter.write_str("invalid list cursor"),
            Self::StaleCursor => formatter.write_str("list cursor no longer matches the store"),
            Self::RecordIsValid { id } => {
                write!(
                    formatter,
                    "memory record {id} is valid and was not quarantined"
                )
            }
            Self::AlreadyExists { id } => write!(formatter, "memory record {id} already exists"),
            Self::RevisionConflict { current_revision } => write!(
                formatter,
                "memory revision conflict; current revision is {}",
                current_revision.get()
            ),
            Self::RevisionOverflow => formatter.write_str("memory or store revision overflow"),
            Self::IdempotencyConflict => {
                formatter.write_str("idempotency key was reused for different input")
            }
            Self::RecoveryRequired => formatter.write_str("store handle requires restart recovery"),
            Self::InvalidTransaction => formatter.write_str("invalid or ambiguous transaction"),
            Self::UnsupportedDurability { capability } => {
                write!(
                    formatter,
                    "required durability capability is unavailable: {capability}"
                )
            }
            Self::InjectedFailure { boundary } => {
                write!(formatter, "injected failure after {boundary:?}")
            }
            Self::Io { operation, .. } => write!(formatter, "store I/O failed during {operation}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
