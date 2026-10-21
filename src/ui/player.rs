//! Player panel: state glyph, LCD time, spectrum, marquee, format line, seek
//! bar, volume/balance sliders, transport row (plan §2.3).

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::action::Action;
use crate::app::App;
use crate::player::PlayState;
use crate::ui::chrome;
use crate::ui::layout::Panel;
use crate::ui::theme::glyphs;
use crate::ui::theme::{Role, Theme};
use crate::ui::widgets::lcd_digits::{LcdDigits, lcd_width};
use crate::ui::widgets::marquee::Marquee;
use crate::ui::widgets::slider::HSlider;
use crate::ui::widgets::spectrum::Spectrum;

/// Format seconds as `m:ss` or `h:mm:ss`.
pub fn format_time(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Render the player panel (full or compact, chosen by `compact`).
pub fn render(app: &App, area: Rect, buf: &mut Buffer, compact: bool) {
    let theme = &app.theme;
    let keymap = &app.keymap;
    let ui = &app.ui;
    let focused = ui.focus == crate::action::Scope::Player;

    let block = chrome::panel_block(Panel::Player, focused, theme, keymap, None);
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    if compact {
        render_compact(app, inner, buf);
    } else {
        render_full(app, inner, buf);
    }
}

fn state_glyph(state: PlayState) -> char {
    match state {
        PlayState::Playing => glyphs::PLAY,
        PlayState::Paused => glyphs::PAUSE,
        PlayState::Stopped => glyphs::STOP,
    }
}

fn render_full(app: &App, area: Rect, buf: &mut Buffer) {
    let theme = &app.theme;
    let ui = &app.ui;
    let snapshot = app.snapshot();

    // Rows: [time + spectrum (2)] [marquee (1)] [format (1)] [seek (1)]
    //        [vol/bal (1)] [transport box (3)] = 9 inner rows (11 with
    //        borders). The transport gets 3 rows so each button is a real
    //        boxed target instead of a cramped glyph.
    let rows = [
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(3),
    ];
    let [
        time_area,
        marquee_area,
        format_area,
        seek_area,
        mix_area,
        transport_area,
    ] = Layout::vertical(rows).areas(area);

    // --- Row 1: state glyph + LCD digits, spectrum on the right.
    let glyph_color = match snapshot.state {
        PlayState::Playing => theme.color(Role::Accent),
        PlayState::Paused => theme.color(Role::Glow),
        PlayState::Stopped => theme.color(Role::Muted),
    };
    let glyph_cell = &mut buf[(time_area.x, time_area.y)];
    glyph_cell.set_symbol(&state_glyph(snapshot.state).to_string());
    glyph_cell.set_fg(glyph_color);

    let shown = if ui.time_remaining {
        snapshot
            .current_track()
            .map(|t| {
                t.duration
                    .as_secs()
                    .saturating_sub(snapshot.position.as_secs())
            })
            .unwrap_or(0)
    } else {
        snapshot.position.as_secs()
    };
    let time_text = format_time(shown);
    let digit_width = lcd_width(&time_text);
    let digit_area = Rect {
        x: time_area.x + 2,
        width: digit_width.min(time_area.width.saturating_sub(2)),
        ..time_area
    };
    // Blink digits while paused.
    let lit = snapshot.state != PlayState::Paused || ui.blink_lit;
    LcdDigits {
        text: &time_text,
        color: theme.color(Role::Primary),
        dim: theme.color(Role::Muted),
        lit,
    }
    .render(digit_area, buf);

    // Spectrum on the right of the time rows.
    let spectrum_x = digit_area.x + digit_area.width + 1;
    if spectrum_x < time_area.x + time_area.width {
        let spectrum_area = Rect {
            x: spectrum_x,
            width: time_area.x + time_area.width - spectrum_x,
            ..time_area
        };
        let spectrum_theme: &Theme = theme;
        let color_for: &dyn Fn(usize, usize) -> ratatui::style::Color =
            &move |i: usize, n: usize| spectrum_theme.viz(i, n);
        Spectrum {
            values: &snapshot.spectrum,
            peaks: &snapshot.peaks,
            color_for,
            peak_color: theme.peak(),
        }
        .render(spectrum_area, buf);
    }

    // --- Row 2: marquee title.
    let title = snapshot
        .current_track()
        .map(|t| {
            let total = t.duration.as_secs();
            format!(
                "{}. {} ({}) {}",
                snapshot.current.map(|i| i + 1).unwrap_or(0),
                t.title,
                format_time(total),
                "*".repeat(if snapshot.shuffle { 3 } else { 0 })
            )
            .trim_end()
            .to_string()
        })
        .unwrap_or_else(|| "--- ampwire ---".into());
    Marquee {
        text: &title,
        offset: ui.marquee_offset,
        style: Style::default().fg(theme.color(Role::Fg)),
        gap: 6,
    }
    .render(marquee_area, buf);

    // --- Row 3: format line (hi-fi first) + shuf/rep flags.
    let fmt = &snapshot.format;
    let mut fmt_parts = vec![fmt.codec.clone()];
    match (fmt.bit_depth, fmt.sample_rate) {
        (Some(depth), Some(rate)) => fmt_parts.push(format!("{depth}/{}", rate / 1000)),
        (Some(depth), None) => fmt_parts.push(format!("{depth}-bit")),
        (None, Some(rate)) => fmt_parts.push(format!("{}kHz", rate / 1000)),
        (None, None) => {}
    }
    fmt_parts.push(fmt.channels.clone());
    if fmt.bit_perfect {
        fmt_parts.push("bit-perfect".into());
    }
    let mut spans: Vec<Span> = vec![Span::styled(
        fmt_parts.join("  "),
        Style::default().fg(theme.color(Role::Primary)),
    )];

    // Flags, right-aligned-ish: push them after filler spaces.
    let flags_width = 12u16;
    let used: u16 = fmt_parts.join("  ").chars().count() as u16;
    let filler = format!(
        "{:width$}",
        "",
        width = (format_area.width.saturating_sub(used + flags_width) as usize).min(64)
    );
    spans.push(Span::styled(
        filler,
        Style::default().fg(theme.color(Role::Primary)),
    ));
    spans.push(Span::styled(
        "shuf",
        Style::default().fg(if snapshot.shuffle {
            theme.color(Role::Accent)
        } else {
            theme.color(Role::Muted)
        }),
    ));
    spans.push(Span::styled(
        "  ",
        Style::default().fg(theme.color(Role::Primary)),
    ));
    spans.push(Span::styled(
        "rep",
        Style::default().fg(if snapshot.repeat {
            theme.color(Role::Accent)
        } else {
            theme.color(Role::Muted)
        }),
    ));
    buf.set_style(format_area, Style::default());
    Line::from(spans).render(format_area, buf);

    // --- Row 4: seek bar with elapsed/total.
    let time_pair = format!(
        "{}/{}",
        format_time(snapshot.position.as_secs()),
        format_time(
            snapshot
                .current_track()
                .map(|t| t.duration.as_secs())
                .unwrap_or(0)
        )
    );
    render_seek(app, seek_area, buf, &time_pair);

    // --- Row 5: volume + balance.
    render_mix(app, mix_area, buf);

    // --- Row 6: transport with accent hotkey letters.
    render_transport(app, transport_area, buf);
}

fn render_seek(app: &App, area: Rect, buf: &mut Buffer, time_pair: &str) {
    let theme = &app.theme;
    let snapshot = app.snapshot();
    if area.width < time_pair.len() as u16 + 4 {
        return;
    }
    let track_width = area.width - time_pair.len() as u16 - 2;
    let total = snapshot
        .current_track()
        .map(|t| t.duration.as_secs())
        .unwrap_or(0);
    let value = if total == 0 {
        0.0
    } else {
        (snapshot.position.as_secs() as f32 / total as f32).clamp(0.0, 1.0)
    };

    let knob_x = ((track_width.saturating_sub(1)) as f32 * value).round() as u16;
    for i in 0..track_width {
        let cell = &mut buf[(area.x + i, area.y)];
        if i == knob_x {
            cell.set_symbol(&glyphs::KNOB.to_string());
            cell.set_fg(theme.color(Role::Accent));
        } else {
            cell.set_symbol("━");
            cell.set_fg(theme.color(Role::Primary));
        }
    }
    let x = area.x + track_width + 1;
    for (x, c) in (x..area.x + area.width).zip(time_pair.chars()) {
        let cell = &mut buf[(x, area.y)];
        cell.set_symbol(&c.to_string());
        cell.set_fg(theme.color(Role::Fg));
    }
}

fn render_mix(app: &App, area: Rect, buf: &mut Buffer) {
    let theme = &app.theme;
    let snapshot = app.snapshot();
    // vol and bal are separated by a 2-column gap so they read as two
    // distinct controls rather than one continuous track.
    const GAP: u16 = 2;
    if area.width <= GAP {
        return;
    }
    let usable = area.width - GAP;
    let vol_w = usable * 55 / 100;
    let bal_w = usable - vol_w;
    let vol_area = Rect {
        x: area.x,
        width: vol_w,
        ..area
    };
    let bal_area = Rect {
        x: area.x + vol_w + GAP,
        width: bal_w,
        ..area
    };

    HSlider {
        value: snapshot.volume as f32 / 100.0,
        track: theme.color(Role::Primary),
        knob: theme.color(Role::Accent),
        label: Some("vol"),
        label_style: theme.color(Role::Fg),
    }
    .render(vol_area, buf);

    // Balance: -1..=1, knob at center when 0.
    let bal_value = snapshot.balance as f32 / 100.0;
    HSlider {
        value: (bal_value + 1.0) / 2.0,
        track: theme.color(Role::Primary),
        knob: theme.color(Role::Accent),
        label: Some("bal"),
        label_style: theme.color(Role::Fg),
    }
    .render(bal_area, buf);
}

const TRANSPORT: &[(Action, &str)] = &[
    (Action::Prev, glyphs::PREV),
    (Action::Play, "▶"),
    (Action::Pause, "‖"),
    (Action::Stop, "■"),
    (Action::Next, glyphs::NEXT),
    (Action::Open, "▲"),
];

fn render_transport(app: &App, area: Rect, buf: &mut Buffer) {
    let theme = &app.theme;
    let keymap = &app.keymap;
    if area.width == 0 || area.height < 3 {
        return;
    }

    // Six buttons spread evenly across the full width, each in its own box.
    // The box is 3 rows: a rounded top border, the hotkey + glyph, and a
    // rounded bottom border — so each button is a real target, not a glyph
    // squeezed against its neighbour.
    let n = TRANSPORT.len() as u16;
    let button_h = 3u16.min(area.height);
    for (i, (action, glyph)) in TRANSPORT.iter().enumerate() {
        let bx = area.x + (i as u16 * area.width) / n;
        let next_x = area.x + ((i as u16 + 1) * area.width) / n;
        let bw = next_x.saturating_sub(bx);
        if bw < 2 {
            continue;
        }

        let top = theme.color(if app.ui.focus == crate::action::Scope::Player {
            Role::Primary
        } else {
            Role::Muted
        });
        // Top and bottom borders, leaving the corners rounded.
        for x in 0..bw {
            let symbol = match x {
                0 => "╭",
                w if w == bw - 1 => "╮",
                _ => "─",
            };
            let cell = &mut buf[(bx + x, area.y)];
            cell.set_symbol(symbol);
            cell.set_fg(top);
            if button_h >= 3 {
                let cell = &mut buf[(bx + x, area.y + 2)];
                let symbol = match x {
                    0 => "╰",
                    w if w == bw - 1 => "╯",
                    _ => "─",
                };
                cell.set_symbol(symbol);
                cell.set_fg(top);
            }
        }

        // Side walls and the content row (middle).
        let mid_y = area.y + 1;
        let content_y = if button_h >= 2 { mid_y } else { area.y };
        for x in 0..bw {
            if x == 0 || x == bw - 1 {
                let cell = &mut buf[(bx + x, content_y)];
                cell.set_symbol("│");
                cell.set_fg(top);
            }
        }

        // Content: hotkey letter (accent) then the glyph (fg), centered.
        let hint = keymap.hint(*action).unwrap_or_default();
        let content: String = format!("{hint}{glyph}");
        let content_width = content.chars().count() as u16;
        let start = bx + 1 + bw.saturating_sub(1).saturating_sub(content_width) / 2;
        let end = bx + bw.saturating_sub(1);
        let hotkey_len = hint.chars().count();
        for (cx, (j, c)) in (start.max(bx + 1)..end).zip(content.chars().enumerate()) {
            let cell = &mut buf[(cx, content_y)];
            cell.set_symbol(&c.to_string());
            // The hotkey letter is accent; the glyph is foreground.
            let is_hotkey = j < hotkey_len;
            cell.set_fg(if is_hotkey {
                theme.color(Role::Accent)
            } else {
                theme.color(Role::Fg)
            });
        }
    }
}

/// Compact player: one row of time + marquee + seek (plan §2.2 row 3).
fn render_compact(app: &App, area: Rect, buf: &mut Buffer) {
    let theme = &app.theme;
    let ui = &app.ui;
    let snapshot = app.snapshot();
    if area.height == 0 || area.width == 0 {
        return;
    }
    let mut x = area.x;

    // State glyph.
    let glyph_cell = &mut buf[(x, area.y)];
    glyph_cell.set_symbol(&state_glyph(snapshot.state).to_string());
    glyph_cell.set_fg(theme.color(Role::Accent));
    x += 2;

    // Small time: m:ss as plain text.
    let shown = if ui.time_remaining {
        snapshot
            .current_track()
            .map(|t| {
                t.duration
                    .as_secs()
                    .saturating_sub(snapshot.position.as_secs())
            })
            .unwrap_or(0)
    } else {
        snapshot.position.as_secs()
    };
    let time_text = format_time(shown);
    for c in time_text.chars() {
        if x >= area.x + area.width {
            return;
        }
        let cell = &mut buf[(x, area.y)];
        cell.set_symbol(&c.to_string());
        cell.set_fg(theme.color(Role::Primary));
        x += 1;
    }
    x += 1;

    // Title, truncated by display width.
    let title = snapshot
        .current_track()
        .map(|t| t.title.clone())
        .unwrap_or_default();
    let rest = (area.x + area.width).saturating_sub(x);
    let seek_width = 12u16.min(rest / 2);
    let title_width = rest.saturating_sub(seek_width + 1);
    let truncated = truncate_to_width(&title, title_width as usize);
    for c in truncated.chars() {
        let cell = &mut buf[(x, area.y)];
        cell.set_symbol(&c.to_string());
        cell.set_fg(theme.color(Role::Fg));
        x += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) as u16;
    }
    x += 1;

    // Mini seek bar.
    if seek_width > 0 && x + seek_width <= area.x + area.width {
        let total = snapshot
            .current_track()
            .map(|t| t.duration.as_secs())
            .unwrap_or(0);
        let value = if total == 0 {
            0.0
        } else {
            (snapshot.position.as_secs() as f32 / total as f32).clamp(0.0, 1.0)
        };
        let knob_x = ((seek_width.saturating_sub(1)) as f32 * value).round() as u16;
        for i in 0..seek_width {
            let cell = &mut buf[(x + i, area.y)];
            if i == knob_x {
                cell.set_symbol(&glyphs::KNOB.to_string());
                cell.set_fg(theme.color(Role::Accent));
            } else {
                cell.set_symbol("━");
                cell.set_fg(theme.color(Role::Primary));
            }
        }
    }
}

/// Truncate `text` to at most `width` display columns, never splitting a
/// character.
pub fn truncate_to_width(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    let mut used = 0usize;
    for c in text.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out
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

    #[test]
    fn format_time_shapes() {
        assert_eq!(format_time(83), "1:23");
        assert_eq!(format_time(3600), "1:00:00");
        assert_eq!(format_time(0), "0:00");
    }

    #[test]
    fn truncate_never_splits_chars() {
        assert_eq!(truncate_to_width("hello", 3), "hel");
        assert_eq!(truncate_to_width("日本語", 4), "日本");
        assert_eq!(truncate_to_width("日本語", 5), "日本"); // no half char
        assert_eq!(truncate_to_width("ab", 10), "ab");
    }

    #[test]
    fn player_renders_into_buffer() {
        let app = test_app();
        let area = Rect::new(0, 0, 52, 9);
        let mut buf = Buffer::empty(area);
        render(&app, area, &mut buf, false);

        // Top-left corner is the rounded border.
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "╭");
        // The state glyph lands in the first inner cell, not on the border.
        let inner = buf.cell((1, 1)).unwrap();
        assert_eq!(inner.symbol(), "▶", "playing state glyph");
    }

    #[test]
    fn compact_player_renders() {
        let app = test_app();
        let area = Rect::new(0, 0, 52, 3);
        let mut buf = Buffer::empty(area);
        render(&app, area, &mut buf, true);
        assert_eq!(buf[(0, 0)].symbol(), "╭");
        // Middle row has content.
        let row: String = (0..10u16)
            .map(|x| buf[(x, 1)].symbol().to_string())
            .collect();
        assert!(
            !row.trim().is_empty(),
            "compact row should have content: {row:?}"
        );
    }

    #[test]
    fn paused_blink_toggles_lit() {
        let mut app = test_app();
        // Pause the mock via dispatch.
        app.dispatch(crate::action::Action::Pause);
        assert_eq!(app.snapshot().state, PlayState::Paused);
        app.tick(std::time::Duration::from_millis(500));
        // After tick the blink phase flipped; digits still render (either
        // lit or unlit) — just ensure no panic and time area is stable.
        let area = Rect::new(0, 0, 52, 9);
        let mut buf = Buffer::empty(area);
        render(&app, area, &mut buf, false);
        assert_eq!(buf[(0, 0)].symbol(), "╭");
    }
}
