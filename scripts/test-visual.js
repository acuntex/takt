#!/usr/bin/env node
"use strict";

var assert = require("assert");
var visual = require("../web/visual.js");

var checks = 0;
function check(actual, expected, label) {
  assert.strictEqual(actual, expected, label);
  checks += 1;
}

function approx(actual, expected, tolerance, label) {
  assert.ok(Math.abs(actual - expected) <= tolerance, label + ": " + actual + " != " + expected);
  checks += 1;
}

var clean = visual.target({});
check(clean.level, 0, "missing levels default to zero");

var clamped = visual.target({ level: 2, bass: -1, mid: 0.5, treble: 3 });
check(clamped.level, 1, "level clamps high");
check(clamped.bass, 0, "bass clamps low");
check(clamped.mid, 0.5, "mid passes through");
check(clamped.treble, 1, "treble clamps high");

var start = visual.create();
var attacked = visual.update(start, { level: 1, bass: 1, mid: 1, treble: 1 }, 0);
var released = visual.update(attacked, start, 100);
assert.ok(attacked.pulse > 0.9, "a kick fires the impulse envelope");
assert.ok(released.pulse <= attacked.pulse * 0.6 + 0.001, "the impulse decays fast");
assert.ok(visual.update(attacked, attacked, 50).pulse < 0.6, "steady audio does not stay pumped");
approx(attacked.bass, 0.85, 0.001, "attack is fast");
assert.ok(released.bass < attacked.bass, "release lowers the level");
assert.ok(released.bass > 0.2, "release is gradual");

check(visual.shouldIdle(29999, 0, true, false), false, "playing waits 30 seconds");
check(visual.shouldIdle(30000, 0, true, false), true, "playing idles at 30 seconds");
check(visual.shouldIdle(15000, 0, false, false), true, "paused idles at 15 seconds");
check(visual.shouldIdle(60000, 0, true, true), false, "open settings never idles");

/* The sleeve breathes with the bass, rises up to 3 px and takes the kick on
   top; one composited transform, capped and stable in shape. */
function sleeveScale(state) {
  return parseFloat(visual.sleeve(state).replace(/^.*scale\(([^)]+)\)$/, "$1"));
}

check(
  visual.sleeve({ bass: 0 }),
  "translateZ(0) translateY(0.00px) scale(1.0000)",
  "silence is the identity transform"
);
check(
  visual.sleeve({ bass: 1 }),
  "translateZ(0) translateY(-3.00px) scale(1.0600)",
  "full bass reaches the 6 % peak and the 3 px lift"
);
check(
  visual.sleeve({ bass: 2 }),
  "translateZ(0) translateY(-3.00px) scale(1.0600)",
  "bass above one is clamped"
);
check(
  visual.sleeve(null),
  "translateZ(0) translateY(0.00px) scale(1.0000)",
  "no state is the identity transform"
);
check(
  visual.sleeve({}),
  "translateZ(0) translateY(0.00px) scale(1.0000)",
  "missing fields are the identity transform"
);
check(
  visual.sleeve({ bass: NaN, pulse: NaN }),
  "translateZ(0) translateY(0.00px) scale(1.0000)",
  "NaN fields are the identity transform"
);
approx(sleeveScale({ bass: 0.62 }), 1.0372, 0.0001, "a real track grows the card by 3.7 %");
assert.ok(
  sleeveScale({ bass: 0 }) < sleeveScale({ bass: 0.3 }) &&
    sleeveScale({ bass: 0.3 }) < sleeveScale({ bass: 0.62 }) &&
    sleeveScale({ bass: 0.62 }) < sleeveScale({ bass: 1 }),
  "the sleeve is monotonic in bass"
);
checks += 1;
check(sleeveScale({ bass: 0, pulse: 1 }), 1.015, "the kick adds its 1.5 % cap");
check(sleeveScale({ bass: 0, pulse: 5 }), 1.015, "the kick is capped above one");
check(
  sleeveScale({ bass: 0.62, pulse: 1 }).toFixed(4),
  "1.0522",
  "the kick is an accent on top of the breathing"
);
assert.ok(
  /^translateZ\(0\) translateY\(-?\d+\.\d{2}px\) scale\(\d\.\d{4}\)$/.test(
    visual.sleeve({ bass: 0.62, pulse: 0.8 })
  ) && !/e[+-]?\d/i.test(visual.sleeve({ bass: 0.62, pulse: 0.8 })),
  "the sleeve keeps one stable, exponent-free shape"
);
checks += 1;

/* The spectrum band: a visible floor at silence, the level carries half the
   length, and the transient above the running mean swings the rest. */
check(visual.band(0, 0), 0.05, "a silent band sits at the floor");
check(visual.band(1, 0), 1, "a full band reaches one");
check(visual.band(1, 1), 0.47, "a full band at its mean shows the level share plus the floor");
check(visual.band(2, 0), 1, "a band above one is clamped");
check(visual.band(-1, 0), 0.05, "a band below zero sits at the floor");
check(visual.band(undefined, undefined), 0.05, "a missing band sits at the floor");
check(visual.band(NaN, NaN), 0.05, "a NaN band sits at the floor");
approx(
  visual.band(0.62, 0.62),
  0.3104,
  0.0001,
  "a bar resting at its own mean shows the level, not a peak"
);
approx(visual.band(0.72, 0.6), 1, 0.0001, "a kick above the mean swings the bar to the end");
check(visual.band(0.62, 0.6).toFixed(2), "0.44", "the band writes clean two decimals");
assert.ok(
  visual.band(0.25, 0.3) < visual.band(0.4, 0.3) && visual.band(0.4, 0.3) < visual.band(0.5, 0.3),
  "the band is monotonic in the level"
);
assert.ok(
  visual.band(0.7, 0.7) < visual.band(0.7, 0.4),
  "the same level swings further when it stands above its mean"
);
checks += 1;

/* Title split: balanced, at most two lines, never truncated. */
var two = visual.splitTitle("Galvanize The Floor");
check(two.line1, "Galvanize", "the balanced split keeps the first word");
check(two.line2, "The Floor", "the balanced split keeps the rest");
var one = visual.splitTitle("  Galvanize  ");
check(one.line1, "Galvanize", "whitespace is collapsed");
check(one.line2, "", "a one-word title has no second line");
var empty = visual.splitTitle(null);
check(empty.line1, "", "a missing title is empty");
check(empty.line2, "", "a missing title has no second line");
var long = visual.splitTitle("Something extremely long that keeps going");
check(long.line1 + " " + long.line2, "Something extremely long that keeps going", "no words drop");
check(
  visual.splitTitle("Galvanize the floor").line1,
  "Galvanize",
  "case does not change the split"
);

/* The album line: one voice, so a redundant album never prints twice. */
check(
  visual.albumLabel("Galvanize The Floor", "Push the Button"),
  "Push the Button",
  "a distinct album is kept"
);
check(
  visual.albumLabel("Push the Button", "Push the Button"),
  "",
  "an album equal to the title is dropped"
);
check(visual.albumLabel("Push The Button", "push the button"), "", "the comparison ignores case");
check(
  visual.albumLabel("Galvanize The Floor", "Galvanize"),
  "",
  "an album inside the title is dropped"
);
check(
  visual.albumLabel("Galvanize", "Floor"),
  "Floor",
  "the album is not dropped by the reverse match"
);
check(
  visual.albumLabel("Galvanize The Floor", " The   Floor "),
  "",
  "whitespace is normalised before the match"
);
check(visual.albumLabel("Galvanize", ""), "", "no album means no line");
check(visual.albumLabel("", "Push the Button"), "Push the Button", "no title keeps the album");

/* Ledger: one line, remaining time always; track numbers only when known. */
var ledger = visual.ledger(232000, 2, 11);
check(ledger.meta, "Track 2 of 11", "track number and count are rendered");
check(ledger.remaining, "3:52 left", "remaining time is rendered");
check(visual.ledger(232000).meta, "", "no track number means no prefix");
check(visual.ledger(232000).remaining, "3:52 left", "remaining time survives a missing count");
check(visual.ledger(232000, 0, 0).meta, "", "zero track numbers are ignored");
check(visual.ledger(0).remaining, "0:00 left", "zero remaining is well formed");
check(visual.formatTime(3661000), "1:01:01", "hour-long tracks keep the hours");
check(visual.ledger(232000, 2.7, 11.9).meta, "Track 2 of 11", "fractional track numbers floor");

/* Palettes: three fixed fallbacks, no invented theme. */
check(visual.palette("night"), "night", "night is a palette");
check(visual.palette("mono"), "mono", "mono is a palette");
check(visual.palette("leonida"), "leonida", "leonida is a palette");
check(visual.palette("aurora"), "leonida", "an unknown palette falls back to leonida");
check(visual.palette(null), "leonida", "a missing palette falls back to leonida");

/* Cover colour: a dominant vote that survives a busy image, and a null for grey. */
var cover = [];
var index;
for (index = 0; index < 400; index += 1) {
  cover.push(24, 30, 40, 255);
}
for (index = 0; index < 100; index += 1) {
  cover.push(230, 60, 40, 255);
}
var dominant = visual.dominantColor(cover, 1);
assert.ok(dominant !== null, "a saturated cover has a dominant colour");
assert.ok(dominant.r > 200 && dominant.g < 100 && dominant.b < 120, "the red minority wins by hue");
approx(visual.meanLuma(cover, 1), 0.177, 0.005, "mean luma is the gamma-encoded average");

var grey = [];
for (index = 0; index < 200; index += 1) {
  grey.push(120, 120, 120, 255);
}
check(visual.dominantColor(grey, 1), null, "a greyscale cover has no dominant hue");
check(visual.dominantColor([]), null, "an empty sample has no dominant colour");
approx(visual.meanLuma([255, 255, 255, 255], 1), 1, 0.001, "white is full luma");
approx(visual.meanLuma([0, 0, 0, 255], 1), 0, 0.001, "black is zero luma");
check(visual.meanLuma([]), 0, "an empty sample is zero luma");

/* Tint: dark spine stops (for light type) and no focus accent — the ring is fixed. */
var tint = visual.tint({ r: 230, g: 60, b: 40 });
assert.ok(/^#[0-9a-f]{6}$/.test(tint.spineA), "the spine stop is a hex colour");
assert.ok(/^#[0-9a-f]{6}$/.test(tint.spineB), "the second spine stop is a hex colour");
var spineHsl = visual.rgbToHsl(
  parseInt(tint.spineA.slice(1, 3), 16),
  parseInt(tint.spineA.slice(3, 5), 16),
  parseInt(tint.spineA.slice(5, 7), 16)
);
assert.ok(spineHsl.l < 0.2, "the spine stays dark enough for light type");
check(tint.accent, undefined, "a cover contributes no focus colour");
check(visual.tint(null), null, "no colour means no tint");
check(visual.hslToHex(0, 1, 0.5), "#ff0000", "hsl converts back to hex");

approx(visual.rgbToHsl(255, 0, 0).h, 0, 0.001, "red is hue zero");
approx(visual.rgbToHsl(0, 255, 0).h, 120, 0.001, "green is hue 120");
approx(visual.rgbToHsl(0, 0, 255).h, 240, 0.001, "blue is hue 240");

console.log("visual helper: " + checks + " checks passed");
