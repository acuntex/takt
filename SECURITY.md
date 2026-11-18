# Security policy

This is a hobby project without a security team and without a bug bounty. Reports are handled on a
best-effort basis by the maintainer.

## Scope

In scope:

- the daemon (`daemon/`) and the way it handles the Sendspin session: the Noise handshake, the
  `KKpsk2` cipher suite, the guest/Sentinel PSK, and the client identity it stores;
- the HTTP API on port `3927`, in particular the endpoint that is meant to be loopback-only
  (`/api/pin`) and the PIN protection of the writing endpoints (`/api/transport`, `/api/config`);
- credential handling: the boot PIN, the Music Assistant token, the player identity in
  `/media/internal/takt.json` and anything that ends up in logs or error messages;
- the packaged service and its permissions on the TV (init script, LS2 dev files, app wrapper).

Out of scope:

- a rooted TV, its webOS installation, Homebrew Channel, SSH configuration or any other pre-existing
  root access — an attacker with root already has everything;
- Music Assistant itself, the Sendspin protocol specification, or the `syncspin` crates; report
  those to their own projects;
- denial of service against a TV you control, and issues that require physical access to the device;
- missing hardening that has no concrete attack path (for example "the service runs as root on a TV
  where everything runs as root").

## Reporting a vulnerability

Do **not** open a public issue for a vulnerability. Use GitHub's private reporting: open the
repository's **Security** tab and choose **Report a vulnerability** (GitHub security advisories).
That channel is private between you and the maintainer.

## What a report should contain

- the commit or version you tested, and whether the binary was built locally or from an IPK;
- TV model and webOS version, and the Music Assistant version;
- what you expected and what happened, with the steps that reproduce it;
- relevant lines from `/tmp/takt.log` and the output of `GET /api/status`, with the PIN, tokens, the
  public key and any private path redacted;
- your assessment of the impact, and whether the issue is reachable from the LAN, from the loopback
  page only, or only with physical access.

## What to expect

An acknowledgement as soon as the maintainer sees the report, usually within a few days. There is no
response-time guarantee, no embargo process and no reward. Fixes land on `main`; a coordinated
disclosure date can be agreed in the advisory if you want one.
