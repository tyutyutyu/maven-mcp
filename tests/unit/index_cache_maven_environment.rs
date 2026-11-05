use std::{fs, os::unix::fs::PermissionsExt};

use tempfile::TempDir;

use super::*;

fn variables(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    pairs
        .iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect()
}

fn names(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn only_maven_relevant_and_referenced_variables_take_part() {
    let referenced = names(&["BUILD_FLAVOR"]);

    for name in [
        "HOME",
        "JAVA_HOME",
        "JAVA_TOOL_OPTIONS",
        "JDK_JAVA_OPTIONS",
        "MAVEN_OPTS",
        "MAVEN_ARGS",
        "MAVEN_USER_HOME",
        "MAVEN_CONFIG",
        "MAVEN_PROJECTBASEDIR",
        "M2_HOME",
        "MAVEN_HOME",
        "MVNW_REPOURL",
        "PATH",
        "JENV_ROOT",
        "JENV_VERSION",
        "HTTP_PROXY",
        "https_proxy",
        "NO_PROXY",
        "no_proxy",
        "BUILD_FLAVOR",
    ] {
        assert!(takes_part(name, Some(&referenced)), "{name}");
    }
    for name in [
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_PID",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "VSCODE_IPC_HOOK",
        "PWD",
        "SSH_AUTH_SOCK",
        "OTHER_FLAVOR",
        "MAVEN_MCP_RUNTIME_DIR",
        "MAX_RESULTS",
        "RUST_LOG",
    ] {
        assert!(!takes_part(name, Some(&referenced)), "{name}");
    }
}

#[test]
fn unknown_references_select_every_variable_but_server_only_ones() {
    assert!(takes_part("PWD", None));
    assert!(takes_part("CLAUDE_CODE_SESSION_ID", None));
    assert!(!takes_part("MAVEN_MCP_CACHE_DIR", None));

    let base = environment_digest(variables(&[("PWD", "/a")]), None);
    assert_ne!(base, environment_digest(variables(&[("PWD", "/b")]), None));
}

#[test]
fn the_digest_is_order_independent_and_covers_names_and_values() {
    let referenced = names(&[]);
    let digest = |pairs: &[(&str, &str)]| environment_digest(variables(pairs), Some(&referenced));
    let base = digest(&[("HOME", "/h"), ("MAVEN_OPTS", "-Xmx1g")]);

    assert_eq!(base, digest(&[("MAVEN_OPTS", "-Xmx1g"), ("HOME", "/h")]));
    assert_eq!(
        base,
        digest(&[("HOME", "/h"), ("MAVEN_OPTS", "-Xmx1g"), ("PWD", "/x")])
    );
    assert_ne!(base, digest(&[("HOME", "/h"), ("MAVEN_OPTS", "-Xmx2g")]));
    assert_ne!(
        base,
        digest(&[("HOME", "/h"), ("MAVEN_OPTS", "-Xmx1g"), ("MAVEN_ARGS", "")]),
        "an added empty variable"
    );
    assert_ne!(
        digest(&[("JAVA_HOME", "a"), ("PATH", "")]),
        digest(&[("JAVA_HOME", ""), ("PATH", "a")]),
        "values stay with their names"
    );
}

#[test]
fn env_references_are_collected_from_interpolated_text() {
    let text = b"<a>${env.BUILD_FLAVOR}</a><b>${env.}</b><c>${project.version}</c>\
                 <d>${env.CI_TOKEN}-${env.BUILD_FLAVOR}</d><e>${env.UNCLOSED\
                 <activation><property><name>!env.PROFILE_SWITCH</name></property>\
                 <f>setenv.sh ${project.env.NOT_A_VARIABLE} see env.SENTENCE.</f>";

    assert_eq!(
        env_references(text),
        Some(names(&[
            "BUILD_FLAVOR",
            "CI_TOKEN",
            "PROFILE_SWITCH",
            "SENTENCE",
            "UNCLOSED"
        ]))
    );
    assert_eq!(env_references(b"env.FIRST"), Some(names(&["FIRST"])));
    assert_eq!(env_references(b""), Some(BTreeSet::new()));
}

#[test]
fn env_references_read_xml_character_and_entity_references_like_maven() {
    assert_eq!(
        env_references(
            b"<a>${env.BUILD&#95;FLAVOR}</a><b>${env&#46;FLAG}</b><c>&#x24;{env.HEX}</c>"
        ),
        Some(names(&["BUILD", "BUILD_FLAVOR", "FLAG", "HEX"]))
    );
    assert_eq!(
        env_references(b"<a>a &amp; b &lt; ${env.PLAIN} & c;</a><b>&#xZZ; &#99999999;</b>"),
        Some(names(&["PLAIN"])),
        "predefined entities, bare ampersands and invalid character references"
    );
    assert_eq!(
        env_references(b"<!DOCTYPE p [<!ENTITY flavor \"env.HIDDEN\">]><p>${&flavor;}</p>"),
        None,
        "a DTD entity hides its value"
    );
}

#[test]
fn referenced_digests_cover_only_the_referenced_variables() {
    let referenced = names(&["DEP_VERSION", "MAX_RESULTS"]);
    let digest = |pairs: &[(&str, &str)]| referenced_digest(variables(pairs), &referenced);
    let base = digest(&[("DEP_VERSION", "1"), ("PWD", "/a")]);

    assert_eq!(
        base,
        digest(&[
            ("DEP_VERSION", "1"),
            ("MAVEN_OPTS", "-X"),
            ("MAX_RESULTS", "9")
        ])
    );
    assert_ne!(base, digest(&[("DEP_VERSION", "2")]));
    assert_ne!(base, digest(&[]));
    assert_eq!(digest(&[]), referenced_digest(Vec::new(), &BTreeSet::new()));
}

#[test]
fn shell_references_are_collected_from_sourced_files() {
    let text = b"MAVEN_OPTS=\"$MAVEN_OPTS -Dx=${CUSTOM_FLAG:-1}\"\nexport A=$1 B=$$ C=${_UNDER}$";

    assert_eq!(
        shell_references(text),
        names(&["CUSTOM_FLAG", "MAVEN_OPTS", "_UNDER"])
    );
}

#[test]
fn reference_files_cover_project_user_and_installation_configuration() {
    let root = Path::new("/projects/app");
    let files = ReferenceFiles::of_project(
        root,
        &[root.join("pom.xml"), root.join("core/pom.xml")],
        &[
            PathBuf::from("/opt/maven/conf"),
            PathBuf::from("/opt/maven/lib/ext"),
        ],
        Some(Path::new("/home/dev")),
    );

    for path in [
        "/projects/app/pom.xml",
        "/projects/app/core/pom.xml",
        "/projects/app/.mvn/maven.config",
        "/projects/app/.mvn/jvm.config",
        "/home/dev/.m2/settings.xml",
        "/home/dev/.m2/toolchains.xml",
        "/opt/maven/conf/settings.xml",
        "/opt/maven/conf/toolchains.xml",
    ] {
        assert!(files.interpolated.contains(&PathBuf::from(path)), "{path}");
    }
    assert!(
        !files
            .interpolated
            .iter()
            .any(|path| path.starts_with("/opt/maven/lib")),
        "extension directories are not configuration"
    );
    for path in [
        "/etc/mavenrc",
        "/home/dev/.mavenrc",
        "/projects/app/.mvn/jvm.config",
    ] {
        assert!(files.shell.contains(&PathBuf::from(path)), "{path}");
    }
}

#[test]
fn referenced_names_are_read_from_existing_files() {
    let directory = TempDir::new().unwrap();
    let root = directory.path();
    fs::create_dir_all(root.join(".mvn")).unwrap();
    fs::write(
        root.join("pom.xml"),
        "<project>${env.BUILD_FLAVOR}<name>env.PROFILE_SWITCH</name></project>",
    )
    .unwrap();
    fs::write(root.join(".mvn/maven.config"), "-Dtoken=${env.CI_TOKEN}").unwrap();
    fs::write(root.join(".mvn/jvm.config"), "-Xmx${HEAP_SIZE}").unwrap();
    let files = ReferenceFiles {
        interpolated: vec![
            root.join("pom.xml"),
            root.join(".mvn/maven.config"),
            root.join("missing.xml"),
        ],
        shell: vec![root.join(".mvn/jvm.config")],
    };

    assert_eq!(
        files.referenced_names(),
        Some(names(&[
            "BUILD_FLAVOR",
            "CI_TOKEN",
            "HEAP_SIZE",
            "PROFILE_SWITCH"
        ]))
    );
}

#[test]
fn an_unknown_xml_entity_leaves_the_references_unknown() {
    let directory = TempDir::new().unwrap();
    let pom = directory.path().join("pom.xml");
    fs::write(&pom, "<project>${env.&custom;}</project>").unwrap();
    let files = ReferenceFiles {
        interpolated: vec![pom],
        shell: Vec::new(),
    };

    assert_eq!(files.referenced_names(), None);
}

#[test]
fn an_unreadable_file_leaves_the_references_unknown() {
    let directory = TempDir::new().unwrap();
    let unreadable = directory.path().join("settings.xml");
    fs::create_dir(&unreadable).unwrap();
    let files = ReferenceFiles {
        interpolated: vec![unreadable],
        shell: Vec::new(),
    };
    assert_eq!(files.referenced_names(), None, "a directory cannot be read");

    let locked = directory.path().join("pom.xml");
    fs::write(&locked, "<project/>").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let readable_anyway = File::open(&locked).is_ok();
    let files = ReferenceFiles {
        interpolated: Vec::new(),
        shell: vec![locked],
    };
    assert_eq!(
        files.referenced_names().is_none(),
        !readable_anyway,
        "an unreadable file, unless running with elevated rights"
    );
}
