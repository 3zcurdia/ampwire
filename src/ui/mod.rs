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
    fn two_column_frame_matches_mockup_shape() {
        // Plan §2.3 mockup at 104×18.
        let app = App::new(Config::default(), Keymap::defaults(), theme::neon(), false);
        let buf = render_frame(&app, 104, 18);
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();

        // Both column borders present.
        assert!(text.contains('╭'), "rounded borders");
        // Panel labels from the chrome.
        assert!(text.contains("player"));
        assert!(text.contains("eq"));
        assert!(text.contains("playlist"));
        // Player content: state glyph, transport hotkeys, format line.
        assert!(text.contains('▶'), "play glyph");
        assert!(text.contains("FLAC"), "format line");
        assert!(text.contains("bit-perfect"));
        // Playlist totals in the bottom border.
        assert!(text.contains("24"), "track count");
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
