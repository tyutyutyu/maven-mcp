use std::{
    env,
    ffi::OsString,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use anyhow::{Context, Result, bail};

pub const DEFAULT_MAX_RESULTS: usize = 100;
pub const DEFAULT_MAX_SOURCE_BYTES: usize = 1_048_576;
pub const DEFAULT_MAVEN_TIMEOUT_SECONDS: usize = 300;
pub const DEFAULT_MAX_MAVEN_OUTPUT_BYTES: usize = 1_048_576;
pub const DEFAULT_MAX_PROJECT_INDEXES: usize = 4;
pub const DEFAULT_MAX_PROJECT_INDEX_BYTES: usize = 1_073_741_824;
pub const DEFAULT_MAX_HEAVY_JOBS: usize = 2;
pub const DEFAULT_MAX_QUEUED_HEAVY_JOBS: usize = 16;
pub const DEFAULT_MAX_XML_BYTES: usize = 16_777_216;
pub const DEFAULT_MAX_JAR_ENTRIES: usize = 200_000;
pub const DEFAULT_MAX_INDEX_ENTRIES: usize = 20_000_000;
pub const DEFAULT_MAX_INDEX_NAME_BYTES: usize = 2_147_483_648;
pub const DEFAULT_MAX_INDEX_CACHE_BYTES: usize = 1_073_741_824;
pub const MAX_ENTRY_NAME_BYTES: usize = 4096;

static MAX_XML_BYTES: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_XML_BYTES);
static MAX_JAR_ENTRIES: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_JAR_ENTRIES);
static MAX_INDEX_ENTRIES: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_INDEX_ENTRIES);
static MAX_INDEX_NAME_BYTES: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_INDEX_NAME_BYTES);

pub fn max_xml_bytes() -> usize {
    MAX_XML_BYTES.load(Ordering::Relaxed)
}

pub fn max_jar_entries() -> usize {
    MAX_JAR_ENTRIES.load(Ordering::Relaxed)
}

pub fn max_index_entries() -> usize {
    MAX_INDEX_ENTRIES.load(Ordering::Relaxed)
}

pub fn max_index_name_bytes() -> usize {
    MAX_INDEX_NAME_BYTES.load(Ordering::Relaxed)
}

pub fn set_resource_limits(xml: usize, jar: usize, index: usize, names: usize) {
    MAX_XML_BYTES.store(xml, Ordering::Relaxed);
    MAX_JAR_ENTRIES.store(jar, Ordering::Relaxed);
    MAX_INDEX_ENTRIES.store(index, Ordering::Relaxed);
    MAX_INDEX_NAME_BYTES.store(names, Ordering::Relaxed);
}

/// Reads an XML file as UTF-8 text, failing deterministically when it exceeds
/// the configured `MAX_XML_BYTES` limit instead of truncating it.
pub fn read_bounded_xml(path: &Path) -> Result<String> {
    read_bounded_xml_with_limit(path, max_xml_bytes())
}

fn read_bounded_xml_with_limit(path: &Path, limit: usize) -> Result<String> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("XML file exceeds configured MAX_XML_BYTES limit of {limit} bytes");
    }
    let xml = String::from_utf8(bytes).context("XML file is not valid UTF-8")?;
    validate_xml_structure(&xml)?;
    Ok(xml)
}

pub const MAX_XML_DEPTH: usize = 128;
pub const MAX_XML_ELEMENTS: usize = 200_000;
pub const MAX_XML_TOTAL_BYTES: usize = 67_108_864;
pub const MAX_XML_FILES: usize = 4096;

fn validate_xml_structure(xml: &str) -> Result<()> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let (mut depth, mut elements) = (0usize, 0usize);
    loop {
        match reader.read_event()? {
            Event::Start(_) => {
                depth += 1;
                elements += 1;
            }
            Event::Empty(_) => {
                if depth >= MAX_XML_DEPTH {
                    bail!("XML exceeds MAX_XML_DEPTH limit of {MAX_XML_DEPTH}");
                }
                elements += 1;
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
            }
            Event::Eof => break,
            _ => {}
        }
        if depth > MAX_XML_DEPTH {
            bail!("XML exceeds MAX_XML_DEPTH limit of {MAX_XML_DEPTH}");
        }
        if elements > MAX_XML_ELEMENTS {
            bail!("XML exceeds MAX_XML_ELEMENTS limit of {MAX_XML_ELEMENTS}");
        }
    }
    Ok(())
}

#[derive(Default)]
pub struct XmlBudget {
    bytes: usize,
    files: usize,
}

impl XmlBudget {
    pub fn read(&mut self, path: &Path) -> Result<String> {
        if self.files >= MAX_XML_FILES {
            bail!("XML set exceeds MAX_XML_FILES limit of {MAX_XML_FILES}");
        }
        let remaining = MAX_XML_TOTAL_BYTES - self.bytes;
        let xml = read_bounded_xml_with_limit(path, max_xml_bytes().min(remaining))
            .map_err(|error| {
                let message = format!("XML read failed within MAX_XML_BYTES and MAX_XML_TOTAL_BYTES budgets: {error:#}");
                error.context(message)
            })?;
        self.bytes += xml.len();
        self.files += 1;
        Ok(xml)
    }
}

fn resource_env(name: &str, default: usize, maximum: usize) -> Result<usize> {
    validate_resource_limit(name, positive_env(name, default)?, maximum)
}

fn validate_resource_limit(name: &str, value: usize, maximum: usize) -> Result<usize> {
    if value == 0 || value > maximum {
        bail!("{name} must be between 1 and {maximum}");
    }
    Ok(value)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JavaEnvironment {
    Inherit,
    Jenv { root: PathBuf },
}

#[derive(Debug, Clone)]
pub struct MavenExecutionConfig {
    pub trusted_project_directories: Vec<PathBuf>,
    pub maven_executable: Option<PathBuf>,
    pub execution_repository: Option<PathBuf>,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    pub max_results: usize,
    pub network_enabled: bool,
    pub java_environment: JavaEnvironment,
}

#[derive(Debug, Clone)]
pub struct ProjectExecutionConfig {
    pub project_root: PathBuf,
    pub maven_executable: Option<PathBuf>,
    pub execution_repository: Option<PathBuf>,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    pub max_results: usize,
    pub network_enabled: bool,
    pub java_environment: JavaEnvironment,
}

impl ProjectExecutionConfig {
    pub fn from_root(project_root: PathBuf, config: &MavenExecutionConfig) -> Self {
        Self {
            project_root,
            maven_executable: config.maven_executable.clone(),
            execution_repository: config.execution_repository.clone(),
            timeout: config.timeout,
            max_output_bytes: config.max_output_bytes,
            max_results: config.max_results,
            network_enabled: config.network_enabled,
            java_environment: config.java_environment.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub max_results: usize,
    pub max_source_bytes: usize,
    pub max_project_indexes: usize,
    pub max_project_index_bytes: usize,
    /// Heavy index, archive and file jobs that may execute at the same time.
    pub max_heavy_jobs: usize,
    /// Heavy jobs that may wait for a running slot; further jobs are rejected.
    pub max_queued_heavy_jobs: usize,
    pub execution: MavenExecutionConfig,
    /// The on-disk index cache, or `None` when it is disabled or has no location.
    pub index_cache: Option<IndexCacheConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexCacheConfig {
    pub directory: PathBuf,
    pub max_bytes: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_env_with_jenv(false)
    }

    pub fn from_env_with_jenv(use_jenv: bool) -> Result<Self> {
        set_resource_limits(
            resource_env("MAX_XML_BYTES", DEFAULT_MAX_XML_BYTES, MAX_XML_TOTAL_BYTES)?,
            resource_env("MAX_JAR_ENTRIES", DEFAULT_MAX_JAR_ENTRIES, 1_000_000)?,
            resource_env(
                "MAX_INDEX_ENTRIES",
                DEFAULT_MAX_INDEX_ENTRIES,
                DEFAULT_MAX_INDEX_ENTRIES,
            )?,
            resource_env(
                "MAX_INDEX_NAME_BYTES",
                DEFAULT_MAX_INDEX_NAME_BYTES,
                DEFAULT_MAX_INDEX_NAME_BYTES,
            )?,
        );
        Ok(Self {
            max_results: positive_env("MAX_RESULTS", DEFAULT_MAX_RESULTS)?,
            max_source_bytes: positive_env("MAX_SOURCE_BYTES", DEFAULT_MAX_SOURCE_BYTES)?,
            max_project_indexes: positive_env("MAX_PROJECT_INDEXES", DEFAULT_MAX_PROJECT_INDEXES)?,
            max_project_index_bytes: positive_env(
                "MAX_PROJECT_INDEX_BYTES",
                DEFAULT_MAX_PROJECT_INDEX_BYTES,
            )?,
            max_heavy_jobs: positive_env("MAX_HEAVY_JOBS", DEFAULT_MAX_HEAVY_JOBS)?,
            max_queued_heavy_jobs: positive_env(
                "MAX_QUEUED_HEAVY_JOBS",
                DEFAULT_MAX_QUEUED_HEAVY_JOBS,
            )?,
            execution: maven_execution_from_env(use_jenv)?,
            index_cache: index_cache_from_env()?,
        })
    }
}

/// Variables that configure only this server's own limits, logging, cache and stats. They
/// are removed from every Maven child, so they cannot change a Maven result and are left out
/// of the persisted classpath identity, which digests the rest of the environment.
pub const SERVER_ONLY_ENVIRONMENT: [&str; 17] = [
    "MAX_RESULTS",
    "MAX_SOURCE_BYTES",
    "MAX_PROJECT_INDEXES",
    "MAX_PROJECT_INDEX_BYTES",
    "MAX_HEAVY_JOBS",
    "MAX_QUEUED_HEAVY_JOBS",
    "MAX_XML_BYTES",
    "MAX_JAR_ENTRIES",
    "MAX_INDEX_ENTRIES",
    "MAX_INDEX_NAME_BYTES",
    "MAX_INDEX_CACHE_BYTES",
    "MAX_MAVEN_OUTPUT_BYTES",
    "MAVEN_TIMEOUT_SECONDS",
    "MAVEN_MCP_INDEX_CACHE",
    "MAVEN_MCP_CACHE_DIR",
    "MAVEN_MCP_RUNTIME_DIR",
    "RUST_LOG",
];

fn index_cache_from_env() -> Result<Option<IndexCacheConfig>> {
    let max_bytes = positive_env("MAX_INDEX_CACHE_BYTES", DEFAULT_MAX_INDEX_CACHE_BYTES)?;
    if !boolean_env_or("MAVEN_MCP_INDEX_CACHE", true)? {
        return Ok(None);
    }
    let directory = index_cache_directory(
        env::var_os("MAVEN_MCP_CACHE_DIR"),
        env::var_os("XDG_CACHE_HOME"),
        env::var_os("HOME"),
    )?;
    Ok(directory.map(|directory| IndexCacheConfig {
        directory,
        max_bytes: u64::try_from(max_bytes).unwrap_or(u64::MAX),
    }))
}

/// `MAVEN_MCP_CACHE_DIR` when set (it must be absolute), otherwise `maven-mcp/index` below
/// an absolute `XDG_CACHE_HOME` or `~/.cache`; `None` when no location is known.
fn index_cache_directory(
    configured: Option<OsString>,
    xdg_cache_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<Option<PathBuf>> {
    if let Some(configured) = configured {
        let directory = PathBuf::from(configured);
        if !directory.is_absolute() {
            bail!("MAVEN_MCP_CACHE_DIR must be an absolute path");
        }
        return Ok(Some(directory));
    }
    let base = xdg_cache_home
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home.map(|home| PathBuf::from(home).join(".cache")));
    Ok(base.map(|base| base.join("maven-mcp/index")))
}

fn maven_execution_from_env(use_jenv: bool) -> Result<MavenExecutionConfig> {
    let timeout_seconds = positive_env("MAVEN_TIMEOUT_SECONDS", DEFAULT_MAVEN_TIMEOUT_SECONDS)?;
    let timeout_seconds =
        u64::try_from(timeout_seconds).context("MAVEN_TIMEOUT_SECONDS is too large")?;
    Ok(MavenExecutionConfig {
        trusted_project_directories: trusted_project_directories_from_env()?,
        maven_executable: env::var_os("MAVEN_EXECUTABLE").map(PathBuf::from),
        execution_repository: env::var_os("MAVEN_EXECUTION_REPO_PATH").map(PathBuf::from),
        timeout: Duration::from_secs(timeout_seconds),
        max_output_bytes: positive_env("MAX_MAVEN_OUTPUT_BYTES", DEFAULT_MAX_MAVEN_OUTPUT_BYTES)?,
        max_results: positive_env("MAX_RESULTS", DEFAULT_MAX_RESULTS)?,
        network_enabled: boolean_env("MAVEN_EXECUTION_NETWORK")?,
        java_environment: java_environment_from_env(use_jenv)?,
    })
}

fn java_environment_from_env(use_jenv: bool) -> Result<JavaEnvironment> {
    if !use_jenv {
        return Ok(JavaEnvironment::Inherit);
    }
    let root = env::var_os("JENV_ROOT")
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".jenv").into()))
        .map(PathBuf::from)
        .context("--jenv requires JENV_ROOT or HOME")?;
    if !root.is_absolute() {
        bail!("JENV_ROOT must be an absolute path when --jenv is enabled");
    }
    Ok(JavaEnvironment::Jenv { root })
}

fn trusted_project_directories_from_env() -> Result<Vec<PathBuf>> {
    let Some(value) = env::var_os("MAVEN_TRUSTED_PROJECT_DIRECTORIES") else {
        return Ok(Vec::new());
    };
    parse_trusted_project_directories(value)
}

fn parse_trusted_project_directories(value: OsString) -> Result<Vec<PathBuf>> {
    let mut directories = env::split_paths(&value)
        .map(|path| canonicalize_absolute_directory(&path))
        .collect::<Result<Vec<_>>>()?;
    if directories.is_empty() {
        bail!("MAVEN_TRUSTED_PROJECT_DIRECTORIES must contain at least one directory");
    }
    directories.sort();
    directories.dedup();
    Ok(directories)
}

fn canonicalize_absolute_directory(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("MAVEN_TRUSTED_PROJECT_DIRECTORIES entries must be absolute paths");
    }
    let canonical = path
        .canonicalize()
        .with_context(|| "MAVEN_TRUSTED_PROJECT_DIRECTORIES contains an unreadable directory")?;
    if !canonical.is_dir() {
        bail!("MAVEN_TRUSTED_PROJECT_DIRECTORIES entries must be directories");
    }
    Ok(canonical)
}

fn boolean_env(name: &str) -> Result<bool> {
    boolean_env_or(name, false)
}

fn boolean_env_or(name: &str, default: bool) -> Result<bool> {
    match env::var(name) {
        Ok(value) if value.eq_ignore_ascii_case("true") || value == "1" => Ok(true),
        Ok(value) if value.eq_ignore_ascii_case("false") || value == "0" => Ok(false),
        Ok(_) => bail!("{name} must be true, false, 1, or 0"),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error).with_context(|| format!("cannot read {name}")),
    }
}

fn positive_env(name: &str, default: usize) -> Result<usize> {
    let Some(value) = env::var_os(name) else {
        return Ok(default);
    };
    let parsed = value
        .to_string_lossy()
        .parse::<usize>()
        .with_context(|| format!("{name} must be a positive integer"))?;
    if parsed == 0 {
        bail!("{name} must be greater than zero");
    }
    Ok(parsed)
}

#[cfg(test)]
#[path = "../tests/unit/config.rs"]
mod tests;

#[cfg(test)]
#[path = "../tests/unit/config_resource_limit_tests.rs"]
mod resource_limit_tests;
