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
        // 16 levels across `area.height` rows (each row = 8 eighth-blocks).
        let total_levels = area.height as f32 * 8.0;

        for col in 0..area.width {
            // Map each column to a bin; more columns → more bars, not
            // stretched ones (bins repeat only when the panel is very wide).
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

            // Bar height in 1/8 blocks, from the bottom. A zero bar draws
            // nothing, so the row can go fully blank.
            let levels = (value * total_levels).round() as u16;
            for row in 0..area.height {
                let y = area.y + area.height - 1 - row;
                let cell = &mut buf[(x, y)];
                // This row covers eighth-levels [row*8, row*8+8) from bottom.
                let lo = row * 8;
                let filled_here = levels.saturating_sub(lo).min(8);
                if filled_here == 0 {
                    continue;
                }
                let symbol = if filled_here >= 8 {
                    EIGHTHS[7]
                } else {
                    EIGHTHS[(filled_here - 1) as usize]
                };
                cell.set_char(symbol);
                cell.set_fg(color);
            }

            // Peak cap: a single accent cell floating just above the bar.
            // Only drawn when the peak clears the bar top, so low bars can
            // drop to zero without a stray block.
            let cap_levels = (peak * total_levels).round() as u16;
            if cap_levels > levels {
                let cap_row = (cap_levels - 1) / 8;
                if cap_row < area.height {
                    let y = area.y + area.height - 1 - cap_row;
                    let cell = &mut buf[(x, y)];
                    cell.set_char(EIGHTHS[7]);
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

    #[test]
    fn thirty_two_bins_span_every_column_of_a_wide_panel() {
        // Plan 04: the real player reports 32 bands (the mock's 24 was
        // never baked in — the widget resamples bins to columns).
        let mut values = [0.0f32; 32];
        values[31] = 1.0; // only the top band is hot
        let buf = render(&values, &[0.0f32; 32], 100, 2);
        assert_eq!(buf.cell((99, 1)).unwrap().symbol(), "█");
        assert_eq!(buf.cell((0, 1)).unwrap().symbol(), " ", "bin 0 stays flat");
    }
}
