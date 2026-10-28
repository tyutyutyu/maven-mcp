#!/usr/bin/env bash
# Opt-in multi-project smoke suite of an installed maven-mcp binary on real Maven
# projects. It is not part of scripts/test-pyramid.sh. The project manifest and the
# result records are private: both must be outside this repository, and the records
# must never be committed or packaged.
set -euo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd -P)"

usage() {
  cat >&2 <<'USAGE'
Usage: scripts/dogfood-smoke.sh --manifest <file> --output <directory> [--binary <path>]
  --manifest  private JSON project manifest (outside the repository)
  --output    private directory for the JSON result records (outside the repository)
  --binary    installed binary; default: <install root>/candidate/maven-mcp
USAGE
}

fail() {
  echo "dogfood-smoke: $*" >&2
  exit 1
}

# Prints the canonical absolute path; missing trailing components are allowed.
canonical() {
  local path="$1"
  [[ "${path}" == /* ]] || fail "${path} must be an absolute path"
  realpath -m "${path}"
}

require_outside_repository() {
  local path="$1"
  case "${path}/" in
    "${PROJECT_ROOT}/"*) fail "${path} is inside the public repository; keep private data outside it" ;;
  esac
}

manifest=""
output=""
binary="${MAVEN_MCP_INSTALL_ROOT:-${XDG_DATA_HOME:-${HOME}/.local/share}/maven-mcp}/candidate/maven-mcp"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --manifest) [[ $# -ge 2 ]] || { usage; exit 2; }; manifest="$2"; shift 2 ;;
    --output) [[ $# -ge 2 ]] || { usage; exit 2; }; output="$2"; shift 2 ;;
    --binary) [[ $# -ge 2 ]] || { usage; exit 2; }; binary="$2"; shift 2 ;;
    *) usage; exit 2 ;;
  esac
done
[[ -n "${manifest}" && -n "${output}" ]] || { usage; exit 2; }

manifest="$(canonical "${manifest}")"
output="$(canonical "${output}")"
[[ -f "${manifest}" ]] || fail "manifest ${manifest} does not exist"
require_outside_repository "${manifest}"
require_outside_repository "${output}"
[[ -x "${binary}" ]] || fail "binary ${binary} is not executable; install a candidate first"
binary="$(canonical "${binary}")"
# The smoke tests an installed runtime, never a build output of this worktree.
require_outside_repository "${binary}"
mkdir -p "${output}"

echo "Dogfood smoke: $("${binary}" --version)"
cd "${PROJECT_ROOT}"
MAVEN_MCP_DOGFOOD_BINARY="${binary}" \
  MAVEN_MCP_DOGFOOD_MANIFEST="${manifest}" \
  MAVEN_MCP_DOGFOOD_OUTPUT="${output}" \
  cargo test --locked --test dogfood_smoke -- --ignored --test-threads=1 --nocapture
