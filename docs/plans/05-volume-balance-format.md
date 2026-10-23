# Plan 05 — Volume, balance and the format/bitrate line

**Status:** ✅ done — 2026-10-23, 5c97b5c. 268 tests pass (2 `#[ignore]`d:
a visual dump and the audio-device smoke test), goldens unchanged, the
`--ignored` device test plays through the new DSP stage; deviations
annotated at the end.

Goal: the volume and balance controls change what you hear (and are
remembered), and the player shows what is playing: `FLAC 24bit 96kHz stereo`
for lossless, `MP3 320kbps 44.1kHz stereo` for lossy. A bit-perfect marker
shows only when no processing touches the samples.

Prerequisites: plan 03 merged (`RealPlayer`, `Engine`). Plan 04 is recommended
first (the sample tap must stay *before* the gain stage).

## Decisions

| Topic | Decision |
|---|---|
| Volume range | 0–100, step 5. **Default 100 (unity).** |
| Volume law | `gain = (v / 100)^2` (const `VOLUME_EXPONENT = 2.0`; 0 → silence). Sounds natural; not linear. |
| Balance range | −100 (full left) … +100 (full right), step 5, default 0. |
| Balance law | Attenuate only the *opposite* channel: `left = 1.0` if `b ≤ 0` else `1.0 - b/100`; `right = 1.0` if `b ≥ 0` else `1.0 + b/100`. Applies to **stereo (2 ch) streams only**; mono/multichannel ignore balance. |
| Where applied | In the audio callback, in a small `Dsp` struct (new), after the (future) EQ and **after** the spectrum tap. |
| Bit-perfect | If target gains are exactly `1.0` and the smoothed gains have settled at `1.0`, **skip the multiply entirely** (samples untouched). |
| Click prevention | Gain changes are smoothed per frame toward the target (`cur += (target - cur) * 0.002`), snapping to the target when `|target - cur| < 1e-5`. |
| Persistence | `[audio] volume`, `balance` in `config.toml`; saved 500 ms after the last change (debounced) and on quit. |
| Keys | Volume: unchanged (`+`/`=`/`-`, `↑↓` in player). Balance: new global `[` (left) and `]` (right). |

## Files

`src/audio/mod.rs`, `src/audio/engine.rs`, `src/player/mod.rs`, `src/player/real.rs`,
`src/player/mock.rs`, `src/action.rs`, `src/keymap.rs`, `src/app.rs`, `src/config.rs`, `src/ui/player.rs`,
`README.md`. New: `src/audio/dsp.rs`.

## Steps

### 1. DSP stage (`src/audio/dsp.rs`) — pure, unit-testable
```rust
pub struct SharedParams {            // shared UI thread → audio callback, all lock-free
    pub volume_gain: AtomicU32,      // f32::to_bits
    pub balance_left: AtomicU32,
    pub balance_right: AtomicU32,
    // plan 06 adds the EQ fields
}
pub struct Dsp { params: Arc<SharedParams>, channels: usize, cur_vol: f32, cur_l: f32, cur_r: f32 }
impl Dsp { pub fn process(&mut self, frames: &mut [f32]); }   // interleaved
pub fn volume_to_gain(v: u8) -> f32;
pub fn balance_to_gains(b: i8) -> (f32, f32);
```
`process`: read the three targets with `Relaxed`. If all targets are `1.0` **and** all `cur_*` are `1.0` → `return` immediately.
Otherwise per frame step the smoothing, multiply sample 0 by `cur_vol * cur_l` and sample 1 by `cur_vol * cur_r` for stereo
(for other channel counts multiply every sample by `cur_vol` only). No allocation.

`SharedParams` is created once by `RealPlayer`, cloned (`Arc`) into the engine. The engine moves a `Dsp` into the cpal callback
and calls `dsp.process(&mut data[..filled])` **after** the callback has pushed the mono samples to the spectrum tap
(plan 04) and **only over the samples that were really popped** — never over the padding silence.

### 2. Commands and snapshot
- `PlayerCommand::SetBalance(i8)` (clamp −100..=100). `SetVolume(u8)` exists.
- `RealPlayer::handle(SetVolume/SetBalance)`: clamp, store, write the atomics with `volume_to_gain` / `balance_to_gains`.
- `MockPlayer`: handle `SetBalance` too (store only).
- `FormatInfo` (src/player/mod.rs) gains `bitrate_kbps: Option<u32>` and `lossless: bool`. Keep `bit_perfect`.
  `bit_perfect` is computed in `RealPlayer::snapshot()`: `volume == 100 && balance == 0` (plan 06 adds `&& !eq_active`).
  Update `FormatInfo::default()` (used by the mock): `lossless: true, bitrate_kbps: None` so goldens stay identical.
- `FormatInfo::empty()` (codec empty) for "nothing playing" — the UI renders no format line for it.

### 3. Format line text
Add `impl FormatInfo { pub fn describe(&self) -> String }`:
- Lossless (`lossless == true`): `"{codec} {depth}bit {rate} {channels}"`, e.g. `FLAC 24bit 96kHz stereo`; omit `{depth}bit` if unknown.
- Lossy: `"{codec} {kbps}kbps {rate} {channels}"`, e.g. `MP3 320kbps 44.1kHz stereo`; omit the kbps part if unknown.
- Rate format: `44100 → "44.1kHz"`, `48000 → "48kHz"`, `96000 → "96kHz"`, `88200 → "88.2kHz"` (drop trailing `.0`).
- Unknown rate → omit.
Look at how `src/ui/player.rs` (~lines 190–200, where `fmt.bit_perfect` is used) renders the format today and make it call `describe()`;
keep the `bit_perfect` marker exactly where it is. If the line is too narrow, drop `channels`, then the rate — read the existing truncation logic first.
Goldens must not change: with the default mock format `describe()` must produce the string the golden files already contain. Check with `cargo test` and fix `describe()` (not the golden) if it differs.

### 4. Where the bitrate comes from
- Lossless: not needed.
- Lossy: when scanning (plan 02) read `tagged.properties().audio_bitrate()` (kbps) and store it as `Track.bitrate_kbps: Option<u32>`;
  if absent compute `file_size * 8 / duration_secs / 1000` when both are known. `RealPlayer` copies it into `FormatInfo` on `Started`.
- Codec / depth / rate / channels / `lossless` come from the decoder's `StreamInfo` (plan 03), not from tags.

### 5. Balance actions (follow README rule 8)
`Action::BalanceLeft` (`"balance_left"`, global, key `"["`, "balance left"), `Action::BalanceRight` (`"balance_right"`, global, `"]"`).
`App::dispatch`: read `snapshot.balance`, ±5, clamp, send `SetBalance`. Hints/help overlay update themselves.
Check that `src/ui/player.rs` renders a balance slider bound to `snapshot.balance` (center = 0) and the volume slider to `snapshot.volume`. If either is missing, add it using the existing `slider.rs` widget.

### 6. Persistence (`src/config.rs`, `src/app.rs`)
- `pub struct AudioConfig { pub volume: u8 (default 100), pub balance: i8 (default 0) }` with `#[serde(default)]`; add `audio: AudioConfig` to `Config`.
  In `sanitize` clamp both. In `Config::save` add `set_table(&mut doc, "audio", |t| { set_preserving_decor(...) })` exactly like the `layout`/`theme` blocks.
- Startup (non-demo): after building the player send `SetVolume(config.audio.volume)` and `SetBalance(config.audio.balance)`.
- Debounce: add `audio_dirty_since: Option<Instant>` to `App`. On every volume/balance change set it (and copy the value into `config.audio`).
  In `tick`, if `dirty_since.elapsed() >= 500 ms` → `save_config()` and clear. On quit (`should_quit`) flush if dirty.
  Reuse the `read_only` guard in `save_config`. (Plan 06 reuses this mechanism for the EQ — name it generically: `config_dirty_since`.)
- Demo mode never saves (the existing `config_path: None` / demo handling already does this — verify).

### 7. Tests
- `volume_to_gain`: 0 → 0.0, 100 → exactly 1.0, 50 → 0.25 (± 1e-6), monotonic increasing.
- `balance_to_gains`: (0 → 1,1), (−100 → 1,0), (100 → 0,1), (−50 → 1,0.5), (50 → 0.5,1).
- `Dsp::process`:
  - unity targets → buffer **bit-identical** (`assert_eq!` on the `Vec<f32>`).
  - volume 0.5 gain on a constant 1.0 buffer → settles to 0.5 (run enough frames) and never overshoots.
  - balance right-only: left channel is zero after settling, right untouched; mono buffer ignores balance.
- `FormatInfo::describe` — all five forms from step 3 plus the missing-field cases.
- Config: round-trip `[audio]`, clamping of `volume = 250`, a pre-existing file with comments keeps them after save.
- App: `]` ×3 → balance 15; at 100 stays 100; the debounce flag is set; `bit_perfect` is false when volume < 100.
- Keymap table test and conflict test updated.
- Manual: with `tests/fixtures`, volume keys audibly change level; `[` pans left; restart the app and the levels come back; at 100/0 the `bit-perfect` marker is on.

### 8. Docs
README keys table (`[` `]`), config example:
```toml
[audio]
volume = 100
balance = 0
```

## Done when
Volume and balance are audible and persisted, the format line matches the per-codec rules, the bit-perfect marker appears only at 100 % / centered, and the three cargo checks pass.

## Assumptions I made (tell me to change)
- Default volume is 100 so a fresh install is bit-perfect.
- Squared volume curve.
- Balance only attenuates the opposite channel and only affects stereo.

## Deviations (annotated at the end, per plans-README rule 11)

Implemented in full; every step's tests pass. The following details differ
from (or clarify) the plan text:

1. **The smoothing snap threshold is 1e-4, not the planned 1e-5.** With a
   `1.0` target, the f32 step `cur += (1 - cur) * 0.002` *stalls* at
   |1 − cur| ≈ 1.5e-5: the increment drops below half an ULP and the
   addition rounds to a no-op — just above the planned threshold, so the
   snap (and with it the bit-perfect fast path, which needs an exact `1.0`)
   would never fire. 1e-4 fires first; the jump is 0.01 % of full scale
   (≈ −0.0009 dB). Documented in a comment on `SNAP` in `src/audio/dsp.rs`.
2. **`describe()` renders the *golden* line format, not the plan's §3
   example strings.** The plan asked for `FLAC 24bit 96kHz stereo`
   (single-spaced, `24bit` + `96kHz` as separate parts), but also said the
   goldens must not change and to fix `describe()` if it differs — and the
   goldens pin `FLAC  24/96  stereo  bit-perfect` (double-spaced parts,
   combined `depth/rate`). `describe()` follows the goldens:
   lossless → `FLAC  24/96  stereo`, lossy → `MP3  320kbps  44.1kHz  stereo`,
   with the same omit-when-unknown rules (omit `{depth}bit` → here the
   combined `24/96` degrades to `96kHz` / `24-bit`; omit kbps; omit rate).
3. **`bit_perfect = lossless && volume == 100 && balance == 0`**, not just
   the volume/balance clause: the pre-existing rule "a lossy source is never
   bit-perfect" had to survive (and its test), so the computed marker in
   `RealPlayer::snapshot()` keeps the `lossless` gate. Plan 06's
   `&& !eq_active` slots in there.
4. **`SetVolume`/`SetBalance` are handled in `RealPlayer::handle` (and the
   mock's), before the empty-playlist guard** — `handle_transport` early-
   returns on an empty playlist, but the config restore at session start
   (`App::new_session`) must land before any track is loaded.
5. **`FormatInfo::format_parts()` was added next to `describe()`.** The UI's
   width ladder (drop channels, then the rate) needs the parts, not one
   joined string; `describe()` (the planned API) is what feeds the widest
   candidate, and the UI appends the `bit-perfect` marker exactly where it
   always was. `format_khz` moved from `src/ui/player.rs` to
   `src/player/mod.rs` so the model can use it; the UI's copy is gone.
6. **The balance command carries an `i8`, so it cannot hold ±200** — the
   ±100 musical clamp happens in `App::dispatch` (i32 arithmetic) and again
   in both players' setters. The clamp tests use ±127/±128 (the `i8`
   extremes) instead of the plan's literal ±200.
7. **`SharedParams::set(volume, left, right)` writes all three atomics in
   one call** instead of only exposing the raw fields: the player always
   updates volume and balance together, so a change to one can never leave
   the other stale.
8. **Extra tests beyond the plan's list**: an engine-level test that the DSP
   runs over *popped frames only* (the padding silence is untouched),
   a return-to-bit-perfect round trip, the library's size/duration bitrate
   estimate on a tag-less WAV, and app-level proof that demo mode and a
   read-only (unparseable) config never write audio changes.

Manual checks from step 7 remain for a human with ears: volume keys
audibly change level, `[` pans left, the levels come back after a restart,
and the `bit-perfect` marker shows at 100 / 0. The automated `--ignored`
device test (`real_engine_plays_a_fixture_end_to_end`) passes on this
machine — the tone plays through the new DSP-wired callback — and the
config round-trip + startup restore are unit-tested.
