//! `?` key-help overlay, generated from the live keymap so remaps show up.
//! Sized to fit the terminal; when the entries don't fit, `↑`/`↓` scroll it
//! so no entry is ever silently cut.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

use crate::action::{Action, Scope};
use crate::app::App;
use crate::ui::theme::{Role, Theme};

const WIDTH: u16 = 62;

/// Render the help overlay centered over `full_area`, showing the window of
/// entries starting at `app.ui.help_scroll`.
pub fn render(app: &App, full_area: Rect, buf: &mut Buffer) {
    let theme = &app.theme;
    let rows = build_lines(app, theme);
    // Fit the overlay to the terminal: as tall as the content, but never
    // taller than the screen minus a row of margin.
    let wanted = rows.len() as u16 + 4; // +4 borders/title/footer
    let height = wanted.min(full_area.height.saturating_sub(1)).max(3);
    let area = centered(full_area, WIDTH, height);
    if area.height < 3 || area.width < 3 {
        return;
    }
    Clear.render(area, buf);

    let scrollable = rows.len() as u16 > area.height.saturating_sub(3);
    let footer = if scrollable {
        " ↑↓ scroll · ?/esc close "
    } else {
        " ? or esc to close "
    };
    let block = Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.color(Role::Glow)))
        .title(Line::from(vec![Span::styled(
            " key help ",
            Style::default().fg(theme.color(Role::Glow)),
        )]))
        .title_bottom(Line::from(Span::styled(
            footer,
            Style::default().fg(theme.color(Role::Muted)),
        )));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height == 0 {
        return;
    }

    // Window the entries by the scroll offset, clamped so the last page is
    // flush (no blank gap that would hide entries).
    let max_scroll = rows.len().saturating_sub(inner.height as usize);
    let scroll = app.ui.help_scroll.min(max_scroll);
    for (i, line) in rows
        .iter()
        .skip(scroll)
        .take(inner.height as usize)
        .enumerate()
    {
        line.render(
            Rect {
                x: inner.x,
                y: inner.y + i as u16,
                width: inner.width,
                height: 1,
            },
            buf,
        );
    }
}

/// Total help lines, so the key handler can clamp the scroll offset.
pub fn line_count(app: &App) -> usize {
    build_lines(app, &app.theme).len()
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

/// One line per action: `key(s)  description  (scope)`.
fn build_lines(app: &App, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut last_scope: Option<Scope> = None;

    for &action in Action::all() {
        let scope = action.scope();
        if last_scope != Some(scope) {
            lines.push(Line::from(Span::styled(
                format!("── {} ", scope_label(scope)),
                Style::default().fg(theme.color(Role::Primary)),
            )));
            last_scope = Some(scope);
        }
        let keys = app.keymap.keys_for(action);
        let keys_text = if keys.is_empty() {
            "—".to_string()
        } else {
            keys.join(", ")
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {:<14}", keys_text),
                Style::default().fg(theme.color(Role::Accent)),
            ),
            Span::styled(
                format!(" {}", action.description()),
                Style::default().fg(theme.color(Role::Fg)),
            ),
        ]));
    }
    lines
}

fn scope_label(scope: Scope) -> &'static str {
    match scope {
        Scope::Global => "global",
        Scope::Player => "player focus",
        Scope::Eq => "eq focus",
        Scope::Playlist => "playlist focus",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::keymap::Keymap;
    use crate::ui::theme;

    #[test]
    fn help_overlay_renders() {
        let app = App::new(Config::default(), Keymap::defaults(), theme::neon(), false);
        let area = Rect::new(0, 0, 80, 46);
        let mut buf = Buffer::empty(area);
        render(&app, area, &mut buf);
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        // Overlay is centered; look for its title and entries.
        assert!(text.contains("key help"), "overlay title missing");
        assert!(text.contains("quit"), "help should list quit");
        assert!(text.contains("play / pause"));
        // Rounded border corner exists somewhere inside the buffer.
        assert!(text.contains('╭'), "overlay border missing");
    }

    #[test]
    fn overlay_fits_in_small_terminals() {
        let app = App::new(Config::default(), Keymap::defaults(), theme::neon(), false);
        let area = Rect::new(0, 0, 20, 6);
        let mut buf = Buffer::empty(area);
        render(&app, area, &mut buf); // must not panic
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(text.contains("key help"), "shrunk overlay still titled");
    }

    #[test]
    fn remapped_key_shows_in_help() {
        // Remap next → "n"; the help overlay must list exactly "n" for the
        // next-track row (not the default "b", and not just any "n" that
        // happens to appear elsewhere on screen).
        let mut overrides = std::collections::HashMap::new();
        overrides.insert("next".to_string(), vec!["n".into()]);
        let (keymap, conflicts) = Keymap::from_overrides(&overrides);
        assert!(conflicts.is_empty());
        let app = App::new(Config::default(), keymap, theme::neon(), false);
        let lines = build_lines(&app, &theme::neon());
        let next_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("next track")))
            .expect("next track listed");
        // The first span is the padded keys column.
        let keys_col = next_line.spans[0].content.to_string();
        assert_eq!(
            keys_col.trim(),
            "n",
            "help shows the remapped key column: {keys_col:?}"
        );
        assert!(
            !keys_col.contains('b'),
            "default `b` is gone after remap: {keys_col:?}"
        );
    }
}

#[cfg(test)]
mod scroll_tests {
    use super::*;
    use crate::config::Config;
    use crate::keymap::Keymap;
    use crate::ui::theme;

    fn press(code: ratatui::crossterm::event::KeyCode) -> ratatui::crossterm::event::KeyEvent {
        ratatui::crossterm::event::KeyEvent::new(
            code,
            ratatui::crossterm::event::KeyModifiers::NONE,
        )
    }

    #[test]
    fn overlay_scrolls_and_nothing_is_cut_at_80x24() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = App::new(Config::default(), Keymap::defaults(), theme::neon(), false);
        app.dispatch(crate::action::Action::Help);

        // Collect the description text visible at each scroll position.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..40 {
            let backend = TestBackend::new(80, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|f| {
                    let area = f.area();
                    render(&app, area, f.buffer_mut());
                })
                .unwrap();
            let buf = terminal.backend().buffer();
            let text: String = buf
                .content()
                .iter()
                .map(|c| c.symbol().to_string())
                .collect();
            // Every action whose description appears in the overlay is seen.
            for action in Action::all() {
                if text.contains(action.description()) {
                    seen.insert(action.description().to_string());
                }
            }
            app.handle_key(press(ratatui::crossterm::event::KeyCode::Down));
        }
        // Every action description must be reachable by scrolling.
        for action in Action::all() {
            assert!(
                seen.contains(action.description()),
                "entry never visible at 80×24: {}",
                action.description()
            );
        }
    }

    #[test]
    fn scroll_down_then_up_returns_to_top() {
        let mut app = App::new(Config::default(), Keymap::defaults(), theme::neon(), false);
        app.dispatch(crate::action::Action::Help);
        assert_eq!(app.ui.help_scroll, 0);
        app.handle_key(press(ratatui::crossterm::event::KeyCode::Down));
        app.handle_key(press(ratatui::crossterm::event::KeyCode::Down));
        assert_eq!(app.ui.help_scroll, 2);
        app.handle_key(press(ratatui::crossterm::event::KeyCode::Up));
        app.handle_key(press(ratatui::crossterm::event::KeyCode::Up));
        assert_eq!(app.ui.help_scroll, 0, "up clamps at the top");
        app.handle_key(press(ratatui::crossterm::event::KeyCode::Up));
        assert_eq!(app.ui.help_scroll, 0);
    }
}
