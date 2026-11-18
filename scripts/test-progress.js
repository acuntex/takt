#!/usr/bin/env node
"use strict";

var assert = require("assert");
var progress = require("../web/progress.js");

function status(overrides) {
  var base = {
    state: "playing",
    title: "Track",
    artist: "Artist",
    progress_ms: 5000,
    duration_ms: 300000,
    playback_speed: 1000,
    metadata_revision: 0,
  };
  var key;
  for (key in overrides) {
    if (Object.prototype.hasOwnProperty.call(overrides, key)) {
      base[key] = overrides[key];
    }
  }
  return base;
}

var checks = 0;
function check(actual, expected, label) {
  assert.strictEqual(actual, expected, label);
  checks += 1;
}

var first = progress.update(null, status({}), 1000);
check(progress.position(first, 1000), 5000, "first snapshot starts at server position");
check(progress.position(first, 2000), 6000, "playing advances with elapsed time");

var unchanged = progress.update(first, status({}), 2000);
check(unchanged.at, 1000, "unchanged snapshot keeps the original base timestamp");
check(progress.position(unchanged, 3000), 7000, "unchanged snapshot keeps accumulating");

var seekZero = progress.update(unchanged, status({ progress_ms: 0, metadata_revision: 1 }), 3000);
check(seekZero.ms, 0, "new metadata revision re-bases even when progress_ms is unchanged to 0");
check(seekZero.at, 3000, "new metadata revision resets the base timestamp");
check(progress.position(seekZero, 5000), 2000, "seek-to-0 continues from zero");

var sameRevision = progress.update(
  seekZero,
  status({ progress_ms: 0, metadata_revision: 1 }),
  5000
);
check(sameRevision.at, 3000, "unchanged revision keeps the original base timestamp");
check(progress.position(sameRevision, 7000), 4000, "unchanged revision keeps accumulating");

var paused = progress.update(unchanged, status({ state: "paused" }), 3000);
check(paused.ms, 5000, "state change re-bases to the server position");
check(progress.position(paused, 10000), 5000, "paused does not advance");

var fast = progress.update(unchanged, status({ playback_speed: 2000 }), 2000);
check(fast.ms, 6000, "speed change preserves the interpolated position");
check(progress.position(fast, 2500), 7000, "speed per mille scales the advance");

var changedTrack = progress.update(unchanged, status({ title: "Next" }), 3000);
check(changedTrack.ms, 5000, "track change re-bases to the server position");
check(changedTrack.at, 3000, "track change resets the base timestamp");

var clamped = progress.update(first, status({ progress_ms: 299000, duration_ms: 300000 }), 4000);
check(progress.position(clamped, 6000), 300000, "position clamps to the duration");

var idle = progress.update(clamped, status({ state: "idle", progress_ms: 0 }), 5000);
check(progress.position(idle, 20000), 0, "idle stays at the re-based position");

console.log("progress helper: " + checks + " checks passed");
