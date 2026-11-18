---
name: sendspin-player-runtime
description: Use when touching the native Sendspin daemon: protocol/noise handshake, playback state, sink, volume or mute handling, reconnect, screensaver veto, or pause/stop behaviour on the LG TV.
---

# Sendspin player runtime

`daemon/` is a native Rust player: Noise-encrypted Sendspin over WebSocket -> FLAC decode
(GStreamer) -> PulseAudio sink `pcm_output`. The web UI only talks to the daemon's HTTP API/SSE. The
Sendspin protocol itself (Noise handshake, session, volume policy, reconnect) lives in the external
`syncspin` crate, pulled in as a pinned git dependency; the daemon only maps its events onto the TV
and the sink. Bumping it is `cargo update -p syncspin-client` after reviewing the new upstream
commit.

## Non-negotiables

- **Encrypted only.** Noise `KKpsk2` with Music Assistant's Sentinel guest mode (`client/hello`
  includes the X25519 identity). No unencrypted/legacy fallback; MA 2.10+ refuses it anyway.
- **Stable identity.** The client ID and secret live in `/media/internal/takt.json` (0600, atomic
  writes). Keep the keypair across rebuilds and reinstalls; MA would see a new player and need
  re-approval.
- **Volume guard.** A TV volume of 40 or below may move to any value. Above 40 one MA _request_ may
  add at most **+20**; the client (`VolumePolicy{40, 20, threshold_floor}`) applies it and reports
  the applied value, and the daemon moves the TV to match. A level the TV itself is at is never
  clamped: it is reported unchanged with `ControlHandle::report_volume`, so MA's slider and
  `/api/status` follow the real TV. Without a Luna snapshot the baseline is unknown and MA volume
  commands are ignored until the first TV report; do not invent 0 as a baseline.
- **Mute is user intent.** An MA `mute` command or a mute pressed on the remote sets and records the
  TV mute, is reported to MA while playing, and survives track changes, seeks, pause/resume and
  reconnects. The daemon never mutes the TV on its own: on `stream/end` it stops the sink, and
  killing `pacat` discards the audio still buffered in its pipe and in PulseAudio (`pacat` has no
  flush API; its buffer dies with the process). `Action::FlushSink` stays as the client's barrier
  and is a no-op here; the stop is what makes a pause silent. The next `stream/start` opens a fresh
  pipeline, and `[takt] pipeline open <n> ms` in the log measures that resume cost.
- **Audible volume is the sink.** The TV master does not change the sound on this setup; the
  PulseAudio sink does. Perceptual mapping: 20 -> 45 %, 50 -> 71 %, 100 -> 100 %. A missing `pactl`
  logs and degrades, never stops audio.
- **App leave is the daemon's.** A held Luna subscription
  (`com.webos.applicationManager/getForegroundAppInfo` with `{"subscribe":true}`) pushes every
  foreground change; `daemon/src/watchdog.rs` pauses once per departure while `Playing` through an
  ordinary `pause` (`ControlHandle::pause`), and falls back to a 5 s query when no push arrives.
  There is no app-leave special case in the pause path: the pause stops the sink and never touches
  the TV mute. Our app, the launcher overlay raised by Back and Home (`com.webos.app.home`,
  `com.webos.app.launcher`) and an unnamed foreground do not pause; only another app or TV input
  does. The page has no leave rule; `GET /api/foreground` stays as the readout of what the watchdog
  sees.
- **Reconnect is the client's.** A drop shows `connecting` in the UI (the client retries 1/2/4 … 30
  s on its own); the daemon shows `error` only after 30 s of failed retries. A fatal sink error ends
  the connection, so the daemon rebuilds sink + client with the same backoff.
- **Screensaver veto** is registered/renewed through Luna; keep the renewal loop when touching the
  worker.

## Where things live

| Path                                 | Purpose                                                  |
| ------------------------------------ | -------------------------------------------------------- |
| `../syncspin/crates/syncspin-client` | Session, Noise handshake, events, volume policy          |
| `daemon/src/app.rs`                  | Client event -> actions, volume/mute, pause stop         |
| `daemon/src/sink/gst.rs`             | `Sink` impl: gst/pacat pipeline, header priming, flush   |
| `daemon/src/sink/process.rs`         | GStreamer/pacat children, 1.5 s PCM bridge, analyzer tap |
| `daemon/src/luna/`                   | TV volume events, screensaver veto                       |
| `daemon/src/http/`                   | API, SSE, embedded web assets, `no-store` headers        |
| `daemon/src/visual.rs`               | 3-band PCM analyzer, stale fallback to silence           |

## Verification

```sh
cd daemon
cargo fmt --check
cargo test          # playback/sink/http/luna/config/... suites (needs ../syncspin)
cd ..
node scripts/test-visual.js
```

Hardware checks: `scripts/tv-ssh.sh 'curl -s http://127.0.0.1:3927/api/status'`,
`node scripts/ma/ma.mjs players`, and `cat /tmp/takt.log` on the TV. Avoid loud volume changes while
testing; MA `volume_level` 40 is a safe level here.
