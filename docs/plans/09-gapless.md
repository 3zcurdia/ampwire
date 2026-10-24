# Plan 09 — Gapless playback (keep the stream open across tracks)

**Status:** ✅ done — 2026-10-23, 5c633f2. 392 tests pass (4 `#[ignore]`d: a
visual dump, the two audio-device smokes — both run manually here and green —
and the real-server Jellyfin smoke).

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

## Deviations (annotated during implementation, 2026-10-23)

All eight steps shipped as written; these are the places where the code
diverges from — or extends — the literal text above.

1. **`Queue`'s payload** — the plan specifies `Queue { source, start: ZERO }`;
   the built-in command is `Queue { source, start, eq_bypass }`. `start` is
   there (and honoured by the pre-open) but always `Duration::ZERO` today; the
   `eq_bypass: bool` is step 6's "the bypass flag moves into the queued item"
   made explicit on the wire.
2. **`QueueSource`, not `TrackSource`** — `source` is a new engine-owned enum
   (`Local(PathBuf)` / `Remote { item_id, container }`) instead of the player's
   `TrackSource`, so `audio` never depends on `player` and carries no playlist
   state.
3. **The compatibility check runs at `Queue` time, not at EOF** (step 3 puts it
   on the decoder's `Ok(false)`). `Feeder::queue` compares the next track's
   rate/channels against the current decoder's — the current format cannot
   change mid-stream, so the information is identical, and an incompatible
   track is dropped from the queue up front (its decoder is probed, then
   released). `switch_next` then swaps whatever is queued unconditionally.
   Observable behaviour matches the plan: incompatible → today's EOF → drain →
   `Finished` → the UI's normal reopen.
4. **A no-`Range` server queues nothing instead of erroring** (plan silent).
   If `HttpRangeSource::open` returns `NoRanges` for the *queued* track, the
   queue is silently dropped, so after the drain the normal `LoadRemote` →
   `Buffering` → whole-file download path plays it exactly as before. Holding
   it as a boundary `Error` would have *skipped* the track — a regression
   against plan 08.
5. **Repeat wraps stay non-gapless.** The plan lists "repeat-one-track and
   end-of-order (no repeat)" as queue-nothing; in this codebase repeat is a
   bool and `PlayOrder::upcoming` never wraps, so a repeat *wrap* (the cursor
   at the end with repeat on) also queues nothing and reopens the device. In
   shuffle + repeat at the end, `order.next` reshuffles anyway, so the next
   track is genuinely unknowable — queueing it would guess.
6. **`stream_pure` keeps two flip sites, only one moved.** Step 6 says "move
   `set_eq_bypass` to the boundary" — done for the gapless path (the engine
   flips the shared flag when it swaps decoders, carrying the queued track's
   flag). The `Load` path still sets it in `load_track`: format changes, seeks
   and non-remote transitions have no boundary, and there the UI's `Load` *is*
   the track change. Both write the same atomic; they can never disagree
   because a `Load` drops the queue that would have used the other one.
7. **`start_offset = 0` after the crossing lives in the UI** (step 4's wording
   implies a handle mutation, but `PlaybackHandle` is shared and immutable
   once sent). `RealPlayer` gained `track_base` (frames) and
   `track_start_offset`: `position = track_start_offset + (played −
   track_base) / rate`, both reset at the crossing — so the new track's clock
   is exactly frame-relative to the boundary, while the engine's handle keeps
   counting the whole stream.
8. **Cache-mode queue needs the file ready.** The plan says the engine opens
   the "cache path" on `Queue`; if the next remote track is still downloading,
   the UI queues nothing (`upcoming(1)` resolves through
   `Cache::path_if_ready`) and the normal buffering path takes over at the
   boundary. A downloaded file queues as a plain local path.
9. **Step 1's test uses a generic cache, not a device seam.** cpal offers no
   seam to fake `supported_output_configs()`, so the cache is `Cached<T>` with
   a `queries` counter: the pure test proves "two starts query once,
   invalidate re-queries" without hardware; the device itself is only touched
   by the `#[ignore]`d tests.
10. **The device test asserts the boundary, not raw frames.** The plan's "no
    zero frames between them" is proven sample-*exactly* by
    `feed_switches_decoders_without_a_gap` (the ring output is compared
    byte-for-byte against standalone decodes of A and B — A's last sample
    directly followed by B's first). The `#[ignore]`d
    `two_tones_play_back_to_back_through_one_stream` proves the same wiring on
    real hardware (Started → Advanced → Finished on one stream, boundary
    recorded) — the sample tap can't be used for that check because it drops
    samples when nothing drains it. **Both ignored device tests were run
    manually during this implementation and pass.**
11. **A `Queue` arriving after the current decoder already hit EOF** (an order
    change in the final milliseconds) cannot switch: the read that set EOF has
    already run. The drain takes the normal `Finished` path — correct, just
    not gapless for that one race.
12. **Symphonia gapless verification** (the Files section's ask): symphonia
    0.6.1's `AudioDecoderOptions::default()` sets `gapless: true`, and the MP3
    decoder trims LAME delay/padding per packet (`buf.trim(trim_start,
    trim_end)`) — MP3 is gapless to the encoder's own trimming. AAC-in-MP4
    does **no** gapless trimming (the isomp4 reader keeps the encoder priming;
    +6.8 % on the 0.5 s AAC fixture, pinned by an existing `decoder.rs` test) —
    noted here per the plan's "note it if MP3/AAC gaps remain audible", left
    to symphonia as assumed.
13. **`src/audio/decoder.rs` needed no changes**, as the plan expected; the
    new decode-side state moved into `audio::engine::Feeder` instead (the
    plan's `Playing::feed` is now `Feeder::feed`, split out so the switch is
    unit-testable without a cpal `Stream`, which is `!Send` and needs real
    hardware).

**Done-when check:** two consecutive same-format local tracks play with no
silent frames between them (sample-exact unit test + the on-hardware two-tone
test); cache-mode remote tracks queue as soon as their file is ready
(`path_if_ready` → `QueueSource::Local`); format changes still play through
the old reopen path (incompatible-queue test + the existing `pick_config`
behaviour, now served from the device cache); `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings` and `cargo test` all pass.
