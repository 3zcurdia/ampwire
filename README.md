# Ampwire

Terminal Winamp 2.0 replica with modern hi-fi playback.

Retro Winamp anatomy — player, equalizer, playlist — rendered as a keyboard-driven Ratatui TUI in a btop-style cyberpunk-retro look.

> Status: Phase 4 — **live sync.** Launch `ampwire ~/Music` (or press `l`) to load lossless/MP3 files with tags and actually hear them: play/pause/stop/next/prev, auto-advance at the end of a track, ordered or shuffled play order (`s`), repeat (`r`), and unreadable files skipped with a notice. The LCD clock, seek bar (`←`/`→`, ±5 s) and the 32-band FFT spectrum follow the audio that is really playing — pausing freezes them, stopping zeroes them. Volume/balance are stored but not yet applied (plan 05), no Jellyfin yet. `--demo` / `--dump` keep the mock player.

## Quickstart

Requirements: Rust (managed via `mise`, see `mise.toml`), a real TTY, and — for audio output on Linux — the ALSA development headers (`libasound2-dev` on Debian/Ubuntu, `alsa-lib` on Arch). macOS plays through CoreAudio with no extra setup.

```sh
cargo run -- ~/Music   # load a real playlist (files or folders) and start playing
cargo run              # start empty: press `l` to open a file or folder
cargo run -- --demo    # mock playlist + default layout, ignores your config
cargo run -- --dump 104x18 # render one frame to stdout, no TUI (for tests / preview)
cargo run -- --dump 104x18 --frame 10  # advance mock clock N frames before dump
```

`--dump` never enters raw mode or the alternate screen. Useful sizes used in tests: `40x3`, `52x12`, `80x24`, `104x18`, `160x45` (see `tests/golden/`).

## Playback

- Formats: WAV/PCM, FLAC, MP3, M4A (AAC or ALAC) — decoded with Symphonia and played through cpal **at the track's own sample rate and channel count** (no resampling; lossless sources show `bit-perfect`).
- Play order: ordered (default) or full-permutation shuffle (`s` — every track plays once per round), repeat (`r`). Tracks auto-advance; the last track stops, or wraps to track 1 with repeat on. `z`/`b` follow the same rule at the end of the playlist.
- Live sync: the LCD clock, seek bar and spectrum follow what is actually being heard. The clock counts frames the audio callback delivered (no drift, freezes on pause), `←`/`→` seek ±5 s by reloading the stream at that offset (a brief glitch; unsupported files restart with `seek not supported for this file`), and the bars are a 2048-point Hann FFT over the played samples in 32 log-spaced bands (40 Hz → 16 kHz).
- Failures never stop the run: a bad file shows `skipped <name>: <reason>` and playback moves on; if every track fails → `no playable tracks`.
- Not yet: volume/balance (plan 05), gapless playback, and resampling for device-unsupported rates (such a track is skipped with `device cannot play 48000 Hz 8ch`).

## Layout

Two columns by default: player + EQ stacked in a fixed 52-column left column, playlist filling the rest. Single-column mode stretches everything full-width.

Hotkeys (all saved to config immediately):

- `1` / `2` / `3` — show/hide player, EQ, playlist
- `Shift+L` — toggle 1 ↔ 2 columns
- `Shift+O` — cycle panel order (playlist left/right + stack swap in 2-col, rotation in 1-col)
- `Tab` / `Shift+Tab` — focus next/previous visible panel

Automatic fallbacks (never overwrite your saved preference):

1. Width < 92 cols → force single column
2. Short terminal → shrink playlist to 5 rows, then hide EQ
3. Very short → player drops to 3-row compact form (time + marquee + seek)
4. Below 40×3 → `terminal too small (need 40×3)`

## Keys

Winamp-style defaults, Space for play/pause. Every action is remappable. Since plan 03 these are **real**: `z x c v b` / `Space` drive the audio engine, `s` / `r` toggle shuffle / repeat with a `shuffle on`-style notice, `l` opens a path for real. Since plan 04 so are `←` / `→` (seek ±5 s) — the clock and bars follow the audio.

| Key | Action | Scope |
|---|---|---|
| `Space` | play / pause | global |
| `z` `x` `c` `v` `b` | prev / play / pause / stop / next | global |
| `l` | open: replace playlist from a path | global |
| `s` / `r` | shuffle / repeat | global |
| `←` / `→` | seek ∓5s | player, playlist |
| `←` / `→` | select EQ band | eq |
| `↑` / `↓` | volume | player |
| `↑` / `↓` | cursor | playlist |
| `↑` / `↓` | band gain ±1 dB | eq |
| `+` `=` / `-` | volume | global |
| `Enter`, `PgUp`/`PgDn`, `Home`/`End` | play under cursor, page, jump | playlist |
| `a` / `d` / `D` | add path / delete track / clear playlist | playlist |
| `/` | find (live filter; `Esc` clears) | playlist |
| `Shift+↑` / `Shift+↓` | move track up / down | playlist |
| `Esc` | clear playlist filter | playlist |
| `Ctrl+T` | elapsed ↔ remaining time | global |
| `?` (`Esc` closes, `↑`/`↓` scroll) | key-help overlay from live keymap | global |
| `q`, `Ctrl+C` | quit | global |

Remap in config:

```toml
[keys]
play_pause = ["space"]
next = ["b", "n"]            # multiple keys per action
toggle_columns = ["shift+l"] # or "scope:key", e.g. "global:+"
```

Spec syntax: `space`, `enter`, `esc`, `tab`, `backtab`, `up/down/left/right`, `home`, `end`, `pgup`/`pgdn`, `f1`–`f5`, single chars, `shift+x`, `ctrl+x`, `alt+x`. Uppercase `L` means `shift+l`. Conflicting remaps are reported in the playlist status line and both actions revert to defaults (swapping two keys is allowed).

## Configuration

Location: `$XDG_CONFIG_HOME/ampwire/config.toml`, fallback `~/.config/ampwire/config.toml` (macOS included).

```toml
[layout]
columns = 2
playlist_side = "right"              # "left" | "right"
stack = ["player", "eq"]
single_order = ["player", "eq", "playlist"]
visible = ["player", "eq", "playlist"]

[theme]
preset = "neon"      # "neon" | "amber" | "matrix" | "synthwave" | "omarchy"
transparent = true   # false → paint panel background

[theme.colors]       # optional per-role overrides
accent = "#00E5FF"

[keys]
next = ["n"]
```

- Load never crashes: missing/invalid config falls back to defaults with a one-line notice in the playlist border.
- Saves use `toml_edit` (comments and formatting survive), written atomically via temp-file + rename.
- If the on-disk config failed to parse, the session is read-only: layout hotkeys still work in memory but nothing is written back (`config invalid — not saving`).

## Theming

Widgets use semantic roles (`bg`, `fg`, `primary`, `glow`, `accent`, `muted`, `warn` + spectrum gradient), never raw colors. Presets:

- `neon` (default): neon orange primary, aqua accent, transparent bg
- `amber`: single-color amber CRT
- `matrix`: green-on-black
- `synthwave`: magenta + cyan
- `omarchy`: auto-follow the active Omarchy theme (`~/.local/state/omarchy/current/theme/colors.toml`, fallback `~/.config/omarchy/current/theme`), re-checked once per second. Missing/unreadable → silent fallback to `neon`. `AMPWIRE_OMARCHY_DIR` overrides the path for tests.

Color depth: non-truecolor terminals get nearest 256-color mapping; 16-color terminals map to ANSI slots.

## Development

```sh
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo test
```

Notes:

- Run the TUI only in a real TTY; output under pipes/CI is garbage.
- `crossterm` is imported as `ratatui::crossterm` — no direct dependency.
- `Cargo.lock` is committed (binary crate).
- Audio on Linux builds against ALSA (`libasound2-dev` on Debian/Ubuntu, `alsa-lib` on Arch); macOS needs nothing. `cargo test` never opens a sound card — the end-to-end device test is `#[ignore]`d: `cargo test -- --ignored` (it plays an audible tone).

## Project structure

```text
src/
  main.rs            --demo / --dump / CLI load paths, theme resolution, ratatui::run
  app.rs             App: UiState + snapshot, event loop (~30fps), tick, dispatch, prompts, scans, Backend{Mock,Real}
  action.rs          Action enum (every remappable command)
  keymap.rs          defaults, spec parsing, (scope, key) → Action
  config.rs          load/save config.toml, XDG paths
  audio/
    mod.rs           3-thread overview: UI thread · engine thread · cpal callback
    decoder.rs       symphonia → interleaved f32 (unit-tested on tests/fixtures/)
    engine.rs        engine thread: owns the cpal Stream + decoder, feeds the rtrb ring
  library/
    mod.rs           audio-file scan (walkdir) + tag reading (lofty) on a thread
  player/
    mod.rs           PlayerSnapshot view model + PlayerCommand + Track + FormatInfo
    playlist.rs      Playlist: append_sorted, remove, reorder, current bookkeeping
    order.rs         PlayOrder: ordered/shuffle permutation (next, prev, jump, look-ahead)
    real.rs          RealPlayer: playlist + play order + AudioOut engine seam
    rng.rs           xorshift RNG shared by the mock's wobble and shuffle
    mock.rs          MockPlayer: fake playlist, ticking clock, spectrum bins
  ui/
    mod.rs           render(frame, &App)
    layout.rs        pure compute_layout(area, prefs, visibility)
    theme/           roles, presets, 256/16-color downgrade, omarchy follow
    chrome.rs        rounded panel + superscript hotkey label + bottom hints
    player.rs        full + compact variants
    equalizer.rs     11 VSliders + braille curve
    playlist.rs      playing/cursor styles, scroll, scrollbar, totals, filter/prompt
    help.rs          ? overlay
    widgets/         lcd_digits, marquee, spectrum, slider, eq_curve
tests/fixtures/      0.5 s 44.1 kHz stereo tones (wav/flac/mp3/aac/alac) for decoder tests
tests/golden/        --dump snapshots at 5 sizes
docs/plans/          design plans (01-winamp-layout.md, 02-playlist-loading.md, …)
```

Data flow: `Key → keymap::resolve(focus) → App::dispatch(Action) → app::Backend → PlayerCommand → MockPlayer | RealPlayer → (engine thread → rtrb ring buffer → cpal callback)`. Loads go `path → library::spawn_scan (thread) → ScanMsg → App::tick → PlayerCommand`; engine events come back over a channel polled from `App::tick` (notices land in the playlist border). `ui` only reads `&PlayerSnapshot`, `&UiState`, `&Theme`. Widgets are stateless borrowed structs implementing `Widget`.

## Roadmap

Done: playlist loading + editing (plan 02), real playback with ordered/shuffle play order (plan 03), live clock/seeking/FFT spectrum synced to the audio (plan 04). Next: volume, balance and EQ applied to the samples (plan 05). Still out of scope: Jellyfin client, mouse, media keys / MPRIS / Now Playing.
