#!/usr/bin/env bash
#
# Run a command on the rooted LG TV over SSH.
#
# The TV only has password auth, so credentials are provided at runtime and never
# committed. Export TV_PASSWORD, or point TV_ASKPASS at a helper that prints the
# password on stdout.
#
# Environment:
#   TV_HOST      TV address or hostname (required, no default)
#   TV_PASSWORD  root password (creates a temporary askpass helper)
#   TV_ASKPASS   existing askpass helper (takes precedence)
#   TV_USER      SSH user (default root)
#
# Usage:
#   TV_HOST=<tv-ip> TV_PASSWORD=... scripts/tv-ssh.sh 'curl -s http://127.0.0.1:3927/api/status'
#   cat file | TV_HOST=<tv-ip> TV_PASSWORD=... scripts/tv-ssh.sh 'cat > /tmp/file'

set -euo pipefail

TV_HOST="${TV_HOST:-}"
TV_USER="${TV_USER:-root}"

if [[ -z "$TV_HOST" ]]; then
  echo ">> FAIL: set TV_HOST to the TV address or hostname (see README.md)" >&2
  exit 2
fi

if [[ -z "${TV_ASKPASS:-}" ]]; then
  if [[ -z "${TV_PASSWORD:-}" ]]; then
    echo ">> FAIL: set TV_PASSWORD or TV_ASKPASS" >&2
    exit 1
  fi
  TV_ASKPASS="$(mktemp)"
  trap 'rm -f "$TV_ASKPASS"' EXIT
  cat >"$TV_ASKPASS" <<'ASKPASS'
#!/bin/sh
printf '%s\n' "$TV_PASSWORD"
ASKPASS
  chmod 700 "$TV_ASKPASS"
fi

exec env SSH_ASKPASS="$TV_ASKPASS" SSH_ASKPASS_REQUIRE=force TV_PASSWORD="${TV_PASSWORD:-}" \
  setsid -w ssh \
  -o StrictHostKeyChecking=no \
  -o UserKnownHostsFile=/dev/null \
  -o LogLevel=ERROR \
  -o ConnectTimeout=8 \
  -o PreferredAuthentications=password \
  -o PubkeyAuthentication=no \
  "$TV_USER@$TV_HOST" "$@"
