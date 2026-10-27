//! App: UI state, event loop, tick, and action dispatch.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::action::{Action, Scope};
use crate::config::Config;
use crate::keymap::Keymap;
use crate::library::ScanMsg;
use crate::player::mock::MockPlayer;
use crate::player::real::RealPlayer;
use crate::player::{PlayState, PlayerCommand, PlayerSnapshot, Track};
use crate::remote::{RemoteCommand, SharedView, View, write_view};
use crate::smb::LoadInput;
use crate::ui;
use crate::ui::layout::{LayoutPrefs, Panel, PlaylistSide, Visibility, compute_layout};
use crate::ui::theme::omarchy::OmarchyWatch;
use crate::ui::theme::{Role, Theme};
use ratatui::layout::Rect;

/// Target frame budget while audio is playing (~30 fps). Spectrum and clock
/// advance every frame.
pub const FRAME: Duration = Duration::from_millis(33);
/// Paused: the only animation left is the 500 ms LCD blink.
const PAUSED_FRAME: Duration = Duration::from_millis(500);
/// Stopped or covered by an overlay: nothing animates. One frame a second is
/// enough for the Omarchy poll, the config debounce and a fresh snapshot.
const IDLE_FRAME: Duration = Duration::from_secs(1);
/// A background load is running: show progress without a full 30 fps.
const SCAN_FRAME: Duration = Duration::from_millis(200);
/// Wake at least this often even when no frame is due, so a control/MPRIS
/// command is applied within ~100 ms instead of waiting for the next frame.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
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

/// An open preset picker (plan 07): the cursor plus the EQ state to restore
/// on cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresetPicker {
    pub cursor: usize,
    pub saved_gains: [i8; 11],
    pub saved_on: bool,
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
    /// Open preset picker (`p`), if any. While open it takes the keys.
    pub presets: Option<PresetPicker>,
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
            presets: None,
            show_help: false,
            help_scroll: 0,
            notice: None,
            prompt: None,
            filter: None,
        }
    }
}

/// One computed [`App::visible_indices`] and what it was computed from.
#[derive(Debug)]
struct VisibleCache {
    tracks: Arc<Vec<Track>>,
    filter: Option<String>,
    indices: Arc<[usize]>,
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
/// Both variants are boxed: with plan 08's cache state the real player grew
/// far past the mock, and the enum would otherwise carry the fat variant on
/// every frame.
pub enum Backend {
    Mock(Box<MockPlayer>),
    Real(Box<RealPlayer>),
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
    /// balance, EQ). `tick` saves once it has aged
    /// [`CONFIG_SAVE_DEBOUNCE`], and quitting flushes it immediately.
    config_dirty_since: Option<Instant>,
    /// Terminal area of the frame about to be drawn; `None` until the first
    /// frame (tests, `--dump`). Focus uses it to skip auto-dropped panels.
    last_area: Option<Rect>,
    /// Memo behind [`App::visible_indices`].
    visible_cache: RefCell<Option<VisibleCache>>,
    /// Omarchy is unavailable and the neon fallback is already applied.
    omarchy_fallback: bool,
    /// A background directory scan is in flight (plan 02, step 8).
    scan_rx: Option<std::sync::mpsc::Receiver<ScanMsg>>,
    /// What to do with the tracks when the running scan finishes.
    scan_mode: ScanMode,
    /// Workers still to report for the running load: the local scan plus
    /// one per `smb://` URL, all over the same channel (plan 10, step 3).
    scan_pending: usize,
    /// This load has already put tracks in the playlist. The first
    /// non-empty `Done` applies the requested mode; every later worker
    /// must append, or it would wipe the half that arrived first.
    scan_loaded: bool,
    /// A background Jellyfin library load is in flight (plan 08, step 4).
    jf_rx: Option<std::sync::mpsc::Receiver<crate::jellyfin::JfMsg>>,
    /// The next `Page` is the first of this load → send `RemoveRemote` once
    /// (pressing `j` again drops the old remote batch before appending).
    jf_first_page: bool,
    /// Tracks actually appended by the running load (for the `Done` notice).
    jf_loaded: usize,
    /// Shared view + remote-command receiver, wired by `main` when MPRIS or
    /// the control socket is available. `None` in tests and `--dump`.
    remote: Option<RemoteState>,
    /// Redraw for a reason outside the frame cadence (input, a remote
    /// command, a scan page, a theme change).
    dirty: bool,
    /// Previous `frame_interval`, so a state change reschedules the deadline
    /// without making every input event push the animation clock forward.
    last_interval: Duration,
}

/// The receiver half of the shared remote plumbing.
struct RemoteState {
    rx: std::sync::mpsc::Receiver<RemoteCommand>,
    view: SharedView,
}

/// Fixed EQ state for `--dump` and the golden tests (plan §2.3 shape).
pub const DETERMINISTIC_EQ_BANDS: [i8; 11] = [0, 3, 6, 2, 0, -2, -3, 0, 2, 4, 5];

impl App {
    pub fn new(config: Config, keymap: Keymap, theme: Theme, demo: bool) -> Self {
        Self::with_config_path(config, keymap, theme, demo, crate::config::config_path())
    }

    /// Deterministic app for `--dump` and golden tests: frozen player state,
    /// fixed EQ bands, no config persistence, no Omarchy polling (the config
    /// is pinned to `neon` so ticks never touch the OS theme).
    pub fn deterministic(keymap: Keymap, theme: Theme) -> Self {
        let ui = UiState {
            vis: Visibility::all(),
            playlist_cursor: 2,
            eq_bands: DETERMINISTIC_EQ_BANDS,
            ..UiState::default()
        };
        let player = Backend::Mock(Box::new(MockPlayer::deterministic()));
        let snapshot = player.snapshot();
        let now = Instant::now();
        let mut config = Config::default();
        config.theme.preset = "neon".into();
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
            config_path: None,
            read_only: false,
            config_dirty_since: None,
            last_area: None,
            visible_cache: RefCell::new(None),
            omarchy_fallback: false,
            scan_rx: None,
            scan_mode: ScanMode::Replace,
            scan_pending: 0,
            scan_loaded: false,
            jf_rx: None,
            jf_first_page: false,
            jf_loaded: 0,
            remote: None,
            dirty: true,
            last_interval: FRAME,
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
        // re-confirmed for plan 03). `MockPlayer::default()` IS `animated()`.
        let player = Backend::Mock(Box::default());
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
            visible_cache: RefCell::new(None),
            omarchy_fallback: false,
            scan_rx: None,
            scan_mode: ScanMode::Replace,
            scan_pending: 0,
            scan_loaded: false,
            jf_rx: None,
            jf_first_page: false,
            jf_loaded: 0,
            remote: None,
            dirty: true,
            last_interval: FRAME,
        }
    }

    /// A real (non-demo) session: the playlist starts empty; tracks arrive
    /// from CLI paths or the `l`/`a` prompts (plan 02).
    pub fn new_session(config: Config, keymap: Keymap, theme: Theme) -> Self {
        let mut app =
            Self::with_config_path(config, keymap, theme, false, crate::config::config_path());
        // The real session starts empty and plays for real: this backend owns
        // the engine thread (dropped → Shutdown + join, so `q` exits fast).
        // `[jellyfin]` decides whether a rolling cache directory exists;
        // `[smb]` is the same switch for the SMB window (and is cloned into
        // the player, which creates that window on the first SMB track).
        let jellyfin = app.config.jellyfin.clone();
        let smb = app.config.smb.clone();
        app.player = Backend::Real(Box::new(RealPlayer::new(jellyfin.as_ref(), smb.as_ref())));
        // Restore the remembered audio settings before anything plays.
        let audio = app.config.audio;
        app.player.handle(PlayerCommand::SetVolume(audio.volume));
        app.player.handle(PlayerCommand::SetBalance(audio.balance));
        app.player
            .handle(PlayerCommand::SetSwapChannels(audio.swap_channels));
        // …and the remembered EQ (`[eq]`): a flat/off config keeps the
        // fresh session bit-perfect.
        app.ui.eq_on = app.config.eq.on;
        app.ui.eq_bands[0] = app.config.eq.preamp;
        app.ui.eq_bands[1..].copy_from_slice(&app.config.eq.bands);
        app.apply_eq();
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

    /// Run the event loop.
    ///
    /// The actual frame rate follows the player state: 30 fps while audio is
    /// playing (spectrum/clock), 2 fps while paused (LCD blink), 1 fps while
    /// stopped or covered by an overlay, and 5 fps during a background load.
    /// The loop still wakes every [`POLL_INTERVAL`] to drain control/MPRIS
    /// commands, but only draws when a frame is due or something changed —
    /// an idle Ampwire no longer redraws 30 times a second.
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        loop {
            let now = Instant::now();
            let due = now >= self.next_frame;
            if self.dirty || due {
                let size = terminal.size()?;
                self.sync_to_area(Rect::new(0, 0, size.width, size.height));
                terminal.draw(|frame| ui::render(frame, self))?;
                self.dirty = false;
                if due {
                    self.next_frame = advance_deadline(self.next_frame, now, self.frame_interval());
                }
            }

            let now = Instant::now();
            let timeout = self
                .next_frame
                .saturating_duration_since(now)
                .min(POLL_INTERVAL);
            if event::poll(timeout)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        self.handle_key(key);
                        self.dirty = true;
                    }
                    // Resize: next draw recomputes the layout from frame.area().
                    Event::Resize(_, _) => self.dirty = true,
                    _ => {}
                }
            }

            // Advance by the real elapsed time; cadence accumulators keep the
            // clock, marquee, blink and Omarchy poll independent of the draw
            // rate.
            let after_poll = Instant::now();
            self.tick(after_poll - self.last_tick);
            self.last_tick = after_poll;

            // A play/pause/stop transition (or a scan starting/finishing)
            // changes the right frame budget; reschedule without making every
            // input event push the animation clock forward.
            let interval = self.frame_interval();
            if interval != self.last_interval {
                self.last_interval = interval;
                self.next_frame = after_poll + interval;
            }

            if self.should_quit {
                self.flush_config(); // a prompt's Ctrl+C lands here, not in dispatch
                self.mark_stopped();
                return Ok(());
            }
        }
    }

    /// The frame budget for the current state. Overlays hide the animated
    /// player, so they use the idle cadence even while music plays.
    fn frame_interval(&self) -> Duration {
        if self.ui.show_help || self.ui.prompt.is_some() || self.ui.presets.is_some() {
            return IDLE_FRAME;
        }
        match self.snapshot.state {
            PlayState::Playing => FRAME,
            PlayState::Paused => PAUSED_FRAME,
            PlayState::Stopped => {
                if self.scan_rx.is_some() || self.jf_rx.is_some() {
                    SCAN_FRAME
                } else {
                    IDLE_FRAME
                }
            }
        }
    }

    /// Wire the shared view and the remote-command receiver (control socket
    /// and/or MPRIS). Called once from `main`, before the first frame.
    pub fn set_remote(&mut self, rx: std::sync::mpsc::Receiver<RemoteCommand>, view: SharedView) {
        self.remote = Some(RemoteState { rx, view });
        self.publish_view();
    }

    /// Replace the shared view with the current snapshot.
    fn publish_view(&self) {
        if let Some(remote) = &self.remote {
            write_view(
                &remote.view,
                View::from_snapshot(&self.snapshot, self.ui.notice.as_deref()),
            );
        }
    }

    /// Mark the shared view as stopped on the way out, so a racing
    /// `--status` does not claim a session that is already gone.
    fn mark_stopped(&self) {
        if let Some(remote) = &self.remote {
            write_view(&remote.view, View::not_running());
        }
    }

    /// Drain every command the services queued since the last tick. The
    /// channel is unbounded and the services never block; only this method
    /// touches the player.
    fn drain_remote(&mut self) {
        let mut commands = Vec::new();
        if let Some(remote) = &self.remote {
            while let Ok(command) = remote.rx.try_recv() {
                commands.push(command);
            }
        }
        for command in commands {
            self.dirty = true;
            self.apply_remote(command);
        }
    }

    fn apply_remote(&mut self, command: RemoteCommand) {
        // Remote commands are not keypresses: the help overlay must not
        // swallow a media key (dispatch's keyboard-only guard closes it).
        let help = self.ui.show_help;
        self.ui.show_help = false;
        match command {
            RemoteCommand::Play => self.dispatch(Action::Play),
            RemoteCommand::Pause => self.dispatch(Action::Pause),
            RemoteCommand::TogglePlayPause => self.dispatch(Action::PlayPause),
            RemoteCommand::Stop => self.dispatch(Action::Stop),
            RemoteCommand::Next => self.dispatch(Action::Next),
            RemoteCommand::Prev => self.dispatch(Action::Prev),
            RemoteCommand::ToggleShuffle => self.dispatch(Action::ToggleShuffle),
            RemoteCommand::ToggleRepeat => self.dispatch(Action::ToggleRepeat),
            RemoteCommand::SetVolume(volume) => self.set_volume_to(volume),
            RemoteCommand::SeekBy { delta_ms } => {
                let target = if delta_ms >= 0 {
                    self.snapshot
                        .position
                        .saturating_add(Duration::from_millis(delta_ms as u64))
                } else {
                    self.snapshot
                        .position
                        .saturating_sub(Duration::from_millis(delta_ms.unsigned_abs()))
                };
                self.player.handle(PlayerCommand::SeekTo(target));
                self.snapshot = self.player.snapshot();
            }
            RemoteCommand::SeekTo {
                track_id,
                position_ms,
            } => {
                if self.snapshot.current_track().map(|track| track.id) == Some(track_id) {
                    self.player
                        .handle(PlayerCommand::SeekTo(Duration::from_millis(position_ms)));
                    self.snapshot = self.player.snapshot();
                }
            }
            RemoteCommand::Load(inputs) => self.load_inputs(inputs, ScanMode::Replace),
            RemoteCommand::Quit => self.dispatch(Action::Quit),
        }
        self.ui.show_help = help;
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
    ///
    /// Asked for several times per frame, so it is computed once per
    /// (track list, filter) and shared (audit P6). The key holds the
    /// snapshot's `Arc` itself: while it is held, the player's next edit
    /// must copy-on-write a new list, so pointer equality really means
    /// "same tracks".
    pub fn visible_indices(&self) -> Arc<[usize]> {
        let tracks = &self.snapshot.tracks;
        let filter = self.ui.filter.as_deref();
        let mut cache = self.visible_cache.borrow_mut();
        if let Some(hit) = cache.as_ref()
            && Arc::ptr_eq(&hit.tracks, tracks)
            && hit.filter.as_deref() == filter
        {
            return Arc::clone(&hit.indices);
        }
        let indices: Arc<[usize]> = match filter {
            None => (0..tracks.len()).collect(),
            Some(filter) => {
                let needle = filter.to_lowercase();
                tracks
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| t.display_name().to_lowercase().contains(&needle))
                    .map(|(i, _)| i)
                    .collect()
            }
        };
        *cache = Some(VisibleCache {
            tracks: Arc::clone(tracks),
            filter: filter.map(str::to_string),
            indices: Arc::clone(&indices),
        });
        indices
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

    /// Feed a pressed key into the app. The preset picker, the help overlay
    /// and an open text prompt take keys before the keymap; everything else
    /// goes through it.
    pub fn handle_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        // The preset picker swallows every key except its own controls
        // (and Ctrl+C, which still quits) — it sits above the help overlay.
        if self.ui.presets.is_some() {
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true;
                }
                _ => self.handle_preset_key(key),
            }
            return;
        }
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

    /// Keys while the preset picker is open: ↑/↓ move + preview, Home/End/
    /// PgUp/PgDn jump, Enter applies, Esc or the picker key cancels.
    /// Everything else is swallowed.
    fn handle_preset_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        use crate::player::eq_presets::PRESETS;
        use ratatui::crossterm::event::KeyCode;
        let max = PRESETS.len().saturating_sub(1);
        let Some(picker) = self.ui.presets.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Up => {
                picker.cursor = picker.cursor.saturating_sub(1);
            }
            KeyCode::Down => {
                picker.cursor = (picker.cursor + 1).min(max);
            }
            KeyCode::Home => picker.cursor = 0,
            KeyCode::End => picker.cursor = max,
            KeyCode::PageUp => picker.cursor = picker.cursor.saturating_sub(10),
            KeyCode::PageDown => picker.cursor = (picker.cursor + 10).min(max),
            KeyCode::Enter => {
                let name = PRESETS[self.ui.presets.take().expect("picker is open").cursor].name;
                self.ui.notice = Some(format!("eq preset: {name}"));
                // The previewed values stay applied (and persist via send_eq).
                return;
            }
            KeyCode::Esc => {
                self.cancel_presets();
                return;
            }
            // The key bound to EqPresets (default `p`) also cancels.
            _ => {
                if self.keymap.resolve(self.ui.focus, key) == Some(Action::EqPresets) {
                    self.cancel_presets();
                }
                return; // everything else is swallowed
            }
        }
        self.preview_preset();
    }

    /// Push the cursor's preset into the live EQ (plan 07): the sliders and
    /// the sound change immediately. Turns the EQ on; the on/off state saved
    /// on open is restored by [`App::cancel_presets`].
    fn preview_preset(&mut self) {
        use crate::player::eq_presets::PRESETS;
        let Some(picker) = self.ui.presets.as_ref() else {
            return;
        };
        let gains = PRESETS[picker.cursor].gains();
        self.ui.eq_bands = gains;
        self.ui.eq_on = true;
        self.send_eq();
    }

    /// `Esc` / `p` on the picker: restore the EQ as it was when the picker
    /// opened, and close it.
    fn cancel_presets(&mut self) {
        if let Some(picker) = self.ui.presets.take() {
            self.ui.eq_bands = picker.saved_gains;
            self.ui.eq_on = picker.saved_on;
            self.send_eq();
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

    /// A submitted `l` / `a` prompt (plan 02, step 7 + plan 10, step 1):
    /// text carrying an `smb://` scheme goes straight to the loader — no
    /// `~` expansion and no `exists()` gate, because a share is not a path
    /// here. Everything else (including a URL that does not *parse*) takes
    /// the local route: expand `~`, the path must exist, then scan
    /// replace/append — which is why a broken URL still reports
    /// `not found: smb://…`.
    fn submit_prompt(&mut self, prompt: Prompt) {
        match prompt.kind {
            // Enter in Find keeps the filter and just closed the prompt.
            PromptKind::Find => {}
            PromptKind::Open | PromptKind::Add => {
                let mode = if prompt.kind == PromptKind::Open {
                    ScanMode::Replace
                } else {
                    ScanMode::Append
                };
                if let Ok(LoadInput::Smb(url)) = crate::smb::parse_load_input(&prompt.text) {
                    self.load_inputs(vec![LoadInput::Smb(url)], mode);
                    return;
                }
                let path = expand_tilde(&prompt.text);
                if !path.exists() {
                    self.ui.notice = Some(format!("not found: {}", path.display()));
                    return;
                }
                self.load_inputs(vec![LoadInput::Local(path)], mode);
            }
        }
    }

    /// Advance the mock clock/spectrum by `dt`, and fire the slower cadences
    /// (marquee, blink, Omarchy) when their accumulators fill up.
    pub fn tick(&mut self, dt: Duration) {
        self.drain_remote();
        self.player.tick(dt);
        self.snapshot = self.player.snapshot();
        // Surface whatever the real player queued this tick (engine errors,
        // shuffle/repeat feedback) in the playlist's notice slot.
        if let Some(notice) = self.player.take_notice() {
            self.ui.notice = Some(notice);
            self.dirty = true;
        }
        self.poll_scan();
        self.poll_jellyfin();

        // Marquee: one step every 200 ms, but only while audio is playing.
        // A paused/stopped title can stand still — it is not worth a redraw.
        if self.snapshot.state == PlayState::Playing {
            self.marquee_acc += dt;
            while self.marquee_acc >= MARQUEE {
                self.marquee_acc -= MARQUEE;
                self.ui.marquee_offset = self.ui.marquee_offset.wrapping_add(1);
            }
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
        // Debounced config save (volume/balance/EQ).
        if self
            .config_dirty_since
            .is_some_and(|since| since.elapsed() >= CONFIG_SAVE_DEBOUNCE)
        {
            self.config_dirty_since = None;
            self.save_config();
        }
        self.publish_view();
    }

    /// Kick off a background load of `inputs` (plan 02, step 8; plan 10,
    /// step 3): local paths go to the disk scanner, `smb://` URLs to one
    /// share-scan thread each — all over *one* channel, so a mixed command
    /// line (`ampwire ~/Music smb://nas/More`) runs both halves at once and
    /// [`App::poll_scan`] merges their messages. Refuses (with a notice)
    /// when a load is already running; results land via
    /// [`App::poll_scan`].
    pub fn load_inputs(&mut self, inputs: Vec<LoadInput>, mode: ScanMode) {
        if inputs.is_empty() {
            return;
        }
        if self.scan_rx.is_some() {
            self.ui.notice = Some("already loading".into());
            return;
        }
        let mut paths = Vec::new();
        let mut urls = Vec::new();
        for input in inputs {
            match input {
                LoadInput::Local(path) => paths.push(path),
                LoadInput::Smb(url) => urls.push(url),
            }
        }
        if paths.is_empty() && urls.is_empty() {
            return;
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let mut workers = 0usize;
        if !paths.is_empty() {
            crate::library::spawn_scan(paths, tx.clone());
            workers += 1;
        }
        // A share scan is much slower than a disk scan (plan 10, step 3
        // measured ≈0.7 s per file: every tag read is ~16 round trips), so
        // the URL is announced right away instead of leaving the UI silent
        // until the first 50-file progress tick.
        let announced = urls.first().map(|url| url.canonical());
        for url in urls {
            let cfg = self.smb_client_cfg(&url);
            crate::smb::scan::spawn_smb_scan(url, cfg, tx.clone());
            workers += 1;
        }
        drop(tx);
        if let Some(canonical) = announced {
            self.ui.notice = Some(format!("loading… {canonical}"));
        }
        self.scan_rx = Some(rx);
        self.scan_mode = mode;
        self.scan_pending = workers;
        self.scan_loaded = false;
    }

    /// How to log in for `url` (plan 10, step 4): its server plus the
    /// `[smb]` account/secret/domain, guest when there is no section.
    fn smb_client_cfg(&self, url: &crate::smb::SmbUrl) -> crate::smb::SmbClientCfg {
        crate::smb::SmbClientCfg::for_url(url, self.config.smb.as_ref())
    }

    /// [`load_inputs`][Self::load_inputs] for local paths only — the plan
    /// 02 callers and their tests. The CLI builds `Vec<LoadInput>` itself
    /// (plan 10, step 1), so nothing outside the tests uses this.
    #[cfg(test)]
    pub fn load_paths(&mut self, paths: Vec<std::path::PathBuf>, mode: ScanMode) {
        self.load_inputs(paths.into_iter().map(LoadInput::Local).collect(), mode)
    }

    /// Drain pending scan messages. `Done` applies the tracks according to
    /// the scan mode; an empty result leaves the playlist alone. One load
    /// may have several workers (the local scanner + one thread per
    /// `smb://` URL, plan 10, step 3), so the channel stays open until the
    /// last of them has reported.
    fn poll_scan(&mut self) {
        let Some(rx) = self.scan_rx.take() else {
            return;
        };
        let keep = loop {
            match rx.try_recv() {
                Ok(ScanMsg::Progress { found }) => {
                    self.ui.notice = Some(format!("loading… {found} files"));
                    self.dirty = true;
                }
                Ok(ScanMsg::Done { tracks, skipped }) => {
                    self.finish_scan(tracks, skipped);
                    self.dirty = true;
                    if self.scan_pending == 0 {
                        break false; // every worker reported
                    }
                }
                Ok(ScanMsg::Failed { reason }) => {
                    self.finish_failure(&reason);
                    self.dirty = true;
                    if self.scan_pending == 0 {
                        break false;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break true,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break false,
            }
        };
        if keep {
            self.scan_rx = Some(rx);
        }
    }

    /// A worker that could not even start (plan 10, step 3: `smb: cannot
    /// connect`, `not found: smb://…`): show its fixed line. The other
    /// workers of the same load keep running.
    ///
    /// With no `[smb]` section we cannot tell a refused login from an
    /// unreachable server, so the plan's answer stands: point at where the
    /// credentials go. Once a section exists the line is the plain
    /// `smb: cannot connect`.
    fn finish_failure(&mut self, reason: &str) {
        self.scan_pending = self.scan_pending.saturating_sub(1);
        self.ui.notice = Some(
            if reason == "smb: cannot connect" && self.config.smb.is_none() {
                "smb: add [smb] credentials to config.toml".to_string()
            } else {
                reason.to_string()
            },
        );
    }

    fn finish_scan(&mut self, tracks: Vec<Track>, skipped: usize) {
        self.scan_pending = self.scan_pending.saturating_sub(1);
        if tracks.is_empty() {
            // Nothing found: leave whatever was in the playlist alone —
            // and only say so once every worker of this load has reported
            // (a mixed load may still be carrying the other half).
            if self.scan_pending == 0 && !self.scan_loaded {
                self.ui.notice = Some("no audio files found".into());
            }
        } else {
            let loaded = tracks.len();
            // The first half of a mixed load applies the mode it was
            // requested with; every later half appends, or it would wipe
            // the tracks that arrived first.
            let mode = if self.scan_loaded {
                ScanMode::Append
            } else {
                self.scan_mode
            };
            match mode {
                ScanMode::Replace => {
                    self.player.handle(PlayerCommand::ReplaceTracks(tracks));
                    self.ui.playlist_cursor = 0;
                    self.ui.playlist_scroll = 0;
                }
                ScanMode::Append => {
                    self.player.handle(PlayerCommand::AddTracks(tracks));
                }
            }
            self.scan_loaded = true;
            self.ui.notice = Some(format!("loaded {loaded} tracks ({skipped} skipped)"));
        }
        self.snapshot = self.player.snapshot();
    }

    /// Kick off a background Jellyfin library load (plan 08, step 4): the
    /// worker thread pages the whole server and reports through
    /// [`App::poll_jellyfin`]; nothing here blocks. Refuses (with a notice)
    /// when there is no `[jellyfin]` section or one is already running.
    fn start_jellyfin_load(&mut self) {
        let configured = self
            .config
            .jellyfin
            .as_ref()
            .is_some_and(|jf| jf.is_complete());
        if !configured {
            self.ui.notice = Some("jellyfin: add [jellyfin] url and api_key to config.toml".into());
            return;
        }
        if self.jf_rx.is_some() {
            self.ui.notice = Some("jellyfin: already loading".into());
            return;
        }
        let config = self.config.jellyfin.clone().expect("checked above");
        let (tx, rx) = std::sync::mpsc::channel();
        crate::jellyfin::spawn_library_load(
            crate::jellyfin::Client::new(&config),
            config.user.clone(),
            tx,
        );
        self.jf_rx = Some(rx);
        self.jf_first_page = true;
        self.jf_loaded = 0;
    }

    /// Drain pending Jellyfin messages. The first `Page` drops the previous
    /// remote batch (`RemoveRemote`, once per load), then each page appends
    /// progressively — the playlist stays fully usable while loading, so
    /// this never touches the cursor or scrolls the view. An `Error` keeps
    /// everything already appended (plan §4).
    fn poll_jellyfin(&mut self) {
        let Some(rx) = self.jf_rx.take() else {
            return;
        };
        let mut changed = false;
        // `Done`/`Error`/disconnect end the load: the receiver is dropped
        // (not restored), but a partially-filled playlist still re-snapshots.
        let mut finished = false;
        loop {
            match rx.try_recv() {
                Ok(crate::jellyfin::JfMsg::Page {
                    tracks,
                    loaded,
                    total,
                }) => {
                    if self.jf_first_page {
                        self.player.handle(PlayerCommand::RemoveRemote);
                        self.jf_first_page = false;
                    }
                    self.jf_loaded += tracks.len();
                    self.player.handle(PlayerCommand::AppendInOrder(tracks));
                    self.ui.notice = Some(format!("jellyfin: loading {loaded}/{total}"));
                    changed = true;
                }
                Ok(crate::jellyfin::JfMsg::Done) => {
                    let n = self.jf_loaded;
                    self.ui.notice = Some(format!("jellyfin: loaded {n} tracks"));
                    finished = true;
                    break;
                }
                Ok(crate::jellyfin::JfMsg::Error(reason)) => {
                    self.ui.notice = Some(reason);
                    finished = true; // keep whatever was appended
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
            }
        }
        if !finished {
            self.jf_rx = Some(rx);
        }
        if changed || finished {
            self.snapshot = self.player.snapshot();
            self.clamp_cursor();
            self.dirty = true;
        }
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
        // The poll runs once a second; a redraw then is the idle cadence
        // anyway, and it makes a real theme change visible immediately.
        self.dirty = true;
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
        if self.ui.show_help
            && action != Action::Help
            && action != Action::Quit
            && action != Action::EqPresets
        {
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
            EqToggle => {
                self.ui.eq_on = !self.ui.eq_on;
                self.send_eq();
            }
            EqReset => {
                self.ui.eq_bands = [0; 11];
                self.send_eq();
            }
            EqPresets => {
                if self.ui.presets.is_some() {
                    // `p` while open acts like Esc.
                    self.cancel_presets();
                } else {
                    // Opening closes the help overlay first, saves the EQ
                    // to restore on cancel, and lands on the preset that
                    // matches the current curve (or Flat). The EQ itself
                    // does not change until a preview or apply.
                    self.ui.show_help = false;
                    let picker = PresetPicker {
                        cursor: crate::player::eq_presets::find_matching(&self.ui.eq_bands)
                            .unwrap_or(0),
                        saved_gains: self.ui.eq_bands,
                        saved_on: self.ui.eq_on,
                    };
                    self.ui.presets = Some(picker);
                }
            }

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
            LoadJellyfin => self.start_jellyfin_load(),
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
        self.dirty = true;
    }

    fn change_volume(&mut self, delta: i32) {
        let volume = (self.snapshot.volume as i32 + delta).clamp(0, 100) as u8;
        self.set_volume_to(volume);
    }

    /// Absolute volume, used by the keys' relative steps and by MPRIS/ctl.
    /// Updates the snapshot immediately so several remote commands in one
    /// tick do not all start from the same stale value.
    fn set_volume_to(&mut self, volume: u8) {
        let volume = volume.min(100);
        self.player.handle(PlayerCommand::SetVolume(volume));
        self.config.audio.volume = volume;
        self.config_dirty_since = Some(Instant::now());
        self.snapshot.volume = volume;
        self.dirty = true;
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
        self.send_eq();
    }

    /// Push the live EQ state (on/off + preamp + 10 gains) to the player
    /// and into the config, arming the debounced save (plan 06).
    fn send_eq(&mut self) {
        self.apply_eq();
        self.config.eq.on = self.ui.eq_on;
        self.config.eq.preamp = self.ui.eq_bands[0];
        self.config.eq.bands.copy_from_slice(&self.ui.eq_bands[1..]);
        self.config_dirty_since = Some(Instant::now());
    }

    /// Send the EQ command only — used at session start, where the config
    /// state is being *restored*, not changed (nothing to save yet).
    fn apply_eq(&mut self) {
        self.player.handle(PlayerCommand::SetEq {
            enabled: self.ui.eq_on,
            gains: self.ui.eq_bands,
        });
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
/// more than one interval late resyncs instead of catching up in a burst.
fn advance_deadline(deadline: Instant, now: Instant, interval: Duration) -> Instant {
    if now < deadline {
        return deadline;
    }
    let next = deadline + interval;
    if next < now { now + interval } else { next }
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
    /// The theme preset is pinned to `neon` so ticks never poll the OS
    /// theme — unit tests stay hermetic on Omarchy machines.
    fn test_app() -> (App, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::default();
        config.theme.preset = "neon".into();
        let app = App::with_config_path(
            config,
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
            deadline = advance_deadline(deadline, start + Duration::from_millis(ms), FRAME);
        }
        assert_eq!(deadline, start + FRAME);
        // Deadline reached: the next frame is one frame later.
        assert_eq!(
            advance_deadline(deadline, deadline, FRAME),
            deadline + FRAME
        );
        // Far behind (slow draw): resync to one interval from now.
        let late = deadline + FRAME * 5;
        assert_eq!(advance_deadline(deadline, late, FRAME), late + FRAME);
    }

    fn remote_app() -> (
        App,
        tempfile::TempDir,
        std::sync::mpsc::Sender<RemoteCommand>,
    ) {
        let (mut app, dir) = test_app();
        let (tx, rx) = std::sync::mpsc::channel();
        app.set_remote(rx, crate::remote::new_view());
        (app, dir, tx)
    }

    #[test]
    fn remote_volume_updates_snapshot_and_config() {
        let (mut app, _dir, tx) = remote_app();
        tx.send(RemoteCommand::SetVolume(42)).unwrap();
        app.tick(Duration::ZERO);
        assert_eq!(app.snapshot().volume, 42);
        assert_eq!(app.config.audio.volume, 42);
        assert!(app.config_dirty_since.is_some(), "debounced save armed");
    }

    #[test]
    fn remote_absolute_seek_respects_the_track_id() {
        let (mut app, _dir, tx) = remote_app();
        let track_id = app.snapshot().current_track().unwrap().id;
        tx.send(RemoteCommand::SeekTo {
            track_id,
            position_ms: 42_000,
        })
        .unwrap();
        app.tick(Duration::ZERO);
        assert_eq!(app.snapshot().position, Duration::from_secs(42));

        let before = app.snapshot().position;
        tx.send(RemoteCommand::SeekTo {
            track_id: track_id + 9999,
            position_ms: 1_000,
        })
        .unwrap();
        app.tick(Duration::ZERO);
        assert_eq!(app.snapshot().position, before, "stale track id ignored");
    }

    #[test]
    fn remote_commands_work_while_help_is_open() {
        let (mut app, _dir, tx) = remote_app();
        app.dispatch(Action::Help);
        assert!(app.ui.show_help);
        tx.send(RemoteCommand::SetVolume(17)).unwrap();
        app.tick(Duration::ZERO);
        assert_eq!(app.snapshot().volume, 17);
        assert!(app.ui.show_help, "a media key must not close the overlay");
    }

    #[test]
    fn remote_quit_quits() {
        let (mut app, _dir, tx) = remote_app();
        tx.send(RemoteCommand::Quit).unwrap();
        app.tick(Duration::ZERO);
        assert!(app.should_quit);
    }

    #[test]
    fn frame_interval_follows_the_player_state() {
        let (mut app, _dir) = test_app();
        // The mock starts playing: full 30 fps for the spectrum.
        assert_eq!(app.frame_interval(), FRAME);
        app.dispatch(Action::Pause);
        assert_eq!(app.frame_interval(), PAUSED_FRAME);
        app.dispatch(Action::Stop);
        assert_eq!(app.frame_interval(), IDLE_FRAME);
        app.dispatch(Action::Play);
        assert_eq!(app.frame_interval(), FRAME);
        // An overlay hides the animated player: idle even while playing.
        app.dispatch(Action::Help);
        assert_eq!(app.frame_interval(), IDLE_FRAME);
    }

    #[test]
    fn paused_marquee_does_not_advance() {
        let (mut app, _dir) = test_app();
        app.dispatch(Action::Pause);
        let before = app.ui.marquee_offset;
        app.tick(Duration::from_millis(600));
        assert_eq!(app.ui.marquee_offset, before);
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
        assert_eq!(*visible, [0, 17], "Aphex Twin ×2: {visible:?}");
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

    /// Audit P6 guard: the visible rows follow every playlist edit and
    /// every filter change — whatever caching sits behind them.
    #[test]
    fn visible_indices_follow_edits_and_filter_changes() {
        let (mut app, _dir) = test_app();
        app.ui.filter = Some("boards".into());
        assert_eq!(app.visible_indices().to_vec(), vec![1, 16]);
        assert_eq!(app.visible_indices().to_vec(), vec![1, 16], "stable");

        app.ui.filter = Some("aphex".into());
        assert_eq!(app.visible_indices().to_vec(), vec![0, 17], "new filter");

        app.ui.playlist_cursor = 0;
        app.dispatch(Action::Del); // removes real track 0 (Aphex Twin)
        assert_eq!(app.visible_indices().to_vec(), vec![16], "after a removal");

        app.ui.filter = None;
        assert_eq!(app.visible_indices().len(), 23, "no filter, one fewer");
    }

    #[test]
    fn select_on_a_filtered_row_plays_the_real_track() {
        let (mut app, _dir) = test_app();
        app.ui.filter = Some("boards".into());
        // "boards of canada": Roygbiv (1) and Dayvan Cowboy (16).
        assert_eq!(*app.visible_indices(), [1, 16]);
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

    /// Plan 10, step 1: a URL that does not parse falls through to the
    /// local path branch, so the user sees the usual `not found: …`.
    #[test]
    fn enter_on_a_malformed_smb_url_notices_not_found() {
        let (mut app, _dir) = test_app();
        app.dispatch(Action::Add);
        for c in "smb://nas".chars() {
            app.handle_key(press(KeyCode::Char(c)));
        }
        app.handle_key(press(KeyCode::Enter));
        assert!(app.ui.prompt.is_none());
        assert_eq!(
            app.ui.notice.as_deref(),
            Some("not found: smb://nas"),
            "an unparseable URL takes the local route"
        );
        assert_eq!(app.snapshot().tracks.len(), 24, "playlist untouched");
    }

    /// Plan 10, step 1: a *valid* `smb://` never reaches `expand_tilde` or
    /// `exists()` — there is no local file to find — so the notice is
    /// anything but `not found: …`.
    #[test]
    fn a_valid_smb_url_skips_the_exists_gate() {
        let (mut app, _dir) = test_app();
        app.dispatch(Action::Open);
        for c in "smb://nas/Music".chars() {
            app.handle_key(press(KeyCode::Char(c)));
        }
        app.handle_key(press(KeyCode::Enter));
        assert!(app.ui.prompt.is_none());
        assert_ne!(
            app.ui.notice.as_deref(),
            Some("not found: smb://nas/Music"),
            "a URL is never checked with Path::exists"
        );
        assert_eq!(app.snapshot().tracks.len(), 24, "playlist untouched");
    }

    /// Plan 10, step 1: a mixed `Vec<LoadInput>` keeps the local half (the
    /// SMB half is scanned by step 3) and refuses a second load while a
    /// scan runs, exactly like two local paths.
    #[test]
    fn mixed_loads_take_the_local_half_and_still_refuse_concurrently() {
        let (mut app, dir) = test_app();
        let music = dir.path().join("music");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::write(music.join("a.mp3"), b"").unwrap();

        let url = match crate::smb::parse_load_input("smb://nas/More").expect("url") {
            LoadInput::Smb(url) => url,
            LoadInput::Local(_) => panic!("parsed as a path"),
        };
        app.load_inputs(
            vec![
                LoadInput::Local(music.clone()),
                LoadInput::Smb(url),
                LoadInput::Local(music),
            ],
            ScanMode::Replace,
        );
        // The scan is running → a second load is refused with the usual line.
        app.load_inputs(
            vec![LoadInput::Local(dir.path().to_path_buf())],
            ScanMode::Append,
        );
        assert_eq!(
            app.ui.notice.as_deref(),
            Some("already loading"),
            "the running scan wins over the notice"
        );
    }

    /// Plan 10, step 3: one `smb://` URL becomes one worker on the *same*
    /// channel — its connect failure surfaces as the fixed line, and while
    /// any worker runs a second load is refused.
    #[test]
    fn an_smb_load_reports_its_connect_failure_and_blocks_concurrent_loads() {
        let (mut app, _dir) = test_app();
        let LoadInput::Smb(url) =
            crate::smb::parse_load_input("smb://127.0.0.1/Music").expect("valid url")
        else {
            panic!("expected an smb input");
        };
        app.load_inputs(vec![LoadInput::Smb(url)], ScanMode::Replace);
        assert!(app.scan_rx.is_some(), "a worker is running");

        // A second load while it runs → the usual refusal.
        app.load_inputs(vec![LoadInput::Local("/tmp".into())], ScanMode::Append);
        assert_eq!(app.ui.notice.as_deref(), Some("already loading"));

        // 127.0.0.1:445 refuses instantly, so the failure is quick.
        for _ in 0..200 {
            app.tick(Duration::from_millis(1));
            if app.scan_rx.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(app.scan_rx.is_none(), "the worker finished");
        assert_eq!(app.scan_pending, 0, "nothing left to report");
        assert_eq!(
            app.ui.notice.as_deref(),
            Some("smb: add [smb] credentials to config.toml"),
            "no `[smb]` section → the plan's pointer, a fixed line with no \
             URL or credential in it"
        );
        assert_eq!(app.snapshot().tracks.len(), 24, "playlist untouched");

        // With a `[smb]` section the line is the plain connect failure.
        let config = toml::from_str::<crate::config::Config>("[smb]\n").expect("section");
        app.config = config;
        let LoadInput::Smb(url) =
            crate::smb::parse_load_input("smb://127.0.0.1/Music").expect("valid url")
        else {
            panic!("expected an smb input");
        };
        app.load_inputs(vec![LoadInput::Smb(url)], ScanMode::Replace);
        for _ in 0..200 {
            app.tick(Duration::from_millis(1));
            if app.scan_rx.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            app.ui.notice.as_deref(),
            Some("smb: cannot connect"),
            "a configured section reports the plain failure"
        );
    }

    /// Plan 10, step 3: a mixed command line runs both halves at once over
    /// one channel — the local scan still lands when the share refuses.
    #[test]
    fn a_mixed_load_lands_the_local_half_even_when_the_share_fails() {
        let (mut app, dir) = test_app();
        let music = dir.path().join("music");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::write(music.join("a.mp3"), b"").unwrap();
        std::fs::write(music.join("b.flac"), b"").unwrap();
        let LoadInput::Smb(url) =
            crate::smb::parse_load_input("smb://127.0.0.1/More").expect("valid url")
        else {
            panic!("expected an smb input");
        };

        app.load_inputs(
            vec![LoadInput::Local(music), LoadInput::Smb(url)],
            ScanMode::Replace,
        );
        for _ in 0..400 {
            app.tick(Duration::from_millis(1));
            if app.scan_rx.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(app.scan_rx.is_none(), "both workers reported");
        assert_eq!(app.scan_pending, 0);
        assert_eq!(
            app.snapshot().tracks.len(),
            2,
            "the local half of a mixed load lands"
        );
        // Whichever worker finished last owns the notice.
        let notice = app.ui.notice.clone().unwrap_or_default();
        assert!(
            notice == "smb: add [smb] credentials to config.toml"
                || notice == "smb: cannot connect"
                || notice.starts_with("loaded 2 tracks"),
            "unexpected notice: {notice}"
        );
    }

    /// Plan 10, step 3 (the "merge the `Done`s" decision): the first
    /// non-empty half of a load applies the mode it was requested with,
    /// every later half appends — otherwise the second worker would wipe
    /// the first — and `no audio files found` only shows when *nothing*
    /// loaded.
    #[test]
    fn the_first_done_of_a_load_applies_the_mode_and_the_rest_append() {
        let (mut app, _dir) = test_app();
        let row = |name: &str| Track {
            id: Track::next_id(),
            source: crate::player::TrackSource::Local(std::path::PathBuf::from(format!(
                "/x/{name}.flac"
            ))),
            artist: None,
            title: None,
            duration: None,
            bitrate_kbps: None,
            pos: Default::default(),
        };

        app.scan_mode = ScanMode::Replace;
        app.scan_pending = 2;
        app.scan_loaded = false;

        app.finish_scan(vec![row("a")], 0);
        assert_eq!(app.snapshot().tracks.len(), 1, "Replace wiped the list");
        assert_eq!(app.scan_pending, 1, "one worker still to go");
        assert_eq!(
            app.ui.notice.as_deref(),
            Some("loaded 1 tracks (0 skipped)")
        );

        app.finish_scan(vec![row("b")], 3);
        assert_eq!(
            app.snapshot().tracks.len(),
            2,
            "the second half appended instead of replacing"
        );
        assert_eq!(
            app.ui.notice.as_deref(),
            Some("loaded 1 tracks (3 skipped)")
        );
        assert_eq!(app.scan_pending, 0);

        // An empty half after a successful one says nothing new…
        app.scan_pending = 1;
        app.finish_scan(Vec::new(), 0);
        assert_eq!(
            app.ui.notice.as_deref(),
            Some("loaded 1 tracks (3 skipped)"),
            "the empty half does not overwrite a successful notice"
        );

        // …while an entirely empty load still reports the local wording.
        app.scan_mode = ScanMode::Replace;
        app.scan_pending = 1;
        app.scan_loaded = false;
        app.finish_scan(Vec::new(), 1);
        assert_eq!(app.ui.notice.as_deref(), Some("no audio files found"));
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

    // --- Plan 06: EQ actions → SetEq → config ---

    #[test]
    fn band_up_reaches_the_player_as_a_set_eq() {
        let (mut app, _dir) = test_app();
        app.ui.focus = Scope::Eq;
        app.ui.eq_focus = 3;
        app.dispatch(Action::BandUp);
        assert_eq!(app.ui.eq_bands[3], 1);
        assert_eq!(
            app.mock_mut().last_eq(),
            Some((true, [0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0])),
            "BandUp must ship the new gains as SetEq"
        );
        // The change also landed in the config and armed the debounce.
        assert_eq!(app.config.eq.bands[2], 1, "band 3 of the config is index 2");
        assert!(app.config_dirty_since.is_some(), "debounce flag set");
    }

    #[test]
    fn eq_reset_zeroes_everything_and_sends_flat() {
        let (mut app, _dir) = test_app();
        app.ui.focus = Scope::Eq;
        app.ui.eq_focus = 0;
        app.dispatch(Action::BandUp); // preamp +1
        app.ui.eq_focus = 7;
        app.dispatch(Action::BandDown); // a band −1
        app.dispatch(Action::EqReset);
        assert_eq!(app.ui.eq_bands, [0; 11], "all 11 values zeroed");
        assert_eq!(app.mock_mut().last_eq(), Some((true, [0; 11])));
        assert_eq!(app.config.eq.preamp, 0);
        assert_eq!(app.config.eq.bands, [0; 10]);
        assert!(app.config_dirty_since.is_some());
    }

    #[test]
    fn o_toggles_the_eq_from_eq_focus_only() {
        let (mut app, _dir) = test_app();
        app.ui.focus = Scope::Eq;
        assert!(app.ui.eq_on);
        app.handle_key(press(KeyCode::Char('o')));
        assert!(!app.ui.eq_on, "`o` toggles the EQ in eq focus");
        assert_eq!(app.mock_mut().last_eq(), Some((false, [0; 11])));
        app.handle_key(press(KeyCode::Char('o')));
        assert!(app.ui.eq_on, "toggled back on");
        assert!(app.config_dirty_since.is_some());

        // From another panel `o` does nothing (eq scope only, no global
        // binding — the keymap conflict test pins that).
        app.ui.focus = Scope::Player;
        app.handle_key(press(KeyCode::Char('o')));
        assert!(app.ui.eq_on, "`o` is eq-scope only");
    }

    #[test]
    fn eq_changes_survive_a_restart_through_the_config() {
        let (mut app, dir) = test_app();
        app.ui.focus = Scope::Eq;
        app.ui.eq_focus = 0;
        for _ in 0..3 {
            app.dispatch(Action::BandUp); // preamp +3
        }
        app.dispatch(Action::EqToggle); // and off
        app.config_dirty_since = Some(Instant::now() - Duration::from_secs(1));
        app.tick(Duration::from_millis(33));

        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(text.contains("[eq]"), "{text}");
        let loaded = Config::load_from(&dir.path().join("config.toml")).config;
        assert!(!loaded.eq.on);
        assert_eq!(loaded.eq.preamp, 3);
        assert_eq!(loaded.eq.bands, [0; 10], "only the preamp moved");
    }

    #[test]
    fn a_real_session_restores_the_eq_from_the_config() {
        let mut config = Config::default();
        config.eq.on = false;
        config.eq.preamp = -4;
        config.eq.bands = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let app = App::new_session(config, Keymap::defaults(), crate::ui::theme::neon());
        assert!(!app.ui.eq_on, "`[eq] on = false` restored");
        assert_eq!(
            app.ui.eq_bands,
            [-4, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
            "`[eq]` preamp + bands restored into the sliders"
        );
    }

    #[test]
    fn demo_mode_never_saves_eq_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut app = App::with_config_path(
            Config::default(),
            Keymap::defaults(),
            crate::ui::theme::neon(),
            true, // demo
            Some(path.clone()),
        );
        app.ui.focus = Scope::Eq;
        app.dispatch(Action::BandUp);
        app.dispatch(Action::EqToggle);
        app.config_dirty_since = Some(Instant::now() - Duration::from_secs(1));
        app.tick(Duration::from_millis(33));
        assert!(!path.exists(), "demo mode never writes the config");
    }

    // --- Plan 07: preset picker ---

    /// Press a key by its default spec (e.g. `p`) through `handle_key`.
    fn key(c: char) -> KeyEvent {
        press(KeyCode::Char(c))
    }

    #[test]
    fn p_opens_the_picker_without_touching_the_eq() {
        let (mut app, _dir) = test_app();
        // A custom curve that matches no preset → cursor starts on Flat (0).
        app.ui.eq_bands = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
        let saved = app.ui.eq_bands;
        app.handle_key(key('p'));
        let picker = app.ui.presets.expect("picker open");
        assert_eq!(picker.cursor, 0, "no match → Flat");
        assert_eq!(picker.saved_gains, saved);
        assert!(picker.saved_on, "eq_on default is true");
        assert_eq!(app.ui.eq_bands, saved, "opening does not change the EQ");

        // A curve matching a preset starts the cursor on it.
        app.handle_key(key('p')); // close (cancel)
        assert!(app.ui.presets.is_none());
        app.ui.eq_bands = crate::player::eq_presets::PRESETS[5].gains(); // Rock
        app.handle_key(key('p'));
        assert_eq!(app.ui.presets.unwrap().cursor, 5, "cursor on the match");
        app.handle_key(key('p'));
    }

    #[test]
    fn down_previews_the_preset_and_turns_the_eq_on() {
        use crate::player::eq_presets::PRESETS;
        let (mut app, _dir) = test_app();
        app.ui.eq_on = false;
        app.handle_key(key('p'));
        let start = app.ui.presets.unwrap().cursor;
        app.handle_key(press(KeyCode::Down));
        let picker = app.ui.presets.expect("still open");
        let expected = (start + 1).min(PRESETS.len() - 1);
        assert_eq!(picker.cursor, expected, "cursor moved down");
        assert_eq!(app.ui.eq_bands, PRESETS[expected].gains(), "previewed");
        assert!(app.ui.eq_on, "previewing turns the EQ on");

        // Up goes back and clamps at the top.
        app.handle_key(press(KeyCode::Up));
        app.handle_key(press(KeyCode::Up));
        assert_eq!(app.ui.presets.unwrap().cursor, 0, "clamped at the top");
        assert_eq!(app.ui.eq_bands, PRESETS[0].gains(), "Flat previewed");
    }

    #[test]
    fn preset_jumps_and_page_keys() {
        use crate::player::eq_presets::PRESETS;
        let (mut app, _dir) = test_app();
        app.handle_key(key('p'));
        app.handle_key(press(KeyCode::End));
        assert_eq!(app.ui.presets.unwrap().cursor, PRESETS.len() - 1);
        app.handle_key(press(KeyCode::PageUp));
        assert_eq!(
            app.ui.presets.unwrap().cursor,
            0,
            "page up from the last row clamps at the top"
        );
        app.handle_key(press(KeyCode::Home));
        app.handle_key(press(KeyCode::PageDown));
        assert_eq!(app.ui.presets.unwrap().cursor, PRESETS.len() - 1);
    }

    #[test]
    fn esc_restores_the_previous_bands_and_on_state() {
        let (mut app, _dir) = test_app();
        // Start with the EQ off and a custom curve: both must come back.
        app.ui.eq_on = false;
        app.ui.eq_bands = [2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2];
        let saved_bands = app.ui.eq_bands;
        app.handle_key(key('p'));
        app.handle_key(press(KeyCode::Down)); // preview Full Bass
        assert_ne!(app.ui.eq_bands, saved_bands, "preview changed the bands");
        assert!(app.ui.eq_on);
        app.handle_key(press(KeyCode::Esc));
        assert!(app.ui.presets.is_none(), "Esc closes the picker");
        assert_eq!(app.ui.eq_bands, saved_bands, "bands restored exactly");
        assert!(!app.ui.eq_on, "the previous on/off state is restored too");
    }

    #[test]
    fn enter_keeps_the_previewed_preset_and_notifies() {
        use crate::player::eq_presets::PRESETS;
        let (mut app, _dir) = test_app();
        app.handle_key(key('p'));
        app.handle_key(press(KeyCode::Down));
        let previewed = PRESETS[app.ui.presets.unwrap().cursor].gains();
        app.handle_key(press(KeyCode::Enter));
        assert!(app.ui.presets.is_none(), "Enter closes the picker");
        assert_eq!(app.ui.eq_bands, previewed, "the preview is kept");
        let notice = app.ui.notice.expect("preset notice");
        assert!(notice.starts_with("eq preset: "), "{notice}");
        // The kept preset is a known one, by name.
        assert!(
            PRESETS
                .iter()
                .any(|p| notice == format!("eq preset: {}", p.name)),
            "{notice}"
        );
    }

    #[test]
    fn preset_picker_swallows_transport_keys_but_ctrl_c_still_quits() {
        let (mut app, _dir) = test_app();
        app.handle_key(key('p'));
        let state = app.snapshot().state;
        let volume = app.snapshot().volume;
        let cursor = app.snapshot().current;
        app.handle_key(press(KeyCode::Char(' '))); // play/pause
        app.handle_key(press(KeyCode::Char('b'))); // next
        app.handle_key(press(KeyCode::Char('+'))); // volume up
        app.handle_key(press(KeyCode::Char('q'))); // quit
        assert_eq!(app.snapshot().state, state, "space was swallowed");
        assert_eq!(app.snapshot().volume, volume, "volume was swallowed");
        assert_eq!(app.snapshot().current, cursor, "next was swallowed");
        assert!(!app.should_quit, "`q` was swallowed");
        assert!(app.ui.presets.is_some(), "still open");
        // Ctrl+C still quits (unflushed config flushes in run(), not here).
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit, "Ctrl+C quits");
    }

    #[test]
    fn p_while_open_cancels() {
        let (mut app, _dir) = test_app();
        app.ui.eq_on = false;
        app.ui.eq_bands = [3; 11];
        app.handle_key(key('p'));
        app.handle_key(press(KeyCode::Down)); // preview
        app.handle_key(key('p')); // cancel acts like Esc
        assert!(app.ui.presets.is_none(), "p closes the picker");
        assert_eq!(app.ui.eq_bands, [3; 11], "restored");
        assert!(!app.ui.eq_on, "restored");
    }

    #[test]
    fn opening_presets_closes_the_help_overlay_first() {
        let (mut app, _dir) = test_app();
        app.dispatch(Action::Help);
        assert!(app.ui.show_help);
        app.handle_key(key('p'));
        assert!(!app.ui.show_help, "help closed by opening presets");
        assert!(app.ui.presets.is_some(), "presets opened");
    }

    // --- Jellyfin load (plan 08, step 4 + 7) ---

    /// A config with a complete `[jellyfin]` section pointing at a closed
    /// port: a real `j` spawns a worker that fails instantly and harmlessly
    /// (nothing listens on 127.0.0.1:9).
    fn jf_config() -> Config {
        let mut config = Config::default();
        config.jellyfin = Some(crate::config::JellyfinConfig {
            url: "http://127.0.0.1:9".into(),
            api_key: "test-key".into(),
            api_key_file: None,
            user: None,
            mode: crate::config::JellyfinMode::Cache,
            cache_ahead: 3,
            cache_behind: 3,
            cache_dir: None,
            notice: None,
        });
        config
    }

    /// A remote (Jellyfin) track, for playlist assertions.
    fn remote_track(id: &str) -> Track {
        Track {
            id: Track::next_id(),
            source: crate::player::TrackSource::Remote {
                item_id: id.into(),
                container: None,
            },
            artist: None,
            title: Some(format!("t-{id}")),
            duration: None,
            bitrate_kbps: None,
            pos: Default::default(),
        }
    }

    #[test]
    fn j_without_a_jellyfin_section_notices_and_loads_nothing() {
        let (mut app, _dir) = test_app(); // default config: no [jellyfin]
        app.dispatch(Action::LoadJellyfin);
        assert_eq!(
            app.ui.notice.as_deref(),
            Some("jellyfin: add [jellyfin] url and api_key to config.toml")
        );
        assert!(app.jf_rx.is_none(), "no worker without config");
    }

    #[test]
    fn j_twice_refuses_the_second_press_while_still_loading() {
        let (mut app, _dir) = test_app();
        app.config = jf_config();
        app.dispatch(Action::LoadJellyfin);
        assert!(app.jf_rx.is_some(), "first j starts the worker");
        assert_eq!(app.ui.notice, None, "no complaint on the first press");
        app.dispatch(Action::LoadJellyfin);
        assert_eq!(app.ui.notice.as_deref(), Some("jellyfin: already loading"));
    }

    #[test]
    fn the_first_jellyfin_page_removes_old_remote_tracks_exactly_once() {
        let (mut app, _dir) = test_app();
        let (tx, rx) = std::sync::mpsc::channel();
        app.jf_rx = Some(rx);
        app.jf_first_page = true;

        // Two pages arrive in one tick: progressive append, one removal.
        tx.send(crate::jellyfin::JfMsg::Page {
            tracks: vec![remote_track("a"), remote_track("b")],
            loaded: 2,
            total: 4,
        })
        .unwrap();
        tx.send(crate::jellyfin::JfMsg::Page {
            tracks: vec![remote_track("c")],
            loaded: 3,
            total: 4,
        })
        .unwrap();
        app.tick(Duration::ZERO);

        let commands = &app.mock_mut().commands;
        let removals = commands
            .iter()
            .filter(|c| matches!(c, PlayerCommand::RemoveRemote))
            .count();
        assert_eq!(removals, 1, "RemoveRemote once per load: {commands:?}");
        let appended: usize = commands
            .iter()
            .filter_map(|c| match c {
                PlayerCommand::AppendInOrder(tracks) => Some(tracks.len()),
                _ => None,
            })
            .sum();
        assert_eq!(appended, 3, "both pages appended in server order");
        assert_eq!(
            app.ui.notice.as_deref(),
            Some("jellyfin: loading 3/4"),
            "progress notice"
        );

        // Done: summary notice, receiver released.
        tx.send(crate::jellyfin::JfMsg::Done).unwrap();
        app.tick(Duration::ZERO);
        assert_eq!(app.ui.notice.as_deref(), Some("jellyfin: loaded 3 tracks"));
        assert!(app.jf_rx.is_none(), "a finished load releases the slot");

        // The next load arms `RemoveRemote` again (first page of load #2).
        let (tx2, rx2) = std::sync::mpsc::channel();
        app.jf_rx = Some(rx2);
        app.jf_first_page = true;
        tx2.send(crate::jellyfin::JfMsg::Page {
            tracks: vec![remote_track("d")],
            loaded: 1,
            total: 1,
        })
        .unwrap();
        app.tick(Duration::ZERO);
        let removals = app
            .mock_mut()
            .commands
            .iter()
            .filter(|c| matches!(c, PlayerCommand::RemoveRemote))
            .count();
        assert_eq!(removals, 2, "one removal per reload");
    }

    #[test]
    fn a_jellyfin_error_keeps_what_was_appended() {
        let (mut app, _dir) = test_app();
        let (tx, rx) = std::sync::mpsc::channel();
        app.jf_rx = Some(rx);
        app.jf_first_page = true;
        let before = app.snapshot.tracks.len(); // the mock's 24 fake rows
        tx.send(crate::jellyfin::JfMsg::Page {
            tracks: vec![remote_track("a")],
            loaded: 1,
            total: 10,
        })
        .unwrap();
        tx.send(crate::jellyfin::JfMsg::Error(
            "jellyfin: invalid API key".into(),
        ))
        .unwrap();
        app.tick(Duration::ZERO);
        assert_eq!(
            app.ui.notice.as_deref(),
            Some("jellyfin: invalid API key"),
            "the worker's reason reaches the notice"
        );
        assert_eq!(
            app.snapshot.tracks.len(),
            before + 1,
            "page 1 stays in the list"
        );
        assert!(app.jf_rx.is_none(), "the failed load is over");
    }
}
