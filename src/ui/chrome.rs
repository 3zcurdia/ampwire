//! Panel chrome: rounded border with the hotkey superscript + label set into
//! the top border, btop-style, and key hints in the bottom border.

use ratatui::layout::{Margin, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders};

use crate::action::Action;
use crate::keymap::Keymap;
use crate::ui::layout::Panel;
use crate::ui::theme::{Role, Theme, glyphs};

/// Superscript char for panel hotkeys 1–3.
fn superscript(action: Action, keymap: &Keymap) -> String {
    let hint = keymap.hint(action).unwrap_or_default();
    // Map a bare digit to its superscript; anything else shows as-is.
    match hint.as_str() {
        "1" => glyphs::SUPERSCRIPTS[0].to_string(),
        "2" => glyphs::SUPERSCRIPTS[1].to_string(),
        "3" => glyphs::SUPERSCRIPTS[2].to_string(),
        other => other.to_string(),
    }
}

fn toggle_action(panel: Panel) -> Action {
    match panel {
        Panel::Player => Action::TogglePlayer,
        Panel::Eq => Action::ToggleEq,
        Panel::Playlist => Action::TogglePlaylist,
        Panel::Unknown => Action::TogglePlayer,
    }
}

/// Build the panel block: superscript hotkey + name in the top border,
/// optional key hints + status in the bottom border.
pub fn panel_block<'a>(
    panel: Panel,
    focused: bool,
    theme: &'a Theme,
    keymap: &'a Keymap,
    bottom: Option<Line<'a>>,
) -> Block<'a> {
    let border_color = if focused {
        theme.color(Role::Glow)
    } else {
        theme.color(Role::Muted)
    };
    let super_hint = superscript(toggle_action(panel), keymap);
    let name = panel.as_str();

    // Superscript in glow always (it's a hotkey); label follows focus. The
    // leading `─` keeps the corner rounded off before the label: `╭─¹player`.
    let title = Line::from(vec![
        Span::styled("─", Style::default().fg(border_color)),
        Span::styled(super_hint, Style::default().fg(theme.color(Role::Glow))),
        Span::styled(name, Style::default().fg(border_color)),
    ]);

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(title);
    if let Some(bottom) = bottom {
        block = block.title_bottom(bottom);
    }
    block
}

/// Render the "terminal too small" message.
pub fn render_too_small(area: Rect, buf: &mut ratatui::buffer::Buffer, theme: &Theme) {
    let message = format!(
        "terminal too small (need {}×{})",
        crate::ui::layout::MIN_WIDTH,
        crate::ui::layout::MIN_HEIGHT
    );
    let inner = area.inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    if inner.width == 0 {
        return;
    }
    // Truncate to fit; never write past the buffer edge.
    let text: String = message.chars().take(inner.width as usize).collect();
    let x = inner.x + (inner.width.saturating_sub(text.chars().count() as u16)) / 2;
    let y = area.y + area.height / 2;
    let style = Style::default().fg(theme.color(Role::Warn));
    for (i, c) in text.chars().enumerate() {
        let cell = &mut buf[(x + i as u16, y)];
        cell.set_symbol(&c.to_string());
        cell.set_style(style);
    }
}
