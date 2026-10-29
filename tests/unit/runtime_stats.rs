use std::process::Command;

use super::*;

#[test]
fn reports_the_running_process_as_alive() {
    assert!(process_is_alive(Path::new(PROC_ROOT), std::process::id()));
}

#[test]
fn reports_a_reaped_child_process_as_not_alive() {
    let mut child = Command::new("true")
        .spawn()
        .expect("the true command must start");
    let pid = child.id();
    child.wait().expect("the child must be reaped");

    assert!(!process_is_alive(Path::new(PROC_ROOT), pid));
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
fn reads_current_process_memory_from_procfs() {
    let memory = process_memory();

    assert!(memory.rss_bytes.is_some_and(|bytes| bytes > 0));
    assert!(memory.peak_rss_bytes >= memory.rss_bytes);
}
