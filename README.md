# Ampwire

A terminal player **inspired by Winamp 2**, implemented from scratch for the terminal — with modern hi-fi playback.

It borrows Winamp 2's anatomy — player, equalizer, playlist — rendered as a keyboard-driven Ratatui TUI in a btop-style cyberpunk-retro look. It is *not* a port, clone or replica of Winamp: the UI, the playback engine and the library handling are all written for the terminal from scratch.

> Status: Phase 6 — **Jellyfin.** Launch `ampwire ~/Music` (or press `l`) to load lossless/MP3 files with tags and actually hear them: play/pause/stop/next/prev, auto-advance at the end of a track — **gapless** between same-format tracks (plan 09), ordered or shuffled play order (`s`), repeat (`r`), and unreadable files skipped with a notice. The LCD clock, seek bar (`←`/`→`, ±5 s) and the 32-band FFT spectrum follow the audio that is really playing — pausing freezes them, stopping zeroes them. Volume (`+`/`-`/`↑↓`) and balance (`[`/`]`) shape what you hear and are remembered across restarts; at 100 % / centered the samples stay bit-perfect. The format line reads `FLAC 24/96 stereo` / `MP3 320kbps 44.1kHz stereo`. With a `[jellyfin]` section in the config, `j` fetches your **whole server library** into the playlist (progressively, while the UI stays responsive) and plays it — downloaded to a rolling temp-dir cache, or streamed over HTTP `Range` requests (`stream_pure` bypasses the EQ for that path). `--demo` / `--dump` keep the mock player.

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

## Platform support

| Platform | Status | Notes |
|---|---|---|
| **Linux** | ✅ works | Audio via ALSA — build needs the ALSA development headers (`libasound2-dev` on Debian/Ubuntu, `alsa-lib` on Arch). |
| **macOS** | ✅ works | Audio via CoreAudio, no extra setup. |
| **Windows** | ⚠️ **untested** | The code has never been built or run on Windows, so it may not even compile there. Treat it as unsupported for now. |

Only Linux and macOS are supported today. Windows is **not tested** — the project has never been built or run there, so it may not even compile, and the audio path is unverified. If you try it, expect to have to fix: the cpal/WASAPI output device (the device + `supported_output_configs()` cache and the exact-rate matching are written for the ALSA/CoreAudio backends), the device names in error notices, and the playback rule itself — Windows shared-mode devices usually expose only a couple of sample rates (44.1/48 kHz), and ampwire never resamples, so anything else is reported as unplayable. The POSIX-specific bits (config file mode `0600`, Jellyfin cache dir mode `0700`) are `cfg(unix)`-guarded, so they compile everywhere but are a no-op off unix.

## Playback

- Formats: WAV/PCM, FLAC, MP3, M4A (AAC or ALAC) — decoded with Symphonia and played through cpal **at the track's own sample rate and channel count** (no resampling; lossless sources show `bit-perfect`).
- Play order: ordered (default) or full-permutation shuffle (`s` — every track plays once per round), repeat (`r`). Tracks auto-advance; the last track stops, or wraps to track 1 with repeat on. `z`/`b` follow the same rule at the end of the playlist. Editing the playlist mid-shuffle keeps the shuffle: added tracks are mixed into what has not played yet, removed ones drop out, moves change nothing (only `s` itself, `l` and clearing reshuffle).
- Gapless (plan 09): when the next track in play order has the **same sample rate and channel count**, the engine pre-opens it (`EngineCmd::Queue`, right after the current track starts and on every order change) and swaps decoders *inside the running stream* at EOF — same ring, same DSP/EQ state, no silence, no device reopen, no click. The clock, title and format line switch exactly when the audio crosses the recorded boundary frame, never when the (earlier) engine event arrives. A format change, a repeat wrap and end-of-order keep the old path: the device is reopened (a short gap), from a **cached** device + config query that is only re-run after a device error or a config miss. In `stream*` modes the next HTTP source is probed ahead of time (hiding its round trip); `stream_pure`'s EQ bypass flips with the queued track, at the boundary.
- Live sync: the LCD clock, seek bar and spectrum follow what is actually being heard. The clock counts frames the audio callback delivered (no drift, freezes on pause), `←`/`→` seek ±5 s by reloading the stream at that offset (a brief glitch; unsupported files restart with `seek not supported for this file`), and the bars are a 2048-point Hann FFT over the played samples in 32 log-spaced bands (40 Hz → 16 kHz).
- Failures never stop the run: a bad file shows `skipped <name>: <reason>` and playback moves on; if every track fails → `no playable tracks: <last reason>` (the reason names the output device, e.g. `External Headphones: cannot play 96000 Hz 2ch`).
- Volume & balance are real (plan 05): software gain in the audio callback with click-free smoothing, volume 0–100 (squared law, default 100 = unity), balance −100…+100 (attenuates only the opposite channel, stereo only). Both live in `config.toml` `[audio]` and are restored on startup, together with `swap_channels` (L/R to opposite ears — for headsets/DACs with reversed wiring; it fixes the balance direction and the stereo image). At volume 100 / balance 0 / swap off the DSP is skipped entirely — lossless stays bit-perfect, as the format line's `bit-perfect` marker shows.
- The format line is per-codec: `FLAC 24/96 stereo` (lossless: depth + rate) vs `MP3 320kbps 44.1kHz stereo` (lossy: bitrate + rate, bitrate from tags). Codec/depth/rate/channels come from the decoder, never the tags.
- Not yet: resampling for device-unsupported rates (such a track is skipped with `<device>: cannot play 48000 Hz 8ch` — ampwire plays only at the device's own rates, no fallback to a second device). Gapless *is* in, between same-format tracks only — see the bullet above.
- Jellyfin (plan 08): with `[jellyfin]` configured, `j` loads every audio track on the server into the playlist (appended to what is there; pressing `j` again first drops the previous remote batch). Loading pages the server 500 items at a time in the background — the playlist stays fully usable — and keeps the server's `Artist, SortName` order. Remote tracks play like local files in one of three `mode`s:
  - **`cache` (default)** — originals are downloaded to a rolling temp-dir window of **3 remote tracks ahead and 3 behind** the current one (in *play* order, so shuffle is respected) and played from disk: seek, EQ, volume, balance and the spectrum all work unchanged.
  - **`stream`** — the original file streams over HTTP `Range` requests (seekable, nothing stored). **The EQ works while streaming**: it is local DSP on the decoded samples, it doesn't care where the bytes came from. The engine's ring buffer doubles to ≈2 s to survive network hiccups.
  - **`stream_pure`** — the same streaming, but the **EQ is bypassed** while a Jellyfin track plays (`stream pure: eq bypassed`, once per session), so the only thing between the server's bytes and the speakers is the decoder (plus volume/balance if you moved them). The EQ panel is untouched and applies again on local tracks.
  - A read stall in `stream*` can block Stop/Next for up to the 15 s read timeout — **use cache mode on slow links**. Never transcoding: ampwire always asks for the original file (`static=true`) and decodes locally.

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

Winamp-inspired defaults, Space for play/pause. Every action is remappable. Since plan 03 these are **real**: `z x c v b` / `Space` drive the audio engine, `s` / `r` toggle shuffle / repeat with a `shuffle on`-style notice, `l` opens a path for real. Plan 04 made `←` / `→` (seek ±5 s) live — the clock and bars follow the audio; plan 05 made volume (`+` `-`) and balance (`[` `]`) shape the actual samples; plan 06 made the EQ real — `↑` / `↓` in the EQ panel actually reshapes the sound; plan 08 made `j` fetch the whole Jellyfin library in the background.

| Key | Action | Scope |
|---|---|---|
| `Space` | play / pause | global |
| `z` `x` `c` `v` `b` | prev / play / pause / stop / next | global |
| `l` | open: replace playlist from a path | global |
| `j` | load the whole Jellyfin library (`[jellyfin]` required) | global |
| `s` / `r` | shuffle / repeat | global |
| `←` / `→` | seek ∓5s | player, playlist |
| `←` / `→` | select EQ band | eq |
| `↑` / `↓` | volume | player |
| `↑` / `↓` | cursor | playlist |
| `↑` / `↓` | band gain ±1 dB | eq |
| `o` | equalizer on / off | eq |
| `0` | reset equalizer to flat | eq |
| `p` (`Esc` cancels, `Enter` applies, `p` too) | EQ preset picker (`↑`/`↓` preview live) | global |
| `+` `=` / `-` | volume | global |
| `[` / `]` | balance left / right | global |
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
preset = "omarchy"   # "omarchy" (default: follows the OS theme, live-reloads) | "neon" | "amber" | "matrix" | "synthwave"
transparent = true   # false → paint panel background

[theme.colors]       # optional per-role overrides
accent = "#00E5FF"

[audio]
volume = 100               # 0–100, default 100 = unity (bit-perfect)
balance = 0                # −100 (full left) … +100 (full right)
swap_channels = false      # true → L/R to opposite ears (reversed-wiring headsets/DACs)

[eq]                       # 10-band equalizer + preamp (dB, −12…+12); flat + on = bit-perfect
on = true
preamp = 0
bands = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]  # 60, 170, 310, 600, 1k, 3k, 6k, 12k, 14k, 16k Hz

[jellyfin]                 # optional: `j` loads the whole server library
url = "http://192.168.1.10:8096"  # trailing slash is trimmed
api_key = "0123456789abcdef"      # Jellyfin dashboard → API key; header only, never in a URL
# api_key_file = "jellyfin.key"   # …or keep the key out of this file (relative = next to config.toml);
#                                 #    AMPWIRE_JELLYFIN_API_KEY in the environment beats both
user = "luis"              # optional; the first user is used if omitted
mode = "cache"             # "cache" (download) | "stream" (HTTP Range) | "stream_pure" (stream, EQ bypassed)
cache_ahead = 3            # cache mode: remote tracks to prefetch ahead (0–10)
cache_behind = 3           # cache mode: …and to keep behind the current one
# cache_dir = "/var/tmp"   # optional: parent of ampwire-jellyfin-<pid> (default: system temp dir)

[keys]
next = ["n"]
```

- Load never crashes: missing/invalid config falls back to defaults with a one-line notice in the playlist border.
- Saves use `toml_edit` (comments and formatting survive), written atomically via temp-file + rename.
- Volume/balance/EQ changes are saved 500 ms after the last keypress (and on quit); layout hotkeys save immediately.
- If the on-disk config failed to parse, the session is read-only: layout hotkeys still work in memory but nothing is written back (`config invalid — not saving`).
- `Config::save` never touches `[jellyfin]` (the key is yours), and the section is never printed — its `Debug` output shows `api_key: "<redacted>"`, and the key only ever travels as the `Authorization` header. **Recommend `chmod 600 config.toml`** — the file holds your API key; saves keep the file's permissions. To keep the key out of the auto-saved file entirely, use `api_key_file` or `AMPWIRE_JELLYFIN_API_KEY` (neither is ever written back). A config parse error is reported as `line N, column M: <message>` — never the offending line itself. An unknown `mode` falls back to `cache` with a notice instead of failing the config.
- Jellyfin cache location & cleanup: `cache` mode downloads into `ampwire-jellyfin-<pid>/` (mode `0700`) under the system temp dir — or under `cache_dir` if you set it. The directory is always freshly created by Ampwire (anything already at that name — a leftover or a planted symlink — is removed first, never reused). In `stream*` modes the same directory is created only if a server ignores `Range` and a track has to be downloaded; that download runs in the background (Stop/Next/quit never wait for it). On exit the directory is deleted; leftovers older than 24 h from crashed runs are swept at the next startup in every mode, and the OS also cleans its temp dir. Server item ids are validated (letters, digits, `-`, `_`), so a server cannot steer a file name. Downloads land in `<item>.part` and are renamed only when complete — a `.part` file is never decoded. The window holds at most `1 + cache_ahead + cache_behind` remote files (each capped at 512 MiB).

## Theming

Widgets use semantic roles (`bg`, `fg`, `primary`, `glow`, `accent`, `muted`, `warn` + spectrum gradient), never raw colors. Presets:

- `omarchy` (default): auto-follow the active Omarchy theme (`~/.local/state/omarchy/current/theme/colors.toml`, fallback `~/.config/omarchy/current/theme`), re-checked once per second. Missing/unreadable → silent fallback to `neon`. `AMPWIRE_OMARCHY_DIR` overrides the path for tests.
- `neon` (fallback): neon orange primary, aqua accent, transparent bg
- `amber`: single-color amber CRT
- `matrix`: green-on-black
- `synthwave`: magenta + cyan

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
- Audio on Linux builds against ALSA (`libasound2-dev` on Debian/Ubuntu, `alsa-lib` on Arch); macOS needs nothing. **Windows is untested** — the suite has never been run there and the WASAPI path is unverified, so treat Windows as unsupported (see [Platform support](#platform-support)). `cargo test` never opens a sound card — the end-to-end device test is `#[ignore]`d: `cargo test -- --ignored` (it plays an audible tone). Plain `cargo test` also stays off the network (its "HTTP server" is a `TcpListener` on `127.0.0.1:0`).
- The Jellyfin path has its own scripted "manual" test against a **real** server — it downloads a cache window, streams + decodes over `Range`, and plays one remote track through the engine (audible for a moment): `AMWIRE_JELLYFIN_URL=… AMWIRE_JELLYFIN_API_KEY=… cargo test -- --ignored jellyfin_real_server`.
- Silent playback on a device? `cargo run --example devices` lists every output device with the channel counts and sample rates it advertises — ampwire plays a track only at an *exact* match on the default device (no resampling, no fallback), so a `no playable tracks: <device>: cannot play 96000 Hz 2ch` notice means the file's rate isn't in that list.

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
    engine.rs        engine thread: owns the cpal Stream + decoder, feeds the rtrb ring, gapless Queue/switch + device cache
    dsp.rs           SharedParams (lock-free targets) + Dsp: smoothed volume/balance + the EQ stage
    eq.rs            10-band peaking EQ (RBJ cookbook biquads + preamp), f64 state, interleaved f32
  library/
    mod.rs           audio-file scan (walkdir) + tag reading (lofty) on a thread
  jellyfin/
    mod.rs           Jellyfin client (ureq): header auth, paged library load, item → Track
    cache.rs         rolling temp-dir window: downloader thread, evict, compute_window, sweep
    http_source.rs   seekable HTTP Range MediaSource (+ whole-file fallback for non-range servers)
    test_server.rs   #[cfg(test)] mock Jellyfin: real HTTP + Range over tests/fixtures/tone.flac
  player/
    mod.rs           PlayerSnapshot view model + PlayerCommand + Track + FormatInfo
    eq_presets.rs    10 named EQ presets (preamp + 10 gains) + find_matching
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
    eq_presets.rs    p-key preset picker overlay (live preview, esc cancel)
    playlist.rs      playing/cursor styles, scroll, scrollbar, totals, filter/prompt
    help.rs          ? overlay
    widgets/         lcd_digits, marquee, spectrum, slider, eq_curve
tests/fixtures/      0.5 s 44.1 kHz stereo tones (wav/flac/mp3/aac/alac) for decoder tests
tests/golden/        --dump snapshots at 5 sizes
docs/plans/          design plans (01-winamp-layout.md, 02-playlist-loading.md, …)
```

Data flow: `Key → keymap::resolve(focus) → App::dispatch(Action) → app::Backend → PlayerCommand → MockPlayer | RealPlayer → (engine thread → rtrb ring buffer → cpal callback)`. Loads go `path → library::spawn_scan (thread) → ScanMsg → App::tick → PlayerCommand`; a Jellyfin load goes `j → jellyfin::spawn_library_load (thread) → JfMsg → App::tick → RemoveRemote (once) + AppendInOrder`, and remote playback goes `TrackSource::Remote → Cache window (downloader thread) → Load` or `→ EngineCmd::LoadRemote → HttpRangeSource`. Engine events come back over a channel polled from `App::tick` (notices land in the playlist border). `ui` only reads `&PlayerSnapshot`, `&UiState`, `&Theme`. Widgets are stateless borrowed structs implementing `Widget`.

## Roadmap

Done: playlist loading + editing (plan 02), real playback with ordered/shuffle play order (plan 03), live clock/seeking/FFT spectrum synced to the audio (plan 04), volume/balance DSP + per-codec format line (plan 05), 10-band equalizer + preamp that really shapes the sound (plan 06), EQ presets screen with live preview (plan 07), Jellyfin server → playlist with cache / stream / stream_pure modes (plan 08), gapless playback between same-format tracks + device/config cache (plan 09). Still out of scope: mouse, media keys / MPRIS / Now Playing.
