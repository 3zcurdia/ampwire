//! Horizontal and vertical sliders: btop-style track with a knob.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;

/// Horizontal slider. `value` is 0.0–1.0.
pub struct HSlider<'a> {
    pub value: f32,
    pub track: Color,
    pub knob: Color,
    /// Optional label rendered left of the track (e.g. "vol").
    pub label: Option<&'a str>,
    pub label_style: Color,
}

impl Widget for HSlider<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let mut x = area.x;
        if let Some(label) = self.label {
            for c in label.chars() {
                if x >= area.x + area.width {
                    return;
                }
                let cell = &mut buf[(x, area.y)];
                cell.set_symbol(&c.to_string());
                cell.set_fg(self.label_style);
                x += 1;
            }
            // One space after the label.
            if x < area.x + area.width {
                x += 1;
            }
        }
        let track_width = (area.x + area.width).saturating_sub(x);
        if track_width == 0 {
            return;
        }
        let knob_x = x + ((self.value.clamp(0.0, 1.0) * (track_width - 1) as f32).round() as u16);
        for i in 0..track_width {
            let cell = &mut buf[(x + i, area.y)];
            if x + i == knob_x {
                cell.set_symbol("●");
                cell.set_fg(self.knob);
            } else {
                cell.set_symbol("━");
                cell.set_fg(self.track);
            }
        }
    }
}

/// Vertical slider. `value` is -1.0..=1.0 (0 = center), rendered with the
/// knob at the proportional row and a `│` track. The focused band's knob
/// uses `knob`; otherwise `track`.
pub struct VSlider {
    pub value: f32,
    pub track: Color,
    pub knob: Color,
    /// Render the knob in the accent color (focused band).
    pub focused: bool,
}

impl Widget for VSlider {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let height = area.height as f32;
        // value +1 → top row, -1 → bottom row.
        let norm = (self.value + 1.0) / 2.0;
        let row = ((1.0 - norm) * (height - 1.0)).round() as u16;
        let knob_y = area.y + row.min(area.height - 1);
        for i in 0..area.height {
            let cell = &mut buf[(area.x, area.y + i)];
            if area.y + i == knob_y {
                cell.set_symbol("█");
                cell.set_fg(if self.focused { self.knob } else { self.track });
            } else {
                cell.set_symbol("│");
                cell.set_fg(self.track);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hslider_knob_at_ends() {
        let area = Rect::new(0, 0, 5, 1);
        let mut buf = Buffer::empty(area);
        HSlider {
            value: 0.0,
            track: Color::Rgb(255, 106, 0),
            knob: Color::Rgb(0, 229, 255),
            label: None,
            label_style: Color::Rgb(255, 106, 0),
        }
        .render(area, &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "●");
        assert_eq!(buf[(0, 0)].fg, Color::Rgb(0, 229, 255));

        let mut buf = Buffer::empty(area);
        HSlider {
            value: 1.0,
            track: Color::Rgb(255, 106, 0),
            knob: Color::Rgb(0, 229, 255),
            label: None,
            label_style: Color::Rgb(255, 106, 0),
        }
        .render(area, &mut buf);
        assert_eq!(buf[(4, 0)].symbol(), "●");
    }

    #[test]
    fn hslider_label_shifts_track() {
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        HSlider {
            value: 0.0,
            track: Color::Rgb(255, 106, 0),
            knob: Color::Rgb(0, 229, 255),
            label: Some("vol"),
            label_style: Color::Rgb(255, 217, 184),
        }
        .render(area, &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "v");
        // Label "vol" (3) + space → knob starts the track at x=4.
        assert_eq!(buf[(4, 0)].symbol(), "●");
    }

    #[test]
    fn vslider_top_and_bottom() {
        let area = Rect::new(0, 0, 1, 5);
        let mut buf = Buffer::empty(area);
        VSlider {
            value: 1.0,
            track: Color::Rgb(255, 106, 0),
            knob: Color::Rgb(0, 229, 255),
            focused: true,
        }
        .render(area, &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "█");
        assert_eq!(buf[(0, 4)].symbol(), "│");

        let mut buf = Buffer::empty(area);
        VSlider {
            value: -1.0,
            track: Color::Rgb(255, 106, 0),
            knob: Color::Rgb(0, 229, 255),
            focused: true,
        }
        .render(area, &mut buf);
        assert_eq!(buf[(0, 4)].symbol(), "█");
    }
}
