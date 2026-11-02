//! The bounded, process-local store of tool results that were too large for one MCP response.
//!
//! A stored result is the exact structured JSON text of one tool call, bound to the canonical
//! project root of that call. It is readable only through `read_tool_result_page` with the
//! same project, at most [`STORED_RESULT_TTL`] after it was stored, and only while it has not
//! been evicted. Eviction is strictly oldest first across every project. Nothing is written to
//! disk, logged, or published in the runtime status; a restart forgets every result.

use std::{
    collections::{VecDeque, hash_map::RandomState},
    hash::{BuildHasher, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Results retained at the same time, across every project.
pub(super) const MAX_STORED_RESULTS: usize = 16;
/// How long a stored result stays readable after it was stored.
pub(super) const STORED_RESULT_TTL: Duration = Duration::from_secs(300);

/// A result whose retained size alone exceeds the store's byte budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ResultTooLarge {
    pub(super) retained_bytes: usize,
    pub(super) limit: usize,
}

struct StoredResult {
    id: String,
    project_root: PathBuf,
    json: Arc<str>,
    stored_at: Instant,
    retained_bytes: usize,
}

pub(super) struct ResultStore {
    /// Insertion order, so the front is always the oldest result.
    entries: VecDeque<StoredResult>,
    max_bytes: usize,
    retained_bytes: usize,
    /// Random per process, so an identifier means nothing to another server process.
    process_key: u64,
    next_sequence: u64,
}

/// Counts only: stored results and project roots never appear in diagnostics.
impl std::fmt::Debug for ResultStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResultStore")
            .field("stored_results", &self.entries.len())
            .field("retained_bytes", &self.retained_bytes)
            .field("max_bytes", &self.max_bytes)
            .finish_non_exhaustive()
    }
}

impl ResultStore {
    pub(super) fn new(max_bytes: usize) -> Self {
        Self::with_process_key(max_bytes, random_process_key())
    }

    pub(super) fn with_process_key(max_bytes: usize, process_key: u64) -> Self {
        Self {
            entries: VecDeque::new(),
            max_bytes,
            retained_bytes: 0,
            process_key,
            next_sequence: 0,
        }
    }

    /// Stores `json` for `project_root` and returns its new identifier, evicting expired and
    /// then the oldest results until both the count and the byte budget allow it. A result
    /// that alone exceeds the byte budget is rejected and evicts nothing.
    pub(super) fn insert(
        &mut self,
        project_root: &Path,
        json: String,
        now: Instant,
    ) -> Result<String, ResultTooLarge> {
        self.expire(now);
        let id = self.next_id();
        let retained_bytes = retained_size(&id, project_root, &json);
        if retained_bytes > self.max_bytes {
            return Err(ResultTooLarge {
                retained_bytes,
                limit: self.max_bytes,
            });
        }
        while self.entries.len() >= MAX_STORED_RESULTS
            || self.retained_bytes.saturating_add(retained_bytes) > self.max_bytes
        {
            self.evict_oldest();
        }
        self.retained_bytes += retained_bytes;
        self.entries.push_back(StoredResult {
            id: id.clone(),
            project_root: project_root.to_owned(),
            json: Arc::from(json),
            stored_at: now,
            retained_bytes,
        });
        Ok(id)
    }

    /// The stored JSON of `id` when it belongs to `project_root` and has not expired. Unknown,
    /// expired, evicted and other-project identifiers are indistinguishable.
    pub(super) fn get(&mut self, id: &str, project_root: &Path, now: Instant) -> Option<Arc<str>> {
        self.expire(now);
        self.entries
            .iter()
            .find(|entry| entry.id == id && entry.project_root == project_root)
            .map(|entry| Arc::clone(&entry.json))
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Retained bytes of every stored result: the JSON text, the identifier and the bound
    /// project root.
    pub(super) fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    fn expire(&mut self, now: Instant) {
        while self.entries.front().is_some_and(|entry| {
            now.saturating_duration_since(entry.stored_at) >= STORED_RESULT_TTL
        }) {
            self.evict_oldest();
        }
    }

    fn evict_oldest(&mut self) {
        if let Some(evicted) = self.entries.pop_front() {
            self.retained_bytes -= evicted.retained_bytes;
        }
    }

    fn next_id(&mut self) -> String {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        format!("r{:016x}{sequence:x}", self.process_key)
    }
}

/// The longest identifier a store can issue.
pub(super) fn longest_result_id() -> String {
    format!("r{:016x}{:x}", u64::MAX, u64::MAX)
}

fn retained_size(id: &str, project_root: &Path, json: &str) -> usize {
    json.len() + id.len() + project_root.as_os_str().len()
}

/// A per-process random value from the standard library's randomly seeded hasher.
fn random_process_key() -> u64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    hasher.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos()),
    );
    hasher.finish()
}

#[cfg(test)]
#[path = "../../tests/unit/server_result_store.rs"]
mod tests;
