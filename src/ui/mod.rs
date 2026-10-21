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

use crate::app::App;

/// Draw one frame.
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let computed = app.compute_layout(area);

    if computed.too_small {
        let theme = &app.theme;
        // Wipe to bg first so the message stands alone.
        frame.render_widget(ratatui::widgets::Clear, area);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::config::Config;
    use crate::keymap::Keymap;
    use crate::ui::theme::{self, Role};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
