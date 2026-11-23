---
description: Run all local checks (UI, harness, daemon) before committing
---

Run the full local verification for $ARGUMENTS (default: the current tree) and report each result.
Stop and fix the first failure before continuing:

1. `node --check web/app.js`, `node --check web/visual.js`, `node --check web/progress.js`
2. `node scripts/test-visual.js` and `node scripts/test-progress.js`
3. `node scripts/version-check.js`
4. `node scripts/skills-check.js`
5. `npx prettier --check .`
6. `node scripts/tv/ui-test.js`
7. `cd daemon && cargo fmt --check && cargo test --locked && cargo clippy --all-targets --all-features --locked -- -D warnings`

Summarize pass/fail per step. Do not touch the TV; these checks are local.
