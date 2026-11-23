---
name: takt-device-check
description:
  Use when a change to Takt must be checked on the TV before merge — ticking "Behaviour checked on
  the device" in a PR, verifying a fix on the real LG webOS TV, deploying a branch for a device
  check, or recording device evidence.
---

# Device check

A ticked **Behaviour checked on the device** box means the person who ticked it saw the behaviour on
the TV. Tests and CI are not an observation.

## Before anything runs

Ask for `TV_HOST` and the credential (`TV_PASSWORD`, or a `TV_ASKPASS` helper), and ask for
permission to interrupt playback and to close and reopen the app. The TV is shared; nobody watching
is a precondition, not a courtesy.

## The three honest outcomes

- **Seen fixed** — the symptom was produced or the touched surface was exercised, and it behaves.
- **Not reproducible here** — the condition cannot be produced on this TV; the evidence names the
  test that covers the behaviour instead.
- **Unticked** — nothing was observed, or a changed UI surface has no automated case and was not
  seen. Say why in the box's own line.

## What makes a check invalid

These are not optional, and each one has silently passed a check before:

- The app was not closed and reopened after the install, so the page still serves the old assets.
- The page URL's `?v=` matches while a versioned asset's does not. Compare both observed values from
  the inspection output with the expected ones: the page URL in `packaging/app-index.html`, the
  asset queries in `web/index.html`.
- `TAKT_BIN` was exported, so the build packaged yesterday's binary.
- The `build-ipk.sh` artifact was confused with the Channel one. Both are called
  `com.github.acuntex.takt_<version>_all.ipk`; the device check installs the `build-ipk.sh` one.
- `install-tv.sh`'s exit status was trusted — it returns 0 even when the daemon never answers. The
  API needs its own gate.
- The evidence claims a test that does not cover the changed file.

For the deeper stale-UI hunt — CDP port, theme state, wrapper versus asset query — follow
`webos-tv-debugging`.

## Evidence

The evidence block goes into a PR comment, and it carries the IPK basename and hash, the asset
version, what was seen, what was not reproducible and with which test, and how the TV was left. No
host, no credentials, no `players` JSON — and no `dist/` path or absolute path.

## Where the commands are

Read `docs/release-process.md`, sections _What "checked on the device" means_ and _Before merge: the
device check_, for the commands. Do not re-derive them from memory.
