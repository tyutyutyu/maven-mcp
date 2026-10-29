#!/usr/bin/env bash
# Cargo linker for Linux targets whose rustc default is GNU ld (see .cargo/config.toml).
# GNU ld records the GLIBC_2.39 version of std's weak pidfd_spawnp/pidfd_getpid
# references as required, so the binary would not load on older glibc; lld marks the
# version reference weak, as rustc's default rust-lld already does on x86_64. This links
# through the C compiler driver with the rust-lld shipped in the active toolchain.
set -euo pipefail

rustc="${RUSTC:-rustc}"
sysroot="$("${rustc}" --print sysroot)"
host="$("${rustc}" -vV | sed -n 's/^host: //p')"
gcc_ld="${sysroot}/lib/rustlib/${host}/bin/gcc-ld"
if [[ ! -x "${gcc_ld}/ld.lld" ]]; then
  echo "${gcc_ld}/ld.lld not found; the active Rust toolchain must ship rust-lld" >&2
  exit 1
fi
exec "${CC:-cc}" -B "${gcc_ld}" -fuse-ld=lld "$@"
