---
name: webos-chromium53-ui
description:
  Use when changing the TV UI or theme, adding animations, or chasing stutter/low frame rate on the
  LG webOS 4 Chromium 53 renderer (ES5, 32-bit ARM, 1080p).
---

# UI on Chromium 53 / ARM

The TV runs Chromium 53. No ES modules, no optional chaining, no modern CSS layout helpers, no
`background-clip: text` without `-webkit-`. Keep `web/*.js` ES5 and verify with `node --check`.

## Measured costs (1080p rAF, same TV, same session)

| Stage variant                                             | fps   |
| --------------------------------------------------------- | ----- |
| 6 beams + sparks + `-webkit-mask` + transform transitions | 7.5   |
| no sparks                                                 | 16.5  |
| no sparks and no masks                                    | 29.3  |
| 3 beams, no masks/sparks/transitions, no glow layer       | ~64   |
| old festival stage (3 beams + horizon + sweep + glow)     | 34-44 |

=> Large translucent layers, masks and CSS transitions on them dominate the cost. The Vice stage
replaces the beams with one static plate, one sleeve transform, three 4 px spectrum bars and opacity
fades; it has not been measured on the TV yet, but it animates one composited card layer plus three
tiny bars, and has no masks or filters.

## Rules that follow

- Animate `transform` and `opacity` only. No masks, filters or box-shadow animation on large layers.
  One large blurred surface: the ghost inside the plate (40 px, painted once per track). The card
  carries a 40 px shadow that only moves with the idle orbit; the title's glyph halo is flat paint
  (13 shadows, no blur).
- Continuous, slow motion (the idle orbit) belongs on the compositor: a CSS keyframe on the sleeve.
  The playing breathe is one `transform` per 50 ms tick (`translateY` 0..-3 px plus the scale), and
  the three bar `scaleX` writes ride the same tick; every write is guarded by its last value and has
  no transition, so a steady value costs nothing.
- The 20 Hz visual tick (`setInterval(visualTick, 50)`) may only change small deltas: scale,
  opacity, small translates. Large 20 Hz position jumps read as judder. It does no audio work while
  paused, idle or empty; it is still the idle clock.
- Idle is one gesture: everything that joins or leaves it (content, wordmark, idle line, transport,
  bar, message) fades on `opacity 400ms cubic-bezier(0.23, 1, 0.32, 1)`. The playhead is retargeted
  every 250 ms over a 250 ms `linear` transform, and a press is `scale(0.97)` over 160 ms.
- A track change is not input: only a key or a pointer event ends idle. The first OK after idle only
  wakes, and the focus returns to the control that had it.
- The sleeve commits when the visible `<img>` has decoded: `has-cover`, the spine stops and `lum-hi`
  are written in that frame, so the old artwork stays painted until the new one is ready. The ghost
  is loaded at that moment, once per track.
- `skewX`/`rotate` needs an `overflow: hidden` container for the cut edge; the plate is rotated
  `-10deg` and promoted with `translateZ(0)` (identity in 2D) so headless Chromium does not paint
  its last rows without its children.
- Keep the number of full-width/height layers low. A full-screen animated layer costs ~1/3 of the
  frame budget.
- Text gradients: `-webkit-background-clip: text` works; the unprefixed property does not. (The Vice
  stage sets Anton in flat light instead, which is cheaper.)
- Icons are inline SVG; there is no icon font and no build step.

## Theme architecture

- The stage is `state-playing | state-paused | state-connecting | state-ready | state-offline`, plus
  `palette-leonida`, `is-empty`, `is-idle`, `has-cover`, `tinted`, `lum-hi`, `show-volume` and
  `settings-open` on `body`. The palette is fixed to Leonida; `palette-night` and `palette-mono`
  remain in the stylesheet as artwork fallbacks, but there is no user switch and nothing persists a
  choice. A cover may still write the two spine stops (`tinted`).
- The ten binding tokens (ink `#17161c`, night `#060d27`, violet-deep `#350056`, violet `#1e1324`,
  lavender `#828ec8`, indigo `#415fce`, neon `#b835c0`, danger `#e8544b`, light `#fdf0b9`, sand
  `#b6aca3`) are the same in all three palettes; the palettes differ only in the spine stops. Neon
  is an accent and never a field, `--focus` is fixed to it and never sampled, and danger carries the
  offline state only. A readable cover contributes only the spine stops; a bright cover drops the
  ghost to 0.08.
- The canvas is 1920x1080 inside `#viewport`, which `app.js` scales by `min(vw/1920, vh/1080)` from
  the top-left corner. Only a resize writes it, so a 1280x720 panel shows the whole rail.
- Type: Anton for display (`/fonts/anton.ttf`, embedded), the system sans for metadata. Chromium 53
  ignores `font-display`, so the title frame is a fixed 316 px and the 158 -> 96 px fit runs on a
  track change and once when the font lands, never per frame. The chrome uses 42 / 36 / 34 / 28 / 26
  / 20 px; the title and the idle line are the only ladders. The metadata is one voice: artist 42 px
  in the warm light, album 28 px in sand at 70 % with no rule, no box and no indent, and the album
  line is dropped when it equals the title or is contained in it (`visual.albumLabel`).
- The state block leads the title: a 36 px word plus one glyph per state (filled play, pause bars,
  three transform-only dots, a dot — danger for Offline). It replaces the 26 px state word. While
  the session is not live (connecting, offline, ready) the rail and the bar drop to 40 % and go
  inert, and the play button shows no glyph; a play triangle only ever means Playing.
- The title may cross the cut edge. There is no ink bed and no measurement: the line carries a
  glyph-following halo (8 offsets at 2 px, 4 at 3 px, one 10 px wash at 28 % alpha) in ink, so the
  letters themselves mask the cover. Measured on a white cover: 15.55:1 glyph-to-halo, no cover
  pixel within 3 px of a letter, and no ink row spanning more than 60 % of the cover.
- Reactive data: daemon PCM analyzer -> SSE `audio` events (20 Hz) -> `SendspinVisual` envelope ->
  the sleeve's `transform` (`translateY` 0..-3 px, `scale(1 .. 1.075)`) and the 300/220/150 px
  spectrum bars (`scaleX(0.05 .. 1)`). `visual.js` must stay DOM-free and testable in Node
  (`scripts/test-visual.js`).
- Nothing loaded is the deliberate empty screen (`is-empty`): the card is not drawn at all, and one
  32 px "Nothing playing", a 28 px sand line pointing at Music Assistant and the player's own
  `room · server` line with a connection dot (neon while the daemon answers, sand while connecting
  or offline) fill the upper third. No 158 px wordmark stands in for a title: the wordmark appears
  once, in the top bar.
- Burn-in protection: idle after 30 s playing / 15 s paused hides the chrome and keeps one 60 px
  line on the upper third (or the empty block when nothing is loaded). The sleeve takes a 3 px
  circle over 60 s; without a card, the block that remains takes it. Do not animate big areas in
  idle.
- The top bar sits inside the safe area (96/54, 56 px tall): `Takt · <room>` on the left, the one
  labelled `Settings` control on the right. Both are sand at 26 px with no box at rest; focus adds
  the rail's ring (2 px ink, then 8 px neon). The wordmark still dispatches `takt:credits` on click
  but is not a d-pad stop: Up from the transport lands on Settings, Down returns to the control the
  remote came from. An unknown/empty player name of "Takt" is dropped, so the wordmark appears once.

## Verification

```sh
node --check web/app.js
node scripts/test-visual.js
node scripts/tv/ui-test.js
node scripts/tv/fps.js 4
```
