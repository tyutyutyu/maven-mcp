#!/usr/bin/env bash
set -euo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
readonly TEST_REPOSITORY="$(mktemp -d)"

cleanup() {
    rmdir "${TEST_REPOSITORY}" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

cd "${PROJECT_ROOT}"
cargo build --locked --quiet --bin maven-mcp
export MAVEN_REPO_PATH="${MAVEN_REPO_PATH:-${TEST_REPOSITORY}}"
readonly MCP_COMMAND="${PROJECT_ROOT}/target/debug/maven-mcp"

echo "MCP Inspector indul STDIO transporttal: ${MCP_COMMAND}"
if [[ "${1:-}" == "--cli" ]]; then
    shift
    npx --yes @modelcontextprotocol/inspector@2.1.0 \
        --cli "${MCP_COMMAND}" --transport stdio "$@"
else
    npx --yes @modelcontextprotocol/inspector@2.1.0 \
        "${MCP_COMMAND}" --transport stdio "$@"
fi
