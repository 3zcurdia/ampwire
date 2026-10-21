//! `?` key-help overlay, generated from the live keymap so remaps show up.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};

use crate::action::{Action, Scope};
use crate::app::App;
use crate::ui::theme::{Role, Theme};

const WIDTH: u16 = 62;

/// Render the help overlay centered over `full_area`.
pub fn render(app: &App, full_area: Rect, buf: &mut Buffer) {
    let theme = &app.theme;
    let area = centered(full_area, WIDTH, entry_count() as u16 + 4);
    Clear.render(area, buf);

    let block = Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.color(Role::Glow)))
        .title(Line::from(vec![Span::styled(
            " key help ",
            Style::default().fg(theme.color(Role::Glow)),
        )]))
        .title_bottom(Line::from(Span::styled(
            " ? or esc to close ",
            Style::default().fg(theme.color(Role::Muted)),
        )));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height == 0 {
        return;
    }

    let rows: Vec<Line> = build_lines(app, theme);
    for (i, line) in rows.iter().take(inner.height as usize).enumerate() {
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

fn entry_count() -> usize {
    Action::all().len()
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
        let text: String = next_line
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(text.contains('n'), "remapped key shows: {text}");
    }
}
