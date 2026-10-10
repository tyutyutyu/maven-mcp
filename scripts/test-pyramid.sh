#!/usr/bin/env bash
set -euo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
readonly COVERAGE_DIR="${PROJECT_ROOT}/target/coverage"
readonly COVERAGE_THRESHOLD=80

cd "${PROJECT_ROOT}"

for prerequisite in "cargo llvm-cov --version" "python3 --version"; do
  if ! ${prerequisite} >/dev/null 2>&1; then
    echo "Missing coverage prerequisite: '${prerequisite}' failed." >&2
    echo "Install: rustup component add llvm-tools-preview && cargo install cargo-llvm-cov --version 0.8.5 --locked" >&2
    exit 1
  fi
done

cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings

# Runs every Cargo test target, including the child-process tests: cargo-llvm-cov
# instruments the production binaries those tests spawn.
rm -rf "${COVERAGE_DIR}"
mkdir -p "${COVERAGE_DIR}"
# --no-report retains profiles, so discard data from previous runs explicitly.
cargo llvm-cov clean --workspace
cargo llvm-cov --locked --all-targets --no-report
cargo llvm-cov report --locked --json --summary-only --output-path "${COVERAGE_DIR}/summary.json"
cargo llvm-cov report --locked --html --output-dir "${COVERAGE_DIR}"
python3 -I "${SCRIPT_DIR}/check-coverage.py" "${COVERAGE_DIR}/summary.json" "${COVERAGE_THRESHOLD}"

echo "MCP keresési riport: ${PROJECT_ROOT}/target/mcp-test-report/report.md"
echo "Lefedettségi riport: ${COVERAGE_DIR}/html/index.html"
