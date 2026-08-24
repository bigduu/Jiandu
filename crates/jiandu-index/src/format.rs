//! Strict deterministic representation stored inside the disposable SQLite
//! carrier.

use crate::error::{IndexDegradedReason, IndexError};
use crate::{INDEX_FORMAT_VERSION, tokenize};
use jiandu_core::{
    MemoryRecord, MemoryScope, MemoryStatus, MemorySummary, MemoryType, StoreRevision, Validate,
};
use jiandu_store::StoreId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub(crate) const INDEX_SQLITE_USER_VERSION: i64 = 1;
pub(crate) const MAX_INDEX_DOCUMENTS: usize = 10_000;
pub(crate) const MAX_TERMS_PER_DOCUMENT: usize = 131_072;
pub(crate) const MAX_DOCUMENT_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const MAX_METADATA_BYTES: usize = 16 * 1024;
pub(crate) const MAX_INDEX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Deterministic field weights. Frequency is retained, and the final public
/// score is normalized against the highest score in the complete result set.
pub const TITLE_WEIGHT: u32 = 12;
pub const TAG_WEIGHT: u32 = 10;
pub const SUMMARY_WEIGHT: u32 = 8;
pub const BODY_WEIGHT: u32 = 4;
pub const TYPE_WEIGHT: u32 = 2;
pub const STATUS_WEIGHT: u32 = 2;
pub const SCOPE_WEIGHT: u32 = 2;
pub const UPDATE_METADATA_WEIGHT: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct IndexMetadata {
    pub format_version: String,
    pub source_store_id: StoreId,
    pub source_store_revision: StoreRevision,
    pub document_count: u64,
    pub content_checksum: String,
}

impl IndexMetadata {
    pub(crate) fn new(
        source_store_id: StoreId,
        source_store_revision: StoreRevision,
        documents: &[IndexDocument],
    ) -> Result<Self, IndexError> {
        let document_count =
            u64::try_from(documents.len()).map_err(|_| IndexError::InvalidRequest)?;
        let content_checksum = index_checksum(
            INDEX_FORMAT_VERSION,
            &source_store_id,
            source_store_revision,
            documents,
        )?;
        Ok(Self {
            format_version: INDEX_FORMAT_VERSION.to_owned(),
            source_store_id,
            source_store_revision,
            document_count,
            content_checksum,
        })
    }

    pub(crate) fn validate_shape(&self) -> Result<(), IndexDegradedReason> {
        if self.format_version != INDEX_FORMAT_VERSION {
            return Err(IndexDegradedReason::IncompatibleVersion);
        }
        if self.document_count > MAX_INDEX_DOCUMENTS as u64
            || !is_lower_sha256(&self.content_checksum)
        {
            return Err(IndexDegradedReason::Corrupt);
        }
        Ok(())
    }

    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, IndexError> {
        let bytes = serde_json::to_vec(self).map_err(|_| IndexError::InvalidRequest)?;
        if bytes.len() > MAX_METADATA_BYTES {
            return Err(IndexError::InvalidRequest);
        }
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, IndexDegradedReason> {
        if bytes.len() > MAX_METADATA_BYTES {
            return Err(IndexDegradedReason::Corrupt);
        }
        let value: Self =
            serde_json::from_slice(bytes).map_err(|_| IndexDegradedReason::Corrupt)?;
        value.validate_shape()?;
        if value
            .canonical_bytes()
            .map_err(|_| IndexDegradedReason::Corrupt)?
            != bytes
        {
            return Err(IndexDegradedReason::Corrupt);
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WeightedTerm {
    pub token: String,
    pub weight: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct IndexDocument {
    pub summary: MemorySummary,
    pub scope_key: String,
    pub terms: Vec<WeightedTerm>,
}

impl IndexDocument {
    pub(crate) fn from_record(record: MemoryRecord) -> Result<Self, IndexError> {
        record.validate().map_err(|_| IndexError::InvalidRequest)?;
        let mut terms = BTreeMap::<String, u32>::new();
        add_terms(&mut terms, &record.title, TITLE_WEIGHT)?;
        if let Some(summary) = &record.summary {
            add_terms(&mut terms, summary, SUMMARY_WEIGHT)?;
        }
        add_terms(&mut terms, &record.body, BODY_WEIGHT)?;
        for tag in &record.tags {
            add_terms(&mut terms, tag.as_str(), TAG_WEIGHT)?;
        }
        add_terms(
            &mut terms,
            memory_type_name(record.memory_type),
            TYPE_WEIGHT,
        )?;
        add_terms(&mut terms, memory_status_name(record.status), STATUS_WEIGHT)?;
        add_terms(&mut terms, &scope_search_text(&record.scope), SCOPE_WEIGHT)?;
        add_terms(
            &mut terms,
            &format!(
                "{} revision {}",
                record.updated_at.as_str(),
                record.revision.get()
            ),
            UPDATE_METADATA_WEIGHT,
        )?;

        if terms.len() > MAX_TERMS_PER_DOCUMENT {
            return Err(IndexError::InvalidRequest);
        }
        let document = Self {
            scope_key: scope_key(&record.scope),
            summary: MemorySummary {
                id: record.id,
                revision: record.revision,
                etag: record.etag,
                scope: record.scope,
                memory_type: record.memory_type,
                status: record.status,
                title: record.title,
                summary: record.summary,
                tags: record.tags,
                updated_at: record.updated_at,
            },
            terms: terms
                .into_iter()
                .map(|(token, weight)| WeightedTerm { token, weight })
                .collect(),
        };
        if document.validate().is_err() {
            return Err(IndexError::InvalidRequest);
        }
        Ok(document)
    }

    pub(crate) fn validate(&self) -> Result<(), IndexDegradedReason> {
        self.summary
            .validate()
            .map_err(|_| IndexDegradedReason::Corrupt)?;
        if self.scope_key != scope_key(&self.summary.scope)
            || self.terms.len() > MAX_TERMS_PER_DOCUMENT
            || self
                .terms
                .windows(2)
                .any(|pair| pair[0].token >= pair[1].token)
            || self.terms.iter().any(|term| {
                term.token.is_empty()
                    || term.weight == 0
                    || !tokenize(&term.token).contains(&term.token)
            })
        {
            return Err(IndexDegradedReason::Corrupt);
        }
        Ok(())
    }

    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, IndexError> {
        let bytes = serde_json::to_vec(self).map_err(|_| IndexError::InvalidRequest)?;
        if bytes.len() > MAX_DOCUMENT_BYTES {
            return Err(IndexError::InvalidRequest);
        }
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, IndexDegradedReason> {
        if bytes.len() > MAX_DOCUMENT_BYTES {
            return Err(IndexDegradedReason::Corrupt);
        }
        let value: Self =
            serde_json::from_slice(bytes).map_err(|_| IndexDegradedReason::Corrupt)?;
        value.validate()?;
        if value
            .canonical_bytes()
            .map_err(|_| IndexDegradedReason::Corrupt)?
            != bytes
        {
            return Err(IndexDegradedReason::Corrupt);
        }
        Ok(value)
    }
}

pub(crate) fn build_documents(
    records: Vec<MemoryRecord>,
) -> Result<Vec<IndexDocument>, IndexError> {
    if records.len() > MAX_INDEX_DOCUMENTS {
        return Err(IndexError::InvalidRequest);
    }
    let mut documents = records
        .into_iter()
        .map(IndexDocument::from_record)
        .collect::<Result<Vec<_>, _>>()?;
    documents.sort_by(|left, right| left.summary.id.cmp(&right.summary.id));
    if documents
        .windows(2)
        .any(|pair| pair[0].summary.id == pair[1].summary.id)
    {
        return Err(IndexError::InvalidRequest);
    }
    Ok(documents)
}

pub(crate) fn index_checksum(
    format_version: &str,
    source_store_id: &StoreId,
    source_store_revision: StoreRevision,
    documents: &[IndexDocument],
) -> Result<String, IndexError> {
    let mut hasher = Sha256::new();
    hasher.update(b"jiandu/lexical-index/content/v1\0");
    add_hash_field(&mut hasher, format_version.as_bytes());
    add_hash_field(&mut hasher, source_store_id.as_str().as_bytes());
    hasher.update(source_store_revision.0.to_be_bytes());
    for document in documents {
        let bytes = document.canonical_bytes()?;
        add_hash_field(&mut hasher, &bytes);
    }
    Ok(lower_hex(&hasher.finalize()))
}

fn add_hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

pub(crate) fn scope_key(scope: &MemoryScope) -> String {
    match scope {
        MemoryScope::Principal { principal_id } => format!("principal:{}", principal_id.as_str()),
        MemoryScope::Project { project_id } => format!("project:{}", project_id.as_str()),
        MemoryScope::Session { session_id } => format!("session:{}", session_id.as_str()),
        MemoryScope::InstanceGlobal {} => "instance_global:".to_owned(),
    }
}

fn scope_search_text(scope: &MemoryScope) -> String {
    match scope {
        MemoryScope::Principal { principal_id } => format!("principal {}", principal_id.as_str()),
        MemoryScope::Project { project_id } => format!("project {}", project_id.as_str()),
        MemoryScope::Session { session_id } => format!("session {}", session_id.as_str()),
        MemoryScope::InstanceGlobal {} => "instance global".to_owned(),
    }
}

fn add_terms(terms: &mut BTreeMap<String, u32>, text: &str, weight: u32) -> Result<(), IndexError> {
    for token in tokenize(text) {
        let entry = terms.entry(token).or_default();
        *entry = entry
            .checked_add(weight)
            .ok_or(IndexError::InvalidRequest)?;
    }
    Ok(())
}

const fn memory_type_name(value: MemoryType) -> &'static str {
    match value {
        MemoryType::Preference => "preference",
        MemoryType::Decision => "decision",
        MemoryType::Project => "project",
        MemoryType::Fact => "fact",
        MemoryType::Feedback => "feedback",
        MemoryType::Reference => "reference",
    }
}

const fn memory_status_name(value: MemoryStatus) -> &'static str {
    match value {
        MemoryStatus::Active => "active",
        MemoryStatus::Stale => "stale",
        MemoryStatus::Superseded => "superseded",
        MemoryStatus::Contradicted => "contradicted",
        MemoryStatus::Archived => "archived",
    }
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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
