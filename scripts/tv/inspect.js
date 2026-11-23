#!/usr/bin/env node
"use strict";

// Inspect the running TV UI over CDP: stage state, title fit, rail geometry and whether the
// sleeve is actually moving. Optional --shot <file> writes a PNG.

const { openApp, sleep } = require("./lib/cdp");

async function main() {
  const shotIndex = process.argv.indexOf("--shot");
  const shotFile = shotIndex > -1 ? process.argv[shotIndex + 1] : null;

  const { page, cdp } = await openApp();
  const evaluate = (expression) => cdp.evaluate(expression);

  console.log("page      ", page.url);
  console.log(
    "document  ",
    await evaluate(
      "JSON.stringify({title:document.title,body:document.body.className,state:document.getElementById('state-word').textContent,empty:document.body.className.indexOf('is-empty')!==-1,titleSize:document.body.getAttribute('data-title-size')})"
    )
  );
  console.log(
    "resources ",
    await evaluate(
      "JSON.stringify(performance.getEntriesByType('resource').map(function(e){return e.name}).filter(function(n){return /(app|visual|progress|style|anton)\\.(js|css|ttf)/.test(n)}))"
    )
  );
  console.log(
    "stage     ",
    await evaluate(
      "JSON.stringify({plate:document.querySelectorAll('.plate').length,sleeve:document.getElementById('art').style.transform,fill:document.getElementById('progress-fill').style.transform,ledger:document.getElementById('ledger').textContent.replace(/\\s+/g,' ').trim(),palette:document.body.className,volume:document.getElementById('volume').textContent})"
    )
  );
  console.log(
    "rail      ",
    await evaluate(
      /* webOS 4's Chromium 53 has no DOMRect.toJSON and no x/y, and an exception here hides the
         whole line. */
      "JSON.stringify((function(){var box=function(id){var r=document.getElementById(id).getBoundingClientRect();return {x:(r.x===undefined?r.left:r.x),y:(r.y===undefined?r.top:r.y),width:r.width,height:r.height};};return {bar:box('progress-track'),ledger:box('ledger'),play:box('play-pause')};})())"
    )
  );

  const before = await evaluate("document.getElementById('art').style.transform");
  await sleep(1200);
  const after = await evaluate("document.getElementById('art').style.transform");
  console.log("animated  ", before !== after, before, "->", after);

  if (shotFile) {
    await cdp.screenshot(shotFile);
    console.log("shot      ", shotFile);
  }
  cdp.close();
}

main().catch((error) => {
  console.error(error.message || error);
  process.exitCode = 1;
});
