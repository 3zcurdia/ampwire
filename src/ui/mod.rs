//! Render entry point: `render(frame, &App)` draws the full frame.

pub mod chrome;
pub mod equalizer;
pub mod help;
pub mod layout;
pub mod player;
pub mod playlist;
pub mod theme;
pub mod widgets;

use ratatui::Frame;
use ratatui::style::Color;

use crate::app::App;
use crate::ui::theme::Role;

/// Draw one frame.
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let computed = app.compute_layout(area);

    // When the theme sets a real `bg` (transparent = false), paint it across
    // the whole frame first so every panel sits on it.
    paint_background(frame, app);

    if computed.too_small {
        let theme = &app.theme;
        chrome::render_too_small(area, frame.buffer_mut(), theme);
        return;
    }

    if let Some(rect) = computed.player {
        player::render(app, rect, frame.buffer_mut(), computed.player_compact);
    }
    if let Some(rect) = computed.eq {
        equalizer::render(app, rect, frame.buffer_mut());
    }
    if let Some(rect) = computed.playlist {
        playlist::render(app, rect, frame.buffer_mut());
    }

    if app.ui.show_help {
        help::render(app, area, frame.buffer_mut());
    }
}

/// Fill the frame with the theme's `bg` role when it is an opaque color.
/// `Color::Reset` (the transparent default) is skipped so the terminal's own
/// background shows through.
fn paint_background(frame: &mut Frame, app: &App) {
    let bg = app.theme.color(Role::Bg);
    if matches!(bg, Color::Reset) {
        return;
    }
    let area = frame.area();
    let buf = frame.buffer_mut();
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_bg(bg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::config::Config;
    use crate::keymap::Keymap;
    use crate::ui::theme::{self, Role};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

    fn render_frame(app: &App, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn focused_border_uses_glow_unfocused_uses_muted() {
        let app = App::new(Config::default(), Keymap::defaults(), theme::neon(), false);
        let buf = render_frame(&app, 104, 18);
        let neon = theme::neon();
        // Player is focused by default: its top-left corner is `glow`.
        let corner = buf.cell((0, 0)).unwrap();
        assert_eq!(corner.fg, neon.color(Role::Glow), "focused border");
        // The EQ (unfocused) top-left corner is `muted`.
        let eq_corner = buf.cell((0, 9)).unwrap();
        assert_eq!(eq_corner.fg, neon.color(Role::Muted), "unfocused border");
    }

    #[test]
    fn too_small_terminal_shows_message() {
        let app = App::new(Config::default(), Keymap::defaults(), theme::neon(), false);
        let buf = render_frame(&app, 34, 10);
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(text.contains("terminal too small"), "{text}");
        assert!(text.contains("40×3"), "{text}");
    }

    #[test]
    fn single_column_and_hidden_panels_render() {
        let mut app = App::new(Config::default(), Keymap::defaults(), theme::neon(), false);
        app.dispatch(crate::action::Action::ToggleEq);
        app.dispatch(crate::action::Action::ToggleColumns);
        let buf = render_frame(&app, 40, 12);
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(!text.contains("auto"), "EQ hidden");
        assert!(text.contains("player"), "player visible");
        assert!(text.contains("playlist"), "playlist visible");
    }

    #[test]
    fn help_overlay_covers_panels() {
        let mut app = App::new(Config::default(), Keymap::defaults(), theme::neon(), false);
        app.dispatch(crate::action::Action::Help);
        let buf = render_frame(&app, 80, 46);
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(text.contains("key help"));
    }

    #[test]
    fn transparent_theme_leaves_bg_reset() {
        // neon's bg is Reset → panel cells keep the terminal background. The
        // playlist cursor row is the one place a bg tint is drawn, so check a
        // player-panel border cell instead.
        let app = App::new(Config::default(), Keymap::defaults(), theme::neon(), false);
        let buf = render_frame(&app, 104, 18);
        let neon = theme::neon();
        // Player top-left corner (a border cell, never specially tinted).
        assert_eq!(
            buf.cell((0, 0)).unwrap().bg,
            neon.color(Role::Bg),
            "transparent keeps the corner bg as Reset"
        );
        // A player interior cell too.
        assert_eq!(buf.cell((5, 2)).unwrap().bg, neon.color(Role::Bg));
    }

    #[test]
    fn opaque_theme_paints_bg_on_every_panel() {
        // With a real bg, panel cells are painted with it. The playlist cursor
        // row keeps its own muted tint, so assert on the player and eq panels.
        let bg = Color::Rgb(18, 18, 28);
        let mut opaque = theme::neon();
        opaque.override_role(Role::Bg, bg);
        let app = App::new(Config::default(), Keymap::defaults(), opaque, false);
        let buf = render_frame(&app, 104, 18);
        // Player panel: border and interior.
        assert_eq!(buf.cell((0, 0)).unwrap().bg, bg, "player border painted");
        assert_eq!(buf.cell((5, 2)).unwrap().bg, bg, "player interior painted");
        // EQ panel (starts at y=9): border and interior.
        assert_eq!(buf.cell((0, 9)).unwrap().bg, bg, "eq border painted");
        assert_eq!(buf.cell((5, 12)).unwrap().bg, bg, "eq interior painted");
        // Playlist panel (right column): a non-cursor row (cursor is on the
        // first track at y=1; y=5 is a plain row).
        assert_eq!(buf.cell((60, 5)).unwrap().bg, bg, "playlist painted");
    }

    /// Render never panics (plan §3.2): sweep the golden sizes plus boundary
    /// cases across every layout-pref and visibility combination.
    #[test]
    fn render_never_panics_across_layouts() {
        use crate::ui::layout::{Visibility, all_pref_variants};
        let sizes = [
            (1u16, 1u16),
            (39, 3),
            (40, 3),
            (52, 12),
            (80, 24),
            (91, 30),
            (104, 18),
            (160, 45),
            (200, 60),
        ];
        let vis_variants = {
            let mut v = Vec::new();
            for p in [true, false] {
                for e in [true, false] {
                    for pl in [true, false] {
                        if p || e || pl {
                            v.push(Visibility {
                                player: p,
                                eq: e,
                                playlist: pl,
                            });
                        }
                    }
                }
            }
            v
        };
        for (w, h) in sizes {
            for prefs in all_pref_variants() {
                for vis in &vis_variants {
                    let mut app =
                        App::new(Config::default(), Keymap::defaults(), theme::neon(), false);
                    app.config.layout.columns = prefs.columns;
                    app.config.layout.playlist_side = prefs.playlist_side;
                    app.config.layout.stack = prefs.stack.clone();
                    app.config.layout.single_order = prefs.single_order.clone();
                    app.ui.vis = *vis;
                    // Must not panic at any size/combination.
                    let _ = render_frame(&app, w, h);
                }
            }
        }
    }

    /// Color checks on key cells (plan §3.1): focused border = glow,
    /// playing row = accent, focused EQ knob = accent, LCD digit = primary.
    #[test]
    fn key_cells_use_the_right_roles() {
        use crate::app::DETERMINISTIC_EQ_BANDS;
        let neon = theme::neon();
        let app = App::deterministic(Keymap::defaults(), neon.clone());

        let backend = TestBackend::new(104, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &app)).unwrap();
        let buf = terminal.backend().buffer();

        // Focused border = glow. Player is focused by default (top-left).
        assert_eq!(
            buf.cell((0, 0)).unwrap().fg,
            neon.color(Role::Glow),
            "focused border uses glow"
        );
        // Unfocused border = muted. EQ panel top-left (y=9).
        assert_eq!(
            buf.cell((0, 9)).unwrap().fg,
            neon.color(Role::Muted),
            "unfocused border uses muted"
        );
        // LCD digit = primary. The time digits sit in the player's inner rows;
        // find a primary-colored block glyph there.
        let mut digit = false;
        for y in 1..3u16 {
            for x in 2..14u16 {
                let c = buf.cell((x, y)).unwrap();
                if c.fg == neon.color(Role::Primary) && c.symbol() != " " {
                    digit = true;
                }
            }
        }
        assert!(digit, "LCD digit uses primary");
        // Focused EQ knob = accent. The EQ focus defaults to band 1 with a
        // nonzero gain, so its knob is accent-colored.
        let mut knob = false;
        for y in 3..7u16 {
            for x in 4..52u16 {
                let c = buf.cell((x, y)).unwrap();
                if c.symbol() == "●" && c.fg == neon.color(Role::Accent) {
                    knob = true;
                }
            }
        }
        assert!(knob, "focused EQ knob uses accent");
        // Playing row = accent. Track 3 is playing (deterministic); its title
        // in the playlist is accent-colored.
        let mut playing = false;
        for x in 53..104u16 {
            let c = buf.cell((x, 3)).unwrap();
            if c.fg == neon.color(Role::Accent) && c.symbol() != " " {
                playing = true;
            }
        }
        assert!(playing, "playing row uses accent");
        // Sanity: the deterministic EQ bands are the fixed fixture.
        assert_eq!(app.ui.eq_bands, DETERMINISTIC_EQ_BANDS);
    }
}

/// Golden frames: exact-text comparisons against `tests/golden/*.txt`, using
/// the deterministic Phase 0 state. Run `UPDATE_GOLDEN=1 cargo test golden` to
/// regenerate after an intentional visual change (then eyeball the diff).
#[cfg(test)]
mod golden {
    use super::*;
    use crate::app::App;
    use crate::keymap::Keymap;
    use crate::ui::theme;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    /// The sizes the plan calls out, plus the two-column mockup size.
    const SIZES: &[(&str, u16, u16)] = &[
        ("104x18", 104, 18),
        ("160x45", 160, 45),
        ("80x24", 80, 24),
        ("52x12", 52, 12),
        ("40x3", 40, 3),
    ];

    fn golden_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
    }

    /// Render the deterministic app at the given size into plain text lines.
    fn render_lines(w: u16, h: u16) -> Vec<String> {
        let app = App::deterministic(Keymap::defaults(), theme::neon());
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &app)).unwrap();
        let buf = terminal.backend().buffer();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn golden_frames_match() {
        let update = std::env::var_os("UPDATE_GOLDEN").is_some();
        let dir = golden_dir();
        if update {
            std::fs::create_dir_all(&dir).unwrap();
        }
        for (name, w, h) in SIZES {
            let actual = render_lines(*w, *h);
            let path = dir.join(format!("{name}.txt"));
            if update {
                std::fs::write(&path, format!("{}\n", actual.join("\n"))).unwrap();
                continue;
            }
            let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                panic!(
                    "missing golden {}: {e}\nrun UPDATE_GOLDEN=1 cargo test golden",
                    path.display()
                )
            });
            let expected: Vec<&str> = expected.lines().collect();
            assert_eq!(
                actual, expected,
                "golden {name} mismatch — run UPDATE_GOLDEN=1 cargo test golden and eyeball the diff"
            );
        }
    }
}

#[cfg(test)]
mod visual_dump {
    #[test]
    #[ignore]
    fn dump_frame() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let app = crate::app::App::new(
            crate::config::Config::default(),
            crate::keymap::Keymap::defaults(),
            crate::ui::theme::neon(),
            false,
        );
        let backend = TestBackend::new(104, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(f, &app)).unwrap();
        let buf = terminal.backend().buffer();
        for y in 0..18u16 {
            let line: String = (0..104u16)
                .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                .collect();
            println!("{line}");
        }
    }
}
