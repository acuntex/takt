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

On that first start the app copies its identity next to the service, registers the service and
starts the daemon. Then:

- **Daemon answers** → the UI opens and Music Assistant can stream to Takt.
- **Daemon does not answer** → **“Takt needs one restart”** appears with **Reboot now** / **Later**.
  After the reboot the daemon starts by itself, and Music Assistant can stream without opening the
  app again.

The identity copy is **one-time**: later starts and updates keep the settings made on the TV
(`/media/internal/takt.json` is only the source for the first copy; after that the service keeps its
own file).

## Uninstall

Remove Takt in the Homebrew Channel. The service directory (including its `takt.json` identity) is
removed with the app; if you want to keep the Music Assistant approval for a later install, back up
the service's `takt.json` first.

## Updates

Install the newer IPK from the same repository. The service and its settings stay; the launcher
rescan needs a reboot only when the app id or name changes.

_Developers building or deploying from this repository use `CONTRIBUTING.md`;
`scripts/install-tv.sh` and `scripts/deploy.sh` are maintainer tools, not the supported install
path._
