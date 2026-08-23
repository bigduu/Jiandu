//! Versioned deterministic list cursors.

use crate::{AuthorizedScopes, StoreError, StoreId};
use jiandu_core::{MemoryListRequest, MemoryScope, PageCursor, StoreRevision};
use serde::Serialize;
use sha2::{Digest, Sha256};

const CURSOR_VERSION: &str = "j1";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CursorBinding<'a> {
    authorized_principal: &'a str,
    authorized_projects: Vec<&'a str>,
    authorized_sessions: Vec<&'a str>,
    authorized_instance_global: bool,
    selected_scopes: &'a [MemoryScope],
    requested_scopes: &'a [jiandu_core::ScopeSelector],
    types: &'a [jiandu_core::MemoryType],
    statuses: &'a [jiandu_core::MemoryStatus],
    tags: Vec<&'a str>,
    updated_after: Option<&'a str>,
    sort: jiandu_core::ListSort,
    limit: u16,
}

pub(crate) fn binding_fingerprint(
    request: &MemoryListRequest,
    authorized: &AuthorizedScopes,
    selected_scopes: &[MemoryScope],
) -> Result<String, StoreError> {
    let binding = CursorBinding {
        authorized_principal: authorized.principal_id.as_str(),
        authorized_projects: authorized
            .project_ids
            .iter()
            .map(jiandu_core::ProjectId::as_str)
            .collect(),
        authorized_sessions: authorized
            .session_ids
            .iter()
            .map(jiandu_core::SessionId::as_str)
            .collect(),
        authorized_instance_global: authorized.instance_global,
        selected_scopes,
        requested_scopes: &request.scopes,
        types: &request.types,
        statuses: &request.statuses,
        tags: request.tags.iter().map(jiandu_core::Tag::as_str).collect(),
        updated_after: request
            .updated_after
            .as_ref()
            .map(jiandu_core::Timestamp::as_str),
        sort: request.sort,
        limit: request.limit.get(),
    };
    let bytes = serde_json::to_vec(&binding).map_err(|_| StoreError::InvalidCursor)?;
    Ok(hex_digest(&bytes))
}

pub(crate) fn encode(
    store_id: &StoreId,
    revision: StoreRevision,
    fingerprint: &str,
    offset: usize,
) -> Result<PageCursor, StoreError> {
    if !is_lower_hex_digest(fingerprint) {
        return Err(StoreError::InvalidCursor);
    }
    let integrity = integrity_digest(store_id.as_str(), revision.0, fingerprint, offset);
    PageCursor::new(format!(
        "{CURSOR_VERSION}_{}_{}_{}_{}_{}",
        store_id.as_str(),
        revision.0,
        fingerprint,
        offset,
        integrity
    ))
    .map_err(|_| StoreError::InvalidCursor)
}

pub(crate) fn decode(
    cursor: &PageCursor,
    store_id: &StoreId,
    revision: StoreRevision,
    fingerprint: &str,
) -> Result<usize, StoreError> {
    let parts: Vec<_> = cursor.as_str().split('_').collect();
    if parts.len() != 6
        || parts[0] != CURSOR_VERSION
        || !is_lower_hex_digest(parts[3])
        || parts[4].is_empty()
        || !is_lower_hex_digest(parts[5])
    {
        return Err(StoreError::InvalidCursor);
    }
    let parsed_revision = parts[2]
        .parse::<u64>()
        .map_err(|_| StoreError::InvalidCursor)?;
    if parsed_revision.to_string() != parts[2] {
        return Err(StoreError::InvalidCursor);
    }
    let offset_u64 = parts[4]
        .parse::<u64>()
        .map_err(|_| StoreError::InvalidCursor)?;
    if offset_u64.to_string() != parts[4] {
        return Err(StoreError::InvalidCursor);
    }
    let offset = usize::try_from(offset_u64).map_err(|_| StoreError::InvalidCursor)?;
    if parts[5] != integrity_digest(parts[1], parsed_revision, parts[3], offset) {
        return Err(StoreError::InvalidCursor);
    }
    if parts[1] != store_id.as_str() || parsed_revision != revision.0 {
        return Err(StoreError::StaleCursor);
    }
    if parts[3] != fingerprint {
        return Err(StoreError::InvalidCursor);
    }
    Ok(offset)
}

fn integrity_digest(store_id: &str, revision: u64, fingerprint: &str, offset: usize) -> String {
    let mut bytes = Vec::new();
    for component in [
        CURSOR_VERSION,
        store_id,
        &revision.to_string(),
        fingerprint,
        &offset.to_string(),
    ] {
        bytes.extend_from_slice(component.as_bytes());
        bytes.push(0);
    }
    hex_digest(&bytes)
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn is_lower_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
