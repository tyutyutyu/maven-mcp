#!/usr/bin/env bash
# Native versioned releases of maven-mcp (README.md, "Releases"). The [package] version in
# Cargo.toml is the only version source: Cargo.lock, the CHANGELOG.md section, the release
# tag, the archive name, `maven-mcp --version` and the MCP serverInfo.version all derive
# from it. Every package and archive manifest is checked against the release privacy rule:
# no private workspace data, test logs or evaluation output, or credential files.
# This script never tags, uploads or publishes; the Release workflow does that.
set -euo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd -P)"
readonly BINARY_NAME="maven-mcp"
readonly SUPPORTED_TARGETS="x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu"
readonly SEMVER_PATTERN='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'
readonly ARCHIVE_PATTERN='^maven-mcp-(.+)-(x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu)\.tar\.gz$'
# Files of a release archive besides the binary, all from the repository root.
readonly ARCHIVE_DOCUMENTS="LICENSE README.md CHANGELOG.md"
# Files the crates.io package must contain.
readonly PACKAGE_REQUIRED_FILES="Cargo.toml Cargo.lock LICENSE README.md CHANGELOG.md src/lib.rs src/main.rs"
# Cargo target directory of release builds, kept apart from the remap-free default builds.
readonly RELEASE_TARGET_DIR="target/release-artifacts"

# Scratch directory of archive and verify-archive, removed on exit.
WORK_DIRECTORY=""

remove_work_directory() {
  [[ -z "${WORK_DIRECTORY}" ]] || rm -rf "${WORK_DIRECTORY}"
}
trap remove_work_directory EXIT

# Creates the scratch directory once; call it outside command substitutions.
require_work_directory() {
  [[ -n "${WORK_DIRECTORY}" ]] || WORK_DIRECTORY="$(mktemp -d)"
}

usage() {
  cat >&2 <<'USAGE'
Usage: scripts/release.sh <command>
  check [--tag <tag>]      check that Cargo.toml, Cargo.lock and CHANGELOG.md agree on the
                           version and that the crates.io package manifest is public-only;
                           with --tag also require tag v<version> and its dated changelog section
  notes <version>          print the CHANGELOG.md section of <version> (the release notes)
  archive <target> <dir>   build the clean HEAD reproducibly and write
                           maven-mcp-<version>-<target>.tar.gz and its .sha256 into <dir>
  verify-archive <archive> verify the checksum, the exact entry list and the binary version
  check-manifest           read package or archive paths from stdin and reject private ones
USAGE
}

fail() {
  echo "release: $*" >&2
  exit 1
}

# The [package] value of <key> in Cargo.toml.
package_field() {
  sed -n "/^\[package\]/,/^\[/ s/^$1 = \"\(.*\)\"\$/\1/p" "${PROJECT_ROOT}/Cargo.toml" | head -n 1
}

# The version Cargo.lock records for the package itself.
locked_version() {
  awk -v name="name = \"$1\"" '
    $0 == name { getline; if (sub(/^version = "/, "")) { sub(/"$/, ""); print; exit } }
  ' "${PROJECT_ROOT}/Cargo.lock"
}

require_semver() {
  [[ "$1" =~ ${SEMVER_PATTERN} ]] || fail "'$1' is not a SemVer version without build metadata"
}

# Prints the `## [<name>]...` heading line of CHANGELOG.md, if present.
changelog_heading() {
  awk -v heading="## [$1]" '
    $0 == heading || index($0, heading " - ") == 1 { print; exit }
  ' "${PROJECT_ROOT}/CHANGELOG.md"
}

# Prints the body of the `## [<version>]` section of CHANGELOG.md (empty when missing).
changelog_section() {
  awk -v heading="## [$1]" '
    index($0, "## ") == 1 { in_section = ($0 == heading || index($0, heading " - ") == 1); next }
    in_section { print }
  ' "${PROJECT_ROOT}/CHANGELOG.md"
}

has_text() {
  grep -q '[^[:space:]]' <<<"$1"
}

# Prints why a package or archive path is private, or nothing when it may be published.
private_path_reason() {
  local path="/${1#./}"
  case "${path}" in
    */.env.example) ;;
    */.backlog/* | */.brv/* | */docs/decisions/* | */docs/dogfooding/* | */docs/testing.md)
      echo "private workspace data" ;;
    */.claude/* | */.codex/* | */CLAUDE.local.md)
      echo "local agent configuration" ;;
    */target/* | */node_modules/* | */mcp-test-report/* | */coverage/* | */promptfoo/results.json | \
      */promptfoo/report.html | *benchmark-results*.json | *.profraw | *.log | *.log.[0-9]*)
      echo "build, test log or evaluation output" ;;
    */.env | */.env.* | */.npmrc | */.netrc | */.pypirc | */.git-credentials | */credentials | \
      */credentials.toml | */settings-security.xml | *.pem | *.key | *.p12 | *.pfx | \
      */id_rsa* | */id_ecdsa* | */id_ed25519*)
      echo "credential file" ;;
  esac
}

check_manifest() {
  local path reason rejected=false count=0
  while IFS= read -r path; do
    [[ -n "${path}" ]] || continue
    count=$((count + 1))
    reason="$(private_path_reason "${path}")"
    if [[ -n "${reason}" ]]; then
      echo "release: ${path} must not be published (${reason})" >&2
      rejected=true
    fi
  done
  [[ "${count}" -gt 0 ]] || fail "the manifest is empty"
  [[ "${rejected}" == false ]] || fail "the manifest contains private files"
}

check_package_manifest() {
  local listing file
  listing="$(cd "${PROJECT_ROOT}" && cargo package --list --locked --allow-dirty)" ||
    fail "cargo package --list failed"
  printf '%s\n' "${listing}" | check_manifest
  for file in ${PACKAGE_REQUIRED_FILES}; do
    grep -F -x -q -e "${file}" <<<"${listing}" || fail "the crates.io package misses ${file}"
  done
}

check_release() {
  local tag="" version name locked heading
  case "$#" in
    0) ;;
    2) [[ "$1" == --tag ]] || { usage; exit 2; }; tag="$2" ;;
    *) usage; exit 2 ;;
  esac
  name="$(package_field name)"
  version="$(package_field version)"
  [[ "${name}" == "${BINARY_NAME}" ]] || fail "the Cargo.toml package name is '${name}', not ${BINARY_NAME}"
  require_semver "${version}"
  locked="$(locked_version "${name}")"
  [[ "${locked}" == "${version}" ]] ||
    fail "Cargo.lock records ${name} ${locked:-<missing>}, Cargo.toml ${version}; run cargo update --workspace"
  [[ "$(changelog_heading Unreleased)" == "## [Unreleased]" ]] ||
    fail "CHANGELOG.md has no '## [Unreleased]' section"
  if [[ -n "${tag}" ]]; then
    [[ "${tag}" == "v${version}" ]] ||
      fail "tag ${tag} does not match the Cargo.toml version ${version} (expected v${version})"
    heading="$(changelog_heading "${version}")"
    [[ "${heading#"## [${version}] - "}" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] ||
      fail "CHANGELOG.md needs a '## [${version}] - YYYY-MM-DD' section"
    has_text "$(changelog_section "${version}")" || fail "the CHANGELOG.md section of ${version} is empty"
  fi
  check_package_manifest
  echo "Release metadata of ${name} ${version} is consistent${tag:+ with tag ${tag}}."
}

release_notes() {
  [[ "$#" -eq 1 ]] || { usage; exit 2; }
  local notes
  require_semver "$1"
  notes="$(changelog_section "$1")"
  has_text "${notes}" || fail "CHANGELOG.md has no non-empty section for $1"
  # Drop the blank lines around the section body.
  printf '%s\n' "${notes}" | sed -e '/[^[:space:]]/,$!d' | tac | sed -e '/[^[:space:]]/,$!d' | tac
}

# Prints the executable path of the maven-mcp binary artifact from Cargo's JSON messages
# (the same rule as scripts/local-install.sh), so a configured target directory or build
# target cannot substitute a stale binary.
built_executable() {
  local messages="$1" artifacts
  artifacts="$(printf '%s\n' "${messages}" |
    grep -F '"reason":"compiler-artifact"' |
    grep -F '"kind":["bin"]' |
    grep -F "\"name\":\"${BINARY_NAME}\"" |
    grep -o '"executable":"[^"\\]*"' |
    sed 's/^"executable":"//; s/"$//')" || true
  [[ -n "${artifacts}" && "${artifacts}" != *$'\n'* ]] ||
    fail "cannot find the ${BINARY_NAME} executable in Cargo's build output"
  printf '%s\n' "${artifacts}"
}

require_clean_worktree() {
  [[ -z "$(git -C "${PROJECT_ROOT}" status --porcelain)" ]] ||
    fail "the worktree has uncommitted changes; release archives are built from a clean commit"
}

# Builds the release binary with the HEAD commit embedded. Paths are remapped and the
# timestamp is the commit time, so the same commit and pinned toolchain give the same bytes
# in any checkout location.
build_release_binary() {
  local commit="$1" epoch="$2" cargo_home="${CARGO_HOME:-${HOME}/.cargo}" messages
  messages="$(cd "${PROJECT_ROOT}" &&
    env -u CARGO_ENCODED_RUSTFLAGS -u CARGO_BUILD_RUSTFLAGS \
      SOURCE_DATE_EPOCH="${epoch}" \
      MAVEN_MCP_BUILD_COMMIT="${commit}" \
      CARGO_TARGET_DIR="${PROJECT_ROOT}/${RELEASE_TARGET_DIR}" \
      RUSTFLAGS="--remap-path-prefix=${cargo_home}=/cargo --remap-path-prefix=${PROJECT_ROOT}=/maven-mcp" \
      cargo build --release --locked --bin "${BINARY_NAME}" --message-format=json-render-diagnostics)" ||
    fail "the release build failed"
  built_executable "${messages}"
}

# Writes <dir>/<name>.tar.gz from the staged <name>/ directory: sorted entries, fixed
# owner, mode and mtime, and a gzip header without file name or time.
write_archive() {
  local staging="$1" name="$2" output="$3" epoch="$4"
  tar --create --format=gnu --sort=name --mtime="@${epoch}" --owner=0 --group=0 --numeric-owner \
    --mode='u+rwX,go+rX,go-w' --directory="${staging}" "${name}" | gzip -9 -n >"${output}/${name}.tar.gz"
  (cd "${output}" && sha256sum "${name}.tar.gz" >"${name}.tar.gz.sha256")
}

build_archive() {
  [[ "$#" -eq 2 ]] || { usage; exit 2; }
  local target="$1" output="$2" host version commit epoch binary name staging document
  [[ " ${SUPPORTED_TARGETS} " == *" ${target} "* ]] ||
    fail "unsupported target '${target}'; supported: ${SUPPORTED_TARGETS}"
  command -v git >/dev/null 2>&1 || fail "git is required"
  host="$(rustc -vV | sed -n 's/^host: //p')"
  [[ "${host}" == "${target}" ]] || fail "release archives are built natively; this host is ${host}"
  require_clean_worktree
  check_release
  version="$(package_field version)"
  commit="$(git -C "${PROJECT_ROOT}" rev-parse HEAD)"
  epoch="$(git -C "${PROJECT_ROOT}" log -1 --format=%ct HEAD)"
  binary="$(build_release_binary "${commit}" "${epoch}")"
  [[ "$(git -C "${PROJECT_ROOT}" rev-parse HEAD)" == "${commit}" ]] || fail "HEAD changed during the build"
  require_clean_worktree
  mkdir -p "${output}"
  "${PROJECT_ROOT}/scripts/check-linux-binary.sh" "${binary}"
  [[ "$("${binary}" --version)" == "${BINARY_NAME} ${version} (commit ${commit})" ]] ||
    fail "the built binary does not report ${BINARY_NAME} ${version} (commit ${commit})"

  name="${BINARY_NAME}-${version}-${target}"
  require_work_directory
  staging="${WORK_DIRECTORY}/staging"
  mkdir -p "${staging}/${name}"
  install -m 0755 "${binary}" "${staging}/${name}/${BINARY_NAME}"
  for document in ${ARCHIVE_DOCUMENTS}; do
    install -m 0644 "${PROJECT_ROOT}/${document}" "${staging}/${name}/${document}"
  done
  write_archive "${staging}" "${name}" "${output}" "${epoch}"
  verify_archive "${output}/${name}.tar.gz"
}

# The exact, sorted entry list of the archive <name>.tar.gz.
expected_entries() {
  local name="$1" document
  {
    printf '%s/\n%s/%s\n' "${name}" "${name}" "${BINARY_NAME}"
    for document in ${ARCHIVE_DOCUMENTS}; do
      printf '%s/%s\n' "${name}" "${document}"
    done
  } | LC_ALL=C sort
}

verify_archive() {
  [[ "$#" -eq 1 ]] || { usage; exit 2; }
  local archive="$1" directory base version target name expected actual extracted file reported
  [[ -f "${archive}" ]] || fail "${archive} is not a file"
  directory="$(cd "$(dirname "${archive}")" && pwd -P)"
  base="$(basename "${archive}")"
  [[ "${base}" =~ ${ARCHIVE_PATTERN} ]] || fail "${base} is not named maven-mcp-<version>-<target>.tar.gz"
  version="${BASH_REMATCH[1]}"
  target="${BASH_REMATCH[2]}"
  require_semver "${version}"
  name="${BINARY_NAME}-${version}-${target}"
  [[ -f "${directory}/${base}.sha256" ]] || fail "${base}.sha256 is missing"
  [[ "$(awk '{ print $2 }' "${directory}/${base}.sha256")" == "${base}" ]] ||
    fail "${base}.sha256 does not name ${base}"
  (cd "${directory}" && sha256sum --check --strict --quiet "${base}.sha256") ||
    fail "${base} does not match its SHA-256 checksum"

  actual="$(tar --list --gzip --file="${archive}" | LC_ALL=C sort)"
  printf '%s\n' "${actual}" | check_manifest
  expected="$(expected_entries "${name}")"
  [[ "${actual}" == "${expected}" ]] ||
    fail "${base} must contain exactly: $(tr '\n' ' ' <<<"${expected}")"

  require_work_directory
  extracted="$(mktemp -d -p "${WORK_DIRECTORY}")"
  tar --extract --gzip --no-same-owner --file="${archive}" --directory="${extracted}"
  for file in "${BINARY_NAME}" ${ARCHIVE_DOCUMENTS}; do
    [[ -f "${extracted}/${name}/${file}" && ! -L "${extracted}/${name}/${file}" ]] ||
      fail "${name}/${file} is not a regular file"
  done
  reported="$("${extracted}/${name}/${BINARY_NAME}" --version)" || fail "cannot run the archived binary"
  [[ "${reported}" =~ ^${BINARY_NAME}\ ${version//./\\.}\ \(commit\ [0-9a-f]{40}\)$ ]] ||
    fail "the archived binary reports '${reported}', not ${BINARY_NAME} ${version} with a commit"
  echo "Verified ${base}: ${reported}"
}

main() {
  local command="${1:-}"
  [[ "$#" -gt 0 ]] && shift
  case "${command}" in
    check) check_release "$@" ;;
    notes) release_notes "$@" ;;
    archive) build_archive "$@" ;;
    verify-archive) verify_archive "$@" ;;
    check-manifest)
      [[ "$#" -eq 0 ]] || { usage; exit 2; }
      check_manifest ;;
    *) usage; exit 2 ;;
  esac
}

main "$@"
