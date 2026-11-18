#!/usr/bin/env bash
#
# Smoke test: prove that the cross-compiled takt binary runs on the LG webOS TV.
#
# Copies daemon/target/<TARGET>/debug/takt to the TV's /tmp and runs it for a few
# seconds. Expected: the startup line "[takt] starting player ..." appears.
#
# The TV is a 32-bit ARM soft-float glibc 2.24 userland, so the target is
# armv7-unknown-linux-gnueabi (NOT gnueabihf), linked against glibc 2.24.
#
# Configuration via environment:
#   TV_HOST (required), TV_USER, TV_PASSWORD, TV_HELPER, TARGET

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

BIN="$REPO_ROOT/daemon/target/$TARGET/debug/takt"
REMOTE_PATH="/tmp/takt"
REMOTE_LOG="/tmp/takt-smoke.log"

# --- ssh runner: prefer the shared helper, fall back to a self-contained ssh ---
ASKPASS=""
cleanup() { if [[ -n "$ASKPASS" ]]; then rm -f "$ASKPASS"; fi; }
trap cleanup EXIT

if [[ -x "$TV_HELPER" ]]; then
    ssh_run() { "$TV_HELPER" "$@"; }
else
    ASKPASS="$(mktemp -t askpass.XXXXXX)"
    cat >"$ASKPASS" <<EOF
#!/bin/sh
echo "$TV_PASSWORD"
EOF
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

# --- cross-build if the binary is missing ---
if [[ ! -x "$BIN" ]]; then
    echo ">> cross-building $TARGET (glibc 2.24) ..." >&2
    (cd "$REPO_ROOT/daemon" && cargo zigbuild --target "$TARGET.2.24")
fi
[[ -x "$BIN" ]] || { echo ">> FAIL: binary not found: $BIN" >&2; exit 1; }

# --- copy to the TV ---
echo ">> copying $BIN -> $TV_USER@$TV_HOST:$REMOTE_PATH ..." >&2
cat "$BIN" | ssh_run "cat > '$REMOTE_PATH' && chmod +x '$REMOTE_PATH'"

# --- run it briefly (the daemon keeps running, so stop it after the startup line) ---
echo ">> running on the TV ..." >&2
set +e
ssh_run "'$REMOTE_PATH' > '$REMOTE_LOG' 2>&1 & sleep 3; kill \$! 2>/dev/null; true"
OUT="$(ssh_run "head -n 5 '$REMOTE_LOG' 2>/dev/null; true")"
RC=$?
set -e
printf '%s\n' "$OUT"

if [[ "$RC" -eq 0 ]] && grep -q '\[takt\] starting player' <<<"$OUT"; then
    echo ">> OK: takt runs on the TV (startup line seen)" >&2
else
    echo ">> FAIL: rc=$RC, output='$OUT'" >&2
    exit 1
fi
