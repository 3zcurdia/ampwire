//! Braille polyline of the 10 EQ band values.
//!
//! Each braille cell is 2 dots wide × 4 tall, so a `w × h` area gives a
//! `2w × 4h` dot canvas. Dots are lit per line segment between consecutive
//! band values, and `⠿` fills are avoided — only the polyline lights up.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;

const BRAILLE_BLANK: u32 = 0x2800;

/// Bit layout of braille dots inside one cell (dot column, dot row) → bit.
///
/// Unicode numbers the dots down the left column first, then the right, with
/// the bottom row (dots 7 and 8) added last:
///
/// ```text
/// 1 4     0x01 0x08
/// 2 5     0x02 0x10
/// 3 6     0x04 0x20
/// 7 8     0x40 0x80
/// ```
fn braille_bit(dx: u32, dy: u32) -> u32 {
    const BITS: [[u32; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];
    BITS.get(dy as usize)
        .and_then(|row| row.get(dx as usize))
        .copied()
        .unwrap_or(0)
}

/// Values in -1.0..=1.0 (dB ±12 mapped by the caller), left to right.
pub struct EqCurve<'a> {
    pub values: &'a [f32],
    pub color: Color,
}

impl Widget for EqCurve<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 || self.values.is_empty() {
            return;
        }
        let dots_w = area.width as usize * 2;
        let dots_h = area.height as usize * 4;
        let n = self.values.len();

        // Map each value to a dot coordinate.
        let points: Vec<(f32, f32)> = self
            .values
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let x = if n == 1 {
                    0.0
                } else {
                    i as f32 * (dots_w as f32 - 1.0) / (n - 1) as f32
                };
                let v = v.clamp(-1.0, 1.0);
                let y = (1.0 - f32::midpoint(v, 1.0)) * (dots_h as f32 - 1.0);
                (x, y)
            })
            .collect();

        // Dot grid.
        let mut dots = vec![false; dots_w * dots_h];
        let mut light = |x: usize, y: usize| {
            if x < dots_w && y < dots_h {
                dots[y * dots_w + x] = true;
            }
        };

        for pair in points.windows(2) {
            let (x0, y0) = (pair[0].0, pair[0].1);
            let (x1, y1) = (pair[1].0, pair[1].1);
            // DDA line: sample along the longer axis.
            let steps = ((x1 - x0).abs().max((y1 - y0).abs())).ceil().max(1.0) as usize;
            for s in 0..=steps {
                let t = s as f32 / steps as f32;
                let x = (x0 + (x1 - x0) * t).round();
                let y = (y0 + (y1 - y0) * t).round();
                light(x as usize, y as usize);
            }
        }
        // Light the value points themselves (visible at band positions).
        for (x, y) in &points {
            light(x.round() as usize, y.round() as usize);
        }

        // Write cells.
        for cy in 0..area.height as usize {
            for cx in 0..area.width as usize {
                let mut bits = 0u32;
                for dy in 0..4u32 {
                    for dx in 0..2u32 {
                        let x = cx * 2 + dx as usize;
                        let y = cy * 4 + dy as usize;
                        if dots[y * dots_w + x] {
                            bits |= braille_bit(dx, dy);
                        }
                    }
                }
                if bits != 0 {
                    let cell = &mut buf[(area.x + cx as u16, area.y + cy as u16)];
                    if let Some(c) = char::from_u32(BRAILLE_BLANK + bits) {
                        cell.set_char(c);
                        cell.set_fg(self.color);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(values: &[f32]) -> Vec<String> {
        let area = Rect::new(0, 0, 6, 2);
        let mut buf = Buffer::empty(area);
        EqCurve {
            values,
            color: Color::Rgb(255, 106, 0),
        }
        .render(area, &mut buf);
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn flat_curve_is_not_blank() {
        let rows = render(&[0.0; 10]);
        assert!(rows.iter().any(|r| r.chars().any(|c| c != ' ')), "{rows:?}");
    }

    #[test]
    fn empty_is_blank() {
        let area = Rect::new(0, 0, 4, 1);
        let mut buf = Buffer::empty(area);
        EqCurve {
            values: &[],
            color: Color::Rgb(255, 106, 0),
        }
        .render(area, &mut buf);
        for y in 0..area.height {
            for x in 0..area.width {
                assert_eq!(buf.cell((x, y)).unwrap().symbol(), " ");
            }
        }
    }

    #[test]
    fn boost_curve_lights_upper_rows() {
        // All bands at +12 dB → dots near the top (row 0).
        let rows = render(&[1.0; 10]);
        assert!(
            rows[0].chars().any(|c| c != ' '),
            "top row should be lit: {rows:?}"
        );
    }

    #[test]
    fn braille_bits_are_correct() {
        // Unicode dot numbering: 1-2-3 down the left, 4-5-6 down the right,
        // then 7 (bottom left) and 8 (bottom right).
        assert_eq!(braille_bit(0, 0), 0x01);
        assert_eq!(braille_bit(0, 1), 0x02);
        assert_eq!(braille_bit(0, 2), 0x04);
        assert_eq!(braille_bit(1, 0), 0x08);
        assert_eq!(braille_bit(1, 1), 0x10);
        assert_eq!(braille_bit(1, 2), 0x20);
        assert_eq!(braille_bit(0, 3), 0x40);
        assert_eq!(braille_bit(1, 3), 0x80);
    }

    #[test]
    fn flat_line_lights_one_dot_row() {
        // A flat 0 dB curve on a 1-row canvas sits on dot row 2 (of 0..=3,
        // 1.5 rounded up) in every cell: both columns, nothing else — `⠤`
        // (dots 3 + 6). The old row-major bit order drew `⠰` here.
        let area = Rect::new(0, 0, 4, 1);
        let mut buf = Buffer::empty(area);
        EqCurve {
            values: &[0.0; 10],
            color: Color::Rgb(255, 106, 0),
        }
        .render(area, &mut buf);
        for x in 0..4 {
            assert_eq!(buf.cell((x, 0)).unwrap().symbol(), "⠤");
        }
    }
}
