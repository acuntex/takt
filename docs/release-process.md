# Device check and release

Two cycles, one page. Before a change is merged it is checked on the TV; after a release the user
path is checked through the Homebrew Channel. The user-facing install path stays
[`install.md`](install.md) — the scripts named here are maintainer tools, not a supported install
path. The two skills that fire on these tasks are `.opencode/skills/takt-device-check/` and
`.opencode/skills/takt-release/`.

## What "checked on the device" means

A ticked **Behaviour checked on the device** box means the person who ticked it saw the behaviour on
the TV. Tests and CI are not an observation.

Three outcomes are honest:

- **Seen fixed** — the reported symptom was produced or the touched surface was exercised, and it
  behaves as intended.
- **Not reproducible here** — the condition cannot be produced on this TV (for example a server-side
  state the daemon cannot force). The evidence names the test that covers the behaviour instead.
- **Unticked** — nothing was observed, or a changed UI surface has no automated case and was not
  seen. Then the box stays open with the reason.

Anything else is a claim nobody can defend.

## Before merge: the device check

Preconditions:

- Takt is installed on the TV. A fresh TV, or a renamed app, needs the IPK first
  (`packaging/build-ipk.sh`, then
  `TV_HOST=<tv-ip> TV_PASSWORD=... IPK=dist/com.github.acuntex.takt_<version>_all.ipk bash scripts/install-tv.sh --reboot`)
  — after asking — and the launcher entry only appears after that boot.
- `TV_HOST` and `TV_PASSWORD` (or `TV_ASKPASS`) are set in the environment.
- Nobody is watching; ask before interrupting playback.
- `cargo-zigbuild` plus a `zig` binary are available, and Python 3 (the version read in
  `build-ipk.sh`). Without `cargo-zigbuild` or `zig` the build exits 1; `ALLOW_HOST_BUILD=1`
  re-enables a host build for local experiments only and must never be used for a device check — the
  result is not the glibc 2.24 artifact.
- GNU `ar` (used by `install-tv.sh` and `build-ipk.sh`) and Node 22 (`scripts/tv/inspect.js` uses
  the built-in `WebSocket`; Node 20 fails there).
- CDP port `9998` is reachable on the TV (the app's dev-mode browser). `scripts/tv/inspect.js` fails
  without it.

Then:

1. If `web/` changed, bump the `?v=` query in `web/index.html` (4 occurrences) and
   `packaging/app-index.html` (2) first — the TV caches by URL, and without the bump a check can
   compare matching values while the TV serves the old assets.

2. Build, and record the artifact:

   ```sh
   unset TAKT_BIN
   packaging/build-ipk.sh
   sha256sum dist/com.github.acuntex.takt_<version>_all.ipk
   ```

   The exact path matters: `dist/` also holds older IPKs, and `install-tv.sh` otherwise takes the
   newest one.

3. Install, then gate on the API:

   ```sh
   TV_HOST=<tv-ip> TV_PASSWORD=... IPK=dist/com.github.acuntex.takt_<version>_all.ipk bash scripts/install-tv.sh
   TV_HOST=<tv-ip> TV_PASSWORD=... bash scripts/tv-ssh.sh 'curl -s --max-time 2 http://127.0.0.1:3927/api/status' | grep -q '"state"' && echo alive || { echo "daemon did not answer" >&2; false; }
   ```

   `install-tv.sh` exits 0 even when the daemon never answers, and piping into `head` would hide the
   failure. Stop when `alive` is not printed. The failure arm uses `false` rather than `exit 1`
   because these lines are pasted into an interactive shell.

4. Close and reopen Takt, with permission. An open page keeps serving the old assets:

   ```sh
   TV_HOST=<tv-ip> TV_PASSWORD=... bash scripts/tv-ssh.sh 'luna-send -n 1 luna://com.webos.applicationManager/closeByAppId "{\"id\":\"com.github.acuntex.takt\"}"'
   TV_HOST=<tv-ip> TV_PASSWORD=... bash scripts/tv-ssh.sh 'luna-send -n 1 luna://com.webos.applicationManager/launch "{\"id\":\"com.github.acuntex.takt\"}"'
   ```

   Retry the next step for up to 30 seconds: the wrapper polls, then redirects to `127.0.0.1:3927`,
   and CDP only sees the app page after that. The wrapper's "Takt needs one restart" page is a
   failed check, not a wait.

5. Inspect:

   ```sh
   TV_HOST=<tv-ip> node scripts/tv/inspect.js
   ```

   The page URL (the wrapper's target from `packaging/app-index.html`) and every JS/CSS asset in the
   `resources` line that carries a `?v=` query — `style.css`, `progress.js`, `visual.js`, `app.js` —
   must match their source: the URL from `packaging/app-index.html`, the queries from
   `web/index.html`. When `web/` changed those values are new; for a daemon-only change they stay
   the same, and that is recorded as such. The font `anton.ttf` is loaded without a query.

6. Exercise the touched surface. Write the evidence block into a PR comment.

7. Tick the box and merge.

`scripts/deploy.sh` is the shorter binary-only path: it copies the binary at
`daemon/target/<target>/release/takt` — building only when none exists — and honours `TAKT_BIN`, so
`unset TAKT_BIN` and `packaging/build-ipk.sh` come first. It verifies `/api/status` and fails when
the daemon is dead. It is enough for a daemon-only change and never proves that the package
installs.

## The evidence blocks

Two blocks, because the two cycles test different artifacts. Both name the IPK by its basename only:

```
Device check
- build: build-ipk.sh <ipk basename> sha256 <hash>
- asset: <viceNN> (inspect.js page URL and resources line)
- seen: <what was exercised, on which screen>
- not reproducible: <symptom> — <why>, covered by <test>
- TV left: <app open / daemon running / volume unchanged>
```

```
Channel check
- install: build-channel-ipk.sh <ipk basename> sha256 <hash>
- version: <version> (appinfo.json read from the TV)
- asset: <viceNN> (inspect.js page URL and resources line)
- seen: <what was exercised, on which screen>
- MA approval: still present
- TV left: <app open / daemon running / volume unchanged>
```

The builder matters: `build-ipk.sh` is what the device check installs, `build-channel-ipk.sh` is
what the Channel gets. Only these blocks go into a public comment or release note — no install
output, no host, no credentials, no `players` JSON from `scripts/ma/ma.mjs`, and no `dist/` prefix
or absolute path.

## What must change per release

| File                                                     | What                                                        |
| -------------------------------------------------------- | ----------------------------------------------------------- |
| `packaging/appinfo.json`                                 | `version` — the only source                                 |
| `daemon/Cargo.toml`                                      | `version`, must match                                       |
| `daemon/Cargo.lock`                                      | the `takt` entry, or every `--locked` job fails             |
| `packaging/setup.sh`                                     | the `"version"` line of the LS2 manifest it writes at start |
| `packaging/ls2/manifests.d/com.github.acuntex.takt.json` | `version`                                                   |
| `repo/apps.json`                                         | generated by `packaging/gen-repo.sh <tag>`                  |
| `dist/<id>.manifest.json`                                | generated by the same script                                |
| README badge                                             | automatic from the release                                  |

`scripts/version-check.js` fails when any of the five copies disagrees. The release asset is
`dist/channel/<id>_<version>_all.ipk` from `packaging/build-channel-ipk.sh`; the IPK that
`build-ipk.sh` writes must not be published — it is the one the device check installs.

## Preconditions for a release

_Release order_ builds and publishes, _The Channel check itself_ tests the user path. You need: GNU
`ar` (used by `build-ipk.sh` and the generator), Node 22 (`inspect.js`, `ma.mjs`), `cargo-zigbuild`,
`zig`, `npx` with network access (the Channel IPK is packaged with `@webos-tools/cli@3.2.6`), Python
3 (`build-channel-ipk.sh` and the generator read `appinfo.json` with it), an authenticated `gh`, and
for the TV step `TV_HOST`, `TV_PASSWORD` (or `TV_ASKPASS`) and nobody watching. Refreshing the
repository, updating and opening Takt is asked for, like every TV action.

Run every command from the repository root (`<repo>`); the scripts and the default `TV_HELPER`
resolve relative to it.

## Release order

0. Start from a clean, current `main`. Both builders package the working tree (the binary embeds
   `web/`), so an uncommitted change would ship inside an artifact the tag does not contain:

   ```sh
   git switch main && git pull --ff-only
   test -z "$(git status --porcelain)" || { echo "working tree is dirty; commit or stash first" >&2; false; }
   ```

1. `main` is protected: it takes changes only through a pull request with green CI, administrators
   included. Bump the five version places in one commit on a branch, run
   `node scripts/version-check.js`, open the PR, wait for CI and merge it:

   ```sh
   git switch -c release/v<version>
   # bump the five places, then:
   node scripts/version-check.js
   git add packaging/appinfo.json daemon/Cargo.toml daemon/Cargo.lock packaging/setup.sh \
     packaging/ls2/manifests.d/com.github.acuntex.takt.json
   git commit -m "chore(release): v<version>"
   git push -u origin release/v<version>
   gh pr create -R acuntex/takt --fill
   gh pr checks --watch -R acuntex/takt \
     && gh pr merge --squash --delete-branch -R acuntex/takt
   ```

   If `gh pr checks` answers "no checks reported", the workflows have not started yet: wait a few
   seconds and run the two chained lines again.

   Then return to the merged `main` and record the commit the release will be cut from. A squash
   merge creates a new commit, so take the hash from `main`, not from the branch:

   ```sh
   git switch main && git pull --ff-only
   SHA="$(git rev-parse HEAD)"
   ```

2. `unset TAKT_BIN`, `packaging/build-ipk.sh` (builds the ARM binary), then
   `packaging/build-channel-ipk.sh` (packages it). Record the artifact:

   ```sh
   sha256sum dist/channel/com.github.acuntex.takt_<version>_all.ipk
   stat -c '%s' dist/channel/com.github.acuntex.takt_<version>_all.ipk
   ```

3. `packaging/gen-repo.sh v<version>` — it validates the tag, the file name and the package inside
   the archive, and writes `dist/<id>.manifest.json` and `repo/apps.json`.
4. Check the generated index against the IPK before anything is pushed; this must print
   `index matches ipk`:

   ```sh
   python3 -c 'import hashlib,json,sys; raw=open(sys.argv[1],"rb").read(); m=json.load(open("repo/apps.json"))["packages"][0]["manifest"]; assert m["ipkSize"]==len(raw) and m["ipkHash"]["sha256"]==hashlib.sha256(raw).hexdigest(), "index does not match the IPK"; print("index matches ipk")' dist/channel/com.github.acuntex.takt_<version>_all.ipk
   ```

5. Publish, pinned to the commit whose IPK was just built:

   ```sh
   gh release create v<version> -R acuntex/takt --target "$SHA" \
     --title "Takt <version>" --generate-notes \
     dist/channel/com.github.acuntex.takt_<version>_all.ipk \
     dist/com.github.acuntex.takt.manifest.json
   git fetch --tags origin
   git rev-parse "v<version>^{commit}"   # must equal $SHA
   ```

   Without `--target`, GitHub tags whatever the default branch points at now instead of the commit
   the IPK was built from.

6. Download the published assets again and compare — the hash and size must equal step 2 and the
   values in `repo/apps.json` — then put `repo/apps.json` on `main` through a pull request, since
   direct pushes are blocked. The modified file (step 3) moves with you onto the new branch:

   ```sh
   CHECK="$(mktemp -d)"
   gh release download v<version> -R acuntex/takt -D "$CHECK" -p '*.ipk' -p '*.manifest.json'
   sha256sum "$CHECK"/*.ipk
   stat -c '%s %n' "$CHECK"/*.ipk
   test "$(git status --porcelain --untracked-files=no)" = " M repo/apps.json" \
     || { echo "only repo/apps.json may be modified here" >&2; false; }
   git switch -c release/index-v<version>
   git add repo/apps.json
   git commit -m "chore(release): point the Channel repo at v<version>"
   git push -u origin release/index-v<version>
   gh pr create -R acuntex/takt --fill
   gh pr checks --watch -R acuntex/takt \
     && gh pr merge --squash --delete-branch -R acuntex/takt \
     && git switch main && git pull --ff-only
   ```

   Same as in step 1: on "no checks reported", wait a moment and run the chained lines again.

7. Wait until the raw index serves the new version before touching the TV:

   ```sh
   found=0
   for i in $(seq 1 40); do
     curl -fsS https://raw.githubusercontent.com/acuntex/takt/main/repo/apps.json | grep -q '"version": "<version>"' && { found=1; break; }
     sleep 15
   done
   [ "$found" -eq 1 ] || { echo "index still serves the old version" >&2; false; }
   ```

## The Channel check itself

With permission, and only when nobody is watching:

1. Homebrew Channel: refresh the repository, update Takt.
2. Open the app once. The update recreates the service directory, and `setup.sh` reinstalls the
   descriptor and the fallback copy on that start.
3. Confirm the installed version:

   ```sh
   TV_HOST=<tv-ip> TV_PASSWORD=... bash scripts/tv-ssh.sh 'cat /media/developer/apps/usr/palm/applications/com.github.acuntex.takt/appinfo.json'
   ```

4. Confirm the assets:

   ```sh
   TV_HOST=<tv-ip> node scripts/tv/inspect.js
   ```

   The page URL and each versioned asset must match the built UI — the page URL from
   `packaging/app-index.html`, the asset queries from `web/index.html`. A new value when `web/`
   changed, the unchanged one otherwise.

5. Confirm that Music Assistant still sees the player:

   ```sh
   MA_WS_URL=ws://<music-assistant-host>:8095/ws MA_TOKEN_FILE=<token-file> node scripts/ma/ma.mjs players
   ```

   Look for the Takt entry by name. Never paste that output anywhere — it carries player ids.

6. Exercise the touched surface, then record the _Channel check_ block in the release notes and in
   `.work/HANDOFF.md` (create the file and its directory if absent — `.work/` is git-ignored and
   missing in a fresh clone):

   ```sh
   NOTES="$(mktemp)"
   gh release view v<version> -R acuntex/takt --json body --jq .body >"$NOTES" && test -s "$NOTES" || { echo "could not read the release body" >&2; false; }
   BLOCK="$(printf '%s\n' \
     'Channel check' \
     '- install: build-channel-ipk.sh <ipk basename> sha256 <hash>' \
     '- version: <version> (appinfo.json read from the TV)' \
     '- asset: <viceNN> (inspect.js page URL and resources line)' \
     '- seen: <what was exercised, on which screen>' \
     '- MA approval: still present' \
     '- TV left: <app open / daemon running / volume unchanged>')"
   printf '\n%s\n' "$BLOCK" >>"$NOTES"
   mkdir -p .work
   printf '%s\n' "$BLOCK" >>.work/HANDOFF.md
   gh release edit v<version> -R acuntex/takt --notes-file "$NOTES"
   ```

   A release check has no PR to comment on.

## When it fails

A failed Channel check is fixed by a patch release; the published `repo/apps.json` is never
hand-edited. If the release dies before the index PR is merged, the Channel keeps serving the
previous version — finish the release or delete it with
`gh release delete v<version> -R acuntex/takt --cleanup-tag` (the flag removes the tag too,
otherwise a retry stops at "tag already exists"), then restore the index and let the generator run
again for the next attempt. Before the index branch exists that is `git checkout -- repo/apps.json`;
after step 6 created it, run `git switch main`, `git branch -D release/index-v<version>` (and
`git push origin --delete release/index-v<version>` if it was pushed) first. `dist/` is ignored and
must be rebuilt. Never leave a half-published index.

## Local versus CI

CI runs `lint` (`cargo fmt`, `cargo clippy --all-targets --all-features --locked`), `test`
(`cargo test --locked`), `arm` (the ARMv7 cross-build), `web` (syntax, helper tests, skills-check,
version check), `ui` (headless Chrome), `format` (pinned Prettier) and `secrets` (gitleaks).

Local and only local: `shellcheck` (a pre-commit hook, not a CI job), the `build-ipk.sh` packaging
step, a deploy and the Channel check.

Every script path named here exists in `scripts/`, `packaging/` or `.opencode/commands/`. Inline
commands stay close to their owners: the TV shell (`luna-send`, `curl`), `gh`, `git`, `sha256sum`,
`stat`, `rm`, `node` for the two harnesses, the one Python check, and plain shell glue (`grep`,
`seq`, `sleep`, `unset`, `[`, `echo`).
