use std::io::Write;

use tempfile::TempDir;
use zip::{ZipWriter, write::SimpleFileOptions};

use super::*;

fn write_jar(path: &Path, entries: &[(&str, &[u8])]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut writer = ZipWriter::new(File::create(path).unwrap());
    for (name, content) in entries {
        writer
            .start_file(*name, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(content).unwrap();
    }
    writer.finish().unwrap();
}

#[test]
fn artifact_pom_enforces_xml_byte_and_structure_limits() {
    let root = TempDir::new().unwrap();
    let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
    write_jar(&jar, &[("x", b"")]);
    let index = build_index(&root, 10, 1024);
    let pom = jar.with_extension("pom");
    for (xml, limit) in [
        (
            " ".repeat(crate::config::DEFAULT_MAX_XML_BYTES + 1),
            "MAX_XML_BYTES",
        ),
        (
            format!(
                "{}{}",
                "<a>".repeat(crate::config::MAX_XML_DEPTH + 1),
                "</a>".repeat(crate::config::MAX_XML_DEPTH + 1)
            ),
            "MAX_XML_DEPTH",
        ),
    ] {
        std::fs::write(&pom, xml).unwrap();
        assert!(
            index
                .pom_descriptor("org.example:demo:1.0")
                .unwrap_err()
                .to_string()
                .contains(limit)
        );
    }
}

#[test]
fn archive_preflight_enforces_entry_and_name_boundaries_including_directories() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("tiny.jar");
    let mut writer = ZipWriter::new(File::create(&path).unwrap());
    writer
        .add_directory("dir/", SimpleFileOptions::default())
        .unwrap();
    writer
        .start_file("x", SimpleFileOptions::default())
        .unwrap();
    writer.finish().unwrap();
    let budget = || ArchiveBudget {
        entries: 2,
        names: 5,
        jar_entries: 2,
    };
    let mut exact = budget();
    preflight_archive(&path, &mut exact).unwrap();
    assert_eq!((exact.entries, exact.names), (0, 0));
    for (mut limited, expected) in [
        (
            ArchiveBudget {
                jar_entries: 1,
                ..budget()
            },
            "MAX_JAR_ENTRIES",
        ),
        (
            ArchiveBudget {
                entries: 1,
                ..budget()
            },
            "MAX_INDEX_ENTRIES",
        ),
        (
            ArchiveBudget {
                names: 4,
                ..budget()
            },
            "MAX_INDEX_NAME_BYTES",
        ),
    ] {
        assert!(
            preflight_archive(&path, &mut limited)
                .unwrap_err()
                .to_string()
                .contains(expected)
        );
    }
    let mut total = ArchiveBudget {
        entries: 4,
        names: 10,
        jar_entries: 2,
    };
    preflight_archive(&path, &mut total).unwrap();
    preflight_archive(&path, &mut total).unwrap();
    assert!(
        preflight_archive(&path, &mut total)
            .unwrap_err()
            .to_string()
            .contains("MAX_INDEX_ENTRIES")
    );
    let mut total_names = ArchiveBudget {
        entries: 10,
        names: 9,
        jar_entries: 2,
    };
    preflight_archive(&path, &mut total_names).unwrap();
    assert!(
        preflight_archive(&path, &mut total_names)
            .unwrap_err()
            .to_string()
            .contains("MAX_INDEX_NAME_BYTES")
    );
    for length in [
        crate::config::MAX_ENTRY_NAME_BYTES,
        crate::config::MAX_ENTRY_NAME_BYTES + 1,
    ] {
        write_jar(&path, &[(&"a".repeat(length), b"")]);
        let result = preflight_archive(&path, &mut ArchiveBudget::configured());
        assert_eq!(
            result.is_ok(),
            length == crate::config::MAX_ENTRY_NAME_BYTES
        );
    }
}

#[test]
fn archive_preflight_supports_zip64_and_bounds_central_directory_bytes() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("zip64.jar");
    write_jar(&path, &[("a", b""), ("b", b"")]);
    let mut bytes = std::fs::read(&path).unwrap();
    let mut end = bytes.split_off(bytes.len() - 22);
    let size = u32::from_le_bytes(end[12..16].try_into().unwrap()) as u64;
    let offset = u32::from_le_bytes(end[16..20].try_into().unwrap()) as u64;
    let zip64_offset = bytes.len() as u64;
    let mut zip64 = vec![0; 56];
    zip64[..4].copy_from_slice(b"PK\x06\x06");
    zip64[4..12].copy_from_slice(&44u64.to_le_bytes());
    zip64[12..14].copy_from_slice(&45u16.to_le_bytes());
    zip64[14..16].copy_from_slice(&45u16.to_le_bytes());
    zip64[24..32].copy_from_slice(&2u64.to_le_bytes());
    zip64[32..40].copy_from_slice(&2u64.to_le_bytes());
    zip64[40..48].copy_from_slice(&size.to_le_bytes());
    zip64[48..56].copy_from_slice(&offset.to_le_bytes());
    bytes.extend(zip64);
    bytes.extend(b"PK\x06\x07");
    bytes.extend(0u32.to_le_bytes());
    bytes.extend(zip64_offset.to_le_bytes());
    bytes.extend(1u32.to_le_bytes());
    end[8..12].fill(255);
    end[12..20].fill(255);
    bytes.extend(end);
    std::fs::write(&path, &bytes).unwrap();
    preflight_archive(&path, &mut ArchiveBudget::configured()).unwrap();
    assert_eq!(read_jar_index(&path).unwrap().1, ["a", "b"]);
    let size_offset = zip64_offset as usize + 40;
    bytes[size_offset..size_offset + 8]
        .copy_from_slice(&(MAX_CENTRAL_DIRECTORY_BYTES + 1).to_le_bytes());
    std::fs::write(&path, bytes).unwrap();
    assert!(
        preflight_archive(&path, &mut ArchiveBudget::configured())
            .unwrap_err()
            .to_string()
            .contains("MAX_CENTRAL_DIRECTORY_BYTES")
    );
}

#[test]
fn archive_preflight_rejects_many_tiny_entries_and_forged_counts() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("tiny.jar");
    let mut writer = ZipWriter::new(File::create(&path).unwrap());
    for i in 0..10_000 {
        writer
            .start_file(format!("{i}/"), SimpleFileOptions::default())
            .unwrap();
    }
    writer.finish().unwrap();
    let budget = || ArchiveBudget {
        entries: 10_000,
        names: 100_000,
        jar_entries: 10_000,
    };
    preflight_archive(&path, &mut budget()).unwrap();
    assert!(
        preflight_archive(
            &path,
            &mut ArchiveBudget {
                jar_entries: 9999,
                ..budget()
            }
        )
        .unwrap_err()
        .is::<ArchiveLimit>()
    );
    let mut bytes = std::fs::read(&path).unwrap();
    let end = bytes.len() - 22;
    bytes[end + 8..end + 12].fill(0);
    std::fs::write(&path, bytes).unwrap();
    assert!(
        preflight_archive(
            &path,
            &mut ArchiveBudget {
                entries: 9999,
                ..budget()
            }
        )
        .unwrap_err()
        .to_string()
        .contains("MAX_INDEX_ENTRIES")
    );
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn push_utf8(bytes: &mut Vec<u8>, value: &str) {
    bytes.push(1);
    push_u16(bytes, u16::try_from(value.len()).unwrap());
    bytes.extend_from_slice(value.as_bytes());
}

fn push_class(bytes: &mut Vec<u8>, name_index: u16) {
    bytes.push(7);
    push_u16(bytes, name_index);
}

fn inspection_class(class_name: &str) -> Vec<u8> {
    let mut bytes = vec![0xca, 0xfe, 0xba, 0xbe];
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 61);
    push_u16(&mut bytes, 20);
    push_utf8(&mut bytes, class_name); // #1
    push_class(&mut bytes, 1); // #2
    push_utf8(&mut bytes, "java/lang/Object"); // #3
    push_class(&mut bytes, 3); // #4
    push_utf8(&mut bytes, "java/io/Serializable"); // #5
    push_class(&mut bytes, 5); // #6
    push_utf8(&mut bytes, "<init>"); // #7
    push_utf8(&mut bytes, "()V"); // #8
    push_utf8(&mut bytes, "greet"); // #9
    push_utf8(&mut bytes, "(Ljava/lang/String;)Ljava/lang/String;"); // #10
    push_utf8(&mut bytes, "value"); // #11
    push_utf8(&mut bytes, "Ljava/lang/String;"); // #12
    push_utf8(&mut bytes, "Signature"); // #13
    push_utf8(&mut bytes, "<T:Ljava/lang/Object;>Ljava/lang/Object;"); // #14
    push_utf8(&mut bytes, "(TT;)TT;"); // #15
    push_utf8(&mut bytes, "RuntimeVisibleAnnotations"); // #16
    push_utf8(&mut bytes, "Ljava/lang/Deprecated;"); // #17
    push_utf8(&mut bytes, "secret"); // #18
    push_utf8(&mut bytes, "(I)Ljava/lang/String;"); // #19

    push_u16(&mut bytes, 0x0421); // public, super, abstract
    push_u16(&mut bytes, 2);
    push_u16(&mut bytes, 4);
    push_u16(&mut bytes, 1);
    push_u16(&mut bytes, 6);

    push_u16(&mut bytes, 2); // fields
    push_u16(&mut bytes, 0x0019); // public static final
    push_u16(&mut bytes, 11);
    push_u16(&mut bytes, 12);
    push_u16(&mut bytes, 1);
    push_u16(&mut bytes, 16);
    push_u32(&mut bytes, 6);
    push_u16(&mut bytes, 1);
    push_u16(&mut bytes, 17);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0x0002); // private
    push_u16(&mut bytes, 18);
    push_u16(&mut bytes, 12);
    push_u16(&mut bytes, 0);

    push_u16(&mut bytes, 4); // methods
    push_u16(&mut bytes, 0x0001); // public constructor
    push_u16(&mut bytes, 7);
    push_u16(&mut bytes, 8);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0x0401); // public abstract
    push_u16(&mut bytes, 9);
    push_u16(&mut bytes, 10);
    push_u16(&mut bytes, 2);
    push_u16(&mut bytes, 13);
    push_u32(&mut bytes, 2);
    push_u16(&mut bytes, 15);
    push_u16(&mut bytes, 16);
    push_u32(&mut bytes, 6);
    push_u16(&mut bytes, 1);
    push_u16(&mut bytes, 17);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0x0401); // overloaded public abstract method
    push_u16(&mut bytes, 9);
    push_u16(&mut bytes, 19);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0x0002); // private method
    push_u16(&mut bytes, 18);
    push_u16(&mut bytes, 8);
    push_u16(&mut bytes, 0);

    push_u16(&mut bytes, 2); // class attributes
    push_u16(&mut bytes, 13);
    push_u32(&mut bytes, 2);
    push_u16(&mut bytes, 14);
    push_u16(&mut bytes, 16);
    push_u32(&mut bytes, 6);
    push_u16(&mut bytes, 1);
    push_u16(&mut bytes, 17);
    push_u16(&mut bytes, 0);
    bytes
}

fn hierarchy_class(class_name: &str, super_class: &str, interfaces: &[&str]) -> Vec<u8> {
    let mut bytes = vec![0xca, 0xfe, 0xba, 0xbe];
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 61);
    push_u16(&mut bytes, u16::try_from(5 + interfaces.len() * 2).unwrap());
    push_utf8(&mut bytes, class_name);
    push_class(&mut bytes, 1);
    push_utf8(&mut bytes, super_class);
    push_class(&mut bytes, 3);
    for (index, interface) in interfaces.iter().enumerate() {
        push_utf8(&mut bytes, interface);
        push_class(&mut bytes, u16::try_from(5 + index * 2).unwrap());
    }
    push_u16(&mut bytes, 0x0021);
    push_u16(&mut bytes, 2);
    push_u16(&mut bytes, 4);
    push_u16(&mut bytes, u16::try_from(interfaces.len()).unwrap());
    for index in 0..interfaces.len() {
        push_u16(&mut bytes, u16::try_from(6 + index * 2).unwrap());
    }
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    bytes
}

fn referencing_class(class_name: &str, target: &str) -> Vec<u8> {
    let mut bytes = vec![0xca, 0xfe, 0xba, 0xbe];
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 61);
    push_u16(&mut bytes, 11);
    push_utf8(&mut bytes, class_name); // #1
    push_class(&mut bytes, 1); // #2
    push_utf8(&mut bytes, "java/lang/Object"); // #3
    push_class(&mut bytes, 3); // #4
    push_utf8(&mut bytes, target); // #5
    push_class(&mut bytes, 5); // #6
    push_utf8(&mut bytes, "call"); // #7
    push_utf8(&mut bytes, "()V"); // #8
    bytes.push(12); // #9 NameAndType
    push_u16(&mut bytes, 7);
    push_u16(&mut bytes, 8);
    bytes.push(10); // #10 MethodRef
    push_u16(&mut bytes, 6);
    push_u16(&mut bytes, 9);
    push_u16(&mut bytes, 0x0021);
    push_u16(&mut bytes, 2);
    push_u16(&mut bytes, 4);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    bytes
}

fn module_info_class() -> Vec<u8> {
    let mut bytes = vec![0xca, 0xfe, 0xba, 0xbe];
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 61);
    push_u16(&mut bytes, 12);
    push_utf8(&mut bytes, "module-info"); // #1
    push_class(&mut bytes, 1); // #2
    push_utf8(&mut bytes, "Module"); // #3
    push_utf8(&mut bytes, "example.module"); // #4
    bytes.push(19); // #5 ModuleInfo
    push_u16(&mut bytes, 4);
    push_utf8(&mut bytes, "example/Service"); // #6
    push_class(&mut bytes, 6); // #7
    push_utf8(&mut bytes, "example/Provider"); // #8
    push_class(&mut bytes, 8); // #9
    push_utf8(&mut bytes, "example/Used"); // #10
    push_class(&mut bytes, 10); // #11
    push_u16(&mut bytes, 0x8000);
    push_u16(&mut bytes, 2);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 1);
    push_u16(&mut bytes, 3);
    push_u32(&mut bytes, 24);
    push_u16(&mut bytes, 5);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 1);
    push_u16(&mut bytes, 11);
    push_u16(&mut bytes, 1);
    push_u16(&mut bytes, 7);
    push_u16(&mut bytes, 1);
    push_u16(&mut bytes, 9);
    bytes
}

fn replace_ascii(bytes: &mut [u8], previous: &[u8], current: &[u8]) {
    assert_eq!(previous.len(), current.len());
    let offset = bytes
        .windows(previous.len())
        .position(|window| window == previous)
        .expect("test classfile should contain replacement text");
    bytes[offset..offset + current.len()].copy_from_slice(current);
}

fn fixture() -> (TempDir, MavenIndex) {
    let root = TempDir::new().unwrap();
    let base = root.path().join("org/example/demo/1.2.0");
    write_jar(
        &base.join("demo-1.2.0.jar"),
        &[
            ("org/example/Foo.class", b"bytecode"),
            ("config/app.conf", b"x"),
        ],
    );
    write_jar(
        &base.join("demo-1.2.0-sources.jar"),
        &[(
            "org/example/Foo.java",
            b"package org.example; public class Foo {}",
        )],
    );
    let index = MavenIndex::build(root.path(), 50, 1024).unwrap();
    (root, index)
}

fn build_index(root: &TempDir, max_results: usize, max_source_bytes: usize) -> MavenIndex {
    MavenIndex::build(root.path(), max_results, max_source_bytes).unwrap()
}

#[test]
fn indexes_classes_and_artifact_versions() {
    let (_root, index) = fixture();
    let matches = index.search_classes("Foo", None);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].jar.coordinate, "org.example:demo:1.2.0");
    assert_eq!(
        index.artifact_versions("demo", Some("org.example"))["org.example:demo"],
        vec!["1.2.0"]
    );
}

#[test]
fn reads_source_and_searches_entries() {
    let (_root, index) = fixture();
    let source = index.class_source("org.example.Foo", None, None).unwrap();
    assert!(source[0].source.contains("class Foo"));
    assert_eq!(index.search_entries("app.conf", None, None).len(), 1);
}

#[test]
fn reads_exact_text_and_binary_entries_without_extracting_them() {
    let root = TempDir::new().unwrap();
    let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
    write_jar(
        &jar,
        &[
            ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\n"),
            ("META-INF/services/example.Service", b"org.example.Foo\n"),
            ("native/image.bin", &[0, 159, 146, 150]),
        ],
    );
    let index = build_index(&root, 10, 1024);

    let manifest = index
        .jar_entry("org.example:demo:1.0", "META-INF/MANIFEST.MF")
        .unwrap();
    assert_eq!(manifest.len(), 1);
    assert_eq!(manifest[0].content_kind, JarEntryContentKind::Text);
    assert_eq!(manifest[0].text.as_deref(), Some("Manifest-Version: 1.0\n"));
    assert_eq!(manifest[0].bytes, None);
    assert_eq!(manifest[0].original_size, 22);
    assert!(!manifest[0].truncated);

    let binary = index.jar_entry("demo-1.0.jar", "native/image.bin").unwrap();
    assert_eq!(binary[0].content_kind, JarEntryContentKind::Binary);
    assert_eq!(
        binary[0].bytes.as_deref(),
        Some([0, 159, 146, 150].as_slice())
    );
    assert_eq!(binary[0].text, None);
    assert!(
        index
            .jar_entry("demo-1.0.jar", "missing.txt")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn exact_entry_reading_applies_the_byte_limit_and_preserves_utf8() {
    let root = TempDir::new().unwrap();
    let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
    write_jar(&jar, &[("utf8.txt", "árvíztűrő".as_bytes())]);
    let index = build_index(&root, 10, 3);

    let result = index.jar_entry("demo-1.0.jar", "utf8.txt").unwrap();
    assert_eq!(result[0].content_kind, JarEntryContentKind::Text);
    assert_eq!(result[0].text.as_deref(), Some("ár"));
    assert_eq!(result[0].original_size, 13);
    assert!(result[0].truncated);
}

#[test]
fn exact_entry_reading_surfaces_changes_that_corrupt_an_indexed_jar() {
    let root = TempDir::new().unwrap();
    let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
    write_jar(&jar, &[("config.txt", b"value")]);
    let index = build_index(&root, 10, 1024);
    std::fs::write(&jar, b"not a zip anymore").unwrap();

    let error = index
        .jar_entry("demo-1.0.jar", "config.txt")
        .expect_err("a real ZIP read error must not become an empty result");
    assert!(error.to_string().contains("no longer a valid ZIP"));
}

#[test]
fn parses_structured_pom_metadata_and_sorts_declared_content() {
    let root = TempDir::new().unwrap();
    let base = root.path().join("org/example/demo/1.0");
    write_jar(&base.join("demo-1.0.jar"), &[("Demo.class", b"bytecode")]);
    std::fs::write(
        base.join("demo-1.0.pom"),
        r#"<project>
            <modelVersion>4.0.0</modelVersion>
            <parent>
                <groupId>org.example.parent</groupId>
                <artifactId>parent</artifactId>
                <version>3.0</version>
            </parent>
            <groupId>org.example</groupId>
            <artifactId>demo</artifactId>
            <version>1.0</version>
            <packaging>maven-plugin</packaging>
            <properties>
                <library.version>2.4</library.version>
                <java.version>21</java.version>
            </properties>
            <dependencyManagement><dependencies>
                <dependency>
                    <groupId>org.platform</groupId><artifactId>bom</artifactId>
                    <version>5.0</version><type>pom</type><scope>import</scope>
                </dependency>
                <dependency>
                    <groupId>org.example</groupId><artifactId>managed</artifactId>
                    <version>${library.version}</version>
                </dependency>
            </dependencies></dependencyManagement>
            <dependencies>
                <dependency>
                    <groupId>org.example</groupId><artifactId>runtime</artifactId>
                    <version>${library.version}</version><scope>runtime</scope>
                    <optional>true</optional><classifier>linux</classifier><type>zip</type>
                    <exclusions>
                        <exclusion><groupId>z.group</groupId><artifactId>last</artifactId></exclusion>
                        <exclusion><groupId>a.group</groupId><artifactId>first</artifactId></exclusion>
                    </exclusions>
                </dependency>
            </dependencies>
        </project>"#,
    )
    .unwrap();
    let index = build_index(&root, 10, 1024);

    let result = index.pom_descriptor("org.example:demo:1.0").unwrap();
    assert!(result.found);
    let descriptor = result.descriptor.unwrap();
    assert_eq!(descriptor.packaging, "maven-plugin");
    assert_eq!(descriptor.parent.unwrap().artifact_id, "parent");
    assert_eq!(descriptor.properties["java.version"], "21");
    assert_eq!(
        descriptor.dependencies[0].version.as_deref(),
        Some("${library.version}")
    );
    assert!(descriptor.dependencies[0].optional);
    assert_eq!(
        descriptor.dependencies[0].classifier.as_deref(),
        Some("linux")
    );
    assert_eq!(descriptor.dependencies[0].r#type.as_deref(), Some("zip"));
    assert_eq!(descriptor.dependencies[0].exclusions[0].group_id, "a.group");
    assert_eq!(descriptor.dependency_management.len(), 1);
    assert_eq!(descriptor.bom_imports.len(), 1);
    assert_eq!(descriptor.bom_imports[0].artifact_id, "bom");
}

#[test]
fn pom_lookup_distinguishes_missing_invalid_and_malformed_descriptors() {
    let root = TempDir::new().unwrap();
    let base = root.path().join("org/example/demo/1.0");
    write_jar(&base.join("demo-1.0.jar"), &[("Demo.class", b"bytecode")]);
    let index = build_index(&root, 10, 1024);

    let missing = index.pom_descriptor("org.example:demo:1.0").unwrap();
    assert!(!missing.found);
    assert_eq!(missing.descriptor, None);
    assert!(index.pom_descriptor("../../outside:demo:1.0").is_err());

    std::fs::write(base.join("demo-1.0.pom"), "<project><broken></project>").unwrap();
    let error = index
        .pom_descriptor("org.example:demo:1.0")
        .expect_err("malformed POM XML must remain an error");
    assert!(error.to_string().contains("cannot parse POM"));
}

#[test]
fn describes_classfile_api_without_a_sources_jar() {
    let root = TempDir::new().unwrap();
    let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
    let class = inspection_class("org/example/Inspectable");
    write_jar(&jar, &[("org/example/Inspectable.class", &class)]);
    let index = build_index(&root, 10, 64 * 1024);

    let public = index
        .describe_class("org.example.Inspectable", None, None, true)
        .unwrap();
    assert_eq!(public.len(), 1);
    assert_eq!(public[0].class_name, "org.example.Inspectable");
    assert_eq!(public[0].visibility, "public");
    assert_eq!(public[0].super_class.as_deref(), Some("java.lang.Object"));
    assert_eq!(public[0].interfaces, vec!["java.io.Serializable"]);
    assert_eq!(
        public[0].generic_signature.as_deref(),
        Some("<T:Ljava/lang/Object;>Ljava/lang/Object;")
    );
    assert_eq!(public[0].annotations, vec!["java.lang.Deprecated"]);
    assert_eq!(public[0].constructors.len(), 1);
    assert_eq!(public[0].methods.len(), 2);
    let generic_method = public[0]
        .methods
        .iter()
        .find(|method| method.generic_signature.is_some())
        .unwrap();
    assert_eq!(generic_method.name, "greet");
    assert_eq!(
        generic_method.generic_signature.as_deref(),
        Some("(TT;)TT;")
    );
    assert_eq!(public[0].fields.len(), 1);
    assert_eq!(
        public[0].fields[0].annotations,
        vec!["java.lang.Deprecated"]
    );

    let all = index
        .describe_class(
            "org.example.Inspectable",
            Some("demo-1.0.jar"),
            Some("1.0"),
            false,
        )
        .unwrap();
    assert_eq!(all[0].methods.len(), 3);
    assert_eq!(all[0].fields.len(), 2);
    assert!(
        index
            .describe_class("org.example.Missing", None, None, true)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn describes_inner_and_multi_release_classes_once() {
    let root = TempDir::new().unwrap();
    let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
    let inner = inspection_class("org/example/Outer$Inner");
    let versioned = inspection_class("org/example/VersionOnly");
    write_jar(
        &jar,
        &[
            ("org/example/Outer$Inner.class", &inner),
            (
                "META-INF/versions/17/org/example/VersionOnly.class",
                &versioned,
            ),
        ],
    );
    let index = build_index(&root, 10, 64 * 1024);

    let inner_result = index
        .describe_class("org.example.Outer$Inner", None, None, true)
        .unwrap();
    assert_eq!(inner_result[0].class_name, "org.example.Outer$Inner");
    let versioned_result = index
        .describe_class("org.example.VersionOnly", None, None, true)
        .unwrap();
    assert_eq!(versioned_result.len(), 1);
}

#[test]
fn diagnoses_complete_snapshot_and_corrupt_artifact_states_without_paths() {
    let root = TempDir::new().unwrap();
    let base = root.path().join("org/example/demo/1.0-SNAPSHOT");
    write_jar(
        &base.join("demo-1.0-SNAPSHOT.jar"),
        &[("Demo.class", b"bytecode")],
    );
    write_jar(
        &base.join("demo-1.0-SNAPSHOT-sources.jar"),
        &[("Demo.java", b"class Demo {}")],
    );
    std::fs::write(base.join("demo-1.0-SNAPSHOT-javadoc.jar"), b"corrupt").unwrap();
    std::fs::write(base.join("demo-1.0-SNAPSHOT.pom"), "<project/>").unwrap();
    std::fs::write(base.join("demo-1.0-SNAPSHOT.jar.sha256"), "checksum").unwrap();
    std::fs::write(base.join("demo-1.0-SNAPSHOT.pom.lastUpdated"), "failure").unwrap();
    std::fs::write(
        base.join("_remote.repositories"),
        "demo-1.0-SNAPSHOT.jar>central=\ndemo-1.0-SNAPSHOT.pom>private-repo=\n",
    )
    .unwrap();
    let index = build_index(&root, 10, 64 * 1024);

    let health = index
        .artifact_health("org.example:demo:1.0-SNAPSHOT")
        .unwrap();
    assert!(health.found);
    assert!(health.snapshot);
    assert_eq!(health.files.len(), 4);
    assert_eq!(health.checksums, vec!["demo-1.0-SNAPSHOT.jar.sha256"]);
    assert_eq!(
        health.last_updated_markers,
        vec!["demo-1.0-SNAPSHOT.pom.lastUpdated"]
    );
    assert_eq!(health.repository_ids, vec!["central", "private-repo"]);
    let corrupt = health
        .files
        .iter()
        .find(|file| file.classifier.as_deref() == Some("javadoc"))
        .unwrap();
    assert_eq!(corrupt.readable, Some(false));
    assert!(
        health
            .files
            .iter()
            .all(|file| !file.file_name.contains('/'))
    );

    let missing = index.artifact_health("org.example:missing:1.0").unwrap();
    assert!(!missing.found);
    assert!(missing.files.is_empty());
}

#[test]
fn searches_class_members_and_annotations_deterministically_with_limits() {
    let root = TempDir::new().unwrap();
    for (artifact, class_name) in [
        ("first", "org/example/First"),
        ("second", "org/example/Second"),
    ] {
        let jar = root
            .path()
            .join("org/example")
            .join(artifact)
            .join("1.0")
            .join(format!("{artifact}-1.0.jar"));
        let class = inspection_class(class_name);
        let entry = format!("{class_name}.class");
        write_jar(&jar, &[(entry.as_str(), &class)]);
    }
    let index = build_index(&root, 10, 64 * 1024);

    let methods = index.search_class_members("GREET", None, None);
    assert_eq!(methods.len(), 4);
    assert_eq!(methods[0].kind, ClassMemberMatchKind::Method);
    assert_eq!(methods[0].jar.artifact_id, "first");
    assert_eq!(methods[1].jar.artifact_id, "first");
    assert_eq!(methods[2].jar.artifact_id, "second");
    assert_eq!(methods[3].jar.artifact_id, "second");
    assert_eq!(
        methods
            .iter()
            .map(|method| method.signature.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "(I)Ljava/lang/String;",
            "(Ljava/lang/String;)Ljava/lang/String;"
        ])
    );
    let fields = index.search_class_members("value", Some("first-1.0.jar"), None);
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].kind, ClassMemberMatchKind::Field);
    let annotations = index.search_class_members("deprecated", None, Some(2));
    assert_eq!(annotations.len(), 2);
    assert!(
        annotations
            .iter()
            .all(|item| item.kind == ClassMemberMatchKind::Annotation)
    );
    assert!(index.search_class_members("missing", None, None).is_empty());
}

#[test]
fn searches_members_with_inherited_name_collisions() {
    let root = TempDir::new().unwrap();
    let base = root.path().join("org/example/base/1.0/base-1.0.jar");
    let child = root.path().join("org/example/child/1.0/child-1.0.jar");
    let base_class = inspection_class("org/example/Base");
    let mut child_class = inspection_class("org/example/Child");
    replace_ascii(&mut child_class, b"java/lang/Object", b"org/example/Base");
    write_jar(&base, &[("org/example/Base.class", &base_class)]);
    write_jar(&child, &[("org/example/Child.class", &child_class)]);
    let index = build_index(&root, 10, 64 * 1024);

    let child_description = index
        .describe_class("org.example.Child", Some("child-1.0.jar"), None, true)
        .unwrap();
    assert_eq!(
        child_description[0].super_class.as_deref(),
        Some("org.example.Base")
    );

    let matches = index.search_class_members("greet", None, None);
    assert_eq!(matches.len(), 4);
    assert_eq!(
        matches
            .iter()
            .filter(|item| item.class_name == "org.example.Base")
            .count(),
        2
    );
    assert_eq!(
        matches
            .iter()
            .filter(|item| item.class_name == "org.example.Child")
            .count(),
        2
    );
}

#[test]
fn compares_added_removed_and_changed_public_api_between_versions() {
    let root = TempDir::new().unwrap();
    let version_one = root.path().join("org/example/demo/1.0");
    let version_two = root.path().join("org/example/demo/2.0");
    let common_v1 = inspection_class("org/example/Common");
    let old_only = inspection_class("org/example/OldOnly");
    write_jar(
        &version_one.join("demo-1.0.jar"),
        &[
            ("org/example/Common.class", &common_v1),
            ("org/example/OldOnly.class", &old_only),
        ],
    );
    let mut common_v2 = inspection_class("org/example/Common");
    replace_ascii(&mut common_v2, b"java/lang/Object", b"java/lang/Number");
    replace_ascii(
        &mut common_v2,
        b"java/io/Serializable",
        b"java/lang/Comparable",
    );
    replace_ascii(&mut common_v2, b"greet", b"other");
    replace_ascii(&mut common_v2, b"value", b"other");
    let new_only = inspection_class("org/example/NewOnly");
    write_jar(
        &version_two.join("demo-2.0.jar"),
        &[
            ("org/example/Common.class", &common_v2),
            ("org/example/NewOnly.class", &new_only),
        ],
    );
    let index = build_index(&root, 20, 64 * 1024);

    let diff = index
        .compare_artifact_api("org.example", "demo", "1.0", "2.0")
        .unwrap();
    assert_eq!(diff.added_classes, vec!["org.example.NewOnly"]);
    assert_eq!(diff.removed_classes, vec!["org.example.OldOnly"]);
    assert_eq!(diff.changed_classes.len(), 1);
    let changed = &diff.changed_classes[0];
    assert_eq!(changed.class_name, "org.example.Common");
    assert_eq!(
        changed.added_members,
        vec![
            "field:other:Ljava/lang/String;",
            "method:other(I)Ljava/lang/String;",
            "method:other(Ljava/lang/String;)Ljava/lang/String;"
        ]
    );
    assert_eq!(
        changed.removed_members,
        vec![
            "field:value:Ljava/lang/String;",
            "method:greet(I)Ljava/lang/String;",
            "method:greet(Ljava/lang/String;)Ljava/lang/String;"
        ]
    );
    assert_eq!(
        changed.previous_super_class.as_deref(),
        Some("java.lang.Object")
    );
    assert_eq!(
        changed.current_super_class.as_deref(),
        Some("java.lang.Number")
    );
    assert_eq!(changed.added_interfaces, vec!["java.lang.Comparable"]);
    assert_eq!(changed.removed_interfaces, vec!["java.io.Serializable"]);

    let unchanged = index
        .compare_artifact_api("org.example", "demo", "2.0", "2.0")
        .unwrap();
    assert!(unchanged.added_classes.is_empty());
    assert!(unchanged.removed_classes.is_empty());
    assert!(unchanged.changed_classes.is_empty());
    assert!(
        index
            .compare_artifact_api("org.example", "demo", "1.0", "missing")
            .is_err()
    );
}

#[test]
fn searches_supported_text_resources_with_context_and_budgets() {
    let root = TempDir::new().unwrap();
    let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
    write_jar(
        &jar,
        &[
            (
                "META-INF/MANIFEST.MF",
                b"Manifest-Version: 1.0\nDemo-Key: Found\n",
            ),
            ("META-INF/services/example.Service", b"org.example.Foo\n"),
            ("config/application.properties", b"feature.name=Found\n"),
            (
                "config/data.json",
                b"this content is deliberately too large: Found",
            ),
            ("config/binary.properties", &[0xff, 0xfe, 0xfd]),
            ("image.bin", b"Found but unsupported"),
        ],
    );
    let index = build_index(&root, 10, 40);

    let service = index
        .search_jar_content("EXAMPLE.FOO", Some("demo-1.0.jar"), None)
        .unwrap();
    assert_eq!(service.results.len(), 1);
    assert_eq!(
        service.results[0].entry,
        "META-INF/services/example.Service"
    );
    assert_eq!(service.results[0].line, 1);
    assert_eq!(service.results[0].context, "org.example.Foo");
    let found = index.search_jar_content("found", None, None).unwrap();
    assert_eq!(found.results.len(), 2);
    assert!(found.incomplete);
    assert!(found.results.iter().all(|item| item.entry != "image.bin"));
    let limited = index.search_jar_content("found", None, Some(1)).unwrap();
    assert_eq!(limited.results.len(), 1);
    assert!(limited.incomplete);
    assert!(
        index
            .search_jar_content("missing", None, None)
            .unwrap()
            .results
            .is_empty()
    );
}

#[test]
fn searches_spring_auto_configuration_descriptors_as_text_resources() {
    let root = TempDir::new().unwrap();
    let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
    write_jar(
        &jar,
        &[
            (
                "META-INF/spring/org.springframework.boot.autoconfigure.AutoConfiguration.imports",
                b"org.example.AutoConfiguration\n",
            ),
            (
                "META-INF/spring.factories",
                b"org.springframework.boot.autoconfigure.EnableAutoConfiguration=org.example.LegacyFactory\n",
            ),
        ],
    );
    let index = build_index(&root, 10, 4096);

    let imports = index
        .search_jar_content("org.example.AutoConfiguration", None, None)
        .unwrap();
    assert_eq!(imports.results.len(), 1);
    assert!(imports.results[0].entry.ends_with(".imports"));

    let factories = index
        .search_jar_content("org.example.LegacyFactory", None, None)
        .unwrap();
    assert_eq!(factories.results.len(), 1);
    assert_eq!(factories.results[0].entry, "META-INF/spring.factories");
}

#[test]
fn jar_content_search_surfaces_zip_errors_after_indexing() {
    let root = TempDir::new().unwrap();
    let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
    write_jar(&jar, &[("config.txt", b"searchable")]);
    let index = build_index(&root, 10, 1024);
    std::fs::write(&jar, b"corrupt after startup").unwrap();

    assert!(index.search_jar_content("searchable", None, None).is_err());
}

#[test]
fn recognizes_multi_release_classes_once() {
    assert_eq!(
        class_name_from_entry("META-INF/versions/17/org/example/Foo.class"),
        Some("org.example.Foo".to_owned())
    );
    assert_eq!(class_name_from_entry("module-info.class"), None);
}

#[test]
fn class_search_is_case_insensitive_and_respects_result_limit() {
    let root = TempDir::new().unwrap();
    let first = root.path().join("org/example/first/1.0/first-1.0.jar");
    let second = root.path().join("org/example/second/1.0/second-1.0.jar");
    write_jar(&first, &[("org/example/Foo.class", b"one")]);
    write_jar(&second, &[("org/example/Foo.class", b"two")]);
    let index = build_index(&root, 1, 1024);

    let matches = index.search_classes("fOo", Some(20));
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].class_name, "org.example.Foo");
    assert!(index.search_classes("   ", None).is_empty());
}

#[test]
fn jar_selection_supports_coordinate_filename_and_relative_path() {
    let (_root, index) = fixture();
    let coordinate = index.list_classes("org.example:demo:1.2.0", 0, None);
    let filename = index.list_classes("demo-1.2.0.jar", 0, None);
    let path = index.list_classes("org/example/demo/1.2.0/demo-1.2.0.jar", 0, None);

    assert_eq!(coordinate, filename);
    assert_eq!(filename, path);
    assert!(index.list_classes("missing.jar", 0, None).is_empty());
}

#[test]
fn class_listing_paginates_and_entry_search_can_target_one_jar() {
    let root = TempDir::new().unwrap();
    let first = root.path().join("org/example/demo/1.0/demo-1.0.jar");
    let second = root.path().join("org/example/other/1.0/other-1.0.jar");
    write_jar(
        &first,
        &[
            ("org/example/Alpha.class", b"a"),
            ("org/example/Beta.class", b"b"),
            ("config/shared.conf", b"first"),
        ],
    );
    write_jar(&second, &[("other/shared.conf", b"second")]);
    let index = build_index(&root, 10, 1024);

    let page = index.list_classes("demo-1.0.jar", 1, Some(1));
    assert_eq!(page[0].total, 2);
    assert_eq!(page[0].classes, vec!["org.example.Beta"]);
    let entries = index.search_entries("shared.conf", Some("demo-1.0.jar"), None);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].jar.artifact_id, "demo");
}

#[test]
fn source_lookup_supports_inner_classes_filters_and_truncation() {
    let root = TempDir::new().unwrap();
    let version_one = root.path().join("org/example/demo/1.0");
    let version_two = root.path().join("org/example/demo/2.0");
    for base in [&version_one, &version_two] {
        let version = base.file_name().unwrap().to_str().unwrap();
        write_jar(
            &base.join(format!("demo-{version}.jar")),
            &[("org/example/Outer$Inner.class", b"bytecode")],
        );
        write_jar(
            &base.join(format!("demo-{version}-sources.jar")),
            &[(
                "org/example/Outer.java",
                b"public class Outer { class Inner {} }",
            )],
        );
    }
    let index = build_index(&root, 10, 12);

    let sources = index
        .class_source("org.example.Outer$Inner", None, Some("2.0"))
        .unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].source_jar.version, "2.0");
    assert!(sources[0].truncated);
    assert_eq!(sources[0].source.len(), 12);
    assert!(
        index
            .class_source("org.example.Outer$Inner", Some("demo-1.0.jar"), Some("2.0"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn jar_names_and_source_lookup_support_unicode_java_and_kotlin() {
    let root = TempDir::new().unwrap();
    let base = root.path().join("org/example/demo/1.0");
    write_jar(
        &base.join("demo-1.0.jar"),
        &[
            ("org/example/Árvíz.class", b"java bytecode"),
            ("org/example/Tükör.class", b"kotlin bytecode"),
        ],
    );
    write_jar(
        &base.join("demo-1.0-sources.jar"),
        &[
            ("org/example/Árvíz.java", b"// Java source"),
            ("org/example/Tükör.kt", b"// Kotlin source"),
        ],
    );
    let index = build_index(&root, 10, 1024);

    assert_eq!(index.search_classes("Árvíz", None).len(), 1);
    assert_eq!(index.search_classes("Tükör", None).len(), 1);
    for (class_name, entry_path, source) in [
        (
            "org.example.Árvíz",
            "org/example/Árvíz.java",
            "// Java source",
        ),
        (
            "org.example.Tükör",
            "org/example/Tükör.kt",
            "// Kotlin source",
        ),
    ] {
        let sources = index.class_source(class_name, None, None).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].entry, entry_path);
        assert_eq!(sources[0].source, source);
        assert!(!sources[0].truncated);
    }
    assert!(
        index
            .class_source("org.example.Missing", None, None)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn source_lookup_surfaces_corrupt_local_entry_headers_after_indexing() {
    let root = TempDir::new().unwrap();
    let base = root.path().join("org/example/demo/1.0");
    let source_jar = base.join("demo-1.0-sources.jar");
    write_jar(
        &base.join("demo-1.0.jar"),
        &[("org/example/Foo.class", b"bytecode")],
    );
    write_jar(&source_jar, &[("org/example/Foo.java", b"// source")]);
    let index = build_index(&root, 10, 1024);

    // Keep the central directory readable, but break the local entry header.
    let mut bytes = std::fs::read(&source_jar).unwrap();
    assert_eq!(&bytes[..4], b"PK\x03\x04");
    bytes[..4].fill(0);
    std::fs::write(&source_jar, bytes).unwrap();
    assert!(ZipArchive::new(File::open(&source_jar).unwrap()).is_ok());

    let error = index
        .class_source("org.example.Foo", None, None)
        .expect_err("an unreadable source entry must not become a missing result");
    assert!(error.to_string().contains("cannot read source JAR entry"));
}

#[test]
fn artifact_versions_are_group_scoped_and_sorted() {
    let root = TempDir::new().unwrap();
    for (group, version) in [
        ("com/acme", "2.0"),
        ("com/acme", "1.0"),
        ("org/demo", "9.0"),
    ] {
        write_jar(
            &root
                .path()
                .join(group)
                .join("shared")
                .join(version)
                .join(format!("shared-{version}.jar")),
            &[("Shared.class", b"x")],
        );
    }
    let index = build_index(&root, 10, 1024);

    assert_eq!(index.artifact_versions("shared", None).len(), 2);
    assert_eq!(
        index.artifact_versions("shared", Some("com.acme"))["com.acme:shared"],
        vec!["1.0", "2.0"]
    );
    assert!(index.artifact_versions("missing", None).is_empty());
}

#[test]
fn classifiers_are_addressable_and_multi_release_classes_are_deduplicated() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("org/example/demo/1.0/demo-1.0-tests.jar");
    write_jar(
        &path,
        &[
            ("org/example/Foo.class", b"base"),
            ("META-INF/versions/17/org/example/Foo.class", b"java17"),
            ("module-info.class", b"module"),
        ],
    );
    let index = build_index(&root, 10, 1024);

    let jars = index.search_jars("org.example:demo:1.0:tests", None);
    assert_eq!(jars.len(), 1);
    assert_eq!(jars[0].classifier.as_deref(), Some("tests"));
    assert_eq!(jars[0].class_count, 1);
}

#[test]
fn searches_direct_and_transitive_type_hierarchy_deterministically() {
    let root = TempDir::new().unwrap();
    let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
    let implementation = hierarchy_class(
        "org/example/Implementation",
        "java/lang/Object",
        &["org/example/Service"],
    );
    let child = hierarchy_class("org/example/Child", "org/example/Implementation", &[]);
    write_jar(
        &jar,
        &[
            ("org/example/Implementation.class", &implementation),
            ("org/example/Child.class", &child),
        ],
    );
    let index = build_index(&root, 10, 64 * 1024);

    let direct = index.search_type_hierarchy("org.example.Service", false, None, None);
    assert_eq!(direct.len(), 1);
    assert_eq!(direct[0].relation, TypeRelation::Implements);
    assert_eq!(
        direct[0].path,
        vec!["org.example.Service", "org.example.Implementation"]
    );
    let transitive = index.search_type_hierarchy("org.example.Service", true, None, None);
    assert_eq!(transitive.len(), 2);
    assert_eq!(transitive[1].type_name, "org.example.Child");
    assert_eq!(transitive[1].depth, 2);
    assert!(
        index
            .search_type_hierarchy("org.example.Missing", true, None, None)
            .is_empty()
    );
}

#[test]
fn searches_java_and_kotlin_sources_with_regex_context_and_limits() {
    let root = TempDir::new().unwrap();
    let base = root.path().join("org/example/demo/1.0");
    write_jar(
        &base.join("demo-1.0.jar"),
        &[("org/example/Demo.class", b"unparseable is still indexed")],
    );
    write_jar(
        &base.join("demo-1.0-sources.jar"),
        &[
            (
                "org/example/Demo.java",
                b"package org.example;\nclass Demo {\n  String needle = \"java\";\n}\n",
            ),
            (
                "org/example/Other.kt",
                b"package org.example\nclass Other { val needle = \"kotlin\" }\n",
            ),
        ],
    );
    let index = build_index(&root, 10, 64 * 1024);

    let found = index
        .search_source("needle\\s*=", true, Some("demo-1.0.jar"), 1, None)
        .unwrap();
    assert_eq!(found.results.len(), 2);
    assert_eq!(found.results[0].line, 3);
    assert!(found.results[0].context.contains("class Demo"));
    assert!(index.search_source("[", true, None, 0, None).is_err());
    assert!(index.search_source(" ", false, None, 0, None).is_err());
}

#[test]
fn returns_class_and_method_declaration_source_slices() {
    let root = TempDir::new().unwrap();
    let base = root.path().join("org/example/demo/1.0");
    let class = inspection_class("org/example/Inspectable");
    write_jar(
        &base.join("demo-1.0.jar"),
        &[("org/example/Inspectable.class", &class)],
    );
    write_jar(
        &base.join("demo-1.0-sources.jar"),
        &[(
            "org/example/Inspectable.java",
            b"package org.example;\npublic abstract class Inspectable {\n  public abstract String greet(String value);\n}\n",
        )],
    );
    let index = build_index(&root, 10, 64 * 1024);

    let class_result = index
        .get_declaration_source("org.example.Inspectable", None, None, None, None)
        .unwrap();
    assert_eq!(class_result.results[0].start_line, 2);
    assert!(class_result.results[0].source.contains("greet"));
    let method = index
        .get_declaration_source(
            "org.example.Inspectable",
            Some("greet"),
            Some("(Ljava/lang/String;)Ljava/lang/String;"),
            None,
            None,
        )
        .unwrap();
    assert_eq!(method.results[0].kind, DeclarationKind::Method);
    assert_eq!(
        method.results[0].source.trim(),
        "public abstract String greet(String value);"
    );
}

#[test]
fn indexes_and_filters_constant_pool_references() {
    assert!(
        std::mem::size_of::<IndexedReference>() <= 16,
        "reference records must remain compact because repositories retain millions of them"
    );
    let root = TempDir::new().unwrap();
    let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
    let caller = referencing_class("org/example/Caller", "org/example/Target");
    let target = hierarchy_class("org/example/Target", "java/lang/Object", &[]);
    write_jar(
        &jar,
        &[
            ("org/example/Caller.class", &caller),
            ("org/example/Target.class", &target),
        ],
    );
    let index = build_index(&root, 10, 64 * 1024);

    let jar = &index.jars[0];
    let caller_facts = jar
        .class_facts
        .iter()
        .filter(|facts| jar.fact_string(facts.source_class) == "org.example.Caller")
        .collect::<Vec<_>>();
    assert_eq!(caller_facts.len(), 1, "source class facts must be grouped");
    let target_owners = caller_facts[0]
        .references
        .iter()
        .filter(|reference| jar.fact_string(reference.target_owner) == "org.example.Target")
        .map(|reference| reference.target_owner)
        .collect::<Vec<_>>();
    assert!(target_owners.len() >= 2);
    assert!(
        target_owners.windows(2).all(|pair| pair[0] == pair[1]),
        "repeated reference strings must use one compact ID per JAR"
    );

    let inbound = index.search_class_references(
        "org.example.Target",
        ReferenceDirection::Inbound,
        Some(ClassReferenceKind::Method),
        Some("call"),
        Some("()V"),
        None,
        None,
    );
    assert_eq!(inbound.len(), 1);
    assert_eq!(inbound[0].source_class, "org.example.Caller");
    assert_eq!(inbound[0].target_artifacts, vec!["org.example:demo:1.0"]);
    let outbound = index.search_class_references(
        "org.example.Caller",
        ReferenceDirection::Outbound,
        None,
        None,
        None,
        None,
        None,
    );
    assert!(outbound.iter().any(|reference| {
        reference.kind == ClassReferenceKind::Class
            && reference.target_owner == "org.example.Target"
    }));
    assert!(
        outbound
            .iter()
            .all(|reference| reference.target_owner != "org.example.Caller")
    );
}

#[test]
fn indexes_service_module_and_spring_provider_facts() {
    let root = TempDir::new().unwrap();
    let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
    let module = module_info_class();
    write_jar(
        &jar,
        &[
            ("module-info.class", &module),
            (
                "META-INF/services/example.Service",
                b"# comment\nexample.Provider\ninvalid provider\nexample.Provider\n",
            ),
            (
                "META-INF/spring.factories",
                b"example.Factory=example.First,\\\n example.Second\nbroken\n",
            ),
            (
                "META-INF/spring/example.ImportSelector.imports",
                b"example.Imported\n",
            ),
        ],
    );
    let index = build_index(&root, 20, 64 * 1024);

    let service = index.search_providers(
        Some("example.Service"),
        None,
        Some(ProviderDescriptorKind::ServiceLoader),
        None,
        None,
    );
    assert_eq!(service.len(), 1);
    assert_eq!(service[0].provider.as_deref(), Some("example.Provider"));
    let module_provides = index.search_providers(
        Some("example.Service"),
        Some("example.Provider"),
        Some(ProviderDescriptorKind::ModuleProvides),
        None,
        None,
    );
    assert_eq!(module_provides.len(), 1);
    let uses = index.search_providers(
        Some("example.Used"),
        None,
        Some(ProviderDescriptorKind::ModuleUses),
        None,
        None,
    );
    assert_eq!(uses.len(), 1);
    assert_eq!(uses[0].provider, None);
    assert_eq!(
        index
            .search_providers(
                Some("example.Factory"),
                None,
                Some(ProviderDescriptorKind::SpringFactories),
                None,
                None,
            )
            .len(),
        2
    );
}

#[test]
fn malformed_and_non_maven_jars_are_skipped() {
    let root = TempDir::new().unwrap();
    let malformed = root.path().join("org/example/demo/1.0/demo-1.0.jar");
    std::fs::create_dir_all(malformed.parent().unwrap()).unwrap();
    std::fs::write(malformed, b"not a zip").unwrap();
    write_jar(&root.path().join("standalone.jar"), &[("Foo.class", b"x")]);

    let index = build_index(&root, 10, 1024);
    assert_eq!(
        index.stats(),
        IndexStats {
            jar_count: 0,
            source_jar_count: 0,
            class_count: 0,
            unique_class_count: 0,
            artifact_count: 0,
        }
    );
}
