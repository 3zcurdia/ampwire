# Plan 12 — Album order: sort by album → disc → track, alphabetical fallback

**Status:** ⬜ not started

Goal: a loaded album plays in **album order** (1-01, 1-02, …, 2-01) instead of
alphabetically by title. Tracks with no track-number tag keep **today's
alphabetical order** exactly (`artist - title`, else the path / URL).

Prerequisites: plans 02 (scan + `append_sorted`), 08 (Jellyfin) and 10 (SMB)
merged — all are ✅ done.

## How sorting works today (read this before changing anything)

- `Track::sort_key()` in `src/player/mod.rs` returns a lowercased `String`:
  `"artist - title"` when both tags exist, otherwise the local path, the SMB
  canonical URL, or (Jellyfin) the title.
- `Playlist::append_sorted` in `src/player/playlist.rs` runs
  `new.sort_by_cached_key(Track::sort_key)` on each **new batch only**, then
  appends it. `Playlist::replace` calls `append_sorted`. This covers local and
  `smb://` loads (`src/player/real.rs`, `AddTracks` / `ReplaceTracks`).
- Jellyfin tracks **never** go through `append_sorted`: the server sorts them
  and `AppendInOrder` → `Playlist::append_in_order` keeps that order (plan 08
  rule: no client-side re-sort of Jellyfin pages).
- Tags come from lofty in `library::tag_fields` (`src/library/mod.rs`). SMB
  scanning (`src/smb/scan.rs::track_for`) reuses `tag_fields`, so any new tag
  read there also reaches SMB tracks with no extra work.

## Decisions

| Topic | Decision |
|---|---|
| Why group first | Sorting a whole library by disc/track alone would play every album's track 1 first. So tracks are **grouped by album** before disc/track. |
| Album group | `"<album artist> - <album>"` lowercased when the file has an album tag (album artist falls back to the track artist; no artist at all → just the album). No album tag → the track's **folder** (local parent directory, or the SMB URL without its last segment), lowercased, **with a trailing `/`**. |
| Missing disc | Counts as disc **1** (so "no disc tag" and "disc 1" merge). |
| Missing track number | The track uses **today's key** (`alpha_key`) for everything — same order as before this plan. |
| Track number `0` / disc `0` | Treated as missing (`None`). |
| Tie-break | Today's key (`alpha_key`), then the stable sort keeps scan order. |
| Mixed batches | Numbered and un-numbered tracks share one ordering: both keys start with a lowercased string (`group` or the alpha key), so an untagged "Aphex Twin - Xtal" lands next to the "aphex twin - …" albums, and folder-grouped tracks (keys start with `/` or `smb://`) sit where their paths sat before. |
| Existing playlist | Unchanged rule: adding a batch never re-sorts what is already in the list. |
| Jellyfin | Server-side only: change `SortBy` to `AlbumArtist,Album,ParentIndexNumber,IndexNumber,SortName`. Also map `Album` / `IndexNumber` / `ParentIndexNumber` onto the track so the data is consistent. Still `append_in_order` — **no** client re-sort. |
| Mock player | Untouched (`Playlist::from_tracks` never sorts) → golden frames must not change. |
| Out of scope | Showing track numbers in playlist rows, a user-selectable sort mode, re-sorting an existing playlist, compilation detection beyond the album-artist tag. |

## Files

`src/player/mod.rs` (new `AlbumPos`, `SortKey`, `alpha_key`, new `sort_key`),
`src/library/mod.rs` (`TagFields` + `tag_fields` + `read_track`),
`src/smb/scan.rs` (`track_for`), `src/jellyfin/mod.rs` (`Item`, query,
`track_from_item`), `src/jellyfin/test_server.rs` (fixture JSON),
`src/player/playlist.rs` (tests only), every other `Track { … }` literal
(listed in step 2), `README.md`, `AGENTS.md`, `CHANGELOG.md`, this file and
`README.md` in this folder.

## Steps

Run `cargo check --all-targets` after **every** step and fix all errors before
moving on. Do not reformat or rename unrelated code.

### Step 1 — `AlbumPos` type on `Track`

In `src/player/mod.rs`, just **above** `pub struct Track`, add:

```rust
/// Album position tags (plan 12): what the playlist groups and orders by
/// before falling back to [`Track::alpha_key`]. All `None` for a file
/// without such tags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlbumPos {
    pub album: Option<String>,
    pub album_artist: Option<String>,
    /// Disc number, `None` when missing or `0`.
    pub disc: Option<u32>,
    /// Track number on the disc, `None` when missing or `0`.
    pub number: Option<u32>,
}
```

Add one field at the **end** of `struct Track` (after `bitrate_kbps`):

```rust
    /// Album/disc/track tags that drive the sort (plan 12).
    pub pos: AlbumPos,
```

### Step 2 — fix every `Track { … }` literal

`cargo check --all-targets` will now fail with "missing field `pos`" errors.
Add `pos: AlbumPos::default(),` (or `pos: Default::default(),` where
`AlbumPos` is not imported) as the **last** field of each literal. The sites
today are:

- `src/app.rs` (two test helpers: the `row` closure and `remote_track`)
- `src/jellyfin/mod.rs` (`track_from_item` — you will replace this in step 6;
  use `Default::default()` for now)
- `src/jellyfin/cache.rs` (test helpers `local`, `remote`)
- `src/library/mod.rs` (`read_track` — replaced in step 4)
- `src/smb/cache.rs` (test helpers `smb_track`, `local_track`, `jellyfin_track`)
- `src/smb/scan.rs` (`track_for` — replaced in step 5)
- `src/player/mock.rs` (the demo list)
- `src/player/playlist.rs` (test helpers `track`, `remote`)
- `src/player/real.rs` (the test `.map(|i| Track { … })`, `remote_track`,
  `smb_row`)
- `src/player/mod.rs` (test helpers `local`, `smb`, `jellyfin`)

Use `grep -rn "Track {" src tests` to make sure none is missed (ignore
`QueuedTrack` and `MoveTrack`, which are different types). Do **not** change
any other field. After this step `cargo test` must still pass unchanged.

### Step 3 — the new sort key

In `src/player/mod.rs`:

1. **Rename** the existing `pub fn sort_key(&self) -> String` to
   `pub fn alpha_key(&self) -> String`. Keep its body exactly. Update its doc
   comment's first line to: `/// Today's alphabetical order (plans 02/08/10),
   used as the tie-break and as the whole key when a track has no track
   number.`
2. Add the key type next to `AlbumPos`. **Field order matters**: the derived
   `Ord` compares fields top to bottom.

```rust
/// What [`Playlist::append_sorted`][crate::player::playlist::Playlist::append_sorted]
/// sorts on (plan 12): album group, disc, track number, then today's
/// alphabetical key. Field order is the comparison order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SortKey {
    group: String,
    disc: u32,
    number: u32,
    alpha: String,
}
```

3. Add these methods inside `impl Track` (right after `alpha_key`):

```rust
    /// Sort order (plan 12): a track with a track number sorts inside its
    /// album group by disc (missing = 1) then number; a track without one
    /// sorts exactly as before ([`alpha_key`][Self::alpha_key]).
    pub fn sort_key(&self) -> SortKey {
        let alpha = self.alpha_key();
        match self.pos.number {
            Some(number) => SortKey {
                group: self.album_group().unwrap_or_else(|| self.folder_key()),
                disc: self.pos.disc.unwrap_or(1),
                number,
                alpha,
            },
            None => SortKey {
                group: alpha.clone(),
                disc: 0,
                number: 0,
                alpha,
            },
        }
    }

    /// `"<album artist> - <album>"` lowercased (album artist falls back to
    /// the track artist); `None` without an album tag.
    fn album_group(&self) -> Option<String> {
        let album = self.pos.album.as_deref()?;
        let artist = self.pos.album_artist.as_deref().or(self.artist.as_deref());
        let group = match artist {
            Some(artist) => format!("{artist} - {album}"),
            None => album.to_string(),
        };
        Some(group.to_lowercase())
    }

    /// The folder holding the track, lowercased, ending in `/` (so a
    /// folder's numbered tracks sort right before its un-numbered files,
    /// whose path key starts with the same text). A Jellyfin row has no
    /// folder and falls back to its alpha key.
    fn folder_key(&self) -> String {
        match &self.source {
            TrackSource::Local(path) => match path.parent() {
                Some(dir) => format!("{}/", dir.to_string_lossy().to_lowercase()),
                None => self.alpha_key(),
            },
            TrackSource::Smb { url } => {
                let canonical = url.canonical().to_lowercase();
                match canonical.rsplit_once('/') {
                    Some((dir, _)) => format!("{dir}/"),
                    None => canonical,
                }
            }
            TrackSource::Remote { .. } => self.alpha_key(),
        }
    }
```

4. `src/player/playlist.rs` needs **no code change**: `sort_by_cached_key(Track::sort_key)`
   works with any `Ord` key. Leave the comment above `append_sorted` as is.
5. Fix the tests that compared `sort_key()` to a `String`:
   - `src/player/mod.rs`: `sort_key_is_artist_title_or_path_lowercase` and
     `smb_rows_display_and_sort_like_local_files` → call `alpha_key()`
     instead of `sort_key()`. Rename the first test to
     `alpha_key_is_artist_title_or_path_lowercase`.
   - `src/library/mod.rs` (around the `assert_eq!(track.sort_key(), …)` in
     its tests) → `alpha_key()`.
   - Update the doc comment on the `track` helper in `playlist.rs` tests:
     "`sort_key`" → "`alpha_key`".

`cargo test` must pass now: every existing track has `pos.number == None`,
so its order is unchanged.

### Step 4 — read the tags (local files)

In `src/library/mod.rs`:

1. Add to `TagFields` (after `bitrate_kbps`):

```rust
    /// Album/disc/track tags (plan 12).
    pub pos: crate::player::AlbumPos,
```

   `TagFields` derives `Default`, and so does `AlbumPos`, so nothing else
   changes. Check that `AlbumPos` is reachable as `crate::player::AlbumPos`
   (it is `pub` in `player/mod.rs`; if `player` re-exports items
   selectively, add it next to `Track`).

2. In `tag_fields`, inside the existing `if let Some(tag) = …` block, after
   the `title` line, add:

```rust
        fields.pos.album = clean(tag.album().as_deref());
        fields.pos.album_artist = clean(tag.get_string(ItemKey::AlbumArtist));
        fields.pos.disc = tag.disk().filter(|n| *n > 0);
        fields.pos.number = tag.track().filter(|n| *n > 0);
```

   `ItemKey` and `Accessor` both come from the `use lofty::prelude::*;`
   already at the top of that function (lofty 0.25.4: `Accessor::album()`
   returns `Option<Cow<str>>`, `track()`/`disk()` return `Option<u32>`,
   `Tag::get_string(ItemKey) -> Option<&str>`). If the compiler disagrees,
   read docs.rs for **lofty 0.25.4** — do not guess.

3. In `read_track`, replace `pos: AlbumPos::default()` (from step 2) with
   `pos: fields.pos,`.

### Step 5 — SMB tracks

In `src/smb/scan.rs::track_for`, replace the step-2 default with
`pos: fields.pos,`. That is the whole SMB change — `track_for` already uses
`library::tag_fields`.

### Step 6 — Jellyfin

In `src/jellyfin/mod.rs`:

1. Add to `struct Item` (all optional, PascalCase is already applied by
   `#[serde(rename_all = "PascalCase")]`):

```rust
    album: Option<String>,
    index_number: Option<i64>,
    parent_index_number: Option<i64>,
```

   `album_artist` already exists — keep it.

2. In `Client::page`, change the query's sort part from
   `&SortBy=Artist,SortName` to
   `&SortBy=AlbumArtist,Album,ParentIndexNumber,IndexNumber,SortName`.
   Leave `SortOrder=Ascending` and everything else identical. Update the doc
   comment above `page` ("server-sorted (`Artist, SortName`)") to name the new
   order.

3. In `track_from_item`, **before** `item.album_artist` is consumed by the
   artist fallback, build the position (clone what the artist code later
   moves):

```rust
    // Plan 12: album position, so the data matches local/SMB rows. The
    // server already sorted on it; the client never re-sorts these.
    let pos = AlbumPos {
        album: item
            .album
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string),
        album_artist: item
            .album_artist
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string),
        disc: item
            .parent_index_number
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0),
        number: item
            .index_number
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0),
    };
```

   Then set `pos,` in the final `Track { … }` (replacing the step-2 default).
   Import `AlbumPos` from `crate::player` next to `Track`.

4. Update the other comments that still say `Artist, SortName`:
   `src/player/playlist.rs` (`append_in_order` doc and the test comment near
   `// Deliberately unsorted`), `src/player/mod.rs` (`AppendInOrder` doc).

5. In `src/jellyfin/test_server.rs`, edit the `ITEMS_PAGE` fixture: give
   `item1` `"Album":"The Album","IndexNumber":3,"ParentIndexNumber":2`. Leave
   `item2` without them (it must map to all-`None`). Do not touch any other
   part of the JSON.

### Step 7 — tests

Add these, each with a one-line doc comment saying what it proves (match the
style of the surrounding tests). Write a small helper in `player/mod.rs`
tests, e.g.:

```rust
    /// A local track with album-position tags (plan 12).
    fn numbered(
        path: &str,
        album: Option<&str>,
        disc: Option<u32>,
        number: Option<u32>,
    ) -> Track {
        let mut t = local(path, Some("Artist"), Some(path));
        t.pos = AlbumPos {
            album: album.map(str::to_string),
            album_artist: None,
            disc,
            number,
        };
        t
    }
```

**`src/player/mod.rs`** (unit tests on `sort_key`):

1. `disc_then_number_within_an_album`: disc 2 track 1 sorts **after** disc 1
   track 9, and disc 1 track 2 **after** disc 1 track 1, all in one album.
2. `missing_disc_counts_as_disc_one`: `(disc None, number 2)` sorts between
   `(disc 1, number 1)` and `(disc 1, number 3)` of the same album.
3. `track_number_beats_title_alphabet`: same album, track 1 titled "Zebra"
   sorts before track 2 titled "Apple".
4. `no_number_falls_back_to_alpha_key`: a track with `number: None` (even
   with an album tag) has `sort_key() == SortKey { group: alpha, disc: 0,
   number: 0, alpha }` — compare via `sort_key().cmp(...)` against another
   un-numbered track to show it orders exactly like `alpha_key()`. (The
   `SortKey` fields are private; tests in the same module can read them.)
5. `album_artist_separates_same_named_albums`: two "Greatest Hits" albums
   with different `album_artist`s do not interleave (all of A's tracks, then
   all of B's).
6. `folder_groups_numbered_tracks_without_album_tag`: two local folders
   `/m/b/` and `/m/a/`, numbered tracks with no album tag → all of `/m/a`
   (by number) then all of `/m/b`; an **untagged** file in `/m/a` (built
   with `local("/m/a/x.flac", None, None)`, so its alpha key is the path)
   sorts **after** `/m/a`'s numbered tracks and **before** `/m/b`'s.
7. `smb_folder_key_drops_the_file_name`: an SMB row `Album/03.flac`, numbered,
   no album tag → its group is `"smb://nas/music/album/"`.

**`src/player/playlist.rs`:**

8. `append_sorted_orders_albums_by_disc_and_track`: one batch mixing a tagged
   album given in scrambled order, an un-numbered tagged track and an
   untagged local file comes out in the expected order. Existing tests
   (`append_sorted_is_stable_for_equal_keys`, …) must pass unchanged.

**`src/library/mod.rs`:**

9. `tag_fields_reads_album_disc_and_track`: write a WAV with the existing
   `write_wav` helper, then add an ID3v2 tag with lofty and read it back
   through `read_track`:

```rust
        use lofty::config::WriteOptions;
        use lofty::prelude::*;
        use lofty::tag::{Tag, TagType};

        let mut tag = Tag::new(TagType::Id3v2);
        tag.set_album("Selected Ambient Works".into());
        tag.insert_text(ItemKey::AlbumArtist, "Aphex Twin".into());
        tag.set_disk(2);
        tag.set_track(3);
        tag.save_to_path(&path, WriteOptions::default()).unwrap();
```

   Assert `pos.album`, `pos.album_artist`, `pos.disc == Some(2)`,
   `pos.number == Some(3)`. A second file tagged `set_track(0)` must give
   `pos.number == None`. If `save_to_path` on a WAV fails in this lofty
   version, use a different container lofty can write and say so in a
   comment — do not skip the test.

**`src/jellyfin/mod.rs`:**

10. Extend `the_library_page_parses_and_maps_every_field`: `first.pos` has
    album `"The Album"`, album artist `"The Album Artist"`, disc `Some(2)`,
    number `Some(3)`; `second.pos == AlbumPos::default()`.

**`src/smb/scan.rs`:** if its in-memory `ShareFs` tests build a tagged
file, add one assertion that `pos` is filled; if they only use untagged
bytes, add nothing (step 5 is a one-line pass-through of `tag_fields`,
already covered by test 9).

### Step 8 — docs

1. `README.md` (repo root): where the playlist/load behavior is described,
   say tracks sort by album → disc → track number, and tracks with no
   track number keep the `artist - title` / path order. In the Jellyfin line,
   replace "keeps the server's `Artist, SortName` order" with the new
   server order.
2. `AGENTS.md`: in the `player` boundary text, mention `Track::sort_key`
   (`SortKey`: album group → disc → number → `alpha_key`) and update the test
   count line (`cargo test` — N tests today) with the real numbers from
   `cargo test`.
3. `CHANGELOG.md`: one entry under the unreleased section, in the existing
   style.
4. `.opencode/docs/features/README.md`: in "Product decisions already
   made", change "Default order: alphabetical by `artist - title`; untagged
   files sort by full path." to describe the new order with its fallback.

### Step 9 — finish

1. `cargo fmt`, then all three must pass with no warnings:
   `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo test`.
2. Golden tests must pass **without** editing any `tests/golden/*.txt` (the
   mock player never sorts). If one fails, you changed something you should
   not have — fix the code, not the golden file.
3. Manual check (real TTY): `cargo run -- <a folder with a tagged
   multi-disc album>` → the playlist shows disc 1 tracks in number order,
   then disc 2. A folder of untagged files keeps its path order.
4. Flip this file's **Status** to `✅ done — <YYYY-MM-DD>, <short hash>` and
   the plan-12 row in this folder's `README.md` to `✅ done`, in the same
   commit that finishes the plan.

## Done when

A tagged album (any mix of local and `smb://`) loads in disc → track order,
several albums in one load stay grouped, untagged or un-numbered files sort
exactly as before, Jellyfin pages arrive in album order from the server, the
golden frames are unchanged and the three cargo checks pass.

## Assumptions I made (tell me to change)

- Album grouping is album tag first, folder second (folder-only would mix
  albums that share one directory).
- A missing disc number means disc 1.
- Jellyfin gets the new order through the server's `SortBy` (step 6); the
  client still never re-sorts Jellyfin pages.
- A compilation without an album-artist tag splits by track artist — known
  limitation, not handled here.
