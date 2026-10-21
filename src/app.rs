//! App: UI state, event loop, tick, and action dispatch.

use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::action::{Action, Scope};
use crate::config::Config;
use crate::keymap::Keymap;
use crate::player::mock::MockPlayer;
use crate::player::{PlayerCommand, PlayerSnapshot};
use crate::ui;
use crate::ui::layout::{LayoutPrefs, Panel, PlaylistSide, Visibility, compute_layout};
use crate::ui::theme::omarchy::OmarchyWatch;
use crate::ui::theme::{Role, Theme};

/// Target frame budget (~30 fps).
const FRAME: Duration = Duration::from_millis(33);
/// UI tick: marquee, blink, spectrum, clock (2 Hz).
const TICK: Duration = Duration::from_millis(500);
/// Omarchy change check cadence (1 Hz).
const OMARCHY_EVERY: u32 = 2;

/// Per-frame UI state the panels read.
#[derive(Debug, Clone)]
pub struct UiState {
    pub focus: Scope,
    pub vis: Visibility,
    pub time_remaining: bool,
    /// Marquee scroll position, in display columns.
    pub marquee_offset: usize,
    /// LCD blink phase (visible when true; digits blink while paused).
    pub blink_lit: bool,
    pub playlist_cursor: usize,
    /// Focused EQ band, 0 = preamp, 1–10 = bands.
    pub eq_focus: usize,
    /// Preamp + 10 bands, in dB (-12..=12).
    pub eq_bands: [i8; 11],
    pub eq_on: bool,
    pub eq_auto: bool,
    pub show_help: bool,
    /// One-line notice for the playlist bottom border.
    pub notice: Option<String>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            focus: Scope::Player,
            vis: Visibility::all(),
            time_remaining: false,
            marquee_offset: 0,
            blink_lit: true,
            playlist_cursor: 0,
            eq_focus: 1,
            eq_bands: [0; 11],
            eq_on: true,
            eq_auto: false,
            show_help: false,
            notice: None,
        }
    }
}

pub struct App {
    pub ui: UiState,
    pub theme: Theme,
    pub keymap: Keymap,
    pub config: Config,
    pub should_quit: bool,
    snapshot: PlayerSnapshot,
    player: MockPlayer,
    omarchy: OmarchyWatch,
    ticks: u32,
    last_tick: Instant,
    /// Where layout hotkeys save the config. `None` = don't persist (tests).
    config_path: Option<std::path::PathBuf>,
}

impl App {
    pub fn new(config: Config, keymap: Keymap, theme: Theme, demo: bool) -> Self {
        Self::with_config_path(config, keymap, theme, demo, crate::config::config_path())
    }

    /// Construct with an explicit config path (tests inject a temp file).
    pub fn with_config_path(
        config: Config,
        keymap: Keymap,
        theme: Theme,
        demo: bool,
        config_path: Option<std::path::PathBuf>,
    ) -> Self {
        let mut ui = UiState {
            vis: Visibility {
                player: config.layout.visible.contains(&Panel::Player),
                eq: config.layout.visible.contains(&Panel::Eq),
                playlist: config.layout.visible.contains(&Panel::Playlist),
            },
            ..UiState::default()
        };
        if demo {
            // A pleasing demo state: cursor on the playing track.
            ui.playlist_cursor = 2;
        }
        let player = MockPlayer::default();
        let snapshot = player.snapshot();
        ui.playlist_cursor = ui
            .playlist_cursor
            .min(snapshot.tracks.len().saturating_sub(1));
        Self {
            ui,
            theme,
            keymap,
            config,
            should_quit: false,
            snapshot,
            player,
            omarchy: OmarchyWatch::new(),
            ticks: 0,
            last_tick: Instant::now(),
            config_path,
        }
    }

    pub fn snapshot(&self) -> &PlayerSnapshot {
        &self.snapshot
    }

    /// Mutable access to the mock (tests).
    #[cfg(test)]
    pub fn mock_mut(&mut self) -> &mut MockPlayer {
        &mut self.player
    }

    /// Re-snapshot after mutating the mock in tests.
    #[cfg(test)]
    pub fn resync(&mut self) {
        self.snapshot = self.player.snapshot();
    }

    /// Layout prefs from config.
    pub fn layout_prefs(&self) -> LayoutPrefs {
        LayoutPrefs {
            columns: self.config.layout.columns,
            playlist_side: self.config.layout.playlist_side,
            stack: self.config.layout.stack.clone(),
            single_order: self.config.layout.single_order.clone(),
        }
    }

    pub fn compute_layout(&self, area: ratatui::layout::Rect) -> ui::layout::ComputedLayout {
        compute_layout(area, &self.layout_prefs(), &self.ui.vis)
    }

    /// Run the event loop (plan §7).
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        loop {
            terminal.draw(|frame| ui::render(frame, self))?;

            let timeout = FRAME.saturating_sub(self.last_tick.elapsed());
            if event::poll(timeout)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if let Some(action) = self.keymap.resolve(self.ui.focus, key) {
                            self.dispatch(action);
                        }
                    }
                    // Resize: next draw recomputes the layout from frame.area().
                    _ => {}
                }
            }

            if self.last_tick.elapsed() >= TICK {
                self.tick();
                self.last_tick = Instant::now();
            }
            if self.should_quit {
                return Ok(());
            }
        }
    }

    /// Advance mock clock, spectrum, marquee, blink, and Omarchy check.
    pub fn tick(&mut self) {
        self.ticks = self.ticks.wrapping_add(1);
        self.player.tick(TICK);
        self.snapshot = self.player.snapshot();

        self.ui.marquee_offset = self.ui.marquee_offset.wrapping_add(1);
        self.ui.blink_lit = !self.ui.blink_lit;

        if self.ticks.is_multiple_of(OMARCHY_EVERY) && self.config.theme.preset == "omarchy" {
            match self.omarchy.check(self.config.theme.transparent) {
                Ok(Some(mut theme)) => {
                    Self::apply_theme_overrides(&mut theme, &self.config);
                    theme.apply_depth(ui::theme::ColorDepth::detect());
                    self.theme = theme;
                }
                Ok(None) => {}
                Err(reason) => {
                    // Silent fallback to neon, log the reason once.
                    if self.config.theme.preset != "neon"
                        && let Some(neon) = Theme::preset("neon")
                    {
                        self.theme = neon;
                        Self::apply_theme_overrides(&mut self.theme, &self.config);
                        self.theme.apply_depth(ui::theme::ColorDepth::detect());
                    }
                    self.ui.notice = Some(format!("theme: {reason}"));
                }
            }
        }
    }

    /// Apply per-role overrides from `[theme.colors]` on top of a preset.
    pub fn apply_theme_overrides(theme: &mut Theme, config: &Config) {
        for (role_name, color_text) in &config.theme.colors {
            if let Some(role) = Role::from_name(role_name)
                && let Some(color) = crate::ui::theme::parse_color(color_text)
            {
                theme.override_role(role, color);
            }
        }
    }

    pub fn dispatch(&mut self, action: Action) {
        if self.ui.show_help && action != Action::Help && action != Action::Quit {
            // Any key closes the overlay, then the key is swallowed.
            self.ui.show_help = false;
            return;
        }

        use Action::*;
        match action {
            Quit => self.should_quit = true,
            Help => self.ui.show_help = !self.ui.show_help,

            // Transport
            PlayPause | Play | Pause | Stop | Prev | Next => {
                let cmd = match action {
                    PlayPause => PlayerCommand::TogglePlayPause,
                    Play => PlayerCommand::Play,
                    Pause => PlayerCommand::Pause,
                    Stop => PlayerCommand::Stop,
                    Prev => PlayerCommand::Prev,
                    Next => PlayerCommand::Next,
                    _ => unreachable!(),
                };
                self.player.handle(cmd);
            }
            Open => {
                self.ui.notice = Some("open: not implemented in this phase".into());
            }
            ToggleShuffle => self.player.handle(PlayerCommand::ToggleShuffle),
            ToggleRepeat => self.player.handle(PlayerCommand::ToggleRepeat),

            // Player scope
            SeekBack => self.player.handle(PlayerCommand::SeekBy(-5)),
            SeekFwd => self.player.handle(PlayerCommand::SeekBy(5)),
            VolumeUp => self.change_volume(5),
            VolumeDown => self.change_volume(-5),

            // EQ scope
            BandLeft => self.ui.eq_focus = self.ui.eq_focus.saturating_sub(1),
            BandRight => self.ui.eq_focus = (self.ui.eq_focus + 1).min(10),
            BandUp => self.change_band(1),
            BandDown => self.change_band(-1),

            // Playlist scope
            CursorUp => {
                self.ui.playlist_cursor = self.ui.playlist_cursor.saturating_sub(1);
            }
            CursorDown => {
                let max = self.snapshot.tracks.len().saturating_sub(1);
                self.ui.playlist_cursor = (self.ui.playlist_cursor + 1).min(max);
            }
            Select => {
                if !self.snapshot.tracks.is_empty() {
                    self.player
                        .handle(PlayerCommand::PlayTrack(self.ui.playlist_cursor));
                }
            }
            PageUp => self.ui.playlist_cursor = self.ui.playlist_cursor.saturating_sub(10),
            PageDown => {
                let max = self.snapshot.tracks.len().saturating_sub(1);
                self.ui.playlist_cursor = (self.ui.playlist_cursor + 10).min(max);
            }
            JumpHome => self.ui.playlist_cursor = 0,
            JumpEnd => {
                self.ui.playlist_cursor = self.snapshot.tracks.len().saturating_sub(1);
            }
            Add | Del | Find => {
                self.ui.notice = Some(format!("{}: not implemented in this phase", action.name()));
            }
            // Layout (saved)
            TogglePlayer => self.toggle_panel(Panel::Player),
            ToggleEq => self.toggle_panel(Panel::Eq),
            TogglePlaylist => self.toggle_panel(Panel::Playlist),
            ToggleColumns => {
                self.config.layout.columns = if self.config.layout.columns == 2 {
                    1
                } else {
                    2
                };
                self.save_config();
            }
            CycleOrder => {
                // Fixed presets (plan §11.2): playlist right ↔ left, and the
                // player/EQ stack swaps.
                match self.config.layout.playlist_side {
                    PlaylistSide::Right => {
                        self.config.layout.playlist_side = PlaylistSide::Left;
                    }
                    PlaylistSide::Left => {
                        self.config.layout.playlist_side = PlaylistSide::Right;
                        self.config.layout.stack.reverse();
                    }
                }
                self.save_config();
            }

            // Global misc
            ToggleTimeMode => self.ui.time_remaining = !self.ui.time_remaining,
            FocusNext => self.cycle_focus(1),
            FocusPrev => self.cycle_focus(-1),
        }

        self.snapshot = self.player.snapshot();
    }

    fn change_volume(&mut self, delta: i32) {
        let volume = self.snapshot.volume as i32 + delta;
        self.player
            .handle(PlayerCommand::SetVolume(volume.clamp(0, 100) as u8));
    }

    fn change_band(&mut self, delta: i8) {
        let band = &mut self.ui.eq_bands[self.ui.eq_focus];
        *band = (*band + delta).clamp(-12, 12);
    }

    fn toggle_panel(&mut self, panel: Panel) {
        let visible = !self.ui.vis.is_visible(panel);
        // Never hide everything: if this was the last one, keep it.
        let any_other = [Panel::Player, Panel::Eq, Panel::Playlist]
            .iter()
            .any(|p| *p != panel && self.ui.vis.is_visible(*p));
        if !visible && !any_other {
            return;
        }
        self.ui.vis.set_visible(panel, visible);
        if !self.ui.vis.is_visible(match self.ui.focus {
            Scope::Player => Panel::Player,
            Scope::Eq => Panel::Eq,
            _ => Panel::Playlist,
        }) {
            self.cycle_focus(1);
        }
        self.save_config();
    }

    fn cycle_focus(&mut self, dir: i32) {
        let order = [Scope::Player, Scope::Eq, Scope::Playlist];
        let visible_scopes: Vec<Scope> = order
            .iter()
            .copied()
            .filter(|s| self.ui.vis.is_visible(panel_for(*s)))
            .collect();
        if visible_scopes.is_empty() {
            return;
        }
        let idx = visible_scopes
            .iter()
            .position(|s| *s == self.ui.focus)
            .unwrap_or(0);
        let len = visible_scopes.len() as i32;
        let next = (idx as i32 + dir).rem_euclid(len) as usize;
        self.ui.focus = visible_scopes[next];
    }

    fn save_config(&mut self) {
        if let Some(path) = self.config_path.clone()
            && let Err(err) = self.config.save(&path)
        {
            self.ui.notice = Some(format!("config save failed: {err}"));
        }
    }
}

fn panel_for(scope: Scope) -> Panel {
    match scope {
        Scope::Player => Panel::Player,
        Scope::Eq => Panel::Eq,
        _ => Panel::Playlist,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;

    /// App wired to a temp config file so saves never touch the real one.
    fn test_app() -> (App, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let app = App::with_config_path(
            Config::default(),
            Keymap::defaults(),
            crate::ui::theme::neon(),
            false,
            Some(path),
        );
        (app, dir)
    }

    #[test]
    fn toggle_columns_switches_and_saves() {
        let (mut app, dir) = test_app();
        assert_eq!(app.config.layout.columns, 2);
        app.dispatch(Action::ToggleColumns);
        assert_eq!(app.config.layout.columns, 1);
        app.dispatch(Action::ToggleColumns);
        assert_eq!(app.config.layout.columns, 2);

        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(text.contains("columns = 2"), "{text}");
        assert!(text.contains("playlist_side"), "{text}");
    }

    #[test]
    fn cycle_order_cycles_presets_and_saves() {
        let (mut app, dir) = test_app();
        assert_eq!(app.config.layout.playlist_side, PlaylistSide::Right);
        app.dispatch(Action::CycleOrder);
        assert_eq!(app.config.layout.playlist_side, PlaylistSide::Left);
        app.dispatch(Action::CycleOrder);
        assert_eq!(app.config.layout.playlist_side, PlaylistSide::Right);
        // Second lap swaps the stack order.
        assert_ne!(app.config.layout.stack, Config::default().layout.stack);

        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(text.contains("stack"), "{text}");
    }

    #[test]
    fn visibility_toggles_save_and_never_hide_everything() {
        let (mut app, dir) = test_app();
        app.dispatch(Action::ToggleEq);
        assert!(!app.ui.vis.eq);
        app.dispatch(Action::TogglePlayer);
        assert!(!app.ui.vis.player);
        app.dispatch(Action::TogglePlaylist);
        // Last panel refuses to hide.
        assert!(app.ui.vis.playlist);

        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(text.contains("visible"), "{text}");
    }

    #[test]
    fn focus_cycles_over_visible_panels() {
        let (mut app, _dir) = test_app();
        assert_eq!(app.ui.focus, Scope::Player);
        app.dispatch(Action::FocusNext);
        assert_eq!(app.ui.focus, Scope::Eq);
        app.dispatch(Action::FocusNext);
        assert_eq!(app.ui.focus, Scope::Playlist);
        app.dispatch(Action::FocusNext);
        assert_eq!(app.ui.focus, Scope::Player);
        app.dispatch(Action::FocusPrev);
        assert_eq!(app.ui.focus, Scope::Playlist);
    }

    #[test]
    fn quit_action_stops_the_app() {
        let (mut app, _dir) = test_app();
        app.dispatch(Action::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn help_toggles_and_swallows_other_keys() {
        let (mut app, _dir) = test_app();
        app.dispatch(Action::Help);
        assert!(app.ui.show_help);
        // A normal key closes the overlay and is swallowed.
        let volume = app.snapshot().volume;
        app.dispatch(Action::VolumeUp);
        assert!(!app.ui.show_help);
        assert_eq!(app.snapshot().volume, volume, "key was swallowed");
        // Pressing ? again reopens.
        app.dispatch(Action::Help);
        assert!(app.ui.show_help);
    }

    #[test]
    fn transport_and_volume_reach_the_mock() {
        let (mut app, _dir) = test_app();
        app.dispatch(Action::Pause);
        assert_eq!(app.snapshot().state, crate::player::PlayState::Paused);
        app.dispatch(Action::PlayPause);
        assert_eq!(app.snapshot().state, crate::player::PlayState::Playing);
        let before = app.snapshot().volume;
        app.dispatch(Action::VolumeUp);
        assert_eq!(app.snapshot().volume, (before + 5).min(100));
        app.dispatch(Action::Next);
        assert_eq!(app.snapshot().current, Some(3));
        app.dispatch(Action::ToggleShuffle);
        assert!(app.snapshot().shuffle);
    }

    #[test]
    fn playlist_navigation_clamps() {
        let (mut app, _dir) = test_app();
        app.dispatch(Action::JumpEnd);
        let last = app.snapshot().tracks.len() - 1;
        assert_eq!(app.ui.playlist_cursor, last);
        app.dispatch(Action::CursorDown);
        assert_eq!(app.ui.playlist_cursor, last, "clamped at the end");
        app.dispatch(Action::JumpHome);
        assert_eq!(app.ui.playlist_cursor, 0);
        app.dispatch(Action::CursorUp);
        assert_eq!(app.ui.playlist_cursor, 0, "clamped at the top");
    }

    #[test]
    fn tick_advances_clock_and_blink() {
        let (mut app, _dir) = test_app();
        let before = app.snapshot().position;
        let blink = app.ui.blink_lit;
        app.tick();
        assert!(app.snapshot().position >= before);
        assert_ne!(app.ui.blink_lit, blink, "blink phase flips each tick");
    }
}
