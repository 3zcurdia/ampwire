# Ampwire — terminal Winamp 2.0 replica with modern hi-fi playback

Goal: retro Winamp 2.x UI/feel in the terminal (Ratatui), but with real modern
playback: local lossless files + Jellyfin server streaming.

## Toolchain

- Rust edition 2024, managed via `mise` (`mise.toml`: `rust = "latest"`).
- TUI: `ratatui 0.30`. Import crossterm as `ratatui::crossterm` — no direct `crossterm` dependency, so the versions always match. Check the ratatui docs first when touching UI/event code — APIs shift between minors.
- Single binary crate. Entrypoint: `src/main.rs` (runs the TUI; `--demo` and `--dump WxH` are the non-interactive paths).

## Commands

- `cargo run` — run the TUI (must use a real TTY; output is garbage under pipes/CI).
- `cargo check --all-targets` — fastest verification loop.
- `cargo clippy --all-targets -- -D warnings` before finishing.
- `cargo fmt --check` / `cargo fmt` — rustfmt is the formatter, no extra config.
- `cargo test` — 241 tests today (239 run, 2 `#[ignore]`d: a visual dump and the audio-device smoke test). Unit tests live next to the modules (`#[cfg(test)] mod tests`); integration tests and the golden frames live in `tests/`.

## Architecture direction

- Keep audio decode/playback decoupled from the Ratatui render loop (dedicated audio thread/task, message-passing — never block `draw` on I/O or decoding).
- Boundaries today: `player` (`PlayerSnapshot` + `PlayerCommand`; `playlist.rs` owns the track list, `order.rs` the ordered/shuffle permutation, `real.rs` the real player, `mock.rs` the demo/dump player), `audio` (symphonia decode → `rtrb` SPSC ring → cpal output on a dedicated engine thread), `library` (background file scan + lofty tags over `std::sync::mpsc`), `ui` (Winamp skin: main window, playlist, EQ, theme). Still to come: `jellyfin` (server client/stream URLs).
- Audio stack (plan 03): `symphonia` (wav/mp3/flac/m4a: AAC or ALAC) → interleaved `f32` → `rtrb` ring buffer → cpal callback. **Three threads**: UI thread (App) · one engine thread that owns the cpal `Stream` *and* the decoder (on macOS the `Stream` is `!Send`, so it is created/used/dropped only there) · cpal's own callback thread (atomics + ring buffer only — never allocates, locks or touches disk). No resampling: the stream opens at the track's own rate/channels; a device that can't is reported as unplayable.
- The UI only ever reads a `PlayerSnapshot`; changes go `Action → App::dispatch → PlayerCommand` into `app::Backend` — `Mock(MockPlayer)` for `--demo`/`--dump`/tests, `Real(RealPlayer)` for real sessions — which talks to the engine thread over `std::sync::mpsc` channels. Disk work (scans) runs on a thread and returns over a channel polled from `App::tick`. Render code never sees either backend.
- Hi-fi matters: preserve lossless/bit-perfect path, avoid resampling or volume normalization unless user opts in. Gapless support is expected.
- No workspace, CI, or lint config yet — don't assume any exists.

## Gotchas

- Ratatui 0.30 requires explicit `Terminal::draw` + crossterm event polling; there is no framework auto-refresh.
- `cpal` is wired up: Linux builds need the ALSA dev headers (`libasound2-dev` on Debian/Ubuntu, `alsa-lib` on Arch); macOS CoreAudio needs no extra setup. The cpal `Stream` is `!Send` on macOS — keep it on the engine thread (`src/audio/engine.rs`).
- Sync (plan 04): the clock is **frames played**, not wall time — the audio callback counts what it really pops into an `AtomicU64` (pausing/buffering freezes it for free), and taps one mono sample per frame into a second small `rtrb` ring. The FFT (`src/audio/analysis.rs`) runs on the UI thread inside `RealPlayer::tick` over that tap — never on the audio thread, which stays atomics-and-ring-buffers only. Seeking reloads the stream at an offset (`Decoder::open_at`); the engine's pause flag survives a `Load`, so every load re-asserts Pause/Resume right after it.
- `cargo test` never opens a sound card; the end-to-end device test is `#[ignore]`d (`cargo test -- --ignored`, it plays an audible tone).
- `Cargo.lock` is committed (binary crate) — keep it that way.
