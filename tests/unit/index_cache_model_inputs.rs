use std::fs;

use tempfile::TempDir;

use super::*;

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn repository_pom(repository: &Path, group: &str, artifact: &str, version: &str) -> PathBuf {
    repository
        .join(group.replace('.', "/"))
        .join(artifact)
        .join(version)
        .join(format!("{artifact}-{version}.pom"))
}

fn parent(group: &str, artifact: &str, version: &str, relative: &str) -> String {
    format!(
        "<parent><groupId>{group}</groupId><artifactId>{artifact}</artifactId>\
         <version>{version}</version>{relative}</parent>"
    )
}

fn import(group: &str, artifact: &str, version: &str) -> String {
    format!(
        "<dependency><groupId>{group}</groupId><artifactId>{artifact}</artifactId>\
         <version>{version}</version><type>pom</type><scope>import</scope></dependency>"
    )
}

fn project(body: &str) -> String {
    format!("<project><modelVersion>4.0.0</modelVersion><artifactId>a</artifactId>{body}</project>")
}

fn managed(imports: &str) -> String {
    format!("<dependencyManagement><dependencies>{imports}</dependencies></dependencyManagement>")
}

#[test]
fn parents_and_imported_boms_are_followed_transitively() {
    let directory = TempDir::new().unwrap();
    let (root, repository) = (directory.path().join("app"), directory.path().join("repo"));
    let reactor = root.join("pom.xml");
    write(
        &reactor,
        &project(&format!(
            "{}{}<profiles><profile>{}</profile></profiles>",
            parent("org.p", "parent", "1", "<relativePath/>"),
            managed(&import("${project.groupId}", "bom", "2")),
            managed(&import("org.q", "profile-bom", "3")),
        )),
    );
    let parent_pom = repository_pom(&repository, "org.p", "parent", "1");
    write(
        &parent_pom,
        &project(&parent("org.g", "grandparent", "9", "")),
    );
    let bom = repository_pom(&repository, "org.p", "bom", "2");
    write(&bom, &project(&managed(&import("org.n", "nested", "4"))));
    let dependency = repository_pom(&repository, "org.d", "dep", "1");
    write(
        &dependency,
        &project(&parent("org.d", "dep-parent", "5", "")),
    );

    let inputs = model_inputs(&[reactor], &[dependency], Some(&repository)).unwrap();

    for expected in [
        parent_pom,
        bom,
        repository_pom(&repository, "org.g", "grandparent", "9"),
        repository_pom(&repository, "org.n", "nested", "4"),
        repository_pom(&repository, "org.q", "profile-bom", "3"),
        repository_pom(&repository, "org.d", "dep-parent", "5"),
    ] {
        assert!(inputs.contains(&expected), "{expected:?} in {inputs:?}");
    }
    assert!(
        !inputs.iter().any(|path| path.starts_with(&root)),
        "an empty relativePath disables the local parent: {inputs:?}"
    );
}

#[test]
fn a_reactor_parent_is_looked_up_beside_the_pom_and_in_the_repository() {
    let directory = TempDir::new().unwrap();
    let (root, repository) = (directory.path().join("app"), directory.path().join("repo"));
    let module = root.join("module/pom.xml");
    write(&module, &project(&parent("org.p", "parent", "1", "")));
    let local_parent = root.join("pom.xml");
    write(
        &local_parent,
        &project(&parent(
            "org.p",
            "outer",
            "1",
            "<relativePath>../outer</relativePath>",
        )),
    );
    let outer = directory.path().join("outer/pom.xml");
    write(&outer, &project(""));

    let inputs = model_inputs(&[module], &[], Some(&repository)).unwrap();

    assert!(inputs.contains(&local_parent.canonicalize().unwrap()));
    assert!(inputs.contains(&outer.canonicalize().unwrap()));
    assert!(inputs.contains(&repository_pom(&repository, "org.p", "parent", "1")));
    assert!(inputs.contains(&repository_pom(&repository, "org.p", "outer", "1")));
}

#[test]
fn a_version_maven_decides_stamps_every_version_of_the_artifact() {
    let directory = TempDir::new().unwrap();
    let repository = directory.path().join("repo");
    let reactor = directory.path().join("app/pom.xml");
    write(
        &reactor,
        &project(&managed(&format!(
            "{}{}",
            import("org.b", "bom", "${bom.version}"),
            import("org.r", "ranged", "[1,2)"),
        ))),
    );
    write(
        &repository_pom(&repository, "org.b", "bom", "1"),
        &project(""),
    );
    write(
        &repository_pom(&repository, "org.b", "bom", "2"),
        &project(&parent("org.b", "bom-parent", "7", "")),
    );

    let inputs = model_inputs(&[reactor], &[], Some(&repository)).unwrap();

    assert!(inputs.contains(&repository.join("org/b/bom")), "{inputs:?}");
    assert!(
        inputs.contains(&repository.join("org/r/ranged")),
        "{inputs:?}"
    );
    assert!(inputs.contains(&repository_pom(&repository, "org.b", "bom-parent", "7")));
}

#[test]
fn unidentifiable_references_decline_persistence() {
    let directory = TempDir::new().unwrap();
    let repository = directory.path().join("repo");
    let reactor = directory.path().join("app/pom.xml");
    let declined = |body: &str, repository: Option<&Path>| {
        write(&reactor, body);
        model_inputs(std::slice::from_ref(&reactor), &[], repository).is_none()
    };

    assert!(declined(
        &project(&managed(&import("${custom.group}", "bom", "1"))),
        Some(&repository)
    ));
    assert!(declined(
        &project(&managed(&import("org..escape", "bom", "1"))),
        Some(&repository)
    ));
    assert!(declined(
        &project(&managed(&import("org.b", "../bom", "1"))),
        Some(&repository)
    ));
    assert!(declined(
        &project(&parent("org.p", "parent", "1", "<relativePath/>")),
        None
    ));
    assert!(declined("<project><parent>", Some(&repository)));
    assert!(!declined(&project(""), None), "nothing to identify");
}
