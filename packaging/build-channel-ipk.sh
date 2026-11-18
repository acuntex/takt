#!/usr/bin/env bash
#
# Builds the Homebrew-Channel IPK with ares-package: the app directory plus the native service.
# The service carries services.json (id/name = <app>.service, executable takt), setup.sh, the
# packaged LS2 descriptors and the init script. No maintainer scripts are shipped.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"
TARGET="${TARGET:-armv7-unknown-linux-gnueabi}"
BIN="${TAKT_BIN:-$REPO_ROOT/daemon/target/$TARGET/release/takt}"
VERSION="$(python3 -c 'import json;print(json.load(open("packaging/appinfo.json"))["version"])')"
ID="$(python3 -c 'import json;print(json.load(open("packaging/appinfo.json"))["id"])')"

[ -x "$BIN" ] || { echo "no ARM binary at $BIN (build it first)" >&2; exit 1; }

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/app" "$STAGE/service" "$REPO_ROOT/dist"

install -m 0644 packaging/appinfo.json "$STAGE/app/appinfo.json"
install -m 0644 packaging/app-index.html "$STAGE/app/index.html"
install -m 0644 packaging/icon.png "$STAGE/app/icon.png"
install -m 0644 packaging/largeIcon.png "$STAGE/app/largeIcon.png"

install -m 0644 packaging/services.json "$STAGE/service/services.json"
install -m 0755 packaging/start.sh "$STAGE/service/start.sh"
install -m 0755 packaging/setup.sh "$STAGE/service/setup.sh"
install -m 0755 packaging/init.d/90-start_takt "$STAGE/service/90-start_takt"
install -m 0755 "$BIN" "$STAGE/service/takt"
cp -R packaging/ls2 "$STAGE/service/ls2"

npx --yes --package @webos-tools/cli@3.2.6 ares-package \
    "$STAGE/app" "$STAGE/service" -o "$REPO_ROOT/dist/channel"
echo ">> built $REPO_ROOT/dist/channel/${ID}_${VERSION}_all.ipk"
