---
name: webos-tv-manual-install
description:
  Use when installing, updating, uninstalling, or re-registering the Takt app on a rooted LG webOS
  TV - especially when appInstallService/dev/install fails with -5 (Failed to extract package) for
  an unsigned IPK, when the launcher still shows an old app name, or when the app is missing from
  listApps after an uninstall.
---

# Manual install on the rooted LG webOS TV

The IPK is unsigned. `luna://com.webos.appInstallService/dev/install` therefore reports
`-5: Failed to extract package` and never registers the package. Installation must be done manually
over SSH; a reboot is only needed to make a **new or renamed** app show up in the launcher, because
`com.webos.applicationManager` rescans installed apps at boot.

## Fast path

```sh
cd <repo>                                   # the repository root
packaging/build-ipk.sh                      # builds the ARM binary and the IPK
TV_HOST=<tv-ip> TV_PASSWORD=... IPK="$PWD/dist/com.github.acuntex.takt_<version>_all.ipk" bash scripts/install-tv.sh             # update only (no reboot)
TV_HOST=<tv-ip> TV_PASSWORD=... IPK="$PWD/dist/com.github.acuntex.takt_<version>_all.ipk" bash scripts/install-tv.sh --reboot  # after a rename / first install
TV_HOST=<tv-ip> TV_PASSWORD=... bash scripts/install-tv.sh --uninstall                                                        # remove app, service, LS2 and init files
```

`TV_HOST` is required, and so is `TV_PASSWORD` (or `TV_ASKPASS`) for the SSH helper; `TV_HELPER`
defaults to `scripts/tv-ssh.sh` in the repository, and `IPK` overrides the newest `dist/*.ipk`.

## What the script does (and why)

1. Extracts the IPK **on the host** with GNU `ar`; the TV's BusyBox `ar` fails with
   `ar: unrecognized file type`.
2. Streams `data.tar.gz` to the TV:
   `cat data.tar.gz | tv-ssh 'tar -xzf - -C /media/developer/apps'`.
3. Installs the LS2 dev files: `cp -R $SERVICE_DIR/ls2/. /var/luna-service2-dev/`.
4. Links the boot entry:
   `ln -sf /media/developer/apps/usr/palm/services/com.github.acuntex.takt.service/90-start_takt /var/lib/webosbrew/init.d/90-start_takt`.
5. `ls-control scan-services`, then starts the native service with
   `luna-send -n 1 luna://com.github.acuntex.takt.service/start '{}'` (the init.d script does the
   same).
6. Waits up to 30 s for `http://127.0.0.1:3927/api/status` to answer with JSON containing `"state"`.
   It does not fail when the API stays silent, so gate on the response separately.

## Reboot rule

- **Binary / web asset update:** no reboot. Relaunch the app, assets are embedded and versioned.
- **New app, new app ID, or appinfo title/vendor change:** the launcher and `listApps` cache the old
  metadata. Reboot the TV once (`scripts/install-tv.sh --reboot`). The manual install itself is
  already complete before the reboot; the reboot only re-registers the package.
- After a reboot the init.d entry starts the service automatically.

## Verification

```sh
TV_HOST=<tv-ip> TV_PASSWORD=... scripts/tv-ssh.sh 'curl -s --max-time 2 http://127.0.0.1:3927/api/status | head -c 200'
TV_HOST=<tv-ip> TV_PASSWORD=... scripts/tv-ssh.sh 'sleep 2 | luna-send -n 1 -f luna://com.webos.applicationManager/listApps "{}"' \
  | python3 -c 'import json,sys; print([a for a in json.load(sys.stdin)["apps"] if a["id"]=="com.github.acuntex.takt"])'
curl -s --max-time 3 http://<tv-ip>:3927/api/status | head -c 200
```

Expected launcher title is `Takt`, vendor `acuntex`.

## Pitfalls

- Do **not** point the app at the TV's BusyBox `ar`; extract on the host and stream the tar.
- Do not regenerate `/media/internal/takt.json` or the client ID inside it: that is the
  Sendspin/Sentinel identity Music Assistant knows.
- The package ID `com.github.acuntex.takt` is mirrored by the app directory, service directory, LS2
  roles, init.d entry and the constants in `scripts/install-tv.sh`; change them together.
- After `--uninstall` the launcher entry stays until a reboot.
- `Web App` UI changes are compiled into the binary via `include_str!`; rebuild and reinstall.
- Uninstall/install kills the daemon; playback stops. Music Assistant pauses only when another app
  takes the screen (the daemon's held foreground subscription), not when the service is restarted
  manually.
