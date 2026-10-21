//! Every remappable command in the app.
//!
//! The keymap maps `(scope, KeyEvent)` to an `Action`; dispatch consumes
//! actions. Widgets never read keys directly — on-screen hints are generated
//! from the live keymap, so a remap shows up everywhere.

/// Where an action applies. `Global` keys work no matter which panel has
/// focus; the rest only when their panel is focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    Global,
    Player,
    Eq,
    Playlist,
}

/// Every remappable command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    // Transport (global)
    PlayPause,
    Prev,
    Play,
    Pause,
    Stop,
    Next,
    Open, // stub until file loading exists
    ToggleShuffle,
    ToggleRepeat,

    // Player scope
    SeekBack,
    SeekFwd,
    VolumeUp,
    VolumeDown,

    // Equalizer scope
    BandLeft,
    BandRight,
    BandUp,
    BandDown,

    // Playlist scope
    CursorUp,
    CursorDown,
    Select,
    PageUp,
    PageDown,
    JumpHome,
    JumpEnd,

    // Layout (global)
    TogglePlayer,
    ToggleEq,
    TogglePlaylist,
    ToggleColumns,
    CycleOrder,

    // Misc (global)
    ToggleTimeMode,
    FocusNext,
    FocusPrev,
    Help,
    Quit,

    // Stubs until playlist editing exists
    Add,
    Del,
    Find,
}

impl Action {
    /// Stable name used in `[keys]` config entries.
    pub fn name(self) -> &'static str {
        match self {
            Action::PlayPause => "play_pause",
            Action::Prev => "prev",
            Action::Play => "play",
            Action::Pause => "pause",
            Action::Stop => "stop",
            Action::Next => "next",
            Action::Open => "open",
            Action::ToggleShuffle => "shuffle",
            Action::ToggleRepeat => "repeat",
            Action::SeekBack => "seek_back",
            Action::SeekFwd => "seek_fwd",
            Action::VolumeUp => "volume_up",
            Action::VolumeDown => "volume_down",
            Action::BandLeft => "band_left",
            Action::BandRight => "band_right",
            Action::BandUp => "band_up",
            Action::BandDown => "band_down",
            Action::CursorUp => "cursor_up",
            Action::CursorDown => "cursor_down",
            Action::Select => "select",
            Action::PageUp => "page_up",
            Action::PageDown => "page_down",
            Action::JumpHome => "jump_home",
            Action::JumpEnd => "jump_end",
            Action::TogglePlayer => "toggle_player",
            Action::ToggleEq => "toggle_eq",
            Action::TogglePlaylist => "toggle_playlist",
            Action::ToggleColumns => "toggle_columns",
            Action::CycleOrder => "cycle_order",
            Action::ToggleTimeMode => "toggle_time_mode",
            Action::FocusNext => "focus_next",
            Action::FocusPrev => "focus_prev",
            Action::Help => "help",
            Action::Quit => "quit",
            Action::Add => "add",
            Action::Del => "del",
            Action::Find => "find",
        }
    }

    /// Scope a key must be pressed in (in addition to `Global`).
    pub fn scope(self) -> Scope {
        match self {
            Action::SeekBack | Action::SeekFwd | Action::VolumeUp | Action::VolumeDown => {
                Scope::Player
            }
            Action::BandLeft | Action::BandRight | Action::BandUp | Action::BandDown => Scope::Eq,
            Action::CursorUp
            | Action::CursorDown
            | Action::Select
            | Action::PageUp
            | Action::PageDown
            | Action::JumpHome
            | Action::JumpEnd
            | Action::Add
            | Action::Del
            | Action::Find => Scope::Playlist,
            _ => Scope::Global,
        }
    }

    /// Human-readable description for the help overlay.
    pub fn description(self) -> &'static str {
        match self {
            Action::PlayPause => "play / pause",
            Action::Prev => "previous track",
            Action::Play => "play",
            Action::Pause => "pause",
            Action::Stop => "stop",
            Action::Next => "next track",
            Action::Open => "open / eject (stub)",
            Action::ToggleShuffle => "shuffle",
            Action::ToggleRepeat => "repeat",
            Action::SeekBack => "seek back 5s",
            Action::SeekFwd => "seek forward 5s",
            Action::VolumeUp => "volume up",
            Action::VolumeDown => "volume down",
            Action::BandLeft => "previous EQ band",
            Action::BandRight => "next EQ band",
            Action::BandUp => "band gain +1 dB",
            Action::BandDown => "band gain -1 dB",
            Action::CursorUp => "cursor up",
            Action::CursorDown => "cursor down",
            Action::Select => "play track under cursor",
            Action::PageUp => "page up",
            Action::PageDown => "page down",
            Action::JumpHome => "jump to first track",
            Action::JumpEnd => "jump to last track",
            Action::TogglePlayer => "show / hide player",
            Action::ToggleEq => "show / hide equalizer",
            Action::TogglePlaylist => "show / hide playlist",
            Action::ToggleColumns => "toggle 1 / 2 columns",
            Action::CycleOrder => "cycle panel order",
            Action::ToggleTimeMode => "elapsed / remaining time",
            Action::FocusNext => "focus next panel",
            Action::FocusPrev => "focus previous panel",
            Action::Help => "key help",
            Action::Quit => "quit",
            Action::Add => "add tracks (stub)",
            Action::Del => "delete track (stub)",
            Action::Find => "find in playlist (stub)",
        }
    }

    /// All actions, in a stable display order for the help overlay.
    pub fn all() -> &'static [Action] {
        use Action::*;
        &[
            PlayPause,
            Prev,
            Play,
            Pause,
            Stop,
            Next,
            Open,
            ToggleShuffle,
            ToggleRepeat,
            SeekBack,
            SeekFwd,
            VolumeUp,
            VolumeDown,
            BandLeft,
            BandRight,
            BandUp,
            BandDown,
            CursorUp,
            CursorDown,
            Select,
            PageUp,
            PageDown,
            JumpHome,
            JumpEnd,
            TogglePlayer,
            ToggleEq,
            TogglePlaylist,
            ToggleColumns,
            CycleOrder,
            ToggleTimeMode,
            FocusNext,
            FocusPrev,
            Help,
            Quit,
            Add,
            Del,
            Find,
        ]
    }
}
