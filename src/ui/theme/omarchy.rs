//! Omarchy auto-follow: read the active theme's `colors.toml`, map its keys to
//! ampwire roles, and re-check once a second for changes.
//!
//! Research notes (omacom/omarchy, branch `quattro`, checked 2026-02):
//! - `bin/omarchy-theme-set` sets `CURRENT_THEME_PATH` to
//!   `~/.local/state/omarchy/current/theme` — a real directory swapped in with
//!   `rm -rf` + `mv`, **not** a symlink under `~/.config/omarchy/current`.
//!   We still probe the older `~/.config/omarchy/current/theme` path for
//!   compatibility, then the `~/.local/state` one.
//! - `colors.toml` uses **named** keys: `accent`, `muted`, `background`,
//!   `foreground`, `red`, `cyan`, `bright_*`, … Legacy themes may only have
//!   `color0`–`color15`; those are accepted too.
//!
//! `AMPWIRE_OMARCHY_DIR` overrides the theme directory for tests.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ratatui::style::Color;

use super::{Role, Theme};
use crate::ui::theme::{distance, lighten, parse_color};

/// Probe theme-directory candidates, newest layout first.
pub fn theme_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("AMPWIRE_OMARCHY_DIR") {
        let dir = PathBuf::from(dir);
        return dir.is_dir().then_some(dir);
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    // Current Omarchy: a real directory, atomically swapped.
    let state = home.join(".local/state/omarchy/current/theme");
    if state.join("colors.toml").is_file() {
        return Some(state);
    }
    // Older layout: a symlink under .config.
    let config = home.join(".config/omarchy/current/theme");
    if config.join("colors.toml").is_file() {
        return Some(config);
    }
    None
}

/// A parsed `colors.toml` palette.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Palette {
    pub accent: Option<Color>,
    pub muted: Option<Color>,
    pub background: Option<Color>,
    pub foreground: Option<Color>,
    pub red: Option<Color>,
    pub cyan: Option<Color>,
    pub color: [Option<Color>; 16],
}

/// Parse `colors.toml` text. Not a full TOML parser — just the flat
/// `key = "#RRGGBB"` lines Omarchy themes use, tolerant of extra keys
/// (`mode`, `selection`, `lighter_background`, …) and `#` comments.
pub fn parse_colors_toml(text: &str) -> Palette {
    let mut palette = Palette::default();
    for line in text.lines() {
        // Split on '=' first so a '#' inside a quoted hex survives.
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.split('#').next().unwrap_or("").trim();
        let value = value.trim();
        // Quoted values ("#RRGGBB") keep their '#'; bare values may have a
        // trailing `# comment`.
        let value = if value.starts_with('"') || value.starts_with('\'') {
            value.trim_matches(|c| c == '"' || c == '\'').to_string()
        } else {
            value.split('#').next().unwrap_or("").trim().to_string()
        };
        let Some(color) = parse_color(&value) else {
            continue;
        };
        match key {
            "accent" => palette.accent = Some(color),
            "muted" => palette.muted = Some(color),
            "background" => palette.background = Some(color),
            "foreground" => palette.foreground = Some(color),
            "red" => palette.red = Some(color),
            "cyan" => palette.cyan = Some(color),
            _ => {
                if let Some(rest) = key.strip_prefix("color")
                    && let Ok(i) = rest.parse::<usize>()
                    && i < 16
                {
                    palette.color[i] = Some(color);
                }
            }
        }
    }
    palette
}

/// Map an Omarchy palette onto ampwire roles (plan §3.3).
///
/// `primary` ← `accent`; `glow` ← lightened `accent`; `accent` ← `cyan` (or
/// `color14`/lightened when too close to `primary`); `muted` ← theme `muted`
/// or `color8`; `fg` ← `foreground`; `warn` ← `red`/`color1`; `bg` stays
/// transparent unless `transparent = false`.
pub fn map_roles(palette: &Palette, transparent: bool) -> Theme {
    let mut theme = crate::ui::theme::neon(); // base; every known role is overwritten below

    let primary = palette
        .accent
        .or(palette.color[4])
        .unwrap_or_else(|| theme.color(Role::Primary));
    let glow = lighten(primary, 0.3);

    // Cyan, or color14 (or a lightened cyan) when cyan sits too close to primary.
    let mut accent = palette.cyan.or(palette.color[6]).unwrap_or(primary);
    if distance(accent, primary) < 40.0 {
        accent = match palette.color[14] {
            Some(bright_cyan) => bright_cyan,
            None => lighten(accent, 0.35),
        };
    }

    let muted = palette
        .muted
        .or(palette.color[8])
        .unwrap_or_else(|| theme.color(Role::Muted));
    let fg = palette
        .foreground
        .or(palette.color[7])
        .unwrap_or_else(|| theme.color(Role::Fg));
    let warn = palette
        .red
        .or(palette.color[1])
        .unwrap_or_else(|| theme.color(Role::Warn));
    let bg = if transparent {
        Color::Reset
    } else {
        palette.background.unwrap_or(Color::Rgb(20, 20, 28))
    };

    theme.override_role(Role::Primary, primary);
    theme.override_role(Role::Glow, glow);
    theme.override_role(Role::Accent, accent);
    theme.override_role(Role::Muted, muted);
    theme.override_role(Role::Fg, fg);
    theme.override_role(Role::Warn, warn);
    theme.override_role(Role::Bg, bg);
    theme
}

/// Load and map the theme in `dir`, if it has a usable `colors.toml`.
pub fn load_theme(dir: &Path, transparent: bool) -> Result<Theme, String> {
    let path = dir.join("colors.toml");
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let palette = parse_colors_toml(&text);
    if palette == Palette::default() {
        return Err(format!("{}: no recognizable colors", path.display()));
    }
    Ok(map_roles(&palette, transparent))
}

/// Watches the Omarchy theme for changes without a file-watcher dependency:
/// check the directory identity + `colors.toml` mtime once a second.
#[derive(Debug, Default)]
pub struct OmarchyWatch {
    seen: Option<(PathBuf, SystemTime)>,
}

/// Identity of a theme: canonical dir path + colors.toml mtime.
fn fingerprint(dir: &Path) -> Option<(PathBuf, SystemTime)> {
    let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let mtime = fs::metadata(dir.join("colors.toml"))
        .and_then(|m| m.modified())
        .ok()?;
    Some((canonical, mtime))
}

impl OmarchyWatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Ok(Some(theme))` when the theme is new or changed since the last
    /// call; `Ok(None)` unchanged; `Err(reason)` present but unusable.
    pub fn check(&mut self, transparent: bool) -> Result<Option<Theme>, String> {
        let Some(dir) = theme_dir() else {
            self.seen = None;
            return Err("omarchy not found".into());
        };
        self.check_dir(&dir, transparent)
    }

    /// Same as [`check`](Self::check), but against an explicit directory —
    /// used by tests and by the `AMPWIRE_OMARCHY_DIR` override.
    pub fn check_dir(&mut self, dir: &Path, transparent: bool) -> Result<Option<Theme>, String> {
        let Some(fp) = fingerprint(dir) else {
            return Err(format!("{}/colors.toml: unreadable", dir.display()));
        };
        if self.seen.as_ref() == Some(&fp) {
            return Ok(None);
        }
        self.seen = Some(fp);
        load_theme(dir, transparent).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKYO: &str = r##"
mode = "dark"

accent = "#7aa2f7"
selection = "#292e42"
muted = "#414868"

background = "#1a1b26"
foreground = "#a9b1d6"

red = "#f7768e"
cyan = "#449dab"
"##;

    #[test]
    fn parses_named_keys() {
        let p = parse_colors_toml(TOKYO);
        assert_eq!(p.accent, Some(Color::Rgb(0x7a, 0xa2, 0xf7)));
        assert_eq!(p.muted, Some(Color::Rgb(0x41, 0x48, 0x68)));
        assert_eq!(p.background, Some(Color::Rgb(0x1a, 0x1b, 0x26)));
        assert_eq!(p.foreground, Some(Color::Rgb(0xa9, 0xb1, 0xd6)));
        assert_eq!(p.red, Some(Color::Rgb(0xf7, 0x76, 0x8e)));
        assert_eq!(p.cyan, Some(Color::Rgb(0x44, 0x9d, 0xab)));
    }

    #[test]
    fn parses_legacy_color_slots() {
        let text = concat!(
            "color0 = \"#000000\"\n",
            "color6 = \"#00ffff\"\n",
            "color1 = \"#ff0000\"\n",
            "color8 = \"#808080\"\n",
        );
        let p = parse_colors_toml(text);
        assert_eq!(p.color[6], Some(Color::Rgb(0, 255, 255)));
        assert_eq!(p.color[1], Some(Color::Rgb(255, 0, 0)));
    }

    #[test]
    fn maps_roles_from_palette() {
        let p = parse_colors_toml(TOKYO);
        let t = map_roles(&p, true);
        // primary = accent
        assert_eq!(t.color(Role::Primary), Color::Rgb(0x7a, 0xa2, 0xf7));
        // accent = cyan (kept: distance from primary is large enough)
        assert_eq!(t.color(Role::Accent), Color::Rgb(0x44, 0x9d, 0xab));
        assert_eq!(t.color(Role::Muted), Color::Rgb(0x41, 0x48, 0x68));
        assert_eq!(t.color(Role::Fg), Color::Rgb(0xa9, 0xb1, 0xd6));
        assert_eq!(t.color(Role::Warn), Color::Rgb(0xf7, 0x76, 0x8e));
        assert_eq!(t.color(Role::Bg), Color::Reset);
    }

    #[test]
    fn opaque_bg_uses_background() {
        let p = parse_colors_toml(TOKYO);
        let t = map_roles(&p, false);
        assert_eq!(t.color(Role::Bg), Color::Rgb(0x1a, 0x1b, 0x26));
    }

    #[test]
    fn too_close_accent_gets_brightened() {
        // cyan == accent → should pick color14 when present.
        let text = "accent = \"#7aa2f7\"\ncyan = \"#7aa2f7\"\ncolor14 = \"#00ffff\"\n";
        let p = parse_colors_toml(text);
        let t = map_roles(&p, true);
        assert_eq!(t.color(Role::Accent), Color::Rgb(0, 255, 255));
    }

    #[test]
    fn fixture_dir_round_trips_and_detects_change() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("colors.toml"), TOKYO).unwrap();

        let mut watch = OmarchyWatch::new();
        let first = watch.check_dir(dir.path(), true).unwrap();
        assert!(first.is_some(), "first check should load the theme");
        let second = watch.check_dir(dir.path(), true).unwrap();
        assert!(second.is_none(), "unchanged theme should not reload");

        // Rewrite with a different palette → change detected.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(
            dir.path().join("colors.toml"),
            "accent = \"#ff0000\"\ncyan = \"#00ff00\"\nforeground = \"#ffffff\"\n",
        )
        .unwrap();
        let third = watch
            .check_dir(dir.path(), true)
            .unwrap()
            .expect("change detected");
        assert_eq!(third.color(Role::Primary), Color::Rgb(255, 0, 0));
    }

    #[test]
    fn unreadable_dir_reports_reason() {
        let mut watch = OmarchyWatch::new();
        let err = watch
            .check_dir(Path::new("/nonexistent/omarchy"), true)
            .unwrap_err();
        assert!(err.contains("unreadable"), "{err}");
    }

    #[test]
    fn dir_without_colors_reports_reason() {
        let dir = tempfile::tempdir().unwrap();
        let mut watch = OmarchyWatch::new();
        let err = watch.check_dir(dir.path(), true).unwrap_err();
        assert!(err.contains("colors.toml"), "{err}");
    }
}
