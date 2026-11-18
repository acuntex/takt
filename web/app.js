/* Takt TV UI — deliberately ES5 for Chromium 53 (webOS 4.x). */
(function () {
  "use strict";

  var STATE_LABELS = {
    idle: "Ready",
    connecting: "Connecting",
    playing: "Playing",
    paused: "Paused",
    error: "Offline",
    offline: "Offline",
  };

  /* The title ladder: design 158, never above it, never below 96. The frame is
     316 px (two 158 px lines) in the stylesheet and the pair is centred in it at
     any size, so the artist, the album and the rail never move. */
  var TITLE = {
    design: 158,
    ceil: 158,
    floor: 96,
    step: 2,
    frame: 316,
    columnRight: 1096,
    safeRight: 1824,
    /* The seam runs from x 742 at the top to 932 at the bottom. Line 2 is
       staggered onto the artwork on purpose; how far it may run past the seam is
       the one number that keeps a long second line from spreading to the far
       side of the cover ("Breath Away" reached 590 px past it). */
    seamTop: 742,
    seamRun: 190,
    crossMax: 260,
    /* The safe-area left edge, and the gap the second line keeps from the seam so
       its top corner never touches the rotated cover edge. */
    left: 96,
    seamGap: 8,
  };

  var VIEWPORT_WIDTH = 1920;
  var VIEWPORT_HEIGHT = 1080;

  var VOLUME_VISIBLE_MS = 1500;
  var SETTINGS_CLOSE_MS = 200;
  /* The credits page's fade and drift: the CSS animation and this timer are the
     same 200 ms, so the page is hidden exactly when its exit has landed. */
  var CREDITS_CLOSE_MS = 200;
  /* The success line stays on screen for a breath, then the panel returns the
     remote to the bar's Settings control. */
  var SAVED_CLOSE_MS = 1200;
  /* The TV audio delay is tuned by ear while music plays: every step is a
     10 ms write through /api/config, clamped to the range the row offers. */
  var DELAY_STEP_MS = 10;
  var DELAY_MAX_MS = 200;
  /* The track-change moment: the longest of its animations ends at 180 + 320 ms,
     so the class that carries them lives a little longer than that. A sleeve
     commit is normally the next frame after the metadata (the probe has already
     fetched the artwork); the deadline keeps a stalled decode from swallowing
     the moment altogether. */
  var TRACK_INTRO_MS = 520;
  var TRACK_INTRO_DEADLINE_MS = 600;
  /* The window in which the 20 Hz sleeve writes are eased into the breathe. */
  var SLEEVE_EASE_MS = 200;
  /* The tick that moves the playhead. The fill has no transition: it steps at
     this rate, under a pixel at any track length. */
  var PROGRESS_TICK_MS = 250;
  /* The bar is a slider: one d-pad press is a 10 s step, and a held direction
     keeps stepping on the browser's own key repeat (no timer of our own). The
     cream cursor answers the press at once; the fill waits for the daemon. A
     status within `SEEK_CONFIRM_MS` of the pending target is the seek landing;
     after `SEEK_PENDING_MS` without one the cursor returns to the fill, so a
     seek MA never took cannot leave the bar lying for good. */
  var SEEK_STEP_MS = 10000;
  var SEEK_CONFIRM_MS = 3000;
  var SEEK_PENDING_MS = 3000;
  /* At an edge the remaining distance shrinks below a step; a seek that small is
     not worth a command, so the bar pins the cursor at the clamp and sends
     nothing. */
  var SEEK_MIN_STEP_MS = 250;
  /* The bar's layout width and the cursor's own width, in stage pixels. The
     cursor is placed by transform, so app.js and the stylesheet must agree. */
  var SEEK_TRACK_WIDTH = 344;
  var SEEK_CURSOR_PX = 3;
  var IDLE_TRACK_WIDTH = 700;
  var COVER_PROBE_SIZE = 48;
  var BRIGHT_COVER_LUMA = 0.6;

  var progressMath = window.SendspinProgress;
  var visualMath = window.SendspinVisual;

  /* The sleeve at silence, written whenever playback stops so the card settles
     on the stylesheet's 240 ms transition instead of snapping. */
  var SLEEVE_REST = visualMath ? visualMath.sleeve(null) : "translateZ(0) scale(1.0000)";
  /* The band's resting scale value. Written once when the stage leaves the
     playing frame, so a hidden band never keeps a live value in the DOM. */
  var BAND_REST = visualMath ? visualMath.band(0).toFixed(2) : "0.05";

  var AUDIO_STALE_MS = 500;

  var current = null;
  var progress = null;
  var pin = null;
  /// Set while a command is retried once with a freshly read PIN.
  var retriedPin = false;
  var pinFromTv = false;
  /* The name that is on the daemon, and the server the installation wrote.
     The input is the draft; Save is the only thing that sends it. */
  var nameCommitted = "";
  var serverAddress = "";
  /* The delay is not a draft: `delayStatusValue` is the last value the daemon
     reported, `delayTarget` is what the row shows (optimistic while a step is
     in flight), and the queue keeps every press's write in order. */
  var delayStatusValue = 0;
  var delayTarget = 0;
  var delayQueue = [];
  var delayInFlight = false;
  var pinRevealed = false;
  var discardOpen = false;
  var savedCloseTimer = null;
  var eventSource = null;
  var pollTimer = null;
  var polling = false;
  var eventRetryTimer = null;
  var eventRetryDelay = 1000;
  var visualState = null;
  var visualTarget = null;
  var lastAudioAt = 0;
  var lastInputAt = Date.now();
  var lastMetadataKey = null;
  var lastTrackKey = null;
  var lastPlaybackState = null;
  /// The daemon says a seek this host sent is still waiting for its stream.
  var seeking = false;
  var lastCaption = "";
  /* The cursor's pending target `{ms, at, until}` — null when the fill and the
     cursor both show the daemon's position. */
  var seekPending = null;
  /* The bar owns the ring: the caption changes shape and the reconnect dim has
     a focused variant. */
  var barFocus = false;
  /* The last ramp key and cursor transform per bar, so a steady value costs no
     style write. */
  var fillRamp = { key: "" };
  var idleRamp = { key: "" };
  var fillCursor = { value: "" };
  var idleCursorState = { value: "" };
  var lastSleeve = "";
  /* The band's three written scales; the tick writes a bar only when its
     two-decimal value changes, so a steady band costs no style writes. */
  var lastBand = ["", "", ""];
  var lastVolume = null;
  var lastMuted = null;
  var volumeTimer = null;
  var idle = false;
  /* Nothing is loaded. The body starts in this state, so the first paint is the
     deliberate empty screen and never the drawn sleeve. */
  var empty = true;
  var bootFocus = true;
  var lastFocused = null;
  var lastRailFocus = null;
  var settingsCloseTimer = null;
  /* The surface also asks to close after a minute without input: long enough to read the
     panel, short enough not to leave it on screen overnight. */
  var SETTINGS_AUTO_CLOSE_MS = 60000;
  /* The theme is fixed: leonida is the one palette on the stage. The other two
     stop sets stay in the stylesheet as artwork fallbacks, but there is no user
     switch and nothing persists a choice any more. */
  var settingsOpen = false;
  var playbackStage = "ready";
  /* The credits page (the wordmark's Easter egg). `creditsClosing` keeps a key
     pressed during the 200 ms exit from acting on the stage underneath. */
  var creditsOpen = false;
  var creditsClosing = false;
  var creditsCloseTimer = null;
  var creditsReturnFocus = null;
  /* The last control the remote actually landed on, remembered across the
     credits page and idle, so the Easter egg can hand the ring back. */
  var lastStageFocus = null;
  var coverUrl = null;
  var coverToken = 0;
  var hasCover = false;
  var tinted = false;
  var lumHi = false;
  var showVolume = false;
  /* The track-change moment is armed by a track change and fired by the sleeve's
     own commit, so the type and the artwork land in one frame. */
  var trackIntro = false;
  var trackIntroPending = false;
  var trackIntroTimer = null;
  var trackIntroDeadline = null;
  var sleeveEase = false;
  var sleeveEaseTimer = null;

  var viewport = document.getElementById("viewport");
  var content = document.getElementById("content");
  var art = document.getElementById("art");
  var cover = document.getElementById("cover");
  var ghostCover = document.getElementById("ghost-cover");
  var stateWord = document.getElementById("state-word");
  var title = document.getElementById("title");
  var titleLine1 = document.getElementById("title-1");
  var titleLine2 = document.getElementById("title-2");
  var artist = document.getElementById("artist");
  var album = document.getElementById("album");
  var brand = document.getElementById("brand");
  var room = document.getElementById("room");
  var roomSep = document.getElementById("room-sep");
  var emptyDot = document.getElementById("empty-dot");
  var emptyName = document.getElementById("empty-name");
  var emptySep = document.getElementById("empty-sep");
  var emptyServer = document.getElementById("empty-server");
  var prev = document.getElementById("prev");
  var playPause = document.getElementById("play-pause");
  var next = document.getElementById("next");
  var progressTrack = document.getElementById("progress-track");
  var progressFill = document.getElementById("progress-fill");
  var progressCursor = document.getElementById("progress-cursor");
  var idleFill = document.getElementById("idle-fill");
  var idleCursor = document.getElementById("idle-cursor");
  var idleLeft = document.getElementById("idle-left");
  var lastIdleFill = "";
  var bandBass = document.getElementById("band-bass");
  var bandMid = document.getElementById("band-mid");
  var bandTreble = document.getElementById("band-treble");
  /* The elements exist by now; the write guards live with the start values. */
  var bandBars = [bandBass, bandMid, bandTreble];
  var ledgerTrack = document.getElementById("ledger-track");
  var ledgerSep = document.getElementById("ledger-sep");
  var ledgerLeft = document.getElementById("ledger-left");
  var volumeElement = document.getElementById("volume");
  var idleTitle = document.getElementById("idle-title");
  var idleArtist = document.getElementById("idle-artist");
  var idleText = document.querySelector(".idle-text");
  var idleState = document.getElementById("idle-state");
  var message = document.getElementById("message");
  var pinElement = document.getElementById("pin");
  var pinHint = document.getElementById("pin-hint");
  var pinInput = document.getElementById("pin-input");
  var pinReveal = document.getElementById("pin-reveal");
  var serverElement = document.getElementById("server");
  var serverDot = document.getElementById("server-dot");
  var serverWord = document.getElementById("server-word");
  var playerInput = document.getElementById("player-name");
  var playerValue = document.getElementById("player-name-value");
  var nameEdit = document.getElementById("name-edit");
  var delayValue = document.getElementById("output-delay-value");
  var delayEdit = document.getElementById("delay-edit");
  var delaySteps = document.getElementById("delay-steps");
  var delayDown = document.getElementById("delay-down");
  var delayUp = document.getElementById("delay-up");
  var saveButton = document.getElementById("save-config");
  var cancelButton = document.getElementById("cancel-config");
  var configMessage = document.getElementById("config-message");
  var discardRow = document.getElementById("discard-row");
  var discardYes = document.getElementById("discard-yes");
  var discardNo = document.getElementById("discard-no");
  var settingsDone = document.getElementById("settings-done");
  var settingsToggle = document.getElementById("settings-toggle");
  var settingsBackdrop = document.getElementById("settings-backdrop");
  var settingsPanel = document.getElementById("settings-panel");
  var settingsClose = document.getElementById("settings-close");
  var creditsOverlay = document.getElementById("credits");

  function on(element, eventName, handler) {
    if (!element) {
      return;
    }
    if (element.addEventListener) {
      element.addEventListener(eventName, handler, false);
    } else if (element.attachEvent) {
      element.attachEvent("on" + eventName, handler);
    }
  }

  function setText(element, text) {
    if (element) {
      element.textContent = text;
    }
  }

  function request(method, url, body, callback) {
    var xhr = new XMLHttpRequest();
    xhr.open(method, url, true);
    xhr.setRequestHeader("Accept", "application/json");
    if (body !== null) {
      xhr.setRequestHeader("Content-Type", "application/json");
    }
    xhr.onreadystatechange = function () {
      if (xhr.readyState !== 4) {
        return;
      }
      var parsed = null;
      if (xhr.responseText) {
        try {
          parsed = JSON.parse(xhr.responseText);
        } catch (error) {
          parsed = null;
        }
      }
      callback(xhr.status, parsed);
    };
    try {
      xhr.send(body === null ? null : JSON.stringify(body));
    } catch (error) {
      callback(0, null);
    }
  }

  function stageFor(state) {
    if (state === "playing" || state === "paused" || state === "connecting") {
      return state;
    }
    if (state === "error" || state === "offline") {
      return "offline";
    }
    return "ready";
  }

  /* Playing and paused are a live session: the transport works and a press does
     what its glyph says. Connecting, offline and a stage with no session are not
     live — the rail is dimmed and inert there. */
  function stageIsLive(stage) {
    return stage === "playing" || stage === "paused";
  }

  /* The empty screen's dot reports the link, not playback: neon while the daemon
     answers (playing, paused or idle), sand while connecting or offline. */
  function stageIsConnected(stage) {
    return stage !== "connecting" && stage !== "offline";
  }

  function syncBodyClasses() {
    var classes = ["state-" + playbackStage, "palette-leonida"];
    if (empty) {
      classes.push("is-empty");
    }
    if (idle) {
      classes.push("is-idle");
    }
    if (settingsOpen) {
      classes.push("settings-open");
    }
    if (creditsOpen) {
      classes.push("credits-open");
    }
    if (hasCover) {
      classes.push("has-cover");
    }
    if (tinted) {
      classes.push("tinted");
    }
    if (lumHi) {
      classes.push("lum-hi");
    }
    if (showVolume) {
      classes.push("show-volume");
    }
    if (barFocus) {
      classes.push("bar-focused");
    }
    if (trackIntro) {
      classes.push("is-track");
    }
    if (sleeveEase) {
      classes.push("is-sleeve-ease");
    }
    document.body.className = classes.join(" ");
  }

  /* ---------- title ---------------------------------------------------------
     The fit runs on a track change and once when Anton lands, never per frame.
     It writes font-size, line-height and the frame's top inset; the artist, the
     album and the rail keep their exact y. The halo is glyph-shaped CSS, so a
     crossing line needs no measurement at all. */

  function textBox(element) {
    if (!element || !element.firstChild) {
      return null;
    }
    var range = document.createRange();
    range.selectNodeContents(element);
    var rect = range.getBoundingClientRect();
    if (range.detach) {
      range.detach();
    }
    return rect;
  }

  function textRight(element) {
    var rect = textBox(element);
    return rect ? rect.right : 0;
  }

  function applyTitleSize(size) {
    title.style.fontSize = size + "px";
    title.style.lineHeight = size + "px";
  }

  function titleFits(size) {
    applyTitleSize(size);
    if (textRight(titleLine1) > TITLE.columnRight) {
      return false;
    }
    if (titleLine2 && titleLine2.firstChild && textRight(titleLine2) > TITLE.safeRight) {
      return false;
    }
    return true;
  }

  /* The canvas is scaled as a whole below 1080p, so a measured rect has to be
     divided by the same factor before it can be compared with a stage constant. */
  function canvasScale() {
    var width = document.documentElement.clientWidth || window.innerWidth || VIEWPORT_WIDTH;
    var height = document.documentElement.clientHeight || window.innerHeight || VIEWPORT_HEIGHT;
    var scale = Math.min(width / VIEWPORT_WIDTH, height / VIEWPORT_HEIGHT);
    return scale > 0 ? scale : 1;
  }

  /* Where the second line sits. The design stagger puts it on the artwork; what
     the eye expects, though, is that it stops at the cover. So the line is set
     to end on the seam whenever it fits in the ink column (that is the normal
     case, and it reads as one block), and only a line too wide for that keeps a
     bounded crossing: at most `crossMax` past the seam, never across the art.
     Measured at the line's own top edge, because the seam runs diagonally. */
  function staggerTitleLine() {
    if (!titleLine2 || !titleLine2.firstChild) {
      return;
    }
    titleLine2.style.marginLeft = "";
    var box = textBox(titleLine2);
    if (!box) {
      return;
    }
    var scale = canvasScale();
    var top = box.top / scale;
    var width = (box.right - box.left) / scale;
    var seam = TITLE.seamTop + (top * TITLE.seamRun) / VIEWPORT_HEIGHT;
    var room = seam - TITLE.seamGap - TITLE.left;
    var fits = width <= room;
    var margin;
    if (fits) {
      margin = room - width;
    } else {
      var base = parseFloat(window.getComputedStyle(titleLine2).marginLeft) || 0;
      margin = Math.min(base, seam + TITLE.crossMax - width - TITLE.left);
    }
    var target = fits ? seam - TITLE.seamGap : seam + TITLE.crossMax;

    /* The width measured above is the width before the line moved, and the first
       layout of a new track can settle a few pixels later (font metrics, a
       reflow). So the edge is placed, measured, and corrected until it lands:
       at most three passes, two pixels of tolerance. */
    var pass;
    for (pass = 0; pass < 3; pass += 1) {
      setStagger(margin);
      var after = textBox(titleLine2);
      if (!after) {
        return;
      }
      var residual = after.right / scale - target;
      if (Math.abs(residual) < 2) {
        return;
      }
      margin -= residual;
    }
  }

  function setStagger(margin) {
    titleLine2.style.marginLeft = Math.max(0, margin) + "px";
  }

  function fitTitle() {
    if (!title) {
      return;
    }
    var size = Math.min(TITLE.design, TITLE.ceil);
    while (size > TITLE.floor && !titleFits(size)) {
      size -= TITLE.step;
    }
    if (!titleFits(size)) {
      size = TITLE.floor;
    }
    applyTitleSize(size);
    staggerTitleLine();
    var lines = titleLine2 && titleLine2.firstChild ? 2 : 1;
    title.style.paddingTop = Math.max(0, (TITLE.frame - lines * size) / 2) + "px";
    document.body.setAttribute("data-title-size", String(size));
  }

  /* The canvas is 1920x1080. scale() makes a 1280x720 panel show the whole
     frame at two thirds; the origin is the top left corner. Only a resize
     writes this, never the 20 Hz tick. */
  function fitViewport() {
    if (!viewport) {
      return;
    }
    var width = document.documentElement.clientWidth || window.innerWidth || VIEWPORT_WIDTH;
    var height = document.documentElement.clientHeight || window.innerHeight || VIEWPORT_HEIGHT;
    var scale = Math.min(width / VIEWPORT_WIDTH, height / VIEWPORT_HEIGHT);
    scale = Math.round(scale * 10000) / 10000;
    // At exactly 1.0 a transform would promote the whole canvas into its own
    // compositing layer for nothing, which cost ~20 fps on the TV. Only scale
    // when the panel is actually smaller than 1080p.
    var value = scale === 1 ? "none" : "scale(" + scale + ")";
    viewport.style.webkitTransform = value;
    viewport.style.transform = value;
  }

  /* The idle line is one line of display type on the upper third. If the title
     and artist do not fit the 760 px ink column it steps down from 60 to 42 px
     in 2 px steps; the CSS ellipsis is the last resort, so a cut is always
     marked. It runs on a track change and when Anton lands, never per frame. */
  function fitIdleLine() {
    if (!idleText) {
      return;
    }
    var size = 60;
    idleText.style.fontSize = size + "px";
    while (size > 42 && idleText.scrollWidth > idleText.clientWidth) {
      size -= 2;
      idleText.style.fontSize = size + "px";
    }
    idleText.setAttribute("data-size", String(size));
  }

  /* ---------- cover and colour --------------------------------------------- */

  function setTint(tint) {
    /* Only the spine follows the cover hue. The focus ring stays on --neon and
       the ground stays night, whatever the artwork does. */
    if (tint) {
      tinted = true;
      document.body.style.setProperty("--spine-a", tint.spineA);
      document.body.style.setProperty("--spine-b", tint.spineB);
    } else {
      tinted = false;
      document.body.style.removeProperty("--spine-a");
      document.body.style.removeProperty("--spine-b");
    }
  }

  /**
   * Samples a decoded image at 48x48. Returns null when the canvas is tainted
   * (a host without a CORS header), which is the documented fallback.
   */
  function sampleCover(image) {
    try {
      var canvas = document.createElement("canvas");
      canvas.width = COVER_PROBE_SIZE;
      canvas.height = COVER_PROBE_SIZE;
      var context = canvas.getContext("2d");
      if (!context) {
        return null;
      }
      context.drawImage(image, 0, 0, COVER_PROBE_SIZE, COVER_PROBE_SIZE);
      return context.getImageData(0, 0, COVER_PROBE_SIZE, COVER_PROBE_SIZE);
    } catch (error) {
      return null;
    }
  }

  /**
   * One image, one decision, one frame. The sleeve, the spine tokens, lum-hi
   * and has-cover are written in a single block, so the scene never swaps
   * before the artwork is there and the ghost is filtered once per track.
   */
  function commitCover(url, tint, bright) {
    if (ghostCover) {
      ghostCover.src = url;
    }
    hasCover = true;
    setTint(tint);
    lumHi = bright;
    syncBodyClasses();
    /* The sleeve is on screen in this frame; the type enters with it. */
    if (trackIntroPending) {
      playTrackIntro();
    }
  }

  /**
   * Back to the drawn sleeve: no committed artwork, no sampled hue, no ghost.
   * The url is kept, so the same track does not try the image again.
   */
  function dropCover() {
    hasCover = false;
    setTint(null);
    lumHi = false;
    cover.removeAttribute("src");
    if (ghostCover) {
      ghostCover.removeAttribute("src");
    }
    syncBodyClasses();
    /* A sleeve that failed to decode still ends a track change: the moment runs
       on the drawn sleeve rather than never. */
    if (trackIntroPending) {
      playTrackIntro();
    }
  }

  function clearCover() {
    coverUrl = null;
    coverToken += 1;
    dropCover();
  }

  /**
   * Decodes the next sleeve off-screen while the old one stays painted. The
   * colour sample rides on the probe, and the visible <img> is the commit
   * point: its onload fires with the decoded bitmap in hand, so has-cover, the
   * spine stops and lum-hi are written in the frame that paints the artwork.
   */
  function loadCover(url, token) {
    if (!window.Image) {
      cover.src = url;
      commitCover(url, null, false);
      return false;
    }
    var probe = new window.Image();
    probe.crossOrigin = "anonymous";
    probe.onload = function () {
      if (token !== coverToken) {
        return;
      }
      var pixels = sampleCover(probe);
      var tint = null;
      var bright = false;
      if (pixels && visualMath) {
        var dominant = visualMath.dominantColor(pixels.data);
        tint = dominant ? visualMath.tint(dominant) : null;
        bright = visualMath.meanLuma(pixels.data) > BRIGHT_COVER_LUMA;
      }
      swapCover(url, token, tint, bright);
    };
    probe.onerror = function () {
      if (token !== coverToken) {
        return;
      }
      /* No CORS header: the artwork still shows, the palette keeps its stops. */
      swapCover(url, token, null, false);
    };
    probe.src = url;
    return true;
  }

  function swapCover(url, token, tint, bright) {
    cover.onload = function () {
      if (token !== coverToken) {
        return;
      }
      commitCover(url, tint, bright);
    };
    cover.onerror = function () {
      if (token === coverToken) {
        dropCover();
      }
    };
    cover.src = url;
  }

  function setCover(url) {
    if (!url) {
      if (coverUrl === null && !hasCover) {
        return false;
      }
      clearCover();
      return false;
    }
    if (url === coverUrl) {
      return false;
    }
    coverUrl = url;
    coverToken += 1;
    return loadCover(url, coverToken);
  }

  /* ---------- the track-change moment --------------------------------------
     The headline of the stage, exactly once per track. `setTrack` arms it when
     the track identity changed; the commit that paints the new sleeve fires it,
     so the card's settle and the type's rise are one moment and not two. A
     metadata revision, a reconnect and a seek never arm it, and idle skips it:
     the type block is gone there and the card is orbiting (burn-in rule). */

  /* Assigning a class the body already carries does not restart an animation, so
     a skip inside the previous window has to commit the unanimated state first.
     One forced layout, only on a skip, never on the 20 Hz tick. */
  function forceReflow(element) {
    return element ? element.offsetWidth : 0;
  }

  function playTrackIntro() {
    trackIntroPending = false;
    if (trackIntroDeadline !== null) {
      window.clearTimeout(trackIntroDeadline);
      trackIntroDeadline = null;
    }
    if (idle) {
      return;
    }
    if (trackIntro) {
      trackIntro = false;
      syncBodyClasses();
      forceReflow(content);
    }
    trackIntro = true;
    syncBodyClasses();
    if (trackIntroTimer !== null) {
      window.clearTimeout(trackIntroTimer);
    }
    trackIntroTimer = window.setTimeout(function () {
      trackIntroTimer = null;
      trackIntro = false;
      syncBodyClasses();
    }, TRACK_INTRO_MS);
  }

  /* `loading` is true while a sleeve decode owns the commit point. When nothing
     is in flight (no artwork, or the same artwork as the previous track) the
     moment runs in the metadata frame instead of waiting for a commit that will
     not come. */
  function armTrackIntro(loading) {
    if (trackIntroPending) {
      return;
    }
    trackIntroPending = true;
    if (!loading) {
      playTrackIntro();
      return;
    }
    trackIntroDeadline = window.setTimeout(function () {
      trackIntroDeadline = null;
      if (trackIntroPending) {
        playTrackIntro();
      }
    }, TRACK_INTRO_DEADLINE_MS);
  }

  /* ---------- metadata ------------------------------------------------------ */

  function setTrack(status, changed) {
    var lines = visualMath
      ? visualMath.splitTitle(status.title || "")
      : { line1: status.title || "", line2: "" };
    setText(titleLine1, lines.line1);
    setText(titleLine2, lines.line2);
    fitTitle();

    setText(artist, status.artist || "");
    /* One metadata voice: the album line disappears when it would only repeat
       the title (or a fragment of it). */
    var albumText = visualMath
      ? visualMath.albumLabel(status.title, status.album)
      : status.album || "";
    setText(album, albumText);
    if (albumText) {
      album.removeAttribute("hidden");
    } else {
      album.setAttribute("hidden", "hidden");
    }

    setText(idleTitle, status.title || "");
    setText(idleArtist, status.artist || "");
    fitIdleLine();

    var decoding = setCover(status.artwork_url || null);
    /* A new track starts at its own zero: a pending target from the last one
       must not survive the change. */
    if (changed) {
      seekPending = null;
    }
    lastCaption = "";
    rebasePlayhead();
    /* Armed after the cover, so a track with no artwork (or the same artwork as
       the last one) fires the moment in this frame instead of waiting for a
       decode that will never arrive. */
    if (changed) {
      armTrackIntro(decoding);
    }
  }

  /* ---------- playhead ------------------------------------------------------- */

  function durationMs() {
    return progress && progress.duration > 0 ? progress.duration : 0;
  }

  /* The fill is the daemon's position and nothing else: the interpolated
     snapshot, rebased by every status that reports a new one. */
  function confirmedPosition(now) {
    var position = progressMath ? progressMath.position(progress, now) : 0;
    var duration = durationMs();
    if (duration > 0 && position > duration) {
      position = duration;
    }
    return position > 0 ? position : 0;
  }

  /* The cursor is the remote's position. A press moves it to the target in that
     frame and it advances at playback speed while it waits; a confirming status
     (or the end of the pending window, whichever comes first) hands it back to
     the fill. A refused seek therefore looks like the cursor returning, never
     like the track snapping. */
  function cursorPosition(now) {
    if (seekPending !== null && now >= seekPending.until) {
      seekPending = null;
    }
    if (seekPending === null) {
      return confirmedPosition(now);
    }
    var position = seekPending.ms;
    if (current && current.state === "playing") {
      var speed = progress && progress.speed ? progress.speed : 1000;
      position += ((now - seekPending.at) * speed) / 1000;
    }
    var duration = durationMs();
    if (duration > 0 && position > duration) {
      position = duration;
    }
    return position > 0 ? position : 0;
  }

  /* One guarded write per value: the tick runs four times a second and the two
     aria strings change only once a second. */
  function writeAttribute(element, name, value) {
    if (element.getAttribute(name) !== value) {
      element.setAttribute(name, value);
    }
  }

  /* The bar reads in seconds, not percent, so the number the remote steps by is
     the number a reader hears; the value text is the caption's own wording. The
     focused bar of a session that is not live says what the screen says. */
  function writeBarAria(position, duration) {
    var max = duration > 0 ? Math.floor(duration / 1000) : 0;
    var now = position > 0 ? Math.round(position / 1000) : 0;
    if (max > 0 && now > max) {
      now = max;
    }
    var text = "Live";
    if (barFocus && !stageIsLive(playbackStage)) {
      text = "Reconnecting";
    } else if (duration > 0 && visualMath) {
      text = visualMath.formatTime(Math.max(0, duration - position)) + " left";
    }
    writeAttribute(progressTrack, "aria-valuemin", "0");
    writeAttribute(progressTrack, "aria-valuemax", String(max));
    writeAttribute(progressTrack, "aria-valuenow", String(now));
    writeAttribute(progressTrack, "aria-valuetext", text);
  }

  /* Indigo `#415fce` -> lavender `#828ec8`, interpolated over the whole track.
     The fill is a full-width gradient scaled by `scaleX`, so its end stop is
     given the colour of the position it has reached: after scaling, every
     played second keeps the colour the full-track ramp gives it. */
  function rampStop(ratio) {
    var t = ratio < 0 ? 0 : ratio > 1 ? 1 : ratio;
    return (
      "rgb(" +
      Math.round(65 + 65 * t) +
      ", " +
      Math.round(95 + 47 * t) +
      ", " +
      Math.round(206 - 6 * t) +
      ")"
    );
  }

  function writeRamp(element, ratio, last) {
    if (!element) {
      return;
    }
    var key = ratio.toFixed(3);
    if (key === last.key) {
      return;
    }
    last.key = key;
    element.style.backgroundImage =
      "linear-gradient(90deg, rgb(65, 95, 206) 0%, " + rampStop(ratio) + " 100%)";
  }

  /* The 3 px cursor sits on the fill's leading edge, its own width inside the
     track at both ends: no overhang, no shadow. Transform only. */
  function writeCursor(element, ratio, width, last) {
    if (!element) {
      return;
    }
    var x = ratio * width - SEEK_CURSOR_PX;
    if (x < 0) {
      x = 0;
    } else if (x > width - SEEK_CURSOR_PX) {
      x = width - SEEK_CURSOR_PX;
    }
    var value = "translateX(" + (Math.round(x * 10) / 10).toFixed(1) + "px)";
    if (value === last.value) {
      return;
    }
    last.value = value;
    element.style.transform = value;
  }

  function writePlayhead(position, cursor, duration) {
    if (!progressFill) {
      return;
    }
    var ratio = duration > 0 ? Math.min(1, position / duration) : 0;
    var cursorRatio = duration > 0 ? Math.min(1, cursor / duration) : 0;
    progressFill.style.transform = "scaleX(" + ratio.toFixed(4) + ")";
    writeRamp(progressFill, ratio, fillRamp);
    writeCursor(progressCursor, cursorRatio, SEEK_TRACK_WIDTH, fillCursor);
    writeBarAria(cursor, duration);
    /* The idle screen carries the same position on its own thin line, with its
       own 3 px cursor. The fill is written only when its rounded value moves. */
    if (idleFill) {
      var value = ratio.toFixed(3);
      if (value !== lastIdleFill) {
        idleFill.style.transform = "scaleX(" + value + ")";
        lastIdleFill = value;
      }
      writeRamp(idleFill, ratio, idleRamp);
      writeCursor(idleCursor, cursorRatio, IDLE_TRACK_WIDTH, idleCursorState);
    }
  }

  /**
   * A metadata revision is not a track change: a tag edit, new artwork or a
   * seek arrives while the playhead keeps its place. The fill lands on the
   * daemon's position in one frame, with no transition to sweep there.
   */
  function rebasePlayhead() {
    var now = Date.now();
    writePlayhead(confirmedPosition(now), cursorPosition(now), durationMs());
  }

  /* ---------- the bar as a d-pad stop --------------------------------------- */

  /* The stop exists only when a press can work: a loaded track with a known
     duration, on a live session. Connecting, offline, the empty screen and a
     stream of unknown length keep the bar inert, exactly as it is today. */
  function barIsStop() {
    return !empty && stageIsLive(playbackStage) && durationMs() > 0;
  }

  function syncBarStop() {
    var stop = barIsStop();
    if (stop) {
      writeAttribute(progressTrack, "data-focusable", "");
      writeAttribute(progressTrack, "tabindex", "0");
      return;
    }
    var had = progressTrack.hasAttribute("data-focusable");
    progressTrack.removeAttribute("data-focusable");
    writeAttribute(progressTrack, "tabindex", "-1");
    /* A reconnect is not a reason to move the ring: the bar keeps the focus and
       dims, and Left/Right go inert. A track or a duration that disappears is
       different — that control cannot be pressed at all, so the focus leaves. */
    if (had && document.activeElement === progressTrack && (empty || durationMs() <= 0)) {
      releaseRailFocus(true);
    }
  }

  /* One d-pad step. The cursor moves to the target in this frame and the fill
     stays on the last confirmed position; only the clamped offset travels, so a
     step at either edge asks for the remainder instead of overshooting. Nothing
     is sent when the step cannot move or when the PIN is missing. */
  function seekStep(delta) {
    if (!barIsStop()) {
      return false;
    }
    var duration = durationMs();
    var now = Date.now();
    var base = Math.round(cursorPosition(now));
    var target = base + delta;
    if (target < 0) {
      target = 0;
    } else if (target > duration) {
      target = duration;
    }
    if (target === base) {
      return false;
    }
    var offset = target - base;
    if (Math.abs(offset) < SEEK_MIN_STEP_MS) {
      /* The clamp is already the whole step: pin the cursor at the edge and put
         nothing on the wire for a seek the daemon would not notice. */
      seekPending = { ms: target, at: now, until: now + SEEK_PENDING_MS };
      tick();
      return false;
    }
    if (usablePin() === null) {
      /* sendCommand opens Settings for the PIN; a cursor we cannot confirm
         would leave the bar lying for its whole window. */
      sendCommand("seek_relative", { offset_ms: offset });
      return false;
    }
    seekPending = { ms: target, at: now, until: now + SEEK_PENDING_MS };
    tick();
    sendCommand("seek_relative", { offset_ms: offset });
    return true;
  }

  function updateVolume(status) {
    var volume = typeof status.volume === "number" ? status.volume : null;
    var muted = status.muted === true;
    if (volume === lastVolume && muted === lastMuted) {
      return;
    }
    var first = lastVolume === null && lastMuted === null;
    lastVolume = volume;
    lastMuted = muted;
    if (first) {
      return;
    }
    if (muted) {
      showVolumeNumber("MUTE");
    } else if (volume !== null) {
      showVolumeNumber(String(volume));
    }
  }

  function showVolumeNumber(text) {
    setText(volumeElement, text);
    showVolume = true;
    syncBodyClasses();
    if (volumeTimer !== null) {
      window.clearTimeout(volumeTimer);
    }
    volumeTimer = window.setTimeout(function () {
      volumeTimer = null;
      showVolume = false;
      syncBodyClasses();
    }, VOLUME_VISIBLE_MS);
  }

  /* ---------- identity ------------------------------------------------------ */

  /* The bar reads `Takt · <room>`. The default player name is the product name;
     printing it would put the wordmark on screen twice, so it is dropped and the
     bar reads `Takt` alone. The same rule feeds the empty screen's identity line. */
  function cleanName(name) {
    var value = String(name === null || name === undefined ? "" : name)
      .replace(/\s+/g, " ")
      .replace(/^\s+|\s+$/g, "");
    return value.toLowerCase() === "takt" ? "" : value;
  }

  function setRoom(name) {
    var value = cleanName(name);
    setText(room, value);
    if (roomSep) {
      if (value) {
        roomSep.removeAttribute("hidden");
      } else {
        roomSep.setAttribute("hidden", "hidden");
      }
    }
  }

  /* ---------- status -------------------------------------------------------- */

  /* The transport and the state block are written from one stage, so each glyph
     has exactly one meaning: pause bars while playing, a play triangle only in a
     live paused session, and nothing while the rail is inert. */
  function setTransport(stage) {
    if (stage === "playing") {
      playPause.className = "btn t-play is-playing";
      playPause.setAttribute("aria-label", "Pause");
    } else if (stage === "paused") {
      playPause.className = "btn t-play";
      playPause.setAttribute("aria-label", "Play");
    } else {
      playPause.className = "btn t-play is-inert";
      playPause.setAttribute("aria-label", "Unavailable");
    }
    setRailLive(stageIsLive(stage));
    if (!stageIsLive(stage)) {
      releaseRailFocus();
    }
  }

  function setRailLive(live) {
    var controls = [prev, playPause, next];
    var index;
    for (index = 0; index < controls.length; index += 1) {
      if (!controls[index]) {
        continue;
      }
      if (live) {
        controls[index].removeAttribute("aria-disabled");
      } else {
        controls[index].setAttribute("aria-disabled", "true");
      }
    }
  }

  /* A dropped connection can make the rail inert. The transport hands the focus
     to the bar's Settings control; the seek bar keeps it and dims instead,
     because a reconnect must not move the ring under the user's thumb. A bar
     that stopped being a control altogether is released with `force`. A
     reconnected session does not steal the focus back. */
  function releaseRailFocus(force) {
    var active = document.activeElement;
    if (!isRailControl(active) || !settingsToggle) {
      return;
    }
    if (active === progressTrack && force !== true) {
      return;
    }
    settingsToggle.focus();
  }

  function applyStatus(status) {
    current = status;
    progress = progressMath ? progressMath.update(progress, status, Date.now()) : null;

    var metadataKey =
      String(status.metadata_revision) +
      "\n" +
      (status.title || "") +
      "\n" +
      (status.artist || "") +
      "\n" +
      (status.album || "") +
      "\n" +
      (status.artwork_url || "");
    /* The server's own position, never the locally interpolated one: a snapshot
       that trails the interpolation by a few hundred ms is a poll, not a seek. */
    var reported = typeof status.progress_ms === "number" ? status.progress_ms : null;
    /* Metadata, artwork and a revision bump are not input. Waking here turned
       the burn-in protection off for 30 s of every automatic track change; the
       stage now idles through normal playback and comes back on a real key or
       pointer event only. */
    if (metadataKey !== lastMetadataKey) {
      var trackKey = (status.title || "") + "\n" + (status.artist || "");
      var trackChanged = trackKey !== lastTrackKey;
      lastTrackKey = trackKey;
      lastMetadataKey = metadataKey;
      setTrack(status, trackChanged);
    }
    /* The pending cursor is settled after the re-base: a status within the
       window of the target is the seek landing, and the fill meets the cursor in
       this frame. A status that raced the command is absorbed by the same
       window; once it closes without a match the daemon did not take the seek,
       so the cursor returns to the fill rather than the track snapping. */
    if (seekPending !== null) {
      var seenAt = Date.now();
      var predicted = cursorPosition(seenAt);
      var landed = reported !== null && Math.abs(reported - predicted) <= SEEK_CONFIRM_MS;
      if (landed || seenAt >= seekPending.until) {
        seekPending = null;
      }
    }
    if (lastPlaybackState !== null && status.state !== lastPlaybackState) {
      wake();
      /* Play eases the sleeve up out of rest for 200 ms; pause settles on the
         stylesheet's 240 ms as soon as the stage leaves playing. Nothing here
         runs on a poll that repeats the same state. */
      if (status.state === "playing" && !idle) {
        easeSleeveIn();
      }
    }
    lastPlaybackState = status.state;

    playbackStage = stageFor(status.state);
    /* The server answers a seek by restarting the stream, and a cold buffer makes
       that take seconds. The daemon knows a seek is in flight, so the stage says
       so instead of claiming "Paused" over a stream that is about to return. */
    seeking = status.seeking === true && status.state !== "error";
    var label = seeking ? "Seeking" : STATE_LABELS[status.state] || STATE_LABELS.idle;
    setText(stateWord, label);
    setText(idleState, label);
    setTransport(playbackStage);

    updateVolume(status);
    setRoom(status.player_name);
    setEmptyIdentity(status.player_name, status.server);
    setEmpty(!(status.title || status.artist));
    /* The bar is a stop only while a press can work: written after the empty
       flag, so a player with nothing loaded drops the stop in the same frame. */
    syncBarStop();
    if (emptyDot) {
      emptyDot.className = stageIsConnected(playbackStage) ? "empty-dot is-live" : "empty-dot";
    }

    serverAddress = trimmed(status.server);
    nameCommitted = String(status.player_name || "");
    delayStatusValue = reportedDelay(status.output_delay_ms);
    /* While a step is in flight its optimistic number stays; every other poll
       puts the value in force back on the row. */
    if (delayQueue.length === 0 && !delayInFlight) {
      delayTarget = delayStatusValue;
      renderDelay();
    }
    /* A live poll never overwrites a draft or the field the remote is typing in;
       everything else follows the daemon's committed name. */
    if (!isDirty() && document.activeElement !== playerInput) {
      if (playerInput) {
        playerInput.value = nameCommitted;
      }
      if (playerValue && playerInput && playerInput.hidden) {
        setText(playerValue, nameCommitted);
      }
    }
    renderServer();

    syncBodyClasses();
    settleBootFocus();
    tick();
  }

  function tick() {
    if (!progressMath) {
      return;
    }
    var now = Date.now();
    var duration = durationMs();
    var position = confirmedPosition(now);
    var cursor = cursorPosition(now);
    writePlayhead(position, cursor, duration);

    /* One caption line, two shapes: focused shows the elapsed time at the left
       end and the remaining time under the bar's right end; blurred returns the
       single "x left" at x 496. The key includes the focus, the stage and the
       second, so a blur, a reconnect and every second update it — and a steady
       second costs nothing. */
    var remaining = duration > 0 ? Math.max(0, duration - cursor) : 0;
    var second = Math.floor(remaining / 1000);
    var key = (barFocus ? "1" : "0") + "|" + playbackStage + "|" + second;
    if (key === lastCaption) {
      return;
    }
    lastCaption = key;
    var lines =
      duration > 0 && visualMath
        ? visualMath.ledger(remaining, current && current.track, current && current.track_count)
        : { meta: "", remaining: "Live" };
    if (barFocus) {
      setText(
        ledgerTrack,
        duration > 0 && visualMath && stageIsLive(playbackStage)
          ? visualMath.formatTime(cursor)
          : ""
      );
      setText(ledgerLeft, stageIsLive(playbackStage) ? lines.remaining : "Reconnecting");
      if (ledgerSep) {
        ledgerSep.className = "is-hidden";
      }
    } else {
      setText(ledgerTrack, lines.meta);
      setText(ledgerLeft, lines.remaining);
      if (ledgerSep) {
        ledgerSep.className = lines.meta ? "" : "is-hidden";
      }
    }
    setText(idleLeft, lines.remaining);
  }

  function loadStatus() {
    request("GET", "/api/status", null, function (status, body) {
      if (status === 200 && body) {
        applyStatus(body);
      }
    });
  }

  function startPolling() {
    if (polling) {
      return;
    }
    polling = true;
    loadStatus();
    pollTimer = window.setInterval(loadStatus, 1000);
  }

  function stopPolling() {
    if (!polling) {
      return;
    }
    polling = false;
    if (pollTimer !== null) {
      window.clearInterval(pollTimer);
      pollTimer = null;
    }
  }

  // An event stream that dropped (daemon restart, deploy, sleep) must come back: polling
  // carries status only, so audio levels would stay silent for the rest of the session.
  function scheduleEventRetry() {
    if (eventRetryTimer !== null) {
      return;
    }
    eventRetryTimer = window.setTimeout(function () {
      eventRetryTimer = null;
      startEvents();
    }, eventRetryDelay);
    eventRetryDelay = Math.min(eventRetryDelay * 2, 30000);
  }

  function startEvents() {
    if (typeof window.EventSource !== "function") {
      startPolling();
      return;
    }
    try {
      eventSource = new window.EventSource("/events");
    } catch (error) {
      startPolling();
      scheduleEventRetry();
      return;
    }
    eventSource.onopen = function () {
      eventRetryDelay = 1000;
      stopPolling();
    };
    eventSource.onmessage = function (event) {
      var status = null;
      try {
        status = JSON.parse(event.data);
      } catch (error) {
        status = null;
      }
      if (status) {
        applyStatus(status);
      }
    };
    if (eventSource.addEventListener) {
      eventSource.addEventListener(
        "audio",
        function (event) {
          var data = null;
          try {
            data = JSON.parse(event.data);
          } catch (error) {
            data = null;
          }
          if (data) {
            applyAudio(data);
          }
        },
        false
      );
    }
    eventSource.onerror = function () {
      if (eventSource) {
        eventSource.close();
        eventSource = null;
      }
      // Keep the last status flowing while the stream is down, then hand control back to it.
      startPolling();
      scheduleEventRetry();
    };
  }

  /* ---------- settings, PIN and configuration -------------------------------
     One PIN concept: on the TV the daemon hands the loopback page its PIN and
     this surface shows it behind Reveal; from the LAN the readout is forbidden,
     so the same row becomes the four-digit entry. The PIN authorises every
     write from here, transport included. The server is read-only and the theme
     has no switch. The player name is a draft until Save. */

  function normalizePin(value) {
    var digits = String(value || "").replace(/\D/g, "");
    return digits.length > 4 ? digits.slice(0, 4) : digits;
  }

  function trimmed(value) {
    return String(value === null || value === undefined ? "" : value)
      .replace(/\s+/g, " ")
      .replace(/^\s+|\s+$/g, "");
  }

  function renderPin() {
    if (!pinElement) {
      return;
    }
    if (pinFromTv) {
      pinElement.removeAttribute("hidden");
      if (pinReveal) {
        pinReveal.removeAttribute("hidden");
        pinReveal.disabled = pin === null;
        pinReveal.setAttribute("aria-pressed", pinRevealed ? "true" : "false");
        setText(pinReveal, pinRevealed ? "Hide" : "Reveal");
      }
      if (pinInput) {
        pinInput.setAttribute("hidden", "hidden");
        pinInput.value = pin || "";
      }
      setText(pinElement, pin === null ? "\u2014" : pinRevealed ? pin : "\u2022\u2022\u2022\u2022");
      pinElement.setAttribute(
        "aria-label",
        pin === null ? "PIN unavailable" : pinRevealed ? "PIN " + pin : "PIN hidden"
      );
      setText(pinHint, "Writes from this TV, e.g. renaming the player");
    } else {
      /* LAN: the daemon refuses the readout, so this one row is the entry. */
      pinElement.setAttribute("hidden", "hidden");
      if (pinReveal) {
        pinReveal.setAttribute("hidden", "hidden");
      }
      if (pinInput) {
        pinInput.removeAttribute("hidden");
        pinInput.value = pin || "";
      }
      setText(pinHint, "Read the PIN on the TV, then type it here");
    }
  }

  function setPin(value) {
    pin = value ? String(value) : null;
    renderPin();
  }

  function usablePin() {
    return pin !== null && pin.length === 4 ? pin : null;
  }

  function focusPin() {
    if (pinInput && !pinInput.hidden) {
      pinInput.focus();
    }
  }

  function loadPin() {
    request("GET", "/api/pin", null, function (status, body) {
      pinFromTv = status === 200 && body && body.pin ? true : false;
      pinRevealed = false;
      setPin(pinFromTv ? body.pin : null);
    });
  }

  /* Re-reads the boot PIN from the daemon; loopback only, which is where this page runs. */
  function refreshPin(done) {
    request("GET", "/api/pin", null, function (status, body) {
      var fresh = status === 200 && body && normalizePin(body.pin);
      if (fresh) {
        pinFromTv = true;
        pinRevealed = false;
        setPin(fresh);
      }
      done(fresh ? true : false);
    });
  }

  function pinRejected(target) {
    setText(target, "Wrong or expired PIN. Check the PIN on the TV.");
    if (pinFromTv) {
      loadPin();
    } else {
      setPin(null);
      focusPin();
    }
    updateCommitState();
  }

  function configFeedback(text) {
    setText(configMessage, text || "\u00a0");
  }

  /* ---------- the player-name draft ----------------------------------------- */

  function isDirty() {
    return trimmed(playerInput ? playerInput.value : nameCommitted) !== trimmed(nameCommitted);
  }

  function clearSavedClose() {
    if (savedCloseTimer !== null) {
      window.clearTimeout(savedCloseTimer);
      savedCloseTimer = null;
    }
  }

  function hideDiscardPrompt() {
    if (!discardRow) {
      return;
    }
    discardOpen = false;
    discardRow.setAttribute("hidden", "hidden");
  }

  function updateCommitState() {
    var dirty = isDirty();
    if (saveButton) {
      saveButton.disabled = !dirty;
    }
    if (cancelButton) {
      cancelButton.disabled = !dirty;
    }
    if (dirty) {
      /* A new keystroke owns the panel again: the saved-close countdown and the
         discard question no longer describe what is on screen. */
      clearSavedClose();
    } else {
      hideDiscardPrompt();
    }
  }

  function showNameValue() {
    if (playerValue) {
      playerValue.removeAttribute("hidden");
      setText(playerValue, nameCommitted);
    }
    if (playerInput) {
      playerInput.setAttribute("hidden", "hidden");
    }
  }

  /* The row reads as a value; Change reveals the input and hands it the remote's
     keyboard. Pressing Change again only refocuses, so a draft is never reset. */
  function editName() {
    if (!playerInput) {
      return;
    }
    if (playerInput.hidden) {
      if (playerValue) {
        playerValue.setAttribute("hidden", "hidden");
      }
      playerInput.removeAttribute("hidden");
      playerInput.value = nameCommitted;
      updateCommitState();
    }
    playerInput.focus();
  }

  function revertName() {
    if (playerInput) {
      playerInput.value = nameCommitted;
    }
    showNameValue();
    updateCommitState();
  }

  /* ---------- the TV audio delay --------------------------------------------
     Not a draft. The delay depends on the external device behind ARC and can
     only be tuned by ear while the group plays, so every step writes at once
     and the row always shows the value the daemon reports. */

  function reportedDelay(value) {
    var number = Number(value);
    if (!isFinite(number) || number < 0) {
      return 0;
    }
    return Math.round(number);
  }

  /* The row shows what the daemon reports; the steps stay in the range this
     surface can write (a hand-edited config may hold more). */
  function clampDelay(value) {
    var number = reportedDelay(value);
    return number > DELAY_MAX_MS ? DELAY_MAX_MS : number;
  }

  function renderDelay() {
    setText(delayValue, delayTarget + " ms");
  }

  function showDelaySteps() {
    if (!delaySteps || !delayEdit) {
      return;
    }
    delayEdit.setAttribute("hidden", "hidden");
    delaySteps.removeAttribute("hidden");
    /* The likely first move is up from zero; Left reaches the minus. */
    if (delayUp) {
      delayUp.focus();
    }
    wake();
  }

  function hideDelaySteps() {
    if (!delaySteps || !delayEdit) {
      return;
    }
    delaySteps.setAttribute("hidden", "hidden");
    delayEdit.removeAttribute("hidden");
  }

  /* The next status owns the number again, not the local increment. */
  function refreshDelay() {
    request("GET", "/api/status", null, function (status, body) {
      if (status !== 200 || !body) {
        return;
      }
      delayStatusValue = reportedDelay(body.output_delay_ms);
      if (delayQueue.length === 0 && !delayInFlight) {
        delayTarget = delayStatusValue;
        renderDelay();
      }
    });
  }

  function writeDelay() {
    if (delayInFlight || delayQueue.length === 0) {
      return;
    }
    var candidate = usablePin();
    if (candidate === null) {
      delayQueue = [];
      delayTarget = delayStatusValue;
      renderDelay();
      configFeedback(
        pinFromTv ? "PIN unavailable. Reopen Settings." : "Enter the 4-digit PIN from the TV"
      );
      focusPin();
      return;
    }
    var value = delayQueue.shift();
    delayInFlight = true;
    request("POST", "/api/config", { pin: candidate, output_delay_ms: value }, function (status) {
      delayInFlight = false;
      if (status === 200) {
        delayStatusValue = value;
        configFeedback("");
        if (delayQueue.length === 0) {
          refreshDelay();
        } else {
          writeDelay();
        }
        return;
      }
      /* A refused write must not leave the optimistic number on screen. */
      delayQueue = [];
      delayTarget = delayStatusValue;
      renderDelay();
      if (status === 403) {
        pinRejected(configMessage);
      } else if (status === 429) {
        configFeedback("Too many PIN attempts, please wait");
      } else {
        configFeedback("Delay not applied");
      }
    });
  }

  function stepDelay(delta) {
    wake();
    if (delta > 0 && delayTarget >= DELAY_MAX_MS) {
      return;
    }
    var next = clampDelay(delayTarget + delta);
    if (next === delayTarget) {
      return;
    }
    delayTarget = next;
    renderDelay();
    delayQueue.push(next);
    writeDelay();
  }

  function showDiscardPrompt() {
    if (!discardRow) {
      return;
    }
    if (!discardOpen) {
      discardOpen = true;
      discardRow.removeAttribute("hidden");
    }
    /* The question is fresh input: the 60 s clock starts again, and while it is
       open the auto-close never answers it by itself. */
    lastInputAt = Date.now();
    if (discardNo) {
      discardNo.focus();
    }
  }

  function keepEditing() {
    hideDiscardPrompt();
    if (playerInput && !playerInput.hidden) {
      playerInput.focus();
    } else if (nameEdit) {
      nameEdit.focus();
    }
    wake();
  }

  function discardChanges() {
    hideDiscardPrompt();
    revertName();
    setSettingsOpen(false);
  }

  /* Every close path — Close, Done, Escape/Back, the scrim, the auto-close and
     a return to the app — asks the same question. */
  function requestSettingsClose() {
    if (!settingsOpen) {
      return;
    }
    if (isDirty()) {
      showDiscardPrompt();
      return;
    }
    setSettingsOpen(false);
  }

  function resetSettingsView() {
    pinRevealed = false;
    renderPin();
    renderServer();
    configFeedback("");
    hideDiscardPrompt();
    clearSavedClose();
    if (playerInput) {
      playerInput.value = nameCommitted;
    }
    showNameValue();
    hideDelaySteps();
    renderDelay();
    updateCommitState();
  }

  function setSettingsOpen(open) {
    if (open === settingsOpen || !settingsPanel || !settingsBackdrop || !settingsToggle) {
      return;
    }
    settingsOpen = open;
    if (settingsCloseTimer !== null) {
      window.clearTimeout(settingsCloseTimer);
      settingsCloseTimer = null;
    }
    if (open) {
      settingsPanel.removeAttribute("hidden");
      settingsPanel.className = "settings-panel is-open";
      settingsBackdrop.removeAttribute("hidden");
      settingsBackdrop.className = "settings-backdrop is-open";
      settingsToggle.setAttribute("aria-expanded", "true");
      setIdle(false);
      syncBodyClasses();
      resetSettingsView();
      /* The first useful control is the name's edit action, not the close
         cross: from there the remote reaches every row with one press. */
      if (nameEdit) {
        nameEdit.focus();
      } else if (settingsClose) {
        settingsClose.focus();
      }
    } else {
      /* A reverse animation, not a hidden attribute: a one-frame cut over the
         1920x1080 scrim is the largest flash in the UI. */
      hideDiscardPrompt();
      clearSavedClose();
      settingsPanel.className = "settings-panel is-closing";
      settingsBackdrop.className = "settings-backdrop is-closing";
      settingsToggle.setAttribute("aria-expanded", "false");
      setIdle(false);
      syncBodyClasses();
      settingsToggle.focus();
      settingsCloseTimer = window.setTimeout(finishSettingsClose, SETTINGS_CLOSE_MS);
    }
    wake();
  }

  function finishSettingsClose() {
    settingsCloseTimer = null;
    if (settingsOpen) {
      return;
    }
    settingsPanel.setAttribute("hidden", "hidden");
    settingsPanel.className = "settings-panel";
    settingsBackdrop.setAttribute("hidden", "hidden");
    settingsBackdrop.className = "settings-backdrop";
  }

  function toggleSettings() {
    setSettingsOpen(!settingsOpen);
  }

  /* The read-only server row: the endpoint is part of the installation, the dot
     and the word are the link state the empty screen already uses. */
  function renderServer() {
    setText(serverElement, serverAddress || "\u2014");
    var connected = Boolean(serverAddress) && stageIsConnected(playbackStage);
    if (serverDot) {
      serverDot.className = connected ? "server-dot is-live" : "server-dot";
    }
    var word = "Waiting";
    if (serverAddress) {
      if (playbackStage === "connecting") {
        word = "Connecting";
      } else if (stageIsConnected(playbackStage)) {
        word = "Connected";
      } else {
        word = "Offline";
      }
    }
    setText(serverWord, word);
  }

  /* ---------- empty ---------------------------------------------------------
     Nothing loaded is a screen of its own: no card, no title, no rail. Entering
     it takes the focus off the hidden transport; leaving it never steals the
     focus back, because the bar's control is where the remote already is. */

  function setEmpty(on) {
    var wanted = on === true;
    if (wanted === empty) {
      return;
    }
    empty = wanted;
    syncBodyClasses();
    if (empty) {
      releaseRailFocus();
    }
  }

  /* The identity line under the empty statement: `Living Room · host:port`, with
     the separator only when both halves exist. */
  function setEmptyIdentity(name, server) {
    var clean = cleanName(name);
    var host = String(server === null || server === undefined ? "" : server)
      .replace(/\s+/g, " ")
      .replace(/^\s+|\s+$/g, "");
    setText(emptyName, clean);
    setText(emptyServer, host);
    if (emptySep) {
      if (clean && host) {
        emptySep.removeAttribute("hidden");
      } else {
        emptySep.setAttribute("hidden", "hidden");
      }
    }
  }

  /* At boot the first status decides where the remote starts: the transport when
     a live session is loaded, the bar's control when the screen is empty or not
     live (the transport is not on screen there). */
  function settleBootFocus() {
    if (!bootFocus) {
      return;
    }
    bootFocus = false;
    if (!empty && stageIsLive(playbackStage)) {
      if (playPause) {
        playPause.focus();
      }
      return;
    }
    if (settingsToggle) {
      settingsToggle.focus();
    }
  }

  /* ---------- idle ---------------------------------------------------------- */

  /* Idle hides the focused control, so it cannot keep the focus: the ring would
     be invisible and the next OK would act on a button nobody can see. The
     control is remembered and handed back when the stage wakes. */
  function rememberFocus() {
    var active = document.activeElement;
    if (active && active !== document.body && active !== document.documentElement) {
      lastFocused = active;
    }
  }

  function restoreFocus() {
    var target = lastFocused;
    var usable =
      target && document.contains(target) && (target.offsetWidth > 0 || target.offsetHeight > 0);
    if (empty) {
      target = settingsToggle;
    } else if (target === progressTrack && !barIsStop()) {
      /* The bar was the control the remote had; the track or the session ended
         while the chrome was hidden, so the transport takes it back. */
      target = playPause;
    } else if (!usable) {
      target = playPause;
    }
    if (!target || document.activeElement === target) {
      return;
    }
    target.focus();
  }

  function setIdle(on) {
    var wanted = on === true;
    if (wanted === idle) {
      return;
    }
    idle = wanted;
    if (idle) {
      rememberFocus();
      if (document.activeElement && document.activeElement.blur) {
        document.activeElement.blur();
      }
    }
    syncBodyClasses();
    if (!idle) {
      restoreFocus();
    }
  }

  function wake() {
    lastInputAt = Date.now();
    if (idle) {
      setIdle(false);
    }
  }

  /* ---------- audio and the sleeve ----------------------------------------- */

  function applyAudio(data) {
    if (!visualMath || !data) {
      return;
    }
    visualTarget = visualMath.target(data);
    lastAudioAt = Date.now();
  }

  /* One property, written only when the value changes, so the 20 Hz tick stays
     on the compositor. */
  function setSleeve(transform) {
    if (transform === lastSleeve) {
      return;
    }
    art.style.transform = transform;
    lastSleeve = transform;
  }

  /* Three tiny scaleX writes, each guarded by its own last value: the band
     never restarts a transition, so a steady bar is not a repaint. `state`
     null puts the bars on the visible floor (pause, idle, empty). */
  function setBars(state) {
    if (!visualMath) {
      return;
    }
    var levels = [state && state.bass, state && state.mid, state && state.treble];
    var means = [state && state.meanBass, state && state.meanMid, state && state.meanTreble];
    var index;
    for (index = 0; index < bandBars.length; index += 1) {
      var bar = bandBars[index];
      if (!bar) {
        continue;
      }
      var value = visualMath.band(levels[index], means[index]).toFixed(2);
      if (value === lastBand[index]) {
        continue;
      }
      bar.style.transform = "scaleX(" + value + ")";
      lastBand[index] = value;
    }
  }

  /* True while all three bars sit on the floor, so the paused/idle tick can
     stop without re-deriving a value it has already written. */
  function barsRest() {
    return lastBand[0] === BAND_REST && lastBand[1] === BAND_REST && lastBand[2] === BAND_REST;
  }

  /* The play rise: for 200 ms after playback starts the sleeve's transform is on
     a transition, so the first 20 Hz writes ease up out of rest instead of
     stepping to the breathe. The class is dropped afterwards and the tick owns
     the property again — one window per real state change, never per poll. */
  function easeSleeveIn() {
    sleeveEase = true;
    syncBodyClasses();
    if (sleeveEaseTimer !== null) {
      window.clearTimeout(sleeveEaseTimer);
    }
    sleeveEaseTimer = window.setTimeout(function () {
      sleeveEaseTimer = null;
      sleeveEase = false;
      syncBodyClasses();
    }, SLEEVE_EASE_MS);
  }

  function visualTick() {
    if (!visualMath) {
      return;
    }
    var now = Date.now();
    var playing = playbackStage === "playing";
    /* Nothing plays or nothing is loaded: the envelope has nobody to move, so it
       stops instead of running 20 times a second forever. The card settles on
       the stylesheet's 240 ms transition rather than snapping to rest, and the
       band drops to its floor while its opacity fades.

       Idle is deliberately not in this list. The burn-in screen is the one place
       where a bright, still card would sit untouched for an hour, so the music
       keeps moving it there too — the same transform on the same layer, at half
       band strength, which is also why the 240 ms transition is disabled for the
       whole playing state. */
    if (playing && !empty) {
      if (now - lastAudioAt > AUDIO_STALE_MS) {
        visualTarget = visualMath.create();
      }
      visualState = visualMath.update(visualState, visualTarget, now);
      setSleeve(visualMath.sleeve(visualState));
      setBars(visualState);
    } else if (lastSleeve !== SLEEVE_REST || !barsRest()) {
      visualState = visualMath.create();
      visualTarget = visualMath.create();
      setSleeve(SLEEVE_REST);
      setBars(null);
    }

    /* A full-screen surface owns the screen: while Settings or the credits page
       is up, the stage underneath keeps its focus instead of idling and blurring
       the control the page will hand the ring back to. The stage's idle rule is
       unchanged whenever neither surface is open. */
    var shouldIdle = visualMath.shouldIdle(
      now,
      lastInputAt,
      playbackStage === "playing",
      settingsOpen || creditsOpen
    );
    if (shouldIdle !== idle) {
      setIdle(shouldIdle);
    }
    if (settingsOpen && !discardOpen && now - lastInputAt >= SETTINGS_AUTO_CLOSE_MS) {
      requestSettingsClose();
    }
  }

  function sendCommand(command, payload) {
    /* The rail is inert while the session is not live: `pointer-events` stops the
       press, this guard stops Enter and every other path. */
    if (!stageIsLive(playbackStage)) {
      return;
    }
    var candidate = usablePin();
    if (candidate === null) {
      setText(message, "Open Settings for the PIN");
      setSettingsOpen(true);
      focusPin();
      return;
    }
    /* The seek commands carry their target: `position_ms` for an absolute seek,
       `offset_ms` for a relative one. The transport commands send the bare name. */
    var body = { pin: candidate, cmd: command };
    if (payload) {
      var key;
      for (key in payload) {
        if (Object.prototype.hasOwnProperty.call(payload, key)) {
          body[key] = payload[key];
        }
      }
    }
    request("POST", "/api/transport", body, function (status, reply) {
      /* The daemon draws a fresh PIN on every start, so a page that stayed open
         across a restart holds a stale one. Read it again and retry once before
         telling the viewer their PIN is wrong. */
      if (status === 403 && !retriedPin) {
        retriedPin = true;
        refreshPin(function (fresh) {
          if (fresh) {
            sendCommand(command, payload);
          } else {
            retriedPin = false;
            pinRejected(message);
          }
        });
        return;
      }
      retriedPin = false;
      if (status === 200) {
        setText(message, "\u00a0");
        return;
      }
      /* A refused seek must not leave the cursor ahead of the fill: drop the
         pending target and let the cursor return to the daemon's position in
         this frame. The fill never moved. */
      if (command === "seek_relative") {
        seekPending = null;
        tick();
      }
      if (status === 403) {
        pinRejected(message);
      } else if (status === 429) {
        setText(message, "Too many PIN attempts, please wait");
      } else {
        setText(message, "Command failed");
      }
    });
  }

  /* Save is the one commit point: it sends the player name with the PIN that is
     already on the panel. The server half of the payload is the installation's
     value, unchanged; the daemon still requires both fields. */
  function saveConfig() {
    if (!isDirty()) {
      return;
    }
    var candidate = usablePin();
    if (candidate === null) {
      configFeedback(
        pinFromTv ? "PIN unavailable. Reopen Settings." : "Enter the 4-digit PIN from the TV"
      );
      focusPin();
      return;
    }
    if (!serverAddress) {
      configFeedback("Server is not known yet");
      return;
    }
    var playerName = trimmed(playerInput.value);
    if (!playerName) {
      configFeedback("The player name cannot be empty");
      playerInput.focus();
      return;
    }
    if (saveButton) {
      saveButton.disabled = true;
    }
    if (cancelButton) {
      cancelButton.disabled = true;
    }
    configFeedback("Saving\u2026");
    request(
      "POST",
      "/api/config",
      { pin: candidate, server: serverAddress, player_name: playerName },
      function (status) {
        if (status === 200) {
          nameCommitted = playerName;
          playerInput.value = playerName;
          showNameValue();
          updateCommitState();
          configFeedback("Saved");
          if (nameEdit) {
            nameEdit.focus();
          }
          clearSavedClose();
          savedCloseTimer = window.setTimeout(function () {
            savedCloseTimer = null;
            if (settingsOpen && !isDirty()) {
              setSettingsOpen(false);
            }
          }, SAVED_CLOSE_MS);
        } else if (status === 403) {
          pinRejected(configMessage);
        } else if (status === 429) {
          configFeedback("Too many PIN attempts, please wait");
          updateCommitState();
        } else {
          configFeedback("Saving failed");
          updateCommitState();
        }
      }
    );
  }

  /* ---------- focus and keys ------------------------------------------------ */

  function focusableElements() {
    var root = settingsOpen && settingsPanel ? settingsPanel : document;
    var nodes = root.querySelectorAll("[data-focusable]");
    var result = [];
    var index;
    for (index = 0; index < nodes.length; index += 1) {
      var element = nodes[index];
      var visible =
        element.offsetWidth > 0 || element.offsetHeight > 0 || element.getClientRects().length > 0;
      if (
        !element.disabled &&
        visible &&
        window.getComputedStyle(element).pointerEvents !== "none"
      ) {
        result.push(element);
      }
    }
    return result;
  }

  function center(rect) {
    return {
      x: rect.left + rect.width / 2,
      y: rect.top + rect.height / 2,
    };
  }

  /* The bar is one stop, and the row is placed by hand, not by the nearest-centre
     score. Up from any transport control or from the selected progress bar lands
     on Settings, Down returns to the control the remote came from
     (`lastRailFocus`, the bar included). Right from Next steps onto the bar; from
     the bar Left/Right only ever seek, and Down steps onto play-pause — the next
     control in the row, not a jump across the screen. The bar's centre sits on
     the pause button's line, and a move that only the geometry approves is the
     one the next layout change breaks. Without this, Up would pick the wordmark
     (a dead stop) or nothing at all, and the only control on the screen would be
     unreachable from the d-pad. */
  function isRailControl(element) {
    return (
      element === prev || element === playPause || element === next || element === progressTrack
    );
  }

  function routeRail(direction, list, active) {
    if (
      direction === "right" &&
      active === next &&
      barIsStop() &&
      list.indexOf(progressTrack) !== -1
    ) {
      progressTrack.focus();
      return true;
    }
    /* Down from the bar is the row's own neighbour: the thumb stays where it is
       and the nearest exit is play-pause. Up remains the way to Settings. */
    if (direction === "down" && active === progressTrack && list.indexOf(playPause) !== -1) {
      playPause.focus();
      return true;
    }
    return false;
  }

  function routeTopbar(direction, list, active) {
    if (!settingsToggle || list.indexOf(settingsToggle) === -1) {
      return false;
    }
    if (direction === "up" && isRailControl(active)) {
      lastRailFocus = active;
      settingsToggle.focus();
      return true;
    }
    if (active !== settingsToggle) {
      return false;
    }
    if (direction === "up") {
      return true;
    }
    if (direction === "down") {
      var target = lastRailFocus;
      if (list.indexOf(target) === -1) {
        target = list.indexOf(playPause) === -1 ? null : playPause;
      }
      if (target) {
        target.focus();
      }
      return true;
    }
    return false;
  }

  function moveFocus(direction) {
    var list = focusableElements();
    if (list.length === 0) {
      return;
    }
    var active = document.activeElement;
    var index = list.indexOf(active);
    if (index === -1) {
      list[0].focus();
      return;
    }

    if (!settingsOpen && routeTopbar(direction, list, active)) {
      return;
    }
    if (!settingsOpen && routeRail(direction, list, active)) {
      return;
    }

    var origin = center(active.getBoundingClientRect());
    var best = null;
    var bestScore = Infinity;
    var cursor;

    for (cursor = 0; cursor < list.length; cursor += 1) {
      var candidate = list[cursor];
      if (candidate === active) {
        continue;
      }
      var point = center(candidate.getBoundingClientRect());
      var dx = point.x - origin.x;
      var dy = point.y - origin.y;
      var primary;
      var secondary;
      if (direction === "left") {
        if (dx >= -4) {
          continue;
        }
        primary = -dx;
        secondary = Math.abs(dy);
      } else if (direction === "right") {
        if (dx <= 4) {
          continue;
        }
        primary = dx;
        secondary = Math.abs(dy);
      } else if (direction === "up") {
        if (dy >= -4) {
          continue;
        }
        primary = -dy;
        secondary = Math.abs(dx);
      } else {
        if (dy <= 4) {
          continue;
        }
        primary = dy;
        secondary = Math.abs(dx);
      }
      var score = primary + secondary * 0.6;
      if (score < bestScore) {
        bestScore = score;
        best = candidate;
      }
    }

    if (!best) {
      var step = direction === "left" || direction === "up" ? -1 : 1;
      best = list[(index + step + list.length) % list.length];
    }
    best.focus();
  }

  function isTextInput(element) {
    if (!element || !element.tagName) {
      return false;
    }
    var tag = String(element.tagName).toUpperCase();
    return tag === "INPUT" || tag === "TEXTAREA";
  }

  function onKeyDown(event) {
    var key = event.keyCode || event.which;
    /* The credits page owns the screen but no control: any key closes it, and
       none of the d-pad routing below may run while it (or its 200 ms exit) is
       up. The focus is handed back when the exit lands. */
    if (creditsOpen) {
      event.preventDefault();
      event.stopPropagation();
      if (!creditsClosing) {
        closeCredits();
      }
      return;
    }
    var wasIdle = idle;
    wake();
    /* A key after the success line means the remote wants to stay: the return to
       Settings only happens when the panel is left alone. */
    if (settingsOpen && savedCloseTimer !== null) {
      clearSavedClose();
    }
    /* Back on webOS: 461 and 27 (Escape) on some builds, 1536/1537 on the LG
       Magic Remote of webOS 4 - verified on the device with a key log, where the
       system opened its app switcher because 1536/1537 were not claimed. The
       listener sits on window in the capture phase and always claims these keys. */
    if (key === 27 || key === 461 || key === 1536 || key === 1537) {
      event.preventDefault();
      event.stopPropagation();
      if (settingsOpen) {
        if (discardOpen) {
          /* Escape on the question means "Keep editing", so a second press is
             needed to ask again — it never discards by surprise. */
          keepEditing();
        } else {
          requestSettingsClose();
        }
      }
      return;
    }
    var direction = null;
    if (key === 37) {
      direction = "left";
    } else if (key === 38) {
      direction = "up";
    } else if (key === 39) {
      direction = "right";
    } else if (key === 40) {
      direction = "down";
    } else if (key === 13) {
      /* The first OK after idle only brings the chrome back. The control it
         would otherwise press is still invisible, so acting would be blind. */
      if (wasIdle) {
        event.preventDefault();
        return;
      }
      var active = document.activeElement;
      /* Enter in a text field is the remote keyboard's "done": it hands the
         remote to the commit row when there is a draft, else to Change. */
      if (isTextInput(active)) {
        event.preventDefault();
        if (active === playerInput && isDirty() && saveButton && !saveButton.disabled) {
          saveButton.focus();
        } else if (nameEdit) {
          nameEdit.focus();
        }
        return;
      }
      if (active && active.tagName === "BUTTON") {
        event.preventDefault();
        active.click();
      }
      return;
    } else {
      return;
    }
    if (isTextInput(document.activeElement) && (direction === "left" || direction === "right")) {
      return;
    }
    event.preventDefault();
    /* The focused bar is a slider, not a row to leave: Left/Right step the track
       by 10 s and keep the focus, so holding the d-pad keeps stepping on the
       browser's own key repeat — no timer here, and no entry is skipped. A step
       that cannot move (the track start, the end, an unknown duration, a
       reconnecting session) is refused locally: the focus stays and the cursor
       stays where it is. Up/Down leave the row any time (Settings above,
       play-pause's row below). */
    if (
      !settingsOpen &&
      document.activeElement === progressTrack &&
      (direction === "left" || direction === "right")
    ) {
      seekStep(direction === "right" ? SEEK_STEP_MS : -SEEK_STEP_MS);
      return;
    }
    moveFocus(direction);
  }

  /* ---------- the credits page ----------------------------------------------
     The wordmark is pointer-only and fires `takt:credits`; the page below opens
     on it. It holds no control of its own, so it is not a d-pad stop: while it
     is up every key closes it, a click closes it, and the stage underneath only
     keeps its playhead ticking. The focus it took is handed back to the control
     the remote came from — Settings on the empty stage, the rail element on a
     loaded one. The page never opens by itself and never opens over Settings. */
  function openCredits() {
    var event = null;
    if (typeof window.CustomEvent === "function") {
      event = new window.CustomEvent("takt:credits", { bubbles: true });
    } else if (document.createEvent) {
      event = document.createEvent("CustomEvent");
      event.initCustomEvent("takt:credits", true, false, null);
    }
    if (event && brand) {
      brand.dispatchEvent(event);
    }
  }

  function focusIsUsable(element) {
    if (!element || element === brand || element === creditsOverlay) {
      return false;
    }
    if (empty && isRailControl(element)) {
      return false;
    }
    return document.contains(element) && (element.offsetWidth > 0 || element.offsetHeight > 0);
  }

  /* The one place the credits page looks for a focus to keep and to hand back:
     the control the remote came from, else the last one it landed on, else the
     stage's own resting control. A bar that stopped being a stop (the track or
     its duration disappeared) falls back to play-pause. */
  function resolveCreditsFocus(preferred) {
    var target = preferred;
    if (target === progressTrack && !barIsStop()) {
      target = playPause;
    }
    if (focusIsUsable(target)) {
      return target;
    }
    target = lastStageFocus;
    if (target === progressTrack && !barIsStop()) {
      target = playPause;
    }
    if (focusIsUsable(target)) {
      return target;
    }
    if (!empty && stageIsLive(playbackStage) && focusIsUsable(playPause)) {
      return playPause;
    }
    return focusIsUsable(settingsToggle) ? settingsToggle : null;
  }

  /* A pointer click on the wordmark can focus the button before the click
     listener runs; the focus-in record is then still the control the remote
     had. Programmatic and remote-triggered clicks leave it on the stage. */
  function currentStageFocus() {
    var active = document.activeElement;
    if (active && active.getAttribute && active.getAttribute("data-focusable") !== null) {
      return active;
    }
    return lastStageFocus;
  }

  function showCredits() {
    if (creditsOpen || settingsOpen || !creditsOverlay) {
      return;
    }
    /* Wake first: an idle stage has blurred its control, and the remembered
       one is exactly where the page must hand the ring back. */
    wake();
    creditsReturnFocus = resolveCreditsFocus(currentStageFocus());
    creditsOpen = true;
    creditsClosing = false;
    if (creditsCloseTimer !== null) {
      window.clearTimeout(creditsCloseTimer);
      creditsCloseTimer = null;
    }
    creditsOverlay.className = "credits";
    creditsOverlay.removeAttribute("hidden");
    syncBodyClasses();
    if (creditsOverlay.focus) {
      creditsOverlay.focus();
    }
  }

  /* Any key, a click and Back all land here. The exit keeps `creditsOpen` true
     until the 200 ms fade has landed, so a second key cannot operate the stage
     through a half-faded page; the focus returns when the page is hidden. */
  function closeCredits() {
    if (!creditsOpen || creditsClosing || !creditsOverlay) {
      return;
    }
    creditsClosing = true;
    wake();
    if (creditsCloseTimer !== null) {
      window.clearTimeout(creditsCloseTimer);
    }
    creditsOverlay.className = "credits is-closing";
    syncBodyClasses();
    creditsCloseTimer = window.setTimeout(finishCreditsClose, CREDITS_CLOSE_MS);
  }

  function finishCreditsClose() {
    creditsCloseTimer = null;
    creditsOpen = false;
    creditsClosing = false;
    creditsOverlay.setAttribute("hidden", "hidden");
    creditsOverlay.className = "credits";
    syncBodyClasses();
    var target = resolveCreditsFocus(creditsReturnFocus);
    creditsReturnFocus = null;
    if (target && document.activeElement !== target) {
      target.focus();
    }
    wake();
  }

  /* ---------- boot ---------------------------------------------------------- */

  /* webOS 4 delivers the remote's Back key as a history navigation, not only as a
     key code: with no history entry the system leaves the app and shows its app
     switcher. One pushed state claims that step; popstate asks the settings surface
     to close and re-arms the state so a second Back cannot leave the app either.
     This is the mechanism LG's own BackButtonControl sample uses. */
  function armBackHistory() {
    if (!window.history || !window.history.pushState) {
      return;
    }
    try {
      window.history.pushState({ takt: true }, "");
    } catch (error) {
      return;
    }
    if (window.addEventListener) {
      window.addEventListener(
        "popstate",
        function () {
          wake();
          if (creditsOpen) {
            closeCredits();
          } else if (settingsOpen) {
            requestSettingsClose();
          }
          try {
            window.history.pushState({ takt: true }, "");
          } catch (error) {
            /* history is best-effort; the key handler still covers 461/1536 */
          }
        },
        false
      );
    }
  }

  function init() {
    armBackHistory();
    if (window.addEventListener) {
      window.addEventListener("keydown", onKeyDown, true);
    } else {
      on(document, "keydown", onKeyDown);
    }
    on(prev, "click", function () {
      sendCommand("prev");
    });
    on(playPause, "click", function () {
      var playing = current && current.state === "playing";
      sendCommand(playing ? "pause" : "play");
    });
    on(next, "click", function () {
      sendCommand("next");
    });
    /* The bar owns the ring: the caption's two ends and the reconnect dim are
       focus states. The focus event lands between ticks, so repaint here. */
    on(progressTrack, "focus", function () {
      barFocus = true;
      syncBodyClasses();
      lastCaption = "";
      tick();
    });
    on(progressTrack, "blur", function () {
      barFocus = false;
      syncBodyClasses();
      lastCaption = "";
      tick();
    });
    on(brand, "click", openCredits);
    /* The Easter egg listens for the wordmark's own event, never a timer; a
       click on the page is as good as a key, and the last focused stage control
       is kept so the page can return the ring where it was. */
    on(document, "takt:credits", showCredits);
    on(creditsOverlay, "click", closeCredits);
    on(document, "focusin", function (event) {
      var target = event.target;
      if (
        target &&
        target.getAttribute &&
        target.getAttribute("data-focusable") !== null &&
        target !== creditsOverlay
      ) {
        lastStageFocus = target;
      }
    });
    on(pinInput, "input", function () {
      pin = normalizePin(pinInput.value) || null;
      wake();
    });
    on(pinReveal, "click", function () {
      pinRevealed = !pinRevealed;
      renderPin();
    });
    on(playerInput, "input", function () {
      wake();
      updateCommitState();
    });
    on(nameEdit, "click", editName);
    on(delayEdit, "click", showDelaySteps);
    on(delayDown, "click", function () {
      stepDelay(-DELAY_STEP_MS);
    });
    on(delayUp, "click", function () {
      stepDelay(DELAY_STEP_MS);
    });
    on(saveButton, "click", saveConfig);
    on(cancelButton, "click", revertName);
    on(discardYes, "click", discardChanges);
    on(discardNo, "click", keepEditing);
    on(settingsDone, "click", requestSettingsClose);
    on(settingsToggle, "click", toggleSettings);
    on(settingsClose, "click", requestSettingsClose);
    on(settingsBackdrop, "click", requestSettingsClose);

    playbackStage = "ready";
    fitIdleLine();
    setText(stateWord, STATE_LABELS.idle);
    setText(idleState, STATE_LABELS.idle);
    renderServer();
    renderDelay();
    fitViewport();
    fitTitle();
    /* The boot frame has no session and nothing loaded: the bar is inert until
       the first status can offer it. */
    syncBarStop();

    if (document.fonts && document.fonts.ready && document.fonts.ready.then) {
      document.fonts.ready.then(function () {
        fitTitle();
        fitIdleLine();
      });
    }

    on(window, "resize", fitViewport);
    on(window, "orientationchange", fitViewport);

    on(window, "focus", function () {
      /* Returning to the app asks to close, which is what a Back press would
         do — but the remote keyboard also blurs and refocuses the window, and
         that must never close the panel mid-edit. */
      var active = document.activeElement;
      var typing = active && (active.tagName === "INPUT" || active.tagName === "TEXTAREA");
      if (settingsOpen && !typing) {
        requestSettingsClose();
      }
    });
    on(document, "mousemove", wake);
    on(document, "click", wake);

    if (visualMath) {
      visualState = visualMath.create();
      visualTarget = visualMath.create();
      window.setInterval(visualTick, 50);
      visualTick();
    }
    loadPin();
    startEvents();
    window.setInterval(tick, PROGRESS_TICK_MS);
    if (playPause) {
      playPause.focus();
    }
  }

  /* A small, documented surface for the headless harness and for the later
     credits task; it exposes no daemon writes. `simulateIdle` ages the input
     clock so the harness can photograph the idle frame without waiting 30 s. */
  window.TaktStage = {
    applyStatus: applyStatus,
    applyAudio: applyAudio,
    simulateIdle: function (on) {
      lastInputAt = on ? Date.now() - 31000 : Date.now();
      setIdle(on);
    },
  };

  init();
})();
