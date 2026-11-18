---
description: Inspect the running TV UI and measure frame rate
---

Inspect the running app on the TV and summarize:

1. `node scripts/tv/inspect.js` - page URL/version, document title, body state, fitted title size,
   sleeve transform and rail geometry.
2. `node scripts/tv/fps.js 4` - stage vs. plate-hidden frame rate in the same run.
3. If $ARGUMENTS mentions a visual change, `node scripts/tv/shots.js /tmp tv` and read the
   screenshots.

Report: asset version, state/palette, animated true/false, both fps numbers, and anything that looks
stale (version mismatch, old binary, missing elements). Read-only - do not relaunch or reboot the
TV.
