#!/usr/bin/env node
"use strict";

// Capture a short screenshot series from the TV UI plus the live stage values, so a reviewer
// can check that the sleeve visibly moves between frames.
//
// Usage: node scripts/tv/shots.js [outDir=<system tmp dir>] [prefix=tv]
// Env:   TV_HOST (required), TV_CDP_PORT, TV_APP_PORT

const os = require("os");
const path = require("path");
const { openApp, sleep } = require("./lib/cdp");

async function main() {
  const outDir = process.argv[2] || os.tmpdir();
  const prefix = process.argv[3] || "tv";
  const { cdp } = await openApp();

  for (const label of ["a", "b", "c"]) {
    const state = await cdp.evaluate(
      "JSON.stringify({sleeve:document.getElementById('art').style.transform,fill:document.getElementById('progress-fill').style.transform,palette:document.body.className})"
    );
    const file = path.join(outDir, `${prefix}-${label}.png`);
    await cdp.screenshot(file);
    console.log(label, file, state);
    if (label !== "c") {
      await sleep(1500);
    }
  }
  cdp.close();
}

main().catch((error) => {
  console.error(error.message || error);
  process.exitCode = 1;
});
