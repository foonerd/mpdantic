#!/bin/bash
# Build the core for each Volumio architecture and put it in bin/<arch>/mpdantic.
# Players run those files; they do not run cargo.
#
# Every binary is linked statically against musl: the daemon needs nothing from a
# player's C library, and a cross glibc would otherwise bind the standard library to a
# glibc newer than Volumio's 2.36. The arm binary is the armv7 build: Volumio's universal
# Raspberry Pi image labels itself arm, so a Pi Zero or Pi 1 (ARMv6) is not served.
set -euo pipefail
ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
cd "$ROOT"
unset CARGO_TARGET_DIR

# The cross compilers drive the link; the C runtime and libc come with the Rust musl
# targets themselves. The cc crate, should a dependency bring a C part, looks for a
# compiler named after the musl triple and finds none, so each is named here.
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=arm-linux-gnueabihf-gcc
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc
export CC_x86_64_unknown_linux_musl=gcc
export AR_x86_64_unknown_linux_musl=ar
export CC_armv7_unknown_linux_musleabihf=arm-linux-gnueabihf-gcc
export AR_armv7_unknown_linux_musleabihf=arm-linux-gnueabihf-ar
export CC_aarch64_unknown_linux_musl=aarch64-linux-gnu-gcc
export AR_aarch64_unknown_linux_musl=aarch64-linux-gnu-ar

ship() {
  local triple=$1 arch=$2
  echo "ship: $arch ($triple)"
  cargo build --release --locked --target "$triple" -p mpdantic --bins
  install -D -m 755 "target/$triple/release/mpdantic" "bin/$arch/mpdantic"
}
ship x86_64-unknown-linux-musl x64
ship armv7-unknown-linux-musleabihf armv7
ship aarch64-unknown-linux-musl armv8
install -D -m 755 bin/armv7/mpdantic bin/arm/mpdantic

echo "ship: static"
for bin in bin/*/mpdantic; do
  # x86-64 musl links as a static PIE; file words that differently.
  file "$bin" | grep -qE 'statically linked|static-pie linked' || { echo "ship: $bin is not statically linked" >&2; exit 1; }
  if readelf -W --dyn-syms "$bin" 2>/dev/null | grep -q 'GLIBC_'; then
    echo "ship: $bin needs glibc" >&2; exit 1
  fi
done
echo "ship: armv7"
readelf -A bin/armv7/mpdantic | grep -q 'Tag_CPU_arch: v7' || { echo "ship: bin/armv7/mpdantic is not ARMv7 code" >&2; exit 1; }
echo "ship: version"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
said=$(bin/x64/mpdantic --version)
[ "$said" = "mpdantic $version" ] || { echo "ship: mpdantic says '$said', the workspace is $version" >&2; exit 1; }
file bin/arm/mpdantic bin/armv7/mpdantic bin/armv8/mpdantic bin/x64/mpdantic
