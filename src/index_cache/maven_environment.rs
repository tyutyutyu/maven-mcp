//! The part of the inherited environment that can change Maven's classpath resolution.
//!
//! MCP hosts pass per-session variables (session IDs, sockets, the working directory) to
//! every server start, so a digest of the whole environment would change on every host
//! restart. Only variables Maven, its launcher scripts, the Maven Wrapper, the JVM, jenv or
//! the proxy configuration read take part, plus every variable the build configuration
//! references explicitly: `${env.NAME}` (or a `env.NAME` profile activation) in the reactor
//! POMs, `.mvn/maven.config`,
//! `.mvn/jvm.config` and the user and installation `settings.xml`/`toolchains.xml`, and
//! `$NAME`/`${NAME}` in the shell files the launcher reads (`mavenrc`, `.mvn/jvm.config`).
//! A file that exists but cannot be scanned selects every variable but the server-only ones,
//! so an unknown reference can never reuse a stale classpath. The parent, imported BOM and
//! dependency POMs outside the reactor are known only after Maven resolved the classpath;
//! their references are stored with the classpath manifest and checked by
//! [`referenced_digest`] instead.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::File,
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
};

use super::{FNV_OFFSET, fnv1a};
use crate::config::SERVER_ONLY_ENVIRONMENT;

/// Variables read by the Maven launcher, the JVM and the Java selection.
const MAVEN_ENVIRONMENT_NAMES: [&str; 6] = [
    "HOME",
    "JAVA_HOME",
    "JAVA_TOOL_OPTIONS",
    "JDK_JAVA_OPTIONS",
    "M2_HOME",
    "PATH",
];
/// Variable families of the Maven launcher (`MAVEN_OPTS`, `MAVEN_ARGS`, `MAVEN_USER_HOME`,
/// `MAVEN_CONFIG`, `MAVEN_PROJECTBASEDIR`, `MAVEN_HOME`, `MAVEN_SKIP_RC`, ...), the Maven
/// Wrapper (`MVNW_*`) and jenv (`JENV_ROOT`, `JENV_VERSION`, ...).
const MAVEN_ENVIRONMENT_PREFIXES: [&str; 3] = ["MAVEN_", "MVNW_", "JENV_"];
/// Proxy variables (`HTTP_PROXY`, `https_proxy`, `NO_PROXY`, ...).
const PROXY_SUFFIXES: [&str; 2] = ["_PROXY", "_proxy"];
/// Upper bound of one scanned configuration file; a larger file selects every variable.
const MAX_SCANNED_FILE_BYTES: u64 = 16 << 20;
const ENV_PROPERTY_PREFIX: &[u8] = b"env.";
/// Upper bound of one XML character or entity reference between `&` and `;`.
const MAX_XML_REFERENCE_BYTES: usize = 32;

/// Files whose variable references take part in the classpath identity.
pub(super) struct ReferenceFiles {
    /// Files read through Maven interpolation, scanned for `env.NAME` references.
    interpolated: Vec<PathBuf>,
    /// Shell files the launcher reads, scanned for `$NAME` and `${NAME}`.
    shell: Vec<PathBuf>,
}

impl ReferenceFiles {
    /// The configuration files of `root`, of `home` and of the installation `conf`
    /// directories (`installation` as returned by the runner) that can reference a variable.
    pub(super) fn of_project(
        root: &Path,
        reactor_poms: &[PathBuf],
        installation: &[PathBuf],
        home: Option<&Path>,
    ) -> Self {
        let jvm_config = root.join(".mvn/jvm.config");
        let configuration_directories = home.map(|home| home.join(".m2")).into_iter().chain(
            installation
                .iter()
                .filter(|directory| directory.ends_with("conf"))
                .cloned(),
        );
        let interpolated = reactor_poms
            .iter()
            .cloned()
            .chain([root.join(".mvn/maven.config"), jvm_config.clone()])
            .chain(configuration_directories.flat_map(|directory| {
                [
                    directory.join("settings.xml"),
                    directory.join("toolchains.xml"),
                ]
            }))
            .collect();
        let shell = [PathBuf::from("/etc/mavenrc"), jvm_config]
            .into_iter()
            .chain(home.map(|home| home.join(".mavenrc")))
            .collect();
        Self {
            interpolated,
            shell,
        }
    }

    /// Every variable name the files reference; `None` when an existing file cannot be read
    /// within [`MAX_SCANNED_FILE_BYTES`].
    pub(super) fn referenced_names(&self) -> Option<BTreeSet<String>> {
        let mut names = BTreeSet::new();
        for path in &self.interpolated {
            names.extend(env_references(&scanned_content(path)?)?);
        }
        for path in &self.shell {
            names.extend(shell_references(&scanned_content(path)?));
        }
        Some(names)
    }
}

/// The bytes of `path`, empty when it does not exist; `None` when it cannot be read whole.
fn scanned_content(path: &Path) -> Option<Vec<u8>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None,
    };
    let mut content = Vec::new();
    file.take(MAX_SCANNED_FILE_BYTES + 1)
        .read_to_end(&mut content)
        .ok()?;
    (u64::try_from(content.len()).ok()? <= MAX_SCANNED_FILE_BYTES).then_some(content)
}

/// Names of every `env.NAME` property reference in `text`, read both literally and with XML
/// character and predefined entity references decoded, because an XML parser hands Maven
/// `${env.A&#95;B}` as `${env.A_B}`. `None` when `text` uses another named entity, whose
/// value is unknown.
pub(super) fn env_references(text: &[u8]) -> Option<BTreeSet<String>> {
    let decoded = decode_xml_references(text)?;
    Some(
        literal_env_references(text)
            .into_iter()
            .chain(literal_env_references(&decoded))
            .collect(),
    )
}

/// Names of every literal `env.NAME` property reference in `text`: `${env.NAME}`
/// expressions and `<name>env.NAME</name>` profile activations.
fn literal_env_references(text: &[u8]) -> BTreeSet<String> {
    text.windows(ENV_PROPERTY_PREFIX.len())
        .enumerate()
        .filter(|(index, window)| {
            *window == ENV_PROPERTY_PREFIX && (*index == 0 || !is_property_byte(text[index - 1]))
        })
        .filter_map(|(index, _)| property_name(&text[index + ENV_PROPERTY_PREFIX.len()..]))
        .collect()
}

/// One `&...;` sequence of a scanned file.
enum XmlReference {
    Character(char),
    /// A named entity other than the predefined ones, declared by a DTD.
    UnknownEntity,
    /// Not a reference; the `&` is kept as it is.
    Literal,
}

/// `text` with every character reference and predefined entity replaced by its character;
/// `None` when it uses an unknown named entity.
fn decode_xml_references(text: &[u8]) -> Option<Vec<u8>> {
    let mut decoded = Vec::with_capacity(text.len());
    let mut rest = text;
    while let Some(ampersand) = rest.iter().position(|byte| *byte == b'&') {
        decoded.extend_from_slice(&rest[..ampersand]);
        rest = &rest[ampersand..];
        let end = rest
            .iter()
            .take(MAX_XML_REFERENCE_BYTES)
            .position(|byte| *byte == b';');
        match end.map(|end| (end, xml_reference(&rest[1..end]))) {
            Some((end, XmlReference::Character(character))) => {
                decoded.extend_from_slice(character.encode_utf8(&mut [0; 4]).as_bytes());
                rest = &rest[end + 1..];
            }
            Some((_, XmlReference::UnknownEntity)) => return None,
            Some((_, XmlReference::Literal)) | None => {
                decoded.push(b'&');
                rest = &rest[1..];
            }
        }
    }
    decoded.extend_from_slice(rest);
    Some(decoded)
}

/// The meaning of the `body` between `&` and `;`.
fn xml_reference(body: &[u8]) -> XmlReference {
    let Ok(body) = std::str::from_utf8(body) else {
        return XmlReference::Literal;
    };
    let code = if let Some(hexadecimal) = body.strip_prefix("#x") {
        u32::from_str_radix(hexadecimal, 16).ok()
    } else if let Some(decimal) = body.strip_prefix('#') {
        decimal.parse().ok()
    } else {
        return match body {
            "lt" => XmlReference::Character('<'),
            "gt" => XmlReference::Character('>'),
            "amp" => XmlReference::Character('&'),
            "quot" => XmlReference::Character('"'),
            "apos" => XmlReference::Character('\''),
            name if is_entity_name(name) => XmlReference::UnknownEntity,
            _ => XmlReference::Literal,
        };
    };
    code.and_then(char::from_u32)
        .map_or(XmlReference::Literal, XmlReference::Character)
}

fn is_entity_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}

fn is_property_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-')
}

/// The property name at the start of `text`, without trailing punctuation.
fn property_name(text: &[u8]) -> Option<String> {
    let length = text
        .iter()
        .take_while(|byte| is_property_byte(**byte))
        .count();
    let name = std::str::from_utf8(&text[..length])
        .ok()?
        .trim_end_matches(['.', '-']);
    (!name.is_empty()).then(|| name.to_owned())
}

/// Names of every `$NAME` and `${NAME...}` shell parameter reference in `text`.
fn shell_references(text: &[u8]) -> BTreeSet<String> {
    text.iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'$')
        .filter_map(|(index, _)| {
            let after = &text[index + 1..];
            let name_start = usize::from(after.first() == Some(&b'{'));
            shell_name(&after[name_start..])
        })
        .collect()
}

/// The shell variable name at the start of `text`: a letter or `_`, then letters, digits or
/// `_`.
fn shell_name(text: &[u8]) -> Option<String> {
    let first = text.first()?;
    if !(first.is_ascii_alphabetic() || *first == b'_') {
        return None;
    }
    let length = text
        .iter()
        .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
        .count();
    std::str::from_utf8(&text[..length]).ok().map(str::to_owned)
}

/// Whether `name` can change Maven's classpath resolution. `referenced` is `None` when the
/// references could not be determined, so every variable Maven inherits takes part.
fn takes_part(name: &str, referenced: Option<&BTreeSet<String>>) -> bool {
    if SERVER_ONLY_ENVIRONMENT.contains(&name) {
        return false;
    }
    let Some(referenced) = referenced else {
        return true;
    };
    MAVEN_ENVIRONMENT_NAMES.contains(&name)
        || MAVEN_ENVIRONMENT_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
        || PROXY_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
        || referenced.contains(name)
}

/// Order-independent digest of the variables that take part: first their ordered names,
/// then every name and value, so adding, removing or changing one changes the digest. A
/// name that is not UTF-8 cannot be selected by name and takes part only when the
/// references are unknown.
pub(super) fn environment_digest(
    variables: impl IntoIterator<Item = (OsString, OsString)>,
    referenced: Option<&BTreeSet<String>>,
) -> u64 {
    selection_digest(
        variables
            .into_iter()
            .filter(|(name, _)| match name.to_str() {
                Some(name) => takes_part(name, referenced),
                None => referenced.is_none(),
            })
            .collect(),
    )
}

/// Digest, like [`environment_digest`], of only the variables named in `referenced` (the
/// server-only ones never take part).
pub(super) fn referenced_digest(
    variables: impl IntoIterator<Item = (OsString, OsString)>,
    referenced: &BTreeSet<String>,
) -> u64 {
    selection_digest(
        variables
            .into_iter()
            .filter(|(name, _)| {
                name.to_str().is_some_and(|name| {
                    referenced.contains(name) && !SERVER_ONLY_ENVIRONMENT.contains(&name)
                })
            })
            .collect(),
    )
}

fn selection_digest(selected: BTreeMap<OsString, OsString>) -> u64 {
    let names = selected.keys().fold(FNV_OFFSET, |hash, name| {
        fnv1a(hash, name.as_encoded_bytes())
    });
    selected
        .iter()
        .fold(fnv1a(names, &[]), |hash, (name, value)| {
            fnv1a(
                fnv1a(hash, name.as_encoded_bytes()),
                value.as_encoded_bytes(),
            )
        })
}

#[cfg(test)]
#[path = "../../tests/unit/index_cache_maven_environment.rs"]
mod tests;
