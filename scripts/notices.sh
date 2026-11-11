#!/bin/bash
# THIRD-PARTY-NOTICES for the zip: every licence the core's crates carry, with its
# text and the crates under it, made by cargo-about from about.toml and about.hbs.
#   notices.sh OUT
# cargo-about from PATH when it is there; else its release binary, pinned by version
# and digest, fetched once into target/tools. A licence not in about.toml's accepted
# list fails here, which is the point: nothing ships under a licence nobody read.
set -euo pipefail
ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
OUT=${1:?the file to write}
VERSION=0.9.3
TRIPLE=x86_64-unknown-linux-musl
SHA256=3ad1e94ea1e63d69d1961f4fb15ccbbe9575d906111f96d2cb4dce6df48f71b4
TOOLS=$ROOT/target/tools

about() {
  if command -v cargo-about >/dev/null 2>&1; then
    cargo about "$@"
    return
  fi
  local bin="$TOOLS/cargo-about-$VERSION"
  if [ ! -x "$bin" ]; then
    [ "$(uname -m)" = x86_64 ] || { echo "notices: no cargo-about here and the pinned binary is x86_64 only" >&2; exit 1; }
    mkdir -p "$TOOLS"
    local tgz="$TOOLS/cargo-about-$VERSION.tar.gz"
    curl -sSfL -o "$tgz" "https://github.com/EmbarkStudios/cargo-about/releases/download/$VERSION/cargo-about-$VERSION-$TRIPLE.tar.gz"
    echo "$SHA256  $tgz" | sha256sum -c --quiet - || { rm -f "$tgz"; echo "notices: the cargo-about download does not match its digest" >&2; exit 1; }
    tar -xzf "$tgz" -C "$TOOLS" --strip-components=1 "cargo-about-$VERSION-$TRIPLE/cargo-about"
    mv "$TOOLS/cargo-about" "$bin"
    rm -f "$tgz"
  fi
  "$bin" about "$@"
}

cd "$ROOT"
about generate about.hbs -o "$OUT"
[ -s "$OUT" ] || { echo "notices: nothing was written to $OUT" >&2; exit 1; }
grep -q "^THIRD-PARTY NOTICES" "$OUT" || { echo "notices: $OUT does not start as the template says" >&2; exit 1; }
echo "notices: $OUT: $(grep -c '^  [a-z0-9_-]* [0-9]' "$OUT") crate entries, $(grep -c '^====' "$OUT") licence texts"
