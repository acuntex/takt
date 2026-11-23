---
description: Build the ARM binary and IPK and install it on the TV
---

Build and deploy $ARGUMENTS (default: the current tree) to the TV:

1. Bump the `?v=` asset query in `web/index.html` and `packaging/app-index.html` if `web/` changed.
2. `packaging/build-ipk.sh` and record `sha256sum dist/com.github.acuntex.takt_<version>_all.ipk`.
3. `TV_HOST=<tv-ip> TV_PASSWORD=... IPK="$PWD/dist/com.github.acuntex.takt_<version>_all.ipk" bash scripts/install-tv.sh`
   (add `--reboot` only if the app id/appinfo name changed; ask the user first).
4. Verify with `TV_HOST=<tv-ip> node scripts/tv/inspect.js` and report the page URL/version, theme
   and body class.

The full procedure, including the evidence a device check needs, is
[`docs/release-process.md`](../../docs/release-process.md).

Never reboot or relaunch the app while the user is watching something. If the TV is in use, stop
after step 2 and report that the install is pending.
