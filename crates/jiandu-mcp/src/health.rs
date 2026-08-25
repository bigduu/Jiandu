//! Secret-safe adapter health exposed during MCP initialization.

use serde::Serialize;

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
