//! Stable, path-free lexical-index diagnostics.

use jiandu_store::{StoreError, StoreErrorCode};
use std::fmt;

/// Observable reason that the disposable index cannot currently serve search.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum IndexDegradedReason {
    Missing,
    Corrupt,
    IncompatibleVersion,
    Stale,
    SourceUnavailable,
}

/// Stable adapter-facing lexical-index failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum IndexErrorCode {
    InvalidRequest,
    Unauthenticated,
    Forbidden,
    InvalidCursor,
    StaleCursor,
    Degraded,
    Io,
}

/// Secret-safe error. It never contains a query, record body, credential, or
/// ambient/canonical path.
#[derive(Debug)]
#[non_exhaustive]
pub enum IndexError {
    InvalidRequest,
    Unauthenticated,
    Forbidden,
    InvalidCursor,
    StaleCursor,
    Degraded { reason: IndexDegradedReason },
    Io { operation: &'static str },
}

impl IndexError {
    #[must_use]
    pub const fn code(&self) -> IndexErrorCode {
        match self {
            Self::InvalidRequest => IndexErrorCode::InvalidRequest,
            Self::Unauthenticated => IndexErrorCode::Unauthenticated,
            Self::Forbidden => IndexErrorCode::Forbidden,
            Self::InvalidCursor => IndexErrorCode::InvalidCursor,
            Self::StaleCursor => IndexErrorCode::StaleCursor,
            Self::Degraded { .. } => IndexErrorCode::Degraded,
            Self::Io { .. } => IndexErrorCode::Io,
        }
    }

    pub(crate) const fn io(operation: &'static str) -> Self {
        Self::Io { operation }
    }
}

impl From<StoreError> for IndexError {
    fn from(error: StoreError) -> Self {
        match error.code() {
            StoreErrorCode::Unauthenticated => Self::Unauthenticated,
            StoreErrorCode::Forbidden => Self::Forbidden,
            StoreErrorCode::InvalidRequest => Self::InvalidRequest,
            _ => Self::Degraded {
                reason: IndexDegradedReason::SourceUnavailable,
            },
        }
    }
}

impl fmt::Display for IndexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest => formatter.write_str("invalid lexical-search request"),
            Self::Unauthenticated => formatter.write_str("trusted search identity is invalid"),
            Self::Forbidden => formatter.write_str("lexical search is not authorized"),
            Self::InvalidCursor => formatter.write_str("invalid lexical-search cursor"),
            Self::StaleCursor => formatter.write_str("lexical-search cursor is stale"),
            Self::Degraded { reason } => write!(formatter, "lexical index is degraded: {reason:?}"),
            Self::Io { operation } => {
                write!(formatter, "lexical-index I/O failed during {operation}")
            }
        }
    }
}

impl std::error::Error for IndexError {}
