//! Strict, trusted local startup configuration.

use jiandu_core::{
    ClientId, CreationActor, Grant, MemoryType, PrincipalId, ProjectId, SessionId,
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

const MAX_CONFIG_BYTES: u64 = 256 * 1024;
const MAX_CLIENTS: usize = 64;
const TOKEN_DIGEST_PREFIX: &str = "sha256:";
const CURSOR_KEY_PREFIX: &str = "hmac-sha256:";

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

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConfiguredScopeKind {
    Principal,
    Project,
    Session,
    InstanceGlobal,
}

impl From<ConfiguredScopeKind> for MutationScopeKind {
    fn from(value: ConfiguredScopeKind) -> Self {
        match value {
            ConfiguredScopeKind::Principal => Self::Principal,
            ConfiguredScopeKind::Project => Self::Project,
            ConfiguredScopeKind::Session => Self::Session,
            ConfiguredScopeKind::InstanceGlobal => Self::InstanceGlobal,
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConfiguredSecretPolicy {
    AllowAll,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MutationPolicyDocument {
    max_body_bytes: usize,
    allowed_types: Vec<MemoryType>,
    allowed_scopes: Vec<ConfiguredScopeKind>,
    secret_content_policy: ConfiguredSecretPolicy,
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
    grants: Vec<Grant>,
    scopes: ScopeDocument,
    creation_actor: CreationActor,
    mutation_policy: MutationPolicyDocument,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ServeConfigDocument {
    bind: SocketAddr,
    data_dir: PathBuf,
    cursor_mac_key: CursorKeyMaterial,
    clients: Vec<ClientDocument>,
}

pub(crate) struct ValidatedClient {
    pub(crate) bearer_digest: BearerDigest,
    pub(crate) context: TrustedRequestContext,
    pub(crate) scopes: AuthorizedScopes,
    pub(crate) creation_actor: CreationActor,
    pub(crate) mutation_policy: Arc<ConfiguredMutationPolicy>,
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
/// cursor key, credential digests, identities, grants, and scope membership.
pub struct ServeConfig {
    pub(crate) bind: SocketAddr,
    pub(crate) data_dir: PathBuf,
    pub(crate) cursor_mac_key: CursorMacKey,
    pub(crate) clients: Vec<ValidatedClient>,
}

impl fmt::Debug for ServeConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServeConfig")
            .field("bind", &self.bind)
            .field("data_dir", &"[REDACTED]")
            .field("cursor_mac_key", &"[REDACTED]")
            .field("client_count", &self.clients.len())
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
        if !document.bind.ip().is_loopback()
            || document.data_dir.as_os_str().is_empty()
            || document.clients.is_empty()
            || document.clients.len() > MAX_CLIENTS
        {
            return Err(ConfigError::invalid());
        }

        let mut credential_digests = BTreeSet::new();
        let mut clients = Vec::with_capacity(document.clients.len());
        for client in document.clients {
            if !credential_digests.insert(client.bearer_token_digest)
                || !all_unique(&client.grants)
                || !all_unique(&client.scopes.project_ids)
                || !all_unique(&client.scopes.session_ids)
                || !all_unique(&client.mutation_policy.allowed_types)
                || !all_unique_by(
                    &client.mutation_policy.allowed_scopes,
                    |scope| match scope {
                        ConfiguredScopeKind::Principal => 0_u8,
                        ConfiguredScopeKind::Project => 1,
                        ConfiguredScopeKind::Session => 2,
                        ConfiguredScopeKind::InstanceGlobal => 3,
                    },
                )
                || client.grants.iter().any(|grant| !supported_grant(grant))
            {
                return Err(ConfigError::invalid());
            }

            let grants = client.grants.into_iter().collect::<BTreeSet<_>>();
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

            let allowed_types = client
                .mutation_policy
                .allowed_types
                .into_iter()
                .collect::<BTreeSet<_>>();
            let allowed_scopes = client
                .mutation_policy
                .allowed_scopes
                .into_iter()
                .map(MutationScopeKind::from)
                .collect::<BTreeSet<_>>();
            let secret_content = match client.mutation_policy.secret_content_policy {
                ConfiguredSecretPolicy::AllowAll => Arc::new(AllowAllSecretContent),
            };
            let mutation_policy = ConfiguredMutationPolicy::new(
                client.mutation_policy.max_body_bytes,
                allowed_types,
                allowed_scopes,
                secret_content,
            )
            .map_err(|_| ConfigError::invalid())?;
            clients.push(ValidatedClient {
                bearer_digest: client.bearer_token_digest,
                context,
                scopes,
                creation_actor: client.creation_actor,
                mutation_policy: Arc::new(mutation_policy),
            });
        }

        Ok(Self {
            bind: document.bind,
            data_dir: document.data_dir,
            cursor_mac_key: document.cursor_mac_key.into_key(),
            clients,
        })
    }
}

fn supported_grant(grant: &Grant) -> bool {
    matches!(
        grant.as_str(),
        "memory:read"
            | "memory:write:principal"
            | "memory:write:project"
            | "memory:write:session"
            | "memory:write:instance_global"
            | "memory:forget:principal"
            | "memory:forget:project"
            | "memory:forget:session"
            | "memory:forget:instance_global"
    )
}

fn all_unique<T: Ord + Clone>(values: &[T]) -> bool {
    values.iter().cloned().collect::<BTreeSet<_>>().len() == values.len()
}

fn all_unique_by<T, K: Ord>(values: &[T], key: impl Fn(&T) -> K) -> bool {
    values.iter().map(key).collect::<BTreeSet<_>>().len() == values.len()
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
