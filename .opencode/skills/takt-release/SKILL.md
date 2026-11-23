---
name: takt-release
description:
  Use when Takt is released — bumping the package version, cutting or publishing a GitHub release,
  regenerating repo/apps.json for the Homebrew Channel, or testing an install or update through the
  Channel on the TV.
---

# Release Takt

A release is done when the Homebrew Channel serves the artifact that was built, and someone has
installed it through the Channel. Publishing alone is not a release.

## Gates that must hold

- **Five version places, one commit.** `packaging/appinfo.json` (the source), `daemon/Cargo.toml`,
  `daemon/Cargo.lock` (the `takt` entry — every `--locked` CI job fails otherwise),
  `packaging/setup.sh` (the LS2 manifest it writes on the first app start) and
  `packaging/ls2/manifests.d/com.github.acuntex.takt.json`. Miss one and the package, the descriptor
  or the lock file disagrees with the release.
- **Two builders, one asset.** `packaging/build-ipk.sh` produces the IPK a device check installs;
  `packaging/build-channel-ipk.sh` produces the one the Channel gets. Only the second is published.
  If `TAKT_BIN` is exported, a build packages that stale binary — clear it.
- **The index is generated, never written.** The generator in `packaging/` validates the tag, the
  file name and the package inside the archive, then writes the manifest and `repo/apps.json`. A
  published `apps.json` is never hand-edited afterwards.
- **Assets before index, index before the TV.** The tag pins the asset URLs; the index points at it;
  only then does the TV see the update. A half-published index means the Channel serves 404s.
- **One app start after an update.** An update recreates the service directory. The app must be
  opened once so `setup.sh` reinstalls the descriptor and the fallback copy; the settings and the
  Music Assistant approval live in `/media/internal/takt.json` and survive.
- **A failed Channel check is a patch release.** Roll forward; do not patch the published index by
  hand.
- **TV rules.** Ask for `TV_HOST` and the credential, ask before refreshing, updating and opening
  the app, and leave the TV as you found it — app open, daemon running, volume untouched.
- **Public notes.** Release notes and comments carry the evidence block only: the IPK basename and
  hash, the version, what was seen. No host, no credentials, no `players` JSON (it carries player
  ids).

## Where the details are

Read `docs/release-process.md`, sections _What must change per release_, _Release order_ and _The
Channel check itself_, for the commands. The evidence block for this cycle is the _Channel check_
block in the page's _The evidence blocks_ section.
