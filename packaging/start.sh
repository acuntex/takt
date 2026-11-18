#!/bin/sh
#
# Start the Takt native Luna service.
#
# Launched by the LS2 hub via services.json ("engine":"native","executable":"start.sh").
# Sets the service environment required by the daemon (PulseAudio socket, minimal PATH)
# and execs the binary, writing its log to /tmp for observability.

BASEDIR="/media/developer/apps/usr/palm/services/com.github.acuntex.takt.service"

export PATH=/usr/bin:/bin
export PULSE_SERVER=unix:/var/run/pulse/native
export HOME=/media/internal

exec "$BASEDIR/takt" > /tmp/takt.log 2>&1
