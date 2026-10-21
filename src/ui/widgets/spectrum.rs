//! Eighth-block spectrum bars with peak caps, btop-style.
//!
//! Bars use the `▁▂▃▄▅▆▇█` eighth-block ramp; a peak cap floats one row above
//! the bar at the value's height. Colors come from the theme's viz gradient.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;

const EIGHTHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// `values` and `peaks` are 0.0–1.0 magnitudes, low → high frequency.
/// `color_for(i, n)` returns the bar color; `peak_color` the caps.
pub struct Spectrum<'a> {
    pub values: &'a [f32],
    pub peaks: &'a [f32],
    pub color_for: &'a dyn Fn(usize, usize) -> Color,
    pub peak_color: Color,
}

impl Widget for Spectrum<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 || self.values.is_empty() {
            return;
        }
        let n = self.values.len();
        let bar_height = area.height as f32;

        for col in 0..area.width {
            // Map each column to a value; wide terminals spread values out
            // by interpolating between bins.
            let idx = (col as usize * n / area.width as usize).min(n - 1);
            let value = self.values[idx].clamp(0.0, 1.0);
            let peak = self
                .peaks
                .get(idx)
                .copied()
                .unwrap_or(value)
                .clamp(0.0, 1.0);

            let x = area.x + col;
            let color = (self.color_for)(idx, n);

            // Bar: fill rows from the bottom; the top cell is partial for the
            // fractional remainder.
            let filled = (value * bar_height).floor() as u16;
            for row in 0..area.height {
                let y = area.y + area.height - 1 - row;
                let cell = &mut buf[(x, y)];
                if row < filled {
                    cell.set_symbol(&EIGHTHS[7].to_string());
                    cell.set_fg(color);
                } else if row == filled && value > 0.0 {
                    let frac = value * bar_height - filled as f32;
                    if frac > 0.0 {
                        let eighth = ((frac * 8.0).round() as usize).clamp(1, 8) - 1;
                        cell.set_symbol(&EIGHTHS[eighth].to_string());
                        cell.set_fg(color);
                    }
                }
            }

            // Peak cap: at or above the bar's top, accent-colored.
            if peak > 0.0 {
                let cap_filled = (peak * bar_height).round() as u16;
                if cap_filled >= filled {
                    let y = area.y + area.height.saturating_sub(cap_filled.max(1));
                    let cell = &mut buf[(x, y)];
                    cell.set_symbol(&EIGHTHS[7].to_string());
                    cell.set_fg(self.peak_color);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(values: &[f32], peaks: &[f32], w: u16, h: u16) -> Buffer {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        let color: &dyn Fn(usize, usize) -> Color = &|_, _| Color::Rgb(255, 106, 0);
        Spectrum {
            values,
            peaks,
            color_for: color,
            peak_color: Color::Rgb(0, 229, 255),
        }
        .render(area, &mut buf);
        buf
    }

    #[test]
    fn full_bar_fills_column() {
        let buf = render(&[1.0], &[1.0], 1, 4);
        for y in 0..4 {
            assert_eq!(buf.cell((0, y)).unwrap().symbol(), "█");
        }
    }

    #[test]
    fn zero_renders_empty() {
        let buf = render(&[0.0], &[0.0], 1, 4);
        for y in 0..4 {
            assert_eq!(buf.cell((0, y)).unwrap().symbol(), " ");
        }
    }

    #[test]
    fn peak_cap_uses_peak_color() {
        // Bar 0.5, peak 1.0 → cap cell near the top in aqua.
        let buf = render(&[0.5], &[1.0], 1, 4);
        let top = buf.cell((0, 0)).unwrap();
        assert_eq!(top.fg, Color::Rgb(0, 229, 255));
    }

    #[test]
    fn empty_input_is_silent() {
        let buf = render(&[], &[], 3, 3);
        for y in 0..3 {
            for x in 0..3 {
                assert_eq!(buf.cell((x, y)).unwrap().symbol(), " ");
            }
        }
    }
}
