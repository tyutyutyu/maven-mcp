use std::{fs::File, io::Write, path::Path};

use anyhow::{Context, Result, bail};
use zip::{ZipWriter, write::SimpleFileOptions};

const MARKER: &str = ".maven-mcp-eval-fixture";

fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .context("usage: maven-eval-fixture PATH")?;
    let root = Path::new(&path);
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
    write_version(root, "1.0", &["org/example/Foo", "org/example/Legacy"])?;
    write_version(root, "2.0", &["org/example/Foo", "org/example/Modern"])?;
    println!("{}", root.canonicalize()?.display());
    Ok(())
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
