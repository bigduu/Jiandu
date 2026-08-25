//! Strict private idempotency-result, receipt, and mutation-audit artifacts.
//!
//! Receipt metadata and audit events deliberately contain hashes and typed
//! identity metadata only. The full replay result lives in a separate bounded
//! private artifact for which this crate exposes no enumeration API.

use crate::layout::{self, StoreDirectory};
use crate::transaction;
use crate::{AuditSequence, StoreError, StoreId, StoreMetadata};
use jiandu_core::{
    Etag, IdempotencyKey, MemoryId, MemoryRecord, MemoryScope, PrincipalId, Revision,
    StoreRevision, Timestamp, Validate,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

pub(crate) const RESULT_FORMAT_VERSION: &str = "jiandu.store.mutation-result/v1alpha1";
pub(crate) const FORGET_RESULT_FORMAT_VERSION: &str = "jiandu.store.forget-result/v1alpha1";
pub(crate) const RECEIPT_FORMAT_VERSION: &str = "jiandu.store.idempotency-receipt/v1alpha1";
pub(crate) const FORGET_RECEIPT_FORMAT_VERSION: &str = "jiandu.store.idempotency-receipt/v1alpha2";
pub(crate) const AUDIT_FORMAT_VERSION: &str = "jiandu.store.mutation-audit/v1alpha1";
pub(crate) const FORGET_AUDIT_FORMAT_VERSION: &str = "jiandu.store.mutation-audit/v1alpha2";
pub(crate) const GENESIS_FORMAT_VERSION: &str = "jiandu.store.audit-genesis/v1alpha1";

const MAX_RESULT_BYTES: usize = 1_048_576;
const MAX_SAFE_ARTIFACT_BYTES: usize = 65_536;
const RECEIPT_ID_LENGTH: usize = 64;

/// Mutation operation included in authorization, receipt identity, and audit.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationOperation {
    Create,
    Update,
    Forget,
}

impl MutationOperation {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Forget => "forget",
        }
    }

    pub(crate) const fn required_grant(self, scope: &MemoryScope) -> &'static str {
        match (self, scope) {
            (Self::Create | Self::Update, MemoryScope::Principal { .. }) => {
                "memory:write:principal"
            }
            (Self::Create | Self::Update, MemoryScope::Project { .. }) => "memory:write:project",
            (Self::Create | Self::Update, MemoryScope::Session { .. }) => "memory:write:session",
            (Self::Create | Self::Update, MemoryScope::InstanceGlobal {}) => {
                "memory:write:instance_global"
            }
            (Self::Forget, MemoryScope::Principal { .. }) => "memory:forget:principal",
            (Self::Forget, MemoryScope::Project { .. }) => "memory:forget:project",
            (Self::Forget, MemoryScope::Session { .. }) => "memory:forget:session",
            (Self::Forget, MemoryScope::InstanceGlobal {}) => "memory:forget:instance_global",
        }
    }
}

/// Derived lookup identity. Raw principal and idempotency key values never
/// leave this constructor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReceiptIdentity {
    pub(crate) receipt_id: String,
    pub(crate) principal_digest: String,
    pub(crate) key_digest: String,
}

impl ReceiptIdentity {
    pub(crate) fn derive(
        principal_id: &PrincipalId,
        operation: MutationOperation,
        key: &IdempotencyKey,
    ) -> Self {
        let principal_digest = domain_digest(b"jiandu/principal/v1\0", principal_id.as_str());
        let key_digest = domain_digest(b"jiandu/idempotency-key/v1\0", key.as_str());
        let mut hasher = Sha256::new();
        hasher.update(b"jiandu/receipt-identity/v1\0");
        hasher.update(principal_id.as_str().as_bytes());
        hasher.update([0]);
        hasher.update(operation.as_str().as_bytes());
        hasher.update([0]);
        hasher.update(key_digest.as_bytes());
        let receipt_id = hex_digest(hasher.finalize().as_slice());
        Self {
            receipt_id,
            principal_digest,
            key_digest,
        }
    }
}

/// Fields repeated in every durable artifact so recovery never infers
/// principal, operation, scope, or result identity from a path.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct MutationBinding {
    pub(crate) receipt_id: String,
    pub(crate) transaction_id: String,
    pub(crate) principal_digest: String,
    pub(crate) key_digest: String,
    pub(crate) operation: MutationOperation,
    pub(crate) scope: MemoryScope,
    pub(crate) request_fingerprint: String,
    pub(crate) memory_id: MemoryId,
    pub(crate) target_revision: Revision,
    pub(crate) target_etag: Etag,
    pub(crate) store_revision: StoreRevision,
    pub(crate) audit_sequence: AuditSequence,
}

/// Body-free WAL extension binding the three durable artifacts by digest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct IdempotencyTransaction {
    pub(crate) binding: MutationBinding,
    pub(crate) result_digest: String,
    pub(crate) receipt_digest: String,
    pub(crate) audit_digest: String,
}

impl IdempotencyTransaction {
    pub(crate) fn validate(&self) -> Result<(), StoreError> {
        self.binding.validate()?;
        if !transaction::valid_content_digest(&self.result_digest)
            || !transaction::valid_content_digest(&self.receipt_digest)
            || !transaction::valid_content_digest(&self.audit_digest)
        {
            return Err(StoreError::InvalidTransaction);
        }
        Ok(())
    }
}

impl MutationBinding {
    pub(crate) fn validate(&self) -> Result<(), StoreError> {
        let valid_revision = match self.operation {
            MutationOperation::Create => self.target_revision.get() == 1,
            MutationOperation::Update => self.target_revision.get() > 1,
            MutationOperation::Forget => true,
        };
        if !valid_receipt_id(&self.receipt_id)
            || !transaction::valid_transaction_id(&self.transaction_id)
            || !transaction::valid_content_digest(&self.principal_digest)
            || !transaction::valid_content_digest(&self.key_digest)
            || !transaction::valid_content_digest(&self.request_fingerprint)
            || !transaction::valid_content_digest(self.target_etag.as_str())
            || self.store_revision.0 == 0
            || self.audit_sequence.0 == 0
            || !valid_revision
        {
            return Err(StoreError::InvalidTransaction);
        }
        Ok(())
    }
}

/// Historical body-bearing create/update replay result.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DurableRecordMutationResult {
    pub(crate) format_version: String,
    pub(crate) store_id: StoreId,
    pub(crate) binding: MutationBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) previous_revision: Option<Revision>,
    pub(crate) record: MemoryRecord,
}

impl DurableRecordMutationResult {
    fn validate(&self) -> Result<(), StoreError> {
        self.binding.validate()?;
        self.record
            .validate()
            .map_err(|_| StoreError::InvalidTransaction)?;
        let valid_previous = match (self.binding.operation, self.previous_revision) {
            (MutationOperation::Create, None) => true,
            (MutationOperation::Update, Some(previous)) => previous
                .get()
                .checked_add(1)
                .is_some_and(|next| next == self.binding.target_revision.get()),
            _ => false,
        };
        if self.format_version != RESULT_FORMAT_VERSION
            || !valid_previous
            || self.record.id != self.binding.memory_id
            || self.record.scope != self.binding.scope
            || self.record.revision != self.binding.target_revision
            || self.record.etag != self.binding.target_etag
        {
            return Err(StoreError::InvalidTransaction);
        }
        Ok(())
    }
}

/// Body-free exact replay result for a committed forget.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DurableForgetResult {
    pub(crate) format_version: String,
    pub(crate) store_id: StoreId,
    pub(crate) binding: MutationBinding,
    pub(crate) forgotten_at: Timestamp,
}

impl DurableForgetResult {
    fn validate(&self) -> Result<(), StoreError> {
        self.binding.validate()?;
        if self.format_version != FORGET_RESULT_FORMAT_VERSION
            || self.binding.operation != MutationOperation::Forget
        {
            return Err(StoreError::InvalidTransaction);
        }
        Ok(())
    }
}

/// Mixed historical result ledger. The untagged wrapper preserves every
/// v1alpha2 create/update byte while admitting a separately versioned,
/// body-free forget result only under the v1alpha3 capability gate.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum DurableMutationResult {
    Record(Box<DurableRecordMutationResult>),
    Forget(Box<DurableForgetResult>),
}

impl DurableMutationResult {
    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, StoreError> {
        self.validate()?;
        canonical_json(self, MAX_RESULT_BYTES)
    }

    pub(crate) fn decode(
        file: File,
        expected_store_id: &StoreId,
        expected_binding: &MutationBinding,
    ) -> Result<Self, StoreError> {
        decode_canonical(file, MAX_RESULT_BYTES, |result: &Self| {
            result.validate()?;
            if result.store_id() != expected_store_id || result.binding() != expected_binding {
                return Err(StoreError::InvalidTransaction);
            }
            Ok(())
        })
    }

    pub(crate) fn binding(&self) -> &MutationBinding {
        match self {
            Self::Record(result) => &result.binding,
            Self::Forget(result) => &result.binding,
        }
    }

    pub(crate) fn store_id(&self) -> &StoreId {
        match self {
            Self::Record(result) => &result.store_id,
            Self::Forget(result) => &result.store_id,
        }
    }

    pub(crate) fn into_record(self) -> Result<DurableRecordMutationResult, StoreError> {
        match self {
            Self::Record(result) => Ok(*result),
            Self::Forget(_) => Err(StoreError::InvalidTransaction),
        }
    }

    pub(crate) fn into_forget(self) -> Result<DurableForgetResult, StoreError> {
        match self {
            Self::Forget(result) => Ok(*result),
            Self::Record(_) => Err(StoreError::InvalidTransaction),
        }
    }

    fn validate(&self) -> Result<(), StoreError> {
        match self {
            Self::Record(result) => result.validate(),
            Self::Forget(result) => result.validate(),
        }
    }
}

/// Body-free durable receipt used for conflict detection and replay lookup.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DurableIdempotencyReceipt {
    pub(crate) format_version: String,
    pub(crate) store_id: StoreId,
    pub(crate) binding: MutationBinding,
    pub(crate) result_digest: String,
}

impl DurableIdempotencyReceipt {
    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, StoreError> {
        self.validate()?;
        canonical_json(self, MAX_SAFE_ARTIFACT_BYTES)
    }

    pub(crate) fn decode(
        file: File,
        expected_store_id: &StoreId,
        expected_receipt_id: &str,
    ) -> Result<Self, StoreError> {
        decode_canonical(file, MAX_SAFE_ARTIFACT_BYTES, |receipt: &Self| {
            receipt.validate()?;
            if &receipt.store_id != expected_store_id
                || receipt.binding.receipt_id != expected_receipt_id
            {
                return Err(StoreError::InvalidTransaction);
            }
            Ok(())
        })
    }

    fn validate(&self) -> Result<(), StoreError> {
        self.binding.validate()?;
        let format_matches = match self.binding.operation {
            MutationOperation::Create | MutationOperation::Update => {
                self.format_version == RECEIPT_FORMAT_VERSION
            }
            MutationOperation::Forget => self.format_version == FORGET_RECEIPT_FORMAT_VERSION,
        };
        if !format_matches || !transaction::valid_content_digest(&self.result_digest) {
            return Err(StoreError::InvalidTransaction);
        }
        Ok(())
    }
}

/// One body-free, sequence-addressed committed mutation event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DurableAuditEvent {
    pub(crate) format_version: String,
    pub(crate) store_id: StoreId,
    pub(crate) binding: MutationBinding,
    pub(crate) result_digest: String,
}

impl DurableAuditEvent {
    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, StoreError> {
        self.validate()?;
        canonical_json(self, MAX_SAFE_ARTIFACT_BYTES)
    }

    pub(crate) fn decode(
        file: File,
        expected_store_id: &StoreId,
        expected_sequence: AuditSequence,
    ) -> Result<Self, StoreError> {
        decode_canonical(file, MAX_SAFE_ARTIFACT_BYTES, |event: &Self| {
            event.validate()?;
            if &event.store_id != expected_store_id
                || event.binding.audit_sequence != expected_sequence
            {
                return Err(StoreError::InvalidTransaction);
            }
            Ok(())
        })
    }

    fn validate(&self) -> Result<(), StoreError> {
        self.binding.validate()?;
        let format_matches = match self.binding.operation {
            MutationOperation::Create | MutationOperation::Update => {
                self.format_version == AUDIT_FORMAT_VERSION
            }
            MutationOperation::Forget => self.format_version == FORGET_AUDIT_FORMAT_VERSION,
        };
        if !format_matches || !transaction::valid_content_digest(&self.result_digest) {
            return Err(StoreError::InvalidTransaction);
        }
        Ok(())
    }
}

/// Body-free migration marker defining where v1alpha2 audit coverage begins.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AuditGenesis {
    pub(crate) format_version: String,
    pub(crate) store_id: StoreId,
    pub(crate) base_store_revision: StoreRevision,
    pub(crate) first_audit_sequence: AuditSequence,
}

impl AuditGenesis {
    pub(crate) fn new(store_id: StoreId, base_store_revision: StoreRevision) -> Self {
        Self {
            format_version: GENESIS_FORMAT_VERSION.to_owned(),
            store_id,
            base_store_revision,
            first_audit_sequence: AuditSequence(1),
        }
    }

    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, StoreError> {
        if self.format_version != GENESIS_FORMAT_VERSION || self.first_audit_sequence.0 != 1 {
            return Err(StoreError::InvalidStoreMetadata);
        }
        canonical_json(self, MAX_SAFE_ARTIFACT_BYTES).map_err(|_| StoreError::InvalidStoreMetadata)
    }

    pub(crate) fn decode(file: File, expected_store_id: &StoreId) -> Result<Self, StoreError> {
        decode_canonical(file, MAX_SAFE_ARTIFACT_BYTES, |genesis: &Self| {
            if genesis.format_version != GENESIS_FORMAT_VERSION
                || &genesis.store_id != expected_store_id
                || genesis.first_audit_sequence.0 != 1
            {
                return Err(StoreError::InvalidStoreMetadata);
            }
            Ok(())
        })
        .map_err(|_| StoreError::InvalidStoreMetadata)
    }
}

pub(crate) struct MutationArtifacts {
    pub(crate) result: DurableMutationResult,
    pub(crate) result_bytes: Vec<u8>,
    pub(crate) result_digest: String,
    pub(crate) receipt: DurableIdempotencyReceipt,
    pub(crate) receipt_bytes: Vec<u8>,
    pub(crate) receipt_digest: String,
    pub(crate) audit: DurableAuditEvent,
    pub(crate) audit_bytes: Vec<u8>,
    pub(crate) audit_digest: String,
}

impl MutationArtifacts {
    pub(crate) fn build(
        store_id: StoreId,
        binding: MutationBinding,
        previous_revision: Option<Revision>,
        record: MemoryRecord,
    ) -> Result<Self, StoreError> {
        let result = DurableMutationResult::Record(Box::new(DurableRecordMutationResult {
            format_version: RESULT_FORMAT_VERSION.to_owned(),
            store_id: store_id.clone(),
            binding: binding.clone(),
            previous_revision,
            record,
        }));
        let result_bytes = result.canonical_bytes()?;
        let result_digest = content_digest(&result_bytes);
        let receipt = DurableIdempotencyReceipt {
            format_version: receipt_format(binding.operation).to_owned(),
            store_id: store_id.clone(),
            binding: binding.clone(),
            result_digest: result_digest.clone(),
        };
        let receipt_bytes = receipt.canonical_bytes()?;
        let receipt_digest = content_digest(&receipt_bytes);
        let audit = DurableAuditEvent {
            format_version: audit_format(binding.operation).to_owned(),
            store_id,
            binding,
            result_digest: result_digest.clone(),
        };
        let audit_bytes = audit.canonical_bytes()?;
        let audit_digest = content_digest(&audit_bytes);
        Ok(Self {
            result,
            result_bytes,
            result_digest,
            receipt,
            receipt_bytes,
            receipt_digest,
            audit,
            audit_bytes,
            audit_digest,
        })
    }

    pub(crate) fn build_forget(
        store_id: StoreId,
        binding: MutationBinding,
        tombstone: &crate::tombstone::ProtectedTombstone,
    ) -> Result<Self, StoreError> {
        if binding.operation != MutationOperation::Forget
            || tombstone.store_id != store_id
            || tombstone.transaction_id != binding.transaction_id
            || tombstone.memory_id != binding.memory_id
            || tombstone.scope != binding.scope
            || tombstone.revision != binding.target_revision
            || tombstone.etag != binding.target_etag
            || tombstone.store_revision != binding.store_revision
            || tombstone.audit_sequence != binding.audit_sequence
        {
            return Err(StoreError::InvalidTransaction);
        }
        let result = DurableMutationResult::Forget(Box::new(DurableForgetResult {
            format_version: FORGET_RESULT_FORMAT_VERSION.to_owned(),
            store_id: store_id.clone(),
            binding: binding.clone(),
            forgotten_at: tombstone.forgotten_at.clone(),
        }));
        let result_bytes = result.canonical_bytes()?;
        let result_digest = content_digest(&result_bytes);
        let receipt = DurableIdempotencyReceipt {
            format_version: receipt_format(binding.operation).to_owned(),
            store_id: store_id.clone(),
            binding: binding.clone(),
            result_digest: result_digest.clone(),
        };
        let receipt_bytes = receipt.canonical_bytes()?;
        let receipt_digest = content_digest(&receipt_bytes);
        let audit = DurableAuditEvent {
            format_version: audit_format(binding.operation).to_owned(),
            store_id,
            binding,
            result_digest: result_digest.clone(),
        };
        let audit_bytes = audit.canonical_bytes()?;
        let audit_digest = content_digest(&audit_bytes);
        Ok(Self {
            result,
            result_bytes,
            result_digest,
            receipt,
            receipt_bytes,
            receipt_digest,
            audit,
            audit_bytes,
            audit_digest,
        })
    }

    pub(crate) fn from_record_intent(
        store_id: StoreId,
        intent: &transaction::RecordTransaction,
        record: MemoryRecord,
    ) -> Result<Self, StoreError> {
        let idempotency = intent
            .idempotency
            .as_ref()
            .ok_or(StoreError::InvalidTransaction)?;
        let artifacts = Self::build(
            store_id,
            idempotency.binding.clone(),
            intent.base_revision,
            record,
        )?;
        if artifacts.result_digest != idempotency.result_digest
            || artifacts.receipt_digest != idempotency.receipt_digest
            || artifacts.audit_digest != idempotency.audit_digest
        {
            return Err(StoreError::InvalidTransaction);
        }
        Ok(artifacts)
    }

    pub(crate) fn from_forget_intent(
        store_id: StoreId,
        intent: &transaction::ForgetTransaction,
        tombstone: &crate::tombstone::ProtectedTombstone,
    ) -> Result<Self, StoreError> {
        let artifacts =
            Self::build_forget(store_id, intent.idempotency.binding.clone(), tombstone)?;
        if artifacts.result_digest != intent.idempotency.result_digest
            || artifacts.receipt_digest != intent.idempotency.receipt_digest
            || artifacts.audit_digest != intent.idempotency.audit_digest
            || crate::idempotency::content_digest(&tombstone.canonical_bytes()?)
                != intent.tombstone_digest
        {
            return Err(StoreError::InvalidTransaction);
        }
        Ok(artifacts)
    }
}

const fn receipt_format(operation: MutationOperation) -> &'static str {
    match operation {
        MutationOperation::Create | MutationOperation::Update => RECEIPT_FORMAT_VERSION,
        MutationOperation::Forget => FORGET_RECEIPT_FORMAT_VERSION,
    }
}

const fn audit_format(operation: MutationOperation) -> &'static str {
    match operation {
        MutationOperation::Create | MutationOperation::Update => AUDIT_FORMAT_VERSION,
        MutationOperation::Forget => FORGET_AUDIT_FORMAT_VERSION,
    }
}

pub(crate) fn request_fingerprint(value: &impl Serialize) -> Result<String, StoreError> {
    let bytes = serde_json::to_vec(value).map_err(|_| StoreError::InvalidRequest)?;
    Ok(domain_digest_bytes(
        b"jiandu/canonical-mutation-input/v1\0",
        &bytes,
    ))
}

pub(crate) fn content_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{}", hex_digest(digest.as_slice()))
}

pub(crate) fn receipt_relative(
    identity: &ReceiptIdentity,
    operation: MutationOperation,
) -> Result<PathBuf, StoreError> {
    validate_identity(identity)?;
    Ok(PathBuf::from(layout::IDEMPOTENCY_RECEIPTS_DIR)
        .join(digest_hex(&identity.principal_digest)?)
        .join(operation.as_str())
        .join(&identity.receipt_id[..2])
        .join(format!("{}.json", identity.receipt_id)))
}

pub(crate) fn receipt_relative_for_binding(
    binding: &MutationBinding,
) -> Result<PathBuf, StoreError> {
    binding.validate()?;
    receipt_relative(
        &ReceiptIdentity {
            receipt_id: binding.receipt_id.clone(),
            principal_digest: binding.principal_digest.clone(),
            key_digest: binding.key_digest.clone(),
        },
        binding.operation,
    )
}

pub(crate) fn result_relative(receipt_id: &str) -> Result<PathBuf, StoreError> {
    if !valid_receipt_id(receipt_id) {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(PathBuf::from(layout::IDEMPOTENCY_RESULTS_DIR)
        .join(&receipt_id[..2])
        .join(format!("{receipt_id}.json")))
}

pub(crate) fn audit_relative(sequence: AuditSequence) -> Result<PathBuf, StoreError> {
    if sequence.0 == 0 {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(PathBuf::from(layout::MUTATION_AUDIT_DIR).join(format!("{:020}.json", sequence.0)))
}

pub(crate) fn result_temp_relative(binding: &MutationBinding) -> Result<PathBuf, StoreError> {
    temp_sibling(
        &result_relative(&binding.receipt_id)?,
        "result",
        &binding.transaction_id,
    )
}

pub(crate) fn receipt_temp_relative(binding: &MutationBinding) -> Result<PathBuf, StoreError> {
    temp_sibling(
        &receipt_relative_for_binding(binding)?,
        "receipt",
        &binding.transaction_id,
    )
}

pub(crate) fn audit_temp_relative(binding: &MutationBinding) -> Result<PathBuf, StoreError> {
    temp_sibling(
        &audit_relative(binding.audit_sequence)?,
        "audit",
        &binding.transaction_id,
    )
}

pub(crate) fn read_receipt(
    root: &StoreDirectory,
    store_id: &StoreId,
    identity: &ReceiptIdentity,
    operation: MutationOperation,
) -> Result<Option<DurableIdempotencyReceipt>, StoreError> {
    let relative = receipt_relative(identity, operation)?;
    root.try_open_regular(&relative, false)?
        .map(|file| {
            StoreDirectory::validate_private_open_file(&file)?;
            DurableIdempotencyReceipt::decode(file, store_id, &identity.receipt_id)
        })
        .transpose()
}

pub(crate) fn read_result(
    root: &StoreDirectory,
    store_id: &StoreId,
    binding: &MutationBinding,
    expected_digest: &str,
) -> Result<DurableMutationResult, StoreError> {
    let mut budget = UnlimitedLedgerBudget;
    read_result_inner(root, store_id, binding, expected_digest, &mut budget)
}

fn read_result_inner(
    root: &StoreDirectory,
    store_id: &StoreId,
    binding: &MutationBinding,
    expected_digest: &str,
    budget: &mut impl LedgerScanBudget,
) -> Result<DurableMutationResult, StoreError> {
    if !transaction::valid_content_digest(expected_digest) {
        return Err(StoreError::InvalidTransaction);
    }
    let file = root
        .try_open_regular(&result_relative(&binding.receipt_id)?, false)?
        .ok_or(StoreError::InvalidTransaction)?;
    charge_file(&file, budget)?;
    StoreDirectory::validate_private_open_file(&file)?;
    let read_file = file
        .try_clone()
        .map_err(|source| StoreError::io("clone mutation result artifact", source))?;
    if transaction::raw_file_digest(read_file)? != expected_digest {
        return Err(StoreError::InvalidTransaction);
    }
    DurableMutationResult::decode(file, store_id, binding)
}

pub(crate) fn verify_audit(
    root: &StoreDirectory,
    store_id: &StoreId,
    binding: &MutationBinding,
    expected_result_digest: &str,
) -> Result<(), StoreError> {
    let mut budget = UnlimitedLedgerBudget;
    verify_audit_inner(root, store_id, binding, expected_result_digest, &mut budget)
}

fn verify_audit_inner(
    root: &StoreDirectory,
    store_id: &StoreId,
    binding: &MutationBinding,
    expected_result_digest: &str,
    budget: &mut impl LedgerScanBudget,
) -> Result<(), StoreError> {
    let file = root
        .try_open_regular(&audit_relative(binding.audit_sequence)?, false)?
        .ok_or(StoreError::InvalidTransaction)?;
    charge_file(&file, budget)?;
    StoreDirectory::validate_private_open_file(&file)?;
    let audit = DurableAuditEvent::decode(file, store_id, binding.audit_sequence)?;
    if audit.binding != *binding || audit.result_digest != expected_result_digest {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(())
}

/// Validate the complete private receipt/result/audit ledger before a store
/// becomes ready. This rejects malformed, foreign, orphaned, duplicated, or
/// partially published artifacts that are not covered by an active WAL.
pub(crate) fn validate_ledger(
    root: &StoreDirectory,
    metadata: &StoreMetadata,
) -> Result<(), StoreError> {
    let mut budget = UnlimitedLedgerBudget;
    let issues = inspect_ledger(root, metadata, &mut budget);
    if issues.is_empty() {
        Ok(())
    } else if issues.contains(&LedgerIssue::Unsafe) {
        Err(StoreError::UnsafePath)
    } else {
        Err(StoreError::InvalidTransaction)
    }
}

/// Build the committed mutation transaction-anchor set once while opening a
/// store. Ordinary mutations consult the resulting in-memory set instead of
/// rescanning the private receipt ledger on every write.
///
/// The exact receipt decoder remains authoritative. Duplicate anchors are an
/// impossible ledger state and therefore fail closed during startup.
pub(crate) fn committed_transaction_ids(
    root: &StoreDirectory,
    metadata: &StoreMetadata,
) -> Result<BTreeSet<String>, StoreError> {
    let mut budget = UnlimitedLedgerBudget;
    let mut transaction_ids = BTreeSet::new();
    for receipt in read_all_receipts(root, metadata, &mut budget)? {
        if !transaction_ids.insert(receipt.binding.transaction_id) {
            return Err(StoreError::InvalidTransaction);
        }
    }
    Ok(transaction_ids)
}

/// Stage-specific view over the exact same ledger invariant used by startup.
/// The set is intentionally closed and carries no artifact names or contents.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum LedgerIssue {
    Receipt,
    Result,
    Audit,
    Backup,
    Tombstone,
    Witness,
    Limit,
    Unsafe,
}

pub(crate) trait LedgerScanBudget {
    fn consume_entry(&mut self) -> bool;
    fn consume_bytes(&mut self, bytes: u64) -> bool;
    fn exceeded(&self) -> bool;
}

struct UnlimitedLedgerBudget;

impl LedgerScanBudget for UnlimitedLedgerBudget {
    fn consume_entry(&mut self) -> bool {
        true
    }

    fn consume_bytes(&mut self, _bytes: u64) -> bool {
        true
    }

    fn exceeded(&self) -> bool {
        false
    }
}

pub(crate) fn inspect_ledger(
    root: &StoreDirectory,
    metadata: &StoreMetadata,
    budget: &mut impl LedgerScanBudget,
) -> BTreeSet<LedgerIssue> {
    let mut issues = BTreeSet::new();
    if metadata.audit_sequence.0 > metadata.store_revision.0 {
        issues.insert(LedgerIssue::Audit);
    }
    let receipts = match read_all_receipts(root, metadata, budget) {
        Ok(receipts) => receipts,
        Err(error) => {
            issues.insert(LedgerIssue::Receipt);
            record_unsafe_issue(&mut issues, &error);
            if budget.exceeded() {
                issues.insert(LedgerIssue::Limit);
            }
            return issues;
        }
    };
    let import_ledger = match crate::portable_import::inspect_import_ledger(root, metadata, budget)
    {
        Ok(inspection) => inspection,
        Err((issue, error)) => {
            issues.insert(issue);
            record_unsafe_issue(&mut issues, &error);
            if budget.exceeded() {
                issues.insert(LedgerIssue::Limit);
            }
            return issues;
        }
    };

    let mut expected_results = BTreeSet::new();
    let mut expected_tombstones = import_ledger.tombstone_paths;
    let mut expected_witnesses = BTreeSet::new();
    let mut receipts_by_sequence = BTreeMap::new();
    for receipt in receipts {
        let binding = &receipt.binding;
        if binding.store_revision.0 > metadata.store_revision.0
            || binding.audit_sequence.0 > metadata.audit_sequence.0
            || receipts_by_sequence
                .insert(binding.audit_sequence, receipt.clone())
                .is_some()
        {
            issues.insert(LedgerIssue::Receipt);
        }
        let result_path = match result_relative(&binding.receipt_id) {
            Ok(path) => path,
            Err(_) => {
                issues.insert(LedgerIssue::Result);
                continue;
            }
        };
        let result = match read_result_inner(
            root,
            &metadata.store_id,
            binding,
            &receipt.result_digest,
            budget,
        ) {
            Ok(result) => Some(result),
            Err(error) => {
                issues.insert(LedgerIssue::Result);
                record_unsafe_issue(&mut issues, &error);
                None
            }
        };
        if let Err(error) = verify_audit_inner(
            root,
            &metadata.store_id,
            binding,
            &receipt.result_digest,
            budget,
        ) {
            issues.insert(LedgerIssue::Audit);
            record_unsafe_issue(&mut issues, &error);
        }
        match (binding.operation, result) {
            (
                MutationOperation::Create | MutationOperation::Update,
                Some(DurableMutationResult::Record(_)),
            ) => {}
            (MutationOperation::Forget, Some(DurableMutationResult::Forget(result)))
                if supports_tombstones(&metadata.format_version) =>
            {
                let tombstone = match crate::tombstone::read_exact_bounded(
                    root,
                    &metadata.store_id,
                    &binding.scope,
                    &binding.memory_id,
                    budget,
                ) {
                    Ok(Some(tombstone)) => Some(tombstone),
                    Ok(None) => {
                        issues.insert(LedgerIssue::Tombstone);
                        None
                    }
                    Err(error) => {
                        issues.insert(LedgerIssue::Tombstone);
                        record_unsafe_issue(&mut issues, &error);
                        None
                    }
                };
                let Some(tombstone) = tombstone else {
                    expected_results.insert(result_path);
                    continue;
                };
                let record_exists = match crate::mutation::record_id_exists_anywhere_bounded(
                    root,
                    &binding.memory_id,
                    budget,
                ) {
                    Ok(exists) => exists,
                    Err(error) => {
                        record_unsafe_issue(&mut issues, &error);
                        true
                    }
                };
                if tombstone.transaction_id != binding.transaction_id
                    || tombstone.revision != binding.target_revision
                    || tombstone.etag != binding.target_etag
                    || tombstone.forgotten_at != result.forgotten_at
                    || tombstone.store_revision != binding.store_revision
                    || tombstone.audit_sequence != binding.audit_sequence
                    || record_exists
                    || !expected_tombstones.insert(tombstone.relative_path())
                {
                    issues.insert(LedgerIssue::Tombstone);
                }
                match transaction::erasure_witness_relative_for(
                    &binding.scope,
                    &binding.memory_id,
                    &binding.transaction_id,
                ) {
                    Ok(path) => {
                        if !expected_witnesses.insert(path) {
                            issues.insert(LedgerIssue::Witness);
                        }
                    }
                    Err(_) => {
                        issues.insert(LedgerIssue::Witness);
                    }
                }
            }
            (_, None) => {}
            _ => {
                issues.insert(LedgerIssue::Result);
            }
        }
        expected_results.insert(result_path);
    }

    let mutation_sequences = receipts_by_sequence
        .keys()
        .copied()
        .collect::<BTreeSet<_>>();
    if mutation_sequences
        .intersection(&import_ledger.sequences)
        .next()
        .is_some()
        || !validate_combined_audit_sequences(
            metadata.audit_sequence,
            &mutation_sequences,
            &import_ledger.sequences,
        )
    {
        issues.insert(LedgerIssue::Audit);
        issues.insert(LedgerIssue::Receipt);
    }

    if let Err(error) = validate_result_namespace(root, &expected_results, budget) {
        issues.insert(LedgerIssue::Result);
        record_unsafe_issue(&mut issues, &error);
    }
    if let Err(error) = validate_audit_namespace(root, metadata, &receipts_by_sequence, budget) {
        issues.insert(LedgerIssue::Audit);
        record_unsafe_issue(&mut issues, &error);
    }
    if supports_tombstones(&metadata.format_version)
        && let Err(error) =
            validate_tombstone_namespace(root, metadata, &expected_tombstones, budget)
    {
        issues.insert(LedgerIssue::Tombstone);
        record_unsafe_issue(&mut issues, &error);
    }
    if let Err(error) = validate_erasure_witness_namespace(root, &expected_witnesses, budget) {
        issues.insert(LedgerIssue::Witness);
        record_unsafe_issue(&mut issues, &error);
    }
    if budget.exceeded() {
        issues.insert(LedgerIssue::Limit);
    }
    issues
}

fn record_unsafe_issue(issues: &mut BTreeSet<LedgerIssue>, error: &StoreError) {
    if matches!(error, StoreError::UnsafePath) {
        issues.insert(LedgerIssue::Unsafe);
    }
}

fn read_all_receipts(
    root: &StoreDirectory,
    metadata: &StoreMetadata,
    budget: &mut impl LedgerScanBudget,
) -> Result<Vec<DurableIdempotencyReceipt>, StoreError> {
    let root_relative = Path::new(layout::IDEMPOTENCY_RECEIPTS_DIR);
    let principal_directories = root.open_directory(root_relative)?;
    let mut receipts = Vec::new();
    for principal_name in entry_names(&principal_directories, "list receipt principals", budget)? {
        let principal = principal_name
            .to_str()
            .filter(|value| valid_hex(value, 64))
            .ok_or(StoreError::InvalidTransaction)?;
        let principal_directory =
            StoreDirectory::open_child_directory(&principal_directories, &principal_name)?;
        for operation_name in entry_names(&principal_directory, "list receipt operations", budget)?
        {
            let operation = match operation_name.to_str() {
                Some("create") => MutationOperation::Create,
                Some("update") => MutationOperation::Update,
                Some("forget") if supports_tombstones(&metadata.format_version) => {
                    MutationOperation::Forget
                }
                _ => return Err(StoreError::InvalidTransaction),
            };
            let operation_directory =
                StoreDirectory::open_child_directory(&principal_directory, &operation_name)?;
            for shard_name in entry_names(&operation_directory, "list receipt shards", budget)? {
                let shard = shard_name
                    .to_str()
                    .filter(|value| valid_hex(value, 2))
                    .ok_or(StoreError::InvalidTransaction)?;
                let shard_directory =
                    StoreDirectory::open_child_directory(&operation_directory, &shard_name)?;
                for file_name in entry_names(&shard_directory, "list receipt artifacts", budget)? {
                    let receipt_id = json_hex_name(&file_name, 64)?;
                    if !receipt_id.starts_with(shard) {
                        return Err(StoreError::InvalidTransaction);
                    }
                    let file = StoreDirectory::try_open_regular_in(&shard_directory, &file_name)?
                        .ok_or(StoreError::InvalidTransaction)?;
                    charge_file(&file, budget)?;
                    StoreDirectory::validate_private_open_file(&file)?;
                    let receipt =
                        DurableIdempotencyReceipt::decode(file, &metadata.store_id, receipt_id)?;
                    let relative = root_relative
                        .join(principal)
                        .join(operation.as_str())
                        .join(shard)
                        .join(&file_name);
                    if receipt.binding.operation != operation
                        || digest_hex(&receipt.binding.principal_digest)? != principal
                        || receipt_relative_for_binding(&receipt.binding)? != relative
                    {
                        return Err(StoreError::InvalidTransaction);
                    }
                    receipts.push(receipt);
                }
            }
        }
    }
    Ok(receipts)
}

fn supports_tombstones(format: &str) -> bool {
    matches!(
        format,
        crate::STORE_FORMAT_VERSION | crate::metadata::V3_STORE_FORMAT_VERSION
    )
}

fn validate_tombstone_namespace(
    root: &StoreDirectory,
    metadata: &StoreMetadata,
    expected: &BTreeSet<PathBuf>,
    budget: &mut impl LedgerScanBudget,
) -> Result<(), StoreError> {
    let tombstones = root.open_directory(Path::new(layout::TOMBSTONES_DIR))?;
    let mut observed = BTreeSet::new();
    let kinds = entry_names(&tombstones, "list tombstone scope kinds", budget)?;
    let expected_kinds: BTreeSet<_> = ["instance_global", "principal", "project", "session"]
        .into_iter()
        .collect();
    let observed_kinds: BTreeSet<_> = kinds
        .iter()
        .map(|name| name.to_str().ok_or(StoreError::InvalidTransaction))
        .collect::<Result<_, _>>()?;
    if observed_kinds != expected_kinds {
        return Err(StoreError::InvalidTransaction);
    }
    for kind_name in kinds {
        let kind = kind_name.to_str().ok_or(StoreError::InvalidTransaction)?;
        let kind_directory = StoreDirectory::open_child_directory(&tombstones, &kind_name)?;
        if kind == "instance_global" {
            collect_tombstone_shards(
                root,
                metadata,
                &kind_directory,
                Path::new(layout::TOMBSTONES_DIR).join(kind),
                &mut observed,
                budget,
            )?;
        } else {
            for owner_name in entry_names(&kind_directory, "list tombstone owners", budget)? {
                layout::validate_owner_entry_name(&owner_name)?;
                let owner_directory =
                    StoreDirectory::open_child_directory(&kind_directory, &owner_name)?;
                collect_tombstone_shards(
                    root,
                    metadata,
                    &owner_directory,
                    Path::new(layout::TOMBSTONES_DIR)
                        .join(kind)
                        .join(&owner_name),
                    &mut observed,
                    budget,
                )?;
            }
        }
    }
    if &observed == expected {
        Ok(())
    } else {
        Err(StoreError::InvalidTransaction)
    }
}

fn validate_erasure_witness_namespace(
    root: &StoreDirectory,
    expected: &BTreeSet<PathBuf>,
    budget: &mut impl LedgerScanBudget,
) -> Result<(), StoreError> {
    let records = root.open_directory(Path::new("records"))?;
    let mut observed = BTreeSet::new();
    collect_erasure_witnesses(
        &records,
        PathBuf::from("records"),
        0,
        expected,
        &mut observed,
        budget,
    )?;
    if &observed == expected {
        Ok(())
    } else {
        Err(StoreError::InvalidTransaction)
    }
}

fn collect_erasure_witnesses(
    directory: &cap_std::fs::Dir,
    relative: PathBuf,
    depth: usize,
    expected: &BTreeSet<PathBuf>,
    observed: &mut BTreeSet<PathBuf>,
    budget: &mut impl LedgerScanBudget,
) -> Result<(), StoreError> {
    if depth > 4 {
        return Err(StoreError::InvalidTransaction);
    }
    for name in entry_names(directory, "list forget erasure witnesses", budget)? {
        let metadata = directory
            .symlink_metadata(&name)
            .map_err(|source| StoreError::io("inspect forget erasure witness", source))?;
        let transaction_id = transaction::transaction_id_from_erasure_witness_name(&name);
        let resembles_witness = name
            .to_str()
            .is_some_and(|name| name.starts_with(".forgotten-"));
        if metadata.is_symlink() {
            if resembles_witness {
                return Err(StoreError::UnsafePath);
            }
            continue;
        }
        if metadata.is_dir() {
            if resembles_witness {
                return Err(StoreError::InvalidTransaction);
            }
            let child = StoreDirectory::open_child_directory(directory, &name)?;
            collect_erasure_witnesses(
                &child,
                relative.join(&name),
                depth + 1,
                expected,
                observed,
                budget,
            )?;
            continue;
        }
        if transaction_id.is_some() {
            let file = StoreDirectory::try_open_regular_in(directory, &name)?
                .ok_or(StoreError::InvalidTransaction)?;
            charge_file(&file, budget)?;
            StoreDirectory::validate_private_open_file(&file)?;
            if file
                .metadata()
                .map_err(|source| StoreError::io("inspect forget erasure witness", source))?
                .len()
                != 0
            {
                return Err(StoreError::InvalidTransaction);
            }
            let witness = relative.join(&name);
            if !expected.contains(&witness) || !observed.insert(witness) {
                return Err(StoreError::InvalidTransaction);
            }
        } else if resembles_witness {
            return Err(StoreError::InvalidTransaction);
        }
    }
    Ok(())
}

fn collect_tombstone_shards(
    root: &StoreDirectory,
    metadata: &StoreMetadata,
    owner_directory: &cap_std::fs::Dir,
    owner_relative: PathBuf,
    observed: &mut BTreeSet<PathBuf>,
    budget: &mut impl LedgerScanBudget,
) -> Result<(), StoreError> {
    for shard_name in entry_names(owner_directory, "list tombstone shards", budget)? {
        let shard = shard_name
            .to_str()
            .filter(|value| valid_hex(value, 2))
            .ok_or(StoreError::InvalidTransaction)?;
        let shard_directory = StoreDirectory::open_child_directory(owner_directory, &shard_name)?;
        for file_name in entry_names(&shard_directory, "list protected tombstones", budget)? {
            let storage_key = layout::validate_tombstone_entry_name(&file_name)?;
            if !storage_key.starts_with(shard) {
                return Err(StoreError::InvalidTransaction);
            }
            let file = StoreDirectory::try_open_regular_in(&shard_directory, &file_name)?
                .ok_or(StoreError::InvalidTransaction)?;
            charge_file(&file, budget)?;
            StoreDirectory::validate_private_open_file(&file)?;
            let tombstone = crate::tombstone::ProtectedTombstone::decode(file, &metadata.store_id)?;
            let relative = owner_relative.join(shard).join(&file_name);
            if tombstone.relative_path() != relative
                || layout::record_storage_key(&tombstone.memory_id) != storage_key
                || !observed.insert(relative)
            {
                return Err(StoreError::InvalidTransaction);
            }
        }
    }
    // Keep the capability-relative root in the signature so callers cannot
    // accidentally replace this traversal with an ambient path walk.
    let _ = root;
    Ok(())
}

fn validate_result_namespace(
    root: &StoreDirectory,
    expected: &BTreeSet<PathBuf>,
    budget: &mut impl LedgerScanBudget,
) -> Result<(), StoreError> {
    let root_relative = Path::new(layout::IDEMPOTENCY_RESULTS_DIR);
    let results = root.open_directory(root_relative)?;
    let mut observed = BTreeSet::new();
    for shard_name in entry_names(&results, "list result shards", budget)? {
        let shard = shard_name
            .to_str()
            .filter(|value| valid_hex(value, 2))
            .ok_or(StoreError::InvalidTransaction)?;
        let shard_directory = StoreDirectory::open_child_directory(&results, &shard_name)?;
        for file_name in entry_names(&shard_directory, "list result artifacts", budget)? {
            let receipt_id = json_hex_name(&file_name, 64)?;
            if !receipt_id.starts_with(shard) {
                return Err(StoreError::InvalidTransaction);
            }
            let file = StoreDirectory::try_open_regular_in(&shard_directory, &file_name)?
                .ok_or(StoreError::InvalidTransaction)?;
            charge_file(&file, budget)?;
            StoreDirectory::validate_private_open_file(&file)?;
            let relative = root_relative.join(shard).join(&file_name);
            if !expected.contains(&relative) || !observed.insert(relative) {
                return Err(StoreError::InvalidTransaction);
            }
        }
    }
    if &observed == expected {
        Ok(())
    } else {
        Err(StoreError::InvalidTransaction)
    }
}

fn validate_audit_namespace(
    root: &StoreDirectory,
    metadata: &StoreMetadata,
    receipts: &BTreeMap<AuditSequence, DurableIdempotencyReceipt>,
    budget: &mut impl LedgerScanBudget,
) -> Result<(), StoreError> {
    let audits = root.open_directory(Path::new(layout::MUTATION_AUDIT_DIR))?;
    let mut observed = BTreeSet::new();
    for file_name in entry_names(&audits, "list mutation audit events", budget)? {
        let name = file_name.to_str().ok_or(StoreError::InvalidTransaction)?;
        let digits = name
            .strip_suffix(".json")
            .filter(|digits| digits.len() == 20 && digits.bytes().all(|byte| byte.is_ascii_digit()))
            .ok_or(StoreError::InvalidTransaction)?;
        let value = digits
            .parse::<u64>()
            .map_err(|_| StoreError::InvalidTransaction)?;
        let sequence = AuditSequence(value);
        if audit_relative(sequence)? != Path::new(layout::MUTATION_AUDIT_DIR).join(&file_name)
            || !observed.insert(sequence)
        {
            return Err(StoreError::InvalidTransaction);
        }
        let receipt = receipts
            .get(&sequence)
            .ok_or(StoreError::InvalidTransaction)?;
        let file = StoreDirectory::try_open_regular_in(&audits, &file_name)?
            .ok_or(StoreError::InvalidTransaction)?;
        charge_file(&file, budget)?;
        StoreDirectory::validate_private_open_file(&file)?;
        let event = DurableAuditEvent::decode(file, &metadata.store_id, sequence)?;
        if event.binding != receipt.binding || event.result_digest != receipt.result_digest {
            return Err(StoreError::InvalidTransaction);
        }
    }
    if observed.len() != receipts.len()
        || observed
            .iter()
            .zip(receipts.keys())
            .any(|(observed, expected)| observed != expected)
    {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(())
}

fn validate_combined_audit_sequences(
    watermark: AuditSequence,
    mutation: &BTreeSet<AuditSequence>,
    import: &BTreeSet<AuditSequence>,
) -> bool {
    let expected_len = usize::try_from(watermark.0).unwrap_or(usize::MAX);
    if mutation.len().checked_add(import.len()) != Some(expected_len) {
        return false;
    }
    let mut previous = 0_u64;
    for sequence in mutation.union(import) {
        let Some(expected) = previous.checked_add(1) else {
            return false;
        };
        if sequence.0 != expected {
            return false;
        }
        previous = sequence.0;
    }
    previous == watermark.0
}

pub(crate) fn entry_names(
    directory: &cap_std::fs::Dir,
    operation: &'static str,
    budget: &mut impl LedgerScanBudget,
) -> Result<Vec<std::ffi::OsString>, StoreError> {
    StoreDirectory::validate_private_open_directory(directory)?;
    let entries = directory
        .entries()
        .map_err(|source| StoreError::io(operation, source))?;
    let mut names = Vec::new();
    for entry in entries {
        if !budget.consume_entry() {
            return Err(StoreError::InvalidRequest);
        }
        names.push(
            entry
                .map_err(|source| StoreError::io(operation, source))?
                .file_name(),
        );
    }
    names.sort();
    Ok(names)
}

pub(crate) fn charge_file(
    file: &File,
    budget: &mut impl LedgerScanBudget,
) -> Result<(), StoreError> {
    let length = file
        .metadata()
        .map_err(|source| StoreError::io("inspect bounded ledger artifact", source))?
        .len();
    if budget.consume_bytes(length) {
        Ok(())
    } else {
        Err(StoreError::InvalidRequest)
    }
}

fn json_hex_name(name: &OsStr, length: usize) -> Result<&str, StoreError> {
    name.to_str()
        .and_then(|value| value.strip_suffix(".json"))
        .filter(|value| valid_hex(value, length))
        .ok_or(StoreError::InvalidTransaction)
}

fn valid_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn decode_canonical<T: DeserializeOwned + Serialize>(
    file: File,
    maximum: usize,
    validate: impl FnOnce(&T) -> Result<(), StoreError>,
) -> Result<T, StoreError> {
    let metadata = file
        .metadata()
        .map_err(|source| StoreError::io("inspect private store artifact", source))?;
    if metadata.len() > maximum as u64 || !StoreDirectory::has_single_link(&file)? {
        return Err(StoreError::InvalidTransaction);
    }
    let mut bytes = Vec::new();
    file.take((maximum + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|source| StoreError::io("read private store artifact", source))?;
    let value: T = serde_json::from_slice(&bytes).map_err(|_| StoreError::InvalidTransaction)?;
    validate(&value)?;
    if canonical_json(&value, maximum)? != bytes {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(value)
}

pub(crate) fn canonical_json(
    value: &impl Serialize,
    maximum: usize,
) -> Result<Vec<u8>, StoreError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|_| StoreError::InvalidTransaction)?;
    bytes.push(b'\n');
    if bytes.len() > maximum {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(bytes)
}

fn validate_identity(identity: &ReceiptIdentity) -> Result<(), StoreError> {
    if !valid_receipt_id(&identity.receipt_id)
        || !transaction::valid_content_digest(&identity.principal_digest)
        || !transaction::valid_content_digest(&identity.key_digest)
    {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(())
}

fn valid_receipt_id(value: &str) -> bool {
    value.len() == RECEIPT_ID_LENGTH
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn digest_hex(value: &str) -> Result<&str, StoreError> {
    if !transaction::valid_content_digest(value) {
        return Err(StoreError::InvalidTransaction);
    }
    Ok(&value[7..])
}

fn temp_sibling(target: &Path, kind: &str, transaction_id: &str) -> Result<PathBuf, StoreError> {
    if !transaction::valid_transaction_id(transaction_id) {
        return Err(StoreError::InvalidTransaction);
    }
    let parent = target.parent().ok_or(StoreError::InvalidTransaction)?;
    Ok(parent.join(format!(".{kind}-{transaction_id}.tmp")))
}

fn domain_digest(domain: &[u8], value: &str) -> String {
    domain_digest_bytes(domain, value.as_bytes())
}

fn domain_digest_bytes(domain: &[u8], value: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(value);
    format!("sha256:{}", hex_digest(hasher.finalize().as_slice()))
}

fn hex_digest(digest: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}
