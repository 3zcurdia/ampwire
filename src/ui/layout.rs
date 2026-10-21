//! Layout engine: a pure function from terminal area + prefs + visibility to
//! panel rects, with automatic fallbacks for small terminals (plan §2.2).
//!
//! Fallbacks never mutate the saved preference — they only affect the rects
//! computed for this frame.

use ratatui::layout::{Constraint, Layout, Rect};
use serde::{Deserialize, Serialize};

/// Natural width of the left column (player + EQ).
pub const NATURAL_LEFT_WIDTH: u16 = 52;
/// Minimum playlist width before we force single column.
pub const MIN_PLAYLIST_WIDTH: u16 = 40;
/// Width below which nothing fits.
pub const MIN_WIDTH: u16 = 40;
/// Height below which nothing fits.
pub const MIN_HEIGHT: u16 = 3;
/// Full player panel height (borders included).
pub const PLAYER_HEIGHT: u16 = 9;
/// Compact player: time, marquee, seek bar.
pub const COMPACT_PLAYER_HEIGHT: u16 = 3;
/// EQ panel height with default sliders.
pub const EQ_HEIGHT: u16 = 9;
/// Minimum EQ total height (rows including borders) before it is dropped
/// rather than rendered squashed: 3 inner rows + 2 border rows.
pub const MIN_EQ_TOTAL: u16 = 5;
/// Playlist minimum height (borders included).
pub const MIN_PLAYLIST_HEIGHT: u16 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Panel {
    Player,
    Eq,
    Playlist,
    #[serde(other)]
    Unknown,
}

impl Panel {
    pub fn as_str(self) -> &'static str {
        match self {
            Panel::Player => "player",
            Panel::Eq => "eq",
            Panel::Playlist => "playlist",
            Panel::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PlaylistSide {
    #[default]
    Right,
    Left,
}

impl PlaylistSide {
    pub fn as_str(self) -> &'static str {
        match self {
            PlaylistSide::Right => "right",
            PlaylistSide::Left => "left",
        }
    }
}

/// What the user asked for, as saved in `[layout]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutPrefs {
    pub columns: u8,
    pub playlist_side: PlaylistSide,
    /// Order of the left stack in two-column mode.
    pub stack: Vec<Panel>,
    /// Panel order in single-column mode.
    pub single_order: Vec<Panel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Visibility {
    pub player: bool,
    pub eq: bool,
    pub playlist: bool,
}

impl Visibility {
    pub fn all() -> Self {
        Self {
            player: true,
            eq: true,
            playlist: true,
        }
    }

    pub fn is_visible(self, panel: Panel) -> bool {
        match panel {
            Panel::Player => self.player,
            Panel::Eq => self.eq,
            Panel::Playlist => self.playlist,
            Panel::Unknown => false,
        }
    }

    pub fn set_visible(&mut self, panel: Panel, visible: bool) {
        match panel {
            Panel::Player => self.player = visible,
            Panel::Eq => self.eq = visible,
            Panel::Playlist => self.playlist = visible,
            Panel::Unknown => {}
        }
    }
}

/// The resolved per-frame layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComputedLayout {
    pub player: Option<Rect>,
    pub eq: Option<Rect>,
    pub playlist: Option<Rect>,
    /// The player renders compact (3 rows) instead of full.
    pub player_compact: bool,
    /// Terminal below 40×3: render the centered "too small" message instead.
    pub too_small: bool,
}

/// Compute the layout for one frame. Pure: same inputs, same rects.
pub fn compute_layout(area: Rect, prefs: &LayoutPrefs, vis: &Visibility) -> ComputedLayout {
    let empty = ComputedLayout {
        player: None,
        eq: None,
        playlist: None,
        player_compact: false,
        too_small: false,
    };

    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        return ComputedLayout {
            too_small: true,
            ..empty
        };
    }

    // Narrow terminal → single column, whatever the setting.
    let two_column =
        prefs.columns == 2 && area.width >= NATURAL_LEFT_WIDTH + MIN_PLAYLIST_WIDTH && vis.playlist;

    if two_column {
        compute_two_column(area, prefs, vis)
    } else {
        compute_single_column(area, prefs, vis)
    }
}

fn split_columns(area: Rect, left_width: u16, playlist_side: PlaylistSide) -> (Rect, Rect) {
    let (stack_area, playlist_area) = match playlist_side {
        PlaylistSide::Right => {
            let [stack, playlist] =
                Layout::horizontal([Constraint::Length(left_width), Constraint::Fill(1)])
                    .areas(area);
            (stack, playlist)
        }
        PlaylistSide::Left => {
            let [playlist, stack] =
                Layout::horizontal([Constraint::Fill(1), Constraint::Length(left_width)])
                    .areas(area);
            (stack, playlist)
        }
    };
    (stack_area, playlist_area)
}

fn compute_two_column(area: Rect, prefs: &LayoutPrefs, vis: &Visibility) -> ComputedLayout {
    let (stack_area, playlist_area) = split_columns(
        area,
        NATURAL_LEFT_WIDTH.min(area.width.saturating_sub(MIN_PLAYLIST_WIDTH)),
        prefs.playlist_side,
    );

    // Visible panels in the stack, in user order.
    let stack: Vec<Panel> = prefs
        .stack
        .iter()
        .copied()
        .filter(|p| vis.is_visible(*p))
        .collect();

    let mut player = None;
    let mut eq = None;
    let mut player_compact = false;

    match stack.as_slice() {
        [] => {}
        // Alone: natural height, compact if very short.
        [only] => {
            let height = match *only {
                Panel::Player => {
                    let compact = stack_area.height < PLAYER_HEIGHT;
                    player_compact = compact;
                    if compact {
                        stack_area.height.min(COMPACT_PLAYER_HEIGHT)
                    } else {
                        PLAYER_HEIGHT.min(stack_area.height)
                    }
                }
                _ => stack_area.height.min(EQ_HEIGHT),
            };
            let rect = Rect {
                height,
                ..stack_area
            };
            match only {
                Panel::Player => player = Some(rect),
                _ => eq = Some(rect),
            }
        }
        // Player and EQ: player keeps its height; spare goes to EQ.
        _ => {
            let player_height = if stack_area.height < PLAYER_HEIGHT {
                player_compact = true;
                stack_area.height.min(COMPACT_PLAYER_HEIGHT)
            } else {
                PLAYER_HEIGHT.min(stack_area.height)
            };
            // Spare height goes to the EQ. A short terminal shrinks it, and
            // the EQ is dropped entirely rather than rendered as a squashed
            // box with fewer than 3 inner rows (≥5 with borders). This is a
            // frame-only fallback; the saved visibility is untouched.
            let remaining = stack_area.height.saturating_sub(player_height);
            let eq_height = if remaining >= EQ_HEIGHT {
                EQ_HEIGHT.max(remaining)
            } else {
                remaining
            };
            let show_eq = eq_height >= MIN_EQ_TOTAL;

            let mut y = stack_area.y;
            for panel in &stack {
                let rect = match panel {
                    Panel::Player => Rect {
                        height: player_height,
                        y,
                        ..stack_area
                    },
                    _ => Rect {
                        height: eq_height,
                        y,
                        ..stack_area
                    },
                };
                y += rect.height;
                match panel {
                    Panel::Player => player = Some(rect),
                    _ => {
                        if show_eq {
                            eq = Some(rect);
                        }
                    }
                }
            }
        }
    }

    ComputedLayout {
        player,
        eq,
        playlist: Some(playlist_area),
        player_compact,
        too_small: false,
    }
}

fn compute_single_column(area: Rect, prefs: &LayoutPrefs, vis: &Visibility) -> ComputedLayout {
    let visible: Vec<Panel> = prefs
        .single_order
        .iter()
        .copied()
        .filter(|p| vis.is_visible(*p))
        .collect();

    // Fallback order (plan §2.2):
    // 1. the playlist shrinks to MIN_PLAYLIST_HEIGHT (already assumed below),
    // 2. then hide the EQ,
    // 3. then the player goes compact.
    let mut effective = *vis;
    let natural: u16 = visible
        .iter()
        .map(|p| match p {
            Panel::Player => PLAYER_HEIGHT,
            Panel::Eq => EQ_HEIGHT,
            _ => MIN_PLAYLIST_HEIGHT,
        })
        .sum();
    if natural > area.height {
        effective.eq = false;
    }

    // Recompute after hiding the EQ.
    let visible: Vec<Panel> = prefs
        .single_order
        .iter()
        .copied()
        .filter(|p| effective.is_visible(*p))
        .collect();
    let reduced: u16 = visible
        .iter()
        .map(|p| match p {
            Panel::Player => PLAYER_HEIGHT,
            Panel::Eq => EQ_HEIGHT,
            _ => MIN_PLAYLIST_HEIGHT,
        })
        .sum();
    // Step 3: still too short → compact player.
    let compact = reduced > area.height && visible.contains(&Panel::Player);

    // Assign rects top to bottom; the playlist absorbs the leftover height.
    let fixed_height = |p: Panel| match p {
        Panel::Player => {
            if compact {
                COMPACT_PLAYER_HEIGHT
            } else {
                PLAYER_HEIGHT
            }
        }
        Panel::Eq => EQ_HEIGHT,
        _ => MIN_PLAYLIST_HEIGHT,
    };
    let fixed_sum: u16 = visible.iter().map(|p| fixed_height(*p)).sum();
    let leftover = area.height.saturating_sub(fixed_sum);

    let mut player = None;
    let mut eq = None;
    let mut playlist = None;
    let mut y = area.y;
    for panel in &visible {
        let mut height = fixed_height(*panel);
        if *panel == Panel::Playlist {
            height += leftover;
        }
        // A panel whose minimum doesn't fit is dropped (e.g. 40×3 shows
        // only the compact player).
        if y >= area.y + area.height {
            break;
        }
        let height = height.min(area.y + area.height - y);
        let rect = Rect { y, height, ..area };
        y += height;
        match panel {
            Panel::Player => player = Some(rect),
            Panel::Eq => eq = Some(rect),
            _ => playlist = Some(rect),
        }
    }

    ComputedLayout {
        player,
        eq,
        playlist,
        player_compact: compact,
        too_small: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefs() -> LayoutPrefs {
        LayoutPrefs {
            columns: 2,
            playlist_side: PlaylistSide::Right,
            stack: vec![Panel::Player, Panel::Eq],
            single_order: vec![Panel::Player, Panel::Eq, Panel::Playlist],
        }
    }

    fn area(w: u16, h: u16) -> Rect {
        Rect::new(0, 0, w, h)
    }

    #[test]
    fn two_column_at_104x18() {
        let l = compute_layout(area(104, 18), &prefs(), &Visibility::all());
        let p = l.player.expect("player");
        let eq = l.eq.expect("eq");
        let pl = l.playlist.expect("playlist");
        assert_eq!(p, Rect::new(0, 0, 52, 9));
        assert_eq!(eq, Rect::new(0, 9, 52, 9));
        assert_eq!(pl, Rect::new(52, 0, 52, 18));
        assert!(!l.player_compact);
        assert!(!l.too_small);
    }

    #[test]
    fn two_column_spare_height_goes_to_eq() {
        // 160×45: left column 52 wide, player 9, EQ takes the rest.
        let l = compute_layout(area(160, 45), &prefs(), &Visibility::all());
        let p = l.player.unwrap();
        let eq = l.eq.unwrap();
        let pl = l.playlist.unwrap();
        assert_eq!(p.height, 9);
        assert_eq!(eq.height + p.height, 45);
        assert_eq!(pl.width, 160 - 52);
    }

    #[test]
    fn narrow_terminal_forces_single_column() {
        // 91 < 52 + 40 → single column even though columns = 2.
        let l = compute_layout(area(91, 30), &prefs(), &Visibility::all());
        let p = l.player.unwrap();
        let eq = l.eq.unwrap();
        let pl = l.playlist.unwrap();
        assert_eq!(p.width, 91);
        assert_eq!(eq.width, 91);
        assert_eq!(pl.width, 91);
        // Stacked: player 9, eq 9, playlist the rest.
        assert_eq!(p.y, 0);
        assert_eq!(eq.y, 9);
        assert_eq!(pl.y, 18);
        assert_eq!(pl.height, 30 - 18);
    }

    #[test]
    fn short_single_column_shrinks_playlist_then_hides_eq() {
        // 52×14 is exactly player 9 + playlist 5 after the EQ is hidden.
        let l = compute_layout(area(52, 14), &prefs(), &Visibility::all());
        assert!(l.player.is_some());
        assert!(l.eq.is_none(), "EQ hidden on short terminal");
        assert!(!l.player_compact);
        let p = l.player.unwrap();
        assert_eq!(p.height, 9);
        let pl = l.playlist.unwrap();
        assert_eq!(pl.height, 5, "playlist at its minimum");

        // 52×12: 9 + 5 = 14 > 12 → the player drops to compact as well.
        let l = compute_layout(area(52, 12), &prefs(), &Visibility::all());
        assert!(l.eq.is_none());
        assert!(l.player_compact, "still too short → compact player");
        assert_eq!(l.player.unwrap().height, 3);
        assert_eq!(l.playlist.unwrap().height, 9);
    }

    #[test]
    fn very_short_terminal_compacts_player() {
        // 52×8: after hiding EQ, 9 + 5 = 14 > 8 → compact player (3).
        let l = compute_layout(area(52, 8), &prefs(), &Visibility::all());
        assert!(l.player_compact);
        let p = l.player.unwrap();
        assert_eq!(p.height, 3);
        let pl = l.playlist.unwrap();
        assert_eq!(pl.height, 5, "playlist at its minimum");
    }

    #[test]
    fn two_column_drops_eq_when_too_squat() {
        // Two-column at 104×N: player keeps 9 rows when it can, leaving the
        // EQ `remaining` rows. Below MIN_EQ_TOTAL (5) the EQ is dropped, not
        // rendered as a squashed box.
        // 104×14: player 9, remaining 5 → EQ shown (exactly at the floor).
        let l = compute_layout(area(104, 14), &prefs(), &Visibility::all());
        assert!(l.player.is_some());
        assert!(!l.player_compact);
        assert!(l.eq.is_some(), "EQ shown at the 5-row floor");
        // 104×13: player 9, remaining 4 → EQ dropped.
        let l = compute_layout(area(104, 13), &prefs(), &Visibility::all());
        assert!(l.player.is_some());
        assert!(l.eq.is_none(), "EQ dropped below the floor");
        // 104×10: player 9, remaining 1 → EQ dropped.
        let l = compute_layout(area(104, 10), &prefs(), &Visibility::all());
        assert!(l.eq.is_none(), "EQ dropped when only 1 row remains");
        // 104×8: below PLAYER_HEIGHT → player compact (3), remaining 5 →
        // the EQ fits again.
        let l = compute_layout(area(104, 8), &prefs(), &Visibility::all());
        assert!(l.player_compact);
        assert!(l.eq.is_some(), "compact player frees room for the EQ");
    }

    #[test]
    fn too_small_message() {
        let l = compute_layout(area(39, 24), &prefs(), &Visibility::all());
        assert!(l.too_small);
        let l = compute_layout(area(80, 2), &prefs(), &Visibility::all());
        assert!(l.too_small);
        // Exactly 40×3 is the minimum that still renders.
        let l = compute_layout(area(40, 3), &prefs(), &Visibility::all());
        assert!(!l.too_small);
    }

    #[test]
    fn playlist_hidden_renders_stack_alone() {
        let mut vis = Visibility::all();
        vis.playlist = false;
        let l = compute_layout(area(104, 18), &prefs(), &vis);
        assert!(l.playlist.is_none());
        assert!(l.player.is_some());
        assert!(l.eq.is_some());
    }

    #[test]
    fn swapped_playlist_side() {
        let mut p = prefs();
        p.playlist_side = PlaylistSide::Left;
        let l = compute_layout(area(104, 18), &p, &Visibility::all());
        let pl = l.playlist.unwrap();
        let p_rect = l.player.unwrap();
        assert_eq!(pl.x, 0);
        assert_eq!(p_rect.x, 52);
    }

    #[test]
    fn hidden_panel_gives_space_to_others() {
        // Hide the player → the EQ alone takes the stack.
        let mut vis = Visibility::all();
        vis.player = false;
        let l = compute_layout(area(104, 18), &prefs(), &vis);
        assert!(l.player.is_none());
        let eq = l.eq.unwrap();
        assert_eq!(eq.height, 9);
        assert!(l.playlist.is_some());
    }

    #[test]
    fn single_column_mode_respected_when_wide() {
        let mut p = prefs();
        p.columns = 1;
        let l = compute_layout(area(160, 45), &p, &Visibility::all());
        let p_rect = l.player.unwrap();
        assert_eq!(p_rect.width, 160, "panels stretch to full width");
    }

    #[test]
    fn single_column_order_is_respected() {
        let mut p = prefs();
        p.columns = 1;
        p.single_order = vec![Panel::Playlist, Panel::Player, Panel::Eq];
        let l = compute_layout(area(80, 24), &p, &Visibility::all());
        let pl = l.playlist.unwrap();
        let player = l.player.unwrap();
        let eq = l.eq.unwrap();
        assert_eq!(pl.y, 0);
        assert!(player.y > pl.y);
        assert!(eq.y > player.y);
    }
}
