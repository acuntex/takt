# How playback behaves

The short version is in the [README](../README.md). This page has the details.

## Volume

Music Assistant moves the TV's master volume and the TV reports back the level it actually reached,
so the MA slider and the TV stay in step. Remote changes are reported to MA during playback.

A level up to 40 is applied directly. Above 40, one MA command may raise the volume by at most 20
points (TV 15 + MA 100 → TV 40; TV 40 + MA 100 → TV 60), and the applied value is reported back at
once so the MA slider follows it. Lowering is always allowed. The guard applies per MA command:
repeated unsafe commands are clamped and reported each time.

While no Luna volume snapshot exists yet, an MA volume command is ignored instead of clamping from
0, so the TV cannot drop to 40 by accident. The initial state reports 0 for safety until the first
Luna snapshot arrives; a playback start re-reports whatever is known by then.

**The audible level is the PulseAudio sink.** On this TV the TV master alone does not change the
sound on the PulseAudio output path, so the daemon also sets `pcm_output` (override with
`TAKT_PULSE_SINK`) using a perceptual curve: 20 → 45 %, 50 → 71 %, 100 → 100 %. A missing or failing
`pactl` is logged and affects neither playback nor the daemon.

## Pause, mute and leaving the app

Pause stops the decoder and player children instead of muting the TV. Killing `pacat` discards the
audio buffered in its pipe and in PulseAudio, so the pause is silent without a device-wide mute, and
the next stream starts with fresh children. Mute stays user intent: an explicit MA or remote mute is
recorded, reported and survives later stream starts. A pause never leaves a mute behind, because it
never sets one.

The daemon owns the "viewer left" rule. A held foreground subscription pauses on the first foreign
app or input, with a five-second query fallback. The webOS Home overlay alone does not pause.

## Screensaver

A veto is held from service start and renewed every 60 seconds for as long as the daemon runs, so
the panel and the screensaver stay off even when nothing is playing. It does not wake the TV from
standby and does not cover the idle-to-standby path; see the Auto Power Off note in the README.

## Group synchronisation

The client schedules playout itself and writes each frame shortly before its deadline. The **TV
audio delay** row (`output_delay_ms`) compensates a receiver behind the TV's audio port: start at 0,
play the same track on a second player in the group, and step in 10 ms until the two line up by ear.

A server that knows the receiver's chain can correct the same value with the `set_output_delay`
player command, which the client advertises. The last writer wins and the value survives reboots.

## Live TV and other apps

In Live TV mode the tuner audio bypasses the PulseAudio sink, so Takt is not audible there. There is
no ducking in v1: music keeps playing when another app makes a sound.
