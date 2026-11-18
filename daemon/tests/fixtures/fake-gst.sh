#!/bin/sh
# Fake gst-launch-1.0 decoder for process-supervision tests (Task 6).
#
# Behavior is controlled by FAKE_GST_MODE:
#   stream (default)  emit FAKE_GST_MARKER on stdout forever (backpressure throttles it)
#   burst             emit FAKE_GST_BURST_BYTES bytes, then stay alive silently
#   file              emit FAKE_GST_PCM_FILE, then stay alive silently
#   silent            stay alive without producing any output
#   delayed           sleep 0.5s, emit FAKE_GST_MARKER once, then stay alive silently
#   stdin             copy encoded stdin to FAKE_GST_STDIN_FILE, stay alive silently
#   exit              exit immediately (after recording the PID)
# It records its PID so the test can cross-check liveness against the supervisor's child
# handle.
#
# Environment:
#   FAKE_GST_PID_FILE    path to write this process's PID to (default /tmp/fake-gst.pid)
#   FAKE_GST_MARKER      marker bytes emitted in stream/delayed mode (default TAKT_PCM_FRAME)
#   FAKE_GST_MODE        stream | burst | file | silent | delayed | stdin | exit (default stream)
#   FAKE_GST_BURST_BYTES bytes emitted in burst mode (default 150000)
#   FAKE_GST_PCM_FILE    raw PCM file emitted in file mode (default /tmp/fake-gst.pcm)
#   FAKE_GST_STDIN_FILE  file receiving the encoded stream in stdin mode (default /tmp/fake-gst.stdin)

pid_file="${FAKE_GST_PID_FILE:-/tmp/fake-gst.pid}"
marker="${FAKE_GST_MARKER:-TAKT_PCM_FRAME}"
mode="${FAKE_GST_MODE:-stream}"
burst_bytes="${FAKE_GST_BURST_BYTES:-150000}"
pcm_file="${FAKE_GST_PCM_FILE:-/tmp/fake-gst.pcm}"
stdin_file="${FAKE_GST_STDIN_FILE:-/tmp/fake-gst.stdin}"

echo "$$" > "$pid_file"

case "$mode" in
    exit)
        exit 0
        ;;
    stdin)
        # Copy the encoded stream so a test can verify exactly what the sink forwarded. fd 3
        # keeps the original stdout (the sink's pipe) open: closing it would look like a
        # decoder exit to the pump even though this fake never produces PCM.
        exec 3>&1
        exec cat > "$stdin_file"
        ;;
    burst)
        # Emit a bounded block, then stay alive without further output. `exec` keeps the
        # `sleep` in this same process so the supervisor reaps it cleanly (no orphan).
        dd if=/dev/zero bs="$burst_bytes" count=1 2>/dev/null
        exec sleep 1000
        ;;
    file)
        # Emit a known raw PCM file, then stay alive silently.
        cat "$pcm_file"
        exec sleep 1000
        ;;
    silent)
        exec sleep 1000
        ;;
    delayed)
        # Emit one marker after a delay so a test can flush while the decoder is blocked
        # reading, then stay alive silently.
        sleep 0.5
        printf '%s' "$marker"
        exec sleep 1000
        ;;
    *)
        # stream: emit the marker forever; pipe backpressure throttles naturally.
        while :; do
            printf '%s' "$marker"
        done
        ;;
esac
