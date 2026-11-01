//! Validity, single-flight index construction and retained-memory accounting for the
//! per-project Maven runners and scoped indexes of one STDIO process.
//!
//! All state is keyed by canonical project root. Every project has a generation that
//! changes whenever its cached model or index becomes invalid, so a build that started
//! under an older generation can never publish its result.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rmcp::ErrorData;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex as AsyncMutex, OnceCell};
use walkdir::WalkDir;

use crate::{
    index::{IndexStats, MavenIndex},
    project::MavenRunner,
    runtime_stats::{IndexState, ProjectIndexStatus, RuntimeCacheStatus, unix_ms},
};

/// Fixed share of the budget charged for one cached runner, in addition to its fingerprint.
const RUNNER_BASE_BYTES: u64 = 16 * 1024;
const PATH_ENTRY_OVERHEAD_BYTES: u64 = 96;

/// Cheap identity of one file: size and modification time with nanosecond resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct FileStamp {
    len: u64,
    modified_ns: u128,
}

fn stamp(path: &Path) -> Option<FileStamp> {
    let metadata = fs::metadata(path).ok()?;
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos());
    Some(FileStamp {
        len: metadata.len(),
        modified_ns,
    })
}

/// Stamps of a fixed set of paths. A missing path is part of the identity, so creating or
/// deleting a file changes the fingerprint just like editing it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProjectFingerprint {
    stamps: BTreeMap<PathBuf, Option<FileStamp>>,
}

impl ProjectFingerprint {
    pub fn capture(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            stamps: paths
                .into_iter()
                .map(|path| {
                    let stamp = stamp(&path);
                    (path, stamp)
                })
                .collect(),
        }
    }

    /// Like [`Self::capture`], but a directory contributes itself and every file and
    /// directory below it (symbolic links are not followed), so editing, adding or removing a
    /// nested file changes the fingerprint. The walk of one directory is bounded by the
    /// archive entry budget, which the index build enforces anyway.
    pub fn capture_trees(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        let limit = crate::config::max_index_entries().saturating_add(1);
        Self::capture(paths.into_iter().flat_map(|path| {
            if path.is_dir() {
                WalkDir::new(&path)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(Result::ok)
                    .take(limit)
                    .map(|entry| entry.into_path())
                    .collect::<Vec<_>>()
            } else {
                vec![path]
            }
        }))
    }

    /// Whether every stamped path still has the stamp recorded in this fingerprint.
    pub fn is_current(&self) -> bool {
        self.stamps
            .iter()
            .all(|(path, recorded)| stamp(path) == *recorded)
    }

    /// Whether every existing stamped file is old enough that a later change must produce a
    /// new stamp; see [`crate::index::SETTLED_AGE`].
    pub fn is_settled(&self) -> bool {
        self.stamps
            .values()
            .flatten()
            .all(|stamp| crate::index::is_settled(stamp.modified_ns))
    }

    /// Whether every existing stamped file was already settled at `moment`: a file that
    /// changed after a reader started at `moment` is never mistaken for what it read.
    pub fn was_settled_at(&self, moment: SystemTime) -> bool {
        self.stamps
            .values()
            .flatten()
            .all(|stamp| crate::index::was_settled_at(stamp.modified_ns, moment))
    }

    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.stamps.keys().map(PathBuf::as_path)
    }

    /// Sum of the sizes of the stamped files that exist.
    pub fn file_bytes(&self) -> u64 {
        self.stamps.values().flatten().map(|stamp| stamp.len).sum()
    }

    fn estimated_bytes(&self) -> u64 {
        self.stamps
            .keys()
            .map(|path| path.as_os_str().len() as u64 + PATH_ENTRY_OVERHEAD_BYTES)
            .sum()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CacheLimits {
    pub max_indexes: usize,
    pub max_runners: usize,
    pub max_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct RunnerSnapshot {
    pub runner: Arc<MavenRunner>,
    pub fingerprint: Arc<ProjectFingerprint>,
    /// Set after Maven ran in the project: the model must be re-read even if no stamp changed.
    pub revalidate: bool,
}

#[derive(Debug)]
pub struct CachedIndex {
    pub index: Arc<MavenIndex>,
    project: Arc<ProjectFingerprint>,
    artifacts: ProjectFingerprint,
    bytes: u64,
    classpath_entries: usize,
    build_duration: Duration,
}

/// A finished build's artifact stamps and the figures `maven-mcp stats` reports for it.
#[derive(Debug)]
pub struct FinishedBuild {
    pub artifacts: ProjectFingerprint,
    pub classpath_entries: usize,
    pub duration: Duration,
}

impl CachedIndex {
    /// Whether the classpath artifacts this index was built from are unchanged.
    pub fn artifacts_are_current(&self) -> bool {
        self.artifacts.is_current()
    }
}

#[derive(Debug, Clone)]
pub enum BuildOutcome {
    Built(Arc<MavenIndex>),
    /// The project was invalidated while the build ran; the result was discarded.
    Stale,
    Failed(ErrorData),
}

pub type BuildCell = Arc<OnceCell<BuildOutcome>>;

/// Ownership of the single build of one generation. The build's blocking work holds the lock
/// until it has really stopped, because cancelling the request does not stop that work: a
/// retry of the same generation waits for it instead of running a second build beside it.
pub type BuildOwner = Arc<AsyncMutex<()>>;

#[derive(Debug)]
pub enum IndexLookup {
    Cached {
        entry: Arc<CachedIndex>,
        generation: u64,
    },
    /// Exactly one caller per generation runs the build inside `cell`, holding `owner`; the
    /// others await it.
    Build {
        cell: BuildCell,
        owner: BuildOwner,
        generation: u64,
    },
}

/// Memory announced by a running build. It is counted while the holder is alive, even after
/// the build was invalidated, because the abandoned build still occupies that memory.
#[derive(Debug)]
pub struct Reservation {
    bytes: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReserveError {
    Stale,
    OverBudget { needed: u64, limit: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publication {
    Stale,
    Retained,
    /// The index alone does not fit the budget; it serves the requests that built it only.
    Uncached,
}

#[derive(Debug)]
struct RunnerEntry {
    runner: Arc<MavenRunner>,
    fingerprint: Arc<ProjectFingerprint>,
    revalidate: bool,
}

impl RunnerEntry {
    fn bytes(&self) -> u64 {
        runner_bytes(&self.runner, &self.fingerprint)
    }

    fn release(self) -> ReleasedRunner {
        ReleasedRunner {
            fingerprint_bytes: self.fingerprint.estimated_bytes(),
            runner: Arc::downgrade(&self.runner),
        }
    }
}

fn runner_bytes(runner: &MavenRunner, fingerprint: &ProjectFingerprint) -> u64 {
    RUNNER_BASE_BYTES + fingerprint.estimated_bytes() + runner.retained_test_result_bytes()
}

/// A runner that left the cache while requests may still hold it.
#[derive(Debug)]
struct ReleasedRunner {
    runner: Weak<MavenRunner>,
    fingerprint_bytes: u64,
}

impl ReleasedRunner {
    fn bytes(&self) -> u64 {
        self.runner.upgrade().map_or(0, |runner| {
            RUNNER_BASE_BYTES + self.fingerprint_bytes + runner.retained_test_result_bytes()
        })
    }
}

#[derive(Debug)]
struct InFlight {
    generation: u64,
    cell: BuildCell,
    owner: BuildOwner,
}

impl InFlight {
    /// Whether a request still holds the build cell or the build's blocking work still holds
    /// its owner lock. A request that was cancelled drops its cell clone, so a marker whose
    /// cell only the cache references and whose lock is free has no build running and nobody
    /// waiting. The lock, not the owner's reference count, is checked: a released guard
    /// drops its owner reference only after waking the next holder, which may already
    /// republish the status.
    fn is_live(&self) -> bool {
        Arc::strong_count(&self.cell) > 1 || self.owner.try_lock().is_err()
    }
}

#[derive(Debug)]
struct ProjectState {
    generation: u64,
    runner: Option<RunnerEntry>,
    index: Option<Arc<CachedIndex>>,
    in_flight: Option<InFlight>,
    /// Set by an explicit refresh: the next published build must not reuse on-disk records.
    full_rebuild: bool,
    last_used_tick: u64,
    last_used_unix_ms: u128,
}

#[derive(Debug)]
pub struct ProjectCache {
    limits: CacheLimits,
    states: HashMap<PathBuf, ProjectState>,
    /// Evicted or invalidated indexes that running requests may still hold.
    released: Vec<(Weak<MavenIndex>, u64)>,
    /// Evicted or replaced runners that running requests may still hold.
    released_runners: Vec<ReleasedRunner>,
    reservations: Vec<Weak<Reservation>>,
    next_generation: u64,
    tick: u64,
    hits: u64,
    misses: u64,
    evictions: u64,
    invalidations: u64,
    persisted: PersistedCounters,
}

/// How builds of this process used the on-disk index cache.
#[derive(Debug, Clone, Copy, Default)]
pub struct PersistedCounters {
    pub classpath_hits: u64,
    pub record_hits: u64,
    pub record_misses: u64,
}

impl ProjectCache {
    pub fn new(limits: CacheLimits) -> Self {
        Self {
            limits,
            states: HashMap::new(),
            released: Vec::new(),
            released_runners: Vec::new(),
            reservations: Vec::new(),
            next_generation: 0,
            tick: 0,
            hits: 0,
            misses: 0,
            evictions: 0,
            invalidations: 0,
            persisted: PersistedCounters::default(),
        }
    }

    /// Adds one build's use of the on-disk cache to the published counters.
    pub fn record_persisted_use(
        &mut self,
        classpath_hit: bool,
        record_hits: u64,
        record_misses: u64,
    ) {
        self.persisted.classpath_hits += u64::from(classpath_hit);
        self.persisted.record_hits += record_hits;
        self.persisted.record_misses += record_misses;
    }

    pub fn runner(&mut self, root: &Path) -> Option<RunnerSnapshot> {
        let tick = self.next_tick();
        let state = self.states.get_mut(root)?;
        state.last_used_tick = tick;
        state.last_used_unix_ms = unix_ms();
        let entry = state.runner.as_ref()?;
        Some(RunnerSnapshot {
            runner: Arc::clone(&entry.runner),
            fingerprint: Arc::clone(&entry.fingerprint),
            revalidate: entry.revalidate,
        })
    }

    /// Stores a freshly discovered runner. A different fingerprint than the replaced
    /// runner's invalidates the project's index and any running build.
    pub fn install_runner(
        &mut self,
        root: &Path,
        runner: Arc<MavenRunner>,
        fingerprint: ProjectFingerprint,
    ) -> RunnerSnapshot {
        let fingerprint = Arc::new(fingerprint);
        let changed = self
            .states
            .get(root)
            .and_then(|state| state.runner.as_ref())
            .is_some_and(|previous| *previous.fingerprint != *fingerprint);
        if changed {
            self.invalidate(root);
        }
        let tick = self.next_tick();
        let generation = self.fresh_generation();
        let state = self
            .states
            .entry(root.to_owned())
            .or_insert_with(|| ProjectState::new(generation));
        let replaced = state.runner.replace(RunnerEntry {
            runner: Arc::clone(&runner),
            fingerprint: Arc::clone(&fingerprint),
            revalidate: false,
        });
        state.last_used_tick = tick;
        state.last_used_unix_ms = unix_ms();
        if let Some(replaced) = replaced {
            self.released_runners.push(replaced.release());
        }
        self.enforce(Some(root));
        RunnerSnapshot {
            runner,
            fingerprint,
            revalidate: false,
        }
    }

    /// Drops the runner and everything derived from it after the model could not be re-read.
    pub fn discard_project(&mut self, root: &Path) {
        self.invalidate(root);
        if let Some(runner) = self
            .states
            .get_mut(root)
            .and_then(|state| state.runner.take())
        {
            self.released_runners.push(runner.release());
        }
        self.enforce(None);
    }

    /// Marks the project as changed by a Maven execution: the index is dropped, running
    /// builds are abandoned and the next request re-reads the reactor model.
    pub fn invalidate_after_maven(&mut self, root: &Path) {
        self.invalidate(root);
        if let Some(runner) = self
            .states
            .get_mut(root)
            .and_then(|state| state.runner.as_mut())
        {
            runner.revalidate = true;
        }
    }

    /// Makes the project's builds read every archive again until one of them is published.
    pub fn require_full_rebuild(&mut self, root: &Path) {
        let generation = self.fresh_generation();
        self.states
            .entry(root.to_owned())
            .or_insert_with(|| ProjectState::new(generation))
            .full_rebuild = true;
    }

    pub fn requires_full_rebuild(&self, root: &Path) -> bool {
        self.states
            .get(root)
            .is_some_and(|state| state.full_rebuild)
    }

    /// Clears [`Self::require_full_rebuild`] after a build of `generation` was published.
    pub fn finish_full_rebuild(&mut self, root: &Path, generation: u64) {
        if let Some(state) = self.states.get_mut(root)
            && state.generation == generation
        {
            state.full_rebuild = false;
        }
    }

    /// Drops the index and abandons the running build, but only if the project is still in
    /// the generation the caller observed.
    pub fn invalidate_generation(&mut self, root: &Path, generation: u64) {
        if self
            .states
            .get(root)
            .is_some_and(|state| state.generation == generation)
        {
            self.invalidate(root);
        }
    }

    pub fn lookup_index(&mut self, root: &Path, project: &ProjectFingerprint) -> IndexLookup {
        let tick = self.next_tick();
        let generation = self.fresh_generation();
        let state = self
            .states
            .entry(root.to_owned())
            .or_insert_with(|| ProjectState::new(generation));
        state.last_used_tick = tick;
        if let Some(entry) = state.index.clone() {
            if *entry.project == *project {
                return IndexLookup::Cached {
                    entry,
                    generation: state.generation,
                };
            }
            self.invalidate(root);
        }
        self.misses += 1;
        let state = self
            .states
            .get_mut(root)
            .expect("the project state was created above");
        let generation = state.generation;
        // A marker left by a cancelled request is reused: its empty cell lets this caller run
        // the build, and its owner makes that build wait until the abandoned work has stopped.
        let (cell, owner) = match &state.in_flight {
            Some(in_flight) if in_flight.generation == generation => {
                (Arc::clone(&in_flight.cell), Arc::clone(&in_flight.owner))
            }
            _ => {
                let cell = Arc::new(OnceCell::new());
                let owner = Arc::new(AsyncMutex::new(()));
                state.in_flight = Some(InFlight {
                    generation,
                    cell: Arc::clone(&cell),
                    owner: Arc::clone(&owner),
                });
                (cell, owner)
            }
        };
        IndexLookup::Build {
            cell,
            owner,
            generation,
        }
    }

    pub fn record_hit(&mut self, root: &Path) {
        self.hits += 1;
        let tick = self.next_tick();
        if let Some(state) = self.states.get_mut(root) {
            state.last_used_tick = tick;
            state.last_used_unix_ms = unix_ms();
        }
    }

    pub fn is_current(&self, root: &Path, generation: u64) -> bool {
        self.states
            .get(root)
            .is_some_and(|state| state.generation == generation)
    }

    /// Reserves `bytes` for a build of the given generation, evicting least recently used
    /// idle indexes first. Fails when the budget cannot hold the build.
    pub fn reserve(
        &mut self,
        root: &Path,
        generation: u64,
        bytes: u64,
    ) -> Result<Arc<Reservation>, ReserveError> {
        self.prune();
        if !self.is_current(root, generation) {
            return Err(ReserveError::Stale);
        }
        let limit = self.limits.max_bytes;
        while self.retained_bytes() + bytes > limit {
            if !self.evict_least_recently_used_index(None)
                && !self.evict_least_recently_used_runner(Some(root))
            {
                return Err(ReserveError::OverBudget {
                    needed: self.retained_bytes() + bytes,
                    limit,
                });
            }
        }
        let reservation = Arc::new(Reservation { bytes });
        self.reservations.push(Arc::downgrade(&reservation));
        Ok(reservation)
    }

    /// Publishes a finished build unless its generation was invalidated meanwhile. The
    /// reservation is consumed so the index is not counted twice.
    pub fn publish(
        &mut self,
        root: &Path,
        generation: u64,
        index: &Arc<MavenIndex>,
        build: FinishedBuild,
        project: Arc<ProjectFingerprint>,
        reservation: Arc<Reservation>,
    ) -> Publication {
        drop(reservation);
        self.prune();
        if !self.is_current(root, generation) {
            return Publication::Stale;
        }
        // The artifact stamps live as long as the index, and a class directory adds one per file.
        let bytes = index.estimated_bytes() + build.artifacts.estimated_bytes();
        if bytes > self.limits.max_bytes {
            // Evicting every other project would not make room for it.
            if let Some(state) = self.states.get_mut(root) {
                state.in_flight = None;
            }
            self.released.push((Arc::downgrade(index), bytes));
            return Publication::Uncached;
        }
        let entry = Arc::new(CachedIndex {
            index: Arc::clone(index),
            project,
            artifacts: build.artifacts,
            bytes,
            classpath_entries: build.classpath_entries,
            build_duration: build.duration,
        });
        let tick = self.next_tick();
        let state = self
            .states
            .get_mut(root)
            .expect("the generation check proved the project state exists");
        state.in_flight = None;
        state.index = Some(entry);
        state.last_used_tick = tick;
        state.last_used_unix_ms = unix_ms();
        self.enforce(Some(root));
        if self.retained_bytes() > self.limits.max_bytes {
            self.release_index(root);
            return Publication::Uncached;
        }
        Publication::Retained
    }

    /// Clears the running-build marker after a failure so the next request retries.
    pub fn finish_failed(&mut self, root: &Path, generation: u64) {
        if let Some(state) = self.states.get_mut(root)
            && state.generation == generation
        {
            state.in_flight = None;
        }
        self.enforce(None);
    }

    pub fn retained_bytes(&self) -> u64 {
        let states = self
            .states
            .values()
            .map(|state| {
                state.runner.as_ref().map_or(0, RunnerEntry::bytes)
                    + state.index.as_ref().map_or(0, |index| index.bytes)
            })
            .sum::<u64>();
        let released_runners = self
            .released_runners
            .iter()
            .map(ReleasedRunner::bytes)
            .sum::<u64>();
        let released = self
            .released
            .iter()
            .filter(|(index, _)| index.strong_count() > 0)
            .map(|(_, bytes)| *bytes)
            .sum::<u64>();
        let reserved = self
            .reservations
            .iter()
            .filter_map(Weak::upgrade)
            .map(|reservation| reservation.bytes)
            .sum::<u64>();
        states + released + released_runners + reserved
    }

    pub fn runtime_cache(&self) -> RuntimeCacheStatus {
        RuntimeCacheStatus {
            entries: self
                .states
                .values()
                .filter(|state| state.index.is_some())
                .count(),
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
            invalidations: self.invalidations,
            retained_bytes: self.retained_bytes(),
            max_bytes: self.limits.max_bytes,
            runners: self
                .states
                .values()
                .filter(|state| state.runner.is_some())
                .count(),
            persisted_classpath_hits: self.persisted.classpath_hits,
            persisted_record_hits: self.persisted.record_hits,
            persisted_record_misses: self.persisted.record_misses,
        }
    }

    /// Every active project: a retained index, a running build, or only a cached model.
    pub fn runtime_projects(&self) -> Vec<ProjectIndexStatus> {
        let mut projects = self
            .states
            .iter()
            .filter_map(|(root, state)| state.runtime_status(root))
            .collect::<Vec<_>>();
        projects.sort_by(|left, right| left.project_path.cmp(&right.project_path));
        projects
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    fn fresh_generation(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    fn invalidate(&mut self, root: &Path) {
        let generation = self.fresh_generation();
        let Some(state) = self.states.get_mut(root) else {
            return;
        };
        state.generation = generation;
        state.in_flight = None;
        self.invalidations += 1;
        self.release_index(root);
    }

    fn release_index(&mut self, root: &Path) {
        let Some(entry) = self
            .states
            .get_mut(root)
            .and_then(|state| state.index.take())
        else {
            return;
        };
        self.released
            .push((Arc::downgrade(&entry.index), entry.bytes));
        drop(entry);
        self.prune();
    }

    /// Forgets released memory nobody holds any more and build markers of cancelled requests.
    fn prune(&mut self) {
        self.released.retain(|(index, _)| index.strong_count() > 0);
        self.released_runners
            .retain(|released| released.runner.strong_count() > 0);
        for state in self.states.values_mut() {
            if state
                .in_flight
                .as_ref()
                .is_some_and(|in_flight| !in_flight.is_live())
            {
                state.in_flight = None;
            }
        }
        self.reservations
            .retain(|reservation| reservation.strong_count() > 0);
    }

    /// Evicts the least recently used index of an idle project other than `protect`.
    fn evict_least_recently_used_index(&mut self, protect: Option<&Path>) -> bool {
        let victim = self
            .states
            .iter()
            .filter(|(root, state)| {
                state.index.is_some() && protect.is_none_or(|protected| protected != root.as_path())
            })
            .min_by_key(|(_, state)| state.last_used_tick)
            .map(|(root, _)| root.clone());
        let Some(victim) = victim else {
            return false;
        };
        self.release_index(&victim);
        self.evictions += 1;
        tracing::debug!(
            evictions = self.evictions,
            retained_bytes = self.retained_bytes(),
            max_bytes = self.limits.max_bytes,
            "project index evicted"
        );
        true
    }

    fn evict_least_recently_used_runner(&mut self, protect: Option<&Path>) -> bool {
        let victim = self
            .states
            .iter()
            .filter(|(root, state)| {
                state.runner.is_some()
                    && !state.in_flight.as_ref().is_some_and(InFlight::is_live)
                    && protect.is_none_or(|protected| protected != root.as_path())
            })
            .min_by_key(|(_, state)| state.last_used_tick)
            .map(|(root, _)| root.clone());
        let Some(victim) = victim else {
            return false;
        };
        // The index stays valid without the runner: its entry carries the project fingerprint.
        if let Some(runner) = self
            .states
            .get_mut(&victim)
            .and_then(|state| state.runner.take())
        {
            self.released_runners.push(runner.release());
        }
        tracing::debug!(
            retained_bytes = self.retained_bytes(),
            max_bytes = self.limits.max_bytes,
            "project runner evicted"
        );
        true
    }

    fn enforce(&mut self, protect: Option<&Path>) {
        self.prune();
        while self
            .states
            .values()
            .filter(|state| state.index.is_some())
            .count()
            > self.limits.max_indexes
        {
            if !self.evict_least_recently_used_index(protect) {
                break;
            }
        }
        while self.retained_bytes() > self.limits.max_bytes {
            if !self.evict_least_recently_used_index(protect)
                && !self.evict_least_recently_used_runner(protect)
            {
                break;
            }
        }
        while self
            .states
            .values()
            .filter(|state| state.runner.is_some())
            .count()
            > self.limits.max_runners
        {
            if !self.evict_least_recently_used_runner(protect) {
                break;
            }
        }
        self.states.retain(|_, state| {
            state.runner.is_some()
                || state.index.is_some()
                || state.in_flight.as_ref().is_some_and(InFlight::is_live)
        });
    }
}

impl ProjectState {
    fn runtime_status(&self, root: &Path) -> Option<ProjectIndexStatus> {
        let status = |state| ProjectIndexStatus {
            project_path: root.display().to_string(),
            last_used_unix_ms: self.last_used_unix_ms,
            state,
            index: IndexStats::default(),
            estimated_bytes: 0,
            classpath_entries: None,
            build_duration_ms: None,
        };
        if let Some(entry) = &self.index {
            return Some(ProjectIndexStatus {
                index: entry.index.stats(),
                estimated_bytes: entry.bytes,
                classpath_entries: Some(entry.classpath_entries),
                build_duration_ms: Some(
                    u64::try_from(entry.build_duration.as_millis()).unwrap_or(u64::MAX),
                ),
                ..status(IndexState::Ready)
            });
        }
        if self.in_flight.as_ref().is_some_and(InFlight::is_live) {
            return Some(status(IndexState::Building));
        }
        self.runner.as_ref().map(|_| status(IndexState::NotIndexed))
    }

    fn new(generation: u64) -> Self {
        Self {
            generation,
            runner: None,
            index: None,
            in_flight: None,
            full_rebuild: false,
            last_used_tick: 0,
            last_used_unix_ms: unix_ms(),
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/project_cache.rs"]
mod tests;
