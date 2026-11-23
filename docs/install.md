# Install

Takt is installed from the **Homebrew Channel** as an IPK. There is no SSH step for users.

## What you need

- A rooted LG TV with webOS 4.x and the Homebrew Channel.
- Music Assistant 2.10.x on the same network.
- Nothing else: no terminal, no token, no account.

## Add the repository

1. Open **Homebrew Channel → Settings → Repositories → Add repository**.
2. Enter `https://raw.githubusercontent.com/acuntex/takt/main/repo/apps.json` (the Channel loads the
   URL verbatim, so it has to be exactly this address).
3. Open the repository list and install **Takt**.

Takt also appears in the central `webosbrew/apps-repo` once it is listed there.

## First start

The install never opens Takt. Start it from the launcher when you want it.

On that first start the app runs `setup.sh`, which registers the service and starts the daemon; the
daemon creates `/media/internal/takt.json`. Then:

- **Daemon answers** → the UI opens and Music Assistant can stream to Takt.
- **Daemon does not answer** → **“Takt needs one restart”** appears with **Reboot now** / **Later**.
  After the reboot the daemon starts by itself, and Music Assistant can stream without opening the
  app again.

The identity copy is **not** the master: the daemon creates and keeps its configuration in
`/media/internal/takt.json`. `setup.sh` copies it next to the service binary when that sibling is
missing, as the fallback for the jailed service — on an update the service directory is recreated
and the first start after the update makes the copy again. Settings and the Music Assistant approval
live in `/media/internal/takt.json` and survive updates and reinstalls.

## Uninstall

Remove Takt in the Homebrew Channel. The app and service directories are removed;
`/media/internal/takt.json` stays, so the Music Assistant approval survives a later reinstall. Back
that file up only before reflashing or resetting the TV.

## Updates

Install the newer IPK from the same repository. The update recreates the service directory, so open
Takt once afterwards to reinstall the service descriptor; the settings and the Music Assistant
approval stay. The launcher rescan needs a reboot only when the app id or name changes.

_Developers building or deploying from this repository use `CONTRIBUTING.md`;
`scripts/install-tv.sh` and `scripts/deploy.sh` are maintainer tools, not the supported install
path._
