use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{config::Config, index::IndexStats};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeReport {
    pub schema_version: u32,
    pub instances: Vec<RuntimeInstanceStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeInstanceStatus {
    pub schema_version: u32,
    pub binary_version: String,
    pub pid: u32,
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
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeCacheStatus {
    pub entries: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectIndexStatus {
    pub project_path: String,
    pub last_used_unix_ms: u128,
    pub index: IndexStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessMemoryStatus {
    pub rss_bytes: Option<u64>,
    pub peak_rss_bytes: Option<u64>,
}

#[derive(Debug)]
pub struct RuntimeStatusPublisher {
    path: PathBuf,
    status: Mutex<RuntimeInstanceStatus>,
}

impl RuntimeStatusPublisher {
    pub fn register(config: &Config) -> Result<Self> {
        let directory = runtime_dir()?;
        fs::create_dir_all(&directory)
            .with_context(|| format!("cannot create runtime directory {}", directory.display()))?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let pid = std::process::id();
        let now = unix_ms();
        let publisher = Self {
            path: directory.join(format!("{pid}.json")),
            status: Mutex::new(RuntimeInstanceStatus {
                schema_version: 1,
                binary_version: env!("CARGO_PKG_VERSION").to_owned(),
                pid,
                started_unix_ms: now,
                updated_unix_ms: now,
                memory: process_memory(),
                config: RuntimeConfigStatus {
                    max_results: config.max_results,
                    max_source_bytes: config.max_source_bytes,
                    max_project_indexes: config.max_project_indexes,
                },
                cache: RuntimeCacheStatus::default(),
                projects: Vec::new(),
            }),
        };
        publisher.publish()?;
        Ok(publisher)
    }

    pub fn update_cache(
        &self,
        cache: RuntimeCacheStatus,
        projects: Vec<ProjectIndexStatus>,
    ) -> Result<()> {
        {
            let mut status = self.status.lock().expect("runtime status mutex poisoned");
            status.updated_unix_ms = unix_ms();
            status.memory = process_memory();
            status.cache = cache;
            status.projects = projects;
        }
        self.publish()
    }

    fn publish(&self) -> Result<()> {
        let status = self.status.lock().expect("runtime status mutex poisoned");
        let temporary = self.path.with_extension("json.tmp");
        fs::write(&temporary, serde_json::to_vec_pretty(&*status)?)?;
        fs::rename(&temporary, &self.path)?;
        Ok(())
    }
}

impl Drop for RuntimeStatusPublisher {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn read_runtime_report() -> Result<RuntimeReport> {
    let directory = runtime_dir()?;
    let mut instances = Vec::new();
    if !directory.is_dir() {
        return Ok(RuntimeReport {
            schema_version: 1,
            instances,
        });
    }
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let status = fs::read(entry.path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<RuntimeInstanceStatus>(&bytes).ok());
        let Some(status) = status else {
            continue;
        };
        if process_is_alive(status.pid) {
            instances.push(status);
        } else {
            let _ = fs::remove_file(entry.path());
        }
    }
    instances.sort_by_key(|instance| instance.pid);
    Ok(RuntimeReport {
        schema_version: 1,
        instances,
    })
}

pub fn format_runtime_report(report: &RuntimeReport) -> String {
    if report.instances.is_empty() {
        return "No live maven-mcp instances.\n".to_owned();
    }
    let mut output = String::new();
    for instance in &report.instances {
        output.push_str(&format!(
            "maven-mcp pid={} version={} rss={} peak_rss={}\n",
            instance.pid,
            instance.binary_version,
            bytes_label(instance.memory.rss_bytes),
            bytes_label(instance.memory.peak_rss_bytes)
        ));
        output.push_str(&format!(
            "  cache: entries={} hits={} misses={} evictions={} max_project_indexes={}\n",
            instance.cache.entries,
            instance.cache.hits,
            instance.cache.misses,
            instance.cache.evictions,
            instance.config.max_project_indexes
        ));
        for project in &instance.projects {
            output.push_str(&format!(
                "  project: {} jars={} sources={} classes={} artifacts={}\n",
                project.project_path,
                project.index.jar_count,
                project.index.source_jar_count,
                project.index.class_count,
                project.index.artifact_count
            ));
        }
    }
    output
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

fn process_is_alive(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).is_dir()
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

fn bytes_label(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |bytes| bytes.to_string())
}
