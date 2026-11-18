#!/usr/bin/env node
"use strict";

// Music Assistant WebSocket CLI used by the TV player work.
//
// Usage:
//   MA_WS_URL=ws://<music-assistant-host>:8095/ws MA_TOKEN_FILE=<token-file> \
//     node scripts/ma/ma.mjs players
//   ... node scripts/ma/ma.mjs call players/cmd/play '{"player_id":"up1234abcd"}'
//   ... node scripts/ma/ma.mjs call players/cmd/volume_set '{"player_id":"up1234abcd","volume_level":40}'
//
// Env:
//   MA_WS_URL      required, e.g. ws://<music-assistant-host>:8095/ws
//   MA_TOKEN_FILE  required, path to a local 0600 file holding the MA token

import fs from "node:fs";

const URL = process.env.MA_WS_URL || "";
const TOKEN_FILE = process.env.MA_TOKEN_FILE || "";

if (!URL) {
    console.error("MA_WS_URL is required, e.g. ws://<music-assistant-host>:8095/ws");
    process.exit(1);
}
if (!TOKEN_FILE) {
    console.error("MA_TOKEN_FILE is required: a local 0600 file holding the MA token");
    process.exit(1);
}

function connect() {
    const socket = new WebSocket(URL);
    let nextId = 0;
    const pending = new Map();

    const ready = new Promise((resolve, reject) => {
        socket.addEventListener("open", resolve);
        socket.addEventListener("error", () => reject(new Error(`cannot connect to ${URL}`)));
    });

    socket.addEventListener("message", (event) => {
        let message;
        try {
            message = JSON.parse(String(event.data));
        } catch (error) {
            return;
        }
        if (!message.message_id || !pending.has(message.message_id)) {
            return;
        }
        const waiter = pending.get(message.message_id);
        pending.delete(message.message_id);
        waiter.resolve(message);
    });

    return {
        ready,
        call(command, args) {
            return new Promise((resolve) => {
                const id = String(++nextId);
                pending.set(id, { resolve });
                socket.send(JSON.stringify({ message_id: id, command, args: args || {} }));
            });
        },
        close() {
            socket.close();
        },
    };
}

async function main() {
    const [mode, command, argsJson] = process.argv.slice(2);
    const token = fs.readFileSync(TOKEN_FILE, "utf8").trim();
    const client = connect();
    await client.ready;
    const auth = await client.call("auth", { token });
    if (auth.error_code) {
        throw new Error(`auth failed: ${JSON.stringify(auth)}`);
    }

    if (!mode || mode === "players") {
        const response = await client.call("players/all");
        for (const player of response.result || []) {
            console.log(
                JSON.stringify({
                    id: player.player_id,
                    name: player.name,
                    provider: player.provider,
                    state: player.state,
                    volume: player.volume_level,
                    source: player.active_source,
                    available: player.available,
                })
            );
        }
    } else if (mode === "call") {
        if (!command) {
            throw new Error("usage: ma.mjs call <command> [json-args]");
        }
        const response = await client.call(command, argsJson ? JSON.parse(argsJson) : {});
        console.log(JSON.stringify(response));
    } else {
        throw new Error("usage: ma.mjs players | call <command> [json-args]");
    }

    client.close();
    setTimeout(() => process.exit(0), 50);
}

main().catch((error) => {
    console.error(error.message || error);
    process.exitCode = 1;
});
