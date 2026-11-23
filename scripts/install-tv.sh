#!/usr/bin/env bash
#
# Manual install/update of the Takt app on the rooted LG TV.
#
# The IPK is unsigned, so webOS `appInstallService` refuses it with
# `-5: Failed to extract package`. This script does what that service would do:
# unpack the IPK on the host (GNU ar; the TV's BusyBox ar cannot read it), stream
# the data tarball to /media/developer/apps, install the LS2 dev files and the
# init.d entry, rescan the hub and start the native service.
#
# A *new* app or a renamed app needs a TV reboot to appear in the launcher,
# because com.webos.applicationManager only rescans installed apps at boot.
# Plain updates (new binary/assets) do not need a reboot.
#
# Environment:
#   TV_HOST     TV address or hostname (required, no default; see README.md)
#   TV_HELPER   SSH helper (default: scripts/tv-ssh.sh in this repo)
#   TV_PASSWORD root password, read by the default helper (or TV_ASKPASS: an askpass program)
#   IPK         IPK path (default: newest dist/*.ipk)
#
# Usage:
#   TV_HOST=<tv-ip> scripts/install-tv.sh             # install/update
#   TV_HOST=<tv-ip> scripts/install-tv.sh --reboot    # install/update, then reboot the TV
#   TV_HOST=<tv-ip> scripts/install-tv.sh --uninstall

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

TV_HOST="${TV_HOST:-}"
TV_HELPER="${TV_HELPER:-$REPO_ROOT/scripts/tv-ssh.sh}"
APP_ID="com.github.acuntex.takt"
APP_DIR="/media/developer/apps/usr/palm/applications/$APP_ID"
SERVICE_DIR="/media/developer/apps/usr/palm/services/$APP_ID.service"
INITD="/var/lib/webosbrew/init.d/90-start_takt"

if [[ -z "$TV_HOST" ]]; then
  echo ">> FAIL: set TV_HOST to the TV address or hostname (see README.md)" >&2
  exit 2
fi

MODE="install"
REBOOT="no"
for arg in "$@"; do
  case "$arg" in
    --reboot) REBOOT="yes" ;;
    --uninstall) MODE="uninstall" ;;
    -h|--help) sed -n '2,23p' "$0"; exit 0 ;;
    *) echo ">> FAIL: unknown argument: $arg" >&2; exit 2 ;;
  esac
done

if [[ ! -x "$TV_HELPER" ]]; then
  echo ">> FAIL: TV_HELPER is not executable: $TV_HELPER (see README.md)" >&2
  exit 1
fi

tv() { "$TV_HELPER" "$@"; }

if [[ "$MODE" == "uninstall" ]]; then
  echo ">> stopping service"
  tv 'kill $(pidof takt) 2>/dev/null; sleep 1; true' >/dev/null 2>&1 || true
  echo ">> unregistering package (may report an extraction error, the removal is what counts)"
  tv "luna-send -n 1 -f luna://com.webos.appInstallService/remove '{\"id\":\"$APP_ID\"}' >/dev/null 2>&1; sleep 2; true" >/dev/null 2>&1 || true
  echo ">> removing files"
  if ! tv "set -e
rm -rf '$APP_DIR' '$SERVICE_DIR' '/media/developer/apps/usr/palm/packages/$APP_ID' '/media/developer/apps/usr/palm/packages/$APP_ID.service' '$INITD'
for t in /var/luna-service2 /var/luna-service2-dev; do
  rm -f \"\$t/services.d/$APP_ID.service\" \"\$t/services.d/$APP_ID.service.service\" \"\$t/roles.d/$APP_ID.service.json\" \"\$t/roles.d/$APP_ID.service.service.json\" \"\$t/roles.d/$APP_ID.app.json\" \"\$t/manifests.d/$APP_ID.json\" \"\$t/client-permissions.d/$APP_ID.app.json\" \"\$t/client-permissions.d/$APP_ID.service.service.json\" \"\$t/client-permissions.d/$APP_ID.service.root.json\" \"\$t/api-permissions.d/$APP_ID.service.api.json\"
done" >/dev/null 2>&1; then
    echo ">> FAIL: removing the files failed (check the password and the TV connection)" >&2
    exit 1
  fi
  tv "ls-control scan-services >/dev/null 2>&1" >/dev/null 2>&1 || true
  echo ">> removed. Reboot the TV so the launcher drops the entry, then run this script again to install."
  exit 0
fi

IPK="${IPK:-$(ls -t "$REPO_ROOT"/dist/*.ipk 2>/dev/null | head -1 || true)}"
if [[ -z "${IPK:-}" || ! -f "$IPK" ]]; then
  echo ">> FAIL: no IPK found; run packaging/build-ipk.sh first or set IPK=" >&2
  exit 1
fi
# `ar` runs inside the temp directory below, so a relative IPK would resolve there.
IPK="$(cd "$(dirname "$IPK")" && pwd)/$(basename "$IPK")"
echo ">> installing $(basename "$IPK") on $TV_HOST"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
(
  cd "$WORK"
  ar x "$IPK"
)

echo ">> unpacking data tarball into /media/developer/apps"
tv 'kill $(pidof takt) 2>/dev/null; sleep 1; true' >/dev/null 2>&1 || true
cat "$WORK/data.tar.gz" | tv "tar -xzf - -C /media/developer/apps"

echo ">> installing LS2 dev files, init.d entry and starting the service"
tv "
set -e
mkdir -p /var/luna-service2-dev
mkdir -p /var/lib/webosbrew/init.d
cp -R '$SERVICE_DIR/ls2/.' /var/luna-service2-dev/
ln -sf '$SERVICE_DIR/90-start_takt' '$INITD'
ls-control scan-services >/dev/null 2>&1 || true
sleep 2
luna-send -n 1 luna://$APP_ID.service/start '{}' >/dev/null 2>&1 || true
" >/dev/null

echo ">> waiting for the local API"
for _ in $(seq 1 30); do
  if tv 'curl -s --max-time 2 http://127.0.0.1:3927/api/status | head -c 60' 2>/dev/null | grep -q '"state"'; then
    echo ">> OK: $(tv 'curl -s --max-time 2 http://127.0.0.1:3927/api/status' 2>/dev/null | head -c 120)"
    break
  fi
  sleep 1
done

if [[ "$REBOOT" == "yes" ]]; then
  echo ">> rebooting the TV so com.webos.applicationManager rescans the app"
  tv 'reboot' >/dev/null 2>&1 || true
  sleep 20
  for _ in $(seq 1 40); do
    if tv 'echo up' >/dev/null 2>&1; then
      echo ">> TV is back"
      break
    fi
    sleep 3
  done
  sleep 20
  echo ">> app list:"
  tv "luna-send -n 1 -f luna://com.webos.applicationManager/listApps '{}' 2>/dev/null" 2>/dev/null \
    | tr ',' '\n' | grep -E '"(id|title)"' | grep -B1 -A1 "$APP_ID" || true
fi

echo ">> done. Launcher name/appinfo changes need --reboot the first time."
