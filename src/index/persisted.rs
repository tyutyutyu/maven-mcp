//! Persistable form of one indexed JAR or class directory, used by the on-disk index cache.
//!
//! A stored record is reused only when everything it was derived from is unchanged: the
//! canonical path, the Maven identity, the limits that shape its facts, and the size and
//! nanosecond modification time of the file, or of every entry below a class directory.
//! Classes and effective classfile entries are not stored; they are re-derived from the
//! stored entry names with the same multi-release rule a fresh build uses.

use std::{
    collections::BTreeSet,
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use super::{
    ArchiveBudget, ArchiveLimit, ClassReferenceKind, FactStringId, IndexedClassFacts,
    IndexedProvider, IndexedReference, IndexedTypeParent, JarIdentity, JarRecord,
    ProviderDescriptorKind, TypeRelation, class_name_from_entry, effective_class_entries,
};
use crate::index_cache::{decode_payload, encode_payload};

/// Files modified this recently may still change without a visible stamp change (coarse
/// filesystem clocks), so records and manifests that depend on them are not persisted.
pub const SETTLED_AGE: Duration = Duration::from_secs(2);

/// Read access to records an earlier process persisted, keyed by canonical artifact path.
pub trait PersistedRecords {
    fn load(&self, path: &Path) -> Option<Vec<u8>>;
}

/// A freshly built record that may be written to the on-disk cache.
#[derive(Debug, Clone)]
pub struct PersistableRecord {
    pub path: PathBuf,
    pub payload: Vec<u8>,
}

/// Whether a modification time is old enough that a later change must produce a new stamp.
pub fn is_settled(modified_ns: u128) -> bool {
    was_settled_at(modified_ns, SystemTime::now())
}

/// Whether a modification time was already settled at `moment`, so every change after
/// `moment` must produce a new stamp.
pub fn was_settled_at(modified_ns: u128, moment: SystemTime) -> bool {
    moment
        .checked_sub(SETTLED_AGE)
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .is_some_and(|threshold| modified_ns <= threshold.as_nanos())
}

/// Size and modification time of the artifact file, or of the directory and every entry
/// below it in walk order; symbolic links are stamped, not followed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ArtifactValidity {
    stamps: Vec<(String, u64, u128)>,
}

impl ArtifactValidity {
    /// `None` when the artifact or one of its entries cannot be stamped, or a class
    /// directory has more entries than the index may hold anyway.
    pub(super) fn capture(path: &Path) -> Option<Self> {
        let limit = crate::config::max_index_entries().saturating_add(1);
        let mut stamps = Vec::new();
        for entry in WalkDir::new(path)
            .follow_links(false)
            .sort_by_file_name()
            .into_iter()
            .take(limit)
        {
            let entry = entry.ok()?;
            let metadata = entry.path().symlink_metadata().ok()?;
            let modified = metadata
                .modified()
                .ok()?
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_nanos();
            let relative = entry.path().strip_prefix(path).ok()?.to_str()?.to_owned();
            stamps.push((relative, metadata.len(), modified));
        }
        (stamps.len() < limit).then_some(Self { stamps })
    }

    fn is_settled(&self) -> bool {
        self.stamps
            .iter()
            .all(|(_, _, modified)| is_settled(*modified))
    }
}

/// Everything besides the artifact content that determines the derived record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RecordContext {
    path: PathBuf,
    group_id: String,
    artifact_id: String,
    version: String,
    classifier: Option<String>,
    relative_path: String,
    max_entry_bytes: usize,
    max_jar_entries: usize,
    max_xml_bytes: usize,
}

impl RecordContext {
    fn new(path: &Path, identity: &JarIdentity, max_entry_bytes: usize) -> Self {
        Self {
            path: path.to_owned(),
            group_id: identity.key.group_id.clone(),
            artifact_id: identity.key.artifact_id.clone(),
            version: identity.key.version.clone(),
            classifier: identity.classifier.clone(),
            relative_path: identity.relative_path.clone(),
            max_entry_bytes,
            max_jar_entries: crate::config::max_jar_entries(),
            max_xml_bytes: crate::config::max_xml_bytes(),
        }
    }
}

/// Entries and name bytes the archive preflight charged against the index-wide budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct PreflightCharge {
    entries: usize,
    name_bytes: usize,
}

impl PreflightCharge {
    pub(super) fn between(before: &ArchiveBudget, after: &ArchiveBudget) -> Self {
        Self {
            entries: before.entries - after.entries,
            name_bytes: before.names - after.names,
        }
    }

    /// Charges a reused record exactly like its preflight did, so the aggregate limits
    /// fail the same way for cached and freshly read archives.
    fn charge(self, budget: &mut ArchiveBudget) -> Result<()> {
        if self.entries > budget.jar_entries {
            return Err(ArchiveLimit("MAX_JAR_ENTRIES").into());
        }
        budget.entries = budget
            .entries
            .checked_sub(self.entries)
            .ok_or(ArchiveLimit("MAX_INDEX_ENTRIES"))?;
        budget.names = budget
            .names
            .checked_sub(self.name_bytes)
            .ok_or(ArchiveLimit("MAX_INDEX_NAME_BYTES"))?;
        Ok(())
    }
}

type StoredReference = (u32, Option<u32>, Option<u32>, ClassReferenceKind);

#[derive(Debug, Serialize, Deserialize)]
struct StoredClassFacts {
    source_class: u32,
    parents: Vec<(u32, TypeRelation)>,
    references: Vec<StoredReference>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredProvider {
    service: String,
    provider: Option<String>,
    descriptor_kind: ProviderDescriptorKind,
    entry: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredRecord {
    context: RecordContext,
    validity: ArtifactValidity,
    preflight: PreflightCharge,
    multi_release: bool,
    entries: Vec<String>,
    fact_strings: Vec<String>,
    class_facts: Vec<StoredClassFacts>,
    providers: Vec<StoredProvider>,
}

/// Encodes a freshly built record; `None` when it must not be persisted because an input
/// was modified too recently or the record cannot be encoded.
pub(super) fn persistable(
    record: &JarRecord,
    validity: ArtifactValidity,
    preflight: PreflightCharge,
    max_entry_bytes: usize,
) -> Option<PersistableRecord> {
    if !validity.is_settled() {
        return None;
    }
    let identity = JarIdentity {
        key: record.key.clone(),
        classifier: record.classifier.clone(),
        relative_path: record.relative_path.clone(),
    };
    let stored = StoredRecord {
        context: RecordContext::new(&record.path, &identity, max_entry_bytes),
        validity,
        preflight,
        multi_release: record.multi_release,
        entries: record.entries.clone(),
        fact_strings: record
            .fact_strings
            .iter()
            .map(|value| value.to_string())
            .collect(),
        class_facts: record.class_facts.iter().map(store_class_facts).collect(),
        providers: record
            .providers
            .iter()
            .map(|provider| StoredProvider {
                service: provider.service.clone(),
                provider: provider.provider.clone(),
                descriptor_kind: provider.descriptor_kind,
                entry: provider.entry.clone(),
            })
            .collect(),
    };
    match encode_payload(&stored) {
        Ok(payload) => Some(PersistableRecord {
            path: record.path.clone(),
            payload,
        }),
        Err(error) => {
            tracing::warn!(
                error_kind = %crate::log_safety::error_category(&error),
                "cannot encode an index record for the cache"
            );
            None
        }
    }
}

/// Restores a stored record when it was derived from exactly the current inputs, charging
/// its preflight against `budget`. `Ok(None)` is a cache miss; an error is a resource limit.
pub(super) fn restore(
    payload: &[u8],
    path: &Path,
    identity: &JarIdentity,
    validity: &ArtifactValidity,
    max_entry_bytes: usize,
    budget: &mut ArchiveBudget,
) -> Result<Option<JarRecord>> {
    let stored = match decode_payload::<StoredRecord>(payload).and_then(validate) {
        Ok(stored) => stored,
        Err(error) => {
            tracing::warn!(
                error_kind = %crate::log_safety::error_category(&error),
                "ignoring an unreadable cached index record"
            );
            return Ok(None);
        }
    };
    if stored.context != RecordContext::new(path, identity, max_entry_bytes)
        || stored.validity != *validity
    {
        return Ok(None);
    }
    stored.preflight.charge(budget)?;
    Ok(Some(into_record(stored, path, identity)))
}

fn store_class_facts(facts: &IndexedClassFacts) -> StoredClassFacts {
    StoredClassFacts {
        source_class: facts.source_class.0.get(),
        parents: facts
            .parents
            .iter()
            .map(|parent| (parent.parent.0.get(), parent.relation))
            .collect(),
        references: facts
            .references
            .iter()
            .map(|reference| {
                (
                    reference.target_owner.0.get(),
                    reference.target_name.map(|id| id.0.get()),
                    reference.target_descriptor.map(|id| id.0.get()),
                    reference.kind,
                )
            })
            .collect(),
    }
}

/// Rejects a record whose fact string IDs point outside its string table.
fn validate(stored: StoredRecord) -> Result<StoredRecord> {
    let count = stored.fact_strings.len();
    let in_range = |id: u32| id >= 1 && (id as usize) <= count;
    let valid = stored.class_facts.iter().all(|facts| {
        in_range(facts.source_class)
            && facts.parents.iter().all(|(parent, _)| in_range(*parent))
            && facts.references.iter().all(|(owner, name, descriptor, _)| {
                in_range(*owner) && name.is_none_or(in_range) && descriptor.is_none_or(in_range)
            })
    });
    if !valid {
        bail!("cached index record references a missing fact string");
    }
    Ok(stored)
}

fn fact_id(raw: u32) -> FactStringId {
    // `validate` proved that every ID is at least one.
    FactStringId(NonZeroU32::new(raw).unwrap_or(NonZeroU32::MIN))
}

fn into_record(stored: StoredRecord, path: &Path, identity: &JarIdentity) -> JarRecord {
    let class_entries = effective_class_entries(&stored.entries, stored.multi_release);
    let classes = class_entries
        .keys()
        .filter_map(|logical| class_name_from_entry(logical))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    JarRecord {
        key: identity.key.clone(),
        classifier: identity.classifier.clone(),
        relative_path: identity.relative_path.clone(),
        path: path.to_owned(),
        classes,
        entries: stored.entries,
        class_entries,
        multi_release: stored.multi_release,
        fact_strings: stored.fact_strings.into_iter().map(Arc::from).collect(),
        class_facts: stored
            .class_facts
            .into_iter()
            .map(|facts| IndexedClassFacts {
                source_class: fact_id(facts.source_class),
                parents: facts
                    .parents
                    .into_iter()
                    .map(|(parent, relation)| IndexedTypeParent {
                        parent: fact_id(parent),
                        relation,
                    })
                    .collect(),
                references: facts
                    .references
                    .into_iter()
                    .map(|(owner, name, descriptor, kind)| IndexedReference {
                        target_owner: fact_id(owner),
                        target_name: name.map(fact_id),
                        target_descriptor: descriptor.map(fact_id),
                        kind,
                    })
                    .collect(),
            })
            .collect(),
        providers: stored
            .providers
            .into_iter()
            .map(|provider| IndexedProvider {
                service: provider.service,
                provider: provider.provider,
                descriptor_kind: provider.descriptor_kind,
                entry: provider.entry,
            })
            .collect(),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/index_persisted.rs"]
mod tests;
