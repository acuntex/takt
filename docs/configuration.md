# Configuration

Takt keeps its configuration and identity in one file on the TV:

```
/media/internal/takt.json
```

The file is written with mode `0600`. **Back it up.** It holds the X25519 keypair Music Assistant
knows as this player; deleting it creates a new identity, and the new key has to be approved in MA
again.

## Fields

| Field             | Meaning                                                               |
| ----------------- | --------------------------------------------------------------------- |
| `server`          | Music Assistant Sendspin address, `host:port` (default `.local` name) |
| `player_name`     | Name shown in Music Assistant                                         |
| `client_id`       | Public half of the client keypair, base64url                          |
| `client_secret`   | Private half, base64url — never leaves the TV                         |
| `auto_open`       | Bring the TV UI to the front when playback starts (default on)        |
| `output_delay_ms` | Delay after the TV's audio port, for external receivers (0–5000 ms)   |

## Connection

Takt connects to Music Assistant as an unpaired guest: Noise `KKpsk2` with the Sentinel PSK. There
is no plaintext fallback and no token to configure. If Music Assistant does not assign the player
role automatically, approve the client key once in the MA interface.

## Changing things

- **Player name and TV audio delay** are Settings rows on the TV. The delay applies immediately; the
  name needs a restart. Run `scripts/install-tv.sh` again, which stops and starts the service.
- **The delay row stops at 200 ms.** Larger values (up to the protocol's 5000 ms) can be written to
  `output_delay_ms` in the file or via `POST /api/config`; the row then shows the value in force.
- **Auto-open** has no row. Set `auto_open` in the file while the service is stopped.
- **Server** is not editable on the TV on purpose. Change `server` in the file while the service is
  stopped, or `POST /api/config` with the PIN; a new server also needs a restart.
- **API writes** (`/api/config`, `/api/transport`) need the four-digit PIN. It is generated on every
  service start (normally once per boot), shown behind **Reveal** in the TV's Settings, and
  `/api/pin` answers on loopback only. Five failed attempts per minute are allowed.

## HTTP API

The daemon advertises two optional roles: `artwork@v1`, with one 640×640 album channel (JPEG, 1 MiB
limit), and `color@v1`, with the six-field palette the server supplies for the track. The TV UI does
not render either yet: the artwork endpoint and the `artwork_revision` and `color` status fields are
daemon-side groundwork.

| Endpoint              | Purpose                                                                                              |
| --------------------- | ---------------------------------------------------------------------------------------------------- |
| `GET /api/status`     | Playback state, metadata, volume, server, output delay, artwork revision, color palette              |
| `GET /events`         | Server-sent events with the same state                                                               |
| `GET /api/artwork`    | Latest album image from the artwork role (`image/jpeg`); 404 JSON until one arrives; no PIN          |
| `GET /api/foreground` | The TV's foreground app and whether it means "left"                                                  |
| `GET /api/pin`        | The boot PIN; loopback only                                                                          |
| `POST /api/transport` | `play`, `pause`, `next`, `prev`, `seek` (`position_ms`), `seek_relative` (`offset_ms`); PIN required |
| `POST /api/config`    | `server`, `player_name`, `output_delay_ms`; PIN required                                             |

`GET /api/artwork` derives `Content-Type` from the image it holds (`image/jpeg` for the advertised
channel, `image/png` if a future configuration asks for it) and answers with
`Cache-Control: no-store` and `ETag: "<artwork_revision>"`.

If the configured address stops answering, Takt browses `_sendspin-server._tcp` at the next start or
connection rebuild and uses the first reachable advertised server for that run. The config is never
rewritten. mDNS does not cross a router: the TV and the server must share one broadcast domain.

## Environment overrides

| Variable          | Effect                                            |
| ----------------- | ------------------------------------------------- |
| `TAKT_CONFIG`     | Use a different config path                       |
| `TAKT_PULSE_SINK` | Use a different PulseAudio sink than `pcm_output` |

## Identifiers

Crate and binary `takt`, app id `com.github.acuntex.takt`, LS2 service
`com.github.acuntex.takt.service`, init script `90-start_takt`, log `/tmp/takt.log`. The protocol
code lives in [`syncspin`](https://github.com/acuntex/syncspin); the daemon depends on
`syncspin-client` from crates.io.
