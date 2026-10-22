//! Ampwire: a terminal Winamp 2.0 replica with modern hi-fi playback.
//!
//! Phase 1 (plan 01): UI shell with mock player state — no audio yet.

mod action;
mod app;
mod config;
mod keymap;
mod library;
mod player;
mod ui;

use app::{App, FRAME, ScanMode};
use config::Config;
use keymap::Keymap;
use ui::theme::{ColorDepth, Theme, omarchy};

/// Parsed `--dump WxH` request.
struct Dump {
    width: u16,
    height: u16,
    frame: u64,
}

/// Parse `--dump WxH [--frame N]` from the args, if present.
fn parse_dump(args: &[String]) -> Result<Option<Dump>, String> {
    let Some(idx) = args.iter().position(|a| a == "--dump") else {
        return Ok(None);
    };
    let spec = args
        .get(idx + 1)
        .ok_or_else(|| "--dump needs a size, e.g. --dump 104x18".to_string())?;
    let (w, h) = spec
        .split_once('x')
        .ok_or_else(|| format!("--dump size must look like WxH, got `{spec}`"))?;
    let width: u16 = w
        .parse()
        .map_err(|_| format!("--dump width must be a number, got `{w}`"))?;
    let height: u16 = h
        .parse()
        .map_err(|_| format!("--dump height must be a number, got `{h}`"))?;
    if width == 0 || height == 0 {
        return Err("--dump size must be non-zero".into());
    }

    let frame = match args.iter().position(|a| a == "--frame") {
        Some(fi) => args
            .get(fi + 1)
            .ok_or_else(|| "--frame needs a number".to_string())?
            .parse()
            .map_err(|_| "--frame must be a number".to_string())?,
        None => 0,
    };
    Ok(Some(Dump {
        width,
        height,
        frame,
    }))
}

/// Render one frame into a `TestBackend` and print it as plain text. Never
/// enters raw mode or the alternate screen.
fn dump(dump: Dump) -> std::io::Result<()> {
    let config = Config::default();
    let (keymap, _) = Keymap::from_overrides(&config.keys);
    let theme = Theme::preset("neon").expect("neon preset exists");
    let mut app = App::deterministic(keymap, theme);

    // Advance the deterministic clock so `--frame N` shows a later moment.
    for _ in 0..dump.frame {
        app.tick(FRAME);
    }

    let backend = ratatui::backend::TestBackend::new(dump.width, dump.height);
    let mut terminal =
        ratatui::Terminal::new(backend).map_err(|e| std::io::Error::other(e.to_string()))?;
    terminal
        .draw(|f| ui::render(f, &app))
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    let buf = terminal.backend().buffer();
    for y in 0..dump.height {
        let line: String = (0..dump.width)
            .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
            .collect();
        println!("{line}");
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    match parse_dump(&args) {
        Ok(Some(dump_req)) => return dump(dump_req),
        Ok(None) => {}
        Err(msg) => {
            eprintln!("ampwire: {msg}");
            std::process::exit(2);
        }
    }

    let demo = args.iter().any(|arg| arg == "--demo");

    // Positional args are load paths (`ampwire ~/Music`), but only outside
    // `--dump` (which returns above) — every arg not starting with `--`.
    let paths: Vec<std::path::PathBuf> = args
        .iter()
        .skip(1)
        .filter(|a| !a.starts_with("--"))
        .map(std::path::PathBuf::from)
        .collect();

    let loaded = Config::load();
    let mut config = loaded.config;
    if demo {
        // Demo: rich mock state, default layout, ignore the user config.
        config = Config::default();
    }

    let (keymap, key_conflicts) = Keymap::from_overrides(&config.keys);

    let mut theme = resolve_theme(&config);
    theme.apply_depth(ColorDepth::detect());

    // Non-demo sessions start with an empty playlist; demo/--dump keep the
    // fake one (plan 02, step 8).
    let mut app = if demo {
        App::new(config, keymap, theme, true)
    } else {
        App::new_session(config, keymap, theme)
    };
    // If the on-disk config failed to load, mark it read-only so layout
    // hotkeys never overwrite the broken file.
    if !demo {
        app.set_read_only(loaded.read_only, loaded.notice);
    }
    if let Some(conflict) = key_conflicts.into_iter().next() {
        // One line only in the border; report the first conflict.
        app.ui.notice = Some(conflict);
    }
    if demo {
        app.ui.notice = Some("demo mode: mock player, default layout".into());
    }
    // CLI paths behave like Open: replace the playlist and start at track 1.
    // `--demo` keeps the fake playlist untouched.
    if !demo && !paths.is_empty() {
        app.load_paths(paths, ScanMode::Replace);
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
