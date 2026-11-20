# Takt

Takt is a native Music Assistant client for LG webOS 4 TVs: a Rust daemon (Noise-encrypted Sendspin,
GStreamer, PulseAudio) plus a dependency-free ES5 TV UI served by the daemon. What is deployed, open
items and next steps: `.work/HANDOFF.md` — local scratch, git-ignored, so the published tree carries
no LAN addresses or home paths. Keep it current.

## Layout

| Path            | Contents                                                                |
| --------------- | ----------------------------------------------------------------------- |
| `daemon/`       | Crate `takt`: protocol, sink, Luna bridge, HTTP API, SSE                |
| `daemon/tests/` | Hermetic integration suites; `fixtures/fake-gst.sh`/`fake-pacat.sh`     |
| `web/`          | TV UI (ES5, no build step), `include_str!`-embedded into the binary     |
| `packaging/`    | IPK build, appinfo, LS2 files, init.d entry                             |
| `scripts/`      | Host harnesses: TV SSH, CDP inspect/fps/shots, MA CLI, headless UI test |
| `.opencode/`    | Project skills and commands; read the relevant skill before the work    |

## Build, test, deploy

```sh
# local checks (same order as /verify); everything here is TV-free
node --check web/app.js && node --check web/visual.js && node --check web/progress.js
node scripts/test-visual.js && node scripts/test-progress.js
node scripts/skills-check.js            # validates .opencode skill/command frontmatter
npx prettier --check .                  # pinned prettier 3.9.10
node scripts/tv/ui-test.js              # headless Chrome; needs google-chrome + Node 22

(cd daemon && cargo fmt --check && cargo test && cargo clippy --all-targets --all-features -- -D warnings)

packaging/build-ipk.sh                  # cargo zigbuild ARMv7 (glibc 2.24) + dist/*.ipk
TV_HOST=<tv-ip> TV_PASSWORD=... bash scripts/deploy.sh  # binary + service files over SSH, no reboot
TV_HOST=<tv-ip> TV_PASSWORD=... bash scripts/install-tv.sh [--reboot]
```

Both scripts require `TV_HOST`; there is no built-in host. `TV_HELPER` defaults to
`$PWD/scripts/tv-ssh.sh`. `install-tv.sh` needs an executable helper (it has no `TV_PASSWORD`
fallback); `deploy.sh` builds its own askpass from `TV_PASSWORD` when no helper is given. A reboot
is only needed for a new or renamed app entry (launcher registration). Both scripts need
`cargo-zigbuild` and a `zig` binary (they look in `PATH` and `~/zig/zig`).

## CI

`.github/workflows/ci.yml` runs the same gates as `/verify` minus the parts that need hardware:
`lint` (fmt + clippy), `test` (cargo), `web` (syntax + helper tests + skills-check), `ui` (headless
Chrome), `format` (Prettier) and `secrets` (gitleaks over history). The ARMv7 build and a deploy
stay local.

Working notes (designs, plans, ledgers) live in `.work/`, which is git-ignored: they are scratch,
not project documentation. `docs/README.md` indexes what is committed.

## Public repository

- This repository is public: no LAN addresses, hostnames, home-directory paths, personal names,
  account names, device identifiers or token-file locations in code, tests, scripts, docs, comments,
  issues or commits.
- Use `<tv-ip>`, `<music-assistant-host>` and `<repo>` in documentation, and environment variables
  (never literals) in scripts.
- Never commit `TV_PASSWORD`, MA tokens, the player identity file or the local `opencode.jsonc`.
- If a value is needed for a test, use the TEST-NET ranges (`192.0.2.0/24`, `198.51.100.0/24`,
  `203.0.113.0/24`).
- When porting code or docs from anywhere else, rewrite the details instead of copying them.

## Things that bite

- Assets are `include_str!`-embedded: after any `web/` change, rebuild the binary. Also bump every
  `?v=` query in `web/index.html` and `packaging/app-index.html` so the launcher wrapper navigates
  to a fresh URL.
- The daemon serves the UI on `0.0.0.0:3927`; the TV app loads `http://127.0.0.1:3927/`. CDP debug
  port is `9998`, the daemon log is `/tmp/takt.log`.
- Writing endpoints (`/api/transport`, `/api/config`) need the 4-digit boot PIN, and `/api/pin`
  answers **loopback only**. From the host you must read the PIN off the TV screen.
- The daemon owns the leave rule: a held Lua foreground subscription pauses on a departure, with a 5
  s query fallback. The page has no leave rule; `GET /api/foreground` stays as the readout.
- The audible level is the PulseAudio sink `pcm_output` (perceptual curve), not the TV master; the
  daemon drives both. `TAKT_PULSE_SINK` and `TAKT_CONFIG` override sink and config path.
- Identifiers are fixed: crate/binary `takt`, app id and LS2 service `com.github.acuntex.takt`,
  service directory `/media/developer/apps/usr/palm/services/com.github.acuntex.takt.service`, init
  script `90-start_takt`, log `/tmp/takt.log`, identity + config `/media/internal/takt.json`.

## Hard constraints

- **ES5 / Chromium 53.** No modules, classes, optional chaining or modern CSS. Icons are inline SVG.
  Verify with `node --check`. See `.opencode/skills/webos-chromium53-ui`.
- **Encrypted Sendspin only** (Noise `KKpsk2`, Sentinel guest). No plaintext fallback.
- **Keep `/media/internal/takt.json`.** A new keypair means MA sees a new player and needs a manual
  re-approval.
- **Volume:** any change up to 40 is allowed; above 40 at most +20 per MA command, and the applied
  value must be reported back. Details in `.opencode/skills/sendspin-player-runtime`.
- **Performance:** animate only `transform`/`opacity`; no masks or transitions on large layers.
  Measure with `node scripts/tv/fps.js 4` before and after a UI change.

## Working agreements

- The TV is shared. Do not launch, close, reboot or change volume while the user is watching; ask
  before a reboot.
- Leave the TV in the state the user had it (app open, volume restored) after manual tests.
- Keep tests hermetic: no test may bind the TV, start audio processes or need the network.
- Conventional Commits; small, verified steps. There is no CI: `.pre-commit-config.yaml` (gitleaks,
  rustfmt, clippy, Prettier, shellcheck) and `/verify` are the gates. Never commit `TV_PASSWORD`,
  the MA token or a local token file. Include the IPK hash when deploying.

## OpenCode assets

- Skills: `.opencode/skills/<id>/SKILL.md`, discovered automatically.
- Commands: `.opencode/commands/` (`/verify`, `/deploy-tv`, `/tv-check`).
- Project MCP `music-assistant` is configured in a local, git-ignored `opencode.jsonc` (template:
  `opencode.example.jsonc`) and in a global config; on-demand `search_tools` discovery, control
  permissions enabled on the MA side so writes reach MA. Its bearer comes from a local 0600 file
  holding the token. MA writes go through `scripts/ma/ma.mjs`.
- Process skills (`brainstorming`, `systematic-debugging`, `test-driven-development`,
  `writing-plans`, `verification-before-completion`, ...) come from a globally pinned plugin. Do not
  vendor or duplicate them here.

## Skills

- `webos-tv-manual-install` - unsigned IPK, manual install, launcher registration/reboot.
- `webos-chromium53-ui` - renderer limits, measured animation costs, theme architecture.
- `webos-tv-debugging` - CDP harnesses, stale-UI checklist, fps interpretation.
- `sendspin-player-runtime` - protocol, volume/mute, sink, app-leave pause.
- `music-assistant-ops` - MA player/queue CLI and test conventions.
