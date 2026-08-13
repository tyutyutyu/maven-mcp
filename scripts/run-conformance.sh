#!/usr/bin/env bash
set -euo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
readonly MCP_TEST_PORT="${MCP_TEST_PORT:-18081}"
readonly MCP_URL="http://127.0.0.1:${MCP_TEST_PORT}/mcp"
readonly TEST_REPOSITORY="$(mktemp -d)"
readonly SERVER_LOG="${PROJECT_ROOT}/target/conformance-server.log"
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
MAVEN_REPO_PATH="${TEST_REPOSITORY}" \
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

if (($# > 0)); then
    npx --yes @modelcontextprotocol/conformance@0.1.16 \
        server --url "${MCP_URL}" "$@"
else
    # The full suite expects scenario-specific fixture tools/resources. These
    # scenarios are capability-independent and therefore applicable to this
    # tools-only production server without adding conformance-only behavior.
    readonly SCENARIOS=(
        server-initialize
        ping
        completion-complete
        server-sse-multiple-streams
        resources-list
        prompts-list
        dns-rebinding-protection
    )
    for scenario in "${SCENARIOS[@]}"; do
        npx --yes @modelcontextprotocol/conformance@0.1.16 \
            server --url "${MCP_URL}" --scenario "${scenario}"
    done
fi
