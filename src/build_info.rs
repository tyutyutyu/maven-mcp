//! Build identity of the running binary: the package version and, for builds made by the
//! local installation flow, the source commit embedded through `MAVEN_MCP_BUILD_COMMIT`.

use std::sync::LazyLock;

/// The Cargo package name, also the MCP `serverInfo.name`.
pub const PACKAGE_NAME: &str = env!("CARGO_PKG_NAME");

/// The Cargo package version: `Cargo.toml` is the only version source.
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Length of a full hexadecimal Git (SHA-1) commit id.
const COMMIT_ID_LENGTH: usize = 40;

/// Length of the commit prefix used in SemVer build metadata and installation identifiers.
const SHORT_COMMIT_LENGTH: usize = 12;

/// Label used when the binary was built without an embedded commit.
const UNKNOWN_COMMIT: &str = "unknown";

const EMBEDDED_COMMIT: Option<&str> = option_env!("MAVEN_MCP_BUILD_COMMIT");

// A malformed commit id fails the build instead of producing an unidentifiable binary.
const _: () = assert!(
    match EMBEDDED_COMMIT {
        Some(commit) => is_commit_id(commit),
        None => true,
    },
    "MAVEN_MCP_BUILD_COMMIT must be a 40-character lowercase hexadecimal commit id"
);

static VERSION_LABEL: LazyLock<String> =
    LazyLock::new(|| version_label_for(PACKAGE_VERSION, EMBEDDED_COMMIT));

/// The full source commit embedded at build time, if any.
pub fn build_commit() -> Option<&'static str> {
    EMBEDDED_COMMIT
}

/// The MCP `serverInfo.version`: the package version with the short commit as SemVer build
/// metadata when a commit is embedded, for example `0.1.0+0123456789ab`.
pub fn server_version() -> String {
    server_version_for(PACKAGE_VERSION, EMBEDDED_COMMIT)
}

/// The `--version` text after the binary name, for example `0.1.0 (commit <full id>)`.
pub fn version_label() -> &'static str {
    VERSION_LABEL.as_str()
}

/// Whether `value` is a full lowercase hexadecimal commit id.
pub const fn is_commit_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != COMMIT_ID_LENGTH {
        return false;
    }
    let mut position = 0;
    while position < bytes.len() {
        if !matches!(bytes[position], b'0'..=b'9' | b'a'..=b'f') {
            return false;
        }
        position += 1;
    }
    true
}

fn server_version_for(version: &str, commit: Option<&str>) -> String {
    match commit {
        Some(commit) => format!("{version}+{}", short_commit(commit)),
        None => version.to_owned(),
    }
}

fn version_label_for(version: &str, commit: Option<&str>) -> String {
    format!("{version} (commit {})", commit.unwrap_or(UNKNOWN_COMMIT))
}

fn short_commit(commit: &str) -> &str {
    commit.get(..SHORT_COMMIT_LENGTH).unwrap_or(commit)
}

#[cfg(test)]
#[path = "../tests/unit/build_info.rs"]
mod tests;
