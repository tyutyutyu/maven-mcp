use std::{collections::BTreeSet, ffi::OsString, fs::File, time::SystemTime};

use tempfile::TempDir;

use super::*;

fn record(path: &str, bytes: usize) -> PersistableRecord {
    PersistableRecord {
        path: PathBuf::from(path),
        payload: encode_payload(&"x".repeat(bytes)).unwrap(),
    }
}

fn cache(directory: &TempDir, max_bytes: u64) -> PersistentIndexCache {
    PersistentIndexCache::new(directory.path().join("index"), max_bytes)
}

fn write_records(cache: &PersistentIndexCache, records: &[PersistableRecord]) {
    cache
        .write(&CacheUpdate {
            records,
            ..CacheUpdate::default()
        })
        .unwrap();
}

fn stored(cache: &PersistentIndexCache, path: &str) -> Option<Vec<u8>> {
    PersistedRecords::load(&cache.reader().unwrap(), Path::new(path))
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn payloads_round_trip_and_damage_is_detected() {
    let payload = encode_payload(&vec!["alpha".to_owned(); 100]).unwrap();
    assert_eq!(
        decode_payload::<Vec<String>>(&payload).unwrap(),
        vec!["alpha".to_owned(); 100]
    );

    let mut flipped = payload.clone();
    let last = flipped.len() - 1;
    flipped[last] ^= 0xff;
    assert!(decode_payload::<Vec<String>>(&flipped).is_err(), "checksum");
    assert!(decode_payload::<Vec<String>>(&payload[..payload.len() / 2]).is_err());
    assert!(decode_payload::<Vec<u32>>(&payload).is_err(), "shape");
}

#[test]
fn records_and_classpaths_are_stored_privately_and_read_back() {
    let directory = TempDir::new().unwrap();
    let cache = cache(&directory, 1 << 20);
    assert!(
        !directory.path().join("index").exists(),
        "construction does no I/O"
    );
    let records = [record("/repo/a.jar", 10)];

    cache
        .write(&CacheUpdate {
            classpath: Some(("context", b"manifest".to_vec())),
            records: &records,
            reused: &[],
        })
        .unwrap();

    assert_eq!(
        stored(&cache, "/repo/a.jar"),
        Some(records[0].payload.clone())
    );
    assert_eq!(stored(&cache, "/repo/b.jar"), None);
    assert_eq!(cache.load_classpath("context"), Some(b"manifest".to_vec()));
    assert_eq!(cache.load_classpath("other"), None);
    assert_eq!(mode(&directory.path().join("index")), 0o700);
    assert_eq!(mode(&cache.database_path()), 0o600);

    cache
        .forget_project_classpaths(Path::new("/projects/a"))
        .unwrap();
    assert_eq!(
        cache.load_classpath("context"),
        Some(b"manifest".to_vec()),
        "a key outside the project scope stays"
    );
    assert!(stored(&cache, "/repo/a.jar").is_some(), "records stay");
}

#[test]
fn forgetting_a_project_drops_every_context_of_that_root_only() {
    let directory = TempDir::new().unwrap();
    let cache = cache(&directory, 1 << 20);
    let config = Config::from_env().unwrap();
    let first = context_key(Path::new("/projects/a"), &config, 1);
    let second = context_key(Path::new("/projects/a"), &config, 2);
    let sibling = context_key(Path::new("/projects/ab"), &config, 1);
    for key in [&first, &second, &sibling] {
        cache
            .write(&CacheUpdate {
                classpath: Some((key, b"manifest".to_vec())),
                ..CacheUpdate::default()
            })
            .unwrap();
    }

    cache
        .forget_project_classpaths(Path::new("/projects/a"))
        .unwrap();

    assert_eq!(cache.load_classpath(&first), None);
    assert_eq!(cache.load_classpath(&second), None, "every environment");
    assert_eq!(
        cache.load_classpath(&sibling),
        Some(b"manifest".to_vec()),
        "a root sharing the prefix stays"
    );
}

#[test]
fn entries_of_another_tool_version_are_invisible() {
    let directory = TempDir::new().unwrap();
    let newer = PersistentIndexCache {
        tool: "99.0.0+newer".to_owned(),
        ..cache(&directory, 1 << 20)
    };
    write_records(&newer, &[record("/repo/a.jar", 10)]);

    let current = cache(&directory, 1 << 20);
    assert_eq!(stored(&current, "/repo/a.jar"), None);
    write_records(&current, &[record("/repo/a.jar", 20)]);

    assert_ne!(
        stored(&current, "/repo/a.jar"),
        stored(&newer, "/repo/a.jar")
    );
    assert!(stored(&newer, "/repo/a.jar").is_some(), "both coexist");
}

#[test]
fn a_garbage_database_is_rebuilt_instead_of_failing() {
    let directory = TempDir::new().unwrap();
    let cache = cache(&directory, 1 << 20);
    fs::create_dir_all(cache.database_path().parent().unwrap()).unwrap();
    fs::write(cache.database_path(), vec![0x5a; 8192]).unwrap();

    assert!(cache.reader().is_some());
    assert_eq!(stored(&cache, "/repo/a.jar"), None);
    write_records(&cache, &[record("/repo/a.jar", 10)]);
    assert!(stored(&cache, "/repo/a.jar").is_some());
}

#[test]
fn a_truncated_database_never_returns_damaged_payloads_and_recovers() {
    let directory = TempDir::new().unwrap();
    let cache = cache(&directory, 1 << 30);
    let records = (0..200)
        .map(|number| record(&format!("/repo/{number}.jar"), 4096))
        .collect::<Vec<_>>();
    write_records(&cache, &records);
    let length = fs::metadata(cache.database_path()).unwrap().len();
    File::options()
        .write(true)
        .open(cache.database_path())
        .unwrap()
        .set_len(length / 2)
        .unwrap();

    for record in &records {
        if let Some(payload) = cache
            .reader()
            .and_then(|reader| PersistedRecords::load(&reader, &record.path))
        {
            assert_eq!(payload, record.payload, "only intact payloads are returned");
        }
    }
    write_records(&cache, &records[..1]);
    assert_eq!(
        stored(&cache, "/repo/0.jar"),
        Some(records[0].payload.clone())
    );
}

#[test]
fn an_incompatible_format_marker_resets_the_cache() {
    let directory = TempDir::new().unwrap();
    let cache = cache(&directory, 1 << 20);
    write_records(&cache, &[record("/repo/a.jar", 10)]);
    Connection::open(cache.database_path())
        .unwrap()
        .execute("UPDATE meta SET value = '999' WHERE name = 'format'", [])
        .unwrap();

    assert_eq!(stored(&cache, "/repo/a.jar"), None);
    let format: String = Connection::open(cache.database_path())
        .unwrap()
        .query_row("SELECT value FROM meta WHERE name = 'format'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(format, INDEX_CACHE_FORMAT.to_string());
}

#[test]
fn the_byte_budget_evicts_least_recently_used_entries() {
    let directory = TempDir::new().unwrap();
    let probe = record("/repo/0.jar", 1000);
    let entry_size = entry_bytes("/repo/0.jar", &probe.payload) as u64;
    let cache = cache(&directory, entry_size * 3);
    for number in 0..3 {
        write_records(&cache, &[record(&format!("/repo/{number}.jar"), 1000)]);
        std::thread::sleep(Duration::from_millis(2));
    }
    // Reusing the oldest entry makes it the most recently used one.
    cache
        .write(&CacheUpdate {
            reused: &[PathBuf::from("/repo/0.jar")],
            ..CacheUpdate::default()
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(2));

    write_records(&cache, &[record("/repo/3.jar", 1000)]);

    assert!(stored(&cache, "/repo/0.jar").is_some());
    assert!(
        stored(&cache, "/repo/1.jar").is_none(),
        "least recently used"
    );
    assert!(stored(&cache, "/repo/2.jar").is_some());
    assert!(stored(&cache, "/repo/3.jar").is_some());
    let connection = Connection::open(cache.database_path()).unwrap();
    assert!(total_bytes(&connection).unwrap() as u64 <= entry_size * 3);
}

#[test]
fn a_recovery_another_process_already_made_is_never_repeated() {
    let directory = TempDir::new().unwrap();
    let (first, second) = (cache(&directory, 1 << 20), cache(&directory, 1 << 20));
    write_records(&first, &[record("/repo/old.jar", 10)]);
    // Both processes observe the same epoch before they diagnose the same damage.
    let observed = second.recovery_epoch().unwrap();
    fs::write(first.database_path(), vec![0x5a; 8192]).unwrap();

    write_records(&first, &[record("/repo/a.jar", 10)]);
    drop(second.recover(observed).unwrap());

    assert!(
        stored(&first, "/repo/a.jar").is_some(),
        "the late recovery kept the rebuilt database"
    );
    assert_eq!(first.recovery_epoch().unwrap(), observed + 1);
    assert_eq!(mode(&first.lock_path()), 0o600);
}

#[test]
fn concurrent_recoveries_of_one_damaged_database_rebuild_it_once() {
    const WRITERS: usize = 8;
    let directory = TempDir::new().unwrap();
    write_records(&cache(&directory, 1 << 30), &[record("/repo/old.jar", 10)]);
    fs::write(cache(&directory, 1 << 30).database_path(), vec![0x5a; 8192]).unwrap();
    let start = std::sync::Barrier::new(WRITERS);

    std::thread::scope(|scope| {
        for writer in 0..WRITERS {
            let (directory, start) = (&directory, &start);
            scope.spawn(move || {
                let cache = cache(directory, 1 << 30);
                start.wait();
                write_records(&cache, &[record(&format!("/repo/{writer}.jar"), 4096)]);
            });
        }
    });

    let cache = cache(&directory, 1 << 30);
    for writer in 0..WRITERS {
        assert!(
            stored(&cache, &format!("/repo/{writer}.jar")).is_some(),
            "no recovery deleted the database another writer rebuilt"
        );
    }
    assert_eq!(cache.recovery_epoch().unwrap(), 1, "rebuilt exactly once");
    let integrity: String = Connection::open(cache.database_path())
        .unwrap()
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
}

fn settled_file(path: &Path) {
    fs::write(path, "x").unwrap();
    File::open(path)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
}

/// Resolution inputs of only `paths`, without any model POM variable reference.
fn stamps_only(paths: &[PathBuf]) -> ResolutionInputs {
    ResolutionInputs::new(
        ProjectFingerprint::capture(paths.to_vec()),
        BTreeSet::new(),
        std::iter::empty(),
    )
}

#[test]
fn a_classpath_manifest_is_reusable_only_while_everything_it_depends_on_is_unchanged() {
    let directory = TempDir::new().unwrap();
    let (pom, settings, jar) = (
        directory.path().join("pom.xml"),
        directory.path().join("settings.xml"),
        directory.path().join("a.jar"),
    );
    for path in [&pom, &settings, &jar] {
        settled_file(path);
    }
    let project = ProjectFingerprint::capture([pom.clone()]);
    let manifest = || {
        ClasspathManifest::settled(
            "context",
            &project,
            stamps_only(std::slice::from_ref(&settings)),
            vec![jar.clone()],
        )
        .unwrap()
    };

    assert_eq!(
        manifest().reusable_classpath("context", &project),
        Some((
            vec![jar.clone()],
            stamps_only(std::slice::from_ref(&settings))
        ))
    );
    assert_eq!(manifest().reusable_classpath("other", &project), None);
    let edited_pom = ProjectFingerprint::capture([directory.path().join("other.xml")]);
    assert_eq!(manifest().reusable_classpath("context", &edited_pom), None);

    let stale_settings = manifest();
    fs::write(&settings, "changed").unwrap();
    assert_eq!(stale_settings.reusable_classpath("context", &project), None);
    settled_file(&settings);

    let missing_entry = manifest();
    fs::remove_file(&jar).unwrap();
    assert_eq!(missing_entry.reusable_classpath("context", &project), None);

    fs::write(&settings, "recent").unwrap();
    assert!(
        ClasspathManifest::settled(
            "context",
            &project,
            stamps_only(std::slice::from_ref(&settings)),
            Vec::new(),
        )
        .is_none(),
        "a recently modified input is not persisted"
    );
}

#[test]
fn classpath_inputs_name_settings_and_sibling_poms_of_repository_artifacts() {
    let root = Path::new("/projects/app");
    let repository = Path::new("/repo");
    let config = Config::from_env().unwrap();
    let classpath = [
        PathBuf::from("/repo/org/t/alpha/1.0/alpha-1.0.jar"),
        PathBuf::from("/repo/org/t/alpha/1.0/alpha-1.0-tests.jar"),
        PathBuf::from("/projects/app/target/classes"),
    ];

    let inputs = classpath_inputs(root, &config, &classpath, Some(repository));

    assert!(inputs.contains(&PathBuf::from("/repo/org/t/alpha/1.0/alpha-1.0.pom")));
    assert_eq!(
        inputs
            .iter()
            .filter(|path| path.starts_with(repository))
            .count(),
        1,
        "{inputs:?}"
    );
    assert!(inputs.contains(&PathBuf::from("/etc/mavenrc")));
    if let Some(home) = std::env::var_os("HOME") {
        assert!(inputs.contains(&PathBuf::from(&home).join(".m2/settings.xml")));
        assert!(inputs.contains(&PathBuf::from(&home).join(".m2/extensions.xml")));
        assert!(inputs.contains(&PathBuf::from(home).join(".mavenrc")));
    }

    let mut jenv = config;
    jenv.execution.java_environment = JavaEnvironment::Jenv {
        root: PathBuf::from("/jenv"),
    };
    let inputs = classpath_inputs(root, &jenv, &[], None);
    assert!(inputs.contains(&PathBuf::from("/jenv/version")));
    assert!(inputs.contains(&PathBuf::from("/projects/app/.java-version")));
    assert!(inputs.contains(&PathBuf::from("/projects/.java-version")));
}

#[test]
fn the_classpath_context_separates_projects_and_maven_selection() {
    let config = Config::from_env().unwrap();
    let context = context_key(Path::new("/projects/a"), &config, 1);

    assert_eq!(context, context_key(Path::new("/projects/a"), &config, 1));
    assert!(context.starts_with(&project_scope(Path::new("/projects/a"))));
    assert_ne!(context, context_key(Path::new("/projects/b"), &config, 1));
    assert_ne!(context, context_key(Path::new("/projects/a"), &config, 2));
    let mut other_repository = config.clone();
    other_repository.execution.execution_repository = Some(PathBuf::from("/other-repo"));
    assert_ne!(
        context,
        context_key(Path::new("/projects/a"), &other_repository, 1)
    );
    let mut jenv = config.clone();
    jenv.execution.java_environment = JavaEnvironment::Jenv {
        root: PathBuf::from("/jenv"),
    };
    assert_ne!(context, context_key(Path::new("/projects/a"), &jenv, 1));
    let mut network = config;
    network.execution.network_enabled = !network.execution.network_enabled;
    assert_ne!(context, context_key(Path::new("/projects/a"), &network, 1));
}

fn variables(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    pairs
        .iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect()
}

/// The context key of `/projects/a` for `pairs` and a POM referencing `${env.BUILD_FLAVOR}`.
fn key_for(pairs: &[(&str, &str)]) -> String {
    let config = Config::from_env().unwrap();
    let referenced = BTreeSet::from(["BUILD_FLAVOR".to_owned()]);
    context_key(
        Path::new("/projects/a"),
        &config,
        environment_digest(variables(pairs), Some(&referenced)),
    )
}

const MAVEN_ENVIRONMENT: [(&str, &str); 5] = [
    ("HOME", "/home/dev"),
    ("JAVA_HOME", "/opt/jdk-21"),
    ("MAVEN_OPTS", "-Xmx1g"),
    ("PATH", "/usr/bin"),
    ("BUILD_FLAVOR", "release"),
];

fn with(overrides: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
    MAVEN_ENVIRONMENT
        .iter()
        .filter(|(name, _)| !overrides.iter().any(|(changed, _)| changed == name))
        .chain(overrides)
        .copied()
        .collect()
}

#[test]
fn host_session_variables_keep_the_classpath_context_key() {
    let first = key_for(&with(&[
        ("CLAUDE_CODE_SESSION_ID", "session-1"),
        ("PWD", "/home/dev/a"),
        ("SSH_AUTH_SOCK", "/tmp/ssh-1/agent"),
        ("VSCODE_IPC_HOOK", "/run/user/1000/vscode-1.sock"),
        ("RUST_LOG", "maven_mcp=info"),
    ]));
    let second = key_for(&with(&[
        ("CLAUDE_CODE_SESSION_ID", "session-2"),
        ("PWD", "/home/dev/b"),
        ("SSH_AUTH_SOCK", "/tmp/ssh-2/agent"),
        ("VSCODE_IPC_HOOK", "/run/user/1000/vscode-2.sock"),
        ("RUST_LOG", "maven_mcp=debug"),
    ]));

    assert_eq!(first, second);
    assert_eq!(
        first,
        key_for(&MAVEN_ENVIRONMENT),
        "absent session variables"
    );
}

#[test]
fn maven_relevant_variables_change_the_classpath_context_key() {
    let base = key_for(&MAVEN_ENVIRONMENT);

    for changed in [
        ("MAVEN_OPTS", "-Xmx2g"),
        ("JAVA_HOME", "/opt/jdk-17"),
        ("PATH", "/opt/maven/bin:/usr/bin"),
        ("BUILD_FLAVOR", "debug"),
    ] {
        assert_ne!(base, key_for(&with(&[changed])), "{}", changed.0);
    }
    assert_ne!(
        base,
        key_for(&with(&[("HTTPS_PROXY", "http://proxy:3128")])),
        "an added relevant variable"
    );
}

#[test]
fn the_classpath_context_reads_env_references_of_the_project_poms() {
    let directory = TempDir::new().unwrap();
    let pom = directory.path().join("pom.xml");
    fs::write(&pom, "<project><name>${env.BUILD_FLAVOR}</name></project>").unwrap();
    let config = Config::from_env().unwrap();

    let context = classpath_context(&ContextSources {
        root: directory.path(),
        config: &config,
        reactor_poms: &[pom],
        installation: &[],
    });

    assert!(context.starts_with(&project_scope(directory.path())));
    assert!(context.contains("\"environment\":"));
}

#[test]
fn a_variable_only_a_parent_or_bom_outside_the_reactor_references_is_a_resolution_input() {
    let directory = TempDir::new().unwrap();
    let repository = directory.path().join("repo");
    let reactor = directory.path().join("app/pom.xml");
    let local_parent = directory.path().join("parent/pom.xml");
    let bom = repository.join("org/b/bom/2/bom-2.pom");
    for (path, content) in [
        (
            &reactor,
            "<project><parent><groupId>org.p</groupId><artifactId>parent</artifactId>\
             <version>1</version><relativePath>../parent/pom.xml</relativePath></parent>\
             <artifactId>app</artifactId><dependencyManagement><dependencies><dependency>\
             <groupId>org.b</groupId><artifactId>bom</artifactId><version>2</version>\
             <type>pom</type><scope>import</scope></dependency></dependencies>\
             </dependencyManagement></project>",
        ),
        (
            &local_parent,
            "<project><artifactId>parent</artifactId>\
             <properties><dep.version>${env.DEP_VERSION}</dep.version></properties></project>",
        ),
        (
            &bom,
            "<project><artifactId>bom</artifactId>\
             <properties><flavor>${env.BOM_FLAVOR}</flavor></properties></project>",
        ),
    ] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let model = model_inputs::model_inputs(&[reactor], &[], Some(&repository)).unwrap();
    let first_session = [
        ("DEP_VERSION", "1.0"),
        ("BOM_FLAVOR", "a"),
        ("CLAUDE_CODE_SESSION_ID", "session-1"),
        ("PWD", "/a"),
    ];

    let inputs = ResolutionInputs::new(
        ProjectFingerprint::capture(model.paths),
        model.env_references,
        variables(&first_session),
    );

    assert!(inputs.matches_environment(variables(&[
        ("BOM_FLAVOR", "a"),
        ("DEP_VERSION", "1.0"),
        ("CLAUDE_CODE_SESSION_ID", "session-2"),
        ("SSH_AUTH_SOCK", "/tmp/agent"),
    ])));
    assert!(
        !inputs.matches_environment(variables(&[("DEP_VERSION", "2.0"), ("BOM_FLAVOR", "a")])),
        "a variable of the local parent outside the reactor"
    );
    assert!(
        !inputs.matches_environment(variables(&[("DEP_VERSION", "1.0")])),
        "a variable of the imported BOM"
    );
    let decoded = decode_payload::<ResolutionInputs>(&encode_payload(&inputs).unwrap()).unwrap();
    assert_eq!(decoded, inputs, "the references survive the manifest");
}

#[test]
fn environment_digests_separate_adjacent_values() {
    let digest = |values: &[&str]| {
        values
            .iter()
            .fold(FNV_OFFSET, |hash, value| fnv1a(hash, value.as_bytes()))
    };

    assert_ne!(digest(&["ab", "c"]), digest(&["a", "bc"]));
    assert_eq!(digest(&["ab", "c"]), digest(&["ab", "c"]));
}
