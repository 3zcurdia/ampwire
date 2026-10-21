//! Ampwire: a terminal Winamp 2.0 replica with modern hi-fi playback.
//!
//! Phase 1 (plan 01): UI shell with mock player state — no audio yet.

mod action;
mod app;
mod config;
mod keymap;
mod player;
mod ui;

use app::App;
use config::Config;
use keymap::Keymap;
use ui::theme::{ColorDepth, Theme, omarchy};

fn main() -> std::io::Result<()> {
    let demo = std::env::args().any(|arg| arg == "--demo");

    let loaded = Config::load();
    let mut config = loaded.config;
    if demo {
        // Demo: rich mock state, default layout, ignore the user config.
        config = Config::default();
    }

    let (keymap, key_conflicts) = Keymap::from_overrides(&config.keys);

    let mut theme = resolve_theme(&config);
    theme.apply_depth(ColorDepth::detect());

    let mut app = App::new(config, keymap, theme, demo);
    if let Some(notice) = loaded.notice {
        app.ui.notice = Some(notice);
    }
    if let Some(conflict) = key_conflicts.into_iter().next() {
        // One line only in the border; report the first conflict.
        app.ui.notice = Some(conflict);
    }
    if demo {
        app.ui.notice = Some("demo mode: mock player, default layout".into());
    }

    ratatui::run(|terminal| app.run(terminal))
}

/// Build the startup theme: preset, omarchy auto-follow, or fallback.
fn resolve_theme(config: &Config) -> Theme {
    match config.theme.preset.as_str() {
        "omarchy" => {
            let mut watch = omarchy::OmarchyWatch::new();
            match watch.check(config.theme.transparent) {
                Ok(Some(mut theme)) => {
                    App::apply_theme_overrides(&mut theme, config);
                    theme
                }
                _ => {
                    // Omarchy missing or unreadable: silently use neon.
                    let mut theme = Theme::preset("neon").expect("neon preset exists");
                    App::apply_theme_overrides(&mut theme, config);
                    theme
                }
            }
        }
        name => {
            let mut theme = Theme::preset(name).unwrap_or_else(|| {
                eprintln!(
                    "ampwire: unknown theme preset `{name}` (available: {}), using neon",
                    Theme::preset_names().join(", ")
                );
                Theme::preset("neon").expect("neon preset exists")
            });
            App::apply_theme_overrides(&mut theme, config);
            theme
        }
    }
}
