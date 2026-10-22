//! Playlist ownership, shared by the mock (plan 02) and the real player
//! (plan 03). Pure data + bookkeeping: no I/O, no rendering.

use std::sync::Arc;

use super::Track;

/// An ordered list of tracks plus the index of the current one.
///
/// The track list is `Arc<Vec<Track>>` so a [`PlayerSnapshot`][super::PlayerSnapshot]
/// can be taken 30 times a second for the cost of an `Arc` clone.
#[derive(Debug, Clone, Default)]
pub struct Playlist {
    tracks: Arc<Vec<Track>>,
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
            tracks: Arc::new(tracks),
            current: None,
        }
    }

    /// Replace a track title (test support, for CJK overflow checks).
    #[cfg(test)]
    pub fn set_title(&mut self, index: usize, title: &str) {
        if let Some(track) = Arc::make_mut(&mut self.tracks).get_mut(index) {
            track.title = Some(title.into());
        }
    }

    pub fn tracks(&self) -> &Arc<Vec<Track>> {
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
        new.sort_by_key(|t| t.sort_key());
        Arc::make_mut(&mut self.tracks).extend(new);
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
        Arc::make_mut(&mut self.tracks).clear();
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

    /// A tagged track keyed on `artist` (title empty), so `sort_key` is
    /// `"<key> - "` and sorting follows `key`.
    fn track(key: &str) -> Track {
        Track {
            id: Track::next_id(),
            source: TrackSource::Local(PathBuf::from(format!("/x/{key}.flac"))),
            artist: Some(key.to_string()),
            title: Some(String::new()),
            duration: Some(Duration::from_secs(60)),
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
    fn clear_empties_and_unsets_current() {
        let mut p = playlist(&["a", "b"]);
        p.set_current(Some(1));
        p.clear();
        assert!(p.is_empty());
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
        let snap: Arc<Vec<Track>> = Arc::clone(p.tracks());
        assert!(Arc::ptr_eq(&snap, p.tracks()));
    }
}
