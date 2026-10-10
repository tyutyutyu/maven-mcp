use super::create_fixture;
use tempfile::TempDir;

#[test]
fn creates_a_scoped_project_and_two_version_repository() -> anyhow::Result<()> {
    let root = TempDir::new()?;
    let fixture_root = root.path().join("fixture");
    let project = create_fixture(&fixture_root)?;

    assert_eq!(project, fixture_root.join("project").canonicalize()?);
    assert!(project.join("pom.xml").is_file());
    assert!(project.join("mvnw").is_file());
    assert!(
        fixture_root
            .join("maven-repository/org/example/demo/1.0/demo-1.0.jar")
            .is_file()
    );
    assert!(
        fixture_root
            .join("maven-repository/org/example/demo/2.0/demo-2.0-sources.jar")
            .is_file()
    );
    Ok(())
}

#[test]
fn refuses_to_replace_an_unmarked_directory() -> anyhow::Result<()> {
    let root = TempDir::new()?;
    let fixture_root = root.path().join("fixture");
    std::fs::create_dir_all(&fixture_root)?;
    std::fs::write(fixture_root.join("keep.txt"), "do not replace")?;

    let error = create_fixture(&fixture_root).expect_err("unmarked directory must be safe");
    assert!(error.to_string().contains("unmarked directory"));
    assert!(fixture_root.join("keep.txt").is_file());
    Ok(())
}
