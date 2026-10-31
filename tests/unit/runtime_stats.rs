use std::process::Command;

use serde_json::json;
use tempfile::TempDir;

use super::*;

fn config() -> RuntimeConfigStatus {
    RuntimeConfigStatus {
        max_results: 10,
        max_source_bytes: 1024,
        max_project_indexes: 2,
        max_project_index_bytes: 4096,
    }
}

fn status(pid: u32, process_start_ticks: Option<u64>) -> RuntimeInstanceStatus {
    RuntimeInstanceStatus {
        schema_version: RUNTIME_SCHEMA_VERSION,
        binary_version: "0.0.0".to_owned(),
        build_commit: None,
        executable: None,
        pid,
        process_start_ticks,
        started_unix_ms: 1,
        updated_unix_ms: 1,
        memory: ProcessMemoryStatus {
            rss_bytes: None,
            peak_rss_bytes: None,
        },
        config: config(),
        cache: RuntimeCacheStatus::default(),
        projects: Vec::new(),
    }
}

fn own_start_ticks() -> Option<u64> {
    process_start_ticks(Path::new(PROC_ROOT), "self")
}

fn reaped_child_pid() -> u32 {
    let mut child = Command::new("true")
        .spawn()
        .expect("the true command must start");
    let pid = child.id();
    child.wait().expect("the child must be reaped");
    pid
}

fn write_status(directory: &Path, status: &RuntimeInstanceStatus) -> PathBuf {
    let path = directory.join(format!("{}.json", status.pid));
    fs::write(&path, serde_json::to_vec(status).unwrap()).unwrap();
    path
}

/// A procfs stand-in: `self` marks it as mounted, and each live PID gets a `stat` file.
fn fake_procfs() -> TempDir {
    let proc_root = TempDir::new().unwrap();
    fs::create_dir(proc_root.path().join("self")).unwrap();
    proc_root
}

fn fake_process(proc_root: &Path, pid: u32, start_ticks: u64) {
    let directory = proc_root.join(pid.to_string());
    fs::create_dir(&directory).unwrap();
    let fields_before_start_time = " 0".repeat(19);
    fs::write(
        directory.join("stat"),
        format!("{pid} (maven mcp){fields_before_start_time} {start_ticks} 0 0"),
    )
    .unwrap();
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn published(path: &Path) -> RuntimeInstanceStatus {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn cache_with_hits(hits: u64) -> RuntimeCacheStatus {
    RuntimeCacheStatus {
        hits,
        ..RuntimeCacheStatus::default()
    }
}

fn status_path(directory: &Path) -> PathBuf {
    directory.join(format!("{}.json", std::process::id()))
}

#[test]
fn reports_the_running_process_as_alive() {
    let own = status(std::process::id(), own_start_ticks());

    assert!(own.process_start_ticks.is_some());
    assert_eq!(
        instance_liveness(Path::new(PROC_ROOT), &own),
        Liveness::Alive
    );
}

#[test]
fn reports_a_reaped_child_process_as_dead() {
    let gone = status(reaped_child_pid(), Some(1));

    assert_eq!(
        instance_liveness(Path::new(PROC_ROOT), &gone),
        Liveness::Dead
    );
}

#[test]
fn a_reused_pid_with_another_start_time_proves_the_recorded_process_gone() {
    let reused = status(std::process::id(), own_start_ticks().map(|ticks| ticks + 1));

    assert_eq!(
        instance_liveness(Path::new(PROC_ROOT), &reused),
        Liveness::Dead
    );
}

#[test]
fn a_status_without_start_time_is_identified_by_pid_only() {
    let legacy = status(std::process::id(), None);

    assert_eq!(
        instance_liveness(Path::new(PROC_ROOT), &legacy),
        Liveness::Alive
    );
}

#[test]
fn unexpected_proc_entries_leave_liveness_unknown() {
    let proc_root = fake_procfs();
    fs::write(proc_root.path().join("41"), "not a process directory").unwrap();
    let malformed = proc_root.path().join("42");
    fs::create_dir(&malformed).unwrap();
    fs::write(malformed.join("stat"), "42 no command field").unwrap();

    assert_eq!(
        instance_liveness(proc_root.path(), &status(41, None)),
        Liveness::Unknown
    );
    assert_eq!(
        instance_liveness(proc_root.path(), &status(42, Some(7))),
        Liveness::Unknown
    );
}

#[test]
fn an_access_error_leaves_liveness_unknown() {
    let parent = TempDir::new().unwrap();
    let proc_root = parent.path().join("proc");
    fs::create_dir(&proc_root).unwrap();
    fs::set_permissions(&proc_root, fs::Permissions::from_mode(0o000)).unwrap();
    // The superuser bypasses permission checks, so there is no access error to observe.
    let denied = fs::symlink_metadata(proc_root.join("43"))
        .is_err_and(|error| error.kind() == ErrorKind::PermissionDenied);

    let liveness = instance_liveness(&proc_root, &status(43, None));

    fs::set_permissions(&proc_root, fs::Permissions::from_mode(0o700)).unwrap();
    if denied {
        assert_eq!(liveness, Liveness::Unknown);
    }
}

#[test]
fn parses_the_start_time_after_a_command_name_with_spaces_and_parentheses() {
    let fields_before_start_time = " 1".repeat(19);
    let stat = format!("7 (a) b (c)){fields_before_start_time} 4242 0 0");

    assert_eq!(parse_start_ticks(&stat), Some(4242));
    assert_eq!(parse_start_ticks("7 (truncated) R 1 2"), None);
    assert_eq!(parse_start_ticks("no command"), None);
}

#[test]
fn accepts_the_mounted_procfs_of_a_supported_linux_system() {
    ensure_procfs(Path::new(PROC_ROOT)).expect("Linux test hosts mount procfs at /proc");
}

#[test]
fn rejects_a_missing_procfs_instead_of_treating_instances_as_stale() {
    let directory = tempfile::tempdir().expect("temporary directory");

    let error = ensure_procfs(directory.path()).expect_err("an empty directory is not procfs");

    let message = error.to_string();
    assert!(
        message.contains("requires a mounted Linux procfs"),
        "{message}"
    );
}

#[test]
fn a_missing_procfs_fails_the_report_without_deleting_any_status() {
    let runtime = TempDir::new().unwrap();
    let missing_proc = TempDir::new().unwrap();
    let stale = write_status(runtime.path(), &status(reaped_child_pid(), None));

    assert!(read_runtime_report_from(missing_proc.path(), runtime.path()).is_err());
    assert!(stale.exists());
}

#[test]
fn report_lists_live_instances_deletes_only_proven_dead_ones_and_keeps_unknown_ones() {
    let proc_root = fake_procfs();
    let runtime = TempDir::new().unwrap();
    fake_process(proc_root.path(), 300, 11);
    fake_process(proc_root.path(), 100, 22);
    fake_process(proc_root.path(), 500, 33);
    fs::write(proc_root.path().join("400"), "").unwrap();
    let live_late = write_status(runtime.path(), &status(300, Some(11)));
    let live_early = write_status(runtime.path(), &status(100, Some(22)));
    let exited = write_status(runtime.path(), &status(200, Some(1)));
    let reused = write_status(runtime.path(), &status(500, Some(34)));
    let unknown = write_status(runtime.path(), &status(400, None));
    let invalid = runtime.path().join("600.json");
    fs::write(&invalid, "{ not json").unwrap();
    let misnamed = runtime.path().join("700.json");
    fs::write(&misnamed, serde_json::to_vec(&status(200, None)).unwrap()).unwrap();
    let temporary = runtime.path().join("200.json.tmp");
    fs::write(&temporary, "partial").unwrap();

    let report = read_runtime_report_from(proc_root.path(), runtime.path()).unwrap();

    let pids = report
        .instances
        .iter()
        .map(|instance| instance.pid)
        .collect::<Vec<_>>();
    assert_eq!(pids, [100, 300]);
    assert_eq!(report.unknown_instances, 1);
    assert!(live_late.exists() && live_early.exists());
    assert!(!exited.exists(), "a missing PID proves the process gone");
    assert!(!reused.exists(), "another start time proves the PID reused");
    assert!(unknown.exists() && invalid.exists() && misnamed.exists() && temporary.exists());
}

fn runtime_entries(directory: &Path) -> Vec<String> {
    let mut names = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn stale_cleanup_keeps_a_replacement_published_after_the_dead_status_was_read() {
    let runtime = TempDir::new().unwrap();
    let path = write_status(runtime.path(), &status(500, Some(34)));
    let read = read_status_file(&path).unwrap();
    // A new instance reuses the PID and replaces the file before the cleanup runs.
    let replacement = RuntimeInstanceStatus {
        started_unix_ms: 2,
        ..status(500, Some(35))
    };
    write_status(runtime.path(), &replacement);

    remove_dead_status_file(&path, &read);

    let kept = published(&path);
    assert_eq!(kept.process_start_ticks, Some(35));
    assert_eq!(kept.started_unix_ms, 2);
    assert_eq!(runtime_entries(runtime.path()), ["500.json"]);
}

#[test]
fn stale_cleanup_removes_a_status_that_still_holds_the_dead_instance() {
    let runtime = TempDir::new().unwrap();
    let dead = status(200, Some(1));
    let path = write_status(runtime.path(), &dead);

    remove_dead_status_file(&path, &dead);
    // A concurrent reader that already removed it leaves nothing to do.
    remove_dead_status_file(&path, &dead);

    assert!(runtime_entries(runtime.path()).is_empty());
}

#[test]
fn report_of_a_missing_runtime_directory_is_empty() {
    let parent = TempDir::new().unwrap();

    let report =
        read_runtime_report_from(Path::new(PROC_ROOT), &parent.path().join("absent")).unwrap();

    assert_eq!(report.schema_version, RUNTIME_SCHEMA_VERSION);
    assert!(report.instances.is_empty());
    assert_eq!(report.unknown_instances, 0);
}

#[test]
fn reads_current_process_memory_from_procfs() {
    let memory = process_memory();

    assert!(memory.rss_bytes.is_some_and(|bytes| bytes > 0));
    assert!(memory.peak_rss_bytes >= memory.rss_bytes);
}

#[test]
fn registration_publishes_a_private_status_and_closing_removes_it_for_good() {
    let parent = TempDir::new().unwrap();
    let runtime = parent.path().join("runtime");

    let publisher = RuntimeStatusPublisher::register_in(runtime.clone(), config()).unwrap();

    let path = status_path(&runtime);
    assert_eq!(mode(&runtime), 0o700);
    assert_eq!(mode(&path), 0o600);
    let registered = published(&path);
    assert_eq!(registered.pid, std::process::id());
    assert_eq!(registered.process_start_ticks, own_start_ticks());
    assert_eq!(registered.config.max_project_indexes, 2);

    publisher.close();
    assert!(!path.exists());
    publisher
        .update_cache(cache_with_hits(1), Vec::new())
        .unwrap();
    assert_eq!(publisher.record_hit(), None);
    assert!(
        !path.exists(),
        "a closed publisher never recreates its file"
    );
}

#[test]
fn dropping_the_publisher_removes_its_status() {
    let runtime = TempDir::new().unwrap();
    let publisher =
        RuntimeStatusPublisher::register_in(runtime.path().to_owned(), config()).unwrap();
    let path = status_path(runtime.path());
    assert!(path.exists());

    drop(publisher);

    assert!(!path.exists());
}

#[test]
fn cache_hits_are_coalesced_into_one_write_after_the_publication_delay() {
    let runtime = TempDir::new().unwrap();
    let publisher =
        RuntimeStatusPublisher::register_in(runtime.path().to_owned(), config()).unwrap();
    let path = status_path(runtime.path());
    let written = publisher.lock().last_written;

    let first = publisher.record_hit_at(written);
    let later = (0..100)
        .map(|offset| publisher.record_hit_at(written + Duration::from_millis(offset)))
        .collect::<Vec<_>>();

    assert_eq!(first, Some(HIT_PUBLICATION_DELAY));
    assert!(later.iter().all(Option::is_none), "{later:?}");
    assert_eq!(published(&path).cache.hits, 0, "a hit alone writes nothing");

    publisher
        .flush_hits(cache_with_hits(101), Vec::new())
        .unwrap();
    assert_eq!(published(&path).cache.hits, 101);

    let written = publisher.lock().last_written;
    assert_eq!(
        publisher.record_hit_at(written + HIT_PUBLICATION_DELAY / 4),
        Some(HIT_PUBLICATION_DELAY * 3 / 4)
    );
    publisher
        .flush_hits(cache_with_hits(102), Vec::new())
        .unwrap();
    let written = publisher.lock().last_written;
    assert_eq!(
        publisher.record_hit_at(written + HIT_PUBLICATION_DELAY * 2),
        Some(Duration::ZERO),
        "a hit after a quiet period is written at once"
    );
}

#[test]
fn a_structural_write_covers_pending_hits_so_the_deferred_flush_writes_nothing() {
    let runtime = TempDir::new().unwrap();
    let publisher =
        RuntimeStatusPublisher::register_in(runtime.path().to_owned(), config()).unwrap();
    let path = status_path(runtime.path());
    assert!(publisher.record_hit().is_some());

    publisher
        .update_cache(cache_with_hits(5), Vec::new())
        .unwrap();
    fs::write(&path, "sentinel").unwrap();
    publisher
        .flush_hits(cache_with_hits(6), Vec::new())
        .unwrap();

    assert_eq!(fs::read_to_string(&path).unwrap(), "sentinel");
    assert!(
        publisher.record_hit().is_some(),
        "the flush cleared the pending write, so the next hit schedules one"
    );
}

#[test]
fn older_status_files_without_the_new_fields_still_parse() {
    let mut legacy = serde_json::to_value(status(9, None)).unwrap();
    legacy
        .as_object_mut()
        .unwrap()
        .remove("process_start_ticks");
    legacy["projects"] = json!([{
        "project_path": "/work/app",
        "last_used_unix_ms": 5,
        "index": IndexStats::default(),
        "estimated_bytes": 10,
    }]);

    let parsed: RuntimeInstanceStatus = serde_json::from_value(legacy).unwrap();

    assert_eq!(parsed.process_start_ticks, None);
    assert_eq!(parsed.projects[0].state, IndexState::Ready);
    assert_eq!(parsed.projects[0].classpath_entries, None);
    assert_eq!(parsed.projects[0].build_duration_ms, None);
}

#[test]
fn json_names_the_index_state_in_snake_case() {
    assert_eq!(
        serde_json::to_value(IndexState::NotIndexed).unwrap(),
        "not_indexed"
    );
    assert_eq!(
        serde_json::to_value(IndexState::Building).unwrap(),
        "building"
    );
    assert_eq!(serde_json::to_value(IndexState::Ready).unwrap(), "ready");
}

#[test]
fn human_report_names_every_project_state_and_its_build_figures() {
    let mut instance = status(77, None);
    instance.started_unix_ms = unix_ms() - 5_000;
    instance.projects = vec![
        ProjectIndexStatus {
            project_path: "/work/app".to_owned(),
            last_used_unix_ms: unix_ms(),
            state: IndexState::Ready,
            index: IndexStats {
                jar_count: 3,
                source_jar_count: 1,
                class_count: 40,
                unique_class_count: 39,
                artifact_count: 3,
            },
            estimated_bytes: 2048,
            classpath_entries: Some(4),
            build_duration_ms: Some(1_250),
        },
        ProjectIndexStatus {
            project_path: "/work/lib".to_owned(),
            last_used_unix_ms: unix_ms(),
            state: IndexState::NotIndexed,
            index: IndexStats::default(),
            estimated_bytes: 0,
            classpath_entries: None,
            build_duration_ms: None,
        },
    ];
    let report = RuntimeReport {
        schema_version: RUNTIME_SCHEMA_VERSION,
        instances: vec![instance],
        unknown_instances: 2,
    };

    let text = format_runtime_report(&report);

    assert!(
        text.contains("maven-mcp pid=77 version=0.0.0 commit=unknown age_s="),
        "{text}"
    );
    assert!(
        text.contains(
            "  project: /work/app state=ready classpath=4 jars=3 sources=1 classes=40 artifacts=3 estimated_bytes=2048 build_ms=1250 idle_s=0\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("  project: /work/lib state=not_indexed classpath=unknown jars=0"),
        "{text}"
    );
    assert!(
        text.ends_with("Kept 2 status file(s) whose process could not be proven live or gone.\n"),
        "{text}"
    );
}

#[test]
fn human_report_of_no_live_instances_is_one_clear_line() {
    let report = RuntimeReport {
        schema_version: RUNTIME_SCHEMA_VERSION,
        instances: Vec::new(),
        unknown_instances: 0,
    };

    assert_eq!(
        format_runtime_report(&report),
        "No live maven-mcp instances.\n"
    );
}
