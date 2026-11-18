#!/usr/bin/env bash
#
# Smoke test: prove the Takt daemon runs as a native Luna service on the LG webOS TV.
#
# 1. Cross-builds the daemon (armv7 soft-float, glibc 2.24).
# 2. Deploys the binary + native-service descriptor (services.json, start.sh) into the
#    required <id>.service dir (/media/developer/apps/usr/palm/services/com.github.acuntex.takt.service/).
# 3. Deploys the LS2 *dev-mode* role/manifest/service into /var/luna-service2-dev/ so
#    the daemon may register the bus name com.github.acuntex.takt and call audio/tvpower.
# 4. Installs the init.d boot entrypoint (symlink in /var/lib/webosbrew/init.d/, like
#    HyperHDR) and boots the service with it (service stopped, then started via init.d).
# 5. Dumps the daemon log — the daemon's own FFI output is the verification evidence
#    (getVolume original, setVolume 30 verified, restore, tvpower veto). No shell luna-send
#    is used for any audio/tvpower check.
#
# Note: luna-send in interactive mode exits as soon as stdin hits EOF, so the init.d
# entrypoint is run with its stdin held open (`sleep N |`) for the duration of the call.
#
# Configuration via environment (defaults below):
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

SERVICE_DIR="/media/developer/apps/usr/palm/services/com.github.acuntex.takt.service"
OLD_SERVICE_DIR="/media/developer/apps/usr/palm/services/com.github.acuntex.takt"
LS2_DEV="/var/luna-service2-dev"
INITD_NAME="90-start_takt"
INITD_DIR="/var/lib/webosbrew/init.d"

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

# --- stop any running instance and drop the stale (no `.service`) deployment ---
echo ">> cleaning up previous deployment ..." >&2
ssh_run "kill \$(pidof takt) 2>/dev/null; true"
ssh_run "rm -rf '$OLD_SERVICE_DIR'"
sleep 1

# --- deploy the native-service dir (required `<id>.service` location) ---
echo ">> deploying service to $SERVICE_DIR ..." >&2
ssh_run "mkdir -p '$SERVICE_DIR'"
cat "$REPO_ROOT/packaging/services.json" | ssh_run "cat > '$SERVICE_DIR/services.json'"
cat "$REPO_ROOT/packaging/start.sh" | ssh_run "cat > '$SERVICE_DIR/start.sh'"
cat "$BIN" | ssh_run "cat > '$SERVICE_DIR/takt'"
cat "$REPO_ROOT/packaging/init.d/$INITD_NAME" | ssh_run "cat > '$SERVICE_DIR/$INITD_NAME'"
ssh_run "chmod +x '$SERVICE_DIR/start.sh' '$SERVICE_DIR/takt' '$SERVICE_DIR/$INITD_NAME'"

# --- install the init.d boot symlink (HyperHDR pattern) ---
echo ">> installing init.d entrypoint $INITD_DIR/$INITD_NAME ..." >&2
ssh_run "mkdir -p '$INITD_DIR'"
ssh_run "ln -sf '$SERVICE_DIR/$INITD_NAME' '$INITD_DIR/$INITD_NAME'"

# --- deploy the LS2 dev-mode role/manifest/service ---
echo ">> deploying LS2 dev-mode config to $LS2_DEV ..." >&2
ls2_deploy() {
    local sub="$1" name="$2"
    cat "$REPO_ROOT/packaging/ls2/$sub/$name" | ssh_run "cat > '$LS2_DEV/$sub/$name'"
}
ls2_deploy services.d com.github.acuntex.takt.service
ls2_deploy roles.d com.github.acuntex.takt.service.json
ls2_deploy manifests.d com.github.acuntex.takt.json
ls2_deploy client-permissions.d com.github.acuntex.takt.service.service.json
ls2_deploy client-permissions.d com.github.acuntex.takt.service.root.json
ls2_deploy api-permissions.d com.github.acuntex.takt.service.api.json

# --- reload the LS2 hub so the new role/manifest take effect ---
echo ">> rescanning LS2 services ..." >&2
ssh_run "ls-control scan-services"
sleep 1

# --- boot via the init.d entrypoint (service stopped, then started) ---
ssh_run "kill \$(pidof takt) 2>/dev/null; true"
sleep 1
echo ">> booting via init.d entrypoint $INITD_NAME ..." >&2
ssh_run "rm -f /tmp/takt.log"
ssh_run "sleep 4 | sh '$INITD_DIR/$INITD_NAME'"

# --- let the FFI demo sequence run (getVolume -> setVolume 30 -> verify -> restore -> veto) ---
echo ">> waiting for the LS2 demo sequence ..." >&2
sleep 8

# --- daemon log is the verification evidence (FFI only, no shell luna-send) ---
echo ">> daemon log (/tmp/takt.log):" >&2
LOG="$(ssh_run "cat /tmp/takt.log")"
printf '%s\n' "$LOG"

# --- teardown ---
ssh_run "kill \$(pidof takt) 2>/dev/null; true"

if grep -q "audio/getVolume ->" <<<"$LOG" && grep -q "subscribed" <<<"$LOG"; then
    echo ">> OK: FFI log shows getVolume and the tvpower veto subscription" >&2
else
    echo ">> FAIL: expected markers missing from the daemon log" >&2
    exit 1
fi
