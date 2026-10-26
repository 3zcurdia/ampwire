//! Ampwire: a terminal player inspired by Winamp 2, implemented from scratch
//! for the terminal, with modern hi-fi playback.
//!
//! Non-demo sessions play for real (plan 03: `crate::audio` decodes and the
//! `player::real` backend drives it); `--demo`, `--dump` and the tests keep
//! the mock player.

mod action;
mod app;
mod audio;
mod config;
#[cfg(unix)]
mod control;
mod jellyfin;
mod keymap;
mod library;
#[cfg(target_os = "linux")]
mod mpris;
mod player;
mod remote;
mod smb;
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

/// Print the short help text and exit.
fn print_help() {
    println!(
        "Usage: ampwire [OPTIONS] [PATH|smb://SERVER/SHARE ...]\n\
\n\
  --demo             run the mock player with the default layout\n\
  --dump WxH         render one frame and exit\n\
  --frame N          advance the mock clock N frames before --dump\n\
  --status [--json]  print the running session's state, then exit\n\
  --ctl <command>    control the running session:\n\
                       play | pause | toggle | stop | next | prev\n\
                       seek <seconds> | seek-to <seconds>\n\
                       volume <0-100> | shuffle | repeat\n\
                       load <path|smb://…> | quit\n\
  -V, --version      print the version and exit\n\
  -h, --help         print this help and exit\n"
    );
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

/// Positional args → load inputs: everything that does not start with
/// `--`, each parsed as a local path or an `smb://` URL. Mixing the two on
/// one command line is allowed (plan 10, "Where URLs are accepted"); a
/// malformed `smb://` is a usage error, reported by `main` as
/// `ampwire: …` + exit 2 (the same path as a bad `--dump`).
fn load_inputs_from_args(args: &[String]) -> Result<Vec<smb::LoadInput>, String> {
    args.iter()
        .skip(1)
        .filter(|arg| !arg.starts_with("--"))
        .map(|arg| smb::parse_load_input(arg))
        .collect()
}

/// Handle the non-session control flags before any config, audio or TUI
/// starts. Returns the process exit code when one of them was requested.
#[cfg(unix)]
fn remote_cli(args: &[String]) -> Option<i32> {
    let status = args.iter().any(|arg| arg == "--status");
    let ctl = args.iter().position(|arg| arg == "--ctl");
    match (status, ctl) {
        (true, Some(_)) => {
            eprintln!("ampwire: --status and --ctl are exclusive");
            Some(2)
        }
        (true, None) => Some(control::client_status(
            args.iter().any(|arg| arg == "--json"),
        )),
        (false, Some(index)) => Some(control::client_ctl(&args[index + 1..])),
        (false, None) => None,
    }
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // `--version`/`--help` must return before any session starts: no config
    // read, no sound card, no raw mode, no TTY. (Plan 11, step 1.)
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("ampwire {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }

    // Status/control talk to an already-running session and must not read
    // config, open the sound card or need a TTY.
    #[cfg(unix)]
    if let Some(code) = remote_cli(&args) {
        use std::io::Write as _;
        // `process::exit` skips destructors; flush piped stdout explicitly.
        let _ = std::io::stdout().flush();
        std::process::exit(code);
    }
    #[cfg(not(unix))]
    if args.iter().any(|a| a == "--status" || a == "--ctl") {
        eprintln!("ampwire: status/control is not supported on this platform");
        std::process::exit(1);
    }

    match parse_dump(&args) {
        Ok(Some(dump_req)) => return dump(dump_req),
        Ok(None) => {}
        Err(msg) => {
            eprintln!("ampwire: {msg}");
            std::process::exit(2);
        }
    }

    let demo = args.iter().any(|arg| arg == "--demo");

    // Positional args are load inputs (`ampwire ~/Music smb://nas/More`),
    // but only outside `--dump` (which returns above) — every arg not
    // starting with `--`. A malformed `smb://` is a fatal CLI error, same
    // as a bad `--dump` (plan 10, step 1).
    let inputs: Vec<smb::LoadInput> = match load_inputs_from_args(&args) {
        Ok(inputs) => inputs,
        Err(msg) => {
            eprintln!("ampwire: {msg}");
            std::process::exit(2);
        }
    };

    // One shared view and one command channel for the control socket and
    // MPRIS. Bind the control socket before `Config::load`/`RealPlayer::new`
    // so a second launch forwards its paths and exits without touching the
    // sound card.
    let shared_view = remote::new_view();
    // The process exists from here on; a status client racing startup should
    // see a stopped-but-running session, not "not running".
    remote::write_view(
        &shared_view,
        remote::View {
            running: true,
            ..remote::View::default()
        },
    );
    let (remote_tx, remote_rx) = std::sync::mpsc::channel();
    let mut remote_started = false;

    #[cfg(unix)]
    let _control_server =
        match control::start(shared_view.clone(), remote_tx.clone(), &inputs, !demo) {
            control::Start::Server(server) => {
                remote_started = true;
                Some(server)
            }
            control::Start::AlreadyRunning => {
                eprintln!("ampwire: already running");
                return Ok(());
            }
            control::Start::Off => None,
        };

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

    // Linux media keys/Omarchy's media widget need MPRIS; a missing session
    // bus just disables it (the UI and control socket keep working).
    #[cfg(target_os = "linux")]
    let _mpris = if demo {
        None
    } else {
        let handle = mpris::start(shared_view.clone(), remote_tx.clone());
        if handle.is_some() {
            remote_started = true;
        }
        handle
    };

    if remote_started {
        app.set_remote(remote_rx, shared_view.clone());
    }

    // CLI args behave like Open: replace the playlist and start at track 1.
    // `--demo` keeps the fake playlist untouched.
    if !demo && !inputs.is_empty() {
        app.load_inputs(inputs, ScanMode::Replace);
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
                    "ampwire: unknown theme preset `{name}` (available: {}, omarchy), using neon",
                    Theme::preset_names().join(", ")
                );
                Theme::preset("neon").expect("neon preset exists")
            });
            App::apply_theme_overrides(&mut theme, config);
            theme
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smb::{LoadInput, SmbUrl};

    /// `["ampwire", …]` → the args a real invocation would have.
    fn args(list: &[&str]) -> Vec<String> {
        std::iter::once("ampwire")
            .chain(list.iter().copied())
            .map(String::from)
            .collect()
    }

    /// Plan 10, step 1: local and `smb://` positionals may be mixed on one
    /// command line, and flags are never inputs.
    #[test]
    fn cli_args_route_local_and_smb_inputs_mixed() {
        let parsed = load_inputs_from_args(&args(&[
            "~/Music",
            "--demo",
            "smb://nas/More-Music",
            "./local album",
        ]))
        .expect("all valid");
        assert_eq!(
            parsed,
            vec![
                LoadInput::Local(std::path::PathBuf::from("~/Music")),
                LoadInput::Smb(SmbUrl {
                    server: "nas".into(),
                    share: "More-Music".into(),
                    path: String::new(),
                }),
                LoadInput::Local(std::path::PathBuf::from("./local album")),
            ]
        );
        assert!(
            load_inputs_from_args(&args(&[]))
                .expect("no args")
                .is_empty()
        );
    }

    /// A malformed `smb://` stops the run with the fixed line — the same
    /// shape as a bad `--dump`.
    #[test]
    fn a_malformed_smb_url_is_a_usage_error() {
        let error = load_inputs_from_args(&args(&["smb://nas"])).expect_err("no share");
        assert!(
            error.starts_with("smb: "),
            "the CLI message is the parse error: {error}"
        );
        // …and the local args before it never mask it.
        let error = load_inputs_from_args(&args(&["/music", "smb://luis:pw@nas/x"]))
            .expect_err("credentials are rejected");
        assert!(!error.contains("pw"), "no password echoed: {error}");
    }
}
