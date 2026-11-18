---
name: webos-tv-debugging
description:
  Use when the TV UI looks stale or wrong, animations stutter, a deployed change is not visible, the
  app is not reachable, or theme/leave behaviour must be verified on the real LG webOS TV.
---

# Debugging the TV UI

The app runs in the TV's Chromium at `http://127.0.0.1:3927/`. The host reaches it two ways:

- HTTP API: `http://<tv-ip>:3927/api/status`
- Chrome DevTools Protocol: `http://<tv-ip>:9998/json` (dev-mode browser)

Both need the TV to be reachable and `TV_HOST` set; SSH needs `TV_PASSWORD` (see
`webos-tv-manual-install`).

## Harnesses (host, Node 22, no npm deps)

```sh
export TV_HOST=<tv-ip>                 # no default
export TV_PASSWORD=...                 # never commit it
scripts/tv-ssh.sh 'curl -s http://127.0.0.1:3927/api/status'

node scripts/tv/inspect.js             # page URL, state, title fit, sleeve and rail geometry
node scripts/tv/inspect.js --shot /tmp/tv.png
node scripts/tv/fps.js 4               # rAF sample with/without the artwork plate
node scripts/tv/shots.js /tmp tv       # three screenshots 1.5 s apart
node scripts/tv/ui-test.js             # headless UI/theme/drawer checks
node scripts/ma/ma.mjs players         # Music Assistant player states
```

## First questions on a stale UI

1. **Is the page URL the current asset version?** `inspect.js` prints it; it must match the `?v=`
   query in `web/index.html` / `packaging/app-index.html` (assets are embedded in the binary and
   served with `no-store`).
2. **Is the running binary the new one?**
   `TV_HOST=<tv-ip> TV_PASSWORD=... scripts/tv-ssh.sh 'strings /media/developer/apps/usr/palm/services/com.github.acuntex.takt.service/takt | grep -o "takt[0-9]*"'`.
   UI changes are `include_str!`-embedded, so a rebuild and reinstall are mandatory.
3. **Did the app reload?** Close and relaunch it via
   `luna://com.webos.applicationManager/closeByAppId` / `launch`.
4. **Is the old process still alive?** `pidof takt`; `cat /tmp/takt.log`.

## Reading fps.js

rAF counts on this TV are noisy (external load, measurement window). Compare the two numbers in the
**same** run, never across runs. The Vice stage animates one sleeve transform and paints one blurred
plate shadow; the target is >= 50 fps at 1080p. If it comes in under, cut the plate's 120 px shadow
first (the next lever), not the type. See `webos-chromium53-ui` for measured costs.

## Verify theme + leave behaviour

- `inspect.js` prints `body` (`state-playing`/`state-paused`, `palette-*`, `is-idle`,
  `settings-open`), the fitted title size, the sleeve transform and the rail rects.
- For leave-the-app: play something, switch to another TV app. The daemon owns the rule through a
  held foreground subscription; `cat /tmp/takt.log` must show one
  `[takt] leaving: foreground is <app>; pausing` line and `api/status` `state: paused`,
  `muted: false`; `ma.mjs players` agrees. Only the first foreign app after a return to Takt pauses.
- `GET /api/foreground` reports the same foreground answer the watchdog saw; the page never polls it
  itself.
- `ui-test.js` covers the Vice stage geometry, the three palettes, the drawer, the volume numeral,
  the idle frame, the credits hook and the page's silence when it is hidden (hiding or blurring
  sends no leave request) without touching the TV.

## Do not disturb

The TV is shared with the user. Do not launch, close, reboot or change volume while the user is
watching something. Ask first when a reboot is required (new app or changed app name).
