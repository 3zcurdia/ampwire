//! EQ panel: on/auto/presets toggles, focused-band readout, braille curve,
//! preamp + 10 band sliders (plan §2.3).

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

    // Rows: [toggles + readout (1)] [curve (1)] [sliders (rest)] [labels (1)]
    let labels_row = 1u16;
    let slider_rows = inner.height.saturating_sub(2 + labels_row);
    let rows = [
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(slider_rows),
        Constraint::Length(labels_row),
    ];
    let [toggle_area, curve_area, slider_area, labels_area] = Layout::vertical(rows).areas(inner);

    render_toggles(app, toggle_area, buf);
    render_curve(app, curve_area, buf);
    render_sliders(app, slider_area, buf);
    render_labels(app, labels_area, buf);
}

fn render_toggles(app: &App, area: Rect, buf: &mut Buffer) {
    let theme = &app.theme;
    let ui = &app.ui;

    let mut x = area.x;
    let mut put = |x: &mut u16, text: &str, color: ratatui::style::Color| {
        for c in text.chars() {
            if *x >= area.x + area.width {
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

    put(&mut x, "on ", if ui.eq_on { on_color } else { off_color });
    put(&mut x, "  ", theme.color(Role::Primary));
    put(
        &mut x,
        "auto ",
        if ui.eq_auto { on_color } else { off_color },
    );
    put(&mut x, "  presets", theme.color(Role::Muted));

    // Focused-band readout on the right.
    let readout = format!(
        "{} {:+}dB",
        band_name(ui.eq_focus),
        ui.eq_bands[ui.eq_focus]
    );
    let readout_width = readout.chars().count() as u16;
    if area.width > readout_width + 2 {
        let rx = area.x + area.width - readout_width;
        let mut cursor = rx;
        put(&mut cursor, &readout, theme.color(Role::Glow));
    }
}

fn render_curve(app: &App, area: Rect, buf: &mut Buffer) {
    let theme = &app.theme;
    let ui = &app.ui;
    EqCurve {
        values: &normalized_bands(&ui.eq_bands),
        color: theme.color(Role::Primary),
    }
    .render(area, buf);
}

/// Map -12..=12 dB to -1.0..=1.0.
fn normalized_bands(bands: &[i8; 11]) -> Vec<f32> {
    bands.iter().map(|db| *db as f32 / 12.0).collect()
}

fn render_sliders(app: &App, area: Rect, buf: &mut Buffer) {
    let theme = &app.theme;
    let ui = &app.ui;
    if area.height == 0 || area.width < 11 {
        return;
    }
    // 11 sliders spread evenly across the width.
    let cell_width = area.width / 11;
    if cell_width == 0 {
        return;
    }
    for (i, db) in ui.eq_bands.iter().enumerate() {
        let x = area.x + i as u16 * cell_width;
        let slider_area = Rect {
            x,
            width: cell_width,
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
}

fn render_labels(app: &App, area: Rect, buf: &mut Buffer) {
    let theme = &app.theme;
    let ui = &app.ui;
    let cell_width = area.width / 11;
    if cell_width == 0 {
        return;
    }
    for (i, label) in BAND_LABELS.iter().enumerate() {
        let x = area.x + i as u16 * cell_width;
        let color = if ui.eq_focus == i {
            theme.color(Role::Glow)
        } else {
            theme.color(Role::Muted)
        };
        for (cx, c) in (x..x + cell_width).zip(label.chars()) {
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

    #[test]
    fn eq_renders_sliders_and_labels() {
        let mut app = test_app();
        app.ui.eq_focus = 3;
        app.ui.eq_bands[3] = 6;
        let area = Rect::new(0, 0, 52, 9);
        let mut buf = Buffer::empty(area);
        render(&app, area, &mut buf);
        // Border intact.
        assert_eq!(buf[(0, 0)].symbol(), "╭");
        // Band label "310" appears in the label row (second from bottom).
        let label_row: String = (1..51u16)
            .map(|x| buf[(x, area.height - 2)].symbol().to_string())
            .collect();
        assert!(label_row.contains("310"), "labels: {label_row:?}");
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
    fn normalized_bands_map_db() {
        let mut bands = [0i8; 11];
        bands[0] = 12;
        bands[1] = -12;
        let norm = normalized_bands(&bands);
        assert!((norm[0] - 1.0).abs() < f32::EPSILON);
        assert!((norm[1] + 1.0).abs() < f32::EPSILON);
    }
}
