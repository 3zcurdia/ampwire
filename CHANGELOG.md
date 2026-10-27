# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `--version` / `--help` (and `-V` / `-h`) CLI flags.
- Release distribution: cargo-dist configuration, generated release workflow,
  GitHub Pages site and install wrapper.
- Shell installer only.
- Linux MPRIS2 service (`org.mpris.MediaPlayer2.ampwire`): media keys, Omarchy's
  media widget, lock screen and `playerctl`.
- `--status [--json]` and `--ctl <command>` over an owner-only Unix socket.
- Single-instance guard: a second launch forwards its paths to the running
  session and exits instead of opening a second sound card.

### Changed
- The draw loop is state-driven: 30 fps while playing, 2 fps paused, 1 fps
  stopped/under an overlay, 5 fps during a background load. It still drains
  remote commands every 100 ms.
- A loaded batch sorts by album → disc → track number (plan 12): tracks group
  by their album tag (`<album artist> - <album>`, folder when the tag is
  missing), then disc (missing = 1), then track number. Tracks with no track
  number keep the `artist - title` / path order exactly; Jellyfin pages now
  arrive server-sorted by `AlbumArtist, Album, ParentIndexNumber, IndexNumber,
  SortName`.

## [0.1.0] - unreleased
### Added
- Initial public release.
