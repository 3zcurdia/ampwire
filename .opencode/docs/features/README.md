# Ampwire plans — index and rules for implementers

Plan `01` (layout) is done. Plans `02`–`08` turn the mock UI into a real player;
`09` (gapless) came out of the 2026-10-23 audit; `10` (native SMB shares) was
finished on 2026-10-25. Plan `11` is release/distribution work (GitHub
Releases, the curl-to-sh installer and the GitHub Pages site), not player
behavior.
**Do them in numeric order.** Each plan lists its prerequisites and assumes
the earlier ones are merged.

| Plan | Status | Feature | Needs |
|---|---|---|---|
| [01](01-winamp-layout.md) | ✅ done | UI shell (mock player, no audio) | — |
| [02](02-playlist-loading.md) | ✅ done | Load files/dirs into the playlist + playlist controls | 01 |
| [03](03-audio-playback.md) | ✅ done | Play wav/mp3/m4a/flac: play, pause, stop, next, prev, **ordered/shuffle play order** (no timer, no spectrum) | 02 |
| [04](04-live-sync.md) | ✅ done | Real timer, seeking, FFT spectrum synced to the audio | 03 |
| [05](05-volume-balance-format.md) | ✅ done | Volume + balance that work, format/bitrate line | 03 (04 recommended) |
| [06](06-equalizer-dsp.md) | ✅ done | Real 10-band equalizer + preamp | 05 |
| [07](07-eq-presets.md) | ✅ done | Presets screen (full bass/treble, jazz, rock, …) | 06 |
| [08](08-jellyfin.md) | ✅ done | Jellyfin server → playlist; temp-dir cache (3 ahead/3 behind) or HTTP streaming, optional EQ bypass | 03, 04, 05, 06 |
| [09](09-gapless.md) | ✅ done | Gapless playback between same-format tracks; device/config cache | 03, 04, 05, 06, 08 |
| [10](10-samba.md) | ✅ done | Native Samba: `smb://` as a file URL (scan, cache or stream, gapless) | 03, 04, 05, 06, 08, 09 |

## Status line — read it before you touch a plan

Every plan file starts with a `**Status:**` line right under its title. It is
the source of truth for whether the work is already done:

- ⬜ `not started` — safe to pick up.
- 🟨 `in progress — <branch/agent>, since <date>` — someone is on it; **do not
  start over**, coordinate first.
- ✅ `done — <date>, <commit>` — **do not re-implement.** Never redo a done
  plan's steps; only fix regressions, and only after `cargo test` shows one.

An agent that has just finished a plan must, **in the same commit that
completes it**:

1. flip the plan's Status to `✅ done — <YYYY-MM-DD>, <short commit hash>`
   (the commit that finished it; if folded into another commit, use the merge
   commit), and
2. set that plan's row in the table above to `✅ done`.

This is what stops the next agent from duplicating finished work — keep both
in sync.

## Rules that apply to EVERY plan

1. **Read first**: the plan's own `**Status:**` line (if it says ✅ done or 🟨
   in progress, **stop** — the effort is already claimed), then `AGENTS.md`,
   `.opencode/docs/features/01-winamp-layout.md` §1, and the
   source files named in the plan's "Files" section. Do not guess APIs: for each
   new crate run `cargo add <crate>`, then read its docs on docs.rs for the
   version Cargo resolved. ratatui/crossterm/symphonia/cpal APIs differ between
   minor versions.
2. **Work in small steps.** After each numbered step run `cargo check --all-targets`.
   Fix errors before moving on. Commit after each green step.
3. **Before declaring done**, all three must pass with no warnings:
   `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
4. **Golden tests** (`tests/golden/*.txt`, `--dump`) must keep passing. They run
   the *mock* player. Never edit a golden file to make a test pass unless the
   plan says the visible output changes; if it does, regenerate with
   `cargo run -- --dump WxH` and review the diff by eye.
5. **Never block the UI thread** on disk, network, decode or audio-device work.
   Heavy work goes on a worker thread and talks back over `std::sync::mpsc`
   (or a lock-free ring buffer for audio samples). The UI only reads
   `PlayerSnapshot` and sends `PlayerCommand`.
6. **No allocation, locking, file/network I/O or `println!` inside the cpal audio
   callback.** Use atomics and ring buffers only.
7. **Hi-fi rule**: no resampling, no normalization, no dithering. When volume is
   100 %, balance is centered and EQ is off/flat, samples must reach the device
   *unchanged* (skip the DSP code entirely, don't multiply by 1.0).
8. Every new user-facing key goes through `src/action.rs` + `src/keymap.rs`
   (never read keys in widgets). Adding an `Action` means editing **all** of:
   the enum, `name()`, `scope()`, `description()`, `all()`, the default bindings
   in `keymap.rs::defaults()`, and the keymap table test. The existing
   "same key bound twice" test will tell you about conflicts.
9. Add unit tests next to the code (`#[cfg(test)] mod tests`). Integration tests
   go in `tests/`.
10. Update `README.md` (keys table, status line) and `AGENTS.md` (architecture)
    when behavior or structure changes. Remove "(stub)" wording from any action
    you make real.
11. If the plan is ambiguous, or you need to deviate from it, **stop and ask**
    instead of inventing behavior.

## Product decisions already made (from the product Q&A)

- Load: CLI args **and** an in-app path prompt. Directories are scanned recursively.
- Playlist controls: add / remove / clear / reorder / find-filter. Default order:
  alphabetical by `artist - title`; untagged files sort by full path.
- Audio: `symphonia` (decode) + `cpal` (output). Auto-advance at end of track;
  unreadable files are skipped with a notice.
- Spectrum: real FFT, log-spaced bars. Timer comes from samples actually played.
- Volume/balance: software gain, saved in `config.toml`.
- Format line is per-codec (lossless: bit depth + rate; lossy: kbps + rate).
- EQ: 10-band biquad + preamp, saved in config. Presets screen is a modal list
  with live preview (Enter applies, Esc reverts).
- Play order: ordered (default) or shuffle (full permutation) via `s`; repeat via `r`.
- Jellyfin: URL + API key in `config.toml`; paged background fetch; original file
  only (never server transcoding); server tracks mixed into the same playlist.
  `mode = cache` (temp dir, 3 tracks ahead + 3 behind, cleaned on exit) /
  `stream` (HTTP Range, EQ still works) / `stream_pure` (stream, EQ bypassed).
