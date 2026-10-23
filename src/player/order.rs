//! Play order: which track plays next, in ordered or shuffle mode (plan 03,
//! step 6).
//!
//! Pure data + bookkeeping — no I/O, no audio. Auto-advance at the end of a
//! track, the manual Next/Prev keys and (later) plan 08's look-ahead all go
//! through [`PlayOrder`], so they can never disagree about "what plays next".

use super::rng::Rng;

/// "Nothing is playing yet": every real index into `order` is `< len`, so
/// `usize::MAX` is a safe sentinel for "before the first track".
const NONE: usize = usize::MAX;

/// The permutation of playlist indices playback follows, plus the cursor into
/// it. Ordered mode keeps the identity permutation (`[0, 1, …]`); shuffle mode
/// keeps a full Fisher–Yates permutation (every track once per round).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayOrder {
    /// Playlist indices in play sequence.
    order: Vec<usize>,
    /// Index into `order` of the current track, or `NONE`.
    pos: usize,
    shuffle: bool,
}

impl PlayOrder {
    /// Ordered and empty (before the first `rebuild`).
    pub fn new() -> Self {
        Self {
            order: Vec::new(),
            pos: NONE,
            shuffle: false,
        }
    }

    /// Rebuild after a playlist edit or a shuffle toggle; `current` is the
    /// playlist index being played, if any.
    ///
    /// Ordered: `order = 0..len`, cursor on `current`. Shuffle: `current`
    /// first, the rest Fisher–Yates shuffled (no current → a free shuffle and
    /// the cursor stays "before the start"). A rebuild loses shuffle history —
    /// accepted by the plan ("current track stays first").
    pub fn rebuild(&mut self, len: usize, current: Option<usize>, shuffle: bool, rng: &mut Rng) {
        self.shuffle = shuffle;
        self.order = (0..len).collect();
        let current = current.filter(|&i| i < len);
        if !shuffle {
            self.pos = current.unwrap_or(NONE);
            return;
        }
        match current {
            Some(cur) => {
                // The rest, shuffled, behind the current track at the front.
                let mut rest: Vec<usize> = (0..len).filter(|&i| i != cur).collect();
                fisher_yates(&mut rest, rng);
                rest.insert(0, cur);
                self.order = rest;
                self.pos = 0;
            }
            None => {
                fisher_yates(&mut self.order, rng);
                self.pos = NONE;
            }
        }
    }

    /// Advance and return the playlist index to play, or `None` when the order
    /// is exhausted and `repeat` is off (caller stops).
    ///
    /// - nothing current yet → the first track of the order;
    /// - ordered + repeat at the end → wraps to track 0;
    /// - shuffle + repeat at the end → a fresh permutation whose first track
    ///   differs from the last played (when `len ≥ 2`), so every track plays
    ///   again before any repeat.
    pub fn next(&mut self, repeat: bool, rng: &mut Rng) -> Option<usize> {
        let len = self.order.len();
        if len == 0 {
            return None;
        }
        if self.pos == NONE {
            self.pos = 0;
            return Some(self.order[0]);
        }
        if self.pos + 1 < len {
            self.pos += 1;
            return Some(self.order[self.pos]);
        }
        // End of the order.
        if !repeat {
            return None;
        }
        if self.shuffle {
            let last = self.order[self.pos];
            fisher_yates(&mut self.order, rng);
            if len >= 2 && self.order[0] == last {
                // The permutation holds every index once, so the second slot
                // cannot be the last-played track.
                self.order.swap(0, 1);
            }
        }
        self.pos = 0;
        Some(self.order[0])
    }

    /// Step back. At the start (or with nothing current) it returns the same /
    /// first track — the caller restarts it. `None` only when the order is
    /// empty.
    pub fn prev(&mut self) -> Option<usize> {
        if self.order.is_empty() {
            return None;
        }
        if self.pos == NONE {
            self.pos = 0;
            return Some(self.order[0]);
        }
        if self.pos > 0 {
            self.pos -= 1;
        }
        Some(self.order[self.pos])
    }

    /// A track was chosen directly (Enter in the playlist): make it current.
    ///
    /// Ordered: the cursor jumps straight to it (`pos = i`). Shuffle: it is
    /// swapped into the *next* slot's place (`pos + 1`, or the first slot with
    /// nothing current), so it plays now and the rest of the permutation
    /// continues behind it.
    pub fn jump_to(&mut self, i: usize) {
        let len = self.order.len();
        if len == 0 || i >= len {
            return;
        }
        if !self.shuffle {
            self.pos = i;
            return;
        }
        let Some(from) = self.order.iter().position(|&track| track == i) else {
            return; // `order` is a permutation of 0..len: unreachable
        };
        let slot = match self.pos {
            NONE => 0,
            pos if pos + 1 < len => pos + 1,
            // Already on the last slot: nothing behind to displace forward.
            _ => len - 1,
        };
        if from != slot {
            self.order.swap(from, slot);
        }
        self.pos = slot;
    }

    /// Look-ahead WITHOUT moving: the next `n` playlist indices, nearest
    /// first. Never wraps and never reshuffles.
    // plan 08 (Jellyfin look-ahead) consumes this; unit-tested here until then.
    #[allow(dead_code)]
    pub fn upcoming(&self, n: usize) -> Vec<usize> {
        let len = self.order.len();
        if len == 0 || n == 0 {
            return Vec::new();
        }
        let after = match self.pos {
            NONE => 0,
            pos => pos + 1,
        };
        let end = after.saturating_add(n).min(len);
        self.order.get(after..end).unwrap_or_default().to_vec()
    }

    /// Look-behind WITHOUT moving: what `prev()` would return over `n` calls,
    /// nearest first and deduplicated at the start (the restart target).
    // plan 08 (the "3 behind" cache) consumes this; unit-tested here until then.
    #[allow(dead_code)]
    pub fn previous(&self, n: usize) -> Vec<usize> {
        let len = self.order.len();
        if len == 0 || n == 0 {
            return Vec::new();
        }
        let nearest = match self.pos {
            NONE => 0,
            pos => pos.saturating_sub(1),
        };
        let first = nearest.saturating_sub(n.saturating_sub(1));
        (first..=nearest).rev().map(|i| self.order[i]).collect()
    }

    /// The playlist index the cursor sits on, if anything is current.
    #[cfg(test)]
    pub fn current(&self) -> Option<usize> {
        match self.pos {
            NONE => None,
            pos => self.order.get(pos).copied(),
        }
    }
}

/// In-place Fisher–Yates: a uniform-ish full permutation of `0..order.len()`.
fn fisher_yates(order: &mut [usize], rng: &mut Rng) {
    for i in (1..order.len()).rev() {
        let j = rng.next_below(i + 1);
        order.swap(i, j);
    }
}

impl Default for PlayOrder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEN: usize = 8;

    /// Ordered over `0..len`, nothing current yet.
    fn ordered(len: usize) -> PlayOrder {
        let mut rng = Rng::new(1);
        let mut order = PlayOrder::new();
        order.rebuild(len, None, false, &mut rng);
        order
    }

    /// Shuffled over `0..len`, nothing current yet.
    fn shuffled(len: usize, seed: u64) -> PlayOrder {
        let mut rng = Rng::new(seed);
        let mut order = PlayOrder::new();
        order.rebuild(len, None, true, &mut rng);
        order
    }

    // --- ordered ---

    #[test]
    fn ordered_next_walks_and_stops_at_the_end() {
        let mut rng = Rng::new(1);
        let mut order = ordered(LEN);
        // Nothing current → next() starts at track 0.
        assert_eq!(order.next(false, &mut rng), Some(0));
        for expected in 1..LEN {
            assert_eq!(order.next(false, &mut rng), Some(expected));
        }
        // End, repeat off → stop (None), and staying stopped doesn't move.
        assert_eq!(order.next(false, &mut rng), None);
        assert_eq!(order.next(false, &mut rng), None);
        assert_eq!(order.current(), Some(LEN - 1));
        // Repeat on → wrap to 0.
        assert_eq!(order.next(true, &mut rng), Some(0));
        assert_eq!(order.current(), Some(0));
    }

    #[test]
    fn ordered_prev_walks_back_and_restarts_at_the_start() {
        let mut rng = Rng::new(1);
        let mut order = ordered(LEN);
        // Ordered + nothing current: prev starts at 0 like next.
        assert_eq!(order.prev(), Some(0));
        for _ in 0..3 {
            order.next(false, &mut rng);
        }
        assert_eq!(order.current(), Some(3));
        assert_eq!(order.prev(), Some(2));
        assert_eq!(order.prev(), Some(1));
        assert_eq!(order.prev(), Some(0));
        // At the start: same track, caller restarts it.
        assert_eq!(order.prev(), Some(0));
        assert_eq!(order.current(), Some(0));
    }

    #[test]
    fn prev_after_three_nexts_returns_the_visited_tracks_in_reverse() {
        let mut rng = Rng::new(1);
        let mut order = ordered(LEN);
        let visited: Vec<usize> = (0..3)
            .map(|_| order.next(false, &mut rng).expect("ordered has a next"))
            .collect();
        assert_eq!(visited, [0, 1, 2]);
        let back: Vec<usize> = (0..3).map(|_| order.prev().expect("never empty")).collect();
        assert_eq!(back, [1, 0, 0], "reverse walk, restarting at the start");
    }

    // --- shuffle ---

    #[test]
    fn shuffle_yields_every_index_once_before_ending() {
        let mut rng = Rng::new(3);
        let mut order = shuffled(LEN, 3);
        let mut seen = Vec::new();
        while let Some(i) = order.next(false, &mut rng) {
            assert!(!seen.contains(&i), "repeated {i} inside one round");
            seen.push(i);
            assert!(seen.len() <= LEN, "shuffle never stopped");
        }
        seen.sort_unstable();
        assert_eq!(seen, (0..LEN).collect::<Vec<_>>(), "full permutation");
    }

    #[test]
    fn shuffle_with_repeat_starts_a_round_that_differs_at_the_front() {
        let mut rng = Rng::new(5);
        let mut order = shuffled(LEN, 5);
        let mut first_round = Vec::new();
        while let Some(i) = order.next(false, &mut rng) {
            first_round.push(i);
        }
        let last_played = *first_round.last().unwrap();
        // Repeat on at the end → a new round, first track ≠ last played.
        let second_first = order.next(true, &mut rng).expect("reshuffled");
        assert_ne!(second_first, last_played, "no immediate repeat (len ≥ 2)");
        // The new round plays every track once too.
        let mut seen = vec![second_first];
        while let Some(i) = order.next(false, &mut rng) {
            assert!(!seen.contains(&i));
            seen.push(i);
        }
        seen.sort_unstable();
        assert_eq!(seen, (0..LEN).collect::<Vec<_>>());
    }

    #[test]
    fn prev_walks_back_through_the_shuffle() {
        let mut rng = Rng::new(11);
        let mut order = shuffled(LEN, 11);
        let forward: Vec<usize> = (0..4)
            .map(|_| order.next(false, &mut rng).expect("has next"))
            .collect();
        let mut back = Vec::new();
        for _ in 0..3 {
            back.push(order.prev().expect("never empty"));
        }
        assert_eq!(back, [forward[2], forward[1], forward[0]]);
    }

    // --- jump_to ---

    #[test]
    fn jump_to_moves_the_cursor_in_ordered_mode() {
        let mut rng = Rng::new(1);
        let mut order = ordered(LEN);
        order.next(false, &mut rng); // current = 0
        order.jump_to(5);
        assert_eq!(order.current(), Some(5));
        // …and the walk continues from there.
        assert_eq!(order.next(false, &mut rng), Some(6));
        // Jumping to an out-of-range index is a no-op, not a panic.
        order.jump_to(99);
        assert_eq!(order.current(), Some(6));
    }

    #[test]
    fn jump_to_swaps_into_the_next_slot_in_shuffle_mode() {
        let mut rng = Rng::new(1);
        let mut order = PlayOrder::new();
        order.rebuild(LEN, Some(2), true, &mut rng);
        assert_eq!(order.current(), Some(2), "current first after rebuild");

        let before = order.order.clone();
        let from = before
            .iter()
            .position(|&t| t == 5)
            .expect("5 in permutation");
        let displaced = before[1]; // the track that would have played next
        order.jump_to(5);
        assert_eq!(order.current(), Some(5), "5 plays now");
        assert_eq!(order.order[1], 5, "moved into the next slot");
        assert_eq!(
            order.order[from], displaced,
            "the displaced track takes 5's old place"
        );
        let mut sorted = order.order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..LEN).collect::<Vec<_>>(), "still a permutation");
        // The walk continues after the jumped-to track.
        if from != 2 {
            assert_eq!(order.upcoming(1), vec![before[2]]);
        }
        // Nothing current yet → the jump lands in the first slot.
        let mut fresh = shuffled(LEN, 9);
        fresh.jump_to(7);
        assert_eq!(fresh.current(), Some(7));
    }

    // --- upcoming / previous ---

    #[test]
    fn upcoming_and_previous_match_what_next_and_prev_return() {
        let mut rng = Rng::new(1);
        let mut order = ordered(LEN);
        for _ in 0..4 {
            order.next(false, &mut rng);
        } // current = 3
        assert_eq!(order.upcoming(3), vec![4, 5, 6]);
        assert_eq!(order.previous(3), vec![2, 1, 0]);
        // Peek, then walk: same tracks, same order.
        let up = order.upcoming(3);
        let walked: Vec<usize> = up
            .iter()
            .map(|_| order.next(false, &mut rng).unwrap())
            .collect();
        assert_eq!(walked, up);

        // No wrap at the end, even with repeat on.
        let mut end = ordered(LEN);
        for _ in 0..LEN {
            end.next(false, &mut rng);
        }
        assert_eq!(end.upcoming(3), Vec::<usize>::new(), "no wrap");
        // Behind the start there is only the restart target.
        let start = ordered(LEN);
        assert_eq!(start.previous(3), vec![0]);
        assert_eq!(start.upcoming(3), vec![0, 1, 2]);
    }

    // --- rebuild ---

    #[test]
    fn rebuild_keeps_the_current_track_first_when_shuffled() {
        let mut rng = Rng::new(2);
        let mut order = shuffled(LEN, 2);
        for _ in 0..3 {
            order.next(false, &mut rng);
        }
        order.rebuild(LEN, Some(5), true, &mut rng);
        assert_eq!(order.current(), Some(5));
        // …and ordered rebuilds put the cursor straight on it.
        order.rebuild(LEN, Some(5), false, &mut rng);
        assert_eq!(order.current(), Some(5));
        assert_eq!(order.next(false, &mut rng), Some(6));
        // A grown playlist is picked up wholesale.
        order.rebuild(LEN + 2, Some(5), false, &mut rng);
        assert_eq!(order.current(), Some(5));
        assert_eq!(order.next(false, &mut rng), Some(6));
    }

    // --- degenerate sizes never panic ---

    #[test]
    fn empty_order_answers_none_or_empty() {
        let mut rng = Rng::new(1);
        let mut order = PlayOrder::new();
        assert_eq!(order.next(false, &mut rng), None);
        assert_eq!(order.next(true, &mut rng), None);
        assert_eq!(order.prev(), None);
        assert_eq!(order.upcoming(3), Vec::<usize>::new());
        assert_eq!(order.previous(3), Vec::<usize>::new());
        order.jump_to(0);
        assert_eq!(order.current(), None);
        // rebuild to empty from a populated state.
        order.rebuild(LEN, Some(3), true, &mut rng);
        order.rebuild(0, None, true, &mut rng);
        assert_eq!(order.next(false, &mut rng), None);
        assert_eq!(order.prev(), None);
    }

    #[test]
    fn single_track_wraps_to_itself_and_restarts() {
        let mut rng = Rng::new(1);
        for shuffle in [false, true] {
            let mut order = PlayOrder::new();
            order.rebuild(1, Some(0), shuffle, &mut rng);
            // One track is also the *last* track: repeat off → stop.
            assert_eq!(order.next(false, &mut rng), None, "single track stops");
            assert_eq!(order.next(true, &mut rng), Some(0), "repeat on: wrap");
            order.jump_to(0);
            assert_eq!(order.current(), Some(0));
            assert_eq!(order.prev(), Some(0));
        }
    }
}
