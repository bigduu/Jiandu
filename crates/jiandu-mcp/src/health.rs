//! Secret-safe adapter health exposed during MCP initialization.

use serde::Serialize;
use std::fmt;
use std::sync::{Arc, RwLock};

/// Closed canonical-store readiness reported to an authenticated connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreReadHealth {
    Ready,
    Degraded,
}

/// Closed lexical-index readiness. Reasons, counts, paths, and watermarks are
/// deliberately not exposed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexReadHealth {
    Ready,
    Degraded,
    Missing,
}

/// Optional adapter capability names safe to expose to an authenticated MCP
/// connection. The enum prevents arbitrary host diagnostics from crossing the
/// protocol boundary.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionalCapability {
    Resources,
}

/// Path-free, count-free readiness snapshot supplied by the trusted host.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadServiceHealth {
    store: StoreReadHealth,
    index: IndexReadHealth,
    exact_read: bool,
    list: bool,
    search: bool,
}

impl ReadServiceHealth {
    /// Construct a self-consistent snapshot; operation flags are derived from
    /// the closed readiness states rather than accepted from wire input.
    #[must_use]
    pub fn new(store: StoreReadHealth, index: IndexReadHealth) -> Self {
        let canonical_ready = store == StoreReadHealth::Ready;
        Self {
            store,
            index,
            exact_read: canonical_ready,
            list: canonical_ready,
            search: canonical_ready && index == IndexReadHealth::Ready,
        }
    }

    #[must_use]
    pub const fn store(&self) -> StoreReadHealth {
        self.store
    }

    #[must_use]
    pub const fn index(&self) -> IndexReadHealth {
        self.index
    }

    #[must_use]
    pub const fn exact_read_available(&self) -> bool {
        self.exact_read
    }

    #[must_use]
    pub const fn list_available(&self) -> bool {
        self.list
    }

    #[must_use]
    pub const fn search_available(&self) -> bool {
        self.search
    }
}

/// Cloneable observer for the sanitized readiness state. It owns only the
/// closed health value, never the canonical backend, store, index, path, or
/// credentials, so a long-lived host readiness handler cannot prolong the
/// singleton writer lock.
#[derive(Clone)]
pub struct ReadHealthSnapshot {
    inner: Arc<RwLock<ReadServiceHealth>>,
}

impl ReadHealthSnapshot {
    pub(crate) fn new(health: ReadServiceHealth) -> Self {
        Self {
            inner: Arc::new(RwLock::new(health)),
        }
    }

    /// Return the latest closed health value. Poisoning is reported only as the
    /// same path-free degraded state already used by the MCP health boundary.
    #[must_use]
    pub fn current(&self) -> ReadServiceHealth {
        self.inner.read().map_or_else(
            |_| ReadServiceHealth::new(StoreReadHealth::Degraded, IndexReadHealth::Degraded),
            |health| health.clone(),
        )
    }

    pub(crate) fn replace(&self, health: ReadServiceHealth) -> Result<(), ()> {
        *self.inner.write().map_err(|_| ())? = health;
        Ok(())
    }
}

impl fmt::Debug for ReadHealthSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReadHealthSnapshot")
            .field("health", &"[REDACTED]")
            .finish()
    }
}
