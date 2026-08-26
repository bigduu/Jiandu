//! Strict, trusted local startup configuration.

use jiandu_core::{
    ClientId, CreationActor, Grant, MAX_BODY_BYTES, MemoryType, PrincipalId, ProjectId, SessionId,
    TrustedRequestContext,
};
use jiandu_index::CursorMacKey;
use jiandu_mcp::{AllowAllSecretContent, ConfiguredMutationPolicy, MutationScopeKind};
use jiandu_store::AuthorizedScopes;
use serde::Deserialize;
use serde::de::Error as _;
use std::collections::BTreeSet;
use std::fmt;
use std::fs::File;
use std::io::Read as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const MAX_CONFIG_BYTES: u64 = 256 * 1024;
const MAX_CLIENTS: usize = 64;
const DEFAULT_DRAIN_TIMEOUT_MS: u64 = 5_000;
const MIN_DRAIN_TIMEOUT_MS: u64 = 10;
const MAX_DRAIN_TIMEOUT_MS: u64 = 60_000;
const TOKEN_DIGEST_PREFIX: &str = "sha256:";
const CURSOR_KEY_PREFIX: &str = "hmac-sha256:";
const SERVICE_MEMORY_TYPES: [MemoryType; 6] = [
    MemoryType::Preference,
    MemoryType::Decision,
    MemoryType::Project,
    MemoryType::Fact,
    MemoryType::Feedback,
    MemoryType::Reference,
];

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct BearerDigest([u8; 32]);

impl BearerDigest {
    pub(crate) const fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for BearerDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BearerDigest([REDACTED])")
    }
}

impl<'de> Deserialize<'de> for BearerDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let encoded = value
            .strip_prefix(TOKEN_DIGEST_PREFIX)
            .ok_or_else(|| D::Error::custom("invalid bearer digest"))?;
        decode_lower_hex_32(encoded)
            .map(Self)
            .ok_or_else(|| D::Error::custom("invalid bearer digest"))
    }
}

struct CursorKeyMaterial([u8; 32]);

impl CursorKeyMaterial {
    fn into_key(self) -> CursorMacKey {
        CursorMacKey::new(self.0)
    }
}

impl fmt::Debug for CursorKeyMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CursorKeyMaterial([REDACTED])")
    }
}

impl<'de> Deserialize<'de> for CursorKeyMaterial {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let encoded = value
            .strip_prefix(CURSOR_KEY_PREFIX)
            .ok_or_else(|| D::Error::custom("invalid cursor key"))?;
        decode_lower_hex_32(encoded)
            .map(Self)
            .ok_or_else(|| D::Error::custom("invalid cursor key"))
    }
}

#[derive(Clone, Copy, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
enum PermissionScopeKind {
    Principal,
    Project,
    Session,
    InstanceGlobal,
}

impl PermissionScopeKind {
    const fn write_grant(self) -> &'static str {
        match self {
            Self::Principal => "memory:write:principal",
            Self::Project => "memory:write:project",
            Self::Session => "memory:write:session",
            Self::InstanceGlobal => "memory:write:instance_global",
        }
    }

    const fn forget_grant(self) -> &'static str {
        match self {
            Self::Principal => "memory:forget:principal",
            Self::Project => "memory:forget:project",
            Self::Session => "memory:forget:session",
            Self::InstanceGlobal => "memory:forget:instance_global",
        }
    }

    fn has_exact_scope(self, scopes: &ScopeDocument) -> bool {
        match self {
            Self::Principal => true,
            Self::Project => !scopes.project_ids.is_empty(),
            Self::Session => !scopes.session_ids.is_empty(),
            Self::InstanceGlobal => scopes.instance_global,
        }
    }
}

impl From<PermissionScopeKind> for MutationScopeKind {
    fn from(value: PermissionScopeKind) -> Self {
        match value {
            PermissionScopeKind::Principal => Self::Principal,
            PermissionScopeKind::Project => Self::Project,
            PermissionScopeKind::Session => Self::Session,
            PermissionScopeKind::InstanceGlobal => Self::InstanceGlobal,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PermissionProfile {
    read: bool,
    write: Vec<PermissionScopeKind>,
    forget: Vec<PermissionScopeKind>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScopeDocument {
    project_ids: Vec<ProjectId>,
    session_ids: Vec<SessionId>,
    instance_global: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClientDocument {
    bearer_token_digest: BearerDigest,
    principal_id: PrincipalId,
    client_id: ClientId,
    scopes: ScopeDocument,
    permissions: PermissionProfile,
    creation_actor: CreationActor,
}

#[derive(Deserialize)]
#[serde(tag = "configVersion", deny_unknown_fields)]
enum ServeConfigDocument {
    #[serde(rename = "jiandu.service.config/v0.1")]
    V0_1 {
        bind: SocketAddr,
        #[serde(rename = "dataDir")]
        data_dir: PathBuf,
        #[serde(rename = "cursorMacKey")]
        cursor_mac_key: CursorKeyMaterial,
        clients: Vec<ClientDocument>,
    },
    #[serde(rename = "jiandu.service.config/v0.2")]
    V0_2 {
        bind: SocketAddr,
        #[serde(rename = "dataDir")]
        data_dir: PathBuf,
        #[serde(rename = "cursorMacKey")]
        cursor_mac_key: CursorKeyMaterial,
        shutdown: ShutdownDocument,
        clients: Vec<ClientDocument>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ShutdownDocument {
    drain_timeout_ms: u64,
}

struct CommonConfigDocument {
    bind: SocketAddr,
    data_dir: PathBuf,
    cursor_mac_key: CursorKeyMaterial,
    clients: Vec<ClientDocument>,
    drain_timeout: Duration,
}

pub(crate) struct ValidatedClient {
    pub(crate) bearer_digest: BearerDigest,
    pub(crate) context: TrustedRequestContext,
    pub(crate) scopes: AuthorizedScopes,
    pub(crate) creation_actor: CreationActor,
    pub(crate) mutation_policy: Option<Arc<ConfiguredMutationPolicy>>,
}

impl fmt::Debug for ValidatedClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ValidatedClient")
            .field("credential", &"[REDACTED]")
            .field("identity", &"[REDACTED]")
            .field("authority", &"[REDACTED]")
            .field("policy", &"[REDACTED]")
            .finish()
    }
}

/// Fully validated startup configuration. Its debug form omits the data path,
/// cursor key, credential digests, identities, permissions, and scope membership.
pub struct ServeConfig {
    pub(crate) bind: SocketAddr,
    pub(crate) data_dir: PathBuf,
    pub(crate) cursor_mac_key: CursorMacKey,
    pub(crate) clients: Vec<ValidatedClient>,
    pub(crate) drain_timeout: Duration,
}

impl fmt::Debug for ServeConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServeConfig")
            .field("bind", &self.bind)
            .field("data_dir", &"[REDACTED]")
            .field("cursor_mac_key", &"[REDACTED]")
            .field("client_count", &self.clients.len())
            .field("drain_timeout", &self.drain_timeout)
            .finish()
    }
}

impl ServeConfig {
    /// Read one bounded local JSON file. Parse and validation failures are
    /// intentionally generic so startup diagnostics cannot echo its contents.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let mut file = File::open(path.as_ref()).map_err(|_| ConfigError::file_unavailable())?;
        let metadata = file
            .metadata()
            .map_err(|_| ConfigError::file_unavailable())?;
        if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::invalid());
        }
        let mut bytes = Vec::new();
        file.by_ref()
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ConfigError::file_unavailable())?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            return Err(ConfigError::invalid());
        }
        Self::from_slice(&bytes)
    }

    pub(crate) fn from_slice(bytes: &[u8]) -> Result<Self, ConfigError> {
        let document: ServeConfigDocument =
            serde_json::from_slice(bytes).map_err(|_| ConfigError::invalid())?;
        Self::validate(document)
    }

    fn validate(document: ServeConfigDocument) -> Result<Self, ConfigError> {
        let common = match document {
            ServeConfigDocument::V0_1 {
                bind,
                data_dir,
                cursor_mac_key,
                clients,
            } => CommonConfigDocument {
                bind,
                data_dir,
                cursor_mac_key,
                clients,
                drain_timeout: Duration::from_millis(DEFAULT_DRAIN_TIMEOUT_MS),
            },
            ServeConfigDocument::V0_2 {
                bind,
                data_dir,
                cursor_mac_key,
                shutdown,
                clients,
            } => {
                if !(MIN_DRAIN_TIMEOUT_MS..=MAX_DRAIN_TIMEOUT_MS)
                    .contains(&shutdown.drain_timeout_ms)
                {
                    return Err(ConfigError::invalid());
                }
                CommonConfigDocument {
                    bind,
                    data_dir,
                    cursor_mac_key,
                    clients,
                    drain_timeout: Duration::from_millis(shutdown.drain_timeout_ms),
                }
            }
        };
        let CommonConfigDocument {
            bind,
            data_dir,
            cursor_mac_key,
            clients: client_documents,
            drain_timeout,
        } = common;
        if !bind.ip().is_loopback()
            || data_dir.as_os_str().is_empty()
            || client_documents.is_empty()
            || client_documents.len() > MAX_CLIENTS
        {
            return Err(ConfigError::invalid());
        }

        let mut credential_digests = BTreeSet::new();
        let mut clients = Vec::with_capacity(client_documents.len());
        for client in client_documents {
            if !credential_digests.insert(client.bearer_token_digest)
                || !all_unique(&client.scopes.project_ids)
                || !all_unique(&client.scopes.session_ids)
                || !client.permissions.read
                || !all_unique(&client.permissions.write)
                || !all_unique(&client.permissions.forget)
                || client
                    .permissions
                    .write
                    .iter()
                    .chain(&client.permissions.forget)
                    .any(|scope| !scope.has_exact_scope(&client.scopes))
            {
                return Err(ConfigError::invalid());
            }

            let mut grants =
                BTreeSet::from([Grant::new("memory:read").map_err(|_| ConfigError::invalid())?]);
            for scope in client.permissions.write.iter().copied() {
                grants.insert(Grant::new(scope.write_grant()).map_err(|_| ConfigError::invalid())?);
            }
            for scope in client.permissions.forget.iter().copied() {
                grants
                    .insert(Grant::new(scope.forget_grant()).map_err(|_| ConfigError::invalid())?);
            }
            let context = TrustedRequestContext {
                principal_id: client.principal_id.clone(),
                client_id: client.client_id,
                grants,
            };
            let mut scopes = AuthorizedScopes::new(client.principal_id);
            for project_id in client.scopes.project_ids {
                scopes = scopes.with_project(project_id);
            }
            for session_id in client.scopes.session_ids {
                scopes = scopes.with_session(session_id);
            }
            if client.scopes.instance_global {
                scopes = scopes.with_instance_global();
            }
            scopes
                .authorize_read(&context)
                .map_err(|_| ConfigError::invalid())?;

            let allowed_scopes = client
                .permissions
                .write
                .iter()
                .chain(&client.permissions.forget)
                .copied()
                .map(MutationScopeKind::from)
                .collect::<BTreeSet<_>>();
            let mutation_policy = if allowed_scopes.is_empty() {
                None
            } else {
                Some(Arc::new(
                    ConfiguredMutationPolicy::new(
                        MAX_BODY_BYTES,
                        BTreeSet::from(SERVICE_MEMORY_TYPES),
                        allowed_scopes,
                        Arc::new(AllowAllSecretContent),
                    )
                    .map_err(|_| ConfigError::invalid())?,
                ))
            };
            clients.push(ValidatedClient {
                bearer_digest: client.bearer_token_digest,
                context,
                scopes,
                creation_actor: client.creation_actor,
                mutation_policy,
            });
        }

        Ok(Self {
            bind,
            data_dir,
            cursor_mac_key: cursor_mac_key.into_key(),
            clients,
            drain_timeout,
        })
    }
}

fn all_unique<T: Ord + Clone>(values: &[T]) -> bool {
    values.iter().cloned().collect::<BTreeSet<_>>().len() == values.len()
}

fn decode_lower_hex_32(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let mut output = [0_u8; 32];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        output[index] = (hex_value(pair[0])? << 4) | hex_value(pair[1])?;
    }
    Some(output)
}

const fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug)]
enum ConfigErrorCode {
    FileUnavailable,
    Invalid,
}

/// Secret- and path-free startup configuration failure.
pub struct ConfigError {
    code: ConfigErrorCode,
}

impl ConfigError {
    const fn file_unavailable() -> Self {
        Self {
            code: ConfigErrorCode::FileUnavailable,
        }
    }

    const fn invalid() -> Self {
        Self {
            code: ConfigErrorCode::Invalid,
        }
    }
}

impl fmt::Debug for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfigError")
            .field("code", &self.code)
            .finish()
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code {
            ConfigErrorCode::FileUnavailable => {
                formatter.write_str("startup configuration is unavailable")
            }
            ConfigErrorCode::Invalid => formatter.write_str("startup configuration is invalid"),
        }
    }
}

impl std::error::Error for ConfigError {}
