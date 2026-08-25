//! Narrow host-owned read backend seam.

use crate::{IndexReadHealth, ReadServiceHealth, StoreReadHealth};
use jiandu_core::{
    MemoryGetRequest, MemoryListRequest, MemoryListResult, MemoryRecord, MemorySearchRequest,
    MemorySearchResult, StoreRevision,
};
use jiandu_index::{CursorMacKey, IndexError, LexicalIndex};
use jiandu_store::{AuthorizedRead, CanonicalStore, StoreError, StoreRead};
use std::fmt;
use std::sync::{Arc, RwLock};

/// Path-free backend error mapped to the stable public Jiandu envelope by the
/// MCP adapter.
pub enum ReadBackendError {
    Store(StoreError),
    Index(IndexError),
    /// The canonical watermark changed around one successful index query.
    /// The result must be discarded rather than paired with a mixed revision.
    UnstableSearchSnapshot,
    /// The host's in-process store coordination primitive is unavailable.
    HostUnavailable,
}

impl fmt::Debug for ReadBackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => formatter.debug_tuple("Store").field(&error.code()).finish(),
            Self::Index(error) => formatter.debug_tuple("Index").field(&error.code()).finish(),
            Self::UnstableSearchSnapshot => formatter.write_str("UnstableSearchSnapshot"),
            Self::HostUnavailable => formatter.write_str("HostUnavailable"),
        }
    }
}

impl fmt::Display for ReadBackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "canonical read failed: {:?}", error.code()),
            Self::Index(error) => write!(formatter, "lexical read failed: {:?}", error.code()),
            Self::UnstableSearchSnapshot => formatter.write_str("search snapshot changed"),
            Self::HostUnavailable => formatter.write_str("read host is unavailable"),
        }
    }
}

impl std::error::Error for ReadBackendError {}

impl From<StoreError> for ReadBackendError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<IndexError> for ReadBackendError {
    fn from(error: IndexError) -> Self {
        Self::Index(error)
    }
}

/// Host-controlled synchronous read seam. A daemon may implement this over a
/// lock or blocking worker without giving the MCP handler ownership of the
/// mutable canonical store.
pub trait McpReadBackend: Send + Sync + 'static {
    fn get(
        &self,
        authorization: &AuthorizedRead,
        request: &MemoryGetRequest,
    ) -> Result<StoreRead<MemoryRecord>, ReadBackendError>;

    fn list(
        &self,
        authorization: &AuthorizedRead,
        request: &MemoryListRequest,
    ) -> Result<StoreRead<MemoryListResult>, ReadBackendError>;

    /// Return a search result paired with the exact stable canonical revision
    /// validated around that result.
    fn search(
        &self,
        authorization: &AuthorizedRead,
        request: &MemorySearchRequest,
    ) -> Result<(StoreRevision, MemorySearchResult), ReadBackendError>;

    /// Return the canonical revision only while the store can safely serve.
    fn store_revision(&self) -> Result<StoreRevision, ReadBackendError>;

    /// Return only the host-approved closed readiness snapshot. Implementors
    /// must not call operator-only diagnostics on behalf of this method.
    fn health(&self) -> ReadServiceHealth;
}

/// Production backend that composes the real canonical store and lexical
/// index while preserving future mutable-store access through one host lock.
/// It intentionally does not call the operator-only index diagnostic API.
pub struct CanonicalReadBackend {
    store: Arc<RwLock<CanonicalStore>>,
    index: LexicalIndex,
    cursor_key: CursorMacKey,
    health: RwLock<ReadServiceHealth>,
}

impl fmt::Debug for CanonicalReadBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CanonicalReadBackend")
            .field("store", &"[REDACTED]")
            .field("index", &"[REDACTED]")
            .field("cursor_key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl CanonicalReadBackend {
    #[must_use]
    pub fn new(
        store: Arc<RwLock<CanonicalStore>>,
        index: LexicalIndex,
        cursor_key: CursorMacKey,
        health: ReadServiceHealth,
    ) -> Self {
        Self {
            store,
            index,
            cursor_key,
            health: RwLock::new(health),
        }
    }

    /// Replace only the pre-sanitized readiness snapshot exposed during MCP
    /// initialization. This does not inspect or mutate canonical/index data.
    pub fn update_health(&self, health: ReadServiceHealth) -> Result<(), ReadBackendError> {
        *self
            .health
            .write()
            .map_err(|_| ReadBackendError::HostUnavailable)? = health;
        Ok(())
    }

    fn read_store(
        &self,
    ) -> Result<std::sync::RwLockReadGuard<'_, CanonicalStore>, ReadBackendError> {
        self.store
            .read()
            .map_err(|_| ReadBackendError::HostUnavailable)
    }
}

impl McpReadBackend for CanonicalReadBackend {
    fn get(
        &self,
        authorization: &AuthorizedRead,
        request: &MemoryGetRequest,
    ) -> Result<StoreRead<MemoryRecord>, ReadBackendError> {
        let store = self.read_store()?;
        Ok(authorization.get(&store, &request.memory_id)?)
    }

    fn list(
        &self,
        authorization: &AuthorizedRead,
        request: &MemoryListRequest,
    ) -> Result<StoreRead<MemoryListResult>, ReadBackendError> {
        let store = self.read_store()?;
        Ok(authorization.list(&store, request)?)
    }

    fn search(
        &self,
        authorization: &AuthorizedRead,
        request: &MemorySearchRequest,
    ) -> Result<(StoreRevision, MemorySearchResult), ReadBackendError> {
        let store = self.read_store()?;
        let begin = store.watermark()?;
        let query_authorization = authorization.authorize_index_query(request)?;
        let result = self
            .index
            .search(&*store, &query_authorization, request, &self.cursor_key)?;
        let end = store.watermark()?;
        if begin != end {
            return Err(ReadBackendError::UnstableSearchSnapshot);
        }
        Ok((begin, result))
    }

    fn store_revision(&self) -> Result<StoreRevision, ReadBackendError> {
        Ok(self.read_store()?.watermark()?)
    }

    fn health(&self) -> ReadServiceHealth {
        self.health.read().map_or_else(
            |_| ReadServiceHealth::new(StoreReadHealth::Degraded, IndexReadHealth::Degraded),
            |health| health.clone(),
        )
    }
}
