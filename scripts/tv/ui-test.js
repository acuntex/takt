#!/usr/bin/env node
"use strict";

// Headless UI harness for the TV page: serves web/ and a small status API, drives Chrome over
// CDP and checks the Vice stage, the rail geometry, the states, the settings surface and the
// page's silence when the TV hides it.
// Needs google-chrome (or set CHROME=/path/to/chrome) and Node 22 (built-in WebSocket).
//
// Usage: node scripts/tv/ui-test.js
// Env:   CHROME, UI_PORT (8765), UI_CDP_PORT (9229), UI_SHOT (optional PNG path)

const { spawn } = require("child_process");
const fs = require("fs");
const http = require("http");
const os = require("os");
const path = require("path");

const ROOT = path.resolve(__dirname, "../..");
const WEB = path.join(ROOT, "web");
const PORT = process.env.UI_PORT || "8765";
const CDP_PORT = process.env.UI_CDP_PORT || "9229";
const CHROME = process.env.CHROME || "/usr/bin/google-chrome";
const PROFILE = path.join(os.tmpdir(), "takt-chrome-ui-test");
const SHOT = process.env.UI_SHOT || "";

const TYPES = {
  ".html": "text/html; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".js": "application/javascript; charset=utf-8",
  ".ttf": "font/ttf",
  ".svg": "image/svg+xml",
};

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/* The status the page polls; individual checks mutate it. */
const status = {
  state: "playing",
  title: "Galvanize The Floor",
  artist: "The Chemical Brothers",
  album: "Push the Button",
  artwork_url: null,
  progress_ms: 118000,
  duration_ms: 350000,
  playback_speed: 1000,
  metadata_revision: 1,
  volume: 42,
  muted: false,
  server: "ma.local:8927",
  player_name: "Takt",
  output_delay_ms: 0,
};

/* The page must never poll the foreground itself; the daemon owns the leave rule. The check
   below counts any hit on this route. */
const foreground = { app_id: "com.webos.app.home", leaving: false, hits: 0 };

/* The loopback PIN the page is allowed to read, and the last /api/config body. */
const PIN = "4921";
let pinReadout = true;
let lastConfig = null;
/* Every /api/config body in order, so a check can count one write per step. */
const configBodies = [];
/* The next /api/config POST answers with this status instead of applying. */
let refuseConfig = null;
/* Every /api/transport body the page sends, in order: the seek checks read the
   exact command and offset off it. */
const transportBodies = [];
/* A check can ask the next transport call of one command to fail after a short
   delay, so the cursor can be seen ahead of the fill before it returns. */
let refuseTransport = null;
let refuseDelayMs = 0;

function readBody(request) {
  return new Promise((resolve) => {
    let body = "";
    request.on("data", (chunk) => (body += chunk));
    request.on("end", () => resolve(body));
  });
}

function startServer() {
  return http.createServer(async (request, response) => {
    const url = request.url.split("?")[0];
    if (url === "/api/status") {
      response.writeHead(200, { "Content-Type": "application/json" });
      response.end(JSON.stringify(status));
      return;
    }
    if (url === "/api/foreground") {
      foreground.hits += 1;
      response.writeHead(200, { "Content-Type": "application/json" });
      response.end(JSON.stringify(foreground));
      return;
    }
    if (url === "/api/pin") {
      /* The page is served on loopback, as the TV app is: the daemon hands it the
         PIN and the settings surface can show it behind Reveal. The final check
         flips this off to prove the LAN mode is a single entry row. */
      if (!pinReadout) {
        response.writeHead(403, { "Content-Type": "application/json" });
        response.end(JSON.stringify({ error: "PIN is only available on loopback" }));
        return;
      }
      response.writeHead(200, { "Content-Type": "application/json" });
      response.end(JSON.stringify({ pin: PIN }));
      return;
    }
    if (url === "/api/config" && request.method === "POST") {
      const raw = await readBody(request);
      let payload = null;
      try {
        payload = JSON.parse(raw);
      } catch (error) {
        payload = null;
      }
      lastConfig = payload;
      configBodies.push(payload);
      if (refuseConfig !== null) {
        const refused = refuseConfig;
        refuseConfig = null;
        response.writeHead(refused, { "Content-Type": "application/json" });
        response.end(JSON.stringify({ error: "config refused" }));
        return;
      }
      if (!payload || payload.pin !== PIN) {
        response.writeHead(403, { "Content-Type": "application/json" });
        response.end(JSON.stringify({ error: "PIN rejected" }));
        return;
      }
      if (typeof payload.player_name === "string") {
        status.player_name = payload.player_name;
      }
      if (typeof payload.output_delay_ms === "number") {
        status.output_delay_ms = payload.output_delay_ms;
      }
      response.writeHead(200, { "Content-Type": "application/json" });
      response.end(JSON.stringify({ ...payload, restart_required: true }));
      return;
    }
    if (url === "/api/transport" && request.method === "POST") {
      /* The same PIN gate as the daemon: a wrong or missing PIN is refused, and
         the body is kept so a check can read the command and its target. */
      const raw = await readBody(request);
      let payload = null;
      try {
        payload = JSON.parse(raw);
      } catch (error) {
        payload = null;
      }
      transportBodies.push(payload);
      if (payload && refuseTransport === payload.cmd) {
        refuseTransport = null;
        await sleep(refuseDelayMs);
        refuseDelayMs = 0;
        response.writeHead(503, { "Content-Type": "application/json" });
        response.end(JSON.stringify({ error: "the player refused the command" }));
        return;
      }
      if (!payload || payload.pin !== PIN) {
        response.writeHead(403, { "Content-Type": "application/json" });
        response.end(JSON.stringify({ error: "PIN rejected" }));
        return;
      }
      response.writeHead(200, { "Content-Type": "application/json" });
      response.end(JSON.stringify({ ok: true }));
      return;
    }
    if (url === "/events") {
      // No stream in the harness: the page falls back to polling, which the checks drive.
      response.writeHead(404, { "Content-Type": "text/plain" });
      response.end("no stream");
      return;
    }
    const file = path.join(WEB, url === "/" ? "index.html" : url);
    if (!file.startsWith(WEB)) {
      response.writeHead(403);
      response.end();
      return;
    }
    fs.readFile(file, (error, body) => {
      if (error) {
        response.writeHead(404);
        response.end();
        return;
      }
      response.writeHead(200, {
        "Content-Type": TYPES[path.extname(file)] || "application/octet-stream",
        "Cache-Control": "no-store",
      });
      response.end(body);
    });
  });
}

function getJson(url) {
  return new Promise((resolve, reject) => {
    http
      .get(url, (res) => {
        let body = "";
        res.on("data", (chunk) => (body += chunk));
        res.on("end", () => {
          try {
            resolve(JSON.parse(body));
          } catch (error) {
            reject(error);
          }
        });
      })
      .on("error", reject);
  });
}

async function waitForCdp(chrome, chromeStderr) {
  for (let attempt = 0; attempt < 300; attempt += 1) {
    try {
      return await getJson(`http://127.0.0.1:${CDP_PORT}/json`);
    } catch (error) {
      /* A cold runner needs more than a few seconds; a Chrome that never started needs to say so
         instead of leaving the caller with a timeout. */
      if (chrome.spawnError) {
        throw new Error(`Could not start ${CHROME}: ${chrome.spawnError.message}`);
      }
      if (chrome.exitCode !== null || chrome.signalCode !== null) {
        const reported = chromeStderr().trim();
        const how =
          chrome.exitCode !== null
            ? `exited with ${chrome.exitCode}`
            : `was killed by ${chrome.signalCode}`;
        throw new Error(
          `Chrome ${how} before the DevTools endpoint came up` + (reported ? `:\n${reported}` : "")
        );
      }
      await sleep(100);
    }
  }
  const reported = chromeStderr().trim();
  throw new Error(
    "Chrome DevTools endpoint did not start within 30 s" + (reported ? `:\n${reported}` : "")
  );
}

function cdpClient(webSocketDebuggerUrl) {
  const socket = new WebSocket(webSocketDebuggerUrl);
  let nextId = 0;
  const pending = new Map();
  const ready = new Promise((resolve, reject) => {
    socket.addEventListener("open", resolve);
    socket.addEventListener("error", () => reject(new Error("CDP socket error")));
  });
  socket.addEventListener("message", (event) => {
    let message;
    try {
      message = JSON.parse(String(event.data));
    } catch (error) {
      return;
    }
    if (!message.id || !pending.has(message.id)) {
      return;
    }
    const waiter = pending.get(message.id);
    pending.delete(message.id);
    if (message.error) {
      waiter.reject(new Error(message.error.message));
    } else {
      waiter.resolve(message.result);
    }
  });
  return {
    ready,
    send(method, params) {
      return new Promise((resolve, reject) => {
        const id = ++nextId;
        pending.set(id, { resolve, reject });
        socket.send(JSON.stringify({ id, method, params: params || {} }));
      });
    },
    close() {
      socket.close();
    },
  };
}

async function main() {
  const server = startServer();
  await new Promise((resolve) => server.listen(Number(PORT), "127.0.0.1", resolve));
  fs.rmSync(PROFILE, { recursive: true, force: true });
  let stderr = "";
  const chrome = spawn(
    CHROME,
    [
      "--headless=new",
      "--no-sandbox",
      "--disable-gpu",
      "--hide-scrollbars",
      `--remote-debugging-port=${CDP_PORT}`,
      `--user-data-dir=${PROFILE}`,
      "--window-size=1920,1080",
      "about:blank",
    ],
    { stdio: ["ignore", "ignore", "pipe"] }
  );
  /* A missing binary emits "error" instead of exiting; unhandled, it would crash with a raw stack. */
  chrome.on("error", (error) => {
    chrome.spawnError = error;
  });
  chrome.stderr.on("data", (chunk) => {
    stderr += String(chunk);
    if (stderr.length > 8000) {
      stderr = stderr.slice(-8000);
    }
  });

  const results = [];
  const check = (condition, label) => results.push({ ok: Boolean(condition), label });

  try {
    const targets = await waitForCdp(chrome, () => stderr);
    const page = targets.find((target) => target.type === "page");
    if (!page) {
      throw new Error("No Chrome page target");
    }
    const cdp = cdpClient(page.webSocketDebuggerUrl);
    await cdp.ready;
    await cdp.send("Page.enable");
    await cdp.send("Runtime.enable");
    await cdp.send("Network.enable");
    await cdp.send("Network.setCacheDisabled", { cacheDisabled: true });
    await cdp.send("Emulation.setFocusEmulationEnabled", { enabled: true });
    await cdp.send("Emulation.setDeviceMetricsOverride", {
      width: 1920,
      height: 1080,
      deviceScaleFactor: 1,
      mobile: false,
    });
    await cdp.send("Page.navigate", { url: `http://127.0.0.1:${PORT}/` });

    const evaluate = async (expression) => {
      const response = await cdp.send("Runtime.evaluate", {
        expression,
        returnByValue: true,
        awaitPromise: true,
      });
      if (response.exceptionDetails) {
        throw new Error(JSON.stringify(response.exceptionDetails));
      }
      return response.result.value;
    };

    const waitFor = async (expression, timeout) => {
      const deadline = Date.now() + (timeout || 3000);
      while (Date.now() < deadline) {
        try {
          if (await evaluate(expression)) {
            return true;
          }
        } catch (error) {
          // page not ready yet
        }
        await sleep(100);
      }
      return false;
    };

    const waitForNode = async (predicate, timeout) => {
      const deadline = Date.now() + (timeout || 3000);
      while (Date.now() < deadline) {
        if (predicate()) {
          return true;
        }
        await sleep(50);
      }
      return false;
    };

    /* One key press on the remote: the page reads `keyCode`, so the virtual code
       is what matters. */
    const press = async (key, code, keyCode) => {
      await cdp.send("Input.dispatchKeyEvent", {
        type: "keyDown",
        key,
        code,
        windowsVirtualKeyCode: keyCode,
        nativeVirtualKeyCode: keyCode,
      });
      await cdp.send("Input.dispatchKeyEvent", {
        type: "keyUp",
        key,
        code,
        windowsVirtualKeyCode: keyCode,
        nativeVirtualKeyCode: keyCode,
      });
    };

    const loaded = await waitFor(
      "document.readyState === 'complete' && !!document.getElementById('play-pause')"
    );
    if (!loaded) {
      throw new Error("TV page did not load");
    }

    /* "3:52 left" -> 232 seconds, for the seek checks that compare the ledger
       before and after a step. */
    const clockSeconds = (text) => {
      const parts = String(text).replace(" left", "").split(":").map(Number);
      return parts.reduce((total, part) => total * 60 + part, 0);
    };

    /* ---------- static structure and type ---------------------------------- */
    /* The bar is one stop. Up from the transport must land on it and Down must
       return to the control the remote came from; both are measured here, on the
       first live frame (the rail is inert while the stage has no session). */
    await waitFor("document.body.className.indexOf('state-playing') !== -1");
    await evaluate("document.getElementById('play-pause').focus()");
    await press("ArrowUp", "ArrowUp", 38);
    const upFocus = await evaluate("document.activeElement.id");
    const bar = JSON.parse(
      await evaluate(
        "JSON.stringify((function(){var b=document.querySelector('.topbar');var g=document.getElementById('settings-toggle');var n=document.querySelector('.brand-name');var r=b.getBoundingClientRect();var gs=getComputedStyle(g);return {left:r.left,top:r.top,right:r.right,bottom:r.bottom,name:n.textContent,room:document.getElementById('room').textContent,sep:document.getElementById('room-sep').hidden,settings:g.textContent.replace(/\\s+/g,' ').trim(),size:parseFloat(gs.fontSize),position:gs.position,icons:g.querySelectorAll('svg').length,color:getComputedStyle(n).color,settingsColor:gs.color,ring:gs.boxShadow};})())"
      )
    );
    await press("ArrowDown", "ArrowDown", 40);
    const downFocus = await evaluate("document.activeElement.id");
    check(
      bar.left === 96 &&
        bar.top === 54 &&
        bar.right === 1824 &&
        bar.name === "Takt" &&
        bar.room === "" &&
        bar.sep === true &&
        bar.settings === "Settings" &&
        bar.size === 26 &&
        bar.position !== "fixed" &&
        bar.icons === 0 &&
        bar.color === "rgb(182, 172, 163)" &&
        bar.settingsColor === "rgb(253, 240, 185)" &&
        /2px/.test(bar.ring) &&
        /8px/.test(bar.ring) &&
        /rgb\(184, 53, 192\)/.test(bar.ring) &&
        upFocus === "settings-toggle" &&
        downFocus === "play-pause",
      "the top bar sits in the safe area, reads Takt and Settings at 26 px, and Up/Down reach it"
    );
    check(
      await evaluate(
        "!!document.getElementById('settings-panel') && !!document.getElementById('settings-backdrop') && !!document.getElementById('settings-close')"
      ),
      "settings surface, backdrop and close exist"
    );
    check(
      await evaluate(
        "document.getElementById('settings-panel').hidden === true && document.getElementById('settings-backdrop').hidden === true"
      ),
      "the surface and backdrop are hidden at start"
    );
    check(
      (await evaluate("document.getElementById('festival') === null")) &&
        (await evaluate("document.title")) === "Takt" &&
        (await evaluate("document.documentElement.lang")) === "en",
      "the old festival stage is gone, the document is Takt and the page is English"
    );

    /* One printed plane: the field and the card share -10deg, and the canvas is
       scaled from its top-left corner, so a 1280x720 panel keeps the whole rail
       on screen and hit-testable. */
    const angles = await evaluate(
      "(function(){var p=getComputedStyle(document.querySelector('.plate')).transform;var c=getComputedStyle(document.getElementById('card')).transform;function a(t){var m=t.match(/matrix\\(([^)]+)\\)/);if(!m){return 999;}var v=m[1].split(',');return Math.atan2(parseFloat(v[1]),parseFloat(v[0]))*180/Math.PI;}return Math.abs(a(p)-a(c))<0.1&&Math.abs(a(c)+10)<0.1;})()"
    );
    await cdp.send("Emulation.setDeviceMetricsOverride", {
      width: 1280,
      height: 720,
      deviceScaleFactor: 1,
      mobile: false,
    });
    await sleep(350);
    const scaled = JSON.parse(
      await evaluate(
        "JSON.stringify({viewport:getComputedStyle(document.getElementById('viewport')).transform,play:document.getElementById('play-pause').getBoundingClientRect().toJSON(),bar:document.getElementById('progress-track').getBoundingClientRect().toJSON(),ledger:document.getElementById('ledger').getBoundingClientRect().toJSON(),hit:(function(){var r=document.getElementById('play-pause').getBoundingClientRect();var e=document.elementFromPoint(r.left+r.width/2,r.top+r.height/2);while(e&&!e.id){e=e.parentNode;}return e?e.id:'';})()})"
      )
    );
    await cdp.send("Emulation.setDeviceMetricsOverride", {
      width: 1920,
      height: 1080,
      deviceScaleFactor: 1,
      mobile: false,
    });
    await sleep(350);
    check(
      angles &&
        scaled.viewport.indexOf("0.6667") !== -1 &&
        scaled.play.right <= 1280 &&
        scaled.play.bottom <= 720 &&
        scaled.bar.right <= 1280 &&
        scaled.ledger.bottom <= 720 &&
        scaled.hit === "play-pause",
      "field and card share one angle, and 1280x720 keeps the rail on screen"
    );
    check(
      await evaluate("document.body.className.indexOf('palette-leonida') !== -1"),
      "Leonida is the fixed palette"
    );
    check(
      await evaluate(
        "document.querySelectorAll('.theme-option').length === 0 && document.querySelector('.palette-panel') === null && document.getElementById('palette-night') === null"
      ),
      "the spine-tint switch is gone and the theme has no user control"
    );
    check(
      await evaluate(
        "document.fonts && document.fonts.check('142px Anton') && document.fonts.check('40px Anton')"
      ),
      "Anton is loaded for the display type"
    );
    check(
      (await evaluate(
        "fetch('/fonts/anton.ttf').then(function(r){return r.status + ':' + r.headers.get('content-type')})"
      )) === "200:font/ttf",
      "the daemon route serves the font"
    );

    /* ---------- the playing frame ------------------------------------------ */
    await waitFor("document.body.className.indexOf('state-playing') !== -1");
    check(
      await evaluate(
        "document.getElementById('state-word').textContent === 'Playing' && parseFloat(getComputedStyle(document.getElementById('state-word')).fontSize) === 36 && getComputedStyle(document.getElementById('state-word')).color === 'rgb(253, 240, 185)' && getComputedStyle(document.querySelector('.m-play')).display === 'block' && getComputedStyle(document.querySelector('.m-pause')).display === 'none' && getComputedStyle(document.querySelector('.m-connecting')).display === 'none' && getComputedStyle(document.querySelector('.m-dot')).display === 'none' && getComputedStyle(document.querySelector('.state-mark')).color === 'rgb(184, 53, 192)'"
      ),
      "the state block answers Playing with a 36 px word and the one filled play mark"
    );
    check(
      await evaluate(
        "getComputedStyle(document.querySelector('#play-pause .g-pause')).display !== 'none' && getComputedStyle(document.querySelector('#play-pause .g-play')).display === 'none'"
      ),
      "playing shows the pause glyph"
    );
    const halo = JSON.parse(
      await evaluate(
        "JSON.stringify((function(){var s=getComputedStyle(document.getElementById('title')).textShadow;var out=[];var re=/(rgba?\\([^)]*\\))\\s*(-?[\\d.]+)px\\s+(-?[\\d.]+)px\\s+([\\d.]+)px/g;var m;while((m=re.exec(s))){out.push({color:m[1],x:parseFloat(m[2]),y:parseFloat(m[3]),blur:parseFloat(m[4])});}return {shadows:out,bed:!!document.querySelector('.title-bed'),line1:document.getElementById('title-1').textContent,line2:document.getElementById('title-2').textContent};})())"
      )
    );
    const near = halo.shadows.filter(
      (shadow) =>
        Math.max(Math.abs(shadow.x), Math.abs(shadow.y)) >= 2 &&
        Math.max(Math.abs(shadow.x), Math.abs(shadow.y)) <= 4 &&
        shadow.blur === 0
    );
    const wash = halo.shadows.filter(
      (shadow) =>
        Math.max(Math.abs(shadow.x), Math.abs(shadow.y)) === 10 &&
        shadow.blur === 0 &&
        /rgba\([^)]*,\s*0?\.\d+\)/.test(shadow.color)
    );
    check(
      halo.bed === false &&
        halo.line1 === "Galvanize" &&
        halo.line2 === "The Floor" &&
        near.length >= 8 &&
        wash.length === 1,
      "a title splits into two staggered lines and the crossing line is masked by a 13-offset glyph halo"
    );
    check(
      (await evaluate("document.body.getAttribute('data-title-size')")) === "158",
      "the design title fits at the 158 px design size"
    );

    /* A second line that fits the ink column ends on the cover's edge, not deep
       in the artwork: its right edge sits `seamGap` short of the seam. */
    const shortLine = JSON.parse(
      await evaluate(
        `JSON.stringify((function(){
          var st=document.getElementById('stage').getBoundingClientRect(); var k=st.width/1920;
          var r=document.createRange(); r.selectNodeContents(document.getElementById('title-2'));
          var b=r.getBoundingClientRect(); var y=(b.top-st.top)/k;
          return { right:Math.round((b.right-st.left)/k), seam:Math.round(742+y*(190/1080)), left:Math.round((b.left-st.left)/k) };
        })())`
      )
    );
    check(
      Math.abs(shortLine.right - (shortLine.seam - 8)) <= 12 && shortLine.left > 96,
      "a second title line that fits is set to end on the cover's edge"
    );

    /* A long second line keeps the crossing short: the stagger is capped at
       260 px past the seam instead of carrying the same 3.1 em across the art. */
    status.title = "Take My Breath Away";
    status.metadata_revision = 11;
    await waitFor("document.getElementById('title-2').textContent === 'Breath Away'", 4000);
    const long = JSON.parse(
      await evaluate(
        `JSON.stringify((function(){
          var st=document.getElementById('stage').getBoundingClientRect(); var k=st.width/1920;
          var r=document.createRange(); r.selectNodeContents(document.getElementById('title-2'));
          var b=r.getBoundingClientRect();
          var y=(b.top-st.top)/k;
          return { left:Math.round((b.left-st.left)/k), right:Math.round((b.right-st.left)/k), seam:Math.round(742+y*(190/1080)) };
        })())`
      )
    );
    check(
      long.right <= long.seam + 265 && long.left > 96,
      "a long second title line crosses the seam by at most 260 px instead of running across the art"
    );
    check(
      (await evaluate("document.getElementById('title').getBoundingClientRect().height")) === 316,
      "the title frame keeps its fixed 316 px"
    );
    check(
      (await evaluate("document.getElementById('artist').textContent")) === "The Chemical Brothers",
      "the artist line carries the artist"
    );
    const voice = JSON.parse(
      await evaluate(
        "JSON.stringify((function(){var a=document.getElementById('album');var s=getComputedStyle(a);var art=getComputedStyle(document.getElementById('artist'));return {text:a.textContent,hidden:a.hidden,size:parseFloat(s.fontSize),color:s.color,border:s.borderLeftWidth,pad:parseFloat(s.paddingLeft),left:Math.round(a.getBoundingClientRect().left),titleLeft:Math.round(document.getElementById('title').getBoundingClientRect().left),artistLeft:Math.round(document.getElementById('artist').getBoundingClientRect().left),artistSize:parseFloat(art.fontSize),artistColor:art.color};})())"
      )
    );
    /* An album equal to the title is a repeat, not metadata. */
    status.title = "Push the Button";
    status.metadata_revision = 2;
    await waitFor("document.getElementById('title-1').textContent === 'Push the'", 4000);
    const repeatedAlbum = await evaluate(
      "document.getElementById('album').hidden === true && document.getElementById('album').textContent === ''"
    );
    status.title = "Galvanize The Floor";
    status.metadata_revision = 3;
    await waitFor("document.getElementById('title-1').textContent === 'Galvanize'", 4000);
    check(
      voice.text === "Push the Button" &&
        voice.hidden === false &&
        voice.size === 28 &&
        voice.color === "rgba(182, 172, 163, 0.7)" &&
        voice.border === "0px" &&
        voice.pad === 0 &&
        voice.left === voice.titleLeft &&
        voice.artistLeft === voice.titleLeft &&
        voice.artistSize === 42 &&
        voice.artistColor === "rgb(253, 240, 185)" &&
        repeatedAlbum,
      "artist 42 px light then album 28 px sand 70 % on the title's edge, and a repeated album is dropped"
    );

    /* The ink half's cut edge: x = 742 at y 0, x = 932 at y 1080. */
    const seam = (y) => 742 + (190 * y) / 1080;
    const rail = JSON.parse(
      await evaluate(
        "JSON.stringify({bar:document.getElementById('progress-track').getBoundingClientRect().toJSON(),ledger:document.getElementById('ledger').getBoundingClientRect().toJSON(),prev:document.getElementById('prev').getBoundingClientRect().toJSON(),play:document.getElementById('play-pause').getBoundingClientRect().toJSON(),next:document.getElementById('next').getBoundingClientRect().toJSON()})"
      )
    );
    check(
      await evaluate(
        "getComputedStyle(document.getElementById('rail')).backgroundColor === 'rgba(0, 0, 0, 0)'"
      ),
      "the rail sits on the ink field, not on a floating plate"
    );
    check(
      rail.bar.left === 496 &&
        rail.bar.right === 840 &&
        rail.bar.width === 344 &&
        rail.bar.height === 20 &&
        rail.bar.right <= seam(rail.bar.top + 8) - 8 &&
        rail.next.right + 8 <= rail.bar.left &&
        (await evaluate(
          "getComputedStyle(document.getElementById('progress-fill')).backgroundImage.indexOf('linear-gradient') === 0"
        )),
      "the 344 px bar ends at x 840 inside the ink half, its fill wears the palette and the ring clears the third button"
    );
    check(
      rail.bar.top + rail.bar.height / 2 === 894 &&
        rail.prev.bottom === 952 &&
        rail.play.bottom === 952 &&
        rail.next.bottom === 952,
      "one grid: bar centre 894 and the transport baseline 952"
    );
    check(
      /^3:5\d left$/.test(
        await evaluate("document.getElementById('ledger-left').textContent.trim()")
      ) && rail.ledger.right <= seam(rail.ledger.top + rail.ledger.height / 2) - 8,
      "the ledger shows the remaining time and stays inside the ink half"
    );

    /* The spectrum band: three thin bars under the transport baseline, in the
       empty lower-left of the ink field. Decoration only — hidden from the
       accessibility tree, no focus, no pointer, never a d-pad stop. */
    const band = JSON.parse(
      await evaluate(
        `JSON.stringify((function(){
          var b=document.getElementById('band');
          var r=b.getBoundingClientRect();
          var bars=[].slice.call(b.querySelectorAll('.band-bar'));
          return {
            exists:!!b,
            hidden:b.getAttribute('aria-hidden'),
            tab:b.tabIndex,
            stops:b.querySelectorAll('[data-focusable]').length,
            pointer:getComputedStyle(b).pointerEvents,
            left:Math.round(r.left), top:Math.round(r.top), right:Math.round(r.right),
            height:Math.round(r.height),
            opacity:getComputedStyle(b).opacity,
            transition:getComputedStyle(b).transitionProperty,
            bars:bars.map(function(bar){
              var s=getComputedStyle(bar);
              return {
                w:parseFloat(s.width), h:parseFloat(s.height),
                origin:s.transformOrigin, color:s.backgroundColor,
                duration:s.transitionDuration, scale:bar.style.transform
              };
            })
          };
        })())`
      )
    );
    check(
      band.exists &&
        band.hidden === "true" &&
        band.tab === -1 &&
        band.stops === 0 &&
        band.pointer === "none" &&
        band.left === 96 &&
        band.top === 988 &&
        band.right <= seam(1020) - 8 &&
        band.height === 32 &&
        band.opacity === "1" &&
        band.transition === "opacity" &&
        band.bars.length === 3 &&
        band.bars[0].w === 300 &&
        band.bars[1].w === 220 &&
        band.bars[2].w === 150 &&
        band.bars.every(
          (bar) =>
            bar.h === 4 &&
            bar.origin === "0px 2px" &&
            bar.duration === "0s" &&
            bar.scale === "scaleX(0.05)"
        ) &&
        band.bars[0].color === "rgb(184, 53, 192)" &&
        band.bars[1].color === "rgb(130, 142, 200)" &&
        band.bars[2].color === "rgb(182, 172, 163)",
      "the band sits under the rail: three 4 px bars, 300/220/150 px, neon/lavender/sand, aria-hidden and outside the d-pad"
    );

    /* The 20 Hz tick drives bars and sleeve from the same envelope. A real
       track (bass 0.62) must move the card visibly and fill the bars in order. */
    await evaluate(
      `new Promise(function(resolve){
        var n=0;
        var timer=setInterval(function(){
          window.TaktStage.applyAudio({level:0.9,bass:0.62,mid:0.4,treble:0.2});
          n+=1;
          if(n>=14){clearInterval(timer);setTimeout(resolve,20);}
        },40);
      })`
    );
    const live = JSON.parse(
      await evaluate(
        `JSON.stringify((function(){
          var bars=[].slice.call(document.querySelectorAll('.band-bar'));
          return {
            opacity:getComputedStyle(document.getElementById('band')).opacity,
            scales:bars.map(function(bar){return bar.style.transform;}),
            sleeve:document.getElementById('art').style.transform
          };
        })())`
      )
    );
    const bandValues = live.scales.map((value) => parseFloat(value.replace(/[^\d.]/g, "")));
    const sleeveScaleLive = parseFloat(live.sleeve.replace(/^.*scale\(([^)]+)\)$/, "$1"));
    check(
      live.opacity === "1" &&
        live.scales.every((value) => /^scaleX\((0\.\d\d|1\.00)\)$/.test(value)) &&
        bandValues[0] > 0.5 &&
        bandValues[0] > bandValues[1] &&
        bandValues[1] > bandValues[2] &&
        bandValues[2] > 0.05 &&
        sleeveScaleLive > 1.03 &&
        /^translateZ\(0px\) translateY\(-\d+\.\d\dpx\) scale\(1\.\d{4}\)$/.test(live.sleeve),
      "the band and the sleeve follow the audio: bass fills further than mid, mid further than treble, and the card grows past 3 %"
    );

    if (SHOT) {
      const shot = await cdp.send("Page.captureScreenshot", { format: "png" });
      fs.writeFileSync(SHOT, Buffer.from(shot.data, "base64"));
      console.log(`screenshot ${SHOT}`);
    }

    /* ---------- the bar as a seek stop -------------------------------------- */
    /* A loaded track on a live session makes the bar the rail's fourth stop. Its
       aria values are seconds — the unit the remote steps by — and the value text
       is the ledger's own "n:n left", so what is heard and what is printed never
       disagree. */
    const barStop = JSON.parse(
      await evaluate(
        `JSON.stringify((function(){
          var b=document.getElementById('progress-track');
          var ledger=document.getElementById('ledger-left').textContent.trim();
          return {
            stop:b.hasAttribute('data-focusable'), tab:b.tabIndex,
            role:b.getAttribute('role'),
            min:b.getAttribute('aria-valuemin'), max:b.getAttribute('aria-valuemax'),
            now:b.getAttribute('aria-valuenow'), text:b.getAttribute('aria-valuetext'),
            ledger:ledger, h:b.getBoundingClientRect().height,
            pointer:getComputedStyle(b).pointerEvents
          };
        })())`
      )
    );
    const barLeft = clockSeconds(barStop.text);
    check(
      barStop.stop &&
        barStop.tab === 0 &&
        barStop.role === "slider" &&
        barStop.min === "0" &&
        barStop.max === "350" &&
        Number(barStop.now) >= 118 &&
        Number(barStop.now) <= 350 &&
        Math.abs(350 - barLeft - Number(barStop.now)) <= 2 &&
        barStop.text === barStop.ledger &&
        /^\d+:\d\d left$/.test(barStop.text) &&
        barStop.h === 20 &&
        barStop.pointer !== "none",
      "the bar is a slider stop with the position in seconds and the ledger's time left"
    );

    /* The ring is the transport's, drawn outside the track: the fill keeps its
       20 px, so the ring can never eat the played height, and the cream cursor
       is the body's full height with no shadow. */
    await evaluate("document.getElementById('progress-track').focus()");
    const barRing = JSON.parse(
      await evaluate(
        `JSON.stringify((function(){
          var b=document.getElementById('progress-track');
          var s=getComputedStyle(b);
          var c=document.getElementById('progress-cursor');
          var cs=getComputedStyle(c);
          var cr=c.getBoundingClientRect();
          return {
            active:document.activeElement.id, shadow:s.boxShadow,
            h:b.getBoundingClientRect().height,
            fill:getComputedStyle(document.getElementById('progress-fill')).height,
            outline:s.outlineStyle,
            cursor:{w:cr.width,h:cr.height,color:cs.backgroundColor,shadow:cs.boxShadow}
          };
        })())`
      )
    );
    check(
      barRing.active === "progress-track" &&
        /2px/.test(barRing.shadow) &&
        /8px/.test(barRing.shadow) &&
        /rgb\(184, 53, 192\)/.test(barRing.shadow) &&
        /rgb\(23, 22, 28\)/.test(barRing.shadow) &&
        barRing.h === 20 &&
        barRing.fill === "20px" &&
        barRing.outline === "none" &&
        barRing.cursor.w === 3 &&
        barRing.cursor.h === 20 &&
        barRing.cursor.color === "rgb(253, 240, 185)" &&
        barRing.cursor.shadow === "none",
      "focus rings the 20 px bar with the rail's ink and neon, and the cream cursor is 3 x 20 px with no shadow"
    );

    /* Up from the bar is the top bar's one control; Down returns to the bar the
       remote came from, remembered like any other rail stop. Down from the bar
       itself is the row's neighbour: play-pause, not a jump across the screen. */
    await press("ArrowUp", "ArrowUp", 38);
    const barUp = await evaluate("document.activeElement.id");
    await press("ArrowDown", "ArrowDown", 40);
    const barDown = await evaluate("document.activeElement.id");
    await press("ArrowDown", "ArrowDown", 40);
    const barDownRow = await evaluate("document.activeElement.id");
    check(
      barUp === "settings-toggle" && barDown === "progress-track" && barDownRow === "play-pause",
      "Up from the bar lands on Settings, Down returns to the bar, and Down again lands on play-pause"
    );

    /* The focused caption splits the one line: elapsed at the left end, the
       remaining time right-aligned under the bar's right end (x 840), with no
       new element. Blur returns the single "x left" caption to x 496. */
    await evaluate("document.getElementById('progress-track').focus()");
    const focusedCaption = JSON.parse(
      await evaluate(
        `JSON.stringify((function(){
          var bar=document.getElementById('progress-track').getBoundingClientRect();
          var ledger=document.getElementById('ledger');
          var left=document.getElementById('ledger-left').getBoundingClientRect();
          var elapsed=document.getElementById('ledger-track').getBoundingClientRect();
          return {
            focus:document.activeElement.id,
            display:getComputedStyle(ledger).display,
            sep:document.getElementById('ledger-sep').className,
            elapsedText:document.getElementById('ledger-track').textContent.trim(),
            leftText:document.getElementById('ledger-left').textContent.trim(),
            elapsedLeft:elapsed.left, leftRight:left.right, barLeft:bar.left, barRight:bar.right
          };
        })())`
      )
    );
    const captionMinutes = (text) =>
      String(text)
        .replace(" left", "")
        .split(":")
        .map(Number)
        .reduce((total, part) => total * 60 + part, 0);
    const captionSum =
      captionMinutes(focusedCaption.elapsedText) + captionMinutes(focusedCaption.leftText);
    check(
      focusedCaption.focus === "progress-track" &&
        focusedCaption.display === "flex" &&
        focusedCaption.sep === "is-hidden" &&
        /^\d+:\d\d$/.test(focusedCaption.elapsedText) &&
        /^\d+:\d\d left$/.test(focusedCaption.leftText) &&
        Math.abs(captionSum - 350) <= 1 &&
        Math.abs(focusedCaption.elapsedLeft - focusedCaption.barLeft) < 1 &&
        Math.abs(focusedCaption.leftRight - focusedCaption.barRight) < 1 &&
        focusedCaption.barRight === 840,
      "the focused caption prints elapsed at the left end and 'x left' under the bar's right end"
    );
    await evaluate("document.getElementById('play-pause').focus()");
    const blurredCaption = JSON.parse(
      await evaluate(
        `JSON.stringify((function(){
          var left=document.getElementById('ledger-left');
          return {
            display:getComputedStyle(document.getElementById('ledger')).display,
            elapsed:document.getElementById('ledger-track').textContent.trim(),
            text:left.textContent.trim(),
            left:left.getBoundingClientRect().left
          };
        })())`
      )
    );
    check(
      blurredCaption.display === "block" &&
        blurredCaption.elapsed === "" &&
        /^\d+:\d\d left$/.test(blurredCaption.text) &&
        Math.abs(blurredCaption.left - 496) < 1,
      "blur returns the single 'x left' caption to x 496"
    );

    /* The ring is a judgement made at 3 m, so the focused frame is photographed
       next to the playing one when the run asks for a shot. */
    await evaluate("document.getElementById('progress-track').focus()");
    await sleep(60);
    if (SHOT) {
      const focusShot = await cdp.send("Page.captureScreenshot", { format: "png" });
      const focusPath = SHOT.replace(/\.png$/, "-seek.png");
      fs.writeFileSync(focusPath, Buffer.from(focusShot.data, "base64"));
      console.log(`screenshot ${focusPath}`);
    }

    /* One Right press: 10 s forward on the wire. The cream cursor and the caption
       answer in the same frame; the fill stays on the position the daemon last
       confirmed. */
    await sleep(150);
    const readPlayhead = () =>
      evaluate(
        `JSON.stringify({
          fill:parseFloat(document.getElementById('progress-fill').style.transform.replace(/[^0-9.]/g,'')),
          cursor:parseFloat(document.getElementById('progress-cursor').style.transform.replace(/[^0-9.]/g,'')),
          now:Number(document.getElementById('progress-track').getAttribute('aria-valuenow')),
          left:document.getElementById('ledger-left').textContent.trim(),
          elapsed:document.getElementById('ledger-track').textContent.trim()
        })`
      ).then(JSON.parse);
    const beforeSeek = await readPlayhead();
    const bodiesBeforeSeek = transportBodies.length;
    await press("ArrowRight", "ArrowRight", 39);
    await sleep(120);
    const afterSeek = await readPlayhead();
    const seekBody = transportBodies[transportBodies.length - 1];
    const seekGain = afterSeek.now - beforeSeek.now;
    const leftGain = clockSeconds(beforeSeek.left) - clockSeconds(afterSeek.left);
    const elapsedGain = clockSeconds(afterSeek.elapsed) - clockSeconds(beforeSeek.elapsed);
    check(
      transportBodies.length === bodiesBeforeSeek + 1 &&
        seekBody !== null &&
        seekBody.pin === PIN &&
        seekBody.cmd === "seek_relative" &&
        seekBody.offset_ms === 10000 &&
        Math.abs(afterSeek.fill - beforeSeek.fill) < 0.003 &&
        afterSeek.cursor - beforeSeek.cursor >= 9 &&
        afterSeek.cursor - beforeSeek.cursor <= 11 &&
        seekGain >= 9 &&
        seekGain <= 11 &&
        leftGain >= 9 &&
        leftGain <= 11 &&
        elapsedGain >= 9 &&
        elapsedGain <= 11,
      "Right posts seek_relative +10000, moves the cursor and the caption, and leaves the fill where the status put it"
    );

    /* The daemon's next status lands the seek: the fill catches up with the
       cursor, which stops being ahead of the truth. */
    status.progress_ms = 128000;
    status.metadata_revision += 1;
    await waitFor(
      "Math.abs(parseFloat(document.getElementById('progress-fill').style.transform.replace(/[^0-9.]/g,'')) - ((parseFloat(document.getElementById('progress-cursor').style.transform.replace(/[^0-9.]/g,'')) + 3) / 344)) < 0.01",
      4000
    );
    const settled = await readPlayhead();
    check(
      Math.abs((settled.cursor + 3) / 344 - settled.fill) < 0.01 &&
        settled.now >= 127 &&
        settled.now <= 131,
      "a confirming status catches the fill up to the cursor"
    );

    /* A refused seek is the cursor returning, not the track snapping: the fill
       never moved, and the command's error path drops the pending target. */
    const fillBeforeRefusal = (await readPlayhead()).fill;
    refuseTransport = "seek_relative";
    refuseDelayMs = 200;
    const bodiesBeforeRefusal = transportBodies.length;
    await press("ArrowRight", "ArrowRight", 39);
    await sleep(80);
    const refusalAhead = await readPlayhead();
    await sleep(320);
    const refusalBack = await readPlayhead();
    check(
      transportBodies.length === bodiesBeforeRefusal + 1 &&
        refusalAhead.cursor - refusalBack.cursor >= 9 &&
        Math.abs(refusalAhead.fill - fillBeforeRefusal) < 0.003 &&
        Math.abs(refusalBack.fill - fillBeforeRefusal) < 0.003 &&
        Math.abs((refusalBack.cursor + 3) / 344 - refusalBack.fill) < 0.01 &&
        (await evaluate("document.activeElement.id")) === "progress-track" &&
        (await evaluate("document.getElementById('message').textContent.trim()")) ===
          "Command failed",
      "a refused seek returns the cursor to the fill and never moves the track"
    );

    /* A still playhead makes the cursor, the ramp and the frame exact: pause at
       50 % and measure the 344 x 20 track from 496 to 840. */
    status.state = "paused";
    status.progress_ms = 175000;
    status.metadata_revision += 1;
    await waitFor(
      "document.body.className.indexOf('state-paused') !== -1 && parseFloat(document.getElementById('progress-fill').style.transform.replace(/[^0-9.]/g,'')) > 0.49",
      4000
    );
    await sleep(80);
    const barFrame = JSON.parse(
      await evaluate(
        `JSON.stringify((function(){
          var bar=document.getElementById('progress-track').getBoundingClientRect();
          var fill=document.getElementById('progress-fill').getBoundingClientRect();
          var cur=document.getElementById('progress-cursor');
          var rect=cur.getBoundingClientRect();
          var style=getComputedStyle(cur);
          return {
            left:bar.left, right:bar.right, width:bar.width, height:bar.height,
            fillRight:fill.right,
            cursor:{left:rect.left,right:rect.right,top:rect.top,bottom:rect.bottom,w:rect.width,h:rect.height,color:style.backgroundColor,shadow:style.boxShadow},
            ledgerRight:document.getElementById('ledger').getBoundingClientRect().right
          };
        })())`
      )
    );
    check(
      barFrame.left === 496 &&
        barFrame.right === 840 &&
        barFrame.width === 344 &&
        barFrame.height === 20 &&
        Math.abs(barFrame.fillRight - (496 + 172)) < 1 &&
        Math.abs(barFrame.cursor.right - barFrame.fillRight) < 1 &&
        barFrame.cursor.w === 3 &&
        barFrame.cursor.h === 20 &&
        Math.abs(barFrame.cursor.top - 884) < 1 &&
        Math.abs(barFrame.cursor.bottom - 904) < 1 &&
        barFrame.cursor.color === "rgb(253, 240, 185)" &&
        barFrame.cursor.shadow === "none" &&
        barFrame.ledgerRight === 840,
      "the body is 344 x 20 px at x 496..840 and the 3 px cursor sits on the fill's leading edge"
    );
    const fillRamp = await evaluate(
      "document.getElementById('progress-fill').style.backgroundImage"
    );
    check(
      fillRamp.indexOf("linear-gradient(90deg, rgb(65, 95, 206) 0%") === 0 &&
        fillRamp.indexOf("rgb(98, 119, 203) 100%") !== -1 &&
        fillRamp.indexOf("rgb(184, 53, 192)") === -1,
      "the fill is the fixed indigo -> lavender ramp over the whole track, with no neon"
    );

    /* No duration, no stop and no command: an unknown stream length is not
       something a step can clamp against. */
    status.duration_ms = 0;
    await waitFor(
      "!document.getElementById('progress-track').hasAttribute('data-focusable')",
      4000
    );
    await evaluate("document.getElementById('next').focus()");
    await press("ArrowRight", "ArrowRight", 39);
    const unreachable = await evaluate("document.activeElement.id");
    const bodiesWithoutDuration = transportBodies.length;
    await evaluate("document.getElementById('progress-track').focus()");
    await press("ArrowRight", "ArrowRight", 39);
    await sleep(120);
    check(
      unreachable !== "progress-track" &&
        transportBodies.length === bodiesWithoutDuration &&
        (await evaluate(
          "document.getElementById('progress-track').getAttribute('aria-valuemax')"
        )) === "0" &&
        (await evaluate("document.getElementById('progress-track').tabIndex")) === -1,
      "an unknown duration offers no stop and puts no seek on the wire"
    );
    status.duration_ms = 350000;
    await waitFor("document.getElementById('progress-track').hasAttribute('data-focusable')", 4000);

    /* A still playhead makes the start edge exact: pause with the playhead on 0
       (the direct route is the status, not eighteen remote presses), then Left
       must refuse locally — no focus move, no command, the cursor stays at 0. */
    await waitFor("document.body.className.indexOf('state-paused') !== -1", 4000);
    await evaluate("document.getElementById('progress-track').focus()");
    status.progress_ms = 0;
    status.metadata_revision += 1;
    await waitFor(
      "parseFloat(document.getElementById('progress-cursor').style.transform.replace(/[^0-9.]/g,'')) === 0 && parseFloat(document.getElementById('progress-fill').style.transform.replace(/[^0-9.]/g,'')) === 0",
      4000
    );
    const bodiesBeforeClamp = transportBodies.length;
    await press("ArrowLeft", "ArrowLeft", 37);
    await sleep(120);
    const startEdge = JSON.parse(
      await evaluate(
        `JSON.stringify({
          fill:parseFloat(document.getElementById('progress-fill').style.transform.replace(/[^0-9.]/g,'')),
          cursor:parseFloat(document.getElementById('progress-cursor').style.transform.replace(/[^0-9.]/g,'')),
          now:document.getElementById('progress-track').getAttribute('aria-valuenow'),
          left:document.getElementById('ledger-left').textContent.trim(),
          focus:document.activeElement.id
        })`
      )
    );
    check(
      startEdge.fill === 0 &&
        startEdge.cursor === 0 &&
        startEdge.now === "0" &&
        startEdge.left === "5:50 left" &&
        startEdge.focus === "progress-track" &&
        transportBodies.length === bodiesBeforeClamp,
      "Left at the track start is refused locally: the cursor stays at 0 and the focus does not move"
    );

    /* The way in: from Next, Right reaches the bar and sends nothing. */
    await sleep(120);
    const bodiesBeforePath = transportBodies.length;
    await evaluate("document.getElementById('next').focus()");
    await press("ArrowRight", "ArrowRight", 39);
    const reachedBar = await evaluate("document.activeElement.id");
    check(
      reachedBar === "progress-track" && transportBodies.length === bodiesBeforePath,
      "Next -> Right reaches the bar without a command"
    );

    /* A held Left at the start repeats. Every repeat is a seek that cannot move,
       so the bar stays where it is: the remote's key repeat never walks the rail. */
    await cdp.send("Input.dispatchKeyEvent", {
      type: "keyDown",
      key: "ArrowLeft",
      code: "ArrowLeft",
      windowsVirtualKeyCode: 37,
      nativeVirtualKeyCode: 37,
      autoRepeat: true,
    });
    const heldFocus = await evaluate("document.activeElement.id");
    await cdp.send("Input.dispatchKeyEvent", {
      type: "keyUp",
      key: "ArrowLeft",
      code: "ArrowLeft",
      windowsVirtualKeyCode: 37,
      nativeVirtualKeyCode: 37,
    });
    await sleep(120);
    check(
      heldFocus === "progress-track" && transportBodies.length === bodiesBeforePath,
      "a held Left at the start stays on the bar and sends nothing"
    );

    /* The other edge: 40 steps forward reach the duration, the status lands it,
       and one more press neither moves the cursor nor touches the wire. */
    for (let step = 0; step < 40; step += 1) {
      await press("ArrowRight", "ArrowRight", 39);
    }
    status.progress_ms = 350000;
    status.metadata_revision += 1;
    await waitFor(
      "parseFloat(document.getElementById('progress-fill').style.transform.replace(/[^0-9.]/g,'')) === 1",
      4000
    );
    const endEdge = JSON.parse(
      await evaluate(
        `JSON.stringify({
          fill:parseFloat(document.getElementById('progress-fill').style.transform.replace(/[^0-9.]/g,'')),
          cursor:parseFloat(document.getElementById('progress-cursor').style.transform.replace(/[^0-9.]/g,'')),
          now:document.getElementById('progress-track').getAttribute('aria-valuenow'),
          left:document.getElementById('ledger-left').textContent.trim(),
          focus:document.activeElement.id
        })`
      )
    );
    const bodiesAtEnd = transportBodies.length;
    await press("ArrowRight", "ArrowRight", 39);
    await sleep(120);
    check(
      endEdge.fill === 1 &&
        endEdge.cursor === 341 &&
        endEdge.now === "350" &&
        endEdge.left === "0:00 left" &&
        endEdge.focus === "progress-track" &&
        transportBodies.length === bodiesAtEnd,
      "Right clamps at the duration: the cursor lands on the end, nothing is left and no seek follows"
    );
    status.state = "playing";
    status.progress_ms = 118000;
    status.metadata_revision += 1;
    await waitFor("document.body.className.indexOf('state-playing') !== -1", 4000);

    /* A held playback: the server reports "playing" with speed 0 while it holds the
       position. The seek is accepted, and the cursor it parks at the target must not
       creep forward while the status says the track stands still. */
    status.playback_speed = 0;
    status.metadata_revision += 1;
    await sleep(1200);
    await evaluate("document.getElementById('progress-track').focus()");
    const holdFocus = await evaluate("document.activeElement.id");
    const bodiesBeforeHold = transportBodies.length;
    await press("ArrowRight", "ArrowRight", 39);
    await sleep(150);
    const holdBody = transportBodies[transportBodies.length - 1];
    const heldCursor = () =>
      evaluate(
        "parseFloat(document.getElementById('progress-cursor').style.transform.replace(/[^0-9.]/g,''))"
      );
    const heldFirst = Number(await heldCursor());
    await sleep(1000);
    const heldSecond = Number(await heldCursor());
    check(
      holdFocus === "progress-track" &&
        transportBodies.length === bodiesBeforeHold + 1 &&
        Boolean(holdBody) &&
        holdBody.cmd === "seek_relative" &&
        holdBody.offset_ms === 10000 &&
        heldFirst === heldSecond,
      "a held playback takes the seek and keeps the cursor still instead of running at 1x"
    );

    /* Confirm the seek at the target so the pending window closes, then hand the
       status back: the paused checks below expect the track at 118 s at speed 1. */
    status.progress_ms = 128000;
    status.metadata_revision += 1;
    await sleep(500);
    status.playback_speed = 1000;
    status.progress_ms = 118000;
    status.metadata_revision += 1;
    await sleep(200);

    /* ---------- paused ------------------------------------------------------ */
    status.state = "paused";
    status.metadata_revision += 1;
    await waitFor("document.body.className.indexOf('state-paused') !== -1", 4000);
    const pausedStage = JSON.parse(
      await evaluate(
        "JSON.stringify({word:document.getElementById('state-word').textContent,pause:getComputedStyle(document.querySelector('.m-pause')).display,play:getComputedStyle(document.querySelector('.m-play')).display,wordColor:getComputedStyle(document.getElementById('state-word')).color,glyph:getComputedStyle(document.querySelector('#play-pause .g-play')).display,glyphPause:getComputedStyle(document.querySelector('#play-pause .g-pause')).display,pointer:getComputedStyle(document.getElementById('play-pause')).pointerEvents,disabled:document.getElementById('play-pause').getAttribute('aria-disabled')})"
      )
    );
    check(
      pausedStage.word === "Paused" &&
        pausedStage.pause === "block" &&
        pausedStage.play === "none" &&
        pausedStage.wordColor === "rgb(182, 172, 163)" &&
        pausedStage.glyph !== "none" &&
        pausedStage.glyphPause === "none" &&
        pausedStage.pointer !== "none" &&
        pausedStage.disabled === null,
      "paused shows the pause mark and no play triangle, and the live transport offers Play"
    );
    await sleep(500);
    const pausedBand = JSON.parse(
      await evaluate(
        `JSON.stringify({
          opacity:getComputedStyle(document.getElementById('band')).opacity,
          scales:[].slice.call(document.querySelectorAll('.band-bar')).map(function(bar){return bar.style.transform;}),
          sleeve:document.getElementById('art').style.transform
        })`
      )
    );
    check(
      pausedBand.opacity === "0" &&
        pausedBand.scales.join(",") === "scaleX(0.05),scaleX(0.05),scaleX(0.05)" &&
        pausedBand.sleeve === "translateZ(0px) translateY(0px) scale(1)",
      "pause fades the band out, parks every bar at the floor and settles the sleeve to rest"
    );

    /* ---------- connecting -------------------------------------------------- */
    /* A reconnect must not move the ring under the user's thumb: the bar keeps
       the focus, the ring stays at full strength, the bar, fill, cursor and
       caption dim, Left/Right are inert and the time says Reconnecting. */
    await evaluate("document.getElementById('progress-track').focus()");
    const cursorBeforeReconnect = await evaluate(
      "document.getElementById('progress-cursor').style.transform"
    );
    const bodiesBeforeReconnect = transportBodies.length;
    status.state = "connecting";
    await waitFor("document.body.className.indexOf('state-connecting') !== -1", 4000);
    await sleep(300);
    const reconnect = JSON.parse(
      await evaluate(
        `JSON.stringify((function(){
          var b=document.getElementById('progress-track');
          return {
            focus:document.activeElement.id,
            prog:getComputedStyle(document.getElementById('prog')).opacity,
            fill:getComputedStyle(document.getElementById('progress-fill')).opacity,
            ledger:getComputedStyle(document.getElementById('ledger')).opacity,
            bar:getComputedStyle(b).backgroundColor,
            ring:getComputedStyle(b).boxShadow,
            text:document.getElementById('ledger-left').textContent.trim(),
            elapsed:document.getElementById('ledger-track').textContent.trim(),
            aria:b.getAttribute('aria-valuetext'),
            stop:b.hasAttribute('data-focusable')
          };
        })())`
      )
    );
    await press("ArrowRight", "ArrowRight", 39);
    await sleep(120);
    check(
      reconnect.focus === "progress-track" &&
        reconnect.prog === "1" &&
        reconnect.fill === "0.4" &&
        reconnect.ledger === "0.4" &&
        reconnect.bar === "rgba(182, 172, 163, 0.2)" &&
        /8px/.test(reconnect.ring) &&
        /rgb\(184, 53, 192\)/.test(reconnect.ring) &&
        reconnect.text === "Reconnecting" &&
        reconnect.elapsed === "" &&
        reconnect.aria === "Reconnecting" &&
        reconnect.stop === false &&
        transportBodies.length === bodiesBeforeReconnect &&
        (await evaluate("document.getElementById('progress-cursor').style.transform")) ===
          cursorBeforeReconnect &&
        (await evaluate("document.activeElement.id")) === "progress-track",
      "a reconnect keeps the ring on the bar, dims it, prints Reconnecting and leaves Left/Right inert"
    );
    await evaluate("document.getElementById('settings-toggle').focus()");
    await sleep(450);
    const dotSamples = JSON.parse(
      await evaluate(
        "new Promise(function(resolve){var out=[],n=0;var t=setInterval(function(){out.push(getComputedStyle(document.querySelector('.m-connecting i')).transform);n+=1;if(n>=6){clearInterval(t);resolve(JSON.stringify(out));}},70);})"
      )
    );
    const connecting = JSON.parse(
      await evaluate(
        "JSON.stringify({word:document.getElementById('state-word').textContent,dots:getComputedStyle(document.querySelector('.m-connecting')).display,animation:getComputedStyle(document.querySelector('.m-connecting i')).animationName,filter:getComputedStyle(document.querySelector('.m-connecting i')).filter,shadow:getComputedStyle(document.querySelector('.m-connecting i')).boxShadow,play:getComputedStyle(document.querySelector('#play-pause .g-play')).display,pause:getComputedStyle(document.querySelector('#play-pause .g-pause')).display,pointer:getComputedStyle(document.getElementById('play-pause')).pointerEvents,buttons:getComputedStyle(document.getElementById('prev')).opacity,bar:getComputedStyle(document.getElementById('prog')).opacity,disabled:document.getElementById('prev').getAttribute('aria-disabled'),keys:(function(){var sheets=document.styleSheets,i,j,k,rule,frames,out=[];for(i=0;i<sheets.length;i+=1){var rules=sheets[i].cssRules;for(j=0;j<rules.length;j+=1){rule=rules[j];if(rule.type===7&&rule.name==='dot-bob'){frames=rule.cssRules||rule.keyRules||[];for(k=0;k<frames.length;k+=1){var s=frames[k].style;var p;for(p=0;p<s.length;p+=1){if(s[p].indexOf('transform')===-1){out.push(s[p]);}}}}}}return out;})()})"
      )
    );
    check(
      connecting.word === "Connecting" &&
        connecting.dots === "block" &&
        connecting.animation === "dot-bob" &&
        connecting.filter === "none" &&
        connecting.shadow === "none" &&
        connecting.play === "none" &&
        connecting.pause === "none" &&
        connecting.pointer === "none" &&
        connecting.buttons === "0.4" &&
        connecting.bar === "0.4" &&
        connecting.disabled === "true" &&
        connecting.keys.length === 0 &&
        dotSamples.filter((value, index) => dotSamples.indexOf(value) === index).length > 1,
      "connecting bobs transform-only dots, drops the rail to 40 % and makes it inert"
    );
    check(
      await evaluate(
        "!document.getElementById('progress-track').hasAttribute('data-focusable') && document.getElementById('progress-track').tabIndex === -1 && getComputedStyle(document.getElementById('progress-track')).pointerEvents === 'none'"
      ),
      "connecting drops the bar's stop and leaves it inert"
    );
    status.state = "playing";
    await waitFor("document.body.className.indexOf('state-playing') !== -1", 4000);
    await evaluate("document.getElementById('play-pause').focus()");

    /* ---------- the volume numeral ----------------------------------------- */
    status.state = "playing";
    status.metadata_revision = 3;
    status.volume = 57;
    await waitFor("document.body.className.indexOf('show-volume') !== -1", 4000);
    await sleep(450);
    const volumeShown = await evaluate(
      "document.getElementById('volume').textContent === '57' && parseFloat(getComputedStyle(document.getElementById('volume')).opacity) > 0.95"
    );
    await sleep(1700);
    const volumeHidden = await evaluate(
      "document.body.className.indexOf('show-volume') === -1 && parseFloat(getComputedStyle(document.getElementById('volume')).opacity) < 0.05"
    );
    check(volumeShown && volumeHidden, "the volume numeral appears after a change and fades again");

    /* ---------- idle -------------------------------------------------------- */
    status.title = "I Love My People (Edit)";
    status.metadata_revision = 4;
    await waitFor("document.getElementById('title-1').textContent === 'I Love My'", 4000);
    await evaluate("window.TaktStage.simulateIdle(true)");
    /* The chrome fades over 400 ms; sampling at a fixed 500 ms occasionally
       caught 0.0026 of the outgoing layer, so wait for the fade to land. */
    await waitFor("getComputedStyle(document.getElementById('content')).opacity === '0'", 3000);
    /* Idle keeps the music visible: the burn-in screen is the one place where a
       still, bright card would sit untouched for an hour, so the sleeve keeps
       breathing and the band stays at half strength. The transition must stay
       off there too — a 240 ms transition restarted 20 times a second runs
       forever and repaints the whole stage. */
    await evaluate(
      `new Promise(function(resolve){
        var n=0;
        var timer=setInterval(function(){
          window.TaktStage.applyAudio({level:0.9,bass:0.62,mid:0.4,treble:0.2});
          n+=1;
          if(n>=10){clearInterval(timer);setTimeout(resolve,20);}
        },40);
      })`
    );
    const idleAudio = JSON.parse(
      await evaluate(
        `JSON.stringify((function(){
          var bars=[].slice.call(document.querySelectorAll('.band-bar'));
          return {
            opacity:getComputedStyle(document.getElementById('band')).opacity,
            scales:bars.map(function(bar){return bar.style.transform;}),
            sleeve:parseFloat(getComputedStyle(document.getElementById('art')).webkitTransform.split(',')[0].replace('matrix(','')),
            transition:getComputedStyle(document.getElementById('art')).transitionDuration
          };
        })())`
      )
    );
    check(
      idleAudio.opacity === "0.5" &&
        idleAudio.scales.every((value) => value !== "scaleX(0.05)") &&
        idleAudio.sleeve > 1.01 &&
        idleAudio.transition === "0s",
      "idle keeps the band at half strength and the sleeve breathing, with no transition to restart"
    );

    /* The screen without transport still answers "how far in": one thin line
       with the rail's indigo -> lavender ramp, its own 3 px cream cursor and the
       same "left" wording. */
    const idleProgress = JSON.parse(
      await evaluate(
        `JSON.stringify({
          fill: parseFloat(document.getElementById('idle-fill').style.transform.replace(/[^0-9.]/g, '')),
          cursor: parseFloat(document.getElementById('idle-cursor').style.transform.replace(/[^0-9.]/g, '')),
          cursorWidth: getComputedStyle(document.getElementById('idle-cursor')).width,
          cursorHeight: getComputedStyle(document.getElementById('idle-cursor')).height,
          cursorColor: getComputedStyle(document.getElementById('idle-cursor')).backgroundColor,
          ramp: document.getElementById('idle-fill').style.backgroundImage,
          left: document.getElementById('idle-left').textContent,
          ledger: document.getElementById('ledger-left').textContent,
          height: getComputedStyle(document.getElementById('idle-fill')).height,
          focus: document.querySelector('.idle-progress').getAttribute('tabindex')
        })`
      )
    );
    check(
      idleProgress.fill > 0.2 &&
        idleProgress.fill < 0.5 &&
        Math.abs(idleProgress.cursor + 3 - idleProgress.fill * 700) < 1 &&
        idleProgress.cursorWidth === "3px" &&
        idleProgress.cursorHeight === "3px" &&
        idleProgress.cursorColor === "rgb(253, 240, 185)" &&
        idleProgress.ramp.indexOf("linear-gradient(90deg, rgb(65, 95, 206) 0%") === 0 &&
        idleProgress.ramp.indexOf("rgb(184, 53, 192)") === -1 &&
        idleProgress.left === idleProgress.ledger &&
        idleProgress.height === "3px" &&
        idleProgress.focus === null,
      "the idle screen carries the playhead on its own 3 px line, its own cursor and the same time left"
    );
    const idle = JSON.parse(
      await evaluate(
        "JSON.stringify({content:getComputedStyle(document.getElementById('content')).opacity,bar:getComputedStyle(document.getElementById('prog')).opacity,line:getComputedStyle(document.getElementById('idle-line')).opacity,orbit:getComputedStyle(document.getElementById('card')).animationName,size:parseFloat(getComputedStyle(document.querySelector('.idle-text')).fontSize),top:document.querySelector('.idle-text').getBoundingClientRect().top,overflow:getComputedStyle(document.querySelector('.idle-text')).overflow,ellipsis:getComputedStyle(document.querySelector('.idle-text')).textOverflow})"
      )
    );
    /* A track change is not input: the stage must stay idle. */
    status.title = "Galvanize";
    status.metadata_revision = 5;
    await waitFor("document.getElementById('title-1').textContent === 'Galvanize'", 4000);
    const stayedIdle = await evaluate("document.body.className.indexOf('is-idle') !== -1");

    /* The first OK after idle only wakes the stage and hands the focus back to
       the transport; it must not act on a control nobody can see. */
    await press("Enter", "Enter", 13);
    await sleep(100);
    const woke = JSON.parse(
      await evaluate(
        "JSON.stringify({idle:document.body.className.indexOf('is-idle')!==-1,active:document.activeElement.id})"
      )
    );
    check(
      idle.content === "0" &&
        idle.bar === "0" &&
        idle.line === "1" &&
        idle.size <= 60 &&
        idle.top >= 300 &&
        idle.top <= 450 &&
        idle.overflow === "hidden" &&
        idle.ellipsis === "ellipsis" &&
        woke.idle === false &&
        woke.active === "play-pause",
      "idle hides the chrome, sets one display line on the upper third, and the first OK only wakes it"
    );
    check(
      idle.orbit === "card-orbit" && stayedIdle,
      "idle runs the card orbit and survives a track change"
    );
    await evaluate("window.TaktStage.simulateIdle(false)");
    await sleep(300);

    /* ---------- the empty screen -------------------------------------------- */
    /* Nothing is loaded: the deliberate empty screen replaces the drawn sleeve
       and the 158 px wordmark. The dot follows the link, not playback. */
    status.title = null;
    status.artist = null;
    status.album = null;
    status.artwork_url = null;
    status.player_name = "Living Room";
    status.metadata_revision = 6;
    await waitFor("document.body.className.indexOf('is-empty') !== -1", 4000);
    await sleep(600);
    check(
      await evaluate("getComputedStyle(document.getElementById('band')).opacity === '0'"),
      "the empty screen fades the band out"
    );
    const emptyFrame = JSON.parse(
      await evaluate(
        "JSON.stringify((function(){var e=document.getElementById('empty');var c=getComputedStyle(e);var t=document.querySelector('.empty-title');var h=document.querySelector('.empty-hint');var d=getComputedStyle(document.getElementById('empty-dot'));return {className:document.body.className,opacity:c.opacity,left:e.getBoundingClientRect().left,top:e.getBoundingClientRect().top,title:t.textContent,titleSize:parseFloat(getComputedStyle(t).fontSize),hint:h.textContent,hintSize:parseFloat(getComputedStyle(h).fontSize),hintColor:getComputedStyle(h).color,dotColor:d.backgroundColor,dotRadius:d.borderRadius,room:document.getElementById('room').textContent,sep:document.getElementById('empty-sep').hidden,server:document.getElementById('empty-server').textContent,card:getComputedStyle(document.getElementById('card')).display,rail:getComputedStyle(document.getElementById('rail')).opacity,content:getComputedStyle(document.getElementById('content')).opacity,barStop:document.getElementById('progress-track').hasAttribute('data-focusable'),barTab:document.getElementById('progress-track').tabIndex,wordmark:document.querySelector('.brand-name').textContent,title1:document.getElementById('title-1').textContent};})())"
      )
    );
    status.state = "error";
    await waitFor("document.body.className.indexOf('state-offline') !== -1", 4000);
    await sleep(450);
    const emptyOffline = await evaluate(
      "getComputedStyle(document.getElementById('empty-dot')).backgroundColor === 'rgb(182, 172, 163)'"
    );
    await evaluate("window.TaktStage.simulateIdle(true)");
    await sleep(300);
    const emptyOrbit = await evaluate(
      "getComputedStyle(document.getElementById('empty')).animationName === 'empty-orbit' && getComputedStyle(document.getElementById('idle-line')).display === 'none'"
    );
    await evaluate("window.TaktStage.simulateIdle(false)");
    status.state = "playing";
    status.player_name = "Takt";
    status.title = "Galvanize The Floor";
    status.artist = "The Chemical Brothers";
    status.album = "Push the Button";
    status.artwork_url = null;
    status.metadata_revision = 7;
    await waitFor("document.body.className.indexOf('is-empty') === -1", 4000);
    check(
      emptyFrame.opacity === "1" &&
        emptyFrame.left === 96 &&
        emptyFrame.top === 300 &&
        emptyFrame.title === "Nothing playing" &&
        emptyFrame.titleSize === 32 &&
        emptyFrame.hint === "Start from Music Assistant on your phone" &&
        emptyFrame.hintSize === 28 &&
        emptyFrame.hintColor === "rgb(182, 172, 163)" &&
        emptyFrame.dotColor === "rgb(184, 53, 192)" &&
        emptyFrame.dotRadius === "50%" &&
        emptyFrame.room === "Living Room" &&
        emptyFrame.sep === false &&
        emptyFrame.server === "ma.local:8927" &&
        emptyFrame.card === "none" &&
        emptyFrame.rail === "0" &&
        emptyFrame.content === "0" &&
        emptyFrame.wordmark === "Takt" &&
        emptyFrame.title1 === "" &&
        emptyFrame.barStop === false &&
        emptyFrame.barTab === -1 &&
        emptyOffline &&
        emptyOrbit,
      "the empty screen drops the fake card, prints one statement and the player's address, and its dot follows the link"
    );

    /* ---------- the credits page (the Easter egg) -------------------------- */
    /* The wordmark is pointer-only: it fires a bubbling `takt:credits` and the
       page opens over the stage. It is a page, not a transport stop. */
    await waitFor("document.body.className.indexOf('state-playing') !== -1");
    await evaluate("document.getElementById('play-pause').focus()");
    const creditsStartsHidden = await evaluate(
      "document.getElementById('credits').hidden === true"
    );
    const creditsTitleBefore = await evaluate("document.getElementById('title-1').textContent");
    const creditsFillBefore = await evaluate(
      "document.getElementById('progress-fill').style.transform"
    );
    const creditsOpened = await evaluate(
      "window.__credits = 0; document.addEventListener('takt:credits', function () { window.__credits += 1; }); document.getElementById('brand').click(); window.__credits"
    );
    await sleep(260);
    const creditsFrame = JSON.parse(
      await evaluate(
        `JSON.stringify((function(){
          var c=document.getElementById('credits');
          var page=c.querySelector('.credits-page');
          var r=c.getBoundingClientRect();
          var p=page.getBoundingClientRect();
          var cs=getComputedStyle(c);
          var ps=getComputedStyle(page);
          function t(sel){var e=c.querySelector(sel);return e?e.textContent:null;}
          function size(sel){var e=c.querySelector(sel);return e?parseFloat(getComputedStyle(e).fontSize):0;}
          function family(sel){var e=c.querySelector(sel);return e?getComputedStyle(e).fontFamily:'';}
          return {
            open:c.hidden===false,
            role:c.getAttribute('role'),
            modal:c.getAttribute('aria-modal'),
            tabIndex:c.tabIndex,
            focus:document.activeElement.id,
            left:Math.round(r.left), top:Math.round(r.top),
            right:Math.round(r.right), bottom:Math.round(r.bottom),
            background:cs.backgroundImage,
            position:cs.position,
            animation:cs.animationName,
            duration:cs.animationDuration,
            iterations:cs.animationIterationCount,
            pageLeft:Math.round(p.left),
            pageAnimation:ps.animationName,
            pageDuration:ps.animationDuration,
            wordmark:t('.credits-wordmark'), sub:t('.credits-sub'), built:t('.credits-built'),
            gta:t('.credits-gta'), punch:t('.credits-punch'), hint:t('.credits-hint'),
            credits:[].slice.call(c.querySelectorAll('.credits-credit')).map(function(e){return e.textContent;}),
            punchSize:size('.credits-punch'), punchFamily:family('.credits-punch'),
            wordmarkSize:size('.credits-wordmark'), metaSize:size('.credits-credit'),
            subSize:size('.credits-sub'), gtaSize:size('.credits-gta'),
            stops:c.querySelectorAll('[data-focusable]').length,
            hintLeft:Math.round(c.querySelector('.credits-hint').getBoundingClientRect().left),
            hintBottom:Math.round(c.querySelector('.credits-hint').getBoundingClientRect().bottom),
            bodyClass:document.body.className
          };
        })())`
      )
    );
    check(
      creditsStartsHidden &&
        creditsOpened === 1 &&
        creditsFrame.open &&
        creditsFrame.role === "dialog" &&
        creditsFrame.modal === "true" &&
        creditsFrame.tabIndex === -1 &&
        creditsFrame.stops === 0 &&
        creditsFrame.left === 0 &&
        creditsFrame.top === 0 &&
        creditsFrame.right === 1920 &&
        creditsFrame.bottom === 1080 &&
        creditsFrame.background.indexOf("linear-gradient") === 0 &&
        creditsFrame.background.indexOf("rgb(6, 13, 39)") !== -1 &&
        creditsFrame.position === "fixed" &&
        creditsFrame.pageLeft === 96 &&
        creditsFrame.hintLeft === 96 &&
        creditsFrame.hintBottom >= 880 &&
        creditsFrame.hintBottom <= 960 &&
        creditsFrame.focus === "credits" &&
        creditsFrame.bodyClass.indexOf("credits-open") !== -1,
      "the wordmark's takt:credits opens a full-screen ink page on the 96 px safe edge, with no d-pad stop inside"
    );
    check(
      creditsFrame.wordmark === "Takt" &&
        creditsFrame.sub === "A Music Assistant client for LG webOS" &&
        creditsFrame.built === "Built 1\u201318 November 2026" &&
        creditsFrame.gta === "GTA VI ships 19 November 2026" &&
        creditsFrame.punch === "We got Takt before GTA 6" &&
        creditsFrame.credits.join("|") ===
          "Protocol: syncspin (MIT OR Apache-2.0)|Music and artwork: Music Assistant|No game assets, fonts or screenshots were used." &&
        creditsFrame.hint.trim() === "Press a button" &&
        creditsFrame.punchSize === 120 &&
        creditsFrame.punchFamily.indexOf("Anton") !== -1 &&
        creditsFrame.punchSize > creditsFrame.wordmarkSize &&
        creditsFrame.punchSize > creditsFrame.subSize &&
        creditsFrame.punchSize > creditsFrame.gtaSize &&
        creditsFrame.punchSize > creditsFrame.metaSize,
      "the credits page prints the project lore, and the punchline is the one large Anton line"
    );
    check(
      creditsFrame.animation === "credits-field-in" &&
        creditsFrame.duration === "0.2s" &&
        creditsFrame.iterations === "1" &&
        creditsFrame.pageAnimation === "credits-page-in" &&
        creditsFrame.pageDuration === "0.2s",
      "the credits page enters on a 200 ms fade and upward drift, once, with no loop"
    );
    await sleep(600);
    const creditsUnder = JSON.parse(
      await evaluate(
        "JSON.stringify({open:document.getElementById('credits').hidden===false,active:document.activeElement.id,title:document.getElementById('title-1').textContent,state:document.getElementById('state-word').textContent,fill:document.getElementById('progress-fill').style.transform})"
      )
    );
    check(
      creditsUnder.open &&
        creditsUnder.active === "credits" &&
        creditsUnder.title === creditsTitleBefore &&
        creditsUnder.state === "Playing" &&
        creditsUnder.fill !== creditsFillBefore,
      "the stage keeps its title and focus while the credits page is open, and the playhead keeps ticking underneath"
    );
    await press("ArrowUp", "ArrowUp", 38);
    await sleep(60);
    const creditsExit = JSON.parse(
      await evaluate(
        "JSON.stringify({closing:document.getElementById('credits').className.indexOf('is-closing')!==-1,hidden:document.getElementById('credits').hidden,animation:getComputedStyle(document.getElementById('credits')).animationName,duration:getComputedStyle(document.getElementById('credits')).animationDuration})"
      )
    );
    await sleep(300);
    const creditsAfterKey = JSON.parse(
      await evaluate(
        "JSON.stringify({hidden:document.getElementById('credits').hidden,focus:document.activeElement.id,body:document.body.className})"
      )
    );
    check(
      creditsExit.closing === true &&
        creditsExit.hidden === false &&
        creditsExit.animation === "credits-field-out" &&
        creditsExit.duration === "0.2s" &&
        creditsAfterKey.hidden === true &&
        creditsAfterKey.focus === "play-pause" &&
        creditsAfterKey.body.indexOf("credits-open") === -1,
      "any key closes the credits page on its 200 ms exit and hands the focus back to the rail control, instead of moving it"
    );
    await evaluate("document.getElementById('brand').click()");
    await sleep(260);
    const creditsClickOpen = await evaluate("document.getElementById('credits').hidden === false");
    await evaluate("document.getElementById('credits').click()");
    await sleep(300);
    const creditsClickClosed = await evaluate(
      "document.getElementById('credits').hidden === true && document.activeElement.id === 'play-pause'"
    );
    await evaluate("document.getElementById('brand').click()");
    await sleep(260);
    await press("Escape", "Escape", 27);
    await sleep(300);
    const creditsEscapeClosed = await evaluate(
      "document.getElementById('credits').hidden === true && document.activeElement.id === 'play-pause'"
    );
    await evaluate("document.getElementById('brand').click()");
    await sleep(260);
    await press("Unidentified", "Unidentified", 461);
    await sleep(300);
    const creditsBackClosed = await evaluate(
      "document.getElementById('credits').hidden === true && document.activeElement.id === 'play-pause'"
    );
    check(
      creditsClickOpen && creditsClickClosed && creditsEscapeClosed && creditsBackClosed,
      "a click, Escape and Back close the credits page and return the focus"
    );
    await evaluate("document.getElementById('next').focus()");
    await evaluate("document.getElementById('brand').click()");
    await sleep(240);
    await evaluate("document.getElementById('credits').click()");
    await sleep(280);
    const creditsNextFocus = await evaluate("document.activeElement.id");
    await evaluate("document.getElementById('progress-track').focus()");
    await evaluate("document.getElementById('brand').click()");
    await sleep(240);
    await evaluate("document.getElementById('credits').click()");
    await sleep(280);
    const creditsBarFocus = await evaluate("document.activeElement.id");
    await evaluate("document.getElementById('settings-toggle').focus()");
    await evaluate("document.getElementById('brand').click()");
    await sleep(240);
    await evaluate("document.getElementById('credits').click()");
    await sleep(280);
    const creditsSettingsFocus = await evaluate("document.activeElement.id");
    check(
      creditsNextFocus === "next" &&
        creditsBarFocus === "progress-track" &&
        creditsSettingsFocus === "settings-toggle",
      "the credits page hands the focus back to the control the remote came from: Next, the seek bar or Settings"
    );
    /* The page never opens over Settings: the wordmark is unreachable there, and
       a programmatic event must not stack two full-screen surfaces either. */
    await evaluate("document.getElementById('settings-toggle').click()");
    await sleep(320);
    await evaluate("document.getElementById('brand').click()");
    const creditsOverSettings = await evaluate(
      "document.getElementById('credits').hidden === true && document.body.className.indexOf('settings-open') !== -1"
    );
    await evaluate("document.getElementById('settings-done').click()");
    await sleep(320);
    const settingsUnaffected = await evaluate(
      "document.getElementById('credits').hidden === true && document.getElementById('settings-panel').hidden === true && document.activeElement.id === 'settings-toggle'"
    );
    check(
      creditsOverSettings && settingsUnaffected,
      "the wordmark does not open the credits page over Settings, and the panel is unaffected"
    );
    const creditsStops = JSON.parse(
      await evaluate(
        "JSON.stringify({inside:document.querySelectorAll('#credits [data-focusable]').length,tabIndex:document.getElementById('credits').tabIndex,brand:document.getElementById('brand').hasAttribute('data-focusable'),brandTab:document.getElementById('brand').tabIndex,hidden:document.getElementById('credits').hidden})"
      )
    );
    await evaluate("document.getElementById('play-pause').focus()");
    await press("ArrowUp", "ArrowUp", 38);
    const creditsUp = await evaluate("document.activeElement.id");
    await press("ArrowDown", "ArrowDown", 40);
    const creditsDown = await evaluate("document.activeElement.id");
    check(
      creditsStops.inside === 0 &&
        creditsStops.tabIndex === -1 &&
        creditsStops.brand === false &&
        creditsStops.brandTab === -1 &&
        creditsStops.hidden === true &&
        creditsUp === "settings-toggle" &&
        creditsDown === "play-pause",
      "the credits page is not a d-pad stop and the wordmark stays out of the focus path"
    );

    /* ---------- the settings surface --------------------------------------- */
    /* A 1000 px left-aligned surface that uses the full height: labels at
       30 px, values at 40-52 px, notes at 28 px and no target under 64 px. */
    await evaluate("document.getElementById('settings-toggle').click()");
    await sleep(320);
    const settingsFrame = JSON.parse(
      await evaluate(
        `JSON.stringify((function(){
          var p=document.getElementById('settings-panel');
          var r=p.getBoundingClientRect();
          var rows=[].slice.call(p.querySelectorAll('.srow')).filter(function(row){return row.offsetHeight>0;}).map(function(row){return Math.round(row.getBoundingClientRect().height);});
          function h(id){return Math.round(document.getElementById(id).getBoundingClientRect().height);}
          function w(id){return Math.round(document.getElementById(id).getBoundingClientRect().width);}
          return {
            open:document.body.className.indexOf('settings-open')!==-1,
            hidden:p.hidden, left:Math.round(r.left), top:Math.round(r.top),
            right:Math.round(r.right), bottom:Math.round(r.bottom),
            scrollHeight:p.scrollHeight, clientHeight:p.clientHeight,
            focus:document.activeElement.id,
            expanded:document.getElementById('settings-toggle').getAttribute('aria-expanded'),
            label:parseFloat(getComputedStyle(p.querySelector('.srow-label')).fontSize),
            value:parseFloat(getComputedStyle(document.getElementById('pin')).fontSize),
            nameValue:parseFloat(getComputedStyle(document.getElementById('player-name-value')).fontSize),
            note:parseFloat(getComputedStyle(p.querySelector('.srow-note')).fontSize),
            close:w('settings-close'), closeH:h('settings-close'),
            reveal:h('pin-reveal'), nameEdit:h('name-edit'),
            done:h('settings-done'), save:h('save-config'), cancel:h('cancel-config'),
            rows:rows,
            rowLabels:[].slice.call(p.querySelectorAll('.srow')).filter(function(row){return row.offsetHeight>0;}).map(function(row){return row.querySelector('.srow-label').textContent;}),
            pin:document.getElementById('pin').textContent,
            pinInputW:w('pin-input'),
            revealText:document.getElementById('pin-reveal').textContent,
            saveDisabled:document.getElementById('save-config').disabled,
            cancelDisabled:document.getElementById('cancel-config').disabled,
            server:document.getElementById('server').textContent,
            serverField:!!document.querySelector('.server-row input'),
            serverWord:document.getElementById('server-word').textContent,
            dot:getComputedStyle(document.getElementById('server-dot')).backgroundColor,
            delayValue:document.getElementById('output-delay-value').textContent,
            delayValueSize:parseFloat(getComputedStyle(document.getElementById('output-delay-value')).fontSize),
            delayEdit:h('delay-edit'),
            delayStepsHidden:document.getElementById('delay-steps').hidden,
            tint:document.querySelectorAll('.theme-option').length
          };
        })())`
      )
    );
    check(
      settingsFrame.open &&
        settingsFrame.hidden === false &&
        settingsFrame.left === 0 &&
        settingsFrame.top === 0 &&
        settingsFrame.right === 1000 &&
        settingsFrame.bottom === 1080 &&
        settingsFrame.focus === "name-edit" &&
        settingsFrame.expanded === "true" &&
        settingsFrame.label === 30 &&
        settingsFrame.value === 52 &&
        settingsFrame.nameValue >= 40 &&
        settingsFrame.nameValue <= 44 &&
        settingsFrame.note === 28 &&
        settingsFrame.close === 64 &&
        settingsFrame.closeH === 64 &&
        settingsFrame.reveal >= 64 &&
        settingsFrame.nameEdit >= 64 &&
        settingsFrame.done >= 80 &&
        settingsFrame.save >= 64 &&
        settingsFrame.cancel >= 64 &&
        settingsFrame.rows.length === 4 &&
        settingsFrame.rows.every((height) => height >= 116) &&
        settingsFrame.rowLabels.join("|") === "Setup PIN|Server|Player name|TV audio delay" &&
        settingsFrame.scrollHeight === settingsFrame.clientHeight &&
        settingsFrame.tint === 0,
      "settings opens as a 1000 px full-height surface: four rows, 30 px labels, 40-52 px values, 64 px targets, focus on Change"
    );
    check(
      settingsFrame.pin === "••••" &&
        settingsFrame.pinInputW === 0 &&
        settingsFrame.revealText === "Reveal" &&
        settingsFrame.server === "ma.local:8927" &&
        settingsFrame.serverField === false &&
        settingsFrame.serverWord === "Connected" &&
        settingsFrame.dot === "rgb(184, 53, 192)" &&
        settingsFrame.saveDisabled === true &&
        settingsFrame.cancelDisabled === true,
      "the PIN is hidden as bullets, the server is a read-only connected value, and Save/Cancel start disabled"
    );
    check(
      settingsFrame.delayValue === "0 ms" &&
        settingsFrame.delayValueSize === 42 &&
        settingsFrame.delayEdit >= 64 &&
        settingsFrame.delayStepsHidden === true,
      "the TV audio delay row shows 0 ms at 42 px behind a 64 px Change control, with the steps hidden"
    );

    const pinRevealed = await evaluate(
      "document.getElementById('pin-reveal').click(); document.getElementById('pin').textContent === '4921' && document.getElementById('pin-reveal').textContent === 'Hide' && document.getElementById('pin-reveal').getAttribute('aria-pressed') === 'true'"
    );
    const pinHiddenAgain = await evaluate(
      "document.getElementById('pin-reveal').click(); document.getElementById('pin').textContent === '••••' && document.getElementById('pin-reveal').textContent === 'Reveal'"
    );
    check(
      pinRevealed && pinHiddenAgain,
      "Reveal shows the one setup PIN and Hide puts the bullets back"
    );

    /* The delay is not a draft: a step writes at once, the row reads the value
       back, and Save/Cancel never see it. */
    const delayRevealed = JSON.parse(
      await evaluate(
        "document.getElementById('delay-edit').click(); JSON.stringify({changeHidden:document.getElementById('delay-edit').hidden,stepsHidden:document.getElementById('delay-steps').hidden,focus:document.activeElement.id,down:Math.round(document.getElementById('delay-down').getBoundingClientRect().height),up:Math.round(document.getElementById('delay-up').getBoundingClientRect().height)})"
      )
    );
    const configsBeforeStep = configBodies.length;
    await evaluate("document.getElementById('delay-up').click()");
    const optimisticTen = await evaluate(
      "document.getElementById('output-delay-value').textContent"
    );
    const appliedOnce = await waitForNode(
      () => status.output_delay_ms === 10 && configBodies.length === configsBeforeStep + 1,
      3000
    );
    await sleep(250);
    const firstStepBody = configBodies[configBodies.length - 1];
    check(
      delayRevealed.changeHidden === true &&
        delayRevealed.stepsHidden === false &&
        delayRevealed.focus === "delay-up" &&
        delayRevealed.down === 64 &&
        delayRevealed.up === 64 &&
        optimisticTen === "10 ms" &&
        appliedOnce &&
        firstStepBody.pin === PIN &&
        firstStepBody.output_delay_ms === 10 &&
        firstStepBody.player_name === undefined &&
        (await evaluate(
          "document.getElementById('output-delay-value').textContent === '10 ms' && document.getElementById('save-config').disabled === true && document.getElementById('cancel-config').disabled === true"
        )),
      "Change reveals two 64 px steps; + writes the new delay at once with the PIN and leaves Save/Cancel disabled"
    );

    const configsBeforeClamp = configBodies.length;
    await evaluate("for (var i=0;i<19;i+=1){document.getElementById('delay-up').click();}");
    const clampReached = await waitForNode(
      () => status.output_delay_ms === 200 && configBodies.length === configsBeforeClamp + 19,
      5000
    );
    const configsAtClamp = configBodies.length;
    const pressedPastClamp = await evaluate(
      "document.getElementById('delay-up').click(); document.getElementById('output-delay-value').textContent"
    );
    await sleep(250);
    check(
      clampReached && pressedPastClamp === "200 ms" && configBodies.length === configsAtClamp,
      "+ steps by 10 and clamps at 200: a press at the bound writes nothing"
    );

    /* A refused write must not leave the optimistic number on screen. */
    refuseConfig = 500;
    const optimisticDown = await evaluate(
      "document.getElementById('delay-down').click(); document.getElementById('output-delay-value').textContent"
    );
    const refusalReverted = await waitFor(
      "document.getElementById('output-delay-value').textContent === '200 ms' && document.getElementById('config-message').textContent === 'Delay not applied'",
      3000
    );
    check(
      optimisticDown === "190 ms" && refusalReverted && status.output_delay_ms === 200,
      "a refused delay write reverts the optimistic step to the last status value and reports it"
    );

    /* The row must show the value in force, even one a hand-edited config set
       above the range its own steps write. */
    status.output_delay_ms = 500;
    const showedAboveRange = await waitFor(
      "document.getElementById('output-delay-value').textContent === '500 ms'",
      2500
    );
    status.output_delay_ms = 200;
    const backInRange = await waitFor(
      "document.getElementById('output-delay-value').textContent === '200 ms'",
      2500
    );
    check(
      showedAboveRange && backInRange,
      "the row reads back the value in force above its step range, and follows status back"
    );

    const nameBefore = JSON.parse(
      await evaluate(
        "JSON.stringify({value:document.getElementById('player-name-value').textContent,input:document.getElementById('player-name').value,hidden:document.getElementById('player-name').hidden})"
      )
    );
    const editing = await evaluate(
      "document.getElementById('name-edit').click(); document.activeElement.id === 'player-name' && document.getElementById('player-name').hidden === false && document.getElementById('player-name-value').hidden === true"
    );
    const dirtyOn = await evaluate(
      "var i=document.getElementById('player-name'); i.value='Kitchen'; i.dispatchEvent(new Event('input',{bubbles:true})); document.getElementById('save-config').disabled === false && document.getElementById('cancel-config').disabled === false"
    );
    const cancelled = await evaluate(
      "document.getElementById('cancel-config').click(); document.getElementById('player-name').value === 'Takt' && document.getElementById('player-name-value').textContent === 'Takt' && document.getElementById('player-name').hidden === true && document.getElementById('output-delay-value').textContent === '200 ms' && document.getElementById('save-config').disabled === true && document.getElementById('cancel-config').disabled === true"
    );
    check(
      nameBefore.value === "Takt" &&
        nameBefore.input === "Takt" &&
        nameBefore.hidden === true &&
        editing &&
        dirtyOn &&
        cancelled,
      "Save and Cancel are disabled until the name changes, and Cancel restores the name only"
    );

    await evaluate(
      "document.getElementById('name-edit').click(); var i=document.getElementById('player-name'); i.value='Kitchen'; i.dispatchEvent(new Event('input',{bubbles:true})); document.getElementById('save-config').click();"
    );
    const showedSaved = await waitFor(
      "document.getElementById('config-message').textContent === 'Saved'",
      3000
    );
    const returned = await waitFor(
      "document.getElementById('settings-panel').hidden === true && document.activeElement.id === 'settings-toggle'",
      4000
    );
    check(
      showedSaved &&
        returned &&
        lastConfig !== null &&
        lastConfig.pin === PIN &&
        lastConfig.server === "ma.local:8927" &&
        lastConfig.player_name === "Kitchen" &&
        lastConfig.output_delay_ms === undefined &&
        status.player_name === "Kitchen",
      "Save commits the player name through /api/config with the on-screen PIN, shows Saved and returns focus to Settings"
    );

    /* The unsaved edit is a question row, not a silent discard and not a modal. */
    await evaluate("document.getElementById('settings-toggle').click()");
    await sleep(320);
    await evaluate(
      "document.getElementById('name-edit').click(); var i=document.getElementById('player-name'); i.value='Garage'; i.dispatchEvent(new Event('input',{bubbles:true}));"
    );
    await press("Escape", "Escape", 27);
    await sleep(140);
    const asked = JSON.parse(
      await evaluate(
        "JSON.stringify({open:document.body.className.indexOf('settings-open')!==-1,panel:document.getElementById('settings-panel').hidden,row:document.getElementById('discard-row').hidden,focus:document.activeElement.id,label:document.querySelector('#discard-row .srow-label').textContent})"
      )
    );
    await evaluate("document.getElementById('discard-no').click()");
    const keptEditing = await evaluate(
      "document.getElementById('discard-row').hidden === true && document.body.className.indexOf('settings-open')!==-1 && document.activeElement.id === 'player-name'"
    );
    await press("Escape", "Escape", 27);
    await sleep(140);
    await evaluate("document.getElementById('discard-yes').click()");
    const discardedClosed = await waitFor(
      "document.getElementById('settings-panel').hidden === true",
      3000
    );
    const reverted = await evaluate(
      "document.getElementById('player-name').value === 'Kitchen' && document.getElementById('player-name-value').textContent === 'Kitchen' && document.getElementById('save-config').disabled === true"
    );
    check(
      asked.open &&
        asked.panel === false &&
        asked.row === false &&
        asked.focus === "discard-no" &&
        asked.label === "Discard changes?" &&
        keptEditing &&
        discardedClosed &&
        reverted,
      "closing with an unsaved name asks in a panel row, Keep editing returns to the field and Discard reverts it"
    );

    /* No tint buttons must not mean a d-pad dead end: Down cycles through the
       panel and every stop stays inside it. */
    await evaluate("document.getElementById('settings-toggle').click()");
    await sleep(320);
    for (let step = 0; step < 6; step += 1) {
      await press("ArrowDown", "ArrowDown", 40);
    }
    const trapped = await evaluate(
      "document.getElementById('settings-panel').contains(document.activeElement) && document.activeElement.id !== ''"
    );
    check(trapped, "Down cycles the surface and the remote never leaves the focus trap");
    await evaluate("document.getElementById('settings-done').click()");
    await sleep(300);

    await evaluate("document.getElementById('settings-toggle').click()");
    await sleep(320);
    check(
      await evaluate("document.body.className.indexOf('settings-open') !== -1"),
      "the settings surface opens from the Settings control"
    );
    await press("Escape", "Escape", 27);
    await sleep(300);
    const escapeClosed = await evaluate("document.body.className.indexOf('settings-open') === -1");

    /* Back (keyCode 461) is the remote's own close key, and the scrim fades
       rather than vanishing over the 1920x1080 surface in one frame. */
    await evaluate("document.getElementById('settings-toggle').click()");
    await sleep(300);
    await press("Unidentified", "Unidentified", 461);
    await sleep(90);
    const fading = await evaluate(
      "document.getElementById('settings-backdrop').hidden === false && document.getElementById('settings-backdrop').className.indexOf('is-closing') !== -1 && document.getElementById('settings-panel').className.indexOf('is-closing') !== -1"
    );
    await sleep(250);
    check(
      escapeClosed &&
        fading &&
        (await evaluate(
          "document.body.className.indexOf('settings-open') === -1 && document.getElementById('settings-panel').hidden === true && document.activeElement.id === 'settings-toggle'"
        )),
      "Escape and Back close the surface, fading the scrim out first"
    );

    await evaluate(
      "window.__beacons=[]; navigator.sendBeacon=function(url){window.__beacons.push(url); return true;};"
    );

    /* The daemon owns the leave rule: a held foreground subscription pauses when another app
       takes the screen. The page has no hide/blur rule and never polls the foreground itself. */
    foreground.hits = 0;
    await evaluate(
      "window.dispatchEvent(new Event('pagehide')); window.dispatchEvent(new Event('blur')); document.dispatchEvent(new Event('visibilitychange'));"
    );
    await sleep(2600);
    check(
      (await evaluate("JSON.stringify(window.__beacons)")) === "[]" && foreground.hits === 0,
      "hiding or blurring the page sends nothing: the daemon owns the leave rule"
    );

    /* ---------- the LAN fallback ------------------------------------------- */
    /* Without the loopback readout the same row is a four-digit entry: one PIN
       concept, never a readout and an entry side by side. */
    pinReadout = false;
    await cdp.send("Page.navigate", { url: `http://127.0.0.1:${PORT}/` });
    await waitFor("document.readyState === 'complete' && !!document.getElementById('play-pause')");
    await waitFor("document.body.className.indexOf('state-playing') !== -1");
    await evaluate("document.getElementById('settings-toggle').click()");
    await sleep(320);
    check(
      await evaluate(
        "document.getElementById('pin').hidden === true && document.getElementById('pin-reveal').hidden === true && document.getElementById('pin-input').hidden === false && document.getElementById('pin-input').getBoundingClientRect().width > 0 && document.getElementById('pin-hint').textContent === 'Read the PIN on the TV, then type it here'"
      ),
      "from the LAN the one PIN row becomes an entry, never a readout and an entry together"
    );
  } finally {
    chrome.kill("SIGKILL");
    server.close();
  }

  const failed = results.filter((result) => !result.ok);
  results.forEach((result, index) =>
    console.log(
      `${result.ok ? "PASS" : "FAIL"} ${String(index + 1).padStart(2, "0")} ${result.label}`
    )
  );
  console.log(`\n${results.length} checks`);
  if (failed.length) {
    console.log(`${failed.length} failure(s)`);
    process.exitCode = 1;
  }
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
