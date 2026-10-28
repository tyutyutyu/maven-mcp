#!/usr/bin/env bash
# Versioned native local installation of maven-mcp with explicit candidate and stable
# aliases. MCP hosts start <root>/stable/maven-mcp or <root>/candidate/maven-mcp over
# STDIO; this script never starts or stops a server.
#
# Layout below the install root (MAVEN_MCP_INSTALL_ROOT, default
# ${XDG_DATA_HOME:-$HOME/.local/share}/maven-mcp):
#   versions/<id>/maven-mcp   immutable copy of one identified release binary
#   versions/<id>/BUILD-INFO  id, version, commit, verified flag and SHA-256 of that copy
#   candidate, stable, previous  symlinks to versions/<id>
# <id> is the MCP serverInfo.version of the binary: <package version>+<12-hex commit>.
set -euo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd -P)"
readonly BINARY_NAME="maven-mcp"
readonly BUILD_INFO_NAME="BUILD-INFO"
readonly SHORT_COMMIT_LENGTH=12
readonly VERSION_PATTERN='^maven-mcp ([0-9A-Za-z.-]+) \(commit ([0-9a-f]{40})\)$'

usage() {
  cat >&2 <<'USAGE'
Usage: scripts/local-install.sh <command>
  candidate         verify the clean worktree with scripts/test-pyramid.sh, build the
                    release binary with the HEAD commit embedded, and install it as candidate
  install --unverified <binary>
                    install an already built, commit-identified binary as candidate without
                    the clean-tree check, the gate, or a source commit match; BUILD-INFO and
                    status record it as verified=false
  promote [<id>]    point stable at the candidate (or <id>); the old stable becomes previous
  rollback          swap stable and previous
  status            show the aliases, installed versions and host command paths
USAGE
}

fail() {
  echo "local-install: $*" >&2
  exit 1
}

install_root() {
  local root="${MAVEN_MCP_INSTALL_ROOT:-${XDG_DATA_HOME:-${HOME}/.local/share}/maven-mcp}"
  [[ "${root}" == /* ]] || fail "MAVEN_MCP_INSTALL_ROOT must be an absolute path"
  root="$(realpath -m "${root}")"
  # The installed runtime must not live in (or be rebuilt from) the source worktree.
  case "${root}/" in
    "${PROJECT_ROOT}/"*) fail "the install root must be outside the source worktree" ;;
  esac
  mkdir -p "${root}/versions"
  printf '%s\n' "${root}"
}

lock_root() {
  local root="$1"
  command -v flock >/dev/null 2>&1 || return 0
  exec 9>"${root}/.lock"
  flock --nonblock 9 || fail "another local-install command is running for ${root}"
}

# Prints "<version> <commit>" from `<binary> --version`, or fails for unidentified builds.
binary_identity() {
  local binary="$1" output
  output="$("${binary}" --version)" || fail "cannot run ${binary} --version"
  [[ "${output}" =~ ${VERSION_PATTERN} ]] ||
    fail "${binary} is not a commit-identified build ('${output}'); build it through 'scripts/local-install.sh candidate'"
  printf '%s %s\n' "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}"
}

sha256_of() {
  sha256sum "$1" | cut -d ' ' -f 1
}

build_info_value() {
  local file="$1" key="$2"
  sed -n "s/^${key}=//p" "${file}"
}

alias_id() {
  local root="$1" name="$2" target
  [[ -L "${root}/${name}" ]] || return 0
  target="$(readlink "${root}/${name}")"
  printf '%s\n' "${target#versions/}"
}

# Fails unless versions/<id> is complete and its binary still matches BUILD-INFO.
check_version() {
  local root="$1" id="$2" directory info identity
  directory="${root}/versions/${id}"
  info="${directory}/${BUILD_INFO_NAME}"
  [[ -n "${id}" && "${id}" != */* && "${id}" != .* && -f "${info}" && -x "${directory}/${BINARY_NAME}" ]] ||
    fail "version '${id}' is not installed below ${root}/versions"
  [[ "$(sha256_of "${directory}/${BINARY_NAME}")" == "$(build_info_value "${info}" sha256)" ]] ||
    fail "version '${id}' does not match its recorded SHA-256"
  identity="$(binary_identity "${directory}/${BINARY_NAME}")"
  [[ "$(identity_id "${identity}")" == "${id}" &&
    "${identity#* }" == "$(build_info_value "${info}" commit)" ]] ||
    fail "version '${id}' reports a different build identity"
}

# Turns "<version> <commit>" into the installation id <version>+<short commit>.
identity_id() {
  local version="${1% *}" commit="${1#* }"
  printf '%s+%s\n' "${version}" "${commit:0:${SHORT_COMMIT_LENGTH}}"
}

# Atomically points <root>/<name> at versions/<id>.
set_alias() {
  local root="$1" name="$2" id="$3" temporary
  temporary="${root}/.${name}.$$"
  ln -s "versions/${id}" "${temporary}"
  mv -T -f "${temporary}" "${root}/${name}"
}

# Fails unless the binary reports the expected source commit.
require_commit() {
  local identity="$1" expected="$2"
  [[ "${identity#* }" == "${expected}" ]] ||
    fail "the built binary reports commit ${identity#* }, not the verified commit ${expected}"
}

# install_binary <binary> <verified: true|false> [<expected commit>]
install_binary() {
  local binary="$1" verified="$2" expected_commit="${3:-}" root identity id directory staging sha
  [[ -f "${binary}" && -x "${binary}" ]] || fail "${binary} is not an executable file"
  root="$(install_root)"
  lock_root "${root}"
  identity="$(binary_identity "${binary}")"
  if [[ -n "${expected_commit}" ]]; then
    require_commit "${identity}" "${expected_commit}"
  fi
  id="$(identity_id "${identity}")"
  directory="${root}/versions/${id}"
  sha="$(sha256_of "${binary}")"
  if [[ -e "${directory}" ]]; then
    [[ "$(build_info_value "${directory}/${BUILD_INFO_NAME}" sha256)" == "${sha}" ]] ||
      fail "version '${id}' is already installed with different content; installed versions are immutable"
    echo "Version ${id} is already installed."
  else
    staging="${root}/versions/.${id}.partial.$$"
    rm -rf "${staging}"
    mkdir -p "${staging}"
    cp "${binary}" "${staging}/${BINARY_NAME}"
    chmod 0755 "${staging}/${BINARY_NAME}"
    [[ "$(sha256_of "${staging}/${BINARY_NAME}")" == "${sha}" ]] || fail "copy of ${binary} is corrupt"
    printf 'id=%s\nversion=%s\ncommit=%s\nverified=%s\nsha256=%s\ninstalled_at=%s\n' \
      "${id}" "${identity% *}" "${identity#* }" "${verified}" "${sha}" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
      >"${staging}/${BUILD_INFO_NAME}"
    mv -T "${staging}" "${directory}"
    echo "Installed version ${id}."
  fi
  check_version "${root}" "${id}"
  set_alias "${root}" candidate "${id}"
  echo "candidate -> ${id} (${root}/candidate/${BINARY_NAME})"
}

# Prints the executable path of the maven-mcp binary artifact from Cargo's JSON messages,
# so CARGO_TARGET_DIR, build.target-dir or a build.target triple cannot redirect the build
# away from the path that gets installed.
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

build_candidate() {
  local commit messages binary identity
  cd "${PROJECT_ROOT}"
  command -v git >/dev/null 2>&1 || fail "git is required"
  [[ -z "$(git status --porcelain)" ]] ||
    fail "the worktree has uncommitted changes; commit or stash them first"
  commit="$(git rev-parse HEAD)"
  scripts/test-pyramid.sh
  messages="$(MAVEN_MCP_BUILD_COMMIT="${commit}" cargo build --release --locked \
    --bin "${BINARY_NAME}" --message-format=json-render-diagnostics)"
  binary="$(built_executable "${messages}")"
  [[ "$(git rev-parse HEAD)" == "${commit}" && -z "$(git status --porcelain)" ]] ||
    fail "the worktree changed during verification or build"
  identity="$(binary_identity "${binary}")"
  require_commit "${identity}" "${commit}"
  install_binary "${binary}" true "${commit}"
}

promote() {
  local root id stable
  root="$(install_root)"
  lock_root "${root}"
  id="${1:-$(alias_id "${root}" candidate)}"
  [[ -n "${id}" ]] || fail "no candidate is installed"
  check_version "${root}" "${id}"
  stable="$(alias_id "${root}" stable)"
  if [[ "${stable}" == "${id}" ]]; then
    echo "stable already -> ${id}"
    return 0
  fi
  if [[ -n "${stable}" ]]; then
    set_alias "${root}" previous "${stable}"
  fi
  set_alias "${root}" stable "${id}"
  echo "stable -> ${id} (previous -> ${stable:-none})"
}

rollback() {
  local root stable previous
  root="$(install_root)"
  lock_root "${root}"
  previous="$(alias_id "${root}" previous)"
  [[ -n "${previous}" ]] || fail "no previous stable version to roll back to"
  check_version "${root}" "${previous}"
  stable="$(alias_id "${root}" stable)"
  set_alias "${root}" stable "${previous}"
  if [[ -n "${stable}" ]]; then
    set_alias "${root}" previous "${stable}"
  fi
  echo "stable -> ${previous} (previous -> ${stable:-none})"
}

status() {
  local root name id info verified
  root="$(install_root)"
  echo "install root: ${root}"
  for name in candidate stable previous; do
    id="$(alias_id "${root}" "${name}")"
    if [[ -n "${id}" ]]; then
      echo "${name}: ${id} (${root}/${name}/${BINARY_NAME})"
    else
      echo "${name}: none"
    fi
  done
  echo "versions:"
  for info in "${root}"/versions/*/"${BUILD_INFO_NAME}"; do
    [[ -f "${info}" ]] || continue
    verified="$(build_info_value "${info}" verified)"
    echo "  $(build_info_value "${info}" id) commit=$(build_info_value "${info}" commit) verified=${verified:-unknown}"
  done
}

for command in sha256sum readlink realpath mv ln grep sed; do
  command -v "${command}" >/dev/null 2>&1 || fail "missing prerequisite: ${command}"
done

case "${1:-}" in
  candidate) [[ $# -eq 1 ]] || { usage; exit 2; }; build_candidate ;;
  install) [[ $# -eq 3 && "$2" == --unverified ]] || { usage; exit 2; }; install_binary "$3" false ;;
  promote) [[ $# -le 2 ]] || { usage; exit 2; }; promote "${2:-}" ;;
  rollback) [[ $# -eq 1 ]] || { usage; exit 2; }; rollback ;;
  status) [[ $# -eq 1 ]] || { usage; exit 2; }; status ;;
  *) usage; exit 2 ;;
esac
