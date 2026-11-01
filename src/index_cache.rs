//! Versioned, deletable on-disk cache of derived index data, shared by every STDIO process
//! of the same user.
//!
//! One SQLite database per cache format holds two kinds of entries:
//!
//! - `record`: one indexed JAR or class directory, keyed by canonical artifact path and
//!   reused only when its stamps, identity and limits still match (see `index::persisted`);
//! - `classpath`: the Maven-selected classpath of one project context, reused only while the
//!   project fingerprint, the classpath inputs and every classpath entry are unchanged.
//!
//! Entries are also keyed by the writing tool version. Every write is one SQLite
//! transaction, so a crash or a concurrent process never exposes a partial entry, and every
//! payload is a zlib stream whose checksum is verified on read. An unreadable, corrupt or
//! incompatible database is deleted and rebuilt; a failed lookup is a cache miss and a
//! failed write only loses the cached copy. The Maven repository is only read.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    build_info,
    config::{Config, JavaEnvironment, SERVER_ONLY_ENVIRONMENT},
    index::{PersistableRecord, PersistedRecords},
    project_cache::ProjectFingerprint,
    runtime_stats::unix_ms,
};

mod model_inputs;

/// Version of the database layout and of every payload; bump it whenever a stored shape or
/// the derivation of a stored record changes. Each format uses its own database file.
pub const INDEX_CACHE_FORMAT: u32 = 1;
/// Lock wait of one cache operation before it is treated as a miss or a skipped write.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound of one decoded payload, so a damaged length cannot exhaust memory.
const MAX_DECODED_PAYLOAD_BYTES: u64 = 1 << 30;
const RECORD_KIND: &str = "record";
const CLASSPATH_KIND: &str = "classpath";

/// Serializes `value` as zlib-compressed JSON. The JSON is compressed in one call: streaming
/// serde's many small writes into the encoder is an order of magnitude slower.
pub fn encode_payload<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(value).context("cannot serialize cache payload")?;
    let mut encoder = ZlibEncoder::new(Vec::with_capacity(json.len() / 4), Compression::fast());
    encoder
        .write_all(&json)
        .context("cannot compress cache payload")?;
    encoder.finish().context("cannot compress cache payload")
}

/// Decodes a payload written by [`encode_payload`], verifying its zlib checksum.
pub fn decode_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T> {
    let mut json = Vec::new();
    ZlibDecoder::new(payload)
        .take(MAX_DECODED_PAYLOAD_BYTES + 1)
        .read_to_end(&mut json)
        .context("cache payload is corrupt")?;
    if json.len() as u64 > MAX_DECODED_PAYLOAD_BYTES {
        bail!("cache payload exceeds its decoded size limit");
    }
    serde_json::from_slice(&json).context("cache payload has an unexpected shape")
}

/// Identity of the writing binary; entries of other versions are never reused.
fn tool_version() -> String {
    format!(
        "{}+{}",
        build_info::PACKAGE_VERSION,
        build_info::build_commit().unwrap_or("unidentified")
    )
}

#[derive(Debug)]
pub struct PersistentIndexCache {
    directory: PathBuf,
    max_bytes: u64,
    tool: String,
}

/// The entries one successful build adds to or refreshes in the cache.
#[derive(Debug, Default)]
pub struct CacheUpdate<'a> {
    pub classpath: Option<(&'a str, Vec<u8>)>,
    pub records: &'a [PersistableRecord],
    pub reused: &'a [PathBuf],
}

impl PersistentIndexCache {
    /// Does no I/O: the directory and the database are created by the first index build.
    pub fn new(directory: PathBuf, max_bytes: u64) -> Self {
        Self {
            directory,
            max_bytes,
            tool: tool_version(),
        }
    }

    pub fn database_path(&self) -> PathBuf {
        self.directory
            .join(format!("index-v{INDEX_CACHE_FORMAT}.sqlite3"))
    }

    /// A reader for persisted records; `None` (logged) when the cache cannot be opened.
    pub fn reader(&self) -> Option<CacheReader> {
        match self.open() {
            Ok(connection) => Some(CacheReader {
                connection,
                tool: self.tool.clone(),
            }),
            Err(error) => {
                tracing::warn!(
                    error_kind = %crate::log_safety::error_category(&error),
                    "index cache is unavailable; building without it"
                );
                None
            }
        }
    }

    /// The stored classpath manifest payload of one project context, if any.
    pub fn load_classpath(&self, context: &str) -> Option<Vec<u8>> {
        self.reader()?.load(CLASSPATH_KIND, context)
    }

    /// Writes one build's entries atomically and evicts least recently used entries until
    /// the cache fits its budget. A database found corrupt is rebuilt and written once more.
    pub fn write(&self, update: &CacheUpdate<'_>) -> Result<()> {
        let epoch = self.recovery_epoch()?;
        match self.try_write(update) {
            Err(error) if is_damaged(&error) => {
                tracing::warn!(
                    error_kind = %crate::log_safety::error_category(&error),
                    "rebuilding a damaged index cache"
                );
                drop(self.recover(epoch)?);
                self.try_write(update)
            }
            result => result,
        }
    }

    /// Forgets the classpath of a project context; its records stay valid by their stamps.
    pub fn forget_classpath(&self, context: &str) -> Result<()> {
        self.open()?.execute(
            "DELETE FROM entries WHERE kind = ?1 AND key = ?2",
            params![CLASSPATH_KIND, context],
        )?;
        Ok(())
    }

    fn try_write(&self, update: &CacheUpdate<'_>) -> Result<()> {
        let mut connection = self.open()?;
        let now = i64::try_from(unix_ms()).unwrap_or(i64::MAX);
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut upsert = transaction.prepare(
                "INSERT OR REPLACE INTO entries (kind, key, tool, payload, bytes, last_used)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            if let Some((context, payload)) = &update.classpath {
                upsert.execute(params![
                    CLASSPATH_KIND,
                    context,
                    self.tool,
                    payload,
                    entry_bytes(context, payload),
                    now
                ])?;
            }
            for record in update.records {
                let Some(key) = record.path.to_str() else {
                    continue;
                };
                upsert.execute(params![
                    RECORD_KIND,
                    key,
                    self.tool,
                    record.payload,
                    entry_bytes(key, &record.payload),
                    now
                ])?;
            }
            let mut touch = transaction.prepare(
                "UPDATE entries SET last_used = ?1 WHERE kind = ?2 AND key = ?3 AND tool = ?4",
            )?;
            for key in update.reused.iter().filter_map(|path| path.to_str()) {
                touch.execute(params![now, RECORD_KIND, key, self.tool])?;
            }
        }
        evict_to_budget(&transaction, self.max_bytes)?;
        transaction.commit()?;
        connection.execute_batch("PRAGMA incremental_vacuum;")?;
        Ok(())
    }

    /// Opens the database, creating the private directory, the file and the schema when
    /// needed. An unreadable or incompatible database is deleted and created again. The shared
    /// lock is held while opening, so no recovery deletes the file between its creation and
    /// its use, and a diagnosis always belongs to the epoch read with it.
    fn open(&self) -> Result<Connection> {
        let lock = self.lock_file()?;
        lock.lock_shared()
            .context("cannot lock the index cache for reading")?;
        let epoch = read_epoch(&lock)?;
        let opened = self.try_open();
        drop(lock);
        match opened {
            Err(error) if is_damaged(&error) => {
                tracing::warn!(
                    error_kind = %crate::log_safety::error_category(&error),
                    "rebuilding a damaged or incompatible index cache"
                );
                self.recover(epoch)
            }
            result => result,
        }
    }

    fn lock_path(&self) -> PathBuf {
        let mut path = self.database_path().into_os_string();
        path.push(".lock");
        PathBuf::from(path)
    }

    /// The private lock file that serializes recovery between processes. It holds the
    /// recovery epoch: the number of times the database was deleted and created again.
    fn lock_file(&self) -> Result<File> {
        create_private_directory(&self.directory)?;
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.lock_path())
            .context("cannot open the index cache lock")
    }

    /// The recovery epoch observed before a diagnosis, read under a shared lock so that it
    /// never sees a recovery in progress.
    fn recovery_epoch(&self) -> Result<u64> {
        let lock = self.lock_file()?;
        lock.lock_shared()
            .context("cannot lock the index cache for reading")?;
        read_epoch(&lock)
    }

    /// Deletes and creates the database again unless another process already did since
    /// `observed` was read: the exclusive lock serializes every recovery, and the epoch shows
    /// whether the damaged file this process diagnosed is still the one at the path. The new
    /// schema is created before the lock is released, so no recovery can delete it half made.
    fn recover(&self, observed: u64) -> Result<Connection> {
        let mut lock = self.lock_file()?;
        lock.lock()
            .context("cannot lock the index cache for recovery")?;
        let current = read_epoch(&lock)?;
        if current == observed {
            self.reset()?;
            write_epoch(&mut lock, current.wrapping_add(1))?;
        }
        self.try_open()
    }

    fn try_open(&self) -> Result<Connection> {
        create_private_directory(&self.directory)?;
        let path = self.database_path();
        // Created before SQLite opens it, so the file is never readable by other users.
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)
            .context("cannot create the index cache database")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let connection = Connection::open(&path).context("cannot open the index cache database")?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        initialize_schema(&connection)?;
        Ok(connection)
    }

    /// Deletes the database and its journals; every entry is derived data. Only
    /// [`Self::recover`] calls it, under the exclusive recovery lock. The database goes last:
    /// until it is gone no process can create a new one whose journal this would delete.
    fn reset(&self) -> Result<()> {
        for suffix in ["-journal", "-wal", "-shm", ""] {
            let mut file = self.database_path().into_os_string();
            file.push(suffix);
            match fs::remove_file(PathBuf::from(file)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("cannot delete the damaged index cache"),
            }
        }
        Ok(())
    }
}

fn read_epoch(mut lock: &File) -> Result<u64> {
    let mut text = String::new();
    lock.read_to_string(&mut text)
        .context("cannot read the index cache lock")?;
    // An empty or unreadable epoch is the initial one: a recovery always rewrites it.
    Ok(text.trim().parse().unwrap_or(0))
}

fn write_epoch(lock: &mut File, epoch: u64) -> Result<()> {
    use std::io::{Seek, SeekFrom};

    lock.set_len(0)?;
    lock.seek(SeekFrom::Start(0))?;
    lock.write_all(epoch.to_string().as_bytes())?;
    lock.sync_data()
        .context("cannot record the index cache recovery")
}

/// Read access to stored entries through one open connection.
pub struct CacheReader {
    connection: Connection,
    tool: String,
}

impl CacheReader {
    fn load(&self, kind: &str, key: &str) -> Option<Vec<u8>> {
        self.connection
            .query_row(
                "SELECT payload FROM entries WHERE kind = ?1 AND key = ?2 AND tool = ?3",
                params![kind, key, self.tool],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .unwrap_or_else(|_| {
                tracing::warn!(
                    error_kind = %"sqlite",
                    "index cache lookup failed; treating it as a miss"
                );
                None
            })
    }
}

impl PersistedRecords for CacheReader {
    fn load(&self, path: &Path) -> Option<Vec<u8>> {
        CacheReader::load(self, RECORD_KIND, path.to_str()?)
    }
}

/// A database that exists but was written in another format.
#[derive(Debug)]
struct IncompatibleCache(String);

impl std::fmt::Display for IncompatibleCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "index cache has incompatible format {}", self.0)
    }
}

impl std::error::Error for IncompatibleCache {}

fn is_damaged(error: &anyhow::Error) -> bool {
    error.is::<IncompatibleCache>()
        || error.chain().any(|cause| {
            cause
                .downcast_ref::<rusqlite::Error>()
                .and_then(rusqlite::Error::sqlite_error_code)
                .is_some_and(|code| {
                    matches!(code, ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt)
                })
        })
}

fn initialize_schema(connection: &Connection) -> Result<()> {
    let format = INDEX_CACHE_FORMAT.to_string();
    let stored = connection
        .query_row("SELECT value FROM meta WHERE name = 'format'", [], |row| {
            row.get::<_, String>(0)
        })
        .optional();
    match stored {
        Ok(Some(value)) if value == format => Ok(()),
        Ok(Some(value)) => Err(IncompatibleCache(value).into()),
        Ok(None) => Err(IncompatibleCache("marker missing".to_owned()).into()),
        Err(rusqlite::Error::SqliteFailure(_, Some(message)))
            if message.starts_with("no such table") =>
        {
            create_schema(connection, &format)
        }
        Err(error) => Err(error.into()),
    }
}

fn create_schema(connection: &Connection, format: &str) -> Result<()> {
    // auto_vacuum must be chosen before the first table exists.
    connection.execute_batch(
        "PRAGMA auto_vacuum = INCREMENTAL;
         BEGIN IMMEDIATE;
         CREATE TABLE IF NOT EXISTS meta (name TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS entries (
             kind TEXT NOT NULL,
             key TEXT NOT NULL,
             tool TEXT NOT NULL,
             payload BLOB NOT NULL,
             bytes INTEGER NOT NULL,
             last_used INTEGER NOT NULL,
             PRIMARY KEY (kind, key, tool)
         );
         CREATE INDEX IF NOT EXISTS entries_by_use ON entries (last_used);",
    )?;
    let inserted = connection.execute(
        "INSERT OR IGNORE INTO meta (name, value) VALUES ('format', ?1)",
        params![format],
    );
    let committed = inserted.and_then(|_| connection.execute_batch("COMMIT;"));
    if let Err(error) = committed {
        let _ = connection.execute_batch("ROLLBACK;");
        return Err(error.into());
    }
    // A concurrent process may have created the schema first.
    initialize_schema(connection)
}

fn entry_bytes(key: &str, payload: &[u8]) -> i64 {
    i64::try_from(key.len() + payload.len()).unwrap_or(i64::MAX)
}

/// Deletes the least recently used entries, oldest first, until the cache fits `max_bytes`.
fn evict_to_budget(connection: &Connection, max_bytes: u64) -> Result<()> {
    let limit = i64::try_from(max_bytes).unwrap_or(i64::MAX);
    let mut excess = total_bytes(connection)?.saturating_sub(limit);
    if excess <= 0 {
        return Ok(());
    }
    let mut victims = Vec::new();
    {
        let mut oldest =
            connection.prepare("SELECT rowid, bytes FROM entries ORDER BY last_used, rowid")?;
        let mut rows = oldest.query([])?;
        while excess > 0
            && let Some(row) = rows.next()?
        {
            victims.push(row.get::<_, i64>(0)?);
            excess -= row.get::<_, i64>(1)?;
        }
    }
    let mut delete = connection.prepare("DELETE FROM entries WHERE rowid = ?1")?;
    for rowid in victims {
        delete.execute(params![rowid])?;
    }
    Ok(())
}

fn total_bytes(connection: &Connection) -> rusqlite::Result<i64> {
    connection.query_row("SELECT COALESCE(SUM(bytes), 0) FROM entries", [], |row| {
        row.get(0)
    })
}

fn create_private_directory(directory: &Path) -> Result<()> {
    fs::create_dir_all(directory).context("cannot create the index cache directory")?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
        .context("cannot make the index cache directory private")
}

/// The Maven-selected classpath of one project context and the stamps it depends on.
#[derive(Debug, Serialize, Deserialize)]
pub struct ClasspathManifest {
    context: String,
    project: ProjectFingerprint,
    inputs: ProjectFingerprint,
    classpath: Vec<PathBuf>,
}

impl ClasspathManifest {
    /// `None` when a stamp is too recent to prove that a later change would be visible.
    pub fn settled(
        context: &str,
        project: &ProjectFingerprint,
        inputs: ProjectFingerprint,
        classpath: Vec<PathBuf>,
    ) -> Option<Self> {
        (project.is_settled() && inputs.is_settled()).then(|| Self {
            context: context.to_owned(),
            project: project.clone(),
            inputs,
            classpath,
        })
    }

    /// The classpath and the input stamps it was resolved from, when the manifest belongs to
    /// `context`, the project fingerprint is unchanged, no classpath input changed, and every
    /// classpath entry still exists.
    pub fn reusable_classpath(
        self,
        context: &str,
        project: &ProjectFingerprint,
    ) -> Option<(Vec<PathBuf>, ProjectFingerprint)> {
        (self.context == context
            && self.project == *project
            && self.inputs.is_current()
            && self.classpath.iter().all(|path| path.exists()))
        .then_some((self.classpath, self.inputs))
    }
}

#[derive(Serialize)]
struct ClasspathContext<'a> {
    format: u32,
    project_root: &'a Path,
    classpath_scope: &'a str,
    maven_executable: Option<&'a Path>,
    execution_repository: Option<&'a Path>,
    network_enabled: bool,
    java: String,
    /// FNV-1a digest of the whole environment the Maven child inherits, so its values
    /// (possibly credentials) are never written to the cache.
    environment: String,
}

/// Exact identity of a classpath resolution: project, scope, the configuration that selects
/// Maven, the repository and Java, and every inherited variable. POMs, settings, `.mvnrc`
/// files and Java tool options may read any variable (`${env.X}`, `JAVA_TOOL_OPTIONS`), so
/// only the variables this server removes from the Maven child are left out.
pub fn classpath_context(root: &Path, config: &Config) -> String {
    let execution = &config.execution;
    let java = match &execution.java_environment {
        JavaEnvironment::Inherit => "inherit".to_owned(),
        JavaEnvironment::Jenv { root } => format!("jenv:{}", root.display()),
    };
    serde_json::to_string(&ClasspathContext {
        format: INDEX_CACHE_FORMAT,
        project_root: root,
        classpath_scope: "test",
        maven_executable: execution.maven_executable.as_deref(),
        execution_repository: execution.execution_repository.as_deref(),
        network_enabled: execution.network_enabled,
        java,
        environment: format!("{:016x}", environment_digest(std::env::vars_os())),
    })
    .unwrap_or_default()
}

/// Order-independent digest of `variables` without [`SERVER_ONLY_ENVIRONMENT`].
fn environment_digest(variables: impl IntoIterator<Item = (OsString, OsString)>) -> u64 {
    variables
        .into_iter()
        .filter(|(name, _)| {
            !SERVER_ONLY_ENVIRONMENT
                .iter()
                .any(|server_only| name.as_os_str() == *server_only)
        })
        .collect::<BTreeMap<_, _>>()
        .iter()
        .fold(FNV_OFFSET, |hash, (name, value)| {
            fnv1a(
                fnv1a(hash, name.as_encoded_bytes()),
                value.as_encoded_bytes(),
            )
        })
}

/// Files outside the reactor that can change which artifacts Maven selects: user settings,
/// toolchains and extensions, the `mavenrc` files the launcher sources, the jenv version
/// files that select Java, and the POM beside every repository artifact.
pub fn classpath_inputs(
    root: &Path,
    config: &Config,
    classpath: &[PathBuf],
    repository_root: Option<&Path>,
) -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from("/etc/mavenrc")];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        paths.push(home.join(".m2/settings.xml"));
        paths.push(home.join(".m2/toolchains.xml"));
        paths.push(home.join(".m2/extensions.xml"));
        paths.push(home.join(".mavenrc"));
    }
    if let JavaEnvironment::Jenv { root: jenv_root } = &config.execution.java_environment {
        paths.push(jenv_root.join("version"));
        paths.extend(
            root.ancestors()
                .map(|directory| directory.join(".java-version")),
        );
    }
    paths.extend(repository_poms(classpath, repository_root));
    paths.sort();
    paths.dedup();
    paths
}

fn repository_poms(classpath: &[PathBuf], repository_root: Option<&Path>) -> Vec<PathBuf> {
    let Some(repository_root) = repository_root else {
        return Vec::new();
    };
    classpath
        .iter()
        .filter(|path| path.starts_with(repository_root))
        .filter_map(|path| sibling_pom(path))
        .collect()
}

/// What one Maven classpath resolution read besides the reactor model.
pub struct ResolutionSources<'a> {
    pub root: &'a Path,
    pub config: &'a Config,
    pub classpath: &'a [PathBuf],
    pub repository_root: Option<&'a Path>,
    /// Every reactor POM; their relative parents and imports are followed.
    pub reactor_poms: &'a [PathBuf],
    /// Directories of the Maven installation (global `conf`, `lib/ext`).
    pub installation: &'a [PathBuf],
}

/// Stamps of every input of one classpath resolution: [`classpath_inputs`], the Maven
/// installation's configuration, and the parent and imported BOM POMs of the reactor and of
/// every repository artifact, transitively. `None` when a parent or import cannot be
/// identified, so the classpath must not be persisted.
pub fn resolution_inputs(sources: &ResolutionSources<'_>) -> Option<ProjectFingerprint> {
    let model = model_inputs::model_inputs(
        sources.reactor_poms,
        &repository_poms(sources.classpath, sources.repository_root),
        sources.repository_root,
    )?;
    Some(ProjectFingerprint::capture_trees(
        classpath_inputs(
            sources.root,
            sources.config,
            sources.classpath,
            sources.repository_root,
        )
        .into_iter()
        .chain(sources.installation.iter().cloned())
        .chain(model),
    ))
}

/// `<artifactId>-<version>.pom` in the directory of a repository artifact file.
fn sibling_pom(artifact: &Path) -> Option<PathBuf> {
    let version_directory = artifact.parent()?;
    let version = version_directory.file_name()?.to_str()?;
    let artifact_id = version_directory.parent()?.file_name()?.to_str()?;
    Some(version_directory.join(format!("{artifact_id}-{version}.pom")))
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0100_0000_01b3;

/// FNV-1a over `bytes` followed by a separator, so adjacent values cannot shift into each
/// other.
fn fnv1a(hash: u64, bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .chain(std::iter::once(&0xff))
        .fold(hash, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
        })
}

#[cfg(test)]
#[path = "../tests/unit/index_cache.rs"]
mod tests;
