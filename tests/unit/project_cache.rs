use std::{
    os::unix::fs::PermissionsExt,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use std::fs;

use tempfile::TempDir;
use tokio::sync::Semaphore;

use super::*;
use crate::config::{JavaEnvironment, MavenExecutionConfig};

fn limits(max_indexes: usize, max_runners: usize, max_bytes: u64) -> CacheLimits {
    CacheLimits {
        max_indexes,
        max_runners,
        max_bytes,
    }
}

fn execution_config() -> MavenExecutionConfig {
    MavenExecutionConfig {
        trusted_project_directories: Vec::new(),
        maven_executable: None,
        execution_repository: None,
        timeout: Duration::from_secs(1),
        max_output_bytes: 1024,
        max_results: 10,
        network_enabled: false,
        java_environment: JavaEnvironment::Inherit,
    }
}

fn project(artifact_id: &str) -> (TempDir, PathBuf, Arc<MavenRunner>) {
    let directory = TempDir::new().unwrap();
    let root = directory.path().canonicalize().unwrap();
    fs::write(
        root.join("pom.xml"),
        format!("<project><artifactId>{artifact_id}</artifactId></project>"),
    )
    .unwrap();
    fs::create_dir_all(root.join(".mvn/wrapper")).unwrap();
    fs::write(root.join(".mvn/wrapper/maven-wrapper.properties"), "").unwrap();
    fs::write(root.join("mvnw"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(root.join("mvnw"), fs::Permissions::from_mode(0o755)).unwrap();
    let runner = MavenRunner::discover_canonical_with_permit(
        root.clone(),
        &execution_config(),
        Arc::new(Semaphore::new(1)),
    )
    .unwrap();
    (directory, root, Arc::new(runner))
}

fn install(cache: &mut ProjectCache, root: &Path, runner: &Arc<MavenRunner>) -> RunnerSnapshot {
    let fingerprint = ProjectFingerprint::capture(runner.fingerprint_paths());
    cache.install_runner(root, Arc::clone(runner), fingerprint)
}

fn empty_index() -> (TempDir, Arc<MavenIndex>) {
    let directory = TempDir::new().unwrap();
    let index = MavenIndex::build(directory.path(), 10, 1024).unwrap();
    (directory, Arc::new(index))
}

fn build(
    cache: &mut ProjectCache,
    root: &Path,
    snapshot: &RunnerSnapshot,
    index: &Arc<MavenIndex>,
) -> Publication {
    let IndexLookup::Build { generation, .. } = cache.lookup_index(root, &snapshot.fingerprint)
    else {
        panic!("expected a cold lookup");
    };
    let reservation = cache.reserve(root, generation, 0).unwrap();
    cache.publish(
        root,
        generation,
        index,
        ProjectFingerprint::default(),
        Arc::clone(&snapshot.fingerprint),
        reservation,
    )
}

#[test]
fn fingerprint_detects_edits_creation_and_removal() {
    let directory = TempDir::new().unwrap();
    let file = directory.path().join("pom.xml");
    let missing = directory.path().join("mvnw");
    fs::write(&file, "<project/>").unwrap();
    let fingerprint = ProjectFingerprint::capture([file.clone(), missing.clone()]);
    assert!(fingerprint.is_current());
    assert_eq!(fingerprint.file_bytes(), 10);

    fs::write(&file, "<project><x/></project>").unwrap();
    assert!(!fingerprint.is_current());
    let edited = ProjectFingerprint::capture([file.clone(), missing.clone()]);
    assert_ne!(fingerprint, edited);

    fs::write(&missing, "#!/bin/sh").unwrap();
    assert!(!edited.is_current());
    let created = ProjectFingerprint::capture([file.clone(), missing.clone()]);
    fs::remove_file(&file).unwrap();
    assert!(!created.is_current());
}

#[test]
fn cold_lookups_of_one_generation_share_a_single_build() {
    let (_directory, root, runner) = project("single-flight");
    let mut cache = ProjectCache::new(limits(4, 8, u64::MAX));
    let snapshot = install(&mut cache, &root, &runner);
    let (_index_root, index) = empty_index();

    let lookups = (0..8)
        .map(|_| cache.lookup_index(&root, &snapshot.fingerprint))
        .map(|lookup| match lookup {
            IndexLookup::Build { cell, generation } => (cell, generation),
            IndexLookup::Cached { .. } => panic!("nothing is cached yet"),
        })
        .collect::<Vec<_>>();
    assert!(lookups.iter().all(|(cell, generation)| {
        Arc::ptr_eq(cell, &lookups[0].0) && *generation == lookups[0].1
    }));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_time()
        .build()
        .unwrap();
    let builds = Arc::new(AtomicUsize::new(0));
    let outcomes = runtime.block_on(async {
        let tasks = lookups.iter().map(|(cell, _)| {
            let (cell, builds, index) = (Arc::clone(cell), Arc::clone(&builds), index.clone());
            tokio::spawn(async move {
                cell.get_or_init(|| async {
                    builds.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    BuildOutcome::Built(index)
                })
                .await
                .clone()
            })
        });
        futures_collect(tasks).await
    });
    assert_eq!(builds.load(Ordering::SeqCst), 1);
    assert!(outcomes.iter().all(|outcome| matches!(
        outcome,
        BuildOutcome::Built(shared) if Arc::ptr_eq(shared, &index)
    )));
}

async fn futures_collect<T>(tasks: impl Iterator<Item = tokio::task::JoinHandle<T>>) -> Vec<T> {
    let mut results = Vec::new();
    for task in tasks.collect::<Vec<_>>() {
        results.push(task.await.unwrap());
    }
    results
}

#[test]
fn invalidation_during_a_build_prevents_publication_and_starts_a_new_generation() {
    let (_directory, root, runner) = project("stale");
    let mut cache = ProjectCache::new(limits(4, 8, u64::MAX));
    let snapshot = install(&mut cache, &root, &runner);
    let (_index_root, index) = empty_index();
    let IndexLookup::Build {
        cell: old_cell,
        generation: old_generation,
    } = cache.lookup_index(&root, &snapshot.fingerprint)
    else {
        panic!("expected a cold lookup");
    };
    let reservation = cache.reserve(&root, old_generation, 0).unwrap();

    cache.invalidate_after_maven(&root);

    assert_eq!(
        cache.publish(
            &root,
            old_generation,
            &index,
            ProjectFingerprint::default(),
            Arc::clone(&snapshot.fingerprint),
            reservation,
        ),
        Publication::Stale
    );
    assert!(cache.reserve(&root, old_generation, 0).is_err());
    let IndexLookup::Build { cell, generation } = cache.lookup_index(&root, &snapshot.fingerprint)
    else {
        panic!("a stale build must not leave a cached index");
    };
    assert_ne!(generation, old_generation);
    assert!(!Arc::ptr_eq(&cell, &old_cell));
    assert!(cache.runner(&root).unwrap().revalidate);
}

#[test]
fn failed_builds_clear_the_in_flight_marker_so_a_retry_can_run() {
    let (_directory, root, runner) = project("retry");
    let mut cache = ProjectCache::new(limits(4, 8, u64::MAX));
    let snapshot = install(&mut cache, &root, &runner);
    let IndexLookup::Build {
        cell: failed,
        generation,
    } = cache.lookup_index(&root, &snapshot.fingerprint)
    else {
        panic!("expected a cold lookup");
    };
    cache.finish_failed(&root, generation);

    let IndexLookup::Build { cell: retry, .. } = cache.lookup_index(&root, &snapshot.fingerprint)
    else {
        panic!("expected a retry build");
    };
    assert!(!Arc::ptr_eq(&failed, &retry));
    // A failure report from an older generation must not clear a newer build.
    cache.finish_failed(&root, generation + 1_000);
    let IndexLookup::Build { cell: joined, .. } = cache.lookup_index(&root, &snapshot.fingerprint)
    else {
        panic!("expected to join the running build");
    };
    assert!(Arc::ptr_eq(&retry, &joined));
}

#[test]
fn changed_project_fingerprint_invalidates_the_index_and_running_build() {
    let (_directory, root, runner) = project("pom-change");
    let mut cache = ProjectCache::new(limits(4, 8, u64::MAX));
    let snapshot = install(&mut cache, &root, &runner);
    let (_index_root, index) = empty_index();
    assert_eq!(
        build(&mut cache, &root, &snapshot, &index),
        Publication::Retained
    );
    assert!(matches!(
        cache.lookup_index(&root, &snapshot.fingerprint),
        IndexLookup::Cached { .. }
    ));

    fs::write(
        root.join("pom.xml"),
        "<project><artifactId>pom-change</artifactId><version>2</version></project>",
    )
    .unwrap();
    assert!(!snapshot.fingerprint.is_current());
    let refreshed = runner
        .rediscover(&execution_config(), Arc::new(Semaphore::new(1)))
        .unwrap();
    let refreshed = Arc::new(refreshed);
    let new_snapshot = install(&mut cache, &root, &refreshed);

    assert!(new_snapshot.fingerprint.is_current());
    assert!(matches!(
        cache.lookup_index(&root, &new_snapshot.fingerprint),
        IndexLookup::Build { .. }
    ));
    assert_eq!(cache.runtime_cache().invalidations, 1);
}

#[test]
fn count_limit_evicts_least_recently_used_but_keeps_in_use_memory_accounted() {
    let mut cache = ProjectCache::new(limits(2, 8, u64::MAX));
    let projects = (0..3)
        .map(|number| project(&format!("lru-{number}")))
        .collect::<Vec<_>>();
    let mut held = Vec::new();
    let mut snapshots = Vec::new();
    for (_, root, runner) in &projects {
        let snapshot = install(&mut cache, root, runner);
        let (index_root, index) = empty_index();
        assert_eq!(
            build(&mut cache, root, &snapshot, &index),
            Publication::Retained
        );
        held.push((index_root, index));
        snapshots.push(snapshot);
    }

    let status = cache.runtime_cache();
    assert_eq!((status.entries, status.evictions), (2, 1));
    assert!(matches!(
        cache.lookup_index(&projects[0].1, &snapshots[0].fingerprint),
        IndexLookup::Build { .. }
    ));
    assert!(matches!(
        cache.lookup_index(&projects[2].1, &snapshots[2].fingerprint),
        IndexLookup::Cached { .. }
    ));

    // The evicted index is still referenced by `held`, so its bytes stay in the budget.
    let evicted_bytes = held[0].1.estimated_bytes();
    let while_in_use = cache.retained_bytes();
    let evicted = held.remove(0);
    drop(evicted);
    assert_eq!(cache.retained_bytes(), while_in_use - evicted_bytes);
}

#[test]
fn byte_budget_evicts_idle_indexes_and_rejects_or_degrades_oversized_builds() {
    let (_index_root, probe) = empty_index();
    let index_bytes = probe.estimated_bytes();
    let runner_bytes = {
        let mut cache = ProjectCache::new(limits(4, 8, u64::MAX));
        let (_directory, root, runner) = project("probe");
        install(&mut cache, &root, &runner);
        cache.retained_bytes()
    };
    // Room for two runners and one index, but not two indexes.
    let mut cache = ProjectCache::new(limits(4, 8, 2 * runner_bytes + index_bytes + 16));
    let first = project("budget-first");
    let second = project("budget-second");
    let first_snapshot = install(&mut cache, &first.1, &first.2);
    let second_snapshot = install(&mut cache, &second.1, &second.2);
    let (_a, first_index) = empty_index();
    let (_b, second_index) = empty_index();
    assert_eq!(
        build(&mut cache, &first.1, &first_snapshot, &first_index),
        Publication::Retained
    );
    drop(first_index);
    assert_eq!(
        build(&mut cache, &second.1, &second_snapshot, &second_index),
        Publication::Retained
    );
    assert_eq!(cache.runtime_cache().entries, 1);
    assert_eq!(cache.runtime_cache().evictions, 1);
    assert!(cache.retained_bytes() <= cache.runtime_cache().max_bytes);

    // A build whose reservation cannot fit even after evicting everything is rejected.
    let IndexLookup::Build { generation, .. } =
        cache.lookup_index(&first.1, &first_snapshot.fingerprint)
    else {
        panic!("the first index was evicted");
    };
    assert!(matches!(
        cache.reserve(&first.1, generation, u64::MAX / 2),
        Err(ReserveError::OverBudget { .. })
    ));

    // An index larger than the whole budget is served to its requester but never retained.
    let mut tiny = ProjectCache::new(limits(4, 8, runner_bytes + 1));
    let (_c, big) = empty_index();
    let snapshot = {
        let fingerprint = ProjectFingerprint::capture(first.2.fingerprint_paths());
        tiny.install_runner(&first.1, Arc::clone(&first.2), fingerprint)
    };
    let IndexLookup::Build { generation, .. } = tiny.lookup_index(&first.1, &snapshot.fingerprint)
    else {
        panic!("expected a cold lookup");
    };
    let reservation = tiny.reserve(&first.1, generation, 0).unwrap();
    assert_eq!(
        tiny.publish(
            &first.1,
            generation,
            &big,
            ProjectFingerprint::default(),
            Arc::clone(&snapshot.fingerprint),
            reservation,
        ),
        Publication::Uncached
    );
    assert_eq!(tiny.runtime_cache().entries, 0);
    assert!(matches!(
        tiny.lookup_index(&first.1, &snapshot.fingerprint),
        IndexLookup::Build { .. }
    ));
}

#[test]
fn live_reservations_count_against_the_budget_until_their_build_ends() {
    let (_directory, root, runner) = project("reservation");
    let mut cache = ProjectCache::new(limits(4, 8, u64::MAX));
    let snapshot = install(&mut cache, &root, &runner);
    let base = cache.retained_bytes();
    let IndexLookup::Build { generation, .. } = cache.lookup_index(&root, &snapshot.fingerprint)
    else {
        panic!("expected a cold lookup");
    };
    let reservation = cache.reserve(&root, generation, 4_096).unwrap();
    assert_eq!(cache.retained_bytes(), base + 4_096);

    // An abandoned build keeps occupying memory until it actually stops.
    cache.invalidate_after_maven(&root);
    assert_eq!(cache.retained_bytes(), base + 4_096);
    drop(reservation);
    assert_eq!(cache.retained_bytes(), base);
}

#[test]
fn project_switching_keeps_runners_and_retained_bytes_bounded() {
    let mut cache = ProjectCache::new(limits(1, 3, u64::MAX));
    let mut peak = 0;
    let mut plateau = 0;
    for round in 0..12 {
        let (_directory, root, runner) = project(&format!("switch-{round}"));
        let snapshot = install(&mut cache, &root, &runner);
        let (_index_root, index) = empty_index();
        build(&mut cache, &root, &snapshot, &index);
        drop(index);
        let status = cache.runtime_cache();
        assert!(status.runners <= 3, "{} runners", status.runners);
        assert!(status.entries <= 1);
        peak = peak.max(cache.retained_bytes());
        if round == 4 {
            plateau = peak;
        }
    }
    assert!(plateau > 0);
    assert_eq!(
        peak, plateau,
        "retained bytes kept growing after the limits were reached"
    );
    assert!(cache.runtime_cache().evictions >= 10);
}

#[test]
fn discarding_a_project_forgets_its_runner_and_index() {
    let (_directory, root, runner) = project("discard");
    let mut cache = ProjectCache::new(limits(4, 8, u64::MAX));
    let snapshot = install(&mut cache, &root, &runner);
    let (_index_root, index) = empty_index();
    build(&mut cache, &root, &snapshot, &index);

    cache.discard_project(&root);

    assert!(cache.runner(&root).is_none());
    let status = cache.runtime_cache();
    assert_eq!((status.entries, status.runners), (0, 0));
}

#[test]
fn tree_fingerprint_detects_nested_class_file_edits_and_additions() {
    let directory = TempDir::new().unwrap();
    let classes = directory.path().join("target/classes");
    let nested = classes.join("org/example/Nested.class");
    fs::create_dir_all(nested.parent().unwrap()).unwrap();
    fs::write(&nested, "v1").unwrap();
    let jar = directory.path().join("lib.jar");
    fs::write(&jar, "jar").unwrap();

    let fingerprint = ProjectFingerprint::capture_trees([classes.clone(), jar.clone()]);
    assert!(fingerprint.paths().any(|path| path == nested));
    assert!(fingerprint.is_current());

    fs::write(&nested, "version 2").unwrap();
    assert!(!fingerprint.is_current());
    let edited = ProjectFingerprint::capture_trees([classes.clone(), jar.clone()]);
    // Directory timestamps advance at the file system's clock granularity.
    std::thread::sleep(Duration::from_millis(50));
    fs::write(classes.join("org/example/Added.class"), "new").unwrap();
    assert!(!edited.is_current());
}

#[test]
fn cancelled_builds_do_not_pin_runners_or_project_states() {
    let mut cache = ProjectCache::new(limits(1, 2, u64::MAX));
    let mut directories = Vec::new();
    for number in 0..6 {
        let (directory, root, runner) = project(&format!("cancel-{number}"));
        let snapshot = install(&mut cache, &root, &runner);
        let IndexLookup::Build { cell, .. } = cache.lookup_index(&root, &snapshot.fingerprint)
        else {
            panic!("expected a cold lookup");
        };
        // A cancelled request drops its cell without running or finishing the build.
        drop(cell);
        directories.push(directory);
        let status = cache.runtime_cache();
        assert!(status.runners <= 2, "{} runners", status.runners);
        assert!(cache.states.len() <= 2, "{} states", cache.states.len());
    }

    // A build that a request still awaits keeps its runner and lets later callers join it.
    let (_live_directory, live_root, live_runner) = project("cancel-live");
    let live_snapshot = install(&mut cache, &live_root, &live_runner);
    let IndexLookup::Build {
        cell: waiting,
        generation,
    } = cache.lookup_index(&live_root, &live_snapshot.fingerprint)
    else {
        panic!("expected a cold lookup");
    };
    for number in 0..4 {
        let (directory, root, runner) = project(&format!("cancel-after-{number}"));
        install(&mut cache, &root, &runner);
        directories.push(directory);
    }
    assert!(cache.runner(&live_root).is_some());
    let IndexLookup::Build {
        cell: joined,
        generation: joined_generation,
    } = cache.lookup_index(&live_root, &live_snapshot.fingerprint)
    else {
        panic!("expected to join the running build");
    };
    assert!(Arc::ptr_eq(&waiting, &joined));
    assert_eq!(generation, joined_generation);
}

#[test]
fn evicted_and_replaced_runners_stay_accounted_while_requests_hold_them() {
    let mut cache = ProjectCache::new(limits(4, 1, u64::MAX));
    let (_first_directory, first_root, first_runner) = project("held-first");
    let first_snapshot = install(&mut cache, &first_root, &first_runner);
    let one_runner = cache.retained_bytes();
    let (_second_directory, second_root, second_runner) = project("held-second");
    drop(install(&mut cache, &second_root, &second_runner));
    assert_eq!(cache.runtime_cache().runners, 1);

    // The evicted runner is still held by a request (`first_runner`, `first_snapshot`).
    let while_held = cache.retained_bytes();
    assert!(while_held >= 2 * one_runner - PATH_ENTRY_OVERHEAD_BYTES * 16);
    drop(first_snapshot);
    drop(first_runner);
    let released = cache.retained_bytes();
    assert!(released < while_held);
    assert!(released <= one_runner + PATH_ENTRY_OVERHEAD_BYTES * 16);

    // Replacing a runner after rediscovery keeps the old one counted until it is dropped.
    let replacement = Arc::new(
        second_runner
            .rediscover(&execution_config(), Arc::new(Semaphore::new(1)))
            .unwrap(),
    );
    drop(install(&mut cache, &second_root, &replacement));
    assert!(cache.retained_bytes() > released);
    drop(second_runner);
    assert_eq!(cache.retained_bytes(), released);
    assert!(cache.released_runners.len() <= 1);
}

#[test]
fn repeated_failed_and_oversized_builds_do_not_accumulate_bookkeeping() {
    let (_directory, root, runner) = project("bookkeeping");
    let mut cache = ProjectCache::new(limits(4, 8, u64::MAX));
    let snapshot = install(&mut cache, &root, &runner);
    for _ in 0..50 {
        let IndexLookup::Build { generation, .. } =
            cache.lookup_index(&root, &snapshot.fingerprint)
        else {
            panic!("a failed build is never cached");
        };
        let reservation = cache.reserve(&root, generation, 64).unwrap();
        drop(reservation);
        cache.finish_failed(&root, generation);
        assert!(cache.reservations.len() <= 1);
    }

    let runner_bytes = cache.retained_bytes();
    let mut tiny = ProjectCache::new(limits(4, 8, runner_bytes + 1));
    let snapshot = install(&mut tiny, &root, &runner);
    for _ in 0..50 {
        let (_index_root, oversized) = empty_index();
        let IndexLookup::Build { generation, .. } = tiny.lookup_index(&root, &snapshot.fingerprint)
        else {
            panic!("an oversized index is never cached");
        };
        let reservation = tiny.reserve(&root, generation, 0).unwrap();
        assert_eq!(
            tiny.publish(
                &root,
                generation,
                &oversized,
                ProjectFingerprint::default(),
                Arc::clone(&snapshot.fingerprint),
                reservation,
            ),
            Publication::Uncached
        );
        assert!(tiny.released.len() <= 1);
        assert!(tiny.reservations.len() <= 1);
    }
}
