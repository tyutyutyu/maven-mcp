use std::io::Write;

use tempfile::TempDir;
use zip::{ZipWriter, write::SimpleFileOptions};

use super::*;

const MULTI_RELEASE_MANIFEST: (&str, &[u8]) = (
    "META-INF/MANIFEST.MF",
    b"Manifest-Version: 1.0\r\nMulti-Release: true\r\n\r\n",
);

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
    assert_eq!(read_jar_index(&path).unwrap().entries, ["a", "b"]);
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
            MULTI_RELEASE_MANIFEST,
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
fn recognizes_effective_class_names() {
    assert_eq!(
        class_name_from_entry("org/example/Foo.class"),
        Some("org.example.Foo".to_owned())
    );
    assert_eq!(class_name_from_entry("module-info.class"), None);
    assert_eq!(
        class_name_from_entry("org/example/package-info.class"),
        None
    );
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
            MULTI_RELEASE_MANIFEST,
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
fn source_search_reports_truncated_sources_regardless_of_matches() {
    let root = TempDir::new().unwrap();
    let base = root.path().join("org/example/demo/1.0");
    write_jar(
        &base.join("demo-1.0.jar"),
        &[("org/example/Demo.class", b"unparseable is still indexed")],
    );
    write_jar(
        &base.join("demo-1.0-sources.jar"),
        &[
            ("org/example/Short.java", b"class Short { int needle; }"),
            (
                "org/example/Long.java",
                "// padding é\nclass Long { int tail; }".as_bytes(),
            ),
        ],
    );
    // Short.java is 27 bytes and fits; Long.java is cut inside the word "tail".
    let index = build_index(&root, 10, 32);

    let after_limit = index.search_source("tail", false, None, 0, None).unwrap();
    assert!(after_limit.results.is_empty());
    assert!(after_limit.incomplete);
    assert_eq!(after_limit.scanned_bytes, 27 + 32);

    let mixed = index.search_source("needle", false, None, 0, None).unwrap();
    assert_eq!(mixed.results.len(), 1);
    assert!(!mixed.results[0].truncated);
    assert!(mixed.incomplete);

    let complete_root = TempDir::new().unwrap();
    write_jar(
        &complete_root
            .path()
            .join("org/example/demo/1.0/demo-1.0.jar"),
        &[("org/example/Demo.class", b"unparseable is still indexed")],
    );
    write_jar(
        &complete_root
            .path()
            .join("org/example/demo/1.0/demo-1.0-sources.jar"),
        &[("org/example/Short.java", b"class Short {}")],
    );
    let complete = build_index(&complete_root, 10, 32)
        .search_source("missing", false, None, 0, None)
        .unwrap();
    assert!(complete.results.is_empty());
    assert!(!complete.incomplete);
    assert_eq!(complete.scanned_bytes, 14);
}

#[test]
fn source_search_trims_a_utf8_sequence_cut_by_truncation() {
    let root = TempDir::new().unwrap();
    let base = root.path().join("org/example/demo/1.0");
    write_jar(
        &base.join("demo-1.0.jar"),
        &[("org/example/Demo.class", b"unparseable is still indexed")],
    );
    write_jar(
        &base.join("demo-1.0-sources.jar"),
        &[("org/example/Demo.java", "abcd\u{e9}\u{e9}".as_bytes())],
    );
    let index = build_index(&root, 10, 5);

    let found = index.search_source("abcd", false, None, 0, None).unwrap();
    assert_eq!(found.results.len(), 1);
    assert!(found.results[0].truncated);
    assert_eq!(found.results[0].context, "abcd");
    assert_eq!(found.scanned_bytes, 4);
    assert!(found.incomplete);
}

#[test]
fn source_search_trims_a_cut_utf8_sequence_after_an_invalid_interior_byte() {
    let root = TempDir::new().unwrap();
    let base = root.path().join("org/example/demo/1.0");
    write_jar(
        &base.join("demo-1.0.jar"),
        &[("org/example/Demo.class", b"unparseable is still indexed")],
    );
    write_jar(
        &base.join("demo-1.0-sources.jar"),
        &[("org/example/Demo.java", b"\xffabcd\xc3\xa9")],
    );
    let index = build_index(&root, 10, 6);

    let found = index.search_source("abcd", false, None, 0, None).unwrap();
    assert_eq!(found.results.len(), 1);
    assert!(found.results[0].truncated);
    assert_eq!(found.results[0].context, "\u{fffd}abcd");
    assert_eq!(found.scanned_bytes, 5);
    assert!(found.incomplete);
}

#[test]
fn source_search_surfaces_read_errors_after_indexing() {
    let root = TempDir::new().unwrap();
    let base = root.path().join("org/example/demo/1.0");
    let sources = base.join("demo-1.0-sources.jar");
    write_jar(
        &base.join("demo-1.0.jar"),
        &[("org/example/Demo.class", b"unparseable is still indexed")],
    );
    write_jar(&sources, &[("org/example/Demo.java", b"class Demo {}")]);
    let index = build_index(&root, 10, 1024);
    std::fs::write(&sources, b"corrupt after startup").unwrap();

    assert!(index.search_source("Demo", false, None, 0, None).is_err());
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

const OUTSIDE_MARKER: &str = "outside-marker-secret";

fn outside_pom(directory: &Path) {
    std::fs::create_dir_all(directory).unwrap();
    std::fs::write(
        directory.join("demo-1.0.pom"),
        format!("<project><description>{OUTSIDE_MARKER}</description></project>"),
    )
    .unwrap();
}

#[test]
fn exact_coordinates_reject_empty_dot_and_separator_segments() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    outside_pom(&outside.path().join("demo/1.0"));
    let index = build_index(&root, 10, 1024);
    let absolute = outside.path().display().to_string().replace('/', ".");
    for coordinate in [
        format!("{absolute}:demo:1.0"),
        ".com.example:demo:1.0".to_owned(),
        "com.example.:demo:1.0".to_owned(),
        "com..example:demo:1.0".to_owned(),
        ".:demo:1.0".to_owned(),
        "com.example:..:1.0".to_owned(),
        "com.example:demo:..".to_owned(),
        "com/example:demo:1.0".to_owned(),
        "com\\example:demo:1.0".to_owned(),
        "com.example:de/mo:1.0".to_owned(),
        "com.example:demo:1.0/".to_owned(),
        "com.example:demo:1.0\\..\\x".to_owned(),
        "com.example:demo:1.0:../../classifier".to_owned(),
        "com.example:demo:1.0:sources".to_owned(),
    ] {
        assert!(
            validate_exact_coordinate(&coordinate).is_err(),
            "{coordinate}"
        );
        let error = index.pom_descriptor(&coordinate).expect_err(&coordinate);
        assert!(!error.to_string().contains(OUTSIDE_MARKER));
        assert!(index.artifact_health(&coordinate).is_err(), "{coordinate}");
    }
    for coordinate in [
        "org.example:demo:1.0",
        "org.example:demo:1.0-SNAPSHOT",
        "org.example-x:demo_y:1.0.0.RELEASE",
    ] {
        validate_exact_coordinate(coordinate).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn exact_artifact_lookups_do_not_follow_symlinks_outside_the_repository() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    outside_pom(&outside.path().join("demo/1.0"));
    std::fs::write(outside.path().join("demo/1.0/demo-1.0-evil.jar"), b"x").unwrap();
    let index = build_index(&root, 10, 1024);

    // Symlinked group directory.
    std::fs::create_dir_all(root.path().join("org")).unwrap();
    symlink(outside.path(), root.path().join("org/example")).unwrap();
    // Symlinked version directory.
    let linked_version = root.path().join("org/linked/demo");
    std::fs::create_dir_all(&linked_version).unwrap();
    symlink(outside.path().join("demo/1.0"), linked_version.join("1.0")).unwrap();
    // Symlinked POM file and jar inside a real artifact directory.
    let real = root.path().join("org/real/demo/1.0");
    std::fs::create_dir_all(&real).unwrap();
    symlink(
        outside.path().join("demo/1.0/demo-1.0.pom"),
        real.join("demo-1.0.pom"),
    )
    .unwrap();
    symlink(
        outside.path().join("demo/1.0/demo-1.0-evil.jar"),
        real.join("demo-1.0-evil.jar"),
    )
    .unwrap();

    for coordinate in [
        "org.example:demo:1.0",
        "org.linked:demo:1.0",
        "org.real:demo:1.0",
    ] {
        let error = index.pom_descriptor(coordinate).expect_err(coordinate);
        assert!(error.is::<ArtifactOutsideRepository>(), "{coordinate}");
        let message = error.to_string();
        assert!(!message.contains(OUTSIDE_MARKER));
        assert!(!message.contains(outside.path().to_str().unwrap()));
    }
    for coordinate in ["org.example:demo:1.0", "org.linked:demo:1.0"] {
        let error = index.artifact_health(coordinate).expect_err(coordinate);
        assert!(error.is::<ArtifactOutsideRepository>(), "{coordinate}");
    }
    // Symlinked files are never listed as artifact files.
    let health = index.artifact_health("org.real:demo:1.0").unwrap();
    assert!(health.found);
    assert!(health.files.is_empty());

    // A missing artifact below a symlinked parent is still rejected, not reported as absent.
    let error = index
        .pom_descriptor("org.example:missing:1.0")
        .expect_err("missing artifact below outward symlink");
    assert!(error.is::<ArtifactOutsideRepository>());
}

#[cfg(unix)]
#[test]
fn exact_artifact_lookups_reject_dangling_symlinks_outside_the_repository() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let index = build_index(&root, 10, 1024);
    let missing_outside = outside.path().join("missing");

    // Dangling group directory symlink.
    std::fs::create_dir_all(root.path().join("org")).unwrap();
    symlink(&missing_outside, root.path().join("org/dangling")).unwrap();
    // Dangling relative POM symlink inside a real artifact directory.
    let real = root.path().join("org/real/demo/1.0");
    std::fs::create_dir_all(&real).unwrap();
    let depth = real.canonicalize().unwrap().components().count() - 1;
    let relative_outside = std::iter::repeat_n("..", depth)
        .collect::<PathBuf>()
        .join(missing_outside.strip_prefix("/").unwrap())
        .join("demo-1.0.pom");
    symlink(&relative_outside, real.join("demo-1.0.pom")).unwrap();
    // Chained dangling symlinks: in-repository hop to an outward dangling link.
    symlink(
        root.path().join("org/dangling"),
        root.path().join("org/chained"),
    )
    .unwrap();

    for coordinate in ["org.dangling:demo:1.0", "org.chained:demo:1.0"] {
        for error in [
            index.pom_descriptor(coordinate).expect_err(coordinate),
            index.artifact_health(coordinate).expect_err(coordinate),
        ] {
            assert!(error.is::<ArtifactOutsideRepository>(), "{coordinate}");
            assert!(!error.to_string().contains(outside.path().to_str().unwrap()));
        }
    }
    let error = index
        .pom_descriptor("org.real:demo:1.0")
        .expect_err("dangling outward POM symlink");
    assert!(error.is::<ArtifactOutsideRepository>());
    assert!(!error.to_string().contains(outside.path().to_str().unwrap()));

    // A dangling symlink that stays inside the repository is still an absent artifact.
    symlink(
        root.path().join("org/nowhere"),
        root.path().join("org/inside"),
    )
    .unwrap();
    assert!(!index.pom_descriptor("org.inside:demo:1.0").unwrap().found);
    assert!(!index.artifact_health("org.inside:demo:1.0").unwrap().found);
}

#[cfg(unix)]
#[test]
fn exact_artifact_lookups_allow_symlinks_that_stay_inside_the_repository() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().unwrap();
    let real = root.path().join("org/example/demo/1.0");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::write(real.join("demo-1.0.pom"), "<project/>").unwrap();
    symlink(
        root.path().join("org/example"),
        root.path().join("org/alias"),
    )
    .unwrap();
    let index = build_index(&root, 10, 1024);

    assert!(index.pom_descriptor("org.alias:demo:1.0").unwrap().found);
    assert!(!index.pom_descriptor("org.alias:demo:2.0").unwrap().found);
}

fn shape_variant(super_class: &[u8], member: &[u8]) -> Vec<u8> {
    let mut bytes = inspection_class("org/example/Shape");
    replace_ascii(&mut bytes, b"java/lang/Object", super_class);
    replace_ascii(&mut bytes, b"greet", member);
    bytes
}

#[test]
fn multi_release_manifest_flag_parsing_is_strict_and_case_insensitive() {
    assert!(manifest_declares_multi_release(
        "Manifest-Version: 1.0\nmulti-release:  TRUE\n"
    ));
    assert!(manifest_declares_multi_release(
        "Manifest-Version: 1.0\nMulti-Rel\n ease: true\n"
    ));
    assert!(manifest_declares_multi_release(
        "Manifest-Version: 1.0\rMulti-Release: true\r\r"
    ));
    assert!(manifest_declares_multi_release(
        "Manifest-Version: 1.0\rMulti-Rel\r ease: true\r\r"
    ));
    assert!(!manifest_declares_multi_release(
        "Manifest-Version: 1.0\r\rMulti-Release: true\r"
    ));
    assert!(!manifest_declares_multi_release("Multi-Release: false\n"));
    assert!(!manifest_declares_multi_release("Created-By: x\n"));
    assert!(!manifest_declares_multi_release(
        "Manifest-Version: 1.0\n\nName: a\nMulti-Release: true\n"
    ));
}

#[test]
fn effective_class_entries_follow_manifest_flag_base_fallback_and_minimum_version() {
    let entries = [
        "META-INF/versions/11/a/Both.class",
        "META-INF/versions/17/a/Both.class",
        "META-INF/versions/17/a/Only.class",
        "META-INF/versions/8/a/Old.class",
        "META-INF/versions/x/a/Bad.class",
        "META-INF/versions/+11/a/Signed.class",
        "META-INF/versions/011/a/Padded.class",
        "META-INF/versions/+21/a/Both.class",
        "META-INF/versions/021/a/Both.class",
        "a/Both.class",
        "a/Base.class",
    ]
    .map(str::to_owned);
    let enabled = effective_class_entries(&entries, true);
    assert_eq!(
        enabled.into_iter().collect::<Vec<_>>(),
        vec![
            ("a/Base.class".to_owned(), "a/Base.class".to_owned()),
            (
                "a/Both.class".to_owned(),
                "META-INF/versions/17/a/Both.class".to_owned()
            ),
            (
                "a/Only.class".to_owned(),
                "META-INF/versions/17/a/Only.class".to_owned()
            ),
        ]
    );
    let disabled = effective_class_entries(&entries, false);
    assert_eq!(
        disabled.into_values().collect::<Vec<_>>(),
        vec!["a/Base.class", "a/Both.class"]
    );
}

#[test]
fn all_classfile_tools_use_the_same_multi_release_variant() {
    let root = TempDir::new().unwrap();
    let base = shape_variant(b"java/lang/Object", b"greet");
    let java11 = shape_variant(b"java/lang/Number", b"mid11");
    let java17 = shape_variant(b"java/lang/Number", b"late7");
    write_jar(
        &root.path().join("org/example/demo/1.0/demo-1.0.jar"),
        &[
            MULTI_RELEASE_MANIFEST,
            ("org/example/Shape.class", &base),
            ("META-INF/versions/11/org/example/Shape.class", &java11),
            ("META-INF/versions/17/org/example/Shape.class", &java17),
        ],
    );
    write_jar(
        &root.path().join("org/example/demo/2.0/demo-2.0.jar"),
        &[("org/example/Shape.class", &base)],
    );
    write_jar(
        &root.path().join("org/example/legacy/1.0/legacy-1.0.jar"),
        &[
            ("org/example/Shape.class", &base),
            ("META-INF/versions/17/org/example/Shape.class", &java17),
            (
                "META-INF/versions/17/org/example/VersionOnly.class",
                &inspection_class("org/example/VersionOnly"),
            ),
        ],
    );
    let index = build_index(&root, 50, 64 * 1024);

    let described = index
        .describe_class(
            "org.example.Shape",
            Some("org.example:demo:1.0"),
            None,
            false,
        )
        .unwrap();
    assert_eq!(described.len(), 1);
    assert_eq!(
        described[0].super_class.as_deref(),
        Some("java.lang.Number")
    );
    assert!(
        described[0]
            .methods
            .iter()
            .any(|method| method.name == "late7")
    );

    let hierarchy = |parent: &str| {
        index
            .search_type_hierarchy(parent, false, Some("org.example:demo:1.0"), None)
            .len()
    };
    assert_eq!(hierarchy("java.lang.Number"), 1);
    assert_eq!(hierarchy("java.lang.Object"), 0);

    let members = |query: &str| {
        index
            .search_class_members(query, Some("org.example:demo:1.0"), None)
            .len()
    };
    assert_eq!(members("late7"), 2);
    assert_eq!(members("mid11"), 0);
    assert_eq!(members("greet"), 0);

    let outbound = index.search_class_references(
        "org.example.Shape",
        ReferenceDirection::Outbound,
        Some(ClassReferenceKind::Class),
        None,
        None,
        Some("org.example:demo:1.0"),
        None,
    );
    let targets = outbound
        .iter()
        .map(|reference| reference.target_owner.as_str())
        .collect::<Vec<_>>();
    assert!(targets.contains(&"java.lang.Number"));
    assert!(!targets.contains(&"java.lang.Object"));

    let diff = index
        .compare_artifact_api("org.example", "demo", "1.0", "2.0")
        .unwrap();
    assert!(diff.added_classes.is_empty() && diff.removed_classes.is_empty());
    assert_eq!(diff.changed_classes.len(), 1);
    assert_eq!(
        diff.changed_classes[0].previous_super_class.as_deref(),
        Some("java.lang.Number")
    );
    assert_eq!(
        diff.changed_classes[0].current_super_class.as_deref(),
        Some("java.lang.Object")
    );

    let listed = index.list_classes("org.example:demo:1.0", 0, None);
    assert_eq!(listed[0].classes, vec!["org.example.Shape"]);
}

#[test]
fn versioned_entries_without_manifest_flag_are_ignored_deterministically() {
    let root = TempDir::new().unwrap();
    let base = shape_variant(b"java/lang/Object", b"greet");
    let java17 = shape_variant(b"java/lang/Number", b"late7");
    write_jar(
        &root.path().join("org/example/legacy/1.0/legacy-1.0.jar"),
        &[
            ("org/example/Shape.class", &base),
            ("META-INF/versions/17/org/example/Shape.class", &java17),
            (
                "META-INF/versions/17/org/example/VersionOnly.class",
                &inspection_class("org/example/VersionOnly"),
            ),
        ],
    );
    let index = build_index(&root, 50, 64 * 1024);

    let described = index
        .describe_class("org.example.Shape", None, None, false)
        .unwrap();
    assert_eq!(
        described[0].super_class.as_deref(),
        Some("java.lang.Object")
    );
    assert_eq!(
        index
            .search_type_hierarchy("java.lang.Object", false, None, None)
            .len(),
        1
    );
    assert!(
        index
            .search_type_hierarchy("java.lang.Number", false, None, None)
            .is_empty()
    );
    assert!(
        index
            .describe_class("org.example.VersionOnly", None, None, false)
            .unwrap()
            .is_empty()
    );
    assert_eq!(index.search_jars("legacy", None)[0].class_count, 1);
}

#[test]
fn cr_only_multi_release_manifest_selects_the_highest_versioned_variant() {
    let root = TempDir::new().unwrap();
    write_jar(
        &root.path().join("org/example/demo/1.0/demo-1.0.jar"),
        &[
            (
                "META-INF/MANIFEST.MF",
                b"Manifest-Version: 1.0\rMulti-Release: true\r\r",
            ),
            (
                "org/example/Shape.class",
                &shape_variant(b"java/lang/Object", b"greet"),
            ),
            (
                "META-INF/versions/17/org/example/Shape.class",
                &shape_variant(b"java/lang/Number", b"late7"),
            ),
            (
                "META-INF/versions/17/org/example/VersionOnly.class",
                &inspection_class("org/example/VersionOnly"),
            ),
        ],
    );
    let index = build_index(&root, 50, 64 * 1024);

    let described = index
        .describe_class("org.example.Shape", None, None, false)
        .unwrap();
    assert_eq!(
        described[0].super_class.as_deref(),
        Some("java.lang.Number")
    );
    assert_eq!(
        index
            .search_type_hierarchy("java.lang.Number", false, None, None)
            .len(),
        1
    );
    assert_eq!(
        index
            .describe_class("org.example.VersionOnly", None, None, false)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn flagged_jar_without_base_entry_uses_highest_versioned_entry() {
    let root = TempDir::new().unwrap();
    write_jar(
        &root.path().join("org/example/demo/1.0/demo-1.0.jar"),
        &[
            MULTI_RELEASE_MANIFEST,
            (
                "META-INF/versions/11/org/example/Shape.class",
                &shape_variant(b"java/lang/Object", b"mid11"),
            ),
            (
                "META-INF/versions/17/org/example/Shape.class",
                &shape_variant(b"java/lang/Number", b"late7"),
            ),
            (
                "META-INF/versions/8/org/example/Old.class",
                &inspection_class("org/example/Old"),
            ),
        ],
    );
    let index = build_index(&root, 50, 64 * 1024);

    let described = index
        .describe_class("org.example.Shape", None, None, false)
        .unwrap();
    assert_eq!(
        described[0].super_class.as_deref(),
        Some("java.lang.Number")
    );
    assert_eq!(
        index
            .search_type_hierarchy("java.lang.Number", false, None, None)
            .len(),
        1
    );
    assert!(
        index
            .describe_class("org.example.Old", None, None, false)
            .unwrap()
            .is_empty()
    );
}

fn write_module_pom(directory: &Path, artifact_id: &str, version: &str) {
    std::fs::create_dir_all(directory).unwrap();
    std::fs::write(
        directory.join("pom.xml"),
        format!(
            "<project><parent><groupId>org.reactor</groupId><artifactId>parent</artifactId><version>{version}</version></parent><artifactId>{artifact_id}</artifactId></project>"
        ),
    )
    .unwrap();
}

fn write_class_directory(directory: &Path, entries: &[(&str, &[u8])]) {
    for (name, content) in entries {
        let path = directory.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
}

fn build_reactor_index(
    repository: Option<&Path>,
    root: &Path,
    modules: &[&str],
    classpath: &[PathBuf],
) -> ScopedIndex {
    let root = root.canonicalize().unwrap();
    let modules = modules
        .iter()
        .map(|module| root.join(module).canonicalize().unwrap())
        .collect::<Vec<_>>();
    MavenIndex::build_scoped(
        repository,
        &ReactorLayout {
            root: &root,
            modules: &modules,
        },
        classpath,
        50,
        1024,
    )
    .unwrap()
}

fn unindexed_reasons(scoped: &ScopedIndex) -> Vec<(String, UnindexedReason)> {
    scoped
        .unindexed
        .iter()
        .map(|entry| {
            (
                entry
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                entry.reason,
            )
        })
        .collect()
}

#[test]
fn scoped_index_contains_only_classpath_repository_and_reactor_outputs() {
    let project = TempDir::new().unwrap();
    std::fs::write(
        project.path().join("pom.xml"),
        "<project><groupId>org.reactor</groupId><artifactId>parent</artifactId><version>3.0</version></project>",
    )
    .unwrap();
    let repository = project.path().join("repo");
    let selected = repository.join("org/libs/selected/1.0/selected-1.0.jar");
    let other_version = repository.join("org/libs/selected/2.0/selected-2.0.jar");
    let unrelated = repository.join("org/libs/unrelated/1.0/unrelated-1.0.jar");
    write_jar(&selected, &[("org/libs/Selected.class", b"x")]);
    write_jar(&other_version, &[("org/libs/Selected.class", b"x")]);
    write_jar(&unrelated, &[("org/libs/Unrelated.class", b"x")]);
    write_jar(
        &repository.join("org/libs/selected/1.0/selected-1.0-sources.jar"),
        &[(
            "org/libs/Selected.java",
            b"package org.libs; class Selected {}",
        )],
    );
    let module = project.path().join("core");
    write_module_pom(&module, "core", "3.0");
    let reactor_jar = module.join("target/core-3.0.jar");
    write_jar(&reactor_jar, &[("org/reactor/Core.class", b"x")]);
    write_jar(
        &module.join("target/core-3.0-sources.jar"),
        &[(
            "org/reactor/Core.java",
            b"package org.reactor; class Core {}",
        )],
    );
    // A custom finalName archive is still the module's output.
    let final_name = module.join("target/app.jar");
    write_jar(&final_name, &[("org/reactor/Renamed.class", b"x")]);
    // A compiled but unpackaged module contributes its class directory.
    let api = project.path().join("api");
    write_module_pom(&api, "api", "3.0");
    let api_classes = api.join("target/classes");
    write_class_directory(
        &api_classes,
        &[
            ("org/reactor/Api.class", b"x"),
            (
                "META-INF/services/org.reactor.Spi",
                b"org.reactor.ApiProvider\n",
            ),
        ],
    );
    let outside = TempDir::new().unwrap();
    let foreign = outside.path().join("foreign/foreign/1.0/foreign-1.0.jar");
    write_jar(&foreign, &[("org/foreign/Foreign.class", b"x")]);
    // A JAR in the project but outside every module build directory is not an output.
    let library = project.path().join("lib/system.jar");
    write_jar(&library, &[("org/system/System.class", b"x")]);

    let classpath = [
        selected,
        reactor_jar,
        final_name,
        api_classes,
        module.join("target/missing.jar"),
        library,
        foreign,
    ]
    .map(|path| path.canonicalize().unwrap_or(path));
    let repository = repository.canonicalize().unwrap();
    let scoped = build_reactor_index(
        Some(&repository),
        project.path(),
        &[".", "core", "api"],
        &classpath,
    );

    assert_eq!(
        unindexed_reasons(&scoped),
        vec![
            ("missing.jar".to_owned(), UnindexedReason::Missing),
            (
                "system.jar".to_owned(),
                UnindexedReason::OutsideRepositoryAndReactor
            ),
            (
                "foreign-1.0.jar".to_owned(),
                UnindexedReason::OutsideRepositoryAndReactor
            ),
        ]
    );
    let index = scoped.index;
    let stats = index.stats();
    assert_eq!((stats.jar_count, stats.source_jar_count), (4, 2));
    for (query, expected) in [
        ("Selected", 1),
        ("Core", 1),
        ("Renamed", 1),
        ("org.reactor.Api", 1),
        ("Unrelated", 0),
        ("Foreign", 0),
        ("org.system", 0),
    ] {
        assert_eq!(index.search_classes(query, None).len(), expected, "{query}");
    }
    assert_eq!(
        index.artifact_versions("selected", Some("org.libs"))["org.libs:selected"],
        vec!["1.0".to_owned()]
    );
    let core = index
        .search_jars("core", None)
        .into_iter()
        .find(|jar| jar.path == "core/target/core-3.0.jar")
        .expect("the reactor JAR is indexed under its module coordinate");
    assert_eq!(core.coordinate, "org.reactor:core:3.0");
    assert!(
        index.search_jars("core", None).iter().any(
            |jar| jar.path == "core/target/app.jar" && jar.coordinate == "org.reactor:core:3.0"
        )
    );
    assert_eq!(
        index
            .class_source("org.reactor.Core", None, None)
            .unwrap()
            .len(),
        1
    );

    let classes = index
        .search_jars("api", None)
        .into_iter()
        .find(|jar| jar.path == "api/target/classes")
        .expect("the class directory is indexed as a reactor output");
    assert_eq!(classes.coordinate, "org.reactor:api:3.0");
    let entry = index
        .jar_entry("api/target/classes", "org/reactor/Api.class")
        .unwrap();
    assert_eq!(entry[0].text.as_deref(), Some("x"));
    for escape in [
        "../pom.xml",
        "../../pom.xml",
        "/etc/passwd",
        "org/../../pom.xml",
    ] {
        assert!(
            index
                .jar_entry("api/target/classes", escape)
                .unwrap()
                .is_empty(),
            "{escape}"
        );
    }
    let providers = index
        .search_jar_content("ApiProvider", Some("api/target/classes"), None)
        .unwrap();
    assert_eq!(providers.results.len(), 1);
}

#[test]
fn reactor_coordinates_resolve_ci_friendly_versions_and_class_directories() {
    let project = TempDir::new().unwrap();
    std::fs::write(
        project.path().join("pom.xml"),
        "<project><groupId>org.reactor</groupId><artifactId>parent</artifactId><version>${revision}</version><properties><revision>4.1</revision><suffix>-x</suffix></properties></project>",
    )
    .unwrap();
    let service = project.path().join("svc");
    write_module_pom(&service, "svc", "${revision}");
    let test_classes = service.join("target/test-classes");
    write_class_directory(&test_classes, &[("org/reactor/SvcTest.class", b"x")]);
    let unresolved = project.path().join("broken");
    write_module_pom(&unresolved, "broken", "${undefined}");
    let broken_jar = unresolved.join("target/broken.jar");
    write_jar(&broken_jar, &[("org/reactor/Broken.class", b"x")]);

    let scoped = build_reactor_index(
        None,
        project.path(),
        &[".", "svc", "broken"],
        &[test_classes.clone(), broken_jar.clone()],
    );
    assert_eq!(
        unindexed_reasons(&scoped),
        vec![(
            "broken.jar".to_owned(),
            UnindexedReason::UnresolvedReactorCoordinate
        )]
    );
    assert_eq!(
        scoped.index.search_jars("svc", None)[0].coordinate,
        "org.reactor:svc:4.1:tests"
    );

    // `.mvn/maven.config` user properties override POM properties, as on the command line.
    std::fs::create_dir_all(project.path().join(".mvn")).unwrap();
    std::fs::write(
        project.path().join(".mvn/maven.config"),
        "-B -Drevision=5.0${suffix}\n--define undefined=1\n",
    )
    .unwrap();
    let scoped = build_reactor_index(
        None,
        project.path(),
        &[".", "svc", "broken"],
        &[test_classes, broken_jar],
    );
    assert!(scoped.unindexed.is_empty());
    let mut coordinates = scoped
        .index
        .search_jars("org.reactor", None)
        .into_iter()
        .map(|jar| jar.coordinate)
        .collect::<Vec<_>>();
    coordinates.sort();
    assert_eq!(
        coordinates,
        ["org.reactor:broken:1", "org.reactor:svc:5.0-x:tests"]
    );
}

#[test]
fn scoped_index_without_a_repository_indexes_reactor_outputs_only() {
    let project = TempDir::new().unwrap();
    let module = project.path().join("api");
    write_module_pom(&module, "api", "1.5");
    let jar = module.join("target/api-1.5-tests.jar");
    write_jar(&jar, &[("org/reactor/ApiTest.class", b"x")]);

    let scoped = build_reactor_index(None, project.path(), &[".", "api"], &[jar]);

    assert!(scoped.unindexed.is_empty());
    assert_eq!(scoped.index.stats().jar_count, 1);
    assert_eq!(
        scoped.index.search_jars("api", None)[0].coordinate,
        "org.reactor:api:1.5:tests"
    );
}

#[test]
fn estimated_index_bytes_grow_with_indexed_content() {
    let empty = TempDir::new().unwrap();
    let (_root, small) = fixture();
    let (many_root, _) = fixture();
    for number in 0..20 {
        write_jar(
            &many_root
                .path()
                .join(format!("org/example/lib{number}/1.0/lib{number}-1.0.jar")),
            &[("org/example/Extra.class", b"bytecode")],
        );
    }
    let many = build_index(&many_root, 50, 1024);

    let empty = build_index(&empty, 50, 1024).estimated_bytes();
    assert!(empty > 0);
    assert!(small.estimated_bytes() > empty);
    assert!(many.estimated_bytes() > small.estimated_bytes());
}

#[test]
fn cancelled_scoped_builds_stop_without_producing_an_index() {
    let repository = TempDir::new().unwrap();
    let jar = repository.path().join("org/example/demo/1.0/demo-1.0.jar");
    write_jar(&jar, &[("org/example/Demo.class", b"not a class")]);
    let reactor_root = TempDir::new().unwrap();
    let reactor_root = reactor_root.path().canonicalize().unwrap();
    let build = || {
        MavenIndex::build_scoped(
            Some(repository.path()),
            &ReactorLayout {
                root: &reactor_root,
                modules: &[],
            },
            std::slice::from_ref(&jar),
            50,
            1024,
        )
    };

    let token = tokio_util::sync::CancellationToken::new();
    assert_eq!(
        crate::blocking::scope(&token, build)
            .unwrap()
            .index
            .stats()
            .jar_count,
        1,
        "an uncancelled job indexes the jar"
    );
    token.cancel();
    let error = crate::blocking::scope(&token, build).unwrap_err();
    assert!(error.is::<crate::blocking::Cancelled>(), "{error:#}");
}

#[test]
fn cancelled_searches_stop_at_their_entry_checkpoint() {
    let root = TempDir::new().unwrap();
    write_jar(
        &root.path().join("org/example/demo/1.0/demo-1.0.jar"),
        &[("config.properties", b"needle=1")],
    );
    write_jar(
        &root
            .path()
            .join("org/example/demo/1.0/demo-1.0-sources.jar"),
        &[("org/example/Demo.java", b"class Demo { /* needle */ }")],
    );
    let index = build_index(&root, 10, 1024);
    assert_eq!(
        index
            .search_jar_content("needle", None, None)
            .unwrap()
            .results
            .len(),
        1
    );
    assert_eq!(
        index
            .search_source("needle", false, None, 0, None)
            .unwrap()
            .results
            .len(),
        1
    );

    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    crate::blocking::scope(&token, || {
        for error in [
            index.search_jar_content("needle", None, None).unwrap_err(),
            index
                .search_source("needle", false, None, 0, None)
                .unwrap_err(),
        ] {
            assert!(error.is::<crate::blocking::Cancelled>(), "{error:#}");
        }
    });
}

fn archive_opens() -> usize {
    ARCHIVE_OPENS.with(std::cell::Cell::get)
}

/// Member matches computed class by class, each class through its own archive open.
fn reference_member_matches(index: &MavenIndex, query: &str) -> Vec<ClassMemberMatch> {
    let mut matches = Vec::new();
    for jar in &index.jars {
        for class_name in &jar.classes {
            let description = read_class_description(
                jar,
                index.summary(jar),
                class_name,
                index.max_source_bytes,
                false,
            )
            .unwrap();
            collect_member_matches(&description, query, &mut matches);
        }
    }
    matches.sort_by(|left, right| {
        (
            &left.jar.coordinate,
            &left.class_name,
            &left.kind,
            &left.name,
            &left.signature,
        )
            .cmp(&(
                &right.jar.coordinate,
                &right.class_name,
                &right.kind,
                &right.name,
                &right.signature,
            ))
    });
    matches
}

fn many_class_entries(count: usize) -> Vec<(String, Vec<u8>)> {
    (0..count)
        .map(|number| {
            let class_name = format!("org/example/Generated{number:03}");
            (format!("{class_name}.class"), inspection_class(&class_name))
        })
        .collect()
}

fn borrowed_entries(entries: &[(String, Vec<u8>)]) -> Vec<(&str, &[u8])> {
    entries
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect()
}

#[test]
fn member_search_and_api_comparison_open_each_jar_once_with_unchanged_results() {
    const CLASSES: usize = 40;
    let root = TempDir::new().unwrap();
    let generated = many_class_entries(CLASSES);
    let mut entries = borrowed_entries(&generated);
    write_jar(
        &root.path().join("org/example/demo/1.0/demo-1.0.jar"),
        &entries,
    );
    let shape = shape_variant(b"java/lang/Number", b"late7");
    entries.push(MULTI_RELEASE_MANIFEST);
    entries.push(("org/example/Shape.class", &shape));
    entries.push(("META-INF/versions/17/org/example/Shape.class", &shape));
    write_jar(
        &root.path().join("org/example/demo/2.0/demo-2.0.jar"),
        &entries,
    );
    write_jar(
        &root.path().join("org/example/empty/1.0/empty-1.0.jar"),
        &[("config.txt", b"no classes")],
    );
    write_jar(
        &root
            .path()
            .join("org/example/demo/1.0/demo-1.0-sources.jar"),
        &[("org/example/Generated000.java", b"class Generated000 {}")],
    );
    let index = build_index(&root, 10_000, 64 * 1024);

    let before = archive_opens();
    let found = index.search_class_members("greet", None, Some(10_000));
    // One open per binary JAR that has classes, not one per class.
    assert_eq!(archive_opens() - before, 2);
    assert!(found.len() >= 2 * CLASSES);
    assert_eq!(found, reference_member_matches(&index, "greet"));
    assert!(found.windows(2).all(|pair| {
        (&pair[0].jar.coordinate, &pair[0].class_name)
            <= (&pair[1].jar.coordinate, &pair[1].class_name)
    }));

    let before = archive_opens();
    index.search_class_members("value", Some("org.example:demo:1.0"), Some(5));
    assert_eq!(archive_opens() - before, 1);
    assert_eq!(index.search_class_members("late7", None, None).len(), 2);

    let before = archive_opens();
    let diff = index
        .compare_artifact_api("org.example", "demo", "1.0", "2.0")
        .unwrap();
    assert_eq!(archive_opens() - before, 2);
    assert_eq!(diff.added_classes, vec!["org.example.Shape"]);
    assert!(diff.removed_classes.is_empty());

    // Describing a class still opens only the archives that hold it.
    let before = archive_opens();
    index
        .describe_class("org.example.Generated007", None, None, false)
        .unwrap();
    assert_eq!(archive_opens() - before, 2);
}

#[test]
fn member_search_skips_bad_entries_and_keeps_reading_the_same_archive() {
    let root = TempDir::new().unwrap();
    let good = inspection_class("org/example/Good");
    let later = inspection_class("org/example/Later");
    let garbage = vec![0u8; 16];
    let oversized = vec![0u8; 4096];
    let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
    std::fs::create_dir_all(jar.parent().unwrap()).unwrap();
    let mut writer = ZipWriter::new(File::create(&jar).unwrap());
    let stored = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, bytes) in [
        ("org/example/Broken.class", garbage.as_slice()),
        ("org/example/Good.class", good.as_slice()),
        ("org/example/Huge.class", oversized.as_slice()),
        ("org/example/Later.class", later.as_slice()),
    ] {
        writer.start_file(name, stored).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap();
    // Flip one payload byte of `Good`: its CRC no longer matches, so reading it fails.
    let mut raw = std::fs::read(&jar).unwrap();
    let offset = raw
        .windows(good.len())
        .position(|window| window == good.as_slice())
        .unwrap();
    raw[offset + good.len() - 1] ^= 0xff;
    std::fs::write(&jar, raw).unwrap();
    let index = build_index(&root, 10_000, good.len() + 64);

    let before = archive_opens();
    let found = index.search_class_members("greet", None, None);
    assert_eq!(archive_opens() - before, 1);
    let classes = found
        .iter()
        .map(|item| item.class_name.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(classes, BTreeSet::from(["org.example.Later"]));
    assert_eq!(
        found,
        reference_member_matches_skipping_errors(&index, "greet")
    );

    let error = index
        .compare_artifact_api("org.example", "demo", "1.0", "1.0")
        .expect_err("an unreadable class must stay an error");
    assert!(
        error.to_string().contains("cannot parse classfile")
            || error.to_string().contains("MAX_SOURCE_BYTES")
            || error.to_string().contains("checksum"),
        "{error}"
    );
}

/// Like [`reference_member_matches`], but skips classes that cannot be read.
fn reference_member_matches_skipping_errors(
    index: &MavenIndex,
    query: &str,
) -> Vec<ClassMemberMatch> {
    let mut matches = Vec::new();
    for jar in &index.jars {
        for class_name in &jar.classes {
            if let Ok(description) = read_class_description(
                jar,
                index.summary(jar),
                class_name,
                index.max_source_bytes,
                false,
            ) {
                collect_member_matches(&description, query, &mut matches);
            }
        }
    }
    matches.sort_by(|left, right| {
        (&left.class_name, &left.kind, &left.name).cmp(&(
            &right.class_name,
            &right.kind,
            &right.name,
        ))
    });
    matches
}

#[test]
fn unopenable_archive_is_skipped_by_member_search_and_fails_the_api_comparison() {
    let root = TempDir::new().unwrap();
    let class = inspection_class("org/example/Good");
    for version in ["1.0", "2.0"] {
        write_jar(
            &root
                .path()
                .join(format!("org/example/demo/{version}/demo-{version}.jar")),
            &[("org/example/Good.class", &class)],
        );
    }
    let index = build_index(&root, 100, 64 * 1024);
    std::fs::write(
        root.path().join("org/example/demo/1.0/demo-1.0.jar"),
        b"not a zip anymore",
    )
    .unwrap();

    let found = index.search_class_members("greet", None, None);
    assert_eq!(found.len(), 2);
    assert!(
        found
            .iter()
            .all(|item| item.jar.coordinate == "org.example:demo:2.0")
    );
    let error = index
        .compare_artifact_api("org.example", "demo", "1.0", "2.0")
        .expect_err("an unopenable archive must stay an error");
    assert!(error.to_string().contains("invalid ZIP/JAR"), "{error:#}");
}
