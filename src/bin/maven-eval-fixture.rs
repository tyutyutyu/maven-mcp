use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use zip::{ZipWriter, write::SimpleFileOptions};

const MARKER: &str = ".maven-mcp-eval-fixture";

fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .context("usage: maven-eval-fixture FIXTURE_ROOT")?;
    let project = create_fixture(Path::new(&path))?;
    println!("{}", project.display());
    Ok(())
}

fn create_fixture(root: &Path) -> Result<PathBuf> {
    if root.exists() {
        if !root.join(MARKER).is_file() {
            bail!(
                "refusing to replace an unmarked directory: {}",
                root.display()
            );
        }
        std::fs::remove_dir_all(root)?;
    }
    std::fs::create_dir_all(root)?;
    std::fs::write(root.join(MARKER), "generated; safe to replace\n")?;
    let repository = root.join("maven-repository");
    write_version(
        &repository,
        "1.0",
        &["org/example/Foo", "org/example/Legacy"],
    )?;
    write_version(
        &repository,
        "2.0",
        &["org/example/Foo", "org/example/Modern"],
    )?;

    let project = root.join("project");
    write_project(&project)?;
    project
        .canonicalize()
        .context("generated fixture project must be canonicalizable")
}

fn write_version(root: &Path, version: &str, classes: &[&str]) -> Result<()> {
    let directory = root.join(format!("org/example/demo/{version}"));
    let class_entries = classes
        .iter()
        .map(|name| (format!("{name}.class"), minimal_class(name)))
        .collect::<Vec<_>>();
    write_jar(
        &directory.join(format!("demo-{version}.jar")),
        &class_entries,
    )?;
    let source_entries = classes
        .iter()
        .map(|name| {
            let simple = name.rsplit('/').next().unwrap_or(name);
            (
                format!("{name}.java"),
                format!("package org.example; public class {simple} {{}}").into_bytes(),
            )
        })
        .collect::<Vec<_>>();
    write_jar(
        &directory.join(format!("demo-{version}-sources.jar")),
        &source_entries,
    )?;
    std::fs::write(
        directory.join(format!("demo-{version}.pom")),
        format!(
            "<project><modelVersion>4.0.0</modelVersion><groupId>org.example</groupId><artifactId>demo</artifactId><version>{version}</version></project>"
        ),
    )?;
    Ok(())
}

fn write_project(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root)?;
    std::fs::create_dir_all(root.join(".mvn/wrapper"))?;
    std::fs::write(
        root.join(".mvn/wrapper/maven-wrapper.properties"),
        "distributionUrl=https://example.invalid/maven.zip\n",
    )?;
    std::fs::write(
        root.join("pom.xml"),
        r#"<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>org.example</groupId>
  <artifactId>maven-mcp-eval-fixture</artifactId>
  <version>1.0</version>
  <packaging>jar</packaging>
</project>
"#,
    )?;

    let wrapper = root.join("mvnw");
    std::fs::write(
        &wrapper,
        r#"#!/bin/sh
set -eu

case " $* " in
  *" dependency:build-classpath "*)
    repository=$(CDPATH= cd -- "$PWD/../maven-repository" && pwd)
    printf '%s\n%s\n' \
      'Dependencies classpath:' \
      "$repository/org/example/demo/1.0/demo-1.0.jar:$repository/org/example/demo/2.0/demo-2.0.jar"
    ;;
  *)
    printf '%s\n' '[INFO] BUILD SUCCESS'
    ;;
esac
"#,
    )?;
    make_executable(&wrapper)?;
    Ok(())
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = path.metadata()?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

fn write_jar(path: &Path, entries: &[(String, Vec<u8>)]) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("JAR path has no parent")?)?;
    let mut writer = ZipWriter::new(File::create(path)?);
    for (name, content) in entries {
        writer.start_file(name, SimpleFileOptions::default())?;
        writer.write_all(content)?;
    }
    writer.finish()?;
    Ok(())
}

fn minimal_class(class_name: &str) -> Vec<u8> {
    fn push_u16(bytes: &mut Vec<u8>, value: u16) {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    fn push_utf8(bytes: &mut Vec<u8>, value: &str) {
        bytes.push(1);
        push_u16(bytes, u16::try_from(value.len()).unwrap_or(u16::MAX));
        bytes.extend_from_slice(value.as_bytes());
    }
    fn push_class(bytes: &mut Vec<u8>, name_index: u16) {
        bytes.push(7);
        push_u16(bytes, name_index);
    }
    let mut bytes = vec![0xca, 0xfe, 0xba, 0xbe];
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 61);
    push_u16(&mut bytes, 5);
    push_utf8(&mut bytes, class_name);
    push_class(&mut bytes, 1);
    push_utf8(&mut bytes, "java/lang/Object");
    push_class(&mut bytes, 3);
    push_u16(&mut bytes, 0x0021);
    push_u16(&mut bytes, 2);
    push_u16(&mut bytes, 4);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    bytes
}

#[cfg(test)]
mod tests {
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
}
