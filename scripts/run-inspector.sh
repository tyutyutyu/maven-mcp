#!/usr/bin/env bash
set -euo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
readonly MCP_TEST_PORT="${MCP_TEST_PORT:-18082}"
readonly MCP_URL="http://127.0.0.1:${MCP_TEST_PORT}/mcp"
readonly TEST_REPOSITORY="$(mktemp -d)"
readonly SERVER_LOG="${PROJECT_ROOT}/target/inspector-server.log"
server_pid=""

cleanup() {
    if [[ -n "${server_pid}" ]] && kill -0 "${server_pid}" 2>/dev/null; then
        kill "${server_pid}"
        wait "${server_pid}" 2>/dev/null || true
    fi
    rmdir "${TEST_REPOSITORY}" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

mkdir -p "${PROJECT_ROOT}/target"
cd "${PROJECT_ROOT}"
MAVEN_REPO_PATH="${MAVEN_REPO_PATH:-${TEST_REPOSITORY}}" \
    BIND_ADDRESS="127.0.0.1:${MCP_TEST_PORT}" \
    cargo run --locked --quiet >"${SERVER_LOG}" 2>&1 &
server_pid="$!"

for _ in $(seq 1 600); do
    if curl --fail --silent "http://127.0.0.1:${MCP_TEST_PORT}/healthz" >/dev/null; then
        break
    fi
    sleep 0.1
done

if ! curl --fail --silent "http://127.0.0.1:${MCP_TEST_PORT}/healthz" >/dev/null; then
    echo "Az MCP szerver nem indult el. Napló: ${SERVER_LOG}" >&2
    tail -n 50 "${SERVER_LOG}" >&2
    exit 1
fi

echo "MCP Inspector indul: ${MCP_URL}"
if [[ "${1:-}" == "--cli" ]]; then
    shift
    npx --yes @modelcontextprotocol/inspector@2.1.0 \
        --cli "${MCP_URL}" --transport http "$@"
else
    npx --yes @modelcontextprotocol/inspector@2.1.0 \
        "${MCP_URL}" --transport http "$@"
fi
