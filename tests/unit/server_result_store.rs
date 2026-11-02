use super::*;

const PROCESS_KEY: u64 = 0x0123_4567_89ab_cdef;

fn project(name: &str) -> PathBuf {
    PathBuf::from(format!("/trusted/{name}"))
}

fn store(max_bytes: usize) -> ResultStore {
    ResultStore::with_process_key(max_bytes, PROCESS_KEY)
}

/// The retained size of a result stored for `project("a")` with a sequence below 16.
fn retained(json: &str) -> usize {
    json.len() + "r0123456789abcdef0".len() + project("a").as_os_str().len()
}

#[test]
fn stored_results_are_read_back_unchanged_only_by_their_project() {
    let now = Instant::now();
    let mut results = store(1024);
    let json = r#"{"results":["ő","😀"]}"#.to_owned();

    let id = results.insert(&project("a"), json.clone(), now).unwrap();

    assert_eq!(
        results.get(&id, &project("a"), now).as_deref(),
        Some(json.as_str())
    );
    assert_eq!(results.get(&id, &project("b"), now), None);
    assert_eq!(results.get("r0123456789abcdefff", &project("a"), now), None);
    assert_eq!(results.get("", &project("a"), now), None);
}

#[test]
fn identifiers_are_unique_and_carry_the_process_key() {
    let now = Instant::now();
    let mut results = store(usize::MAX);
    let ids = (0..40)
        .map(|_| results.insert(&project("a"), "{}".to_owned(), now).unwrap())
        .collect::<std::collections::BTreeSet<_>>();

    assert_eq!(ids.len(), 40);
    assert!(ids.iter().all(|id| id.starts_with("r0123456789abcdef")));
    assert!(ids.iter().all(|id| id.len() <= longest_result_id().len()));
    let other_process = ResultStore::with_process_key(usize::MAX, 7)
        .insert(&project("a"), "{}".to_owned(), now)
        .unwrap();
    assert!(!ids.contains(&other_process));
}

#[test]
fn results_expire_exactly_after_the_ttl_without_waiting() {
    let stored_at = Instant::now();
    let mut results = store(1024);
    let id = results
        .insert(&project("a"), "{}".to_owned(), stored_at)
        .unwrap();

    let just_before = stored_at + STORED_RESULT_TTL - Duration::from_millis(1);
    assert!(results.get(&id, &project("a"), just_before).is_some());
    assert!(
        results
            .get(&id, &project("a"), stored_at + STORED_RESULT_TTL)
            .is_none()
    );
    assert_eq!(results.len(), 0);
    assert_eq!(results.retained_bytes(), 0);
}

#[test]
fn the_seventeenth_result_evicts_the_oldest_across_projects() {
    let now = Instant::now();
    let mut results = store(usize::MAX);
    let ids = (0..MAX_STORED_RESULTS)
        .map(|number| {
            let owner = if number % 2 == 0 { "a" } else { "b" };
            results
                .insert(&project(owner), "{}".to_owned(), now)
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), MAX_STORED_RESULTS);

    let newest = results.insert(&project("b"), "{}".to_owned(), now).unwrap();

    assert_eq!(results.len(), MAX_STORED_RESULTS);
    assert!(
        results.get(&ids[0], &project("a"), now).is_none(),
        "project a's oldest result is evicted by project b"
    );
    assert!(results.get(&ids[1], &project("b"), now).is_some());
    assert!(results.get(&newest, &project("b"), now).is_some());
}

#[test]
fn the_byte_budget_counts_json_identifier_and_project_and_evicts_oldest_first() {
    let now = Instant::now();
    let json = "x".repeat(100);
    let one = retained(&json);
    let mut results = store(2 * one + 10);

    let first = results.insert(&project("a"), json.clone(), now).unwrap();
    let second = results.insert(&project("a"), json.clone(), now).unwrap();
    assert_eq!(results.retained_bytes(), 2 * one);

    let third = results.insert(&project("a"), json.clone(), now).unwrap();

    assert!(results.get(&first, &project("a"), now).is_none());
    assert!(results.get(&second, &project("a"), now).is_some());
    assert!(results.get(&third, &project("a"), now).is_some());
    assert_eq!(results.retained_bytes(), 2 * one);
}

#[test]
fn a_result_larger_than_the_budget_is_rejected_without_evicting_anything() {
    let now = Instant::now();
    let small = "s".repeat(10);
    let limit = retained(&small) + 50;
    let mut results = store(limit);
    let kept = results.insert(&project("a"), small, now).unwrap();
    let oversized = "x".repeat(limit);

    let error = results
        .insert(&project("a"), oversized.clone(), now)
        .unwrap_err();

    assert_eq!(error.limit, limit);
    assert!(error.retained_bytes > oversized.len());
    assert!(results.get(&kept, &project("a"), now).is_some());
    assert_eq!(results.len(), 1);
}

#[test]
fn expired_results_free_their_bytes_before_a_new_result_is_stored() {
    let stored_at = Instant::now();
    let json = "x".repeat(100);
    let mut results = store(retained(&json) + 10);
    results
        .insert(&project("a"), json.clone(), stored_at)
        .unwrap();

    let later = stored_at + STORED_RESULT_TTL;
    let id = results.insert(&project("b"), json, later).unwrap();

    assert_eq!(results.len(), 1);
    assert!(results.get(&id, &project("b"), later).is_some());
}

#[test]
fn diagnostics_show_counts_but_never_results_or_projects() {
    let mut results = store(1024);
    results
        .insert(
            &project("secret-project"),
            r#"{"secret":1}"#.to_owned(),
            Instant::now(),
        )
        .unwrap();

    let debug = format!("{results:?}");

    assert!(debug.contains("stored_results: 1"), "{debug}");
    assert!(!debug.contains("secret"), "{debug}");
}
