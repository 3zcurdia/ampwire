//! Horizontal scrolling text. Scrolls only when the text doesn't fit.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

/// One line of scrolling text.
///
/// `offset` is in display columns, advanced by the caller on each tick. When
/// the text fits, the offset is ignored and the text is left-aligned.
pub struct Marquee<'a> {
    pub text: &'a str,
    pub offset: usize,
    pub style: Style,
    /// Columns to pad with after the text before wrapping around.
    pub gap: usize,
}

impl Widget for Marquee<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let width = area.width as usize;
        let text_width = self.text.width();

        if text_width <= width {
            // Fits: no scrolling, just render.
            let mut x = 0usize;
            for (g, w) in unicode_segment_widths(self.text) {
                if x + w > width {
                    break;
                }
                if let Some(cell) = buf.cell_mut((area.x + x as u16, area.y)) {
                    cell.set_symbol(&g);
                    cell.set_style(self.style);
                }
                x += w;
            }
            return;
        }

        // Scrolls: index graphemes of text+gap by display column, then walk
        // the visible window [offset, offset+width) with wraparound.
        let cycle = text_width + self.gap;
        let doubled: String = format!("{}{}", self.text, " ".repeat(self.gap));
        let graphemes = unicode_segment_widths(&doubled);
        let mut columns: Vec<(&str, usize)> = Vec::with_capacity(cycle);
        for (g, w) in &graphemes {
            for _ in 0..*w {
                columns.push((g.as_str(), *w));
            }
        }
        let cycle = columns.len().max(1);

        let mut rendered = 0usize;
        let mut i = self.offset % cycle;
        while rendered < width {
            let (g, w) = columns[i % cycle];
            if rendered + w > width {
                break; // never split a wide char across the edge
            }
            if let Some(cell) = buf.cell_mut((area.x + rendered as u16, area.y)) {
                cell.set_symbol(g);
                cell.set_style(self.style);
            }
            // Wide chars occupy a continuation cell, left empty like
            // `Buffer::set_string` does.
            if w > 1 {
                for extra in 1..w {
                    if let Some(cell) = buf.cell_mut((area.x + (rendered + extra) as u16, area.y)) {
                        cell.set_symbol("");
                    }
                }
            }
            rendered += w;
            i += w;
        }
    }
}

/// Split into (grapheme, display width) pairs. Combining marks (zero-width
/// chars) append to their base char; good enough for track titles.
fn unicode_segment_widths(text: &str) -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = Vec::new();
    for c in text.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        match out.last_mut() {
            Some(last) if cw == 0 => last.0.push(c),
            _ => out.push((c.to_string(), cw)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(marquee: Marquee<'_>, w: u16) -> String {
        let area = Rect::new(0, 0, w, 1);
        let mut buf = Buffer::empty(area);
        marquee.render(area, &mut buf);
        (0..w)
            .map(|x| buf.cell((x, 0)).unwrap().symbol().to_string())
            .collect()
    }

    #[test]
    fn short_text_renders_whole() {
        let out = render(
            Marquee {
                text: "abc",
                offset: 0,
                style: Style::default(),
                gap: 4,
            },
            6,
        );
        assert_eq!(out, "abc   ");
    }

    #[test]
    fn long_text_scrolls() {
        let first = render(
            Marquee {
                text: "abcdefgh",
                offset: 0,
                style: Style::default(),
                gap: 4,
            },
            4,
        );
        assert_eq!(first, "abcd");
        let second = render(
            Marquee {
                text: "abcdefgh",
                offset: 2,
                style: Style::default(),
                gap: 4,
            },
            4,
        );
        assert_eq!(second, "cdef");
    }

    #[test]
    fn wide_chars_do_not_break() {
        // CJK is 2 cells; the second cell is an empty continuation, so the
        // rendered symbols concatenate to full characters only.
        let out = render(
            Marquee {
                text: "日本語テスト",
                offset: 0,
                style: Style::default(),
                gap: 2,
            },
            5,
        );
        assert!(out.starts_with("日本"), "{out:?}");
        // Never a half character: 語 would need 2 cells and doesn't fit in
        // the 5th column, so it's dropped (continuation cells are empty).
        assert!(
            out.starts_with("日本") && !out.starts_with("日本語"),
            "{out:?}"
        );
    }
}
