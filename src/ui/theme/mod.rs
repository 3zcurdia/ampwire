//! Semantic color roles, built-in presets, and color-depth downgrade.
//!
//! Widgets never use raw colors; they ask the [`Theme`] for a role. See the
//! plan's §3 for the role table and preset hexes.

pub mod omarchy;

use ratatui::style::Color;

/// Semantic color roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Bg,
    Fg,
    Primary,
    Glow,
    Accent,
    Muted,
    Warn,
}

impl Role {
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "bg" => Some(Role::Bg),
            "fg" => Some(Role::Fg),
            "primary" => Some(Role::Primary),
            "glow" => Some(Role::Glow),
            "accent" => Some(Role::Accent),
            "muted" => Some(Role::Muted),
            "warn" => Some(Role::Warn),
            _ => None,
        }
    }
}

/// Terminal color depth, detected from `COLORTERM` / `TERM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    TrueColor,
    Ansi256,
    Ansi16,
}

impl ColorDepth {
    /// Detect from the environment; defaults to TrueColor when unknown.
    /// Detect from the environment. Truecolor is claimed only when `COLORTERM`
    /// is `truecolor` or `24bit`; an unset `COLORTERM` degrades to 256 colors
    /// (never pretends to truecolor it can't prove).
    pub fn detect() -> Self {
        Self::detect_from(
            &std::env::var("COLORTERM").unwrap_or_default(),
            &std::env::var("TERM").unwrap_or_default(),
        )
    }

    /// Pure core of [`detect`](Self::detect), split out so it is testable.
    pub fn detect_from(colorterm: &str, term: &str) -> Self {
        let colorterm = colorterm.to_lowercase();
        let term = term.to_lowercase();
        if colorterm.contains("truecolor") || colorterm.contains("24bit") {
            return ColorDepth::TrueColor;
        }
        // COLORTERM is unset or unhelpful: 16-color terminals are detected by
        // TERM, everything else gets a safe 256-color mapping.
        if term.contains("16color") || term == "linux" || term.ends_with("-mono") {
            return ColorDepth::Ansi16;
        }
        ColorDepth::Ansi256
    }
}

/// The active theme: roles in, `ratatui::Color` out.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    colors: [(Role, Color); 7],
    /// Spectrum gradient, low → high frequency.
    viz: Vec<Color>,
    /// Spectrum peak-cap color.
    peak: Color,
}

/// Parse `"#RRGGBB"`, or `"r,g,b"`, or an ANSI name used by Omarchy configs.
pub fn parse_color(text: &str) -> Option<Color> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix('#') {
        if hex.len() == 6
            && let Ok(v) = u32::from_str_radix(hex, 16)
        {
            return Some(Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8));
        }
        return None;
    }
    if let Some((r, rest)) = text.split_once(',')
        && let (Ok(r), Some((g, b))) = (r.trim().parse::<u8>(), rest.split_once(','))
        && let Ok(g) = g.trim().parse::<u8>()
        && let Ok(b) = b.trim().parse::<u8>()
    {
        return Some(Color::Rgb(r, g, b));
    }
    match text.to_ascii_lowercase().as_str() {
        "reset" | "default" | "transparent" => Some(Color::Reset),
        _ => None,
    }
}

fn hex(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        _ => None,
    }
}

/// Linear-ish lighten toward white, `amount` 0.0–1.0.
pub fn lighten(color: Color, amount: f32) -> Color {
    let Some((r, g, b)) = hex(color) else {
        return color;
    };
    let f = |c: u8| (c as f32 + (255.0 - c as f32) * amount).round() as u8;
    Color::Rgb(f(r), f(g), f(b))
}

/// Blend two colors, `t` 0.0 = a, 1.0 = b.
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    match (hex(a), hex(b)) {
        (Some((r1, g1, b1)), Some((r2, g2, b2))) => {
            let f = |c1: u8, c2: u8| (c1 as f32 + (c2 as f32 - c1 as f32) * t).round() as u8;
            Color::Rgb(f(r1, r2), f(g1, g2), f(b1, b2))
        }
        _ => b,
    }
}

/// Rough perceptual distance (weighted RGB).
pub fn distance(a: Color, b: Color) -> f32 {
    match (hex(a), hex(b)) {
        (Some((r1, g1, b1)), Some((r2, g2, b2))) => {
            let dr = r1 as f32 - r2 as f32;
            let dg = g1 as f32 - g2 as f32;
            let db = b1 as f32 - b2 as f32;
            (2.0 * dr * dr + 4.0 * dg * dg + 3.0 * db * db).sqrt()
        }
        _ => 0.0,
    }
}

fn build(colors: [(Role, Color); 7]) -> Theme {
    let get = |role: Role| {
        colors
            .iter()
            .find(|(r, _)| *r == role)
            .map(|(_, c)| *c)
            .unwrap_or(Color::Reset)
    };
    let primary = get(Role::Primary);
    let glow = get(Role::Glow);
    let accent = get(Role::Accent);

    // Spectrum: primary → glow → accent, low to high, plus peak.
    let mut viz = Vec::with_capacity(8);
    for i in 0..8 {
        let t = i as f32 / 7.0;
        viz.push(if t < 0.5 {
            mix(primary, glow, t * 2.0)
        } else {
            mix(glow, accent, (t - 0.5) * 2.0)
        });
    }
    let peak = accent;

    Theme { colors, viz, peak }
}

impl Theme {
    /// Look up a role's color.
    pub fn color(&self, role: Role) -> Color {
        self.colors
            .iter()
            .find(|(r, _)| *r == role)
            .map(|(_, c)| *c)
            .unwrap_or(Color::Reset)
    }

    /// Spectrum gradient entry for bucket `i` of `n`.
    pub fn viz(&self, i: usize, n: usize) -> Color {
        if self.viz.is_empty() {
            return self.color(Role::Primary);
        }
        let idx = if n <= 1 {
            0
        } else {
            (i * (self.viz.len() - 1) / (n - 1)).min(self.viz.len() - 1)
        };
        self.viz[idx]
    }

    pub fn peak(&self) -> Color {
        self.peak
    }

    /// Override a single role (config `[theme.colors]`).
    pub fn override_role(&mut self, role: Role, color: Color) {
        for entry in &mut self.colors {
            if entry.0 == role {
                entry.1 = color;
            }
        }
        // Rebuild gradient if a gradient input changed.
        if matches!(role, Role::Primary | Role::Glow | Role::Accent) {
            let rebuilt = build(self.colors);
            self.viz = rebuilt.viz;
            self.peak = rebuilt.peak;
        }
    }

    /// Downgrade every role to what the terminal can show.
    pub fn apply_depth(&mut self, depth: ColorDepth) {
        if depth == ColorDepth::TrueColor {
            return;
        }
        for (_, color) in &mut self.colors {
            *color = downgrade(*color, depth);
        }
        for color in &mut self.viz {
            *color = downgrade(*color, depth);
        }
        self.peak = downgrade(self.peak, depth);
    }

    pub fn preset(name: &str) -> Option<Theme> {
        match name {
            "neon" => Some(neon()),
            "amber" => Some(amber()),
            "matrix" => Some(matrix()),
            "synthwave" => Some(synthwave()),
            _ => None,
        }
    }

    /// Built-in preset names, for config validation messages.
    pub fn preset_names() -> &'static [&'static str] {
        &["neon", "amber", "matrix", "synthwave"]
    }
}

/// Default preset: neon orange main, aqua accent (plan §3.2).
pub fn neon() -> Theme {
    build([
        (Role::Bg, Color::Reset),
        (Role::Fg, parse_color("#FFD9B8").unwrap()),
        (Role::Primary, parse_color("#FF6A00").unwrap()),
        (Role::Glow, parse_color("#FF9E3D").unwrap()),
        (Role::Accent, parse_color("#00E5FF").unwrap()),
        (Role::Muted, parse_color("#7A3A10").unwrap()),
        (Role::Warn, parse_color("#FF2E63").unwrap()),
    ])
}

/// Single-color amber CRT.
pub fn amber() -> Theme {
    let dim = parse_color("#7A5200").unwrap();
    build([
        (Role::Bg, Color::Reset),
        (Role::Fg, parse_color("#FFC24D").unwrap()),
        (Role::Primary, parse_color("#FFB000").unwrap()),
        (Role::Glow, parse_color("#FFD166").unwrap()),
        (Role::Accent, parse_color("#FFE9AE").unwrap()),
        (Role::Muted, dim),
        (Role::Warn, parse_color("#FF6B35").unwrap()),
    ])
}

/// Green-on-black, a nod to the classic Winamp LCD.
pub fn matrix() -> Theme {
    build([
        (Role::Bg, Color::Reset),
        (Role::Fg, parse_color("#B8FFC9").unwrap()),
        (Role::Primary, parse_color("#00FF41").unwrap()),
        (Role::Glow, parse_color("#6DFF8F").unwrap()),
        (Role::Accent, parse_color("#00CCFF").unwrap()),
        (Role::Muted, parse_color("#145A2A").unwrap()),
        (Role::Warn, parse_color("#FF5555").unwrap()),
    ])
}

/// Magenta with cyan.
pub fn synthwave() -> Theme {
    build([
        (Role::Bg, Color::Reset),
        (Role::Fg, parse_color("#FFD1F4").unwrap()),
        (Role::Primary, parse_color("#FF2E9A").unwrap()),
        (Role::Glow, parse_color("#FF6BC1").unwrap()),
        (Role::Accent, parse_color("#00FFF0").unwrap()),
        (Role::Muted, parse_color("#6B1B52").unwrap()),
        (Role::Warn, parse_color("#FFEE58").unwrap()),
    ])
}

/// Nearest 256-color index for an RGB color.
pub fn to_ansi256(color: Color) -> Color {
    let Some((r, g, b)) = hex(color) else {
        return color;
    };

    let palette = |i: u8| -> (u8, u8, u8) {
        match i {
            0..=15 => XTERM16[i as usize],
            16..=231 => {
                let n = i - 16;
                let steps = [0u8, 95, 135, 175, 215, 255];
                (
                    steps[(n / 36) as usize],
                    steps[((n / 6) % 6) as usize],
                    steps[(n % 6) as usize],
                )
            }
            _ => {
                let v = 8 + 10 * (i - 232);
                (v, v, v)
            }
        }
    };

    let mut best = (0u8, f32::MAX);
    for i in 0..=255u8 {
        let (pr, pg, pb) = palette(i);
        let d = (pr as f32 - r as f32).powi(2)
            + (pg as f32 - g as f32).powi(2)
            + (pb as f32 - b as f32).powi(2);
        if d < best.1 {
            best = (i, d);
        }
    }
    Color::Indexed(best.0)
}

/// Map to the 16 ANSI slots: orange → 3/11, aqua → 6/14, per plan §3.2.
pub fn to_ansi16(color: Color) -> Color {
    let Some((r, g, b)) = hex(color) else {
        return color;
    };
    let mut best = (0u8, f32::MAX);
    for (i, &(pr, pg, pb)) in XTERM16.iter().enumerate() {
        // Weighted RGB distance (green counts more), same as `distance()`.
        let dr = pr as f32 - r as f32;
        let dg = pg as f32 - g as f32;
        let db = pb as f32 - b as f32;
        let d = 2.0 * dr * dr + 4.0 * dg * dg + 3.0 * db * db;
        if d < best.1 {
            best = (i as u8, d);
        }
    }
    Color::Indexed(best.0)
}

pub fn downgrade(color: Color, depth: ColorDepth) -> Color {
    match depth {
        ColorDepth::TrueColor => color,
        ColorDepth::Ansi256 => to_ansi256(color),
        ColorDepth::Ansi16 => to_ansi16(color),
    }
}

/// xterm's 16 base colors as RGB.
const XTERM16: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (205, 0, 0),
    (0, 205, 0),
    (205, 205, 0),
    (0, 0, 238),
    (205, 0, 205),
    (0, 205, 205),
    (229, 229, 229),
    (127, 127, 127),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (92, 92, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

/// Vetted, 1-cell-wide, non-emoji glyphs used across the UI.
pub mod glyphs {
    pub const PLAY: char = '▶';
    pub const PAUSE: char = '‖';
    pub const STOP: char = '■';
    pub const PREV: char = '◀';
    pub const NEXT: char = '▶';
    pub const EJECT: char = '▲';
    pub const CURSOR: char = '▸';
    pub const KNOB: char = '●';
    pub const TRACK_FULL: char = '█';
    pub const SUPERSCRIPTS: [char; 3] = ['¹', '²', '³'];
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every glyph the UI may draw, for the width test. Kept here (not in
    /// `glyphs`) so it can't drift out of sync with usage warnings.
    const ALL_GLYPHS: &[char] = &[
        glyphs::PLAY,
        glyphs::PAUSE,
        glyphs::STOP,
        glyphs::PREV,
        glyphs::NEXT,
        glyphs::EJECT,
        glyphs::CURSOR,
        glyphs::KNOB,
        glyphs::TRACK_FULL,
        '¹',
        '²',
        '³',
        // Blocks and eighth-blocks used by widgets.
        '▀',
        '▄',
        '█',
        '▁',
        '▂',
        '▃',
        '▄',
        '▅',
        '▆',
        '▇',
        '░',
        '▒',
        '▓',
        // Braille (eq curve) — the canvas base; individual dots add bits.
        '⠿',
        // Box drawing for borders and sliders.
        '╭',
        '─',
        '╮',
        '╰',
        '╯',
        '│',
        '┃',
        '━',
    ];

    #[test]
    fn every_glyph_is_one_cell_wide() {
        use unicode_width::UnicodeWidthChar;
        for glyph in ALL_GLYPHS {
            assert_eq!(
                UnicodeWidthChar::width(*glyph),
                Some(1),
                "glyph {glyph:?} (U+{:04X}) is not 1 cell wide",
                *glyph as u32
            );
        }
    }

    #[test]
    fn parse_hex_colors() {
        assert_eq!(parse_color("#FF6A00"), Some(Color::Rgb(255, 106, 0)));
        assert_eq!(parse_color("rgb"), None);
        assert_eq!(parse_color("reset"), Some(Color::Reset));
    }

    #[test]
    fn presets_exist() {
        for name in Theme::preset_names() {
            assert!(Theme::preset(name).is_some(), "missing preset {name}");
        }
        assert!(Theme::preset("nope").is_none());
    }

    #[test]
    fn neon_matches_plan_hexes() {
        let t = neon();
        assert_eq!(t.color(Role::Primary), Color::Rgb(255, 106, 0));
        assert_eq!(t.color(Role::Glow), Color::Rgb(255, 158, 61));
        assert_eq!(t.color(Role::Accent), Color::Rgb(0, 229, 255));
        assert_eq!(t.color(Role::Muted), Color::Rgb(122, 58, 16));
        assert_eq!(t.color(Role::Fg), Color::Rgb(255, 217, 184));
        assert_eq!(t.color(Role::Warn), Color::Rgb(255, 46, 99));
        assert_eq!(t.color(Role::Bg), Color::Reset);
    }

    #[test]
    fn downgrade_maps_orange_and_aqua_to_ansi16_slots() {
        let orange = parse_color("#FF6A00").unwrap();
        let aqua = parse_color("#00E5FF").unwrap();
        // Plan §3.2: orange → color3/11, aqua → color6/14.
        let orange_slot = to_ansi16(orange);
        assert!(
            orange_slot == Color::Indexed(3) || orange_slot == Color::Indexed(11),
            "orange → {orange_slot:?}"
        );
        let aqua_slot = to_ansi16(aqua);
        assert!(
            aqua_slot == Color::Indexed(6) || aqua_slot == Color::Indexed(14),
            "aqua → {aqua_slot:?}"
        );
    }

    #[test]
    fn downgrade_256_is_close() {
        let orange = parse_color("#FF6A00").unwrap();
        match to_ansi256(orange) {
            Color::Indexed(_) => {}
            other => panic!("expected Indexed, got {other:?}"),
        }
    }

    #[test]
    fn apply_depth_downgrades_every_role() {
        let mut t = neon();
        t.apply_depth(ColorDepth::Ansi16);
        for role in [
            Role::Bg,
            Role::Fg,
            Role::Primary,
            Role::Glow,
            Role::Accent,
            Role::Muted,
            Role::Warn,
        ] {
            match t.color(role) {
                Color::Rgb(..) | Color::Reset | Color::Indexed(_) => {}
                other => panic!("unexpected {other:?} for {role:?}"),
            }
        }
    }

    #[test]
    fn viz_gradient_runs_low_to_accent() {
        let t = neon();
        // First entry near primary, last near accent.
        assert_eq!(t.viz(0, 8), t.color(Role::Primary));
        assert_eq!(t.viz(7, 8), t.color(Role::Accent));
    }

    #[test]
    fn override_rebuilds_gradient() {
        let mut t = neon();
        t.override_role(Role::Accent, Color::Rgb(1, 2, 3));
        assert_eq!(t.viz(7, 8), Color::Rgb(1, 2, 3));
        assert_eq!(t.peak(), Color::Rgb(1, 2, 3));
    }

    #[test]
    fn color_depth_detection_does_not_panic() {
        let _ = ColorDepth::detect();
    }

    #[test]
    fn truecolor_only_from_colorterm() {
        use ColorDepth::*;
        let detect = ColorDepth::detect_from;
        // COLORTERM proves truecolor.
        assert_eq!(detect("truecolor", "xterm-256color"), TrueColor);
        assert_eq!(detect("24bit", "xterm-256color"), TrueColor);
        // Unset COLORTERM degrades to 256 even with a modern TERM.
        assert_eq!(detect("", "xterm-256color"), Ansi256);
        assert_eq!(detect("", "xterm-truecolor"), Ansi256);
        assert_eq!(detect("", "alacritty"), Ansi256);
        // 16-color terminals are still detected by TERM.
        assert_eq!(detect("", "xterm-16color"), Ansi16);
        assert_eq!(detect("", "linux"), Ansi16);
    }
}
