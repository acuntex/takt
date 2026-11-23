#!/usr/bin/env bash
#
# Deploy/update Takt on a rooted LG webOS TV over SSH — no reboot required.
#
# Copies the native-service files, the LS2 dev-mode role/manifest/service files, and the
# init.d boot symlink, rescans the LS2 hub, then starts the service through init.d and
# verifies the local HTTP API. Unlike scripts/tv/smoke-luna.sh this performs no demo sequence
# (no setVolume/veto probe) and leaves the service running.
#
# Environment:
#   TV_HOST       TV address or hostname (required, no default; see README.md)
#   TV_USER       SSH user (default root)
#   TV_PASSWORD   password for the self-contained SSH fallback (never printed)
#   TV_HELPER     shared SSH helper (default: scripts/tv-ssh.sh in this repo)
#   TAKT_BIN      prebuilt ARMv7 binary; otherwise the release build path is used
#   TARGET        Rust target (default armv7-unknown-linux-gnueabi)
#   ALLOW_HOST_BUILD  1 allows a host-linked build when cargo-zigbuild is absent (experiments only;
#                 the result is not the glibc 2.24 artifact and must never be deployed)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

TV_HOST="${TV_HOST:-}"
TV_USER="${TV_USER:-root}"
TV_PASSWORD="${TV_PASSWORD:-}"
TV_HELPER="${TV_HELPER:-$REPO_ROOT/scripts/tv-ssh.sh}"
TARGET="${TARGET:-armv7-unknown-linux-gnueabi}"

if [[ -z "$TV_HOST" ]]; then
  echo ">> FAIL: set TV_HOST to the TV address or hostname (see README.md)" >&2
  exit 2
fi

# cargo-zigbuild needs a `zig` executable; allow a local unpacked toolchain.
if ! command -v zig >/dev/null 2>&1; then
  for candidate in "$HOME/zig/zig" /usr/local/bin/zig /opt/zig/zig; do
    if [[ -x "$candidate" ]]; then
      zig_dir="$(dirname "$candidate")"
      export PATH="$zig_dir:$PATH"
      break
    fi
  done
fi

if [[ -n "${TAKT_BIN:-}" ]]; then
  BIN="$TAKT_BIN"
else
  BIN="$REPO_ROOT/daemon/target/$TARGET/release/takt"
fi

SERVICE_DIR="/media/developer/apps/usr/palm/services/com.github.acuntex.takt.service"
LS2_DEV="/var/luna-service2-dev"
INITD_DIR="/var/lib/webosbrew/init.d"
INITD_NAME="90-start_takt"

ASKPASS=""
cleanup() {
  if [[ -n "$ASKPASS" ]]; then
    rm -f "$ASKPASS"
  fi
}
trap cleanup EXIT

if [[ -x "$TV_HELPER" ]]; then
  ssh_run() { "$TV_HELPER" "$@"; }
else
  if [[ -z "$TV_PASSWORD" ]]; then
    echo ">> FAIL: set TV_PASSWORD or provide an executable TV_HELPER" >&2
    exit 1
  fi
  ASKPASS="$(mktemp -t takt-askpass.XXXXXX)"
  # The helper reads the exported variable; printf avoids echo mangling passwords that start
  # with "-" or contain backslashes. The password itself is never printed by this script.
  export TV_PASSWORD
  printf '%s\n' '#!/bin/sh' 'printf '\''%s\n'\'' "$TV_PASSWORD"' >"$ASKPASS"
  chmod 700 "$ASKPASS"
  ssh_run() {
    env SSH_ASKPASS="$ASKPASS" SSH_ASKPASS_REQUIRE=force \
      setsid -w ssh \
      -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      -o LogLevel=ERROR -o ConnectTimeout=8 \
      -o PreferredAuthentications=password -o PubkeyAuthentication=no \
      "$TV_USER@$TV_HOST" "$@"
  }
fi

if [[ ! -x "$BIN" ]]; then
  if [[ -n "${TAKT_BIN:-}" ]]; then
    echo ">> FAIL: TAKT_BIN is not executable: $BIN" >&2
    exit 1
  fi
  if ! cargo zigbuild --help >/dev/null 2>&1 && [[ "${ALLOW_HOST_BUILD:-0}" != "1" ]]; then
    echo ">> FAIL: cargo-zigbuild is missing; a deploy needs the glibc 2.24 build (set ALLOW_HOST_BUILD=1 to override)" >&2
    exit 1
  fi
  echo ">> building release ARMv7 binary ($TARGET, glibc 2.24) ..." >&2
  (
    cd "$REPO_ROOT/daemon"
    if cargo zigbuild --help >/dev/null 2>&1; then
      cargo zigbuild --release --target "$TARGET.2.24"
    else
      echo ">> WARNING: host build, not the glibc 2.24 artifact" >&2
      cargo build --release --target "$TARGET"
    fi
  )
fi
[[ -x "$BIN" ]] || {
  echo ">> FAIL: binary not found: $BIN" >&2
  exit 1
}

echo ">> stopping Takt on $TV_HOST ..." >&2
ssh_run "kill \$(pidof takt) 2>/dev/null || true"
sleep 1

echo ">> copying native service to $SERVICE_DIR ..." >&2
ssh_run "mkdir -p '$SERVICE_DIR'"
cat "$REPO_ROOT/packaging/services.json" | ssh_run "cat > '$SERVICE_DIR/services.json'"
cat "$REPO_ROOT/packaging/start.sh" | ssh_run "cat > '$SERVICE_DIR/start.sh'"
cat "$REPO_ROOT/packaging/init.d/$INITD_NAME" | ssh_run "cat > '$SERVICE_DIR/$INITD_NAME'"
cat "$BIN" | ssh_run "cat > '$SERVICE_DIR/takt'"
ssh_run "chmod 0755 '$SERVICE_DIR/start.sh' '$SERVICE_DIR/takt' '$SERVICE_DIR/$INITD_NAME'"
ssh_run "chmod 0644 '$SERVICE_DIR/services.json'"

echo ">> installing init.d entrypoint $INITD_DIR/$INITD_NAME ..." >&2
ssh_run "mkdir -p '$INITD_DIR'"
ssh_run "ln -sf '$SERVICE_DIR/$INITD_NAME' '$INITD_DIR/$INITD_NAME'"

echo ">> deploying LS2 dev-mode config to $LS2_DEV ..." >&2
ssh_run "mkdir -p '$LS2_DEV/services.d' '$LS2_DEV/roles.d' '$LS2_DEV/manifests.d' '$LS2_DEV/client-permissions.d' '$LS2_DEV/api-permissions.d'"
for file in "$REPO_ROOT"/packaging/ls2/*/*; do
  relative="${file#"$REPO_ROOT"/packaging/ls2/}"
  cat "$file" | ssh_run "cat > '$LS2_DEV/$relative'"
done

echo ">> rescanning LS2 services ..." >&2
ssh_run "ls-control scan-services"
sleep 1

echo ">> starting via init.d entrypoint ..." >&2
ssh_run "rm -f /tmp/takt.log"
ssh_run "sleep 4 | sh '$INITD_DIR/$INITD_NAME'"
sleep 3

verify_status() {
  local response=""
  if command -v curl >/dev/null 2>&1; then
    response="$(curl -fsS --max-time 3 "http://$TV_HOST:3927/api/status" 2>/dev/null || true)"
  fi
  if [[ -z "$response" ]]; then
    response="$(ssh_run "wget -q -O - 'http://127.0.0.1:3927/api/status'" 2>/dev/null || true)"
  fi
  if [[ -z "$response" ]]; then
    response="$(ssh_run "curl -fsS --max-time 3 'http://127.0.0.1:3927/api/status'" 2>/dev/null || true)"
  fi
  printf '%s' "$response"
}

echo ">> verifying HTTP /api/status ..." >&2
STATUS="$(verify_status)"
if [[ -z "$STATUS" ]]; then
  echo ">> FAIL: /api/status did not respond; daemon log follows" >&2
  ssh_run "cat /tmp/takt.log 2>/dev/null || true" >&2
  exit 1
fi
printf '%s\n' "$STATUS"

echo ">> deploy complete; service left running" >&2
