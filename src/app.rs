//! App: UI state, event loop, tick, and action dispatch.

use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::action::{Action, Scope};
use crate::config::Config;
use crate::keymap::Keymap;
use crate::library::ScanMsg;
use crate::player::mock::MockPlayer;
use crate::player::real::RealPlayer;
use crate::player::{PlayerCommand, PlayerSnapshot, Track};
use crate::ui;
use crate::ui::layout::{LayoutPrefs, Panel, PlaylistSide, Visibility, compute_layout};
use crate::ui::theme::omarchy::OmarchyWatch;
use crate::ui::theme::{Role, Theme};
use ratatui::layout::Rect;

/// Target frame budget (~30 fps). Spectrum and clock advance every frame.
pub const FRAME: Duration = Duration::from_millis(33);
/// Marquee scroll step cadence.
const MARQUEE: Duration = Duration::from_millis(200);
/// LCD blink cadence (only while paused).
const BLINK: Duration = Duration::from_millis(500);
/// Omarchy theme change check cadence.
const OMARCHY_INTERVAL: Duration = Duration::from_secs(1);
/// Save the config this long after the last volume/balance change (the
/// slider emits one command per keypress; writing TOML each time is waste).
const CONFIG_SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// What a text prompt was opened for (plan 02, step 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Open,
    Add,
    Find,
}

/// An open one-line text prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub kind: PromptKind,
    pub text: String,
}

impl Prompt {
    pub fn new(kind: PromptKind) -> Self {
        Self {
            kind,
            text: String::new(),
        }
    }

    /// Label shown in the playlist bottom border while typing.
    pub fn label(&self) -> &'static str {
        match self.kind {
            PromptKind::Open => "open",
            PromptKind::Add => "add",
            PromptKind::Find => "find",
        }
    }
}

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
    /// First visible playlist row. Moves only when the cursor would leave
    /// the window, so ↑ near the bottom moves the cursor, not the list.
    pub playlist_scroll: usize,
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
    /// Open text prompt (`l` / `a` / `/`), if any.
    pub prompt: Option<Prompt>,
    /// Active playlist filter (from the Find prompt).
    pub filter: Option<String>,
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
            playlist_scroll: 0,
            eq_focus: 1,
            eq_bands: [0; 11],
            eq_on: true,
            eq_auto: false,
            show_help: false,
            help_scroll: 0,
            notice: None,
            prompt: None,
            filter: None,
        }
    }
}

/// Which scan to run over the playlist when the files arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanMode {
    /// Replace the list, then play track 1 (`l`, CLI args).
    Replace,
    /// Append to the list without starting playback (`a`).
    Append,
}

/// The player driving the UI: the mock (demo, `--dump`, every test) or the
/// real engine (a real, non-demo session). Both speak the same
/// [`PlayerCommand`] / [`PlayerSnapshot`] contract, so `ui` never notices.
pub enum Backend {
    Mock(MockPlayer),
    Real(RealPlayer),
}

impl Backend {
    fn handle(&mut self, cmd: PlayerCommand) {
        match self {
            Backend::Mock(player) => player.handle(cmd),
            Backend::Real(player) => player.handle(cmd),
        }
    }

    fn tick(&mut self, dt: Duration) {
        match self {
            Backend::Mock(player) => player.tick(dt),
            Backend::Real(player) => player.tick(dt),
        }
    }

    fn snapshot(&self) -> PlayerSnapshot {
        match self {
            Backend::Mock(player) => player.snapshot(),
            Backend::Real(player) => player.snapshot(),
        }
    }

    /// A queued one-line notice (`skipped …`, `shuffle on`, …). The mock has
    /// none; the real player surfaces engine errors and toggle feedback.
    fn take_notice(&mut self) -> Option<String> {
        match self {
            Backend::Mock(_) => None,
            Backend::Real(player) => player.take_notice(),
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
    player: Backend,
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
    /// When the config last changed in a way worth persisting (volume,
    /// balance; plan 06 adds the EQ). `tick` saves once it has aged
    /// [`CONFIG_SAVE_DEBOUNCE`], and quitting flushes it immediately.
    config_dirty_since: Option<Instant>,
    /// Terminal area of the frame about to be drawn; `None` until the first
    /// frame (tests, `--dump`). Focus uses it to skip auto-dropped panels.
    last_area: Option<Rect>,
    /// Omarchy is unavailable and the neon fallback is already applied.
    omarchy_fallback: bool,
    /// A background directory scan is in flight (plan 02, step 8).
    scan_rx: Option<std::sync::mpsc::Receiver<ScanMsg>>,
    /// What to do with the tracks when the running scan finishes.
    scan_mode: ScanMode,
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
        let player = Backend::Mock(MockPlayer::deterministic());
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
            config_dirty_since: None,
            last_area: None,
            omarchy_fallback: false,
            scan_rx: None,
            scan_mode: ScanMode::Replace,
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
        // The low-level constructor always builds the mock: `--demo`, every
        // test and the golden frames depend on the 24-track fake playlist.
        // Real sessions come from `new_session` below (plan 02's precedent,
        // re-confirmed for plan 03).
        let player = Backend::Mock(MockPlayer::animated());
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
            config_dirty_since: None,
            last_area: None,
            omarchy_fallback: false,
            scan_rx: None,
            scan_mode: ScanMode::Replace,
        }
    }

    /// A real (non-demo) session: the playlist starts empty; tracks arrive
    /// from CLI paths or the `l`/`a` prompts (plan 02).
    pub fn new_session(config: Config, keymap: Keymap, theme: Theme) -> Self {
        let mut app =
            Self::with_config_path(config, keymap, theme, false, crate::config::config_path());
        // The real session starts empty and plays for real: this backend owns
        // the engine thread (dropped → Shutdown + join, so `q` exits fast).
        app.player = Backend::Real(RealPlayer::new());
        // Restore the remembered audio settings before anything plays.
        let audio = app.config.audio;
        app.player.handle(PlayerCommand::SetVolume(audio.volume));
        app.player.handle(PlayerCommand::SetBalance(audio.balance));
        app.player
            .handle(PlayerCommand::SetSwapChannels(audio.swap_channels));
        app.snapshot = app.player.snapshot();
        app.ui.playlist_cursor = 0;
        app
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

    /// Mutable access to the mock (tests). Panics on the real backend — the
    /// production session is the only one that uses it, and no test does.
    #[cfg(test)]
    pub fn mock_mut(&mut self) -> &mut MockPlayer {
        match &mut self.player {
            Backend::Mock(player) => player,
            Backend::Real(_) => panic!("mock_mut() on the real backend"),
        }
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
            let size = terminal.size()?;
            self.sync_to_area(Rect::new(0, 0, size.width, size.height));
            terminal.draw(|frame| ui::render(frame, self))?;

            let now = Instant::now();
            self.next_frame = advance_deadline(self.next_frame, now);
            if event::poll(self.next_frame.saturating_duration_since(now))? {
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
                self.flush_config(); // a prompt's Ctrl+C lands here, not in dispatch
                return Ok(());
            }
        }
    }

    /// Bring focus and playlist scroll in line with what the layout draws at
    /// `area`. Called just before each frame is drawn.
    pub fn sync_to_area(&mut self, area: Rect) {
        self.last_area = Some(area);
        // A fallback (short terminal) may have dropped the focused panel.
        if !self.rendered_panels().is_visible(panel_for(self.ui.focus)) {
            self.cycle_focus(1);
        }
        if let Some(rect) = self.compute_layout(area).playlist {
            let rows = rect.height.saturating_sub(2) as usize;
            self.ui.playlist_scroll = ui::playlist::scroll_for(
                self.ui.playlist_scroll,
                self.ui.playlist_cursor,
                rows,
                self.visible_indices().len(),
            );
        }
    }

    /// Real track indices currently shown, in order: everything when no
    /// filter is active, else the case-insensitive `display_name` matches.
    /// The playlist cursor addresses *this* list, not the raw tracks.
    pub fn visible_indices(&self) -> Vec<usize> {
        let Some(filter) = self.ui.filter.as_deref() else {
            return (0..self.snapshot.tracks.len()).collect();
        };
        let needle = filter.to_lowercase();
        self.snapshot
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.display_name().to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect()
    }

    /// Keep the cursor inside the visible list after it changes shape.
    fn clamp_cursor(&mut self) {
        let max = self.visible_indices().len().saturating_sub(1);
        self.ui.playlist_cursor = self.ui.playlist_cursor.min(max);
    }

    /// Panels the layout actually draws: the visibility toggles minus any
    /// panel an automatic fallback dropped at the last known size.
    fn rendered_panels(&self) -> Visibility {
        let Some(area) = self.last_area else {
            return self.ui.vis;
        };
        let layout = self.compute_layout(area);
        Visibility {
            player: layout.player.is_some(),
            eq: layout.eq.is_some(),
            playlist: layout.playlist.is_some(),
        }
    }

    /// Feed a pressed key into the app. The help overlay and an open text
    /// prompt take keys before the keymap; everything else goes through it.
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
        // A text prompt swallows every key except its own controls.
        if self.ui.prompt.is_some() {
            self.handle_prompt_key(key);
            return;
        }
        if let Some(action) = self.keymap.resolve(self.ui.focus, key) {
            self.dispatch(action);
        }
    }

    /// Keys while a prompt is open: Enter submits, Esc closes (Find also
    /// clears its filter), Backspace pops, plain chars type. Ctrl+C still
    /// quits; everything else is swallowed.
    fn handle_prompt_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        let Some(prompt) = self.ui.prompt.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Enter => {
                let prompt = self.ui.prompt.take().expect("prompt is open");
                self.submit_prompt(prompt);
            }
            KeyCode::Esc => {
                let kind = prompt.kind;
                self.ui.prompt = None;
                if kind == PromptKind::Find {
                    // Closing Find discards the filter.
                    self.ui.filter = None;
                    self.ui.playlist_cursor = 0;
                }
            }
            KeyCode::Backspace => {
                prompt.text.pop();
                if prompt.kind == PromptKind::Find {
                    self.live_filter();
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                prompt.text.push(c);
                if prompt.kind == PromptKind::Find {
                    self.live_filter();
                }
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            _ => {}
        }
    }

    /// Find filters live: every keystroke re-narrows the list and rewinds
    /// the cursor.
    fn live_filter(&mut self) {
        let filter = self
            .ui
            .prompt
            .as_ref()
            .map(|p| p.text.clone())
            .unwrap_or_default();
        self.ui.filter = Some(filter);
        self.ui.playlist_cursor = 0;
        self.ui.playlist_scroll = 0;
    }

    /// A submitted `l` / `a` prompt: expand `~`, check the path exists,
    /// then kick off the scan (replace or append).
    fn submit_prompt(&mut self, prompt: Prompt) {
        match prompt.kind {
            // Enter in Find keeps the filter and just closed the prompt.
            PromptKind::Find => {}
            PromptKind::Open | PromptKind::Add => {
                let path = expand_tilde(&prompt.text);
                if !path.exists() {
                    self.ui.notice = Some(format!("not found: {}", path.display()));
                    return;
                }
                let mode = if prompt.kind == PromptKind::Open {
                    ScanMode::Replace
                } else {
                    ScanMode::Append
                };
                self.load_paths(vec![path], mode);
            }
        }
    }

    /// Advance the mock clock/spectrum by `dt`, and fire the slower cadences
    /// (marquee, blink, Omarchy) when their accumulators fill up.
    pub fn tick(&mut self, dt: Duration) {
        self.player.tick(dt);
        self.snapshot = self.player.snapshot();
        // Surface whatever the real player queued this tick (engine errors,
        // shuffle/repeat feedback) in the playlist's notice slot.
        if let Some(notice) = self.player.take_notice() {
            self.ui.notice = Some(notice);
        }
        self.poll_scan();

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
        // Debounced config save (volume/balance; plan 06 reuses it for EQ).
        if self
            .config_dirty_since
            .is_some_and(|since| since.elapsed() >= CONFIG_SAVE_DEBOUNCE)
        {
            self.config_dirty_since = None;
            self.save_config();
        }
    }

    /// Kick off a background scan of `paths` (plan 02, step 8). Refuses when
    /// one is already running; the results land via [`App::poll_scan`].
    pub fn load_paths(&mut self, paths: Vec<std::path::PathBuf>, mode: ScanMode) {
        if paths.is_empty() {
            return;
        }
        if self.scan_rx.is_some() {
            self.ui.notice = Some("already loading".into());
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        crate::library::spawn_scan(paths, tx);
        self.scan_rx = Some(rx);
        self.scan_mode = mode;
    }

    /// Drain pending scan messages. `Done` applies the tracks according to
    /// the scan mode; an empty result leaves the playlist alone.
    fn poll_scan(&mut self) {
        let Some(rx) = self.scan_rx.take() else {
            return;
        };
        loop {
            match rx.try_recv() {
                Ok(ScanMsg::Progress { found }) => {
                    self.ui.notice = Some(format!("loading… {found} files"));
                }
                Ok(ScanMsg::Done { tracks, skipped }) => {
                    self.finish_scan(tracks, skipped);
                    return; // scan complete: rx stays dropped
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            }
        }
        self.scan_rx = Some(rx);
    }

    fn finish_scan(&mut self, tracks: Vec<Track>, skipped: usize) {
        if tracks.is_empty() {
            // Nothing found: leave whatever was in the playlist alone.
            self.ui.notice = Some("no audio files found".into());
        } else {
            let loaded = tracks.len();
            match self.scan_mode {
                ScanMode::Replace => {
                    self.player.handle(PlayerCommand::ReplaceTracks(tracks));
                    self.ui.playlist_cursor = 0;
                    self.ui.playlist_scroll = 0;
                }
                ScanMode::Append => {
                    self.player.handle(PlayerCommand::AddTracks(tracks));
                }
            }
            self.ui.notice = Some(format!("loaded {loaded} tracks ({skipped} skipped)"));
        }
        self.snapshot = self.player.snapshot();
    }

    /// Poll the Omarchy theme once and adopt it if it changed. If Omarchy is
    /// absent or unreadable, fall back to neon silently (plan §3.3), once —
    /// not on every poll while it stays missing.
    fn check_omarchy(&mut self) {
        match self.omarchy.check(self.config.theme.transparent) {
            Ok(Some(mut theme)) => {
                Self::apply_theme_overrides(&mut theme, &self.config);
                theme.apply_depth(ui::theme::ColorDepth::detect());
                self.theme = theme;
                self.omarchy_fallback = false;
            }
            Ok(None) => {}
            Err(_reason) => {
                // Silent fallback to neon: Omarchy is optional (e.g. macOS).
                if !self.omarchy_fallback
                    && let Some(neon) = Theme::preset("neon")
                {
                    self.theme = neon;
                    Self::apply_theme_overrides(&mut self.theme, &self.config);
                    self.theme.apply_depth(ui::theme::ColorDepth::detect());
                    self.omarchy_fallback = true;
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
            Quit => {
                self.should_quit = true;
                self.flush_config();
            }
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
            ToggleShuffle => self.player.handle(PlayerCommand::ToggleShuffle),
            ToggleRepeat => self.player.handle(PlayerCommand::ToggleRepeat),

            // Player scope
            SeekBack => self.player.handle(PlayerCommand::SeekBy(-5)),
            SeekFwd => self.player.handle(PlayerCommand::SeekBy(5)),
            VolumeUp => self.change_volume(5),
            VolumeDown => self.change_volume(-5),
            BalanceLeft => self.change_balance(-5),
            BalanceRight => self.change_balance(5),

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
                let max = self.visible_indices().len().saturating_sub(1);
                self.ui.playlist_cursor = (self.ui.playlist_cursor + 1).min(max);
            }
            Select => {
                // The cursor is a position in the *visible* (filtered) list.
                if let Some(&track) = self.visible_indices().get(self.ui.playlist_cursor) {
                    self.player.handle(PlayerCommand::PlayTrack(track));
                }
            }
            PageUp => {
                self.ui.playlist_cursor = self.ui.playlist_cursor.saturating_sub(10);
            }
            PageDown => {
                let max = self.visible_indices().len().saturating_sub(1);
                self.ui.playlist_cursor = (self.ui.playlist_cursor + 10).min(max);
            }
            JumpHome => self.ui.playlist_cursor = 0,
            JumpEnd => {
                self.ui.playlist_cursor = self.visible_indices().len().saturating_sub(1);
            }
            Open => self.ui.prompt = Some(Prompt::new(PromptKind::Open)),
            Add => self.ui.prompt = Some(Prompt::new(PromptKind::Add)),
            Find => {
                // Reopen with the active filter pre-filled.
                let mut prompt = Prompt::new(PromptKind::Find);
                prompt.text = self.ui.filter.clone().unwrap_or_default();
                self.ui.prompt = Some(prompt);
            }
            Del => {
                if let Some(&track) = self.visible_indices().get(self.ui.playlist_cursor) {
                    self.player.handle(PlayerCommand::RemoveTrack(track));
                }
                self.clamp_cursor();
            }
            Clear => {
                self.player.handle(PlayerCommand::ClearPlaylist);
                self.ui.playlist_cursor = 0;
                self.ui.playlist_scroll = 0;
                self.ui.filter = None;
            }
            MoveUp | MoveDown => {
                let up = matches!(action, MoveUp);
                if self.ui.filter.is_some() {
                    self.ui.notice = Some("clear the filter to reorder".into());
                } else {
                    // Playlist::move_* no-ops at the edge; the cursor's own
                    // saturating/clamped arithmetic keeps it put there.
                    let from = self.ui.playlist_cursor;
                    self.player.handle(PlayerCommand::MoveTrack { from, up });
                    if up {
                        self.ui.playlist_cursor = from.saturating_sub(1);
                    } else {
                        let max = self.snapshot.tracks.len().saturating_sub(1);
                        self.ui.playlist_cursor = (from + 1).min(max);
                    }
                }
            }
            ClearFilter => {
                self.ui.filter = None;
                self.ui.playlist_cursor = 0;
                self.ui.playlist_scroll = 0;
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

        // Every dispatch ends with a fresh snapshot and a cursor that still
        // points inside the visible list ("always clamp after the change").
        self.snapshot = self.player.snapshot();
        self.clamp_cursor();
    }

    fn change_volume(&mut self, delta: i32) {
        let volume = (self.snapshot.volume as i32 + delta).clamp(0, 100) as u8;
        self.player.handle(PlayerCommand::SetVolume(volume));
        self.config.audio.volume = volume;
        self.config_dirty_since = Some(Instant::now());
    }

    fn change_balance(&mut self, delta: i32) {
        let balance = (self.snapshot.balance as i32 + delta).clamp(-100, 100) as i8;
        self.player.handle(PlayerCommand::SetBalance(balance));
        self.config.audio.balance = balance;
        self.config_dirty_since = Some(Instant::now());
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
        if !self.rendered_panels().is_visible(panel_for(self.ui.focus)) {
            self.cycle_focus(1);
        }
        self.save_config();
    }

    /// Move focus `dir` steps through player → eq → playlist, skipping
    /// panels that aren't drawn. Starting from a hidden panel picks its
    /// neighbour in that direction.
    fn cycle_focus(&mut self, dir: i32) {
        let order = [Scope::Player, Scope::Eq, Scope::Playlist];
        let rendered = self.rendered_panels();
        let start = order.iter().position(|s| *s == self.ui.focus).unwrap_or(0) as i32;
        for step in 1..=order.len() as i32 {
            let scope = order[(start + dir * step).rem_euclid(order.len() as i32) as usize];
            if rendered.is_visible(panel_for(scope)) {
                self.ui.focus = scope;
                return;
            }
        }
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

    /// Save immediately if a debounced save is pending (on quit).
    fn flush_config(&mut self) {
        if self.config_dirty_since.take().is_some() {
            self.save_config();
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

/// When the next periodic frame is due. Only a deadline that has passed
/// moves forward, so redraws woken early by input don't push the animation
/// clock into the future (holding a key used to freeze the spectrum). A draw
/// more than a frame late resyncs instead of catching up in a burst.
fn advance_deadline(deadline: Instant, now: Instant) -> Instant {
    if now < deadline {
        return deadline;
    }
    let next = deadline + FRAME;
    if next < now { now + FRAME } else { next }
}

fn panel_for(scope: Scope) -> Panel {
    match scope {
        Scope::Player => Panel::Player,
        Scope::Eq => Panel::Eq,
        _ => Panel::Playlist,
    }
}

/// Expand a leading `~` (bare or `~/…`) with `$HOME`. Anything else —
/// including `~user` — is left for the filesystem to reject.
fn expand_tilde(text: &str) -> std::path::PathBuf {
    let Some(rest) = text.strip_prefix('~') else {
        return std::path::PathBuf::from(text);
    };
    if !(rest.is_empty() || rest.starts_with('/')) {
        return std::path::PathBuf::from(text);
    }
    match std::env::var_os("HOME") {
        Some(home) => std::path::PathBuf::from(home).join(rest.trim_start_matches('/')),
        None => std::path::PathBuf::from(text),
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
    fn early_wakeups_do_not_push_the_deadline() {
        let start = Instant::now();
        // Woken by input before the deadline: it stays put, however often.
        let mut deadline = start + FRAME;
        for ms in [1, 2, 3, 10, 20] {
            deadline = advance_deadline(deadline, start + Duration::from_millis(ms));
        }
        assert_eq!(deadline, start + FRAME);
        // Deadline reached: the next frame is one frame later.
        assert_eq!(advance_deadline(deadline, deadline), deadline + FRAME);
        // Far behind (slow draw): resync to one frame from now.
        let late = deadline + FRAME * 5;
        assert_eq!(advance_deadline(deadline, late), late + FRAME);
    }

    #[test]
    fn focus_skips_panels_dropped_by_a_fallback() {
        let (mut app, _dir) = test_app();
        // 104×15: two columns, but too short for the EQ under the player.
        app.ui.focus = Scope::Eq;
        app.sync_to_area(Rect::new(0, 0, 104, 15));
        assert_eq!(app.ui.focus, Scope::Playlist, "focus left the dropped EQ");
        app.dispatch(Action::FocusNext);
        assert_eq!(app.ui.focus, Scope::Player);
        app.dispatch(Action::FocusNext);
        assert_eq!(app.ui.focus, Scope::Playlist, "EQ skipped");
    }

    #[test]
    fn hiding_the_focused_panel_moves_focus_to_its_neighbour() {
        let (mut app, _dir) = test_app();
        assert_eq!(app.ui.focus, Scope::Player);
        app.dispatch(Action::TogglePlayer);
        assert_eq!(app.ui.focus, Scope::Eq);
    }

    #[test]
    fn playlist_scroll_moves_only_at_the_window_edge() {
        let (mut app, _dir) = test_app();
        // 104×18: the playlist shows 16 rows of 24 tracks.
        let area = Rect::new(0, 0, 104, 18);
        app.dispatch(Action::JumpEnd);
        app.sync_to_area(area);
        assert_eq!(app.ui.playlist_scroll, 8);
        // ↑ moves the cursor inside the window; the list stays put.
        app.dispatch(Action::CursorUp);
        app.sync_to_area(area);
        assert_eq!(app.ui.playlist_scroll, 8);
        // Past the top edge the list follows.
        app.dispatch(Action::JumpHome);
        app.sync_to_area(area);
        assert_eq!(app.ui.playlist_scroll, 0);
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

    // --- Plan 02: filter, prompts, playlist controls ---

    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn find_filter_narrows_visible_indices() {
        let (mut app, _dir) = test_app();
        assert_eq!(app.visible_indices().len(), 24, "no filter → everything");

        app.dispatch(Action::Find);
        assert_eq!(
            app.ui.prompt.as_ref().map(|p| p.kind),
            Some(crate::app::PromptKind::Find)
        );
        app.handle_key(press(KeyCode::Char('a')));
        app.handle_key(press(KeyCode::Char('p')));
        app.handle_key(press(KeyCode::Char('h')));
        app.handle_key(press(KeyCode::Char('e')));
        app.handle_key(press(KeyCode::Char('x')));
        // Live filter: matches update on every keystroke, cursor rewinds.
        assert_eq!(app.ui.filter.as_deref(), Some("aphex"));
        let visible = app.visible_indices();
        assert_eq!(visible, vec![0, 17], "Aphex Twin ×2: {visible:?}");
        assert_eq!(app.ui.playlist_cursor, 0);

        // Enter keeps the filter and closes the prompt.
        app.handle_key(press(KeyCode::Enter));
        assert!(app.ui.prompt.is_none());
        assert_eq!(app.ui.filter.as_deref(), Some("aphex"));

        // Esc (with no prompt) clears it via ClearFilter.
        app.ui.focus = crate::action::Scope::Playlist;
        app.handle_key(press(KeyCode::Esc));
        assert_eq!(app.ui.filter, None);
        assert_eq!(app.visible_indices().len(), 24);
    }

    #[test]
    fn select_on_a_filtered_row_plays_the_real_track() {
        let (mut app, _dir) = test_app();
        app.ui.filter = Some("boards".into());
        // "boards of canada": Roygbiv (1) and Dayvan Cowboy (16).
        assert_eq!(app.visible_indices(), vec![1, 16]);
        app.ui.playlist_cursor = 1;
        app.dispatch(Action::Select);
        assert_eq!(app.snapshot().current, Some(16), "second visible row");
        assert_eq!(
            app.snapshot().current_track().unwrap().display_name(),
            "Boards of Canada - Dayvan Cowboy"
        );
    }

    #[test]
    fn del_with_a_filter_removes_the_right_track() {
        let (mut app, _dir) = test_app();
        app.ui.filter = Some("boards".into());
        app.ui.playlist_cursor = 1; // visible 1 = real 16 (Dayvan Cowboy)
        app.dispatch(Action::Del);
        let names: Vec<String> = app
            .snapshot()
            .tracks
            .iter()
            .map(|t| t.display_name())
            .collect();
        assert!(
            !names.iter().any(|n| n.contains("Dayvan Cowboy")),
            "removed the filtered row's real track"
        );
        assert!(
            names.iter().any(|n| n.contains("Roygbiv")),
            "the other match survives"
        );
        assert_eq!(app.snapshot().tracks.len(), 23);
        // Cursor clamped into the shrunken visible list.
        assert!(app.ui.playlist_cursor < app.visible_indices().len());
    }

    #[test]
    fn prompt_keys_type_backspace_and_escape() {
        let (mut app, _dir) = test_app();
        app.dispatch(Action::Open);
        assert_eq!(app.ui.prompt.as_ref().map(|p| p.label()), Some("open"));

        for c in "music".chars() {
            app.handle_key(press(KeyCode::Char(c)));
        }
        assert_eq!(app.ui.prompt.as_ref().unwrap().text, "music");

        app.handle_key(press(KeyCode::Backspace));
        assert_eq!(app.ui.prompt.as_ref().unwrap().text, "musi");

        // Prompt swallows keys the keymap would otherwise act on.
        app.handle_key(press(KeyCode::Char(' ')));
        assert!(!app.should_quit, "space didn't reach the keymap");
        app.handle_key(press(KeyCode::Char('q')));
        assert!(!app.should_quit, "q didn't reach the keymap");

        // Esc closes without touching the playlist.
        app.handle_key(press(KeyCode::Esc));
        assert!(app.ui.prompt.is_none());
        assert_eq!(app.snapshot().tracks.len(), 24);
    }

    #[test]
    fn enter_on_a_missing_path_notices_and_scans_nothing() {
        let (mut app, _dir) = test_app();
        app.dispatch(Action::Add);
        for c in "no/nope".chars() {
            app.handle_key(press(KeyCode::Char(c)));
        }
        app.handle_key(press(KeyCode::Enter));
        assert!(app.ui.prompt.is_none());
        assert_eq!(
            app.ui.notice.as_deref(),
            Some("not found: no/nope"),
            "notice reports the path that failed"
        );
        assert_eq!(app.snapshot().tracks.len(), 24, "playlist untouched");
    }

    #[test]
    fn ctrl_c_quits_while_a_prompt_is_open() {
        let (mut app, _dir) = test_app();
        app.dispatch(Action::Open);
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    #[test]
    fn clear_empties_the_playlist_and_filter() {
        let (mut app, _dir) = test_app();
        app.ui.filter = Some("aphex".into());
        app.ui.playlist_cursor = 1;
        app.dispatch(Action::Clear);
        assert_eq!(app.snapshot().tracks.len(), 0);
        assert_eq!(app.snapshot().current, None);
        assert_eq!(app.ui.playlist_cursor, 0);
        assert_eq!(app.ui.filter, None);
    }

    #[test]
    fn move_up_down_swaps_tracks_and_carries_the_cursor() {
        let (mut app, _dir) = test_app();
        app.ui.playlist_cursor = 3;
        let first = app.snapshot().tracks[0].display_name();
        let third = app.snapshot().tracks[3].display_name();
        let fourth = app.snapshot().tracks[4].display_name();

        app.dispatch(Action::MoveDown);
        assert_eq!(app.snapshot().tracks[4].display_name(), third, "swapped");
        assert_eq!(app.snapshot().tracks[3].display_name(), fourth);
        assert_eq!(app.ui.playlist_cursor, 4, "cursor followed the track");

        app.dispatch(Action::MoveUp);
        assert_eq!(app.snapshot().tracks[3].display_name(), third, "back");
        assert_eq!(app.ui.playlist_cursor, 3);

        // Edges: no move, cursor stays put, no panic.
        app.ui.playlist_cursor = 0;
        app.dispatch(Action::MoveUp);
        assert_eq!(app.ui.playlist_cursor, 0);
        assert_eq!(
            app.snapshot().tracks[0].display_name(),
            first,
            "top edge holds"
        );

        app.dispatch(Action::JumpEnd);
        app.dispatch(Action::MoveDown);
        let last = app.snapshot().tracks.len() - 1;
        assert_eq!(app.ui.playlist_cursor, last, "bottom edge holds");
    }

    #[test]
    fn move_is_refused_while_filtered() {
        let (mut app, _dir) = test_app();
        app.ui.filter = Some("aphex".into());
        app.ui.playlist_cursor = 0;
        let before: Vec<String> = app
            .visible_indices()
            .iter()
            .map(|&i| app.snapshot().tracks[i].display_name())
            .collect();
        app.dispatch(Action::MoveDown);
        assert_eq!(
            app.ui.notice.as_deref(),
            Some("clear the filter to reorder")
        );
        let after: Vec<String> = app
            .visible_indices()
            .iter()
            .map(|&i| app.snapshot().tracks[i].display_name())
            .collect();
        assert_eq!(before, after, "order untouched");
    }

    #[test]
    fn cursor_bounds_use_the_visible_list() {
        let (mut app, _dir) = test_app();
        app.ui.filter = Some("boards".into()); // 2 matches
        app.dispatch(Action::JumpEnd);
        assert_eq!(app.ui.playlist_cursor, 1, "last *visible* row");
        app.dispatch(Action::CursorDown);
        assert_eq!(app.ui.playlist_cursor, 1, "clamped to visible len");
        app.dispatch(Action::JumpHome);
        app.dispatch(Action::CursorUp);
        assert_eq!(app.ui.playlist_cursor, 0);
    }

    #[test]
    fn open_prompt_replaces_and_starts_playing_via_scan() {
        let (mut app, dir) = test_app();
        // A tiny real library: one file in a temp dir.
        let music = dir.path().join("music");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::write(music.join("a.mp3"), b"").unwrap();
        std::fs::write(music.join("b.flac"), b"").unwrap();

        app.dispatch(Action::Open);
        // Type the path.
        for c in music.to_string_lossy().chars() {
            app.handle_key(press(KeyCode::Char(c)));
        }
        app.handle_key(press(KeyCode::Enter));
        assert!(app.ui.prompt.is_none());
        // Scan runs on a thread; drain it deterministically.
        for _ in 0..200 {
            app.tick(Duration::from_millis(1));
            if app
                .ui
                .notice
                .as_deref()
                .is_some_and(|n| n.starts_with("loaded"))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(app.snapshot().tracks.len(), 2, "scan replaced the list");
        assert_eq!(app.snapshot().current, Some(0), "Open starts at track 1");
        assert_eq!(app.snapshot().state, crate::player::PlayState::Playing);
        assert!(
            app.ui
                .notice
                .as_deref()
                .is_some_and(|n| n.contains("loaded")),
            "notice: {:?}",
            app.ui.notice
        );
    }

    #[test]
    fn second_load_is_refused_while_scanning() {
        let (mut app, dir) = test_app();
        // Hold a scan open by loading a big tree, then race a second load.
        let music = dir.path().join("music");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::write(music.join("a.mp3"), b"").unwrap();
        app.load_paths(vec![music.clone()], ScanMode::Append);
        app.load_paths(vec![music], ScanMode::Replace);
        assert_eq!(
            app.ui.notice.as_deref(),
            Some("already loading"),
            "second load refused"
        );
        // Drain.
        for _ in 0..200 {
            app.tick(Duration::from_millis(1));
            if app.ui.notice.as_deref() == Some("loaded 1 tracks (0 skipped)") {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn empty_playlist_canonical_start_renders_and_reports_no_files() {
        // `ampwire <dir with no audio>`: playlist untouched + notice.
        let (mut app, dir) = test_app();
        let empty = dir.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(empty.join("cover.jpg"), b"").unwrap();
        app.load_paths(vec![empty], ScanMode::Replace);
        for _ in 0..200 {
            app.tick(Duration::from_millis(1));
            if app.ui.notice.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(app.ui.notice.as_deref(), Some("no audio files found"));
        assert_eq!(app.snapshot().tracks.len(), 24, "playlist left alone");
    }

    // --- Plan 05: volume, balance, debounced persistence ---

    #[test]
    fn bracket_keys_move_the_balance_by_five() {
        let (mut app, _dir) = test_app();
        assert_eq!(app.snapshot().balance, 0);
        // `]` ×3 → +15.
        app.dispatch(Action::BalanceRight);
        app.dispatch(Action::BalanceRight);
        app.dispatch(Action::BalanceRight);
        assert_eq!(app.snapshot().balance, 15);
        // `[` ×6 → back past center to −15.
        for _ in 0..6 {
            app.dispatch(Action::BalanceLeft);
        }
        assert_eq!(app.snapshot().balance, -15);
        // Clamped at full left / full right.
        for _ in 0..50 {
            app.dispatch(Action::BalanceLeft);
        }
        assert_eq!(app.snapshot().balance, -100);
        for _ in 0..100 {
            app.dispatch(Action::BalanceRight);
        }
        assert_eq!(app.snapshot().balance, 100, "clamped at the right edge");
    }

    #[test]
    fn volume_clamps_at_100_and_balance_changes_mark_the_config_dirty() {
        let (mut app, _dir) = test_app();
        // Drive the volume up past the ceiling: it stays at 100.
        for _ in 0..100 {
            app.dispatch(Action::VolumeUp);
        }
        assert_eq!(app.snapshot().volume, 100, "clamped at 100");
        for _ in 0..100 {
            app.dispatch(Action::VolumeDown);
        }
        assert_eq!(app.snapshot().volume, 0, "clamped at 0");
        // Both changes copied into the config and set the debounce flag.
        assert_eq!(app.config.audio.volume, 0);
        assert!(app.config_dirty_since.is_some(), "debounce flag set");
    }

    #[test]
    fn a_dirty_config_saves_once_the_debounce_ages() {
        let (mut app, dir) = test_app();
        app.dispatch(Action::BalanceRight);
        assert_eq!(app.config.audio.balance, 5);
        // Not yet: a fresh flag never saves on its first tick.
        app.tick(Duration::from_millis(33));
        assert!(!dir.path().join("config.toml").exists());

        // Age the flag past the debounce window, then tick.
        app.config_dirty_since = Some(Instant::now() - Duration::from_secs(1));
        app.tick(Duration::from_millis(33));
        assert!(
            dir.path().join("config.toml").exists(),
            "debounced save fires from tick"
        );
        assert!(app.config_dirty_since.is_none(), "flag cleared after save");
        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(text.contains("balance = 5"), "{text}");
    }

    #[test]
    fn quitting_flushes_a_pending_config_save() {
        let (mut app, dir) = test_app();
        app.dispatch(Action::BalanceLeft);
        assert!(app.config_dirty_since.is_some());
        app.dispatch(Action::Quit);
        assert!(app.should_quit);
        assert!(app.config_dirty_since.is_none(), "flushed on quit");
        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(text.contains("balance = -5"), "{text}");
    }

    #[test]
    fn demo_mode_never_saves_audio_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut app = App::with_config_path(
            Config::default(),
            Keymap::defaults(),
            crate::ui::theme::neon(),
            true, // demo
            Some(path.clone()),
        );
        app.dispatch(Action::VolumeDown);
        app.dispatch(Action::BalanceRight);
        app.config_dirty_since = Some(Instant::now() - Duration::from_secs(1));
        app.tick(Duration::from_millis(33));
        assert!(!path.exists(), "demo mode never writes the config");
    }

    #[test]
    fn read_only_config_is_never_overwritten_by_audio_changes() {
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
        app.set_read_only(true, None);
        app.dispatch(Action::VolumeDown);
        app.config_dirty_since = Some(Instant::now() - Duration::from_secs(1));
        app.tick(Duration::from_millis(33));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "this is not toml [",
            "the broken file is untouched"
        );
    }
}
