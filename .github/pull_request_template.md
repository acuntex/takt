## What changes

<!-- One or two sentences: what is different afterwards, and why. -->

## Checks

- [ ] `cargo test --locked` (in `daemon/`) green.
- [ ] `cargo fmt --check` and `cargo clippy --all-targets --all-features --locked -- -D warnings`
      clean.
- [ ] `node --check web/app.js && node --check web/visual.js && node --check web/progress.js` and
      the helper tests green.
- [ ] `node scripts/version-check.js` green.
- [ ] `node scripts/tv/ui-test.js` green.
- [ ] `npx prettier --check .` clean.
- [ ] No secret, no player identity and no private path in the diff.
- [ ] UI change: `?v=` bumped in `web/index.html` and `packaging/app-index.html`.
- [ ] Behaviour checked on the device — evidence in a comment, per
      [docs/release-process.md](https://github.com/acuntex/takt/blob/main/docs/release-process.md);
      or explicitly named as unchecked.
