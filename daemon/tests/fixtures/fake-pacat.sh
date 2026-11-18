#!/bin/sh
# Fake pacat player for process-supervision tests (Task 6).
#
# Behavior is controlled by FAKE_PACAT_MODE:
#   cat   (default)  copy stdin into FAKE_PACAT_OUT_FILE until stdin closes
#   idle             stay alive without reading stdin
#   gated            wait until FAKE_PACAT_GATE_FILE exists, then copy stdin to out_file
#   close            close stdin, then stay alive (breaks the player's writes)
#   exit             exit immediately (after recording the PID)
# It records its PID so the test can cross-check liveness. `exec` keeps the reader in
# this same process so the recorded PID is the process the supervisor kills (no orphan).
#
# Environment:
#   FAKE_PACAT_PID_FILE   path to write this process's PID to (default /tmp/fake-pacat.pid)
#   FAKE_PACAT_OUT_FILE   path to write received PCM to (default /tmp/fake-pacat.out)
#   FAKE_PACAT_GATE_FILE  path whose creation unblocks `gated` mode (default /tmp/fake-pacat.gate)
#   FAKE_PACAT_MODE       cat | idle | gated | close | exit (default cat)

pid_file="${FAKE_PACAT_PID_FILE:-/tmp/fake-pacat.pid}"
out_file="${FAKE_PACAT_OUT_FILE:-/tmp/fake-pacat.out}"
gate_file="${FAKE_PACAT_GATE_FILE:-/tmp/fake-pacat.gate}"
mode="${FAKE_PACAT_MODE:-cat}"

echo "$$" > "$pid_file"

case "$mode" in
    exit)
        exit 0
        ;;
    idle)
        exec sleep 1000
        ;;
    gated)
        # Stay blocked before reading stdin until the test creates the gate file, then
        # drain stdin into out_file in this same process.
        while [ ! -f "$gate_file" ]; do
            sleep 0.02
        done
        exec cat > "$out_file"
        ;;
    close)
        # Close stdin so the player's writes fail with EPIPE while this child stays alive.
        exec 0<&-
        exec sleep 1000
        ;;
    *)
        exec cat > "$out_file"
        ;;
esac
