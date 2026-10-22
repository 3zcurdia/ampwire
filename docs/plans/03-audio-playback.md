# Plan 03 — Real audio playback (wav, mp3, m4a, flac)

**Status:** ⬜ not started — safe to pick up (needs plan 02 done).

Goal: tracks in the playlist actually play through the speakers, with
**play, pause, stop, next, prev** working. Tracks auto-advance; bad files are
skipped with a notice.

**Explicitly NOT in this plan**: the timer, seeking, spectrum animation (plan 04),
volume/balance (plan 05), EQ (06). In this plan the real player reports
`position = 0:00` and an all-zero spectrum. That is expected.

Prerequisites: plan 02 merged (`Playlist`, `Track`, `TrackSource`).

## Decisions

| Topic | Decision |
|---|---|
| Decode | `symphonia` (wav, mp3, flac, m4a with AAC or ALAC). |
| Output | `cpal`, `f32` samples. |
| Threads | UI thread (App) · one **engine thread** (owns the cpal `Stream` and the decoder) · cpal's own audio callback thread. |
| Sample path | Decoder → `f32` interleaved → SPSC ring buffer → cpal callback. **No resampling.** The cpal stream is opened at the track's own sample rate and channel count. |
| Unsupported device config | If the device cannot open that rate/channel count → treat the track as unplayable (skip + notice `device cannot play 48000 Hz 8ch`). Resampling is a later plan. |
| Gapless | Not in this plan. A new stream is built per track, so a tiny gap between tracks is expected. |
| End of track | Next track. Last track: `repeat` on → wrap to track 0; off → stop. |
| Manual Next on last track | Same rule as end of track. |
| Prev | If `position > 3 s` restart the track; else previous track (first track → restart). Position is not tracked until plan 04, so **for now Prev always goes to the previous track** (first → restart). Leave a `// plan 04` comment. |
| Play order | Two modes, toggled by the existing `s` key: **Ordered** (playlist order, default) and **Shuffle**. Both go through one `PlayOrder` type (step 6) so `Next`, `Prev`, auto-advance and the Jellyfin look-ahead (plan 08) all agree on "what plays next". |
| Shuffle | A full random permutation of the playlist (Fisher–Yates), with the current track first. Every track plays once before any repeats. `Prev` walks back through the same permutation. Permutation exhausted → `repeat` on: build a new one (its first track must differ from the last played when `len ≥ 2`); off: stop. |
| Toggling shuffle | Keeps the current track playing. Ordered → Shuffle builds a permutation starting at the current track. Shuffle → Ordered continues from the current track's playlist position. Show notice `shuffle on` / `shuffle off`. |
| Playlist edits while shuffled | Any add/remove/move/clear rebuilds the permutation (current track stays first; shuffle history is lost). Acceptable. |
| Bad file | Notice `skipped <name>: <reason>`, advance to the next. If every track in a row fails (count ≥ playlist length) → stop, notice `no playable tracks`. |
| Demo / dump | `--demo` and `--dump` keep using `MockPlayer`. |
| Linux | cpal needs ALSA dev headers (`libasound2-dev` on Debian/Ubuntu). Document it in README. |

## Files

`Cargo.toml`, `src/main.rs`, `src/app.rs`, `src/player/mod.rs`, `README.md`, `AGENTS.md`.
New: `src/audio/mod.rs`, `src/audio/decoder.rs`, `src/audio/engine.rs`, `src/player/real.rs`,
`tests/fixtures/*` (see step 9).

## Steps

### 1. Dependencies
```
cargo add symphonia --features mp3,isomp4,aac,alac,flac,wav,pcm
cargo add cpal
cargo add rtrb          # wait-free SPSC ring buffer
```
Check the actual feature names in the symphonia version Cargo resolved
(docs.rs → symphonia → Feature Flags). If a name differs, use the documented one.

### 2. Module skeleton
Add `mod audio;` to `main.rs`. `src/audio/mod.rs` declares `decoder` and `engine`.

### 3. Decoder (`src/audio/decoder.rs`) — no audio device, fully unit-testable

```rust
pub struct StreamInfo {
    pub codec: String,            // "FLAC", "MP3", "AAC", "ALAC", "PCM"
    pub lossless: bool,           // flac, alac, pcm = true
    pub sample_rate: u32,
    pub channels: u16,
    pub bit_depth: Option<u8>,    // codec_params.bits_per_sample
    pub duration: Option<Duration>,  // n_frames / sample_rate
}
pub struct Decoder { /* format reader, decoder, track_id, SampleBuffer<f32> */ }
impl Decoder {
    pub fn open(path: &Path) -> Result<Decoder, String>;
    pub fn info(&self) -> &StreamInfo;
    /// Decode the next packet and append interleaved f32 samples to `out`.
    /// Ok(true) = produced data, Ok(false) = end of stream.
    pub fn read(&mut self, out: &mut Vec<f32>) -> Result<bool, String>;
}
```
Recipe (verify names against the symphonia docs):
1. `File::open` → `MediaSourceStream::new(Box::new(file), Default::default())`.
2. `Hint` with the file extension → `symphonia::default::get_probe().format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())`.
3. Pick `format.default_track()` (must have a codec that is not `CODEC_TYPE_NULL`).
4. `symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())`.
5. `read`: loop `format.next_packet()`. Ignore packets whose `track_id` differs.
   `decoder.decode(&packet)`: on `Error::DecodeError` → skip that packet and keep looping;
   on `Error::IoError` with `UnexpectedEof` → return `Ok(false)`; any other error → `Err(msg)`.
   Convert with a lazily created `SampleBuffer::<f32>::new(capacity, spec)` and `copy_interleaved_ref(decoded)`;
   re-create it if `decoded.capacity()` grows.
6. Codec name: `get_codecs().get_codec(codec_type).map(|d| d.short_name)`, upper-cased.
   `lossless` = codec type is FLAC, ALAC or one of the PCM types.

### 4. Engine (`src/audio/engine.rs`) — the only place that touches cpal

Why one thread owns both: on macOS the cpal `Stream` is `!Send`, so it must be created and dropped on the thread that uses it.

```rust
pub enum EngineCmd { Load { path: PathBuf }, Pause, Resume, Stop, Shutdown }
pub enum EngineEvent {
    Started(StreamInfo),
    Finished,                    // decoder hit EOF AND the ring buffer drained
    Error(String),
}
pub struct Engine { tx: Sender<EngineCmd>, rx: Receiver<EngineEvent>, join: Option<JoinHandle<()>> }
impl Engine { pub fn spawn() -> Engine; pub fn send(&self, c: EngineCmd); pub fn poll(&self) -> Vec<EngineEvent>; }
impl Drop for Engine { /* send Shutdown, join */ }
```

Thread loop (pseudocode):
```
let host = cpal::default_host(); let device = host.default_output_device();
state: Option<Playing { stream, decoder, producer, eof: bool, scratch: Vec<f32> }>, paused: Arc<AtomicBool>
loop {
    // 1. commands (non-blocking)
    while let Ok(cmd) = rx.try_recv() { match cmd {
        Load{path} => { drop(old state); match start(path) { Ok(p) => {send Started(info); state = Some(p)}, Err(e) => send Error(e) } }
        Pause  => paused.store(true),   Resume => paused.store(false),
        Stop   => state = None,         Shutdown => return,
    } }
    // 2. feed the ring buffer
    if let Some(p) = &mut state {
        if !p.eof && producer.slots() >= p.scratch.len().max(MIN_CHUNK) { refill scratch via decoder.read, push all; on Ok(false) → eof = true; on Err(e) → send Error(e), state = None }
        else if p.eof && producer.slots() == capacity { send Finished; state = None }   // drained
    }
    thread::sleep(5ms) when nothing to do   // never busy-spin
}
```
`start(path)`:
1. `Decoder::open(path)`; no output device → `Err("no audio output device")`.
2. Find a supported output config: iterate `device.supported_output_configs()`; need `channels == info.channels`, sample format `F32`,
   and `min_sample_rate <= rate <= max_sample_rate`. Use `.with_sample_rate(rate)`. None found → `Err(format!("device cannot play {rate} Hz {ch}ch"))`.
3. Ring buffer: `rtrb::RingBuffer::<f32>::new(rate as usize * channels as usize / 2)` (≈ 0.5 s). Producer stays in the engine thread; Consumer moves into the callback.
4. `device.build_output_stream(&config, move |data: &mut [f32], _| { ... }, err_cb, None)`.
   The callback: if `paused` → fill `data` with `0.0`; else for each slot `*s = consumer.pop().unwrap_or(0.0)`. Nothing else (rule 6).
5. `stream.play()`. Pre-fill the buffer **before** `play()` so playback does not start on silence: decode until the ring is ~50% full or EOF.
6. `err_cb` sets an `Arc<AtomicBool>` the thread loop checks → sends `Error("audio device error")`.

If a stream is already running when `Load` arrives, drop it first (old ring buffer dies with it, so no stale audio).

### 5. Real player (`src/player/real.rs`)
```rust
pub struct RealPlayer {
    playlist: Playlist, state: PlayState, shuffle: bool, repeat: bool,
    volume: u8, balance: i8,                 // stored only; plan 05 applies them
    format: FormatInfo,                      // from EngineEvent::Started
    engine: Engine, order: PlayOrder, fail_streak: usize,
    notice: Option<String>, rng: Rng,        // the shuffle source; copy the tiny xorshift from mock.rs into a shared `player/rng.rs`
}
```
- `snapshot()` → `PlayerSnapshot` with `position: ZERO`, `spectrum`/`peaks`: 24 zeros.
  `format`: blank (`FormatInfo` with empty codec) when stopped/no track.
- `handle(cmd)`:
  - `Play`: if paused → `Resume`, state Playing. If stopped → play current (or index 0 if `None`) via `start_track(i)`.
  - `Pause`: Playing → `engine.send(Pause)`, state Paused.
  - `TogglePlayPause`: Playing → pause, otherwise → play.
  - `Stop`: `engine.send(Stop)`, state Stopped, keep `current`.
  - `Next` / `Prev` / `PlayTrack(i)`: ask `order.next(repeat, &mut rng)` / `order.prev()` (step 6), then `start_track`. `PlayTrack(i)` calls `order.jump_to(i)`.
  - `SeekBy`: ignore (plan 04). `SetVolume(v)`: store. `ToggleShuffle`: flip, then `order.rebuild(len, current, shuffle, &mut rng)` and set the notice. `ToggleRepeat`: flip, notice `repeat on/off`.
  - Playlist commands (`AddTracks`, `ReplaceTracks`, `RemoveTrack`, `ClearPlaylist`, `MoveTrack`): same semantics as `MockPlayer`; if the removed/cleared track was current → `engine.send(Stop)`, state Stopped.
  - `ReplaceTracks(non-empty)` → also `start_track(0)`.
- `start_track(i)`: set `current = Some(i)`, state = Playing, `engine.send(Load{path})`.
  Remote sources do not exist yet; `match track.source { Local(p) => … }`.
- `tick(dt)`: `for ev in engine.poll()`:
  - `Started(info)` → fill `format` (codec, lossless, bit_depth, sample_rate, channels string: 1→`mono`, 2→`stereo`, n→`{n}ch`), `fail_streak = 0`.
  - `Finished` → `advance_after_end()`.
  - `Error(e)` → `notice = Some(format!("skipped {}: {e}", track.display_name()))`, `fail_streak += 1`; if `fail_streak >= playlist.len()` → stop, notice `no playable tracks`; else `advance_after_end()`.
- `take_notice(&mut self) -> Option<String>`.

### 6. Play order (`src/player/order.rs`, new; pure, unit-test it)
```rust
pub struct PlayOrder { order: Vec<usize>, pos: usize, shuffle: bool }
impl PlayOrder {
    pub fn new() -> Self;                                   // ordered, empty
    /// Rebuild after a playlist edit or a shuffle toggle. `current` = playlist index being played.
    /// Ordered: order = 0..len, pos = current. Shuffle: current first, the rest Fisher–Yates shuffled.
    pub fn rebuild(&mut self, len: usize, current: Option<usize>, shuffle: bool, rng: &mut Rng);
    /// Advance and return the playlist index to play, or None when the order is finished and `repeat` is off.
    /// Ordered + repeat at the end → wraps to 0. Shuffle + repeat at the end → reshuffle (see Decisions).
    pub fn next(&mut self, repeat: bool, rng: &mut Rng) -> Option<usize>;
    /// Step back; at the start returns the same track (caller restarts it).
    pub fn prev(&mut self) -> Option<usize>;
    /// A track was chosen directly (Enter in the playlist): make it current. Ordered: pos = i.
    /// Shuffle: swap it into the *next* slot's place by moving it to pos+1 (so it plays now and the rest of the permutation continues).
    pub fn jump_to(&mut self, i: usize);
    /// Look-ahead/behind WITHOUT moving: the next `n` playlist indices (nearest first), no wrap, no reshuffle.
    pub fn upcoming(&self, n: usize) -> Vec<usize>;
    pub fn previous(&self, n: usize) -> Vec<usize>;
}
```
Rules: `len == 0` → every query returns `None`/empty. `current == None` → `next()` returns `Some(order[0])`.
`next`/`prev` update `pos`; `upcoming`/`previous` never do. `RealPlayer` calls `rebuild` whenever the playlist changes or shuffle toggles
(for example inside `AddTracks`, `ReplaceTracks`, `RemoveTrack`, `ClearPlaylist`, `MoveTrack`, `ToggleShuffle`).
Tests: ordered next/prev/wrap/stop-at-end; shuffle yields every index exactly once before ending; shuffle with `repeat` reshuffles and the first of the new
round differs from the last of the old when `len ≥ 2`; `prev` after 3 `next`s returns the exact tracks visited, in reverse; `jump_to` in both modes;
`upcoming(3)` / `previous(3)` match what `next()` / `prev()` then return; `rebuild` keeps `current` first in shuffle; `len == 1` and `len == 0` never panic.
Use a seeded `Rng` so shuffle tests are deterministic.

### 7. Backend switch in `App` (`src/app.rs`)
Replace the `player: MockPlayer` field with an enum so the old tests keep working:
```rust
pub enum Backend { Mock(MockPlayer), Real(RealPlayer) }
impl Backend { fn handle(&mut self, c: PlayerCommand); fn tick(&mut self, dt: Duration);
               fn snapshot(&self) -> PlayerSnapshot; fn take_notice(&mut self) -> Option<String> }
```
`mock_mut()` (test-only) matches `Backend::Mock`. `App::tick` additionally does
`if let Some(n) = self.player.take_notice() { self.ui.notice = Some(n); }`.
`App::new(.., demo)`: `demo` → `Mock(MockPlayer::animated())`, otherwise `Real(RealPlayer::new())`.
`App::deterministic` stays `Mock`. Replace plan-02's `MockPlayer::empty()` use in `main.rs` accordingly.

### 8. Shutdown
`Drop for RealPlayer` drops `Engine` (sends `Shutdown`, joins). Make sure `q` exits promptly even if a stream is playing.

### 9. Tests and fixtures
- Generate tiny fixtures once (0.5 s, 44.1 kHz stereo 440 Hz sine; ffmpeg is needed only for generation) and commit them under `tests/fixtures/` (each < 30 KB):
  ```
  ffmpeg -f lavfi -i "sine=frequency=440:duration=0.5" -ar 44100 -ac 2 tone.wav
  ffmpeg -i tone.wav tone.flac ; ffmpeg -i tone.wav -b:a 128k tone.mp3
  ffmpeg -i tone.wav -c:a aac -b:a 128k tone_aac.m4a ; ffmpeg -i tone.wav -c:a alac tone_alac.m4a
  ```
- `Decoder` tests: for each fixture, `open` succeeds; summing `read` output ≈ `0.5 * 44100 * 2` samples (±5 % for lossy priming);
  `info()` has the right codec/rate/channels/`lossless`; a text file renamed `.mp3` → `Err`.
- `PlayOrder` tests (step 6).
- `RealPlayer` state-machine tests **without a device**: factor the engine behind a small trait (`trait AudioOut { fn send(&self, EngineCmd); fn poll(&self) -> Vec<EngineEvent>; }`)
  and give tests a fake that records commands and replays events. Test: Play on a 3-track list sends `Load` for track 0; `Finished` → `Load` for track 1;
  last track + `Finished` with repeat off → state Stopped; `Error` → notice + next `Load`; N errors in a row → Stopped.
- Manual check (cannot be automated): `cargo run -- tests/fixtures`, hear the tones, press `Space`, `v`, `b`, `z`, and confirm no panic when `q` is pressed mid-play.

### 10. Docs
README: status line, Linux ALSA note, keys table (transport keys now real, `l` open). AGENTS.md: replace "audio: not yet chosen" with the chosen stack and the 3-thread model.

## Done when
Playing a folder of mixed wav/mp3/m4a/flac works with all five transport keys, a corrupt file is skipped with a notice,
and the three cargo checks pass. `--demo` and the golden tests still use the mock and are unchanged.

## Assumptions I made (tell me to change)
- No resampling in v1: unsupported device rates are skipped (not converted).
- A small gap between tracks (no gapless yet).
- Ordered play is the default; shuffle is the `s` toggle (no separate "order" UI beyond the existing shuffle indicator and notice).
- Prev ignores the "restart if > 3 s" rule until the clock exists (plan 04).
