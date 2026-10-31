use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{build_info, config::Config, index::IndexStats};

/// Linux procfs, the only supported source of process liveness and memory figures.
const PROC_ROOT: &str = "/proc";

/// The version of the `stats --json` report and of every runtime status file.
pub const RUNTIME_SCHEMA_VERSION: u32 = 1;

/// The longest time a status file may lag behind cache hits. Hits only change counters and
/// last-use times, so they are coalesced into at most one write per this delay; every other
/// change (runner discovery, build, eviction, invalidation) is written at once.
pub const HIT_PUBLICATION_DELAY: Duration = Duration::from_secs(1);

/// Owner-only access for every status file and for the directory holding them.
const STATUS_FILE_MODE: u32 = 0o600;
const RUNTIME_DIR_MODE: u32 = 0o700;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeReport {
    pub schema_version: u32,
    pub instances: Vec<RuntimeInstanceStatus>,
    /// Status files kept because their process could be neither proven live nor proven gone.
    #[serde(default)]
    pub unknown_instances: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeInstanceStatus {
    pub schema_version: u32,
    pub binary_version: String,
    /// Full source commit embedded at build time; absent for unidentified builds.
    #[serde(default)]
    pub build_commit: Option<String>,
    /// Resolved path of the running executable, which names the installed version.
    #[serde(default)]
    pub executable: Option<String>,
    pub pid: u32,
    /// Process start in kernel clock ticks since boot (`/proc/<pid>/stat` field 22). Together
    /// with the PID it identifies the process, so a reused PID is not mistaken for it.
    #[serde(default)]
    pub process_start_ticks: Option<u64>,
    pub started_unix_ms: u128,
    pub updated_unix_ms: u128,
    pub memory: ProcessMemoryStatus,
    pub config: RuntimeConfigStatus,
    pub cache: RuntimeCacheStatus,
    pub projects: Vec<ProjectIndexStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfigStatus {
    pub max_results: usize,
    pub max_source_bytes: usize,
    pub max_project_indexes: usize,
    #[serde(default)]
    pub max_project_index_bytes: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeCacheStatus {
    pub entries: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    #[serde(default)]
    pub invalidations: u64,
    #[serde(default)]
    pub retained_bytes: u64,
    #[serde(default)]
    pub max_bytes: u64,
    #[serde(default)]
    pub runners: usize,
    /// Builds that reused a classpath from the on-disk index cache instead of running Maven.
    #[serde(default)]
    pub persisted_classpath_hits: u64,
    /// JAR and class-directory records reused from the on-disk index cache.
    #[serde(default)]
    pub persisted_record_hits: u64,
    /// Records read from their archives because the on-disk cache had no valid copy.
    #[serde(default)]
    pub persisted_record_misses: u64,
}

/// The index state of one active project.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexState {
    /// A built index is retained and answers requests. Older status files listed only these.
    #[default]
    Ready,
    /// No index is retained and a build is running.
    Building,
    /// Only the project model is cached: the index was never built, was invalidated, or was
    /// evicted.
    NotIndexed,
}

impl IndexState {
    fn label(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Building => "building",
            Self::NotIndexed => "not_indexed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectIndexStatus {
    pub project_path: String,
    pub last_used_unix_ms: u128,
    #[serde(default)]
    pub state: IndexState,
    /// Counts of the retained index; all zero unless `state` is `ready`.
    pub index: IndexStats,
    /// Estimated retained heap of the index and its artifact stamps; an estimate, not an
    /// allocator measurement, and zero unless `state` is `ready`.
    #[serde(default)]
    pub estimated_bytes: u64,
    /// Classpath entries (JARs and class directories) the retained index was built from.
    #[serde(default)]
    pub classpath_entries: Option<usize>,
    /// Wall-clock time of the build that produced the retained index, from the start of the
    /// classpath resolution to its publication.
    #[serde(default)]
    pub build_duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessMemoryStatus {
    pub rss_bytes: Option<u64>,
    pub peak_rss_bytes: Option<u64>,
}

/// Publishes the status of this process into its private status file.
#[derive(Debug)]
pub struct RuntimeStatusPublisher {
    path: PathBuf,
    state: Mutex<PublisherState>,
}

#[derive(Debug)]
struct PublisherState {
    status: RuntimeInstanceStatus,
    last_written: Instant,
    /// A cache hit changed the status after the last write.
    dirty: bool,
    /// A deferred write is scheduled and will include every hit recorded until it runs.
    pending: bool,
    /// The server stopped and removed the file; nothing may recreate it.
    closed: bool,
}

impl RuntimeStatusPublisher {
    pub fn register(config: &Config) -> Result<Self> {
        Self::register_in(
            runtime_dir()?,
            RuntimeConfigStatus {
                max_results: config.max_results,
                max_source_bytes: config.max_source_bytes,
                max_project_indexes: config.max_project_indexes,
                max_project_index_bytes: config.max_project_index_bytes,
            },
        )
    }

    fn register_in(directory: PathBuf, config: RuntimeConfigStatus) -> Result<Self> {
        fs::create_dir_all(&directory)
            .with_context(|| format!("cannot create runtime directory {}", directory.display()))?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(RUNTIME_DIR_MODE))?;
        let pid = std::process::id();
        let now = unix_ms();
        let status = RuntimeInstanceStatus {
            schema_version: RUNTIME_SCHEMA_VERSION,
            binary_version: build_info::PACKAGE_VERSION.to_owned(),
            build_commit: build_info::build_commit().map(str::to_owned),
            executable: running_executable(),
            pid,
            process_start_ticks: process_start_ticks(Path::new(PROC_ROOT), "self"),
            started_unix_ms: now,
            updated_unix_ms: now,
            memory: process_memory(),
            config,
            cache: RuntimeCacheStatus::default(),
            projects: Vec::new(),
        };
        let publisher = Self {
            path: directory.join(format!("{pid}.json")),
            state: Mutex::new(PublisherState {
                status,
                last_written: Instant::now(),
                dirty: false,
                pending: false,
                closed: false,
            }),
        };
        publisher.write(&mut publisher.lock())?;
        Ok(publisher)
    }

    /// Writes a structural change of the cache at once.
    pub fn update_cache(
        &self,
        cache: RuntimeCacheStatus,
        projects: Vec<ProjectIndexStatus>,
    ) -> Result<()> {
        let mut state = self.lock();
        if state.closed {
            return Ok(());
        }
        state.status.cache = cache;
        state.status.projects = projects;
        self.write(&mut state)
    }

    /// Records a cache hit without writing it. Returns when the caller must call
    /// [`Self::flush_hits`]: `None` when an already scheduled write covers this hit,
    /// otherwise the time left until [`HIT_PUBLICATION_DELAY`] has passed since the last
    /// write (zero when it already has).
    pub fn record_hit(&self) -> Option<Duration> {
        self.record_hit_at(Instant::now())
    }

    fn record_hit_at(&self, now: Instant) -> Option<Duration> {
        let mut state = self.lock();
        if state.closed {
            return None;
        }
        state.dirty = true;
        if state.pending {
            return None;
        }
        state.pending = true;
        Some(
            HIT_PUBLICATION_DELAY.saturating_sub(now.saturating_duration_since(state.last_written)),
        )
    }

    /// The scheduled write of recorded hits. When a structural write already covered them,
    /// nothing is written again.
    pub fn flush_hits(
        &self,
        cache: RuntimeCacheStatus,
        projects: Vec<ProjectIndexStatus>,
    ) -> Result<()> {
        let mut state = self.lock();
        state.pending = false;
        if state.closed || !state.dirty {
            return Ok(());
        }
        state.status.cache = cache;
        state.status.projects = projects;
        self.write(&mut state)
    }

    /// Removes the status file on shutdown and ignores every later update. The process exit
    /// after a signal skips destructors, so the server calls this explicitly.
    pub fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        remove_status_file(&self.path);
    }

    fn lock(&self) -> MutexGuard<'_, PublisherState> {
        // Every field stays valid on its own, so a panic elsewhere must not stop reporting.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self, state: &mut PublisherState) -> Result<()> {
        state.status.updated_unix_ms = unix_ms();
        state.status.memory = process_memory();
        state.last_written = Instant::now();
        state.dirty = false;
        let temporary = self.path.with_extension("json.tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(STATUS_FILE_MODE)
            .open(&temporary)
            .with_context(|| format!("cannot write runtime status {}", temporary.display()))?;
        file.write_all(&serde_json::to_vec_pretty(&state.status)?)?;
        drop(file);
        fs::rename(&temporary, &self.path)?;
        Ok(())
    }
}

impl Drop for RuntimeStatusPublisher {
    fn drop(&mut self) {
        remove_status_file(&self.path);
    }
}

fn remove_status_file(path: &Path) {
    let _ = fs::remove_file(path);
}

pub fn read_runtime_report() -> Result<RuntimeReport> {
    read_runtime_report_from(Path::new(PROC_ROOT), &runtime_dir()?)
}

/// Lists the instances whose status files belong to live processes. Only a process proven
/// gone has its file deleted; an unreadable or malformed file is skipped and kept, and a file
/// whose process liveness is unknown is kept and counted.
fn read_runtime_report_from(proc_root: &Path, directory: &Path) -> Result<RuntimeReport> {
    ensure_procfs(proc_root)?;
    let mut report = RuntimeReport {
        schema_version: RUNTIME_SCHEMA_VERSION,
        instances: Vec::new(),
        unknown_instances: 0,
    };
    if !directory.is_dir() {
        return Ok(report);
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(status) = read_status_file(&path) else {
            continue;
        };
        match instance_liveness(proc_root, &status) {
            Liveness::Alive => report.instances.push(status),
            Liveness::Dead => remove_dead_status_file(&path, &status),
            Liveness::Unknown => report.unknown_instances += 1,
        }
    }
    report.instances.sort_by_key(|instance| instance.pid);
    Ok(report)
}

/// Deletes the status file of the proven-dead `dead` only while the file still holds that
/// status. A new instance that reused the PID may have replaced the file since it was read:
/// the file is first moved to a unique name that no publisher or reader uses, read again
/// there, and a replacement found there is linked back without overwriting a newer one.
fn remove_dead_status_file(path: &Path, dead: &RuntimeInstanceStatus) {
    let quarantine = quarantine_path(path);
    if fs::rename(path, &quarantine).is_err() {
        return;
    }
    let still_dead = read_status_file_named(&quarantine, path)
        .is_some_and(|current| is_same_instance(&current, dead));
    if still_dead {
        remove_status_file(&quarantine);
        return;
    }
    match fs::hard_link(&quarantine, path) {
        Ok(()) => remove_status_file(&quarantine),
        // A newer publication of the replacement is already in place.
        Err(error) if error.kind() == ErrorKind::AlreadyExists => remove_status_file(&quarantine),
        // The replacement stays in quarantine rather than being lost.
        Err(_) => {}
    }
}

/// A name outside the `.json` files that readers list, unique to this reader and moment.
fn quarantine_path(path: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    path.with_extension(format!("json.{}-{nanos}.stale", std::process::id()))
}

/// The PID, the process start time and the publisher's start identify one instance.
fn is_same_instance(left: &RuntimeInstanceStatus, right: &RuntimeInstanceStatus) -> bool {
    left.pid == right.pid
        && left.process_start_ticks == right.process_start_ticks
        && left.started_unix_ms == right.started_unix_ms
}

/// A status whose PID does not match its `<pid>.json` name is not trusted to decide whether
/// that file may be deleted.
fn read_status_file(path: &Path) -> Option<RuntimeInstanceStatus> {
    read_status_file_named(path, path)
}

/// Reads the status at `path`, which must match the `<pid>.json` name of `named`.
fn read_status_file_named(path: &Path, named: &Path) -> Option<RuntimeInstanceStatus> {
    let status = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<RuntimeInstanceStatus>(&bytes).ok())?;
    let stem = named.file_stem()?.to_str()?;
    (stem == status.pid.to_string()).then_some(status)
}

pub fn format_runtime_report(report: &RuntimeReport) -> String {
    let mut output = String::new();
    if report.instances.is_empty() {
        output.push_str("No live maven-mcp instances.\n");
    }
    let now = unix_ms();
    for instance in &report.instances {
        format_instance(&mut output, instance, now);
    }
    if report.unknown_instances > 0 {
        output.push_str(&format!(
            "Kept {} status file(s) whose process could not be proven live or gone.\n",
            report.unknown_instances
        ));
    }
    output
}

fn format_instance(output: &mut String, instance: &RuntimeInstanceStatus, now: u128) {
    output.push_str(&format!(
        "maven-mcp pid={} version={} commit={} age_s={} rss={} peak_rss={}\n",
        instance.pid,
        instance.binary_version,
        instance.build_commit.as_deref().unwrap_or("unknown"),
        now.saturating_sub(instance.started_unix_ms) / 1000,
        optional_label(instance.memory.rss_bytes),
        optional_label(instance.memory.peak_rss_bytes)
    ));
    if let Some(executable) = &instance.executable {
        output.push_str(&format!("  executable: {executable}\n"));
    }
    output.push_str(&format!(
        "  cache: entries={} hits={} misses={} evictions={} invalidations={} max_project_indexes={}\n",
        instance.cache.entries,
        instance.cache.hits,
        instance.cache.misses,
        instance.cache.evictions,
        instance.cache.invalidations,
        instance.config.max_project_indexes
    ));
    output.push_str(&format!(
        "  retained: bytes={} max_bytes={} runners={}\n",
        instance.cache.retained_bytes,
        instance.config.max_project_index_bytes,
        instance.cache.runners
    ));
    output.push_str(&format!(
        "  persisted: classpath_hits={} record_hits={} record_misses={}\n",
        instance.cache.persisted_classpath_hits,
        instance.cache.persisted_record_hits,
        instance.cache.persisted_record_misses
    ));
    for project in &instance.projects {
        output.push_str(&format!(
            "  project: {} state={} classpath={} jars={} sources={} classes={} artifacts={} estimated_bytes={} build_ms={} idle_s={}\n",
            project.project_path,
            project.state.label(),
            optional_label(project.classpath_entries),
            project.index.jar_count,
            project.index.source_jar_count,
            project.index.class_count,
            project.index.artifact_count,
            project.estimated_bytes,
            optional_label(project.build_duration_ms),
            now.saturating_sub(project.last_used_unix_ms) / 1000
        ));
    }
}

pub fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn runtime_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("MAVEN_MCP_RUNTIME_DIR") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(path).join("maven-mcp"));
    }
    let home = std::env::var_os("HOME").context("HOME is required for maven-mcp runtime stats")?;
    Ok(PathBuf::from(home).join(".cache/maven-mcp/runtime"))
}

/// The canonical path of the running binary; on Linux it is resolved through symlinks, so a
/// stable or candidate alias reports the versioned installation directory it points to.
fn running_executable() -> Option<String> {
    std::env::current_exe()
        .ok()
        .map(|path| path.display().to_string())
}

/// Without a mounted procfs no instance could be proven live or stale, so the report fails
/// instead of claiming that every instance is gone and deleting their status files.
fn ensure_procfs(proc_root: &Path) -> Result<()> {
    if proc_root.join("self").is_dir() {
        return Ok(());
    }
    bail!(
        "maven-mcp stats requires a mounted Linux procfs at {} to tell live instances from stale ones",
        proc_root.display()
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    Alive,
    /// The recorded process no longer exists: its PID is absent or names another process.
    Dead,
    /// Neither could be proven, for example because procfs denied access.
    Unknown,
}

/// `/proc/<pid>` exists exactly while a process with that PID exists, so only a missing entry
/// proves the recorded process gone; any other error leaves its liveness unknown. When the
/// status carries the process start time, a live PID with a different start time was reused,
/// which also proves the recorded process gone. Status files without the start time, written
/// by older versions, are identified by PID only.
fn instance_liveness(proc_root: &Path, status: &RuntimeInstanceStatus) -> Liveness {
    let process = proc_root.join(status.pid.to_string());
    match fs::symlink_metadata(&process) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Liveness::Unknown,
        Err(error) if error.kind() == ErrorKind::NotFound => return Liveness::Dead,
        Err(_) => return Liveness::Unknown,
    }
    let Some(recorded) = status.process_start_ticks else {
        return Liveness::Alive;
    };
    match fs::read_to_string(process.join("stat")) {
        Ok(stat) => match parse_start_ticks(&stat) {
            Some(current) if current == recorded => Liveness::Alive,
            Some(_) => Liveness::Dead,
            None => Liveness::Unknown,
        },
        Err(error) if error.kind() == ErrorKind::NotFound => Liveness::Dead,
        Err(_) => Liveness::Unknown,
    }
}

fn process_start_ticks(proc_root: &Path, process: &str) -> Option<u64> {
    fs::read_to_string(proc_root.join(process).join("stat"))
        .ok()
        .as_deref()
        .and_then(parse_start_ticks)
}

/// Field 22 (`starttime`) of `/proc/<pid>/stat`. The command name in field 2 may contain
/// spaces and parentheses, so fields are counted after its last closing parenthesis.
fn parse_start_ticks(stat: &str) -> Option<u64> {
    const START_TIME_INDEX_AFTER_COMMAND: usize = 19;
    let (_, after_command) = stat.rsplit_once(')')?;
    after_command
        .split_whitespace()
        .nth(START_TIME_INDEX_AFTER_COMMAND)?
        .parse()
        .ok()
}

fn process_memory() -> ProcessMemoryStatus {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    ProcessMemoryStatus {
        rss_bytes: proc_status_kb(&status, "VmRSS:").map(|kb| kb * 1024),
        peak_rss_bytes: proc_status_kb(&status, "VmHWM:").map(|kb| kb * 1024),
    }
}

fn proc_status_kb(status: &str, key: &str) -> Option<u64> {
    status.lines().find_map(|line| {
        let rest = line.strip_prefix(key)?.trim();
        rest.split_whitespace().next()?.parse::<u64>().ok()
    })
}

fn optional_label<T: ToString>(value: Option<T>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
}

#[cfg(test)]
#[path = "../tests/unit/runtime_stats.rs"]
mod tests;
