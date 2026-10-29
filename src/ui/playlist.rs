//! Playlist panel: cursor + playing styles, scroll following the cursor,
//! overflow-only scrollbar, bottom-border totals (plan §2.3).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use crate::action::{Action, Scope};
use crate::app::App;
use crate::keymap::Keymap;
use crate::ui::chrome;
use crate::ui::layout::Panel;
use crate::ui::theme::glyphs;
use crate::ui::theme::{Role, Theme};

/// Format a track total as `h:mm:ss` / `m:ss`.
pub fn format_duration(secs: u64) -> String {
    crate::ui::player::format_time(secs)
}

/// Adjust the stored scroll so the cursor stays visible: the window only
/// moves when the cursor would leave it, and never scrolls past the end.
pub fn scroll_for(scroll: usize, cursor: usize, height: usize, total: usize) -> usize {
    let scroll = if cursor < scroll {
        cursor
    } else if cursor >= scroll + height {
        (cursor + 1).saturating_sub(height)
    } else {
        scroll
    };
    scroll.min(total.saturating_sub(height))
}

/// Build the playlist bottom-border line: key hints, then an optional notice
/// (truncated by display width with `…`, first line only), then the track
/// count and total time. `width` is the available inner border width; the
/// count and total are never dropped: the notice goes first, then the hints
/// from the right.
fn playlist_bottom_border<'a>(
    theme: &'a Theme,
    keymap: &'a Keymap,
    notice: Option<&'a str>,
    totals: &'a str,
    width: usize,
) -> Line<'a> {
    let muted = Style::default().fg(theme.color(Role::Muted));
    let mut spans: Vec<Span<'a>> = Vec::new();

    // The fixed tail: `─ <totals> ─`, reserved so it always fits.
    let totals_tail: Vec<Span<'a>> = vec![
        Span::styled("─ ", muted),
        Span::styled(
            totals.to_string(),
            Style::default().fg(theme.color(Role::Fg)),
        ),
        Span::styled(" ─", muted),
    ];
    let tail_width: usize = totals_tail.iter().map(|s| s.content.width()).sum();

    // Hints: `─ a add ─ d del ─ / find ─`, each kept only while it fits.
    let hints: [(Action, &str); 3] = [
        (Action::Add, "add"),
        (Action::Del, "del"),
        (Action::Find, "find"),
    ];
    let mut head_width = 0usize;
    for (action, label) in hints {
        let key = keymap.hint(action).unwrap_or_default();
        let hint = [
            Span::styled("─ ", muted),
            Span::styled(key, Style::default().fg(theme.color(Role::Accent))),
            Span::styled(format!(" {label} "), muted),
        ];
        let hint_width: usize = hint.iter().map(|s| s.content.width()).sum();
        if head_width + hint_width + tail_width > width {
            break;
        }
        head_width += hint_width;
        spans.extend(hint);
    }

    if let Some(notice) = notice {
        // Collapse multi-line TOML errors to their first line.
        let first_line = notice.lines().next().unwrap_or(notice);
        let available = width
            .saturating_sub(head_width)
            .saturating_sub(tail_width)
            .saturating_sub(3); // `─ ` before the notice, a space after
        if available >= 4 {
            let truncated = truncate_ellipsis(first_line, available);
            spans.push(Span::styled("─ ", muted));
            spans.push(Span::styled(
                format!("{truncated} "),
                Style::default().fg(theme.color(Role::Warn)),
            ));
        }
        // If there's no room at all, drop the notice — the totals win.
    }

    spans.extend(totals_tail);
    Line::from(spans)
}

/// Truncate `text` to at most `width` display columns, appending `…` when it
/// had to cut. Never splits a character.
fn truncate_ellipsis(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if text.width() <= width {
        return text.to_string();
    }
    let budget = width.saturating_sub(1); // room for the ellipsis
    let mut out = String::new();
    let mut used = 0usize;
    for c in text.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

#[allow(
    clippy::too_many_lines,
    reason = "one draw pass for the playlist rows, footer and scrollbar"
)]
pub fn render(app: &App, area: Rect, buf: &mut Buffer) {
    let theme = &app.theme;
    let keymap = &app.keymap;
    let snapshot = app.snapshot();
    let focused = app.ui.focus == Scope::Playlist;

    // Totals always count the *whole* playlist, filtered or not; unknown
    // durations are excluded from the sum.
    let totals = format!(
        "{} · {}",
        snapshot.tracks.len(),
        format_duration(app.playlist_seconds())
    );
    // The notice slot: an open prompt types here, else the active filter,
    // else whatever notice was left (truncated; totals always fit).
    let prompt_or_notice = match &app.ui.prompt {
        Some(prompt) => Some(format!("{}: {}▏", prompt.label(), prompt.text)),
        None => match &app.ui.filter {
            Some(filter) => Some(format!("filter: {filter}")),
            None => app.ui.notice.clone(),
        },
    };
    // Layout: `─ a add ─ d del ─ / find ── <notice…> ── 24 · 1:56:43 ─`.
    // The count and total always survive; the notice is truncated first, and
    // multi-line TOML errors collapse to their first line.
    let bottom = playlist_bottom_border(
        theme,
        keymap,
        prompt_or_notice.as_deref(),
        &totals,
        area.width.saturating_sub(2) as usize,
    );

    let block = chrome::panel_block(Panel::Playlist, focused, theme, keymap, Some(bottom));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let snapshot = app.snapshot();
    // The cursor addresses the visible (filtered) list, not raw tracks.
    let visible = app.visible_indices();
    let cursor = app.ui.playlist_cursor.min(visible.len().saturating_sub(1));
    let height = inner.height as usize;
    // The app keeps the scroll in sync before each frame; re-derive it here
    // so the cursor is visible even when rendered without that (tests).
    let scroll = scroll_for(app.ui.playlist_scroll, cursor, height, visible.len());

    // Empty (or everything filtered out): one dim hint row, no scrollbar.
    if visible.is_empty() {
        let hint = if snapshot.tracks.is_empty() {
            "empty — press l to open a file or folder"
        } else {
            "no matches"
        };
        for (i, c) in hint.chars().enumerate() {
            if i >= inner.width as usize {
                break;
            }
            let cell = &mut buf[(inner.x + i as u16, inner.y)];
            cell.set_char(c);
            cell.set_fg(theme.color(Role::Muted));
        }
        return;
    }

    let overflow = visible.len() > inner.height as usize;
    let list_width = if overflow {
        inner.width.saturating_sub(1)
    } else {
        inner.width
    };

    for row in 0..inner.height {
        let Some(visible_pos) = scroll.checked_add(row as usize) else {
            break;
        };
        let Some(&index) = visible.get(visible_pos) else {
            break;
        };
        let Some(track) = snapshot.tracks.get(index) else {
            break;
        };

        let y = inner.y + row;
        // The now-playing marker compares the *real* index to `current`;
        // the cursor row is a position in the visible list.
        let is_current = snapshot.current == Some(index);
        let is_cursor = cursor == visible_pos;

        // Cursor-row background tint (muted).
        if is_cursor {
            for x in 0..list_width {
                buf[(inner.x + x, y)].set_style(Style::default().bg(theme.color(Role::Muted)));
            }
        }

        // Leading marker + number (the playlist's real track number).
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
        cell.set_char(marker);
        cell.set_fg(theme.color(Role::Accent));
        x += 1;

        // Number.
        for c in number.chars() {
            if x >= inner.x + list_width {
                break;
            }
            let cell = &mut buf[(x, y)];
            cell.set_char(c);
            cell.set_fg(theme.color(Role::Muted));
            x += 1;
        }
        x += 1;

        // Duration right-aligned (`--:--` when unknown); title truncated to
        // the space between.
        let duration = match track.duration {
            Some(d) => format_duration(d.as_secs()),
            None => "--:--".into(),
        };
        let duration_width = duration.width();
        let title_x = x;
        let available =
            (inner.x + list_width).saturating_sub(title_x + duration_width as u16 + 1) as usize;
        let name = track.display_name();
        let truncated = crate::ui::player::truncate_to_width(&name, available);
        for c in truncated.chars() {
            let cell = &mut buf[(x, y)];
            cell.set_char(c);
            cell.set_style(title_style);
            x += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) as u16;
        }
        // Duration.
        let dx = inner.x + list_width - 1 - duration_width as u16;
        for (i, c) in duration.chars().enumerate() {
            let cell = &mut buf[(dx + i as u16, y)];
            cell.set_char(c);
            cell.set_fg(if is_current {
                theme.color(Role::Accent)
            } else {
                theme.color(Role::Muted)
            });
        }
    }

    // Scrollbar only when the list overflows.
    if overflow {
        render_scrollbar(app, buf, scroll, visible.len(), inner);
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
        let in_thumb = f32::from(i) >= thumb_pos && f32::from(i) < thumb_pos + thumb_h;
        let cell = &mut buf[(x, inner.y + i)];
        if in_thumb {
            cell.set_char(glyphs::TRACK_FULL);
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
        // The scroll keeps the cursor visible in a 16-row window.
        let scroll = scroll_for(0, app.ui.playlist_cursor, 16, 24);
        assert_eq!(scroll, 8, "cursor on the last row");
        // Moving up inside the window keeps the scroll.
        assert_eq!(scroll_for(scroll, 20, 16, 24), 8);
        // Above the window, it follows the cursor.
        assert_eq!(scroll_for(scroll, 3, 16, 24), 3);
    }

    #[test]
    fn narrow_border_drops_hints_before_totals() {
        let app = test_app();
        for width in [40u16, 44, 52] {
            let border = bottom_border(&app, width);
            assert!(border.contains("24 · 1:56:43"), "totals kept: {border}");
        }
        assert!(
            bottom_border(&app, 52).contains("find"),
            "room for all hints"
        );
        assert!(!bottom_border(&app, 40).contains("find"), "find dropped");
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

    /// Flatten the bottom-border row of a rendered playlist.
    fn bottom_border(app: &App, width: u16) -> String {
        let area = Rect::new(0, 0, width, 18);
        let mut buf = Buffer::empty(area);
        render(app, area, &mut buf);
        let y = 17; // last row = bottom border
        (0..width)
            .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
            .collect()
    }

    #[test]
    fn notice_keeps_track_count_and_total() {
        let mut app = test_app();
        app.ui.notice = Some("config invalid — not saving".into());
        let border = bottom_border(&app, 104);
        // The count and total time must survive.
        assert!(border.contains("24"), "count kept: {border}");
        assert!(border.contains("1:56:43"), "total kept: {border}");
        // The notice appears, truncated by width if needed.
        assert!(border.contains("config invalid"), "notice shown: {border}");
    }

    #[test]
    fn multiline_notice_collapses_to_first_line() {
        let mut app = test_app();
        app.ui.notice = Some("config invalid — not saving\n  extra detail line".into());
        let border = bottom_border(&app, 104);
        assert!(border.contains("config invalid"), "{border}");
        assert!(
            !border.contains("extra detail"),
            "second line dropped: {border}"
        );
        assert!(border.contains("24"), "count kept: {border}");
    }

    #[test]
    fn long_notice_is_truncated_totals_survive() {
        let mut app = test_app();
        app.ui.notice = Some("x".repeat(400));
        // Wide panel: the notice fits but must be truncated with an ellipsis.
        let border = bottom_border(&app, 104);
        assert!(border.contains("24"), "count kept: {border}");
        assert!(border.contains("1:56:43"), "total kept: {border}");
        assert!(border.contains('…'), "notice truncated: {border}");
        assert!(
            !border.contains(&"x".repeat(100)),
            "notice capped: {border}"
        );

        // Narrow panel: no room for the notice at all → it is dropped, but
        // the count and total still win the space.
        let narrow = bottom_border(&app, 44);
        assert!(
            narrow.contains("24"),
            "count kept when notice dropped: {narrow}"
        );
        assert!(narrow.contains("1:56:43"), "total kept: {narrow}");
        assert!(!narrow.contains('…'), "no room → notice omitted: {narrow}");
    }
}
