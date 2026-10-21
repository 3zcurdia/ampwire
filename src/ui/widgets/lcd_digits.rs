//! 2-row half-block LCD time digits, Winamp-style.
//!
//! Each digit is a 2×4 sub-cell grid rendered with half blocks: the top row
//! of each terminal line uses `▀`, the bottom `▄`, `█` for both, space for
//! neither. A colon is one column wide.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;

/// 2 columns × 4 sub-rows per digit, top-left first.
const DIGITS: [[u8; 8]; 10] = [
    // 0
    [1, 1, 1, 0, 1, 0, 1, 1],
    // 1
    [0, 1, 1, 1, 0, 1, 1, 1],
    // 2
    [1, 1, 0, 1, 1, 1, 1, 0],
    // 3
    [1, 1, 0, 1, 1, 1, 0, 1],
    // 4
    [1, 0, 1, 1, 1, 1, 0, 1],
    // 5
    [1, 1, 1, 0, 1, 1, 0, 1],
    // 6
    [1, 1, 1, 0, 1, 1, 1, 1],
    // 7
    [1, 1, 0, 1, 0, 1, 0, 1],
    // 8
    [1, 1, 1, 1, 1, 1, 1, 1],
    // 9
    [1, 1, 1, 1, 1, 1, 0, 1],
];

/// Colon: 1 column × 4 sub-rows, lit at rows 1 and 2.
const COLON: [u8; 4] = [0, 1, 1, 0];

/// Width in columns of an LCD string (`"1:23"` → 2+1+2+2 = 7... digits 2 wide, colon 1).
pub fn lcd_width(text: &str) -> u16 {
    text.chars()
        .map(|c| if c == ':' || c == '.' { 1 } else { 2 })
        .sum()
}

/// Renders `text` (`"1:23"`, `"1:23:45"` with `.` separators allowed) as
/// 2-row half-block digits. Non-digit, non-separator chars render blank.
pub struct LcdDigits<'a> {
    pub text: &'a str,
    pub color: Color,
    /// Unlit segments use this color (blink effect: pass `muted` while paused).
    pub dim: Color,
    /// When false, every segment renders unlit (blink off-phase).
    pub lit: bool,
}

impl Widget for LcdDigits<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height < 2 {
            return;
        }
        let half = |top: u8, bottom: u8| match (top == 1, bottom == 1) {
            (true, true) => '█',
            (true, false) => '▀',
            (false, true) => '▄',
            (false, false) => ' ',
        };
        let mut x = area.x;
        for c in self.text.chars() {
            if x + 2 > area.x + area.width {
                break;
            }
            let fg = if self.lit { self.color } else { self.dim };
            match c {
                '0'..='9' => {
                    let grid = &DIGITS[(c as u8 - b'0') as usize];
                    for terminal_row in 0..2u16 {
                        let top = terminal_row * 2;
                        let bottom = top + 1;
                        for col in 0..2u16 {
                            let idx = (top * 2 + col) as usize;
                            let idx2 = (bottom * 2 + col) as usize;
                            let cell = &mut buf[(x + col, area.y + terminal_row)];
                            cell.set_symbol(&half(grid[idx], grid[idx2]).to_string());
                            cell.set_fg(fg);
                        }
                    }
                    x += 2;
                }
                ':' | '.' => {
                    // Lit at sub-rows 1 and 2 → '▀' over '▄'.
                    for terminal_row in 0..2usize {
                        let top = COLON[terminal_row * 2];
                        let bottom = COLON[terminal_row * 2 + 1];
                        let cell = &mut buf[(x, area.y + terminal_row as u16)];
                        cell.set_symbol(&half(top, bottom).to_string());
                        cell.set_fg(fg);
                    }
                    x += 1;
                }
                ' ' => x += 1,
                _ => x += 2, // unknown: blank advance
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_text(text: &str) -> Vec<String> {
        let area = Rect::new(0, 0, lcd_width(text), 2);
        let mut buf = Buffer::empty(area);
        LcdDigits {
            text,
            color: Color::Rgb(255, 106, 0),
            dim: Color::Rgb(122, 58, 16),
            lit: true,
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
    fn width_counts() {
        assert_eq!(lcd_width("1:23"), 7);
        assert_eq!(lcd_width("12:34"), 9);
        assert_eq!(lcd_width("0:00"), 7);
    }

    #[test]
    fn digits_render_two_rows() {
        let rows = render_text("1:23");
        assert_eq!(rows.len(), 2);
        // All rows fit the width.
        for row in &rows {
            assert_eq!(row.chars().count() as u16, lcd_width("1:23"));
        }
    }

    #[test]
    fn unlit_renders_blanks() {
        let area = Rect::new(0, 0, lcd_width("8"), 2);
        let mut buf = Buffer::empty(area);
        LcdDigits {
            text: "8",
            color: Color::Rgb(255, 106, 0),
            dim: Color::Rgb(122, 58, 16),
            lit: false,
        }
        .render(area, &mut buf);
        for y in 0..area.height {
            for x in 0..area.width {
                let cell = buf.cell((x, y)).unwrap();
                assert!(
                    cell.symbol() == " " || cell.fg == Color::Rgb(122, 58, 16),
                    "unlit segment must not use the lit color"
                );
            }
        }
    }

    #[test]
    fn eight_fills_both_columns() {
        let rows = render_text("8");
        // Digit 8 is fully lit: top line all '█', bottom line all '█'.
        assert!(rows[0].contains('█') && rows[1].contains('█'));
    }
}
