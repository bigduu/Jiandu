//! SQLite-backed disposable index, rebuild, diagnostics, and ranked query.

use crate::cursor::{CursorBinding, CursorMacKey, decode_cursor, encode_cursor};
use crate::directory::{DirectoryOpenError, INDEX_FILE_NAME, IndexDirectory};
use crate::error::{IndexDegradedReason, IndexError};
use crate::format::{
    INDEX_SQLITE_USER_VERSION, IndexDocument, IndexMetadata, MAX_INDEX_DOCUMENTS, build_documents,
    index_checksum, scope_key,
};
use crate::{CanonicalRecordReader, tokenize};
use jiandu_core::{
    MemorySearchRequest, MemorySearchResult, MemorySummary, RankedMemorySummary, SearchDiagnostics,
    SearchScore, StoreRevision, Timestamp, Validate,
};
use jiandu_store::{AuthorizedIndexAdmin, AuthorizedIndexQuery, StoreId};
use rusqlite::{Connection, MAIN_DB, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const SQLITE_APPLICATION_ID: i64 = 0x4a49_4458;

/// Path-free compatibility and freshness marker for the derived index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexWatermark {
    pub format_version: String,
    pub source_store_id: StoreId,
    pub source_store_revision: StoreRevision,
    pub document_count: u64,
    pub content_checksum: String,
}

impl From<&IndexMetadata> for IndexWatermark {
    fn from(metadata: &IndexMetadata) -> Self {
        Self {
            format_version: metadata.format_version.clone(),
            source_store_id: metadata.source_store_id.clone(),
            source_store_revision: metadata.source_store_revision,
            document_count: metadata.document_count,
            content_checksum: metadata.content_checksum.clone(),
        }
    }
}

/// Observable derived-index readiness. A degraded index never changes or
/// blocks exact canonical-store reads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IndexHealth {
    Ready(IndexWatermark),
    Degraded { reason: IndexDegradedReason },
}

/// Path-free administrative diagnostic result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexDiagnostic {
    pub health: IndexHealth,
    pub rebuild_supported: bool,
}

/// Successful all-store rebuild result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexRebuildReport {
    pub watermark: IndexWatermark,
    pub replaced_existing: bool,
}

/// Disposable lexical index at a host-configured private path. Debug output
/// intentionally omits that ambient path.
pub struct LexicalIndex {
    directory: PathBuf,
}

impl fmt::Debug for LexicalIndex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LexicalIndex")
            .field("path", &"[REDACTED]")
            .finish()
    }
}

impl LexicalIndex {
    /// Configure an existing private derived-index directory. The canonical
    /// store layout provisions this directory; this crate never creates or
    /// chmods an ambient parent. The filename is fixed internally, and
    /// construction itself performs no I/O.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    /// Rebuild the single all-store index from one stable canonical snapshot.
    /// The separate admin capability prevents a tenant-scoped caller from
    /// permanently omitting another tenant. The SQLite image is built in a
    /// separate private directory, copied into a create-new file relative to
    /// one held directory capability, synced, then atomically renamed.
    pub fn rebuild<R: CanonicalRecordReader>(
        &self,
        reader: &R,
        authorization: &AuthorizedIndexAdmin,
    ) -> Result<IndexRebuildReport, IndexError> {
        let snapshot = reader.read_index_snapshot(authorization)?;
        let documents = build_documents(snapshot.records)?;
        let metadata = IndexMetadata::new(snapshot.store_id, snapshot.store_revision, &documents)?;
        let directory = IndexDirectory::open(&self.directory).map_err(rebuild_directory_error)?;
        #[cfg(test)]
        crate::directory::run_test_hook(crate::directory::TestHookPoint::AfterDirectoryOpen);
        let replacement_target = directory.replacement_target()?;

        let build_directory =
            TempDir::new().map_err(|_| IndexError::io("create private index build directory"))?;
        let build_path = build_directory.path().join(INDEX_FILE_NAME);
        write_database(&build_path, &metadata, &documents)?;
        sync_file(&build_path)?;
        directory.publish(&build_path, replacement_target)?;

        Ok(IndexRebuildReport {
            watermark: IndexWatermark::from(&metadata),
            replaced_existing: replacement_target.existed(),
        })
    }

    /// Inspect format, SQLite integrity, exact contents, checksum, and store
    /// freshness under the same operator-only administrative capability used
    /// for rebuild. This prevents all-store counts and watermarks from
    /// becoming an unauthenticated diagnostic side channel.
    pub fn diagnose<R: CanonicalRecordReader>(
        &self,
        reader: &R,
        _authorization: &AuthorizedIndexAdmin,
    ) -> Result<IndexDiagnostic, IndexError> {
        let current = match reader.current_store_watermark() {
            Ok(current) => current,
            Err(_) => {
                return Ok(IndexDiagnostic {
                    health: IndexHealth::Degraded {
                        reason: IndexDegradedReason::SourceUnavailable,
                    },
                    rebuild_supported: true,
                });
            }
        };
        let directory = match IndexDirectory::open(&self.directory) {
            Ok(directory) => directory,
            Err(error) => {
                return Ok(IndexDiagnostic {
                    health: IndexHealth::Degraded {
                        reason: directory_degraded_reason(error),
                    },
                    rebuild_supported: true,
                });
            }
        };
        let health = match load_and_validate_all(&directory) {
            Ok((metadata, _))
                if metadata.source_store_id == current.0
                    && metadata.source_store_revision == current.1 =>
            {
                IndexHealth::Ready(IndexWatermark::from(&metadata))
            }
            Ok(_) => IndexHealth::Degraded {
                reason: IndexDegradedReason::Stale,
            },
            Err(reason) => IndexHealth::Degraded { reason },
        };
        Ok(IndexDiagnostic {
            health,
            rebuild_supported: true,
        })
    }

    /// Search only the exact scopes in a fresh unforgeable store capability.
    /// The complete private derived image is integrity-checked first; only
    /// documents intersecting the authorized scope set can then produce hits.
    /// Store and index watermarks are checked before and after reading, so a
    /// concurrent mutation yields a safe stale failure rather than a mixed
    /// page.
    pub fn search<R: CanonicalRecordReader>(
        &self,
        reader: &R,
        authorization: &AuthorizedIndexQuery,
        request: &MemorySearchRequest,
        cursor_key: &CursorMacKey,
    ) -> Result<MemorySearchResult, IndexError> {
        request.validate().map_err(|_| IndexError::InvalidRequest)?;
        if !authorization.matches_selectors(&request.scopes) {
            return Err(IndexError::Forbidden);
        }
        let query_tokens = tokenize(&request.query)
            .into_iter()
            .collect::<BTreeSet<_>>();
        if query_tokens.is_empty() {
            return Err(IndexError::InvalidRequest);
        }

        let begin = reader.current_store_watermark().map_err(IndexError::from)?;
        let directory =
            IndexDirectory::open(&self.directory).map_err(|error| IndexError::Degraded {
                reason: directory_degraded_reason(error),
            })?;
        let (metadata, documents) =
            open_validated_all(&directory).map_err(|reason| IndexError::Degraded { reason })?;
        if metadata.source_store_id != begin.0 || metadata.source_store_revision != begin.1 {
            return Err(IndexError::Degraded {
                reason: IndexDegradedReason::Stale,
            });
        }
        let request_fingerprint = request_fingerprint(request, authorization, &query_tokens)?;
        let binding = CursorBinding {
            authority_fingerprint: authorization.authority_fingerprint(),
            request_fingerprint: &request_fingerprint,
            source_store_id: &metadata.source_store_id,
            source_store_revision: metadata.source_store_revision,
            index_content_checksum: &metadata.content_checksum,
        };
        let offset = request.cursor.as_ref().map_or(Ok(0_u32), |cursor| {
            decode_cursor(cursor_key, cursor, &binding)
        })?;

        let authorized_scope_keys = authorization
            .scopes()
            .iter()
            .map(scope_key)
            .collect::<BTreeSet<_>>();
        let mut matches = documents
            .into_iter()
            .filter(|document| authorized_scope_keys.contains(&document.scope_key))
            .filter(|document| matches_filters(&document.summary, request))
            .filter_map(|document| {
                let raw_score = raw_score(&document, &query_tokens);
                (raw_score > 0).then_some((document.summary, raw_score))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            right
                .1
                .cmp(&left.1)
                .then_with(|| left.0.id.cmp(&right.0.id))
        });

        let offset = usize::try_from(offset).map_err(|_| IndexError::InvalidCursor)?;
        if offset > matches.len() {
            return Err(IndexError::InvalidCursor);
        }
        let limit = usize::from(request.limit.get());
        let end = offset.saturating_add(limit).min(matches.len());
        let max_score = matches.first().map_or(1_u32, |entry| entry.1);
        let memories = matches[offset..end]
            .iter()
            .map(|(summary, raw)| {
                let score = SearchScore::new(f64::from(*raw) / f64::from(max_score))
                    .map_err(|_| IndexError::InvalidRequest)?;
                Ok(RankedMemorySummary::from_summary(summary.clone(), score))
            })
            .collect::<Result<Vec<_>, IndexError>>()?;
        let has_more = end < matches.len();
        let next_cursor = if has_more {
            Some(encode_cursor(
                cursor_key,
                u32::try_from(end).map_err(|_| IndexError::InvalidCursor)?,
                &binding,
            )?)
        } else {
            None
        };

        directory
            .validate_ambient_identity()
            .map_err(|_| IndexError::Degraded {
                reason: IndexDegradedReason::Corrupt,
            })?;
        let finish = reader.current_store_watermark().map_err(IndexError::from)?;
        if begin != finish {
            return Err(IndexError::Degraded {
                reason: IndexDegradedReason::Stale,
            });
        }
        let result = MemorySearchResult {
            memories,
            next_cursor,
            has_more,
            diagnostics: SearchDiagnostics {
                index_degraded: false,
                warnings: Vec::new(),
            },
        };
        result.validate().map_err(|_| IndexError::InvalidRequest)?;
        Ok(result)
    }
}

fn write_database(
    path: &Path,
    metadata: &IndexMetadata,
    documents: &[IndexDocument],
) -> Result<(), IndexError> {
    let mut connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )
    .map_err(|_| IndexError::io("open index temporary database"))?;
    connection
        .execute_batch(&format!(
            "PRAGMA page_size=4096;
             PRAGMA journal_mode=DELETE;
             PRAGMA synchronous=FULL;
             PRAGMA foreign_keys=ON;
             PRAGMA trusted_schema=OFF;
             PRAGMA application_id={SQLITE_APPLICATION_ID};
             PRAGMA user_version={INDEX_SQLITE_USER_VERSION};"
        ))
        .map_err(|_| IndexError::io("initialize index pragmas"))?;
    {
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| IndexError::io("start index rebuild transaction"))?;
        transaction
            .execute_batch(
                "CREATE TABLE index_metadata (
                 singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                 value BLOB NOT NULL
             ) STRICT;
             CREATE TABLE documents (
                 memory_id TEXT PRIMARY KEY NOT NULL,
                 scope_key TEXT NOT NULL,
                 value BLOB NOT NULL
             ) STRICT;
             CREATE INDEX documents_by_scope ON documents(scope_key, memory_id);",
            )
            .map_err(|_| IndexError::io("initialize index schema"))?;
        transaction
            .execute(
                "INSERT INTO index_metadata(singleton, value) VALUES (1, ?1)",
                params![metadata.canonical_bytes()?],
            )
            .map_err(|_| IndexError::io("write index metadata"))?;
        {
            let mut statement = transaction
                .prepare("INSERT INTO documents(memory_id, scope_key, value) VALUES (?1, ?2, ?3)")
                .map_err(|_| IndexError::io("prepare index document insert"))?;
            for document in documents {
                statement
                    .execute(params![
                        document.summary.id.as_str(),
                        &document.scope_key,
                        document.canonical_bytes()?
                    ])
                    .map_err(|_| IndexError::io("write index document"))?;
            }
        }
        transaction
            .commit()
            .map_err(|_| IndexError::io("commit rebuilt index"))?;
    }
    connection
        .execute_batch("PRAGMA optimize;")
        .map_err(|_| IndexError::io("optimize rebuilt index"))?;
    connection
        .close()
        .map_err(|_| IndexError::io("close rebuilt index"))?;
    Ok(())
}

fn load_and_validate_all(
    directory: &IndexDirectory,
) -> Result<(IndexMetadata, Vec<IndexDocument>), IndexDegradedReason> {
    open_validated_all(directory)
}

fn open_validated_all(
    directory: &IndexDirectory,
) -> Result<(IndexMetadata, Vec<IndexDocument>), IndexDegradedReason> {
    let mut image = directory.open_image()?;
    let image_length = usize::try_from(image.length).map_err(|_| IndexDegradedReason::Corrupt)?;
    let mut connection = Connection::open_in_memory().map_err(|_| IndexDegradedReason::Corrupt)?;
    connection
        .deserialize_read_exact(MAIN_DB, &mut image.file, image_length, true)
        .map_err(|_| IndexDegradedReason::Corrupt)?;
    directory.revalidate_open_image(&image)?;
    connection
        .execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF; BEGIN;")
        .map_err(|_| IndexDegradedReason::Corrupt)?;
    validate_sqlite_header(&connection)?;
    validate_schema(&connection)?;
    let metadata = read_metadata(&connection)?;
    let documents = read_all_documents(&connection)?;
    if metadata.document_count != documents.len() as u64
        || documents
            .iter()
            .any(|document| document.summary.revision.get() > metadata.source_store_revision.0)
        || metadata.content_checksum
            != index_checksum(
                &metadata.format_version,
                &metadata.source_store_id,
                metadata.source_store_revision,
                &documents,
            )
            .map_err(|_| IndexDegradedReason::Corrupt)?
    {
        return Err(IndexDegradedReason::Corrupt);
    }
    directory.revalidate_open_image(&image)?;
    directory
        .validate_ambient_identity()
        .map_err(|_| IndexDegradedReason::Corrupt)?;
    connection
        .close()
        .map_err(|_| IndexDegradedReason::Corrupt)?;
    Ok((metadata, documents))
}

fn validate_sqlite_header(connection: &Connection) -> Result<(), IndexDegradedReason> {
    let application_id: i64 = connection
        .query_row("PRAGMA application_id", [], |row| row.get(0))
        .map_err(|_| IndexDegradedReason::Corrupt)?;
    let user_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|_| IndexDegradedReason::Corrupt)?;
    if application_id != SQLITE_APPLICATION_ID {
        return Err(IndexDegradedReason::Corrupt);
    }
    if user_version != INDEX_SQLITE_USER_VERSION {
        return Err(IndexDegradedReason::IncompatibleVersion);
    }
    let integrity: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|_| IndexDegradedReason::Corrupt)?;
    if integrity != "ok" {
        return Err(IndexDegradedReason::Corrupt);
    }
    Ok(())
}

fn validate_schema(connection: &Connection) -> Result<(), IndexDegradedReason> {
    let mut statement = connection
        .prepare(
            "SELECT type, name FROM sqlite_schema
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
        )
        .map_err(|_| IndexDegradedReason::Corrupt)?;
    let actual = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|_| IndexDegradedReason::Corrupt)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| IndexDegradedReason::Corrupt)?;
    let expected = vec![
        ("index".to_owned(), "documents_by_scope".to_owned()),
        ("table".to_owned(), "documents".to_owned()),
        ("table".to_owned(), "index_metadata".to_owned()),
    ];
    if actual != expected {
        return Err(IndexDegradedReason::Corrupt);
    }
    let expected_metadata_sql = normalize_schema_sql(
        "CREATE TABLE index_metadata (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
            value BLOB NOT NULL
        ) STRICT",
    );
    let expected_documents_sql = normalize_schema_sql(
        "CREATE TABLE documents (
            memory_id TEXT PRIMARY KEY NOT NULL,
            scope_key TEXT NOT NULL,
            value BLOB NOT NULL
        ) STRICT",
    );
    let expected_index_sql =
        normalize_schema_sql("CREATE INDEX documents_by_scope ON documents(scope_key, memory_id)");
    if schema_sql(connection, "table", "index_metadata")? != expected_metadata_sql
        || schema_sql(connection, "table", "documents")? != expected_documents_sql
        || schema_sql(connection, "index", "documents_by_scope")? != expected_index_sql
    {
        return Err(IndexDegradedReason::Corrupt);
    }
    if table_columns(connection, "index_metadata")?
        != [
            ("singleton".to_owned(), "INTEGER".to_owned(), false, 1),
            ("value".to_owned(), "BLOB".to_owned(), true, 0),
        ]
        || table_columns(connection, "documents")?
            != [
                ("memory_id".to_owned(), "TEXT".to_owned(), true, 1),
                ("scope_key".to_owned(), "TEXT".to_owned(), true, 0),
                ("value".to_owned(), "BLOB".to_owned(), true, 0),
            ]
    {
        return Err(IndexDegradedReason::Corrupt);
    }
    let mut index_statement = connection
        .prepare("PRAGMA index_info(documents_by_scope)")
        .map_err(|_| IndexDegradedReason::Corrupt)?;
    let index_columns = index_statement
        .query_map([], |row| row.get::<_, String>(2))
        .map_err(|_| IndexDegradedReason::Corrupt)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| IndexDegradedReason::Corrupt)?;
    if index_columns != ["scope_key", "memory_id"] {
        return Err(IndexDegradedReason::Corrupt);
    }
    Ok(())
}

fn schema_sql(
    connection: &Connection,
    object_type: &str,
    name: &str,
) -> Result<String, IndexDegradedReason> {
    let sql = connection
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type = ?1 AND name = ?2",
            params![object_type, name],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| IndexDegradedReason::Corrupt)?;
    Ok(normalize_schema_sql(&sql))
}

fn normalize_schema_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn table_columns(
    connection: &Connection,
    table: &str,
) -> Result<Vec<(String, String, bool, i64)>, IndexDegradedReason> {
    let sql = format!("PRAGMA table_info({table})");
    let mut statement = connection
        .prepare(&sql)
        .map_err(|_| IndexDegradedReason::Corrupt)?;
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? != 0,
                row.get::<_, i64>(5)?,
            ))
        })
        .map_err(|_| IndexDegradedReason::Corrupt)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| IndexDegradedReason::Corrupt)
}

fn read_metadata(connection: &Connection) -> Result<IndexMetadata, IndexDegradedReason> {
    let bytes = connection
        .query_row(
            "SELECT value FROM index_metadata WHERE singleton = 1",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(|_| IndexDegradedReason::Corrupt)?
        .ok_or(IndexDegradedReason::Corrupt)?;
    let row_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM index_metadata", [], |row| row.get(0))
        .map_err(|_| IndexDegradedReason::Corrupt)?;
    if row_count != 1 {
        return Err(IndexDegradedReason::Corrupt);
    }
    IndexMetadata::decode(&bytes)
}

fn read_all_documents(connection: &Connection) -> Result<Vec<IndexDocument>, IndexDegradedReason> {
    let mut statement = connection
        .prepare("SELECT memory_id, scope_key, value FROM documents ORDER BY memory_id")
        .map_err(|_| IndexDegradedReason::Corrupt)?;
    let mut rows = statement
        .query([])
        .map_err(|_| IndexDegradedReason::Corrupt)?;
    let mut documents = Vec::new();
    while let Some(row) = rows.next().map_err(|_| IndexDegradedReason::Corrupt)? {
        if documents.len() == MAX_INDEX_DOCUMENTS {
            return Err(IndexDegradedReason::Corrupt);
        }
        let memory_id = row
            .get::<_, String>(0)
            .map_err(|_| IndexDegradedReason::Corrupt)?;
        let stored_scope = row
            .get::<_, String>(1)
            .map_err(|_| IndexDegradedReason::Corrupt)?;
        let bytes = row
            .get::<_, Vec<u8>>(2)
            .map_err(|_| IndexDegradedReason::Corrupt)?;
        let document = IndexDocument::decode(&bytes)?;
        if document.summary.id.as_str() != memory_id || document.scope_key != stored_scope {
            return Err(IndexDegradedReason::Corrupt);
        }
        documents.push(document);
    }
    Ok(documents)
}

fn raw_score(document: &IndexDocument, query_tokens: &BTreeSet<String>) -> u32 {
    document
        .terms
        .iter()
        .filter(|term| query_tokens.contains(&term.token))
        .fold(0_u32, |score, term| score.saturating_add(term.weight))
}

fn matches_filters(summary: &MemorySummary, request: &MemorySearchRequest) -> bool {
    (request.types.is_empty() || request.types.contains(&summary.memory_type))
        && (request.statuses.is_empty() || request.statuses.contains(&summary.status))
        && request
            .tags
            .iter()
            .all(|requested| summary.tags.contains(requested))
        && request.updated_after.as_ref().is_none_or(|watermark| {
            timestamp_nanos(&summary.updated_at) > timestamp_nanos(watermark)
        })
}

fn timestamp_nanos(timestamp: &Timestamp) -> i128 {
    OffsetDateTime::parse(timestamp.as_str(), &Rfc3339)
        .map_or(i128::MIN, |value| value.unix_timestamp_nanos())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RequestBinding<'a> {
    query_tokens: &'a BTreeSet<String>,
    scope_keys: Vec<String>,
    types: Vec<String>,
    statuses: Vec<String>,
    tags: Vec<&'a str>,
    updated_after: Option<&'a str>,
    limit: u16,
}

fn request_fingerprint(
    request: &MemorySearchRequest,
    authorization: &AuthorizedIndexQuery,
    query_tokens: &BTreeSet<String>,
) -> Result<String, IndexError> {
    let mut types = request
        .types
        .iter()
        .map(|value| format!("{value:?}").to_ascii_lowercase())
        .collect::<Vec<_>>();
    types.sort();
    let mut statuses = request
        .statuses
        .iter()
        .map(|value| format!("{value:?}").to_ascii_lowercase())
        .collect::<Vec<_>>();
    statuses.sort();
    let mut tags = request
        .tags
        .iter()
        .map(|tag| tag.as_str())
        .collect::<Vec<_>>();
    tags.sort_unstable();
    let mut scope_keys = authorization
        .scopes()
        .iter()
        .map(scope_key)
        .collect::<Vec<_>>();
    scope_keys.sort();
    let binding = RequestBinding {
        query_tokens,
        scope_keys,
        types,
        statuses,
        tags,
        updated_after: request.updated_after.as_ref().map(Timestamp::as_str),
        limit: request.limit.get(),
    };
    let bytes = serde_json::to_vec(&binding).map_err(|_| IndexError::InvalidRequest)?;
    let mut hasher = Sha256::new();
    hasher.update(b"jiandu/index/search-request/v1\0");
    hasher.update(bytes);
    Ok(lower_hex(&hasher.finalize()))
}

fn sync_file(path: &Path) -> Result<(), IndexError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|_| IndexError::io("sync rebuilt index"))
}

fn rebuild_directory_error(error: DirectoryOpenError) -> IndexError {
    match error {
        DirectoryOpenError::Missing | DirectoryOpenError::Unsafe => IndexError::InvalidRequest,
        DirectoryOpenError::Io => IndexError::io("open private index directory"),
    }
}

const fn directory_degraded_reason(error: DirectoryOpenError) -> IndexDegradedReason {
    match error {
        DirectoryOpenError::Missing => IndexDegradedReason::Missing,
        DirectoryOpenError::Unsafe | DirectoryOpenError::Io => IndexDegradedReason::Corrupt,
    }
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
