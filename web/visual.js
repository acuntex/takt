/* Takt stage helpers — ES5, no DOM, testable in Node.
 *
 * Two kinds of maths live here, and neither touches the document:
 *  - the audio envelope and the idle timing that the UI polls;
 *  - the pure stage rules: title split, ledger text, sleeve transform, and the
 *    cover-colour sampling that falls back to a fixed palette when a cover
 *    cannot be read (CORS) at all.
 */
(function (root) {
  "use strict";

  var ATTACK = 0.85;
  var RELEASE = 0.3;
  var PULSE_DECAY = 0.55;
  var IDLE_PLAYING_MS = 30000;
  var IDLE_PAUSED_MS = 15000;

  /* The sleeve breathes 6 % at full bass, so a real track (bass ~0.62) grows the
   * card by ~3.7 % — visible at 3 m. The kick adds up to 1.5 % more on top and
   * the card rises 3 px at full bass: the beat is an accent on the breathing. */
  var SLEEVE_PEAK = 0.06;
  var SLEEVE_LIFT = 3;
  var PULSE_PEAK = 0.015;

  /* A silent spectrum bar keeps a visible sliver; a full band reaches 1. */
  /* The bars answer two things at once: the sustained level, which decides how
   * far they sit out, and the transient above a slow running mean, which is what
   * makes them hit. Without the second part a real track (bass 0.52-0.81) only
   * swings the bars by a fifth of their length, because the daemon's levels are
   * smoothed envelopes, not peaks.
   *
   * Measured over 20 s of real playback: base 0.5 and hit 4.0 turn that into a
   * 0.2-1.0 span at the same 20 Hz update, so the bars rest near two fifths and
   * jump on a kick. The mean follows fast on the way up and slowly on the way
   * down, so a sustained passage settles instead of staying pinned at full. */
  var BAND_FLOOR = 0.05;
  var BAND_BASE = 0.42;
  var BAND_HIT = 6.5;
  var BAND_MEAN_ATTACK = 0.2;
  var BAND_MEAN_RELEASE = 0.07;

  /* Hue buckets for the dominant-colour vote. Twelve is coarse enough to survive a
   * busy photograph and fine enough to separate a sunset from a forest. */
  var HUE_BUCKETS = 12;

  var PALETTES = ["leonida", "night", "mono"];

  function clamp(value) {
    if (typeof value !== "number" || !isFinite(value)) {
      return 0;
    }
    if (value < 0) {
      return 0;
    }
    if (value > 1) {
      return 1;
    }
    return value;
  }

  function create() {
    return {
      level: 0,
      bass: 0,
      mid: 0,
      treble: 0,
      pulse: 0,
      meanBass: 0,
      meanMid: 0,
      meanTreble: 0,
    };
  }

  function target(levels) {
    var source = levels || {};
    return {
      level: clamp(source.level),
      bass: clamp(source.bass),
      mid: clamp(source.mid),
      treble: clamp(source.treble),
    };
  }

  function approach(current, goal, attack, release) {
    var factor = goal > current ? attack : release;
    return current + (goal - current) * factor;
  }

  function update(state, goal, now) {
    var current = state || create();
    var wanted = target(goal);
    var rise = Math.max(0, wanted.bass - clamp(current.bass));
    var kick = clamp(rise * 5.5 + Math.max(0, wanted.level - clamp(current.level)) * 2.5);
    var level = approach(current.level, wanted.level, ATTACK, RELEASE);
    var bass = approach(current.bass, wanted.bass, ATTACK, RELEASE);
    var mid = approach(current.mid, wanted.mid, ATTACK, RELEASE);
    var treble = approach(current.treble, wanted.treble, ATTACK, RELEASE);
    return {
      level: level,
      bass: bass,
      mid: mid,
      treble: treble,
      pulse: Math.max(kick, clamp(current.pulse) * PULSE_DECAY),
      meanBass: followMean(current.meanBass, bass, now),
      meanMid: followMean(current.meanMid, mid, now),
      meanTreble: followMean(current.meanTreble, treble, now),
    };
  }

  /* The running mean the bars are measured against. `now` is not used for the
   * step, but it keeps the signature uniform with the other envelope helpers. */
  function followMean(current, value, now) {
    void now;
    var from = clamp(current);
    var to = clamp(value);
    return approach(from, to, BAND_MEAN_ATTACK, BAND_MEAN_RELEASE);
  }

  function shouldIdle(now, lastInput, playing, settingsOpen) {
    if (settingsOpen) {
      return false;
    }
    var timeout = playing ? IDLE_PLAYING_MS : IDLE_PAUSED_MS;
    return now - lastInput >= timeout;
  }

  /* ---------- stage rules ------------------------------------------------- */

  /**
   * The composited sleeve transform for a playing stage. One property, no
   * layout: the card grows with the bass, rises up to 3 px, and takes the
   * kick on top. The order and the precision are fixed, so equal input gives
   * the same string (the tick's write guard relies on it) and a missing level
   * can never print NaN or an exponent.
   */
  function sleeve(state) {
    var source = state || {};
    var bass = clamp(source.bass);
    var pulse = clamp(source.pulse);
    var scale = 1 + bass * SLEEVE_PEAK + pulse * PULSE_PEAK;
    var lift = Math.round(bass * SLEEVE_LIFT * 100) / 100;
    return (
      "translateZ(0) translateY(" +
      (lift === 0 ? "0.00" : "-" + lift.toFixed(2)) +
      "px) scale(" +
      scale.toFixed(4) +
      ")"
    );
  }

  /**
   * The scale for one spectrum bar: a 5 % floor so a silent band is a visible
   * sliver, linear to 1 at a full band. Clamped, rounded to 4 places so the
   * two-decimal write never sees a float tail.
   */
  /**
   * How far one bar sits out: the sustained level carries `BAND_BASE` of the
   * length, and how far it rises above its own running mean carries the rest, so
   * a kick swings the bar and a steady passage settles.
   */
  function band(value, mean) {
    var level = clamp(value);
    var avg = clamp(mean);
    var base = BAND_FLOOR + level * BAND_BASE;
    var hit = Math.max(0, level - avg) * BAND_HIT;
    return Math.round(clamp(base + hit) * 10000) / 10000;
  }

  /**
   * Splits a title into the two staggered lines the stage sets. The break is the one
   * with the smallest length difference, so short titles stay balanced ("Galvanize" /
   * "The Floor") and a one-word title keeps an empty second line. Never truncates.
   */
  function splitTitle(text) {
    var clean = cleanText(text);
    if (!clean) {
      return { line1: "", line2: "" };
    }
    var words = clean.split(" ");
    if (words.length === 1) {
      return { line1: words[0], line2: "" };
    }
    var best = 1;
    var bestDiff = Infinity;
    var index;
    for (index = 1; index < words.length; index += 1) {
      var left = words.slice(0, index).join(" ").length;
      var right = words.slice(index).join(" ").length;
      var diff = Math.abs(left - right);
      if (diff < bestDiff) {
        bestDiff = diff;
        best = index;
      }
    }
    return {
      line1: words.slice(0, best).join(" "),
      line2: words.slice(best).join(" "),
    };
  }

  function cleanText(value) {
    return String(value === null || value === undefined ? "" : value)
      .replace(/\s+/g, " ")
      .replace(/^\s+|\s+$/g, "");
  }

  /**
   * The album line, or "" when it would only repeat the title. Plenty of players
   * tag a single with its own name as the album; the screen must not print the
   * same words twice, and an album that is part of the title ("Galvanize" on
   * "Galvanize The Floor") is a fragment of the statement, not a second voice.
   */
  function albumLabel(title, album) {
    var albumText = cleanText(album);
    if (!albumText) {
      return "";
    }
    if (cleanText(title).toLowerCase().indexOf(albumText.toLowerCase()) !== -1) {
      return "";
    }
    return albumText;
  }

  function formatTime(milliseconds) {
    var total = Math.max(0, Math.floor((milliseconds || 0) / 1000));
    var hours = Math.floor(total / 3600);
    var minutes = Math.floor((total % 3600) / 60);
    var seconds = total % 60;
    var text = minutes + ":" + (seconds < 10 ? "0" : "") + seconds;
    if (hours > 0) {
      text = hours + ":" + (minutes < 10 ? "0" : "") + text;
    }
    return text;
  }

  /**
   * The rail's one metadata line. The protocol's metadata may carry a track number
   * and a track count; when either is absent the line keeps the remaining time only,
   * so a missing field can never print "TRACK undefined".
   */
  function ledger(remainingMs, track, count) {
    var meta = "";
    if (typeof track === "number" && isFinite(track) && track > 0) {
      meta = "Track " + Math.floor(track);
      if (typeof count === "number" && isFinite(count) && count > 0) {
        meta += " of " + Math.floor(count);
      }
    }
    return {
      meta: meta,
      remaining: formatTime(remainingMs) + " left",
    };
  }

  function palette(value) {
    var name = String(value === null || value === undefined ? "" : value).toLowerCase();
    var index;
    for (index = 0; index < PALETTES.length; index += 1) {
      if (name === PALETTES[index]) {
        return name;
      }
    }
    return "leonida";
  }

  /* ---------- cover colour ------------------------------------------------ */

  /* r,g,b are 0-255. Returns { h: 0-360, s: 0-1, l: 0-1 }. */
  function rgbToHsl(r, g, b) {
    var red = r / 255;
    var green = g / 255;
    var blue = b / 255;
    var max = Math.max(red, green, blue);
    var min = Math.min(red, green, blue);
    var lightness = (max + min) / 2;
    var delta = max - min;
    var hue = 0;
    var saturation = 0;
    if (delta > 0) {
      saturation = lightness > 0.5 ? delta / (2 - max - min) : delta / (max + min);
      if (max === red) {
        hue = ((green - blue) / delta + (green < blue ? 6 : 0)) * 60;
      } else if (max === green) {
        hue = ((blue - red) / delta + 2) * 60;
      } else {
        hue = ((red - green) / delta + 4) * 60;
      }
    }
    return { h: hue, s: saturation, l: lightness };
  }

  function channel(value) {
    var rounded = Math.round(value * 255);
    var hex = Math.max(0, Math.min(255, rounded)).toString(16);
    return hex.length === 1 ? "0" + hex : hex;
  }

  function hslToHex(hue, saturation, lightness) {
    var h = ((hue % 360) + 360) % 360;
    var s = clamp(saturation);
    var l = clamp(lightness);
    var c = (1 - Math.abs(2 * l - 1)) * s;
    var x = c * (1 - Math.abs(((h / 60) % 2) - 1));
    var m = l - c / 2;
    var red = 0;
    var green = 0;
    var blue = 0;
    if (h < 60) {
      red = c;
      green = x;
    } else if (h < 120) {
      red = x;
      green = c;
    } else if (h < 180) {
      green = c;
      blue = x;
    } else if (h < 240) {
      green = x;
      blue = c;
    } else if (h < 300) {
      red = x;
      blue = c;
    } else {
      red = c;
      blue = x;
    }
    return "#" + channel(red + m) + channel(green + m) + channel(blue + m);
  }

  /**
   * Votes for the dominant hue among the pixels of a decoded cover. Saturated,
   * mid-bright pixels count; near-black and near-white ones do not. Returns the
   * bucket's mean RGB, or null when the image has no usable colour.
   */
  function dominantColor(rgba, stride) {
    if (!rgba || typeof rgba.length !== "number" || rgba.length < 4) {
      return null;
    }
    var step = typeof stride === "number" && stride > 0 ? Math.floor(stride) : 4;
    var buckets = [];
    var index;
    for (index = 0; index < HUE_BUCKETS; index += 1) {
      buckets.push({ weight: 0, r: 0, g: 0, b: 0 });
    }
    var pixels = Math.floor(rgba.length / 4);
    var pixel;
    for (pixel = 0; pixel < pixels; pixel += step) {
      var offset = pixel * 4;
      var r = rgba[offset];
      var g = rgba[offset + 1];
      var b = rgba[offset + 2];
      var max = Math.max(r, g, b);
      var min = Math.min(r, g, b);
      if (max < 31 || max - min < 24) {
        continue;
      }
      var lightness = (max + min) / 510;
      var saturation = max === min ? 0 : (max - min) / max;
      if (saturation < 0.18 || lightness < 0.1 || lightness > 0.97) {
        continue;
      }
      var hue;
      var delta = max - min;
      if (max === r) {
        hue = ((g - b) / delta + (g < b ? 6 : 0)) * 60;
      } else if (max === g) {
        hue = ((b - r) / delta + 2) * 60;
      } else {
        hue = ((r - g) / delta + 4) * 60;
      }
      var bucket = buckets[Math.floor(hue / (360 / HUE_BUCKETS)) % HUE_BUCKETS];
      var weight = saturation * (0.35 + lightness);
      bucket.weight += weight;
      bucket.r += r * weight;
      bucket.g += g * weight;
      bucket.b += b * weight;
    }
    var winner = null;
    for (index = 0; index < buckets.length; index += 1) {
      if (buckets[index].weight > 0 && (!winner || buckets[index].weight > winner.weight)) {
        winner = buckets[index];
      }
    }
    if (!winner) {
      return null;
    }
    return {
      r: Math.round(winner.r / winner.weight),
      g: Math.round(winner.g / winner.weight),
      b: Math.round(winner.b / winner.weight),
    };
  }

  /** ITU-R 601 luma of the gamma-encoded samples, 0-1. Decides the bright-cover panel. */
  function meanLuma(rgba, stride) {
    if (!rgba || typeof rgba.length !== "number" || rgba.length < 4) {
      return 0;
    }
    var step = typeof stride === "number" && stride > 0 ? Math.floor(stride) : 4;
    var pixels = Math.floor(rgba.length / 4);
    var total = 0;
    var count = 0;
    var pixel;
    for (pixel = 0; pixel < pixels; pixel += step) {
      var offset = pixel * 4;
      total += (0.299 * rgba[offset] + 0.587 * rgba[offset + 1] + 0.114 * rgba[offset + 2]) / 255;
      count += 1;
    }
    return count > 0 ? total / count : 0;
  }

  /**
   * Derives the two tokens a cover contributes: the spine's stops, held at a
   * fixed dark lightness so the light type keeps its contrast. The focus ring
   * is fixed to --neon and the ground stays night, so nothing else is sampled.
   */
  function tint(rgb) {
    if (!rgb || typeof rgb.r !== "number") {
      return null;
    }
    var hsl = rgbToHsl(rgb.r, rgb.g, rgb.b);
    var spineSaturation = Math.max(0.22, Math.min(0.5, hsl.s));
    return {
      spineA: hslToHex(hsl.h, spineSaturation, 0.155),
      spineB: hslToHex(hsl.h + 10, spineSaturation, 0.175),
    };
  }

  var api = {
    create: create,
    target: target,
    update: update,
    shouldIdle: shouldIdle,
    sleeve: sleeve,
    band: band,
    splitTitle: splitTitle,
    albumLabel: albumLabel,
    formatTime: formatTime,
    ledger: ledger,
    palette: palette,
    rgbToHsl: rgbToHsl,
    hslToHex: hslToHex,
    dominantColor: dominantColor,
    meanLuma: meanLuma,
    tint: tint,
  };

  if (typeof module === "object" && module.exports) {
    module.exports = api;
  } else {
    root.SendspinVisual = api;
  }
})(typeof window !== "undefined" ? window : this);
