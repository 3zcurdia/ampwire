//! Playlist panel: cursor + playing styles, scroll following the cursor,
//! overflow-only scrollbar, bottom-border totals (plan §2.3).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use crate::action::{Action, Scope};
use crate::app::App;
use crate::ui::chrome;
use crate::ui::layout::Panel;
use crate::ui::theme::Role;
use crate::ui::theme::glyphs;

/// Format a track total as `h:mm:ss` / `m:ss`.
pub fn format_duration(secs: u64) -> String {
    crate::ui::player::format_time(secs)
}

/// The cursor stays visible: scroll is derived from the cursor and the
/// visible height, never stored.
pub fn scroll_for(cursor: usize, height: usize) -> usize {
    cursor.saturating_sub(height.saturating_sub(1))
}

pub fn render(app: &App, area: Rect, buf: &mut Buffer) {
    let theme = &app.theme;
    let keymap = &app.keymap;
    let snapshot = app.snapshot();
    let focused = app.ui.focus == Scope::Playlist;
    let notice = app.ui.notice.clone();

    let total_secs: u64 = snapshot.tracks.iter().map(|t| t.duration.as_secs()).sum();
    let status = Span::styled(
        format!(
            " {} · {} ",
            snapshot.tracks.len(),
            format_duration(total_secs)
        ),
        Style::default().fg(theme.color(Role::Fg)),
    );
    let bottom = chrome::hint_line(
        &[
            (Action::Add, "add"),
            (Action::Del, "del"),
            (Action::Find, "find"),
        ],
        theme,
        keymap,
        notice.as_deref().map(|n| {
            Span::styled(
                format!(" {n} "),
                Style::default().fg(theme.color(Role::Warn)),
            )
        }),
    );
    // Append the totals after the hints.
    let bottom = if notice.is_some() {
        bottom
    } else {
        let mut line = bottom;
        line.spans.push(Span::styled(
            " ─ ",
            Style::default().fg(theme.color(Role::Muted)),
        ));
        line.spans.push(status);
        line
    };

    let block = chrome::panel_block(Panel::Playlist, focused, theme, keymap, Some(bottom));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let snapshot = app.snapshot();
    let cursor = app
        .ui
        .playlist_cursor
        .min(snapshot.tracks.len().saturating_sub(1));
    let total = snapshot.tracks.len();
    let height = inner.height as usize;
    // Keep the cursor visible; clamp so the list never scrolls past the end.
    let scroll = scroll_for(cursor, height).min(total.saturating_sub(height));

    let overflow = snapshot.tracks.len() > inner.height as usize;
    let list_width = if overflow {
        inner.width.saturating_sub(1)
    } else {
        inner.width
    };

    for row in 0..inner.height {
        let Some(index) = scroll.checked_add(row as usize) else {
            break;
        };
        let Some(track) = snapshot.tracks.get(index) else {
            break;
        };

        let y = inner.y + row;
        let is_current = snapshot.current == Some(index);
        let is_cursor = cursor == index;

        // Cursor-row background tint (muted).
        if is_cursor {
            for x in 0..list_width {
                buf[(inner.x + x, y)].set_style(Style::default().bg(theme.color(Role::Muted)));
            }
        }

        // Leading marker + number.
        let marker = if is_current { glyphs::CURSOR } else { ' ' };
        let number = format!("{:>2}.", index + 1);
        let mut x = inner.x;

        let title_style = if is_current {
            Style::default().fg(theme.color(Role::Accent))
        } else if is_cursor {
            Style::default().fg(theme.color(Role::Fg))
        } else {
            Style::default().fg(theme.color(Role::Fg)).dim()
        };

        // Marker.
        let cell = &mut buf[(x, y)];
        cell.set_symbol(&marker.to_string());
        cell.set_fg(theme.color(Role::Accent));
        x += 1;

        // Number.
        for c in number.chars() {
            if x >= inner.x + list_width {
                break;
            }
            let cell = &mut buf[(x, y)];
            cell.set_symbol(&c.to_string());
            cell.set_fg(theme.color(Role::Muted));
            x += 1;
        }
        x += 1;

        // Duration right-aligned; title truncated to the space between.
        let duration = format_duration(track.duration.as_secs());
        let duration_width = duration.width();
        let title_x = x;
        let available =
            (inner.x + list_width).saturating_sub(title_x + duration_width as u16 + 1) as usize;
        let truncated = crate::ui::player::truncate_to_width(&track.title, available);
        for c in truncated.chars() {
            let cell = &mut buf[(x, y)];
            cell.set_symbol(&c.to_string());
            cell.set_style(title_style);
            x += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) as u16;
        }
        // Duration.
        let dx = inner.x + list_width - 1 - duration_width as u16;
        for (i, c) in duration.chars().enumerate() {
            let cell = &mut buf[(dx + i as u16, y)];
            cell.set_symbol(&c.to_string());
            cell.set_fg(if is_current {
                theme.color(Role::Accent)
            } else {
                theme.color(Role::Muted)
            });
        }
    }

    // Scrollbar only when the list overflows.
    if overflow {
        render_scrollbar(app, buf, scroll, snapshot.tracks.len(), inner);
    }
}

fn render_scrollbar(app: &App, buf: &mut Buffer, scroll: usize, total: usize, inner: Rect) {
    let theme = &app.theme;
    let height = inner.height as usize;
    if height == 0 || total == 0 || inner.width == 0 {
        return;
    }
    let x = inner.x + inner.width - 1;
    let track_h = height as f32;
    let thumb_h = ((height * height) as f32 / total as f32).round().max(1.0);
    let max_scroll = (total - height).max(1) as f32;
    let thumb_pos = (scroll as f32 / max_scroll * (track_h - thumb_h)).round();

    for i in 0..inner.height {
        let in_thumb = (i as f32) >= thumb_pos && (i as f32) < thumb_pos + thumb_h;
        let cell = &mut buf[(x, inner.y + i)];
        if in_thumb {
            cell.set_symbol(&glyphs::TRACK_FULL.to_string());
            cell.set_fg(theme.color(Role::Primary));
        } else {
            cell.set_symbol("│");
            cell.set_fg(theme.color(Role::Muted));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::keymap::Keymap;
    use crate::ui::theme;
    use ratatui::style::Color;

    fn test_app() -> App {
        App::new(Config::default(), Keymap::defaults(), theme::neon(), false)
    }

    #[test]
    fn playlist_lists_tracks() {
        let app = test_app();
        let area = Rect::new(0, 0, 52, 18);
        let mut buf = Buffer::empty(area);
        render(&app, area, &mut buf);
        // Border.
        assert_eq!(buf[(0, 0)].symbol(), "╭");
        // First row inside starts with the marker column, then " 1.".
        let row1: String = (1..20u16)
            .map(|x| buf[(x, 1)].symbol().to_string())
            .collect();
        assert!(row1.contains("1."), "row: {row1:?}");
        // Track 1 title visible.
        assert!(row1.contains("Aphex"), "row: {row1:?}");
    }

    #[test]
    fn playing_track_marked_and_colored() {
        let app = test_app();
        let area = Rect::new(0, 0, 52, 18);
        let mut buf = Buffer::empty(area);
        render(&app, area, &mut buf);
        // Current track is index 2 → row 3 (y=3).
        let marker = buf.cell((1, 3)).unwrap();
        assert_eq!(marker.symbol(), glyphs::CURSOR.to_string());
        assert_eq!(marker.fg, Color::Rgb(0, 229, 255), "playing row is aqua");
    }

    #[test]
    fn scroll_follows_cursor() {
        let mut app = test_app();
        // Move the cursor far down.
        for _ in 0..30 {
            app.dispatch(Action::CursorDown);
        }
        assert_eq!(app.ui.playlist_cursor, 23); // clamped to last track
        // Derived scroll keeps the cursor visible in a 16-row window.
        let scroll = scroll_for(app.ui.playlist_cursor, 16);
        assert!(scroll + 16 > 23, "cursor must be inside the window");
    }

    #[test]
    fn truncation_is_display_width() {
        // Covered by player::truncate_to_width tests; here we ensure the
        // playlist renders a CJK title without panicking or overflowing.
        let mut app = test_app();
        app.mock_mut()
            .set_track_title(0, "日本語のとても長いタイトル");
        app.resync();
        let area = Rect::new(0, 0, 30, 8);
        let mut buf = Buffer::empty(area);
        render(&app, area, &mut buf);
        // No cell outside the buffer was touched (would have panicked).
        assert_eq!(buf[(0, 0)].symbol(), "╭");
    }
}
