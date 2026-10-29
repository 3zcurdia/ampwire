//! `p` key preset picker overlay (plan 07): a centered list of the EQ
//! presets; moving the cursor previews each one live. Same structure as the
//! `?` help overlay (rounded border, `Glow` colour) — drawn on top of
//! everything, right after help.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

use crate::app::App;
use crate::player::eq_presets::{self, PRESETS};
use crate::ui::theme::Role;

const WIDTH: u16 = 30;

/// Render the preset picker centered over `full_area`, keeping the cursor
/// row visible: on a short terminal the list scrolls under a derived offset
/// (no state — `render` only reads the app).
pub fn render(app: &App, full_area: Rect, buf: &mut Buffer) {
    let Some(picker) = app.ui.presets.as_ref() else {
        return;
    };
    let theme = &app.theme;
    // Fit the overlay to the terminal: as tall as the content, but never
    // taller than the screen minus a row of margin.
    let wanted = PRESETS.len() as u16 + 4; // +4 borders/title/footer
    let height = wanted.min(full_area.height.saturating_sub(1)).max(3);
    let area = centered(full_area, WIDTH, height);
    if area.height < 3 || area.width < 3 {
        return;
    }
    Clear.render(area, buf);

    // Shortened footer when the border is too narrow for the full hint.
    let footer = if area.width >= 34 {
        " ↑↓ preview · enter apply · esc cancel "
    } else {
        " enter · esc "
    };
    let block = Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.color(Role::Glow)))
        .title(Line::from(vec![Span::styled(
            " eq presets ",
            Style::default().fg(theme.color(Role::Glow)),
        )]))
        .title_bottom(Line::from(Span::styled(
            footer,
            Style::default().fg(theme.color(Role::Muted)),
        )));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // Scroll so the cursor row stays visible on a short terminal.
    let rows = inner.height as usize;
    let scroll = picker.cursor.saturating_sub(rows.saturating_sub(1));
    // The preset the EQ came from gets a ● marker (the saved, pre-preview
    // state — not the live preview, which the cursor itself shows).
    let origin = eq_presets::find_matching(&picker.saved_gains);
    for (i, preset) in PRESETS.iter().enumerate().skip(scroll).take(rows) {
        let y = inner.y + (i - scroll) as u16;
        let cursor_row = i == picker.cursor;
        let marker_row = origin == Some(i);
        let name_x = inner.x + 2;
        let marker_x = inner.x + inner.width.saturating_sub(1);

        // Name: `▸ Name` on the cursor row (accent), two spaces otherwise.
        let prefix = if cursor_row { "▸ " } else { "  " };
        let style = if cursor_row {
            Style::default().fg(theme.color(Role::Accent))
        } else {
            Style::default().fg(theme.color(Role::Fg))
        };
        let line = Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(preset.name.to_string(), style),
        ]);
        let row = Rect {
            x: name_x,
            y,
            width: inner.width.saturating_sub(2),
            height: 1,
        };
        line.render(row, buf);

        // `●` on the row matching the EQ the picker was opened with.
        if marker_row && marker_x > name_x + 2 && marker_x < inner.x + inner.width {
            let cell = &mut buf[(marker_x, y)];
            cell.set_symbol("●");
            cell.set_fg(theme.color(Role::Muted));
        }
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(1);
    let height = height.min(area.height.saturating_sub(2)).max(1);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::keymap::Keymap;
    use crate::ui::theme;

    fn test_app() -> App {
        App::new(Config::default(), Keymap::defaults(), theme::neon(), false)
    }

    fn buffer_text(buf: &Buffer) -> String {
        buf.content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect()
    }

    #[test]
    fn overlay_lists_presets_at_80x24() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = test_app();
        app.dispatch(crate::action::Action::EqPresets);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| super::render(&app, f.area(), f.buffer_mut()))
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("eq presets"), "title: {text}");
        assert!(text.contains("Full Bass & Treble"), "{text}");
        assert!(text.contains("Jazz"), "{text}");
        assert!(text.contains("Folk"), "{text}");
        // Rounded border, like the help overlay.
        assert!(text.contains('╭'), "border: {text}");
    }

    #[test]
    fn small_terminal_keeps_the_cursor_visible() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = test_app();
        app.dispatch(crate::action::Action::EqPresets);
        // Walk the cursor to the last preset, then draw at 40×8: the list is
        // clipped but the cursor row must be on screen and nothing panics.
        for _ in 0..PRESETS.len() {
            app.handle_key(press(ratatui::crossterm::event::KeyCode::Down));
        }
        let last = PRESETS.len() - 1;
        assert_eq!(app.ui.presets.unwrap().cursor, last);
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| super::render(&app, f.area(), f.buffer_mut()))
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("Folk"), "cursor row visible: {text}");
        assert!(!text.contains("Flat"), "scrolled off the top: {text}");
    }

    #[test]
    fn marker_shows_the_origin_preset_and_closed_picker_renders_nothing() {
        let mut app = test_app();
        // Custom EQ: opening the picker matches no preset, so no ● row.
        app.ui.eq_bands = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
        app.dispatch(crate::action::Action::EqPresets);
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        render(&app, area, &mut buf);
        assert!(
            !buffer_text(&buf).contains('●'),
            "no marker for a custom EQ"
        );

        // Starting from Rock marks Rock's row (before any preview).
        let mut app = test_app();
        app.ui.eq_bands = crate::player::eq_presets::PRESETS[5].gains();
        app.dispatch(crate::action::Action::EqPresets);
        let mut buf = Buffer::empty(area);
        render(&app, area, &mut buf);
        let text = buffer_text(&buf);
        assert!(text.contains('●'), "origin marker: {text}");

        // Closed picker renders nothing at all.
        app.dispatch(crate::action::Action::EqPresets); // cancel via `p`
        let mut buf = Buffer::empty(area);
        render(&app, area, &mut buf);
        assert!(!buffer_text(&buf).contains("eq presets"));
    }

    fn press(code: ratatui::crossterm::event::KeyCode) -> ratatui::crossterm::event::KeyEvent {
        ratatui::crossterm::event::KeyEvent::new(
            code,
            ratatui::crossterm::event::KeyModifiers::NONE,
        )
    }
}
