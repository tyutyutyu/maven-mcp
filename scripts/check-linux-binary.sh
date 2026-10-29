#!/usr/bin/env bash
# Checks release binaries against the documented Linux platform requirements (README.md,
# "Supported Platforms"): dynamically linked glibc executables that link only the C runtime
# libraries and need no non-weak glibc symbol version newer than MAX_GLIBC. It only
# inspects the ELF files and never runs them.
set -euo pipefail

readonly MAX_GLIBC="2.34"
readonly ALLOWED_LIBRARIES="libc.so.6 libm.so.6 libgcc_s.so.1 ld-linux-x86-64.so.2 ld-linux-aarch64.so.1"

if [[ $# -eq 0 ]]; then
  echo "Usage: scripts/check-linux-binary.sh <executable>..." >&2
  exit 2
fi
for command in readelf sort; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "${command} is required" >&2
    exit 2
  fi
done

# Prints the newest glibc version the binary cannot run without; weak references
# (Flags: WEAK) are resolved at run time only when present.
required_glibc() {
  readelf --version-info --wide "$1" \
    | sed -n 's/.*Name: GLIBC_\([0-9.]*\)  *Flags: none.*/\1/p' \
    | sort -uV | tail -n 1
}

check_binary() {
  local binary="$1"
  local failed=false
  if [[ ! -x "${binary}" ]]; then
    echo "Not an executable: ${binary}" >&2
    return 1
  fi
  while read -r library; do
    if [[ " ${ALLOWED_LIBRARIES} " != *" ${library} "* ]]; then
      echo "${binary}: unexpected shared library dependency ${library}" >&2
      failed=true
    fi
  done < <(readelf --dynamic --wide "${binary}" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p')

  local required
  required="$(required_glibc "${binary}")"
  if [[ -z "${required}" ]]; then
    echo "${binary}: no glibc version requirement; not a dynamically linked glibc binary" >&2
    failed=true
  elif [[ "$(printf '%s\n%s\n' "${required}" "${MAX_GLIBC}" | sort -V | tail -n 1)" != "${MAX_GLIBC}" ]]; then
    echo "${binary}: requires glibc ${required}, newer than the documented ${MAX_GLIBC}" >&2
    failed=true
  fi
  if [[ "${failed}" == true ]]; then
    return 1
  fi
  echo "$(uname -m): ${binary} needs glibc ${required} (documented maximum ${MAX_GLIBC})"
}

status=0
for binary in "$@"; do
  check_binary "${binary}" || status=1
done
exit "${status}"
