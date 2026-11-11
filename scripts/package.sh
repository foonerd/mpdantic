#!/bin/bash
# The plugin as the zip a player installs: dist/mpdantic-<version>.zip, with the plugin's
# files at its root, the core from bin/<arch> for every architecture, build.json naming
# the commit, and the node modules.
#
# The plugin's version follows the workspace version in Cargo.toml.
set -euo pipefail
ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
cd "$ROOT"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
NAME=mpdantic
STAGE=$ROOT/dist/stage/$NAME
ARCHES="arm armv7 armv8 x64"
rm -rf "$ROOT/dist/stage"
mkdir -p "$STAGE" "$ROOT/dist"
cp -r "$ROOT/plugin/." "$STAGE/"
rm -rf "$STAGE/test" "$STAGE/node_modules" "$STAGE/package-lock.json"
sed -i "s/\"version\": \"[^\"]*\"/\"version\": \"$VERSION\"/" "$STAGE/package.json"

# The build the zip carries: the commit and the time.
printf '{ "commit": "%s", "built": "%s" }\n' \
  "$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)" \
  "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$STAGE/build.json"

for arch in $ARCHES; do
  bin="$ROOT/bin/$arch/mpdantic"
  [ -x "$bin" ] || { echo "package: $bin is missing; run scripts/ship.sh first" >&2; exit 1; }
  install -D -m 755 "$bin" "$STAGE/bin/$arch/mpdantic"
done

# The licences of what the core is built from, beside the core: a zip without them is
# not made.
"$ROOT/scripts/notices.sh" "$STAGE/THIRD-PARTY-NOTICES" || { echo "package: the notices were not made; the zip was not made" >&2; exit 1; }

# Node modules: with npm on this machine, directly; otherwise in a container with the
# Node major Volumio ships. A zip without them is no plugin: the install must succeed,
# and every module package.json names must be there.
if command -v npm >/dev/null 2>&1; then
  ( cd "$STAGE" && npm install --omit=dev --no-audit --no-fund --loglevel=error >/dev/null )
else
  docker run --rm -v "$STAGE:/plugin" -w /plugin node:20-bookworm-slim sh -c \
    "npm install --omit=dev --no-audit --no-fund --loglevel=error >/dev/null; rc=\$?; chown -R $(id -u):$(id -g) /plugin/node_modules /plugin/package-lock.json 2>/dev/null; exit \$rc" \
    || { echo "package: npm install failed; the zip was not made" >&2; exit 1; }
fi
rm -f "$STAGE/package-lock.json"
for module in $(sed -n '/"dependencies"/,/}/p' "$STAGE/package.json" | sed -n 's/^ *"\([^"]*\)": *"[^"]*",*$/\1/p'); do
  [ -f "$STAGE/node_modules/$module/package.json" ] || { echo "package: the module $module is not in the stage; the zip was not made" >&2; exit 1; }
done

ZIP=$ROOT/dist/$NAME-$VERSION.zip
rm -f "$ZIP"
( cd "$STAGE" && zip -qr "$ZIP" . )
rm -rf "$ROOT/dist/stage"
echo "package: $ZIP: $(stat -c %s "$ZIP") bytes, sha256 $(sha256sum "$ZIP" | cut -d' ' -f1)"
