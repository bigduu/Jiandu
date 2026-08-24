//! Authenticated public pagination cursors.
//!
//! The embedded checksum only detects accidental corruption. Authorization
//! and tamper resistance come exclusively from the standard HMAC-SHA256 tag.

use crate::error::IndexError;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use jiandu_core::{PageCursor, StoreRevision};
use jiandu_store::StoreId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

const CURSOR_FORMAT_VERSION: &str = "jiandu.index.cursor/v1alpha1";
const MAX_CURSOR_BYTES: usize = 768;
type HmacSha256 = Hmac<Sha256>;

/// Host-held cursor signing key. It is deliberately not serializable and its
/// debug representation never exposes key material.
#[derive(Clone)]
pub struct CursorMacKey([u8; 32]);

impl CursorMacKey {
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for CursorMacKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CursorMacKey([REDACTED])")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CursorBinding<'a> {
    pub authority_fingerprint: &'a str,
    pub request_fingerprint: &'a str,
    pub source_store_id: &'a StoreId,
    pub source_store_revision: StoreRevision,
    pub index_content_checksum: &'a str,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CursorEnvelope {
    payload: CursorPayload,
    mac: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CursorPayload {
    format_version: String,
    offset: u32,
    authority_fingerprint: String,
    request_fingerprint: String,
    source_store_id: StoreId,
    source_store_revision: StoreRevision,
    index_content_checksum: String,
    checksum: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UnsignedCursorPayload<'a> {
    format_version: &'a str,
    offset: u32,
    authority_fingerprint: &'a str,
    request_fingerprint: &'a str,
    source_store_id: &'a StoreId,
    source_store_revision: StoreRevision,
    index_content_checksum: &'a str,
}

pub(crate) fn encode_cursor(
    key: &CursorMacKey,
    offset: u32,
    binding: &CursorBinding<'_>,
) -> Result<PageCursor, IndexError> {
    let unsigned = UnsignedCursorPayload {
        format_version: CURSOR_FORMAT_VERSION,
        offset,
        authority_fingerprint: binding.authority_fingerprint,
        request_fingerprint: binding.request_fingerprint,
        source_store_id: binding.source_store_id,
        source_store_revision: binding.source_store_revision,
        index_content_checksum: binding.index_content_checksum,
    };
    let unsigned_bytes = serde_json::to_vec(&unsigned).map_err(|_| IndexError::InvalidCursor)?;
    let checksum = sha256_hex(b"jiandu/index/cursor-checksum/v1\0", &unsigned_bytes);
    let payload = CursorPayload {
        format_version: CURSOR_FORMAT_VERSION.to_owned(),
        offset,
        authority_fingerprint: binding.authority_fingerprint.to_owned(),
        request_fingerprint: binding.request_fingerprint.to_owned(),
        source_store_id: binding.source_store_id.clone(),
        source_store_revision: binding.source_store_revision,
        index_content_checksum: binding.index_content_checksum.to_owned(),
        checksum,
    };
    let payload_bytes = serde_json::to_vec(&payload).map_err(|_| IndexError::InvalidCursor)?;
    let envelope = CursorEnvelope {
        mac: hmac_hex(key, &payload_bytes),
        payload,
    };
    let bytes = serde_json::to_vec(&envelope).map_err(|_| IndexError::InvalidCursor)?;
    if bytes.len() > MAX_CURSOR_BYTES {
        return Err(IndexError::InvalidCursor);
    }
    PageCursor::new(URL_SAFE_NO_PAD.encode(bytes)).map_err(|_| IndexError::InvalidCursor)
}

pub(crate) fn decode_cursor(
    key: &CursorMacKey,
    cursor: &PageCursor,
    binding: &CursorBinding<'_>,
) -> Result<u32, IndexError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor.as_str())
        .map_err(|_| IndexError::InvalidCursor)?;
    if bytes.len() > MAX_CURSOR_BYTES || URL_SAFE_NO_PAD.encode(&bytes) != cursor.as_str() {
        return Err(IndexError::InvalidCursor);
    }
    let envelope: CursorEnvelope =
        serde_json::from_slice(&bytes).map_err(|_| IndexError::InvalidCursor)?;
    if serde_json::to_vec(&envelope).map_err(|_| IndexError::InvalidCursor)? != bytes {
        return Err(IndexError::InvalidCursor);
    }
    let payload_bytes =
        serde_json::to_vec(&envelope.payload).map_err(|_| IndexError::InvalidCursor)?;
    let tag = decode_lower_hex(&envelope.mac).ok_or(IndexError::InvalidCursor)?;
    let mut verifier = HmacSha256::new_from_slice(&key.0).map_err(|_| IndexError::InvalidCursor)?;
    verifier.update(b"jiandu/index/public-cursor-hmac/v1\0");
    verifier.update(&payload_bytes);
    if verifier.verify_slice(&tag).is_err() {
        return Err(IndexError::InvalidCursor);
    }

    let unsigned = UnsignedCursorPayload {
        format_version: &envelope.payload.format_version,
        offset: envelope.payload.offset,
        authority_fingerprint: &envelope.payload.authority_fingerprint,
        request_fingerprint: &envelope.payload.request_fingerprint,
        source_store_id: &envelope.payload.source_store_id,
        source_store_revision: envelope.payload.source_store_revision,
        index_content_checksum: &envelope.payload.index_content_checksum,
    };
    let unsigned_bytes = serde_json::to_vec(&unsigned).map_err(|_| IndexError::InvalidCursor)?;
    let expected_checksum = sha256_hex(b"jiandu/index/cursor-checksum/v1\0", &unsigned_bytes);
    if envelope.payload.format_version != CURSOR_FORMAT_VERSION
        || expected_checksum != envelope.payload.checksum
    {
        return Err(IndexError::InvalidCursor);
    }

    if envelope.payload.authority_fingerprint != binding.authority_fingerprint
        || envelope.payload.request_fingerprint != binding.request_fingerprint
    {
        return Err(IndexError::InvalidCursor);
    }
    if &envelope.payload.source_store_id != binding.source_store_id
        || envelope.payload.source_store_revision != binding.source_store_revision
        || envelope.payload.index_content_checksum != binding.index_content_checksum
    {
        return Err(IndexError::StaleCursor);
    }
    Ok(envelope.payload.offset)
}

fn hmac_hex(key: &CursorMacKey, message: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(&key.0).expect("HMAC accepts a 32-byte key");
    mac.update(b"jiandu/index/public-cursor-hmac/v1\0");
    mac.update(message);
    lower_hex(&mac.finalize().into_bytes())
}

fn sha256_hex(domain: &[u8], value: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(value);
    lower_hex(&hasher.finalize())
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn decode_lower_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() != 64 {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_value(pair[0])?;
            let low = hex_value(pair[1])?;
            Some((high << 4) | low)
        })
        .collect()
}

const fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiandu_core::StoreRevision;

    fn binding<'a>(store_id: &'a StoreId) -> CursorBinding<'a> {
        CursorBinding {
            authority_fingerprint: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            request_fingerprint: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            source_store_id: store_id,
            source_store_revision: StoreRevision(9),
            index_content_checksum: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        }
    }

    #[test]
    fn hmac_authentication_is_distinct_from_checksum_and_binds_authority_and_watermark() {
        let store_id = StoreId::new("00000000-0000-4000-8000-000000000006").expect("store ID");
        let key = CursorMacKey::new([0x11; 32]);
        let cursor = encode_cursor(&key, 7, &binding(&store_id)).expect("cursor");
        assert!(matches!(
            decode_cursor(&key, &cursor, &binding(&store_id)),
            Ok(7)
        ));
        assert!(matches!(
            decode_cursor(&CursorMacKey::new([0x12; 32]), &cursor, &binding(&store_id)),
            Err(IndexError::InvalidCursor)
        ));

        let bytes = URL_SAFE_NO_PAD
            .decode(cursor.as_str())
            .expect("cursor bytes");
        let mut envelope: CursorEnvelope = serde_json::from_slice(&bytes).expect("cursor envelope");
        envelope.payload.offset = 8;
        let unsigned = UnsignedCursorPayload {
            format_version: &envelope.payload.format_version,
            offset: envelope.payload.offset,
            authority_fingerprint: &envelope.payload.authority_fingerprint,
            request_fingerprint: &envelope.payload.request_fingerprint,
            source_store_id: &envelope.payload.source_store_id,
            source_store_revision: envelope.payload.source_store_revision,
            index_content_checksum: &envelope.payload.index_content_checksum,
        };
        envelope.payload.checksum = sha256_hex(
            b"jiandu/index/cursor-checksum/v1\0",
            &serde_json::to_vec(&unsigned).expect("unsigned payload"),
        );
        let tampered = PageCursor::new(
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&envelope).expect("tampered envelope")),
        )
        .expect("tampered token shape");
        assert!(matches!(
            decode_cursor(&key, &tampered, &binding(&store_id)),
            Err(IndexError::InvalidCursor)
        ));

        let changed_authority = CursorBinding {
            authority_fingerprint: "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            ..binding(&store_id)
        };
        assert!(matches!(
            decode_cursor(&key, &cursor, &changed_authority),
            Err(IndexError::InvalidCursor)
        ));
        let changed_watermark = CursorBinding {
            source_store_revision: StoreRevision(10),
            ..binding(&store_id)
        };
        assert!(matches!(
            decode_cursor(&key, &cursor, &changed_watermark),
            Err(IndexError::StaleCursor)
        ));
    }
}
