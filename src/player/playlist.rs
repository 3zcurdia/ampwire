//! Playlist ownership, shared by the mock (plan 02) and the real player
//! (plan 03). Pure data + bookkeeping: no I/O, no rendering.

use std::sync::Arc;
use std::time::Duration;

use super::{Track, TrackList};

/// An ordered list of tracks plus the index of the current one.
///
/// The track list is `Arc<Vec<Track>>` so a [`PlayerSnapshot`][super::PlayerSnapshot]
/// can be taken 30 times a second for the cost of an `Arc` clone.
#[derive(Debug, Clone, Default)]
pub struct Playlist {
    tracks: TrackList,
    current: Option<usize>,
}

impl Playlist {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wrap an already-built list as-is (no sort — the mock's authored order
    /// must survive for the golden frames).
    pub fn from_tracks(tracks: Vec<Track>) -> Self {
        Self {
            tracks: Arc::new(tracks.into_iter().map(Arc::new).collect()),
            current: None,
        }
    }

    /// Replace a track title (test support, for CJK overflow checks).
    #[cfg(test)]
    pub fn set_title(&mut self, index: usize, title: &str) {
        if let Some(track) = Arc::make_mut(&mut self.tracks).get_mut(index) {
            Arc::make_mut(track).title = Some(title.into());
        }
    }

    /// Record the decoder's duration for track `index` (plan 04, step 3):
    /// the container's frame count is accurate where tag durations lie or
    /// are missing, which is what makes the remaining-time display work.
    pub fn set_duration(&mut self, index: usize, duration: Duration) {
        if let Some(track) = Arc::make_mut(&mut self.tracks).get_mut(index) {
            Arc::make_mut(track).duration = Some(duration);
        }
    }

    pub fn tracks(&self) -> &TrackList {
        &self.tracks
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn current(&self) -> Option<usize> {
        self.current
    }

    pub fn set_current(&mut self, current: Option<usize>) {
        self.current = current;
    }

    /// Stable-sort the new tracks among themselves, then append them after
    /// whatever is already in the list (manual order survives an add).
    pub fn append_sorted(&mut self, mut new: Vec<Track>) {
        // One key per track, not two allocations per comparison (audit P5).
        new.sort_by_cached_key(Track::sort_key);
        Arc::make_mut(&mut self.tracks).extend(new.into_iter().map(Arc::new));
    }

    /// Append exactly as given (plan 08): server pages arrive already
    /// sorted by `AlbumArtist, Album, ParentIndexNumber, IndexNumber,
    /// SortName`, so the client must not re-sort them.
    pub fn append_in_order(&mut self, new: Vec<Track>) {
        Arc::make_mut(&mut self.tracks).extend(new.into_iter().map(Arc::new));
    }

    /// Remove every track matching `f` in one pass. Returns `true` when the
    /// *current* track was among them (then `current` clears, exactly like
    /// [`Playlist::remove`]); otherwise it keeps pointing at the same track,
    /// with the indices before it closed up.
    pub fn remove_where(&mut self, f: impl Fn(&Track) -> bool) -> bool {
        let current = self.current;
        let tracks = Arc::make_mut(&mut self.tracks);
        let mut kept: Vec<Arc<Track>> = Vec::with_capacity(tracks.len());
        let mut new_current = None;
        let mut removed_current = false;
        for (index, track) in tracks.drain(..).enumerate() {
            if f(&track) {
                if current == Some(index) {
                    removed_current = true;
                }
                continue;
            }
            if current == Some(index) {
                new_current = Some(kept.len());
            }
            kept.push(track);
        }
        *tracks = kept;
        self.current = new_current;
        removed_current
    }

    /// Remove track `i`. Returns `true` when the removed track was the
    /// current one. Indices after the hole close up.
    pub fn remove(&mut self, i: usize) -> bool {
        if i >= self.len() {
            return false;
        }
        Arc::make_mut(&mut self.tracks).remove(i);
        let was_current = self.current == Some(i);
        self.current = match self.current {
            Some(c) if c == i => None,
            Some(c) if i < c => Some(c - 1),
            other => other,
        };
        was_current
    }

    /// Empty the list; nothing is current any more.
    pub fn clear(&mut self) {
        // A fresh empty list: never copy the old one just to drop it.
        self.tracks = TrackList::default();
        self.current = None;
    }

    /// Swap with the track above. Returns the moved track's new index, or
    /// `None` at the top edge. `current` keeps pointing at the same track.
    pub fn move_up(&mut self, i: usize) -> Option<usize> {
        if i == 0 || i >= self.len() {
            return None;
        }
        {
            let tracks = Arc::make_mut(&mut self.tracks);
            tracks.swap(i, i - 1);
        }
        self.current = match self.current {
            Some(c) if c == i => Some(i - 1),
            Some(c) if c == i - 1 => Some(i),
            other => other,
        };
        Some(i - 1)
    }

    /// Swap with the track below. Returns the moved track's new index, or
    /// `None` at the bottom edge. `current` keeps pointing at the same track.
    pub fn move_down(&mut self, i: usize) -> Option<usize> {
        if i + 1 >= self.len() {
            return None;
        }
        {
            let tracks = Arc::make_mut(&mut self.tracks);
            tracks.swap(i, i + 1);
        }
        self.current = match self.current {
            Some(c) if c == i => Some(i + 1),
            Some(c) if c == i + 1 => Some(i),
            other => other,
        };
        Some(i + 1)
    }

    /// Replace the whole list (`clear` + `append_sorted`).
    pub fn replace(&mut self, new: Vec<Track>) {
        self.clear();
        self.append_sorted(new);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::player::TrackSource;

    /// A tagged track keyed on `artist` (title empty), so `alpha_key` is
    /// `"<key> - "` and sorting follows `key`.
    fn track(key: &str) -> Track {
        Track {
            id: Track::next_id(),
            source: TrackSource::Local(PathBuf::from(format!("/x/{key}.flac"))),
            artist: Some(key.to_string()),
            title: Some(String::new()),
            duration: Some(Duration::from_secs(60)),
            bitrate_kbps: None,
            pos: crate::player::AlbumPos::default(),
        }
    }

    fn playlist(keys: &[&str]) -> Playlist {
        let mut p = Playlist::new();
        p.append_sorted(keys.iter().map(|k| track(k)).collect());
        p
    }

    fn keys_of(p: &Playlist) -> Vec<String> {
        p.tracks()
            .iter()
            .map(|t| t.artist.clone().unwrap_or_default())
            .collect()
    }

    #[test]
    fn append_sorted_sorts_new_tracks_and_keeps_existing_order() {
        let mut p = playlist(&["b", "c"]);
        p.set_current(Some(0)); // manual state, e.g. playing "b"
        p.append_sorted(vec![track("d"), track("a")]);
        // The new batch sorts among itself ("a", "d") and lands *after* the
        // existing list — a later add never re-sorts manual ordering.
        assert_eq!(keys_of(&p), ["b", "c", "a", "d"]);
        // Appending does not disturb the current index (same track, same place).
        assert_eq!(p.current(), Some(0));
    }

    #[test]
    fn append_sorted_is_stable_for_equal_keys() {
        // Two tracks with the same sort key: insertion order is preserved.
        let mut p = Playlist::new();
        let mut a = track("same");
        a.id = 1;
        let mut b = track("same");
        b.id = 2;
        p.append_sorted(vec![a, b]);
        assert_eq!(p.tracks().iter().map(|t| t.id).collect::<Vec<_>>(), [1, 2]);
    }

    /// Plan 12: one mixed batch — a scrambled album, an un-numbered
    /// tagged track and an untagged file — comes out album-grouped, then
    /// disc, then track number; the others keep today's alpha order.
    #[test]
    fn append_sorted_orders_albums_by_disc_and_track() {
        fn album(path: &str, artist: &str, album: &str, number: u32) -> Track {
            let mut t = Track {
                id: Track::next_id(),
                source: TrackSource::Local(PathBuf::from(path)),
                artist: Some(artist.into()),
                title: Some(format!("{album} track {number}")),
                duration: None,
                bitrate_kbps: None,
                pos: crate::player::AlbumPos::default(),
            };
            t.pos.album = Some(album.into());
            t.pos.number = Some(number);
            t
        }
        let untagged = Track {
            id: Track::next_id(),
            source: TrackSource::Local(PathBuf::from("/m/untagged.flac")),
            artist: None,
            title: None,
            duration: None,
            bitrate_kbps: None,
            pos: crate::player::AlbumPos::default(),
        };

        let mut p = Playlist::new();
        p.append_sorted(vec![
            album("/m/b/02.flac", "Bee", "Beta", 2), // scrambled on purpose
            track("a"),                              // un-numbered: today's order
            album("/m/b/01.flac", "Bee", "Beta", 1),
            untagged,
            album("/m/a/01.flac", "Aph", "Alpha", 1),
        ]);

        let paths: Vec<String> = p
            .tracks()
            .iter()
            .map(|t| match &t.source {
                TrackSource::Local(path) => path.to_string_lossy().into_owned(),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            paths,
            [
                // Untagged: its path key starts with "/", before any letter.
                "/m/untagged.flac".to_string(),
                // Un-numbered "a - " stays in alphabetical position.
                "/x/a.flac".to_string(),
                // The two albums, grouped and in number order.
                "/m/a/01.flac".to_string(),
                "/m/b/01.flac".to_string(),
                "/m/b/02.flac".to_string(),
            ]
        );
    }

    #[test]
    fn remove_adjusts_current_around_the_hole() {
        // Removing *before* the current track shifts it down.
        let mut p = playlist(&["a", "b", "c"]);
        p.set_current(Some(2));
        assert!(!p.remove(0), "removed track was not current");
        assert_eq!(p.current(), Some(1));

        // Removing *the* current track clears it.
        assert!(p.remove(1), "removed track was current");
        assert_eq!(p.current(), None);

        // Removing *after* the current track leaves it alone.
        let mut p = playlist(&["a", "b", "c"]);
        p.set_current(Some(0));
        assert!(!p.remove(2));
        assert_eq!(p.current(), Some(0));

        // Out of range: no-op, false.
        assert!(!p.remove(99));
        assert_eq!(p.len(), 2, "out-of-range remove left the list alone");
    }

    #[test]
    fn set_duration_fills_in_the_decoder_length() {
        let mut p = playlist(&["a", "b"]);
        assert_eq!(p.tracks()[0].duration, Some(Duration::from_secs(60)));
        p.set_duration(1, Duration::from_millis(2_500));
        assert_eq!(p.tracks()[1].duration, Some(Duration::from_millis(2_500)));
        // Out of range: no-op, never a panic.
        p.set_duration(99, Duration::from_secs(1));
        assert_eq!(p.len(), 2);
    }

    #[test]
    fn clear_empties_and_unsets_current() {
        let mut p = playlist(&["a", "b"]);
        p.set_current(Some(1));
        p.clear();
        assert_eq!(p.len(), 0);
        assert_eq!(p.current(), None);
    }

    #[test]
    fn move_up_and_down_track_the_moved_and_current_tracks() {
        let mut p = playlist(&["a", "b", "c"]);
        p.set_current(Some(1)); // current = "b"

        // "a" up: edge, no move.
        assert_eq!(p.move_up(0), None);
        // "b" (current) up: it moves and current follows it.
        assert_eq!(p.move_up(1), Some(0));
        assert_eq!(keys_of(&p), ["b", "a", "c"]);
        assert_eq!(p.current(), Some(0));

        // "a" down: current ("b" at 0) is unaffected.
        assert_eq!(p.move_down(1), Some(2));
        assert_eq!(keys_of(&p), ["b", "c", "a"]);
        assert_eq!(p.current(), Some(0));

        // Current swaps with the track below it: current follows.
        assert_eq!(p.move_down(0), Some(1));
        assert_eq!(p.current(), Some(1));

        // Bottom edge.
        assert_eq!(p.move_down(2), None);
        // Out of range.
        assert_eq!(p.move_up(99), None);
    }

    #[test]
    fn replace_swaps_the_whole_list() {
        let mut p = playlist(&["a", "b"]);
        p.set_current(Some(1));
        p.replace(vec![track("z"), track("m")]);
        assert_eq!(keys_of(&p), ["m", "z"]);
        assert_eq!(p.current(), None, "replace clears current");
    }

    #[test]
    fn snapshot_shares_the_arc() {
        // The list Arc is shared, not deep-cloned (plan §step 4).
        let p = playlist(&["a"]);
        let snap: TrackList = Arc::clone(p.tracks());
        assert!(Arc::ptr_eq(&snap, p.tracks()));
    }

    // --- server order + remote removal (plan 08, step 3) ---

    /// A remote (Jellyfin) track keyed on its item id.
    fn remote(id: &str) -> Track {
        Track {
            id: Track::next_id(),
            source: TrackSource::Remote {
                item_id: id.into(),
                container: Some("flac".into()),
            },
            artist: Some(format!("artist-{id}")),
            title: Some(format!("title-{id}")),
            duration: None,
            bitrate_kbps: None,
            pos: crate::player::AlbumPos::default(),
        }
    }

    #[test]
    fn append_in_order_keeps_the_server_order_and_existing_tracks() {
        let mut p = playlist(&["b", "c"]);
        p.set_current(Some(0));
        // Deliberately unsorted: the server already sorted by
        // AlbumArtist,Album,ParentIndexNumber,IndexNumber,SortName and
        // plan 08 forbids a second client-side sort.
        p.append_in_order(vec![remote("zz"), remote("aa")]);
        assert_eq!(
            p.tracks()
                .iter()
                .map(|t| t.display_name())
                .collect::<Vec<_>>(),
            [
                "b - ",
                "c - ",
                "artist-zz - title-zz",
                "artist-aa - title-aa",
            ],
            "appended exactly as given"
        );
        assert_eq!(p.current(), Some(0), "current untouched by an append");
    }

    #[test]
    fn remove_where_reports_and_clears_a_removed_current_track() {
        let mut p = Playlist::new();
        p.append_in_order(vec![remote("a"), track("local"), remote("b")]);
        p.set_current(Some(2)); // playing remote "b"

        // Removing the remote batch takes the current track too.
        assert!(
            p.remove_where(|t| !matches!(t.source, crate::player::TrackSource::Local(_))),
            "the current track was removed"
        );
        assert_eq!(p.current(), None, "current clears, like `remove`");
        assert_eq!(keys_of(&p), ["local"], "only the local track survives");

        // With the current track *not* matching, it survives and its index
        // closes up around the holes.
        let mut p = Playlist::new();
        p.append_in_order(vec![remote("a"), track("local"), remote("b")]);
        p.set_current(Some(1));
        assert!(!p.remove_where(|t| !matches!(t.source, crate::player::TrackSource::Local(_))));
        assert_eq!(p.current(), Some(0), "still pointing at the local track");
        assert_eq!(p.len(), 1);

        // No match at all: nothing moves, `false` comes back.
        let mut p = playlist(&["x", "y"]);
        p.set_current(Some(1));
        assert!(!p.remove_where(|t| !matches!(t.source, crate::player::TrackSource::Local(_))));
        assert_eq!(p.len(), 2);
        assert_eq!(p.current(), Some(1));
    }
}
