//! 2-row LCD time digits, Winamp-style.
//!
//! Each digit is a 3-column, 2-row glyph (`"█▀▀"` over `"▄▄█"` etc). Glyphs
//! are separated by one blank column; the colon is a single column. Digits 0–3
//! and the colon match the plan §2.3 mockup; 4–9 follow the same style.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;

/// Top and bottom row of each digit 0–9. Every glyph is 3 columns wide.
const DIGITS: [(&str, &str); 10] = [
    ("█▀█", "█▄█"), // 0
    ("▀█ ", "▄█▄"), // 1
    ("▀▀█", "█▄▄"), // 2
    ("▀▀█", "▄▄█"), // 3
    ("█ █", "▀▀█"), // 4
    ("█▀▀", "▄▄█"), // 5
    ("█▀▀", "█▄█"), // 6
    ("▀▀█", "  █"), // 7
    ("█▀█", "█▀█"), // 8
    ("█▀█", "▀▀█"), // 9
];

/// Colon: 1 column wide, lit on both rows (a half-block dot each).
const COLON: (&str, &str) = ("▄", "▄");

/// Width in columns of an LCD string, including one blank column between
/// glyphs. Digits are 3 wide, the colon 1.
pub fn lcd_width(text: &str) -> u16 {
    let mut width = 0u16;
    let mut glyphs = 0u16;
    for c in text.chars() {
        width += match c {
            ':' | '.' | ' ' => 1,
            _ => 3,
        };
        glyphs += 1;
    }
    // One blank column between each pair of glyphs.
    width + glyphs.saturating_sub(1)
}

/// Renders `text` (`"1:23"`, `"12:34"`) as 2-row LCD digits.
pub struct LcdDigits<'a> {
    pub text: &'a str,
    /// Lit segments (playing). Digits use the `primary` role.
    pub color: Color,
    /// Unlit segments: the pause-blink off phase, in `muted`.
    pub dim: Color,
    /// When false every segment renders in `dim` (blink off phase).
    pub lit: bool,
}

impl Widget for LcdDigits<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height < 2 {
            return;
        }
        let fg = if self.lit { self.color } else { self.dim };
        let mut x = area.x;
        let right = area.x + area.width;

        for c in self.text.chars() {
            let (top, bottom): (&str, &str) = match c {
                '0'..='9' => DIGITS[(c as u8 - b'0') as usize],
                ':' | '.' => COLON,
                ' ' => {
                    x += 2; // one blank glyph + its separator
                    continue;
                }
                _ => {
                    x += 4; // unknown: blank glyph + separator
                    continue;
                }
            };
            let width = top.chars().count() as u16;
            if x + width > right {
                break;
            }
            write_row(buf, x, area.y, top, fg);
            write_row(buf, x, area.y + 1, bottom, fg);
            x += width + 1; // glyph + one blank column
        }
    }
}

/// Write one 1-row glyph string starting at (x, y), clipped to the buffer.
fn write_row(buf: &mut Buffer, x: u16, y: u16, glyphs: &str, fg: Color) {
    for (i, c) in glyphs.chars().enumerate() {
        let cx = x + i as u16;
        if let Some(cell) = buf.cell_mut((cx, y)) {
            cell.set_char(c);
            cell.set_fg(fg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORANGE: Color = Color::Rgb(255, 106, 0);
    const MUTED: Color = Color::Rgb(122, 58, 16);

    fn render_text(text: &str, lit: bool) -> Vec<String> {
        let area = Rect::new(0, 0, lcd_width(text), 2);
        let mut buf = Buffer::empty(area);
        LcdDigits {
            text,
            color: ORANGE,
            dim: MUTED,
            lit,
        }
        .render(area, &mut buf);
        (0..2)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn width_includes_separators() {
        // "1:23" = 3 + 1 + 1 + 3 + 3 glyphs widths (10) + 3 separators = 13.
        assert_eq!(lcd_width("1:23"), 13);
        // "12:34" = 3+3+1+3+3 = 13 + 4 separators = 17.
        assert_eq!(lcd_width("12:34"), 17);
        assert_eq!(lcd_width("0"), 3);
    }

    #[test]
    fn all_ten_glyphs_are_distinct() {
        let mut seen = std::collections::HashSet::new();
        for (top, bottom) in DIGITS {
            let glyph = format!("{top}|{bottom}");
            assert!(seen.insert(glyph.clone()), "duplicate digit glyph: {glyph}");
        }
        assert_eq!(seen.len(), 10);
        // Every glyph is 3 columns wide.
        for (top, bottom) in DIGITS {
            assert_eq!(top.chars().count(), 3, "{top}");
            assert_eq!(bottom.chars().count(), 3, "{bottom}");
        }
    }

    #[test]
    fn digits_match_the_plan_mockup() {
        // 0–3 and the colon are copied from the plan §2.3 mockup. Digit 1 is
        // "▀█ " (padded), so a separator after it yields two spaces.
        let rows = render_text("0123", true);
        assert_eq!(rows[0], "█▀█ ▀█  ▀▀█ ▀▀█");
        assert_eq!(rows[1], "█▄█ ▄█▄ █▄▄ ▄▄█");
        let colon = render_text(":", true);
        assert_eq!(colon[0], "▄");
        assert_eq!(colon[1], "▄");
    }

    #[test]
    fn lit_uses_primary_unlit_uses_muted() {
        // Blink phases share glyphs but differ in color, so compare the color
        // of a lit segment.
        let area = Rect::new(0, 0, lcd_width("1"), 2);
        let mut buf = Buffer::empty(area);
        LcdDigits {
            text: "1",
            color: ORANGE,
            dim: MUTED,
            lit: true,
        }
        .render(area, &mut buf);
        assert_eq!(buf.cell((1, 0)).unwrap().fg, ORANGE, "lit → primary");

        let mut buf = Buffer::empty(area);
        LcdDigits {
            text: "1",
            color: ORANGE,
            dim: MUTED,
            lit: false,
        }
        .render(area, &mut buf);
        assert_eq!(buf.cell((1, 0)).unwrap().fg, MUTED, "unlit → muted");
    }

    #[test]
    fn too_narrow_renders_nothing_rather_than_panicking() {
        let area = Rect::new(0, 0, 2, 2);
        let mut buf = Buffer::empty(area);
        LcdDigits {
            text: "12:34",
            color: ORANGE,
            dim: MUTED,
            lit: true,
        }
        .render(area, &mut buf);
        // A 3-wide digit doesn't fit in 2 columns, so nothing is drawn.
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), " ");
    }
}
