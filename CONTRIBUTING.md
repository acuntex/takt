# Contributing

Takt is a personal project, but the rules apply to me too: small, verified steps instead of large
rewrites.

## Before the first command

- The daemon depends on `syncspin-client` from crates.io, pinned to `=0.5.0`; a local `syncspin`
  checkout is not needed.
- Tools: Rust stable with the `armv7-unknown-linux-gnueabi` target, Node 22, `npx`, Python 3 (read
  by `build-ipk.sh` and the release generator), and for builds and deploys `cargo-zigbuild` plus a
  `zig` binary.
- `TV_PASSWORD` and the MA token files live outside the repository and are never committed.

## Gates

The same commands as in `AGENTS.md` and in CI:

```sh
node --check web/app.js && node --check web/visual.js && node --check web/progress.js
node scripts/test-visual.js && node scripts/test-progress.js
node scripts/version-check.js
node scripts/skills-check.js
npx prettier --check .
node scripts/tv/ui-test.js            # headless Chrome

cd daemon && cargo fmt --check && cargo test --locked \
  && cargo clippy --all-targets --all-features --locked -- -D warnings
```

The ARMv7 cross-build runs in CI (`arm`). The `build-ipk.sh` packaging step stays local. A deploy
and the Channel check stay local too and happen only after asking, because they touch the TV. The
device check and the release follow [`docs/release-process.md`](docs/release-process.md).

## What does not happen here

- No deploy, reboot or volume change while someone is watching TV — ask first.
- No secrets in the repository: neither tokens nor passwords nor the player's identity file.
- No private paths, device names or hostnames in docs or commits that could be published. Working
  notes belong in `.work/` (git-ignored), not in the tree.
- No change to `web/` without bumping the `?v=` query in `web/index.html` and
  `packaging/app-index.html`: the assets are embedded in the binary and the TV caches. The device
  check is described in [`docs/release-process.md`](docs/release-process.md).

## Commits

Conventional Commits (`fix(ui):`, `docs:`, `build:` …), one topic per commit, verification
documented in the commit message or the handoff. `.pre-commit-config.yaml` checks gitleaks, rustfmt,
clippy, Prettier and shellcheck locally.
