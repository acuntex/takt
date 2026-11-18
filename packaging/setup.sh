#!/bin/sh
#
# Channel self-setup, run once from the app through the Homebrew Channel's exec bridge.
#
# The installer already writes a usable LS2 manifest for the app and the service, but the service
# descriptor it writes wraps the binary in `jailer -t native_devmode`; that sandbox has no CSPRNG
# and cannot read /media/internal, so the daemon dies there. This script replaces the descriptor
# with the packaged, unjailed one, keeps the app's Luna role in the manifest (without it the app
# gets no reply from any Luna call), gives the daemon its identity, links the boot script and
# starts the service. It is idempotent and logs to /tmp/takt-setup.log.

set -e
exec >>/tmp/takt-setup.log 2>&1
echo "[setup] $(date) start"

SERVICE=com.github.acuntex.takt.service
APP=com.github.acuntex.takt
SRC=/media/developer/apps/usr/palm/services/$SERVICE

# The native service runs unjailed but the jail used to hide /media/internal; keep the daemon's
# own copy so the first start never depends on the sandbox rules.
if [ ! -f "$SRC/takt.json" ] && [ -f /media/internal/takt.json ]; then
    cp -f /media/internal/takt.json "$SRC/takt.json"
    chown 1000:1000 "$SRC/takt.json" 2>/dev/null || true
    chmod 600 "$SRC/takt.json"
    echo "[setup] identity copied once to $SRC/takt.json"
fi

for TREE in /var/luna-service2 /var/luna-service2-dev; do
    for dir in services.d roles.d manifests.d client-permissions.d api-permissions.d; do
        mkdir -p "$TREE/$dir"
    done

    cp -R "$SRC/ls2/roles.d/." "$TREE/roles.d/"
    cp -R "$SRC/ls2/client-permissions.d/." "$TREE/client-permissions.d/"
    cp -R "$SRC/ls2/api-permissions.d/." "$TREE/api-permissions.d/"
    cp -f "$SRC/ls2/services.d/$SERVICE" "$TREE/services.d/$SERVICE"

    # The installer's descriptor jails the binary and its role file duplicates the packaged one.
    rm -f "$TREE/services.d/$SERVICE.service" "$TREE/roles.d/$SERVICE.service.json"

    # The manifest is what makes the hub load the roles: a role file that no manifest references is
    # never read, and the app then has no Luna permission at all.
    cat >"$TREE/manifests.d/$APP.json" <<EOF
{
  "id": "$APP",
  "version": "0.1.0",
  "serviceFiles": ["$TREE/services.d/$SERVICE"],
  "roleFiles": [
    "$TREE/roles.d/$SERVICE.json",
    "$TREE/roles.d/$APP.app.json"
  ],
  "apiPermissionFiles": ["$TREE/api-permissions.d/$SERVICE.api.json"],
  "clientPermissionFiles": [
    "$TREE/client-permissions.d/$SERVICE.service.json",
    "$TREE/client-permissions.d/$SERVICE.root.json",
    "$TREE/client-permissions.d/$APP.app.json"
  ]
}
EOF
done

# The installer also registers a separate service package directory; with the normalised files in
# place it is a duplicate and confuses the hub.
rm -rf "/media/developer/apps/usr/palm/packages/$SERVICE"

mkdir -p /var/lib/webosbrew/init.d
ln -sf "$SRC/90-start_takt" /var/lib/webosbrew/init.d/90-start_takt

if command -v ls-control >/dev/null 2>&1; then
    ls-control scan-services >/dev/null 2>&1 || true
else
    /usr/sbin/ls-control scan-services >/dev/null 2>&1 || true
fi
sleep 1

# The setup can run before the hub picked the new manifest up; the start is retried by the boot
# script and by the next app launch, so a failure here is not fatal.
luna-send -n 1 -w 5000 "luna://$SERVICE/start" '{}' >/dev/null 2>&1 || true

echo "[setup] done"
