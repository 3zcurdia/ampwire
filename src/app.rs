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

/// Target frame budget (~30 fps). Spectrum and clock advance every frame.
pub const FRAME: Duration = Duration::from_millis(33);
/// Marquee scroll step cadence.
const MARQUEE: Duration = Duration::from_millis(200);
/// LCD blink cadence (only while paused).
const BLINK: Duration = Duration::from_millis(500);
/// Omarchy theme change check cadence.
const OMARCHY_INTERVAL: Duration = Duration::from_secs(1);

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
    /// Scroll offset into the `?` overlay, in rows.
    pub help_scroll: usize,
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
            help_scroll: 0,
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
    /// Frame deadline: when we next redraw (independent of the 30 fps tick).
    next_frame: Instant,
    /// When the clock/spectrum last advanced, for `tick(dt)`.
    last_tick: Instant,
    /// Per-cadence accumulators, advanced by `dt` each frame: the marquee
    /// steps every 200 ms, the LCD blinks every 500 ms, Omarchy is polled
    /// every second. Keeping them separate (and driven by the frame `dt`, not
    /// the wall clock) makes each cadence deterministic and testable.
    marquee_acc: Duration,
    blink_acc: Duration,
    omarchy_acc: Duration,
    /// Where layout hotkeys save the config. `None` = don't persist (tests).
    config_path: Option<std::path::PathBuf>,
    /// True when the on-disk config failed to load: never write back.
    read_only: bool,
}

/// Fixed EQ state for `--dump` and the golden tests (plan §2.3 shape).
pub const DETERMINISTIC_EQ_BANDS: [i8; 11] = [0, 3, 6, 2, 0, -2, -3, 0, 2, 4, 5];

impl App {
    pub fn new(config: Config, keymap: Keymap, theme: Theme, demo: bool) -> Self {
        Self::with_config_path(config, keymap, theme, demo, crate::config::config_path())
    }

    /// Deterministic app for `--dump` and golden tests: frozen player state,
    /// fixed EQ bands, no config persistence, no Omarchy polling.
    pub fn deterministic(keymap: Keymap, theme: Theme) -> Self {
        let ui = UiState {
            vis: Visibility::all(),
            playlist_cursor: 2,
            eq_bands: DETERMINISTIC_EQ_BANDS,
            ..UiState::default()
        };
        let player = MockPlayer::deterministic();
        let snapshot = player.snapshot();
        let now = Instant::now();
        Self {
            ui,
            theme,
            keymap,
            config: Config::default(),
            should_quit: false,
            snapshot,
            player,
            omarchy: OmarchyWatch::new(),
            next_frame: now,
            last_tick: now,
            marquee_acc: Duration::ZERO,
            blink_acc: Duration::ZERO,
            omarchy_acc: Duration::ZERO,
            config_path: None,
            read_only: false,
        }
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
        let player = MockPlayer::animated();
        let snapshot = player.snapshot();
        ui.playlist_cursor = ui
            .playlist_cursor
            .min(snapshot.tracks.len().saturating_sub(1));
        let now = Instant::now();
        Self {
            ui,
            theme,
            keymap,
            config,
            should_quit: false,
            snapshot,
            player,
            omarchy: OmarchyWatch::new(),
            next_frame: now,
            last_tick: now,
            marquee_acc: Duration::ZERO,
            blink_acc: Duration::ZERO,
            omarchy_acc: Duration::ZERO,
            // Demo mode never writes: layout changes stay in memory only.
            config_path: if demo { None } else { config_path },
            read_only: false,
        }
    }

    /// Mark the config read-only for this session (its load failed). Hotkeys
    /// keep working on the in-memory layout; nothing is written back.
    pub fn set_read_only(&mut self, read_only: bool, notice: Option<String>) {
        self.read_only = read_only;
        if let Some(notice) = notice {
            self.ui.notice = Some(notice);
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

    /// Run the event loop (plan §7, revised).
    ///
    /// The frame deadline is tracked separately from the tick so an idle app
    /// blocks in `poll` instead of spinning (the old code let the poll timeout
    /// fall to 0 and burned a core). Each cadence — spectrum/clock (every
    /// frame), marquee, blink, Omarchy — has its own deadline.
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        loop {
            terminal.draw(|frame| ui::render(frame, self))?;

            self.next_frame += FRAME;
            let now = Instant::now();
            if self.next_frame < now {
                // We fell behind (slow draw); resync rather than catch up in a
                // burst that would peg the CPU.
                self.next_frame = now;
            }
            if event::poll(self.next_frame - now)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        self.handle_key(key);
                    }
                    // Resize: next draw recomputes the layout from frame.area().
                    _ => {}
                }
            }

            // Advance by the real elapsed time every frame (30 fps), so the
            // clock and spectrum stay smooth.
            let after_poll = Instant::now();
            self.tick(after_poll - self.last_tick);
            self.last_tick = after_poll;

            if self.should_quit {
                return Ok(());
            }
        }
    }

    /// Feed a pressed key into the app. `Esc` closes the help overlay (the
    /// footer promises it); everything else goes through the keymap.
    pub fn handle_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        use ratatui::crossterm::event::KeyCode;
        // While the help overlay is open, Esc closes it and ↑/↓ scroll it.
        if self.ui.show_help {
            match key.code {
                KeyCode::Esc => {
                    self.ui.show_help = false;
                    return;
                }
                KeyCode::Up => {
                    self.ui.help_scroll = self.ui.help_scroll.saturating_sub(1);
                    return;
                }
                KeyCode::Down => {
                    let max = crate::ui::help::line_count(self);
                    self.ui.help_scroll = (self.ui.help_scroll + 1).min(max.saturating_sub(1));
                    return;
                }
                _ => {}
            }
        }
        if let Some(action) = self.keymap.resolve(self.ui.focus, key) {
            self.dispatch(action);
        }
    }

    /// Advance the mock clock/spectrum by `dt`, and fire the slower cadences
    /// (marquee, blink, Omarchy) when their accumulators fill up.
    pub fn tick(&mut self, dt: Duration) {
        self.player.tick(dt);
        self.snapshot = self.player.snapshot();

        // Marquee: one step every 200 ms.
        self.marquee_acc += dt;
        while self.marquee_acc >= MARQUEE {
            self.marquee_acc -= MARQUEE;
            self.ui.marquee_offset = self.ui.marquee_offset.wrapping_add(1);
        }
        // LCD blink: toggle every 500 ms (render only shows it while paused).
        self.blink_acc += dt;
        while self.blink_acc >= BLINK {
            self.blink_acc -= BLINK;
            self.ui.blink_lit = !self.ui.blink_lit;
        }
        // Omarchy: poll once a second, only when the preset asks for it.
        if self.config.theme.preset == "omarchy" {
            self.omarchy_acc += dt;
            while self.omarchy_acc >= OMARCHY_INTERVAL {
                self.omarchy_acc -= OMARCHY_INTERVAL;
                self.check_omarchy();
            }
        }
    }

    /// Poll the Omarchy theme once and adopt it if it changed. If Omarchy is
    /// absent or unreadable, fall back to neon silently (plan §3.3) — no
    /// notice, just the reason logged to the field below for debugging.
    fn check_omarchy(&mut self) {
        match self.omarchy.check(self.config.theme.transparent) {
            Ok(Some(mut theme)) => {
                Self::apply_theme_overrides(&mut theme, &self.config);
                theme.apply_depth(ui::theme::ColorDepth::detect());
                self.theme = theme;
            }
            Ok(None) => {}
            Err(_reason) => {
                // Silent fallback to neon: Omarchy is optional (e.g. macOS).
                if self.config.theme.preset != "neon"
                    && let Some(neon) = Theme::preset("neon")
                {
                    self.theme = neon;
                    Self::apply_theme_overrides(&mut self.theme, &self.config);
                    self.theme.apply_depth(ui::theme::ColorDepth::detect());
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
            // Any other key closes the overlay, then is swallowed.
            self.ui.show_help = false;
            return;
        }

        use Action::*;
        match action {
            Quit => self.should_quit = true,
            Help => {
                self.ui.show_help = !self.ui.show_help;
                self.ui.help_scroll = 0;
            }

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
                if self.config.layout.columns == 2 {
                    // Two-column: playlist right ↔ left, then swap the stack.
                    match self.config.layout.playlist_side {
                        PlaylistSide::Right => {
                            self.config.layout.playlist_side = PlaylistSide::Left;
                        }
                        PlaylistSide::Left => {
                            self.config.layout.playlist_side = PlaylistSide::Right;
                            self.config.layout.stack.reverse();
                        }
                    }
                } else {
                    // Single-column: rotate the panel order left by one.
                    let order = &mut self.config.layout.single_order;
                    if !order.is_empty() {
                        let first = order.remove(0);
                        order.push(first);
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

    /// Persist the current layout. Mirrors the live visibility into
    /// `config.layout.visible` first so what's on screen is what's saved, and
    /// refuses to write when the on-disk config failed to load.
    fn save_config(&mut self) {
        if self.read_only {
            // The file on disk didn't parse; never overwrite it.
            self.ui.notice = Some("config invalid — not saving".into());
            return;
        }
        self.sync_visible_to_config();
        if let Some(path) = self.config_path.clone()
            && let Err(err) = self.config.save(&path)
        {
            self.ui.notice = Some(format!("config save failed: {err}"));
        }
    }

    /// Copy the live visibility (which panels are shown) into the config, in
    /// canonical panel order.
    fn sync_visible_to_config(&mut self) {
        let mut visible = Vec::new();
        for panel in [Panel::Player, Panel::Eq, Panel::Playlist] {
            if self.ui.vis.is_visible(panel) {
                visible.push(panel);
            }
        }
        self.config.layout.visible = visible;
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
        // One 500 ms step: the clock advances and the blink phase flips.
        app.tick(Duration::from_millis(500));
        assert!(
            app.snapshot().position > before,
            "clock advances every tick"
        );
        assert_ne!(app.ui.blink_lit, blink, "blink flips every 500 ms");
    }

    #[test]
    fn tick_cadences_are_independent() {
        let (mut app, _dir) = test_app();
        // A short frame advances the clock but not the marquee (200 ms) or
        // blink (500 ms) yet.
        let marquee = app.ui.marquee_offset;
        let blink = app.ui.blink_lit;
        app.tick(Duration::from_millis(33));
        assert!(
            app.snapshot().position.as_millis() >= 33,
            "clock runs at 30 fps"
        );
        assert_eq!(app.ui.marquee_offset, marquee, "marquee waits for 200 ms");
        assert_eq!(app.ui.blink_lit, blink, "blink waits for 500 ms");

        // After 200 ms the marquee has stepped once.
        app.tick(Duration::from_millis(167));
        assert_eq!(
            app.ui.marquee_offset,
            marquee + 1,
            "marquee steps at 200 ms"
        );
        assert_eq!(app.ui.blink_lit, blink, "blink still waiting");
    }

    #[test]
    fn demo_mode_never_writes_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut app = App::with_config_path(
            Config::default(),
            Keymap::defaults(),
            crate::ui::theme::neon(),
            true, // demo
            Some(path.clone()),
        );
        assert!(
            app.config_path.is_none(),
            "demo mode drops the config path entirely"
        );
        app.dispatch(Action::ToggleEq);
        app.dispatch(Action::ToggleColumns);
        assert!(!path.exists(), "no config file is created in demo mode");
    }

    #[test]
    fn invalid_config_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "this is not toml [").unwrap();
        let mut app = App::with_config_path(
            Config::default(),
            Keymap::defaults(),
            crate::ui::theme::neon(),
            false,
            Some(path.clone()),
        );
        app.set_read_only(true, Some("config invalid — not saving".into()));
        app.dispatch(Action::ToggleEq);
        app.dispatch(Action::ToggleColumns);
        // In-memory layout changed…
        assert!(!app.ui.vis.eq);
        // …but the broken file is untouched.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "this is not toml ["
        );
        assert_eq!(
            app.ui.notice.as_deref(),
            Some("config invalid — not saving")
        );
    }

    #[test]
    fn visibility_persists_to_saved_config() {
        let (mut app, dir) = test_app();
        app.dispatch(Action::ToggleEq);
        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        // Isolate the `visible = [...]` line so we don't trip over "eq" in
        // the `stack` array.
        let visible_line = text
            .lines()
            .find(|l| l.trim_start().starts_with("visible"))
            .expect("visible line saved");
        assert!(
            !visible_line.contains("eq"),
            "saved visible must drop eq after pressing 2: {visible_line}"
        );
        assert!(visible_line.contains("player"), "{visible_line}");
        assert!(visible_line.contains("playlist"), "{visible_line}");
    }

    #[test]
    fn cycle_order_rotates_single_order_in_single_column() {
        let (mut app, _dir) = test_app();
        app.dispatch(Action::ToggleColumns); // → single column
        assert_eq!(app.config.layout.columns, 1);
        let before = app.config.layout.single_order.clone();
        app.dispatch(Action::CycleOrder);
        let after = app.config.layout.single_order.clone();
        assert_ne!(before, after, "single_order rotates");
        assert_eq!(after[0], before[1], "rotated left by one");
        assert_eq!(*after.last().unwrap(), before[0]);
        // Two-column mode keeps the side/stack cycle.
        app.dispatch(Action::ToggleColumns); // → two columns
        let side = app.config.layout.playlist_side;
        app.dispatch(Action::CycleOrder);
        assert_ne!(side, app.config.layout.playlist_side);
    }

    #[test]
    fn esc_closes_help_overlay() {
        let (mut app, _dir) = test_app();
        app.dispatch(Action::Help);
        assert!(app.ui.show_help);
        let esc = ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Esc,
            ratatui::crossterm::event::KeyModifiers::NONE,
        );
        app.handle_key(esc);
        assert!(!app.ui.show_help, "esc closes the overlay");
    }
}
