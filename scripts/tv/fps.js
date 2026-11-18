#!/usr/bin/env node
"use strict";

// rAF frame-rate sampling on the TV. The TV's rAF count is a rough main-thread/compositor
// proxy: compare the stage with the artwork plate hidden to see what the plate (its one
// 120 px drop shadow) costs.
//
// Usage: node scripts/tv/fps.js [seconds=4]
// Env:   TV_HOST (required), TV_CDP_PORT, TV_APP_PORT

const { openApp, sleep } = require("./lib/cdp");

async function main() {
  const seconds = Number(process.argv[2] || 4);
  const { cdp } = await openApp();

  async function sample(label) {
    await cdp.evaluate(
      "if(window.__fpsRun){window.__fpsRun=false;} window.__fps=0; window.__fpsRun=true; " +
        "window.__fpsTick=function(){if(!window.__fpsRun){return;} window.__fps++; requestAnimationFrame(window.__fpsTick);}; " +
        "requestAnimationFrame(window.__fpsTick);"
    );
    await sleep(seconds * 1000);
    const frames = await cdp.evaluate("window.__fpsRun=false; window.__fps");
    console.log(label.padEnd(22), (frames / seconds).toFixed(1), "fps");
  }

  await sample("vice stage");
  await cdp.evaluate("document.querySelector('.plate').style.display='none'");
  await sample("plate hidden");
  await cdp.send("Page.reload");
  await sleep(800);
  cdp.close();
}

main().catch((error) => {
  console.error(error.message || error);
  process.exitCode = 1;
});
