# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1] - 2026-10-29

### Fixed
- The `bit-perfect` marker no longer shows for sources that `f32` cannot hold
  exactly (32-bit integer PCM/FLAC, 64-bit float PCM); the docs now say that on
  Linux a PipeWire/Pulse/`dmix` default device can still resample after Ampwire.

### Changed
- Idle CPU is lower across the board: the audio engine thread now blocks on its
  command channel when nothing plays (100 ms waits while paused) instead of
  waking every 5 ms, and the control socket blocks in `accept` instead of
  waking about 50 times a second.
- Playlist tracks are shared per track (`Arc<Track>`), so loading a large
  Jellyfin or SMB library and mid-playlist edits no longer clone the whole
  track list; the playlist footer's total duration is memoized and the
  Jellyfin/SMB cache windows now share one implementation.
- The spectrum FFT runs once per drawn frame rather than per wake-up, snapshot
  spectrum bars and format info no longer allocate, the `--status`/MPRIS view
  publishes at most every 100 ms, and the Omarchy theme check only redraws when
  the theme actually changed.
- Play-order look-ahead/look-behind became lazy iterators, so the cache windows
  stop after the few tracks they need, and decode packets now go straight into
  the output buffer without a scratch copy.

### Added
- `docs/architecture.md`: a full description of the thread model, module
  boundaries and playback pipeline.

## [0.1.0] - 2026-10-27

First public release: a Winamp-2-inspired terminal player (player, equalizer,
playlist) written from scratch in Rust with Ratatui, with modern hi-fi
playback — local files, Jellyfin streaming and native SMB shares.

### Added
- Hi-fi playback: no transcoding, no resampling, no normalization. The output
  stream opens at the track's own sample rate and channel count; a device that
  cannot play that format is reported, not converted. Lossless sources show a
  `bit-perfect` marker whenever the DSP stage is fully bypassed.
- Formats: FLAC, WAV/PCM, M4A (AAC or ALAC) and MP3, decoded with Symphonia and
  played through cpal — one local decode path for bytes from disk, Jellyfin or
  SMB.
- Gapless playback between tracks that share a sample rate and channel count:
  the next decoder is pre-opened and swapped into the running stream at EOF —
  same ring, same DSP/EQ state, no silence, no click, no device reopen.
- 10-band equalizer (peaking biquads + preamp, ±12 dB) with an on-screen curve,
  live presets picker (`p`) and `[eq]` persistence; works in every source mode,
  including while streaming.
- Volume, balance and channel swap with click-free smoothing in the audio
  callback, persisted in `[audio]` and restored on startup.
- Live sync: the LCD clock, seek bar and spectrum follow what is actually being
  heard — the clock counts frames the audio callback delivered, `←`/`→` seek by
  reloading the stream at that offset, and the bars are a 2048-point Hann FFT
  in 32 log-spaced bands (40 Hz → 16 kHz).
- Play order: ordered (default) or full-permutation shuffle (`s`), repeat
  (`r`); editing the playlist mid-shuffle keeps the shuffle state.
- Jellyfin streaming: `j` progressively loads the whole server library in the
  background while the playlist stays usable, with `cache` (rolling 3-ahead /
  3-behind window), `stream` (seekable HTTP `Range`) and `stream_pure` (EQ
  bypassed) modes; always the original file (`static=true`), never transcoded,
  and the API key never appears in URLs, errors or logs.
- Native SMB2/3 shares: `smb://server/share/path` is accepted anywhere a path
  is — a pure-Rust client, no kernel mount, no `libsmbclient`. Directory URLs
  scan recursively (tags read over the wire, progress every 50 files) into
  `cache` or `stream` mode; credentials come from `[smb]` only, never the URL.
- Linux MPRIS2 service (`org.mpris.MediaPlayer2.ampwire`): media keys, Omarchy's
  media widget, lock screen and `playerctl`.
- `--status [--json]` and `--ctl <command>` over an owner-only Unix socket;
  `--status --json` reports the format and bit-perfect state for bar widgets.
- Single-instance guard: a second launch forwards its paths to the running
  session and exits instead of opening a second sound card.
- `--version` / `--help` (and `-V` / `-h`) CLI flags, `--demo` mock player and
  `--dump WxH` frame renderer (with golden-frame tests).
- Failures never stop the run: a bad file shows `skipped <name>: <reason>` and
  playback moves on; if every track fails, the reason names the output device.
- Idle-CPU draw loop: 30 fps while playing, 2 fps paused, 1 fps stopped or under
  an overlay, 5 fps during a background load — while still draining remote
  commands every 100 ms.
- Release distribution: cargo-dist configuration, generated release workflow,
  GitHub Pages site and shell installer (macOS Apple Silicon/Intel, Linux
  x86_64/arm64).

### Changed
- A loaded batch sorts by album → disc → track number: tracks group by their
  album tag (`<album artist> - <album>`, folder when the tag is missing), then
  disc (missing = 1), then track number. Tracks with no track number keep the
  `artist - title` / path order exactly; Jellyfin pages now arrive
  server-sorted by `AlbumArtist, Album, ParentIndexNumber, IndexNumber,
  SortName`.

[Unreleased]: https://github.com/3zcurdia/ampwire/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/3zcurdia/ampwire/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/3zcurdia/ampwire/releases/tag/v0.1.0
