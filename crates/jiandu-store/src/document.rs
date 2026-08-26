//! Strict canonical Markdown encoding and decoding.

use crate::{InvalidRecordReason, StoreError};
use jiandu_core::{Etag, MemoryFrontmatterV1Alpha1, MemoryId, MemoryRecord, Validate};
use sha2::{Digest, Sha256};

const FRONTMATTER_START: &str = "---\n";
const FRONTMATTER_END: &str = "\n---\n";
pub(crate) const MAX_CANONICAL_DOCUMENT_BYTES: usize = 1_048_576;

/// A validated canonical document and its API record projection.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CanonicalDocument {
    pub record: MemoryRecord,
}

/// Canonicalize one trusted host-adapter record projection without opening a
/// store.
///
/// Migration adapters use this seam to generate the exact revision-bound ETag
/// that the canonical store would validate. The operation is pure: it encodes
/// and decodes the committed Markdown representation in memory and performs no
/// filesystem I/O.
pub fn canonical_record_from_document_parts(
    frontmatter: &MemoryFrontmatterV1Alpha1,
    body: &str,
) -> Result<MemoryRecord, StoreError> {
    let bytes = encode_canonical_document(frontmatter, body)?;
    Ok(decode_canonical_document(&bytes, Some(&frontmatter.id))?.record)
}

/// Encode strict frontmatter and an exact API body into canonical bytes.
///
/// The final LF is a file terminator and is not part of `body`. If the API
/// body itself ends in LF, the file therefore ends in two LFs and decoding
/// removes exactly one of them.
pub(crate) fn encode_canonical_document(
    frontmatter: &MemoryFrontmatterV1Alpha1,
    body: &str,
) -> Result<Vec<u8>, StoreError> {
    validate_canonical_body(body, Some(frontmatter.id.clone()))?;
    frontmatter.validate_document(body).map_err(|_| {
        invalid(
            Some(frontmatter.id.clone()),
            InvalidRecordReason::ValidationFailed,
        )
    })?;
    let yaml = serde_yaml_ng::to_string(frontmatter).map_err(|_| {
        invalid(
            Some(frontmatter.id.clone()),
            InvalidRecordReason::MalformedFrontmatter,
        )
    })?;
    if yaml.contains('\r') || !yaml.ends_with('\n') || yaml.starts_with("---\n") {
        return Err(invalid(
            Some(frontmatter.id.clone()),
            InvalidRecordReason::NonCanonicalEncoding,
        ));
    }

    let mut bytes = Vec::with_capacity(FRONTMATTER_START.len() + yaml.len() + 4 + body.len() + 1);
    bytes.extend_from_slice(FRONTMATTER_START.as_bytes());
    bytes.extend_from_slice(yaml.as_bytes());
    bytes.extend_from_slice(b"---\n");
    bytes.extend_from_slice(body.as_bytes());
    bytes.push(b'\n');
    if bytes.len() > MAX_CANONICAL_DOCUMENT_BYTES {
        return Err(invalid(
            Some(frontmatter.id.clone()),
            InvalidRecordReason::ValidationFailed,
        ));
    }
    Ok(bytes)
}

pub(crate) fn decode_canonical_document(
    bytes: &[u8],
    id_hint: Option<&MemoryId>,
) -> Result<CanonicalDocument, StoreError> {
    let hinted = id_hint.cloned();
    if bytes.len() > MAX_CANONICAL_DOCUMENT_BYTES {
        return Err(invalid(hinted, InvalidRecordReason::ValidationFailed));
    }
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) || bytes.contains(&b'\r') {
        return Err(invalid(hinted, InvalidRecordReason::NonCanonicalEncoding));
    }
    if bytes.last() != Some(&b'\n') {
        return Err(invalid(hinted, InvalidRecordReason::Truncated));
    }
    let content = std::str::from_utf8(bytes)
        .map_err(|_| invalid(id_hint.cloned(), InvalidRecordReason::InvalidUtf8))?;
    let without_file_terminator = &content[..content.len() - 1];
    let rest = without_file_terminator
        .strip_prefix(FRONTMATTER_START)
        .ok_or_else(|| invalid(id_hint.cloned(), InvalidRecordReason::NonCanonicalEncoding))?;
    let (yaml, body) = rest
        .split_once(FRONTMATTER_END)
        .ok_or_else(|| invalid(id_hint.cloned(), InvalidRecordReason::Truncated))?;
    let frontmatter: MemoryFrontmatterV1Alpha1 = serde_yaml_ng::from_str(yaml)
        .map_err(|_| invalid(id_hint.cloned(), InvalidRecordReason::MalformedFrontmatter))?;
    frontmatter.validate_document(body).map_err(|_| {
        invalid(
            Some(frontmatter.id.clone()),
            InvalidRecordReason::ValidationFailed,
        )
    })?;

    let canonical = encode_canonical_document(&frontmatter, body)?;
    if canonical != bytes {
        return Err(invalid(
            Some(frontmatter.id.clone()),
            InvalidRecordReason::NonCanonicalEncoding,
        ));
    }
    let etag = etag_for(bytes)?;
    let record = frontmatter.into_record(etag, body.to_owned());
    record.validate().map_err(|_| {
        invalid(
            Some(record.id.clone()),
            InvalidRecordReason::ValidationFailed,
        )
    })?;
    Ok(CanonicalDocument { record })
}

fn validate_canonical_body(body: &str, id: Option<MemoryId>) -> Result<(), StoreError> {
    if body.contains('\r') {
        return Err(invalid(id, InvalidRecordReason::NonCanonicalEncoding));
    }
    Ok(())
}

fn etag_for(bytes: &[u8]) -> Result<Etag, StoreError> {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(7 + digest.len() * 2);
    encoded.push_str("sha256:");
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Etag::new(encoded).map_err(|_| StoreError::InvalidStoreMetadata)
}

fn invalid(id: Option<MemoryId>, reason: InvalidRecordReason) -> StoreError {
    StoreError::InvalidRecord { id, reason }
}
