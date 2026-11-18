---
name: music-assistant-ops
description:
  Use when inspecting or controlling Music Assistant players, queues or volume during TV work, or
  when a TV player does not show up, pause or report its state correctly.
---

# Music Assistant operations

MA runs at `<music-assistant-host>:8927`; its WebSocket API is
`ws://<music-assistant-host>:8095/ws`. The token lives in a local 0600 file holding the token and
must never be committed. The TV player is the universal player `<player-id>` (name "TV").

## CLI

```sh
export MA_WS_URL=ws://<music-assistant-host>:8095/ws
export MA_TOKEN_FILE=<token-file>   # local 0600 file holding the token
node scripts/ma/ma.mjs players
node scripts/ma/ma.mjs call players/cmd/play '{"player_id":"<player-id>"}'
node scripts/ma/ma.mjs call players/cmd/volume_set '{"player_id":"<player-id>","volume_level":40}'
```

`ma.mjs` authenticates, sends one command and prints the JSON answer. `MA_WS_URL` and
`MA_TOKEN_FILE` are both required.

## MCP (active, read-only)

The official Music Assistant MCP server is installed at `http://<music-assistant-host>:8095/mcp/v1`
and authenticated with the existing long-lived MA token. It exposes a **discovery interface**
instead of a fixed tool list:

1. `search_tools` with a short query (or an empty query to browse the catalog) returns command names
   such as `ma_api:players/get` plus a `policy_mode` (`allow` or `confirm`).
2. `get_tool_schema` with `tool_name` returns the exact input schema and command.
3. `call_tool` with `name` and `arguments` executes one command.

- OpenCode config: `opencode.jsonc` (git-ignored; `opencode.example.jsonc` is the template). The
  `Authorization` header comes from a local 0600 file holding `Bearer <token>`; never commit it.
- Permissions are the read-only defaults (`query:library`, `query:queue`, `query:players`,
  `query:metadata`); write/playback commands stay gated. Enabling Control permissions in MA ->
  Settings -> Plugins -> MCP Server opens them up (then `/reload`).
- Verify with `opencode2 mcp list` (`music-assistant connected`).
- `scripts/ma/ma.mjs` remains the zero-dependency fallback and covers write commands such as
  `players/cmd/play` and `players/cmd/volume_set`.

## Practical notes

- The TV player is a `universal_player` that consumes the Sendspin (Sentinel guest) session. Do not
  confuse it with the soundbar or with other Sendspin players in the list.
- After a pause the player may report `state: "idle"` in `players/all` while the daemon already
  shows `paused`; both mean "not playing". Do not treat `idle` as an error.
- Volume: 40 is the safe test level. The daemon clamps any jump above 40 to +20 per command and
  reports the applied value, so MA's `volume_level` can differ by one step from what you sent.
- A new Sendspin client may need its public key approved once in the MA UI (guest/sentinel role).
  Check `players/all` for a fresh player instead of assuming the TV is broken.
- When testing leave-to-another-app, put the TV back where the user had it and restore the volume
  afterwards.
