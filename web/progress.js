/* Takt progress interpolation — ES5, no DOM, testable in Node. */
(function (root) {
  "use strict";

  function numberOr(value, fallback) {
    if (typeof value === "number" && isFinite(value) && value >= 0) {
      return value;
    }
    return fallback;
  }

  /* Zero is a speed: the server holds the position of a track at speed 0 while a clip plays. */
  function speedOr(value) {
    if (typeof value === "number" && isFinite(value) && value >= 0) {
      return value;
    }
    return 1000;
  }

  function normalize(status) {
    return {
      ms: numberOr(status.progress_ms, 0),
      duration: numberOr(status.duration_ms, 0),
      speed: speedOr(status.playback_speed),
      state: status.state || "idle",
      title: status.title || "",
      artist: status.artist || "",
      revision: numberOr(status.metadata_revision, 0),
    };
  }

  function sameTrack(previous, next) {
    return previous.title === next.title && previous.artist === next.artist;
  }

  /**
   * Builds the interpolation record for a status snapshot.
   *
   * The server resends an unchanged snapshot every second. A changed position, duration, track
   * identity, playback state, or metadata revision re-bases the position; unchanged snapshots keep
   * the local accumulation so the progress bar does not jump back. The metadata revision catches
   * seeks to the same numeric position (e.g. 0) and repeats, where `progress_ms` alone does not
   * change.
   */
  function update(previous, status, now) {
    var next = normalize(status);
    var hadPrevious = previous && previous.initialized === true;
    var rebase = !hadPrevious;

    if (hadPrevious) {
      if (
        previous.ms !== next.ms ||
        previous.duration !== next.duration ||
        previous.state !== next.state ||
        previous.revision !== next.revision ||
        !sameTrack(previous, next)
      ) {
        rebase = true;
      }
    }

    var record = {
      initialized: true,
      ms: next.ms,
      duration: next.duration,
      speed: next.speed,
      state: next.state,
      title: next.title,
      artist: next.artist,
      revision: next.revision,
      at: now,
    };

    if (hadPrevious) {
      if (rebase) {
        // The server supplied a new base position; use it as-is.
        record.ms = next.ms;
        record.at = now;
      } else if (previous.speed !== next.speed) {
        // Keep the locally accumulated position continuous across a rate change.
        record.ms = position(previous, now);
        record.at = now;
      } else {
        // Unchanged snapshot: retain the original base and timestamp.
        record.ms = previous.ms;
        record.at = previous.at;
      }
    }

    return record;
  }

  /** Returns the interpolated position in milliseconds, advancing only while playing. */
  function position(record, now) {
    if (!record || record.initialized !== true) {
      return 0;
    }
    var result = record.ms;
    if (record.state === "playing") {
      var elapsed = now - record.at;
      if (elapsed > 0) {
        result += (elapsed * record.speed) / 1000;
      }
    }
    if (record.duration > 0 && result > record.duration) {
      result = record.duration;
    }
    if (result < 0) {
      result = 0;
    }
    return result;
  }

  var api = {
    update: update,
    position: position,
  };

  if (typeof module === "object" && module.exports) {
    module.exports = api;
  } else {
    root.SendspinProgress = api;
  }
})(typeof window !== "undefined" ? window : this);
