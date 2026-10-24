# Plan 09 — Gapless playback (keep the stream open across tracks)

**Status:** ⬜ not started

Goal: when one track ends and the next one has the **same sample rate and
channel count**, the next track's first sample follows the previous track's
last sample with no silence, no device reopen and no click — as AGENTS.md
promises ("Gapless support is expected"). When the format differs, the
engine reopens the device as it does today.

Prerequisites: plans 03 (engine, `PlayOrder`), 04 (frames-played clock),
05/06 (`Dsp` in the callback) and 08 (remote sources) merged.

## Why today is not gapless (from the 2026-10-23 audit, P7)

- `EngineCmd::Load` drops the whole `Playing` (cpal `Stream` + ring) and
  `finish_start` builds a new one: `cpal::default_host()`,
  `default_output_device()`, `supported_output_configs()`, a fresh ring, a
  pre-fill and `play()` — tens of ms of silence on every track change.
- The engine only learns about the next track when the UI reacts to
  `EngineEvent::Finished`, i.e. after the ring has fully drained.
- The decoder for the next track is opened only then, so its probe (and for
  `stream*` modes, an HTTP round trip) adds to the gap.

## Decisions

| Topic | Decision |
|---|---|
| Scope | Gapless only between tracks with identical `sample_rate` **and** `channels`. Anything else reopens the device (today's path). No resampling, ever (hi-fi rule 7). |
| Who decides "next" | The UI thread, as now (`PlayOrder` is the single source of "what plays next"). It tells the engine ahead of time with a new `EngineCmd::Queue { source, start: ZERO }`; the engine never guesses. |
| When to queue | `RealPlayer` sends `Queue` for `order.upcoming(1)` right after every `Started`, and re-sends it (or `Unqueue`) whenever the order changes (shuffle/repeat toggle, playlist edit, `PlayTrack`). Repeat-one-track and end-of-order (no repeat) queue nothing. |
| Pre-open | On `Queue`, the engine opens the next `Decoder` immediately (local file, cache path, or `HttpRangeSource`), but decodes nothing yet. Opening errors are held and reported at the boundary (as today's `Error`, so the skip path is unchanged). |
| The switch | When the current decoder hits EOF and a compatible next decoder is queued, `Playing::feed` simply continues pushing the next decoder's samples into the **same** ring. The `Stream`, the ring and the `Dsp` (EQ filter state included) are kept. |
| Boundary clock | The engine records the boundary as *frames pushed so far* when it switches decoders. `PlaybackHandle` gains a shared `AtomicU64 boundary` (frames-played value at which the next track starts). `RealPlayer::tick` compares `played` with it and performs the track change (new current, `start_offset = 0`, format line) exactly when the audio crosses it — never earlier. A new `EngineEvent::Advanced(StreamInfo)` carries the next track's info. |
| `Finished` | Sent only when the ring drains with **no** compatible queued track (end of order, or a format change — the latter followed by the UI's normal `Load`). |
| Device cache | The engine keeps the `cpal::Device` and its `supported_output_configs()` between tracks (re-queried on `audio device error` or a config miss). Reopens get cheaper even when not gapless. |
| `stream_pure` | `eq_bypass` is per *track*: the engine flips it at the boundary (it owns the switch moment), not the UI at `Load`. The bypass flag moves into the queued item. |
| Seek / Stop / Next | Any `Load`, `LoadRemote`, `Stop` or `Unqueue` drops the queued decoder. A seek near the end does not cancel the queue. |
| Out of scope | Crossfade, ReplayGain, encoder delay/padding trimming beyond what symphonia already does (`Track::delay/padding`) — note it if MP3/AAC gaps remain audible. |

## Files

`src/audio/engine.rs` (Queue/Unqueue, decoder switch, boundary, device cache),
`src/audio/decoder.rs` (nothing new expected — verify symphonia's gapless
trimming options for MP3/AAC in the resolved version), `src/player/real.rs`
(queue maintenance, boundary-driven track change, stream_pure bypass move),
`src/player/mod.rs` (`PlayerCommand` unchanged), `README.md`, `AGENTS.md`.

## Steps

1. **Device cache** (independent, ship first): hold `Device` +
   `Vec<SupportedStreamConfigRange>` on the engine thread; invalidate on the
   cpal error callback. Test: two `Load`s query the configs once (fake seam
   or a counter behind `#[cfg(test)]`).
2. **`EngineCmd::Queue/Unqueue`** + pre-open into `Playing.next: Option<Decoder>`.
3. **The switch in `Playing::feed`**: on `Ok(false)` from the current
   decoder, if `next` is compatible, swap it in, record `boundary`, keep
   feeding in the same call. Incompatible → keep today's EOF behaviour.
4. **Boundary clock + `EngineEvent::Advanced`**; `RealPlayer` moves
   `current`/order cursor when `played >= boundary` (not when the event
   arrives — the event can precede the audio by the ring's length).
5. **Queue maintenance in `RealPlayer`** (after `Started`/`Advanced`, and on
   every order change).
6. **stream_pure**: move `set_eq_bypass` to the boundary.
7. **Tests**: `feed` switches decoders without a gap (two fixtures at the
   same rate → the ring holds A's last frame immediately followed by B's
   first), an incompatible next track yields `Finished`, the boundary clock
   moves `current` exactly at the crossing frame, `Stop` drops the queue,
   a shuffle toggle re-queues. Device test (`#[ignore]`): two tones play
   back to back with no zero frames between them.
8. **Docs**: README status line ("gapless between same-format tracks"),
   AGENTS.md engine description.

## Done when

Two consecutive same-format local tracks (and cache-mode remote tracks)
play with no silent frames between them, the timer and track title switch
exactly when the new track is heard, format changes still play (with the
reopen gap), and the three cargo checks pass.

## Assumptions I made (tell me to change)

- "Gapless" means sample-continuous between same-format tracks; a format
  change may keep a short gap.
- `stream*` modes get the same treatment (the next `HttpRangeSource` is
  probed ahead, which also hides its round trip).
- Encoder delay/padding is left to symphonia's defaults in this plan.
