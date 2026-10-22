# Plan 02 — Load playlist from files/directories + playlist controls

**Status:** ✅ done — 2026-10-22. Implemented in this working tree (uncommitted);
180 tests pass, goldens unchanged. See **Deviations** at the end.

Goal: the user can launch `ampwire ~/Music` (or press `l` / `a` in the app and
type a path) and get a real playlist read from disk. Each row shows
`artist - title (m:ss)`, or `filename (m:ss)` when tags are missing. The
playlist can be edited (add, delete, clear, reorder) and filtered.

**Still no audio.** The `MockPlayer` keeps faking playback (fake clock) over the
real playlist. Real sound arrives in plan 03.

Prerequisites: plan 01 merged (it is).

## Decisions

| Topic | Decision |
|---|---|
| Supported extensions | `wav`, `mp3`, `m4a`, `flac` (case-insensitive). Everything else is ignored. |
| Directory scan | Recursive. Do not follow symlinks. Hidden files are included. |
| Tag reading | Crate `lofty` (tags + duration, no decoding). Needs only `lofty::read_from_path`. |
| Display name | Both `artist` and `title` tags present → `artist - title`. Otherwise → file name **without extension** (e.g. `01 intro`). |
| Duration | `Some(d)` from lofty, else `None`; shown as `--:--`. |
| Default order | New tracks are sorted among themselves by `sort_key` (below) and **appended after** whatever is already in the list. Manual reordering is therefore never undone by a later add. |
| `sort_key` | Both tags present → `"{artist} - {title}"` lowercased. Otherwise the full path as a string, lowercased. |
| `l` (Open) | Prompt for a path, **replace** the playlist, start playing track 1. |
| `a` (Add, playlist focus) | Prompt for a path, **append** to the playlist, do not start playing. |
| CLI args | `ampwire PATH...` behaves like Open with all the paths. |
| Persistence | The playlist is **not** saved between runs (out of scope). |

## Files you will touch

`Cargo.toml`, `src/main.rs`, `src/app.rs`, `src/action.rs`, `src/keymap.rs`,
`src/player/mod.rs`, `src/player/mock.rs`, `src/ui/playlist.rs`, `src/ui/mod.rs`,
`README.md`. New: `src/library/mod.rs`, `src/player/playlist.rs`.

## Steps

### 1. Dependencies
`cargo add lofty walkdir`. (`tempfile` is already a dev-dependency.)

### 2. Enrich `Track` (`src/player/mod.rs`)
Replace the struct with:

```rust
pub type TrackId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackSource {
    Local(std::path::PathBuf),
    // plan 08 adds `Remote { .. }`
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub id: TrackId,            // unique per process, from a static AtomicU64
    pub source: TrackSource,
    pub artist: Option<String>,
    pub title: Option<String>,
    pub duration: Option<Duration>,
}
```

Add `impl Track`:
- `Track::next_id() -> TrackId` (a `static NEXT: AtomicU64`, `fetch_add(1, Relaxed)`).
- `display_name(&self) -> String` — rule in the Decisions table. For `Local(path)` use
  `path.file_stem()`; if even that is missing use `"?"`.
- `sort_key(&self) -> String` — rule in the Decisions table.

Fix every compile error: run `grep -rn "Track {" src` and update each use.
`MockPlayer::fake_tracks()` must now build tracks by splitting the existing
strings on `" - "` into `artist` and `title`, with
`source: TrackSource::Local(PathBuf::from(format!("/mock/{raw}.flac")))`.
**The rendered text must stay identical** so golden files do not change.

### 3. `Playlist` type (`src/player/playlist.rs`, new)
Move playlist ownership out of `MockPlayer` into this struct so plan 03's real
player can reuse it:

```rust
pub struct Playlist {
    tracks: Arc<Vec<Track>>,   // Arc so snapshots are O(1) (see step 4)
    current: Option<usize>,
}
```

Methods (all pure, no I/O):
- `new()`, `tracks(&self) -> &Arc<Vec<Track>>`, `len()`, `current()`, `set_current(Option<usize>)`.
- `append_sorted(&mut self, new: Vec<Track>)` — stable-sort `new` by `sort_key`, then push.
  Use `Arc::make_mut`.
- `remove(&mut self, i) -> bool` — returns `true` if the removed track was the current one.
  Adjust `current`: removed index < current → `current - 1`; removed == current → `None`.
- `clear()` — empties the list, `current = None`.
- `move_up(&mut self, i) -> Option<usize>` / `move_down(...)` — swap with neighbour,
  return the new index of the moved track (`None` at the edge). Keep `current` pointing at the same track.
- `replace(&mut self, new: Vec<Track>)` — `clear()` then `append_sorted`.

Unit tests for each method, especially the `current` bookkeeping.

### 4. Snapshot: stop cloning the playlist 30 times a second
In `PlayerSnapshot` change `tracks: Vec<Track>` to `tracks: Arc<Vec<Track>>`
(`Default` still works). `Playlist::tracks()` returns the `Arc`; the snapshot clones the `Arc` only.
Update readers (`ui/playlist.rs`, `app.rs`, tests): `.tracks.len()`, `.tracks.get(i)` and iteration still work through `Deref`.

### 5. Commands
In `PlayerCommand` **remove `Copy`** (keep `Clone`) and add:

```rust
AddTracks(Vec<Track>),        // append_sorted
ReplaceTracks(Vec<Track>),    // replace, then behave like PlayTrack(0) if non-empty
RemoveTrack(usize),
ClearPlaylist,
MoveTrack { from: usize, up: bool },   // up=true → move_up
```
Fix call sites (`self.player.handle(cmd)` takes ownership already; add `.clone()` only where the compiler asks).

`MockPlayer`: replace its `tracks`/`current` fields with a `Playlist`. Implement the
new commands by calling `Playlist` methods. `RemoveTrack` of the current track →
`state = Stopped`, `position = ZERO`. Because durations can be `None`, the mock's
end-of-track check uses `duration.unwrap_or(Duration::from_secs(180))`.
Add `MockPlayer::empty()`: stopped, `current = None`, no tracks, `volume 68`, `balance 0`.

### 6. Library scanner (`src/library/mod.rs`, new; add `mod library;` to `main.rs`)

```rust
pub fn is_audio(path: &Path) -> bool;                 // extension check
pub fn read_track(path: &Path) -> Track;              // never fails
pub enum ScanMsg {
    Progress { found: usize },
    Done { tracks: Vec<Track>, skipped: usize },
}
pub fn spawn_scan(paths: Vec<PathBuf>, tx: mpsc::Sender<ScanMsg>);  // std::thread::spawn
```

- `read_track`: `lofty::read_from_path`; on **any** error still return a `Track`
  (all tags `None`, `duration None`) — the file may still play; plan 03 handles real decode errors.
  Tags: `tagged.primary_tag().or(tagged.first_tag())`, then `tag.artist()` / `tag.title()`
  (`use lofty::prelude::*;`). Treat empty/whitespace strings as `None`. Duration: `tagged.properties().duration()`;
  a zero duration → `None`.
- `spawn_scan`: for each path — directory → `walkdir::WalkDir::new(p).follow_links(false)`, keep files where `is_audio`;
  file → keep if `is_audio` else count in `skipped`. Send `Progress` every 50 files.
  At the end send `Done` with all tracks (**do not sort here**; the player sorts in `append_sorted`).
  Unreadable directory entries increment `skipped`; never panic.

Tests (use `tempfile`): nested dirs, mixed case extensions (`A.MP3`), non-audio ignored,
a 0-byte `.mp3` still yields a track named after the file, tag-less file → `display_name` is the file stem.
For a duration test, write a 1-second 16-bit PCM WAV header+data by hand in the test and assert duration ≈ 1 s.

### 7. Text prompt + new actions
`UiState` gets:
```rust
pub prompt: Option<Prompt>,        // Prompt { kind: PromptKind, text: String }
pub filter: Option<String>,        // active playlist filter
pub enum PromptKind { Open, Add, Find }
```
New `Action`s (follow rule 8 in the README): `Clear` (`"clear"`, playlist scope, key `"D"`, "clear playlist"),
`MoveUp` (`"move_up"`, playlist scope, `"shift+up"`), `MoveDown` (`"move_down"`, playlist scope, `"shift+down"`),
`ClearFilter` (`"clear_filter"`, playlist scope, `"esc"`). Update descriptions of
`Open`, `Add`, `Del`, `Find` to drop "(stub)".

`App::handle_key`: **before** the keymap, if `ui.prompt.is_some()`:
`Esc` → close (for `Find`: also clear the filter); `Enter` → submit; `Backspace` → pop a char;
`KeyCode::Char(c)` without CONTROL → push `c`; `Ctrl+C` still quits; swallow everything else.
For `Find`, update `ui.filter = Some(text)` on **every** keystroke (live filter) and reset the cursor to 0.
`Enter` in `Find` keeps the filter and closes the prompt.

Submitting `Open`/`Add`: expand a leading `~` using `HOME`, split nothing (one path per prompt),
check `Path::exists()`; if missing, set `ui.notice = "not found: <path>"` and stop.
Otherwise start a scan (step 8).

### 8. Wiring scans into `App`
Fields: `scan_rx: Option<Receiver<ScanMsg>>`, `scan_mode: ScanMode { Replace, Append }`.
`App::load_paths(paths, mode)`: if a scan is already running → notice `already loading`, return.
Create the channel, `spawn_scan`, store `scan_rx`.
In `App::tick`, call `poll_scan()`: `try_recv` in a loop —
`Progress` → notice `loading… N files`; `Done` →
- zero tracks → notice `no audio files found`, leave the playlist alone;
- `Replace` → `player.handle(ReplaceTracks(tracks))`, cursor 0;
- `Append` → `player.handle(AddTracks(tracks))`;
- notice `loaded N tracks (S skipped)`; clear `scan_rx`.
Always `self.snapshot = self.player.snapshot()` afterwards.

`main.rs`: positional args = every argument not starting with `--` (skip this when `--dump` is present).
In non-demo mode build the app with `MockPlayer::empty()` and call `app.load_paths(positional, Replace)` if any were given.
`--demo` and `--dump` keep the fake playlist.

### 9. Playlist controls in `dispatch`
Introduce `App::visible_indices(&self) -> Vec<usize>`: all indices when `ui.filter` is `None`; otherwise those
whose `display_name().to_lowercase()` contains the lowercase filter. **`playlist_cursor` is a position in this
visible list**, not a track index. Use it in every playlist action:
- `Select` → `PlayTrack(visible[cursor])`.
- `Del` → `RemoveTrack(visible[cursor])`, then clamp the cursor.
- `Clear` → `ClearPlaylist`, cursor 0, clear the filter.
- `MoveUp`/`MoveDown` → if a filter is active, notice `clear the filter to reorder` and do nothing; else send
  `MoveTrack` and move the cursor with the track.
- `Add` → open prompt `Add`; `Open` → open prompt `Open`; `Find` → open prompt `Find` pre-filled with the current filter.
- `ClearFilter` → `ui.filter = None`, cursor 0.
- `CursorUp/Down/PageUp/PageDown/JumpEnd` use `visible.len()` as the bound. `sync_to_area` passes `visible.len()` to `scroll_for`.
- Always clamp the cursor after the list changes.

### 10. Rendering (`src/ui/playlist.rs`, `src/ui/mod.rs`)
Read the existing row renderer first and keep its look. Change only:
- Row text = `track.display_name()`; right-aligned time `(m:ss)` or `(--:--)` when `None`.
- Iterate `visible_indices()`; the "now playing" marker still compares the **real** index to `snapshot.current`.
- Bottom border: when `ui.prompt` is `Some`, show `open: <text>▏` / `add: …` / `find: …` in the notice slot
  (same truncation as notices; the totals must still fit). When a filter is active and no prompt is open show `filter: <text>`.
- Totals = sum of known durations; track count = total (unfiltered) tracks.
- Empty playlist: show one dim row `empty — press l to open a file or folder` (use `Role::Muted`).
  Verify the player panel and marquee do not panic when `current` is `None` (add a test that renders with `MockPlayer::empty()`).

### 11. Docs
README: keys table (`l`, `a`, `d`, `D`, `/`, `Esc`, `Shift+↑/↓`), Quickstart `cargo run -- ~/Music`, update the status line.

## Tests checklist
- `Playlist` ops (remove before/at/after current; move up/down; replace; append_sorted order + stability).
- `display_name` / `sort_key` for tagged, untagged, title-only.
- Scanner tests from step 6.
- App tests: Find filter narrows `visible_indices`; `Select` on a filtered row plays the right real track;
  `Del` with a filter removes the right track; prompt keys (type, Backspace, Esc, Enter with a nonexistent path → notice).
- Keymap table test updated for the new actions; no duplicate-binding conflicts.
- Goldens unchanged.

## Done when
`cargo run -- <a real music dir>` shows the sorted list with times, `a`/`d`/`D`/`Shift+↑↓`/`/` work, `l` replaces the list,
and the three cargo checks pass.

## Assumptions I made (tell me to change)
- Title-only or artist-only files fall back to the file name.
- `Open` autoplays; `Add` does not.
- Live filter (not "jump to match").
- Playlist is not persisted between runs.

## Deviations (implemented 2026-10-22)

Annotated at the end as requested (normally plans-README rule 11 says stop and
ask; the implementer instruction was to annotate instead):

1. **Files outside the plan's list.** `src/ui/player.rs` — the enriched `Track`
   (`title: String` → tags + `display_name()`, `duration` → `Option<Duration>`)
   forced edits in the marquee, seek bar, compact title, and remaining-time
   code. Also touched: `AGENTS.md` (test count + boundaries, plans-README rule
   10) and `docs/plans/README.md` (index status row), plus this status header.
2. **`App::new_session()` added.** Plan step 8 says "in non-demo mode build the
   app with `MockPlayer::empty()`", but existing tests construct non-demo apps
   (`App::new(…, false)` / `with_config_path(…, false, …)`) and depend on the
   24-track mock playlist, so `App::new` keeps the mock; only the real
   entrypoint uses the new constructor. `--demo` / `--dump` unchanged.
3. **`Playlist` extras beyond the listed API**: `from_tracks()` (wraps without
   sorting — the mock's authored track order must survive or the golden frames
   change; `append_sorted` would reorder the fake tracks), `is_empty()`, and
   `#[cfg(test)] set_title()` (backs `MockPlayer::set_track_title`).
4. **`MockPlayer::handle` split** into playlist-mutation commands +
   `handle_transport()`: the old blanket `if len == 0 { return; }` would make
   Add/Replace dead on the empty playlist that sessions now start with.
5. **`Key::matches` tightened** (not in the plan, required by its step 7): the
   SHIFT tolerance for `+`-style chars used to apply to *any* non-alphabetic
   key, so plain `up` would match a `shift+up` press — `MoveUp`/`MoveDown`
   could never fire and the defaults would self-conflict. Tolerance now applies
   only to non-alphabetic **chars**; `shift+up`/`shift+down`, `D`, and letters
   match modifiers strictly. Tests cover both directions (`+`/`?` still
   shifted-tolerant).
6. **`SeekBy` also clamps to the 180 s assumed length** when the duration is
   unknown (plan specified only the end-of-track check).
7. **A filter with zero matches** renders a dim `no matches` row (the plan
   specifies only the empty-playlist hint); the border totals still show the
   unfiltered count.
8. **`~` expansion**: only a bare `~` or a leading `~/` expands via `$HOME`;
   `~user/…` is left literal (plan: "expand a leading `~` using HOME").
9. **The scroll resets together with the cursor** on Replace / Clear /
   ClearFilter and while typing a live filter (plan mentions only the cursor) —
   keeps `scroll_for` consistent after the list reshapes.
10. **lofty resolved to 0.25.4**: `tag.artist()`/`title()` return `Cow<str>`
    (`.as_deref()` needed). API detail only; behavior as planned.
11. **Post-plan fix, user-approved: `._*` AppleDouble sidecars are skipped**
    (`is_apple_double()` in `library/mod.rs`). Loading an external volume
    (`/Volumes/…`) surfaced macOS metadata stubs that inherit the `.flac`
    extension — they doubled the row count and would fail to decode in plan 03.
    The plan's "hidden files are included" decision otherwise stands: a
    deliberately hidden `.track.flac` still loads; an explicit `._…` argument
    counts as `skipped`.

Extra verification beyond the checklist: the real TUI was driven in a pty —
CLI load of `~/Music/aepi` (`loaded 10 tracks (0 skipped)`, `10 · 30:00`),
`l` prompt + `~` path, `a` append (→ `20 · 1:00:00`), `d` (10→9), `D` → empty
hint, `/` live filter + `Esc` clear, `Shift+↑/↓` reorder, not-found notice,
and a clean `q` exit.

The plan's Assumptions block above stands unchanged — nothing in it was
overridden.
