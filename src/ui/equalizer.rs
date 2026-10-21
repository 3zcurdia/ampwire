//! EQ panel: on/auto/presets toggles + readout + braille curve on the top row,
//! then a left dB scale column with preamp + 10 band sliders, then band labels
//! (plan §2.3).

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::widgets::Widget;

use crate::action::Scope;
use crate::app::App;
use crate::ui::chrome;
use crate::ui::layout::Panel;
use crate::ui::theme::Role;
use crate::ui::widgets::eq_curve::EqCurve;
use crate::ui::widgets::slider::VSlider;

/// Band labels shown under the sliders.
pub const BAND_LABELS: [&str; 11] = [
    "pre", "60", "170", "310", "600", "1k", "3k", "6k", "12k", "14k", "16k",
];

/// Width of the left dB scale column (`+12` / `0` / `-12`).
const SCALE_WIDTH: u16 = 4;
/// Minimum columns we keep for the braille curve on the toggle row.
const MIN_CURVE_WIDTH: u16 = 8;

/// Center frequency name for the focused-band readout.
fn band_name(focus: usize) -> String {
    match focus {
        0 => "preamp".into(),
        i => format!("{}Hz", BAND_LABELS[i]),
    }
}

pub fn render(app: &App, area: Rect, buf: &mut Buffer) {
    let theme = &app.theme;
    let keymap = &app.keymap;
    let ui = &app.ui;
    let focused = ui.focus == Scope::Eq;

    let block = chrome::panel_block(Panel::Eq, focused, theme, keymap, None);
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height < 3 || inner.width == 0 {
        return;
    }

    // Rows: [toggles + readout + curve (1)] [sliders (rest)] [labels (1)]
    let labels_row = 1u16;
    let slider_rows = inner.height.saturating_sub(1 + labels_row);
    let rows = [
        Constraint::Length(1),
        Constraint::Min(slider_rows),
        Constraint::Length(labels_row),
    ];
    let [top_area, slider_area, labels_area] = Layout::vertical(rows).areas(inner);

    render_toggles_and_curve(app, top_area, buf);
    render_sliders(app, slider_area, buf);
    render_labels(app, labels_area, buf);
}

/// Top row: `on  auto  presets` on the left, the focused-band readout next to
/// it, then the braille curve of the 10 bands filling the rest. The readout
/// keeps its space — the curve shrinks first, then the toggles clip.
fn render_toggles_and_curve(app: &App, area: Rect, buf: &mut Buffer) {
    let theme = &app.theme;
    let ui = &app.ui;
    if area.width == 0 {
        return;
    }
    let right = area.x + area.width;

    let readout = format!(
        "{} {:+}dB",
        band_name(ui.eq_focus),
        ui.eq_bands[ui.eq_focus]
    );
    let rw = readout.chars().count() as u16;

    // 1. Toggles on the left, clipped so toggles + gap + readout fit.
    let mut x = area.x;
    let toggle_stop = right.saturating_sub(rw + 1);
    let mut put = |x: &mut u16, text: &str, color: ratatui::style::Color, stop: u16| {
        for c in text.chars() {
            if *x >= stop {
                return;
            }
            let cell = &mut buf[(*x, area.y)];
            cell.set_symbol(&c.to_string());
            cell.set_fg(color);
            *x += 1;
        }
    };
    let on_color = theme.color(Role::Accent);
    let off_color = theme.color(Role::Muted);
    put(
        &mut x,
        "on",
        if ui.eq_on { on_color } else { off_color },
        toggle_stop,
    );
    put(&mut x, "  ", theme.color(Role::Primary), toggle_stop);
    put(
        &mut x,
        "auto",
        if ui.eq_auto { on_color } else { off_color },
        toggle_stop,
    );
    put(&mut x, "  presets", theme.color(Role::Muted), toggle_stop);

    // 2. Readout right after the toggles; never clipped (falls back to
    //    right-aligning if the panel is too narrow for both).
    let rx = (x + 1).min(right.saturating_sub(rw));
    for (i, c) in readout.chars().enumerate() {
        let cx = rx + i as u16;
        if cx < right {
            let cell = &mut buf[(cx, area.y)];
            cell.set_symbol(&c.to_string());
            cell.set_fg(theme.color(Role::Glow));
        }
    }

    // 3. Curve fills whatever remains to the right of the readout.
    let curve_x = rx + rw + 2;
    if right.saturating_sub(curve_x) >= MIN_CURVE_WIDTH {
        let curve_area = Rect {
            x: curve_x,
            width: right - curve_x,
            ..area
        };
        // The curve plots the 10 bands, not the preamp.
        EqCurve {
            values: &curve_bands(&ui.eq_bands),
            color: theme.color(Role::Primary),
        }
        .render(curve_area, buf);
    }
}

/// Normalize the 10 EQ bands (skipping the preamp) to -1.0..=1.0 for the curve.
fn curve_bands(bands: &[i8; 11]) -> Vec<f32> {
    bands[1..].iter().map(|db| *db as f32 / 12.0).collect()
}

/// Slider region: everything right of the scale column.
fn slider_region(area: Rect) -> Rect {
    let width = area.width.saturating_sub(SCALE_WIDTH);
    Rect {
        x: area.x + SCALE_WIDTH.min(area.width),
        width,
        ..area
    }
}

fn render_sliders(app: &App, area: Rect, buf: &mut Buffer) {
    let theme = &app.theme;
    let ui = &app.ui;
    let region = slider_region(area);
    if region.width == 0 || area.height == 0 {
        return;
    }

    // 11 sliders spread evenly across the full region, reaching both edges.
    let n = ui.eq_bands.len(); // 11
    for (i, db) in ui.eq_bands.iter().enumerate() {
        let x = region.x + (i as u16 * (region.width.saturating_sub(1))) / (n as u16 - 1);
        let slider_area = Rect {
            x,
            width: 1,
            ..area
        };
        VSlider {
            value: *db as f32 / 12.0,
            track: theme.color(Role::Primary),
            knob: theme.color(Role::Accent),
            focused: ui.eq_focus == i,
        }
        .render(slider_area, buf);
    }

    // Left dB scale: +12 top, 0 middle, -12 bottom of the slider rows.
    let scale = [
        ("+12", 0u16),
        ("0", area.height / 2),
        ("-12", area.height - 1),
    ];
    for (label, row) in scale {
        if row >= area.height {
            continue;
        }
        // Right-align within the scale column so it never touches a slider.
        let start = area.x + SCALE_WIDTH.saturating_sub(label.len() as u16);
        for (i, c) in label.chars().enumerate() {
            let cx = start + i as u16;
            if cx < area.x + SCALE_WIDTH {
                let cell = &mut buf[(cx, area.y + row)];
                cell.set_symbol(&c.to_string());
                cell.set_fg(theme.color(Role::Muted));
            }
        }
    }
}

fn render_labels(app: &App, area: Rect, buf: &mut Buffer) {
    let theme = &app.theme;
    let ui = &app.ui;
    let region = slider_region(area);
    if region.width == 0 {
        return;
    }
    let n = ui.eq_bands.len();
    for (i, label) in BAND_LABELS.iter().enumerate() {
        let x = region.x + (i as u16 * (region.width.saturating_sub(1))) / (n as u16 - 1);
        let color = if ui.eq_focus == i {
            theme.color(Role::Glow)
        } else {
            theme.color(Role::Muted)
        };
        // Center the label under its slider, clamped so it never overflows
        // the panel edge (the last band sits at the far right).
        let lw = label.chars().count() as u16;
        let center = x.saturating_add(1);
        let start = center
            .saturating_sub(lw / 2)
            .min(area.x + area.width.saturating_sub(lw.min(area.width)));
        for (j, c) in label.chars().enumerate() {
            let cx = start + j as u16;
            if cx >= area.x + area.width {
                break;
            }
            let cell = &mut buf[(cx, area.y)];
            cell.set_symbol(&c.to_string());
            cell.set_fg(color);
        }
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

    /// Flatten a buffer region into one string of symbols.
    fn buffer_text(buf: &Buffer, area: Rect) -> String {
        let mut out = String::new();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                out.push_str(buf.cell((x, y)).unwrap().symbol());
            }
        }
        out
    }

    #[test]
    fn eq_renders_scale_labels_and_band_labels() {
        let mut app = test_app();
        app.ui.eq_focus = 3;
        app.ui.eq_bands[3] = 6;
        let area = Rect::new(0, 0, 52, 9);
        let mut buf = Buffer::empty(area);
        render(&app, area, &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "╭");
        let text = buffer_text(&buf, area);
        // Scale column labels.
        assert!(text.contains("+12"), "top scale label: {text}");
        assert!(text.contains("-12"), "bottom scale label: {text}");
        // Band label under a slider.
        assert!(text.contains("310"), "band labels: {text}");
    }

    #[test]
    fn scale_column_does_not_overlap_preamp_slider() {
        let app = test_app();
        let area = Rect::new(0, 0, 52, 9);
        let mut buf = Buffer::empty(area);
        render(&app, area, &mut buf);
        // Inner starts at y=1 (border); toggle row is y=1, top slider row is
        // y=2. The `+12` label lives in the 4-wide gutter (cols 1..4); the
        // preamp slider starts at col 4.
        let top_slider_row: String = (0..6u16)
            .map(|x| buf.cell((x, 2)).unwrap().symbol().to_string())
            .collect();
        assert!(
            top_slider_row.contains("+12"),
            "scale on top slider row: {top_slider_row}"
        );
        // The gutter is cols 1..4 (col 0 is the panel border). No `│` slider
        // track may appear inside it — the scale owns that space.
        let gutter: String = (1..5u16)
            .map(|x| buf.cell((x, 2)).unwrap().symbol().to_string())
            .collect();
        assert!(
            !gutter.contains('│'),
            "scale gutter must be clear of sliders: {gutter} (row {top_slider_row})"
        );
    }

    #[test]
    fn readout_is_never_clipped() {
        let mut app = test_app();
        app.ui.eq_focus = 0; // "preamp +0dB" (longest name)
        for width in [40u16, 52, 80, 120] {
            let area = Rect::new(0, 0, width, 9);
            let mut buf = Buffer::empty(area);
            render(&app, area, &mut buf);
            // The readout is on the toggle row (y=1).
            let top: String = (0..width)
                .map(|x| buf.cell((x, 1)).unwrap().symbol().to_string())
                .collect();
            assert!(
                top.contains("preamp"),
                "readout clipped at width {width}: {top}"
            );
        }
    }

    #[test]
    fn focused_knob_uses_accent_color() {
        let mut app = test_app();
        app.ui.eq_focus = 1;
        app.ui.eq_bands[1] = 6;
        let area = Rect::new(0, 0, 52, 9);
        let mut buf = Buffer::empty(area);
        render(&app, area, &mut buf);
        let neon = theme::neon();
        // Scan the slider rows (y=2..7) for the accent-colored ●.
        let mut found = false;
        for y in 2..7u16 {
            for x in 4..52u16 {
                let cell = buf.cell((x, y)).unwrap();
                if cell.symbol() == "●" && cell.fg == neon.color(Role::Accent) {
                    found = true;
                }
            }
        }
        assert!(found, "focused knob rendered in accent");
    }

    #[test]
    fn band_adjust_updates_readout() {
        let mut app = test_app();
        app.ui.eq_focus = 1;
        app.dispatch(crate::action::Action::BandUp);
        app.dispatch(crate::action::Action::BandUp);
        assert_eq!(app.ui.eq_bands[1], 2);
        app.dispatch(crate::action::Action::BandDown);
        assert_eq!(app.ui.eq_bands[1], 1);
    }

    #[test]
    fn band_focus_stays_in_range() {
        let mut app = test_app();
        app.ui.eq_focus = 0;
        app.dispatch(crate::action::Action::BandLeft);
        assert_eq!(app.ui.eq_focus, 0);
        app.ui.eq_focus = 10;
        app.dispatch(crate::action::Action::BandRight);
        assert_eq!(app.ui.eq_focus, 10);
    }

    #[test]
    fn curve_uses_ten_bands_not_preamp() {
        let mut bands = [0i8; 11];
        bands[0] = 12; // preamp maxed — must not appear in the curve
        bands[1] = -12;
        let curve = curve_bands(&bands);
        assert_eq!(curve.len(), 10, "curve plots 10 bands, not the preamp");
        assert!((curve[0] + 1.0).abs() < f32::EPSILON, "band 1 → -1.0");
    }
}
