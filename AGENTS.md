# Ampwire — terminal Winamp 2.0 replica with modern hi-fi playback

Goal: retro Winamp 2.x UI/feel in the terminal (Ratatui), but with real modern
playback: local lossless files + Jellyfin server streaming.

## Toolchain

- Rust edition 2024, managed via `mise` (`mise.toml`: `rust = "latest"`).
- TUI: `ratatui 0.30` + `crossterm 0.29`. Check those docs first when touching UI/event code — APIs shift between minors.
- Single binary crate. Entrypoint: `src/main.rs` (currently just `println!` stub).

## Commands

- `cargo run` — run the TUI (must use a real TTY; output is garbage under pipes/CI).
- `cargo check --all-targets` — fastest verification loop.
- `cargo clippy --all-targets -- -D warnings` before finishing.
- `cargo fmt --check` / `cargo fmt` — rustfmt is the formatter, no extra config.
- `cargo test` — currently no tests; add unit tests next to modules, integration tests under `tests/`.

## Architecture direction

- Keep audio decode/playback decoupled from the Ratatui render loop (dedicated audio thread/task, message-passing — never block `draw` on I/O or decoding).
- Planned boundaries: `player` (queue + transport state), `audio` (local decode/output, e.g. Symphonia + cpal/rodio — not yet chosen), `jellyfin` (server client/stream URLs), `ui` (Winamp skin: main window, playlist, EQ).
- Hi-fi matters: preserve lossless/bit-perfect path, avoid resampling or volume normalization unless user opts in. Gapless support is expected.
- No workspace, CI, or lint config yet — don't assume any exists.

## Gotchas

- Ratatui 0.30 requires explicit `Terminal::draw` + crossterm event polling; there is no framework auto-refresh.
- macOS audio output needs no extra setup, but Linux targets will need ALSA deps — flag it if adding `cpal`/system audio crates.
- `Cargo.lock` is committed (binary crate) — keep it that way.
