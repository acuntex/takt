"use strict";

// Minimal Chrome DevTools Protocol client for the TV UI and the local headless harness.
// Uses the WebSocket implementation built into Node 22, so no npm dependency is needed.

const http = require("http");

const TV_HOST = process.env.TV_HOST || "";
const TV_CDP_PORT = process.env.TV_CDP_PORT || "9998";
const APP_PORT = process.env.TV_APP_PORT || "3927";

function tvHost() {
  if (!TV_HOST) {
    throw new Error("set TV_HOST to the TV address or hostname (see README.md)");
  }
  return TV_HOST;
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
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
    async evaluate(expression) {
      const response = await this.send("Runtime.evaluate", {
        expression,
        returnByValue: true,
        awaitPromise: true,
      });
      if (response.exceptionDetails) {
        throw new Error(JSON.stringify(response.exceptionDetails));
      }
      return response.result.value;
    },
    async screenshot(file) {
      const shot = await this.send("Page.captureScreenshot", { format: "png" });
      require("fs").writeFileSync(file, Buffer.from(shot.data, "base64"));
      return file;
    },
  };
}

async function appPage(port) {
  const host = tvHost();
  const targetPort = port || APP_PORT;
  const targets = await getJson(`http://${host}:${TV_CDP_PORT}/json`);
  const page = targets.find(
    (target) => target.type === "page" && String(target.url).includes(`127.0.0.1:${targetPort}`)
  );
  if (!page) {
    throw new Error(`TV app page not found (is the app open? CDP http://${host}:${TV_CDP_PORT})`);
  }
  return page;
}

async function openApp() {
  const page = await appPage();
  const cdp = cdpClient(page.webSocketDebuggerUrl);
  await cdp.ready;
  await cdp.send("Runtime.enable");
  await cdp.send("Page.enable");
  return { page, cdp };
}

module.exports = {
  APP_PORT,
  TV_CDP_PORT,
  TV_HOST,
  cdpClient,
  appPage,
  getJson,
  openApp,
  sleep,
};
