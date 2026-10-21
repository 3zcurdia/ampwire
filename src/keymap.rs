//! Default keymap, config parsing, and `(scope, KeyEvent) → Action` resolution.
//!
//! Lookup order: the focused panel's scope first, then `Global`. Keys are
//! parsed from config strings like `"space"`, `"shift+l"`, `"ctrl+t"`.

use std::collections::HashMap;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::{Action, Scope};

/// A normalized key: code + modifiers, ignoring press/release kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl Key {
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    /// Build from a crossterm event, normalizing uppercase chars so that
    /// `Shift+L` arrives as `Char('l') + SHIFT` on every terminal.
    pub fn from_event(event: KeyEvent) -> Self {
        let mut code = event.code;
        let modifiers = event.modifiers;
        if let KeyCode::Char(c) = code
            && modifiers.contains(KeyModifiers::SHIFT)
            && c.is_ascii_uppercase()
        {
            code = KeyCode::Char(c.to_ascii_lowercase());
        }
        Self { code, modifiers }
    }

    /// Parse a config key spec: `"z"`, `"space"`, `"shift+l"`, `"ctrl+t"`.
    /// A lone `+` is the plus key, not a modifier prefix.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim().to_ascii_lowercase();
        let mut modifiers = KeyModifiers::NONE;
        let mut code_part = spec.as_str();

        // Only split on '+' when a modifier name precedes it.
        if let Some((prefix, rest)) = code_part.split_once('+')
            && !prefix.is_empty()
            && !rest.is_empty()
            && !rest.starts_with('+')
        {
            let mut parts = vec![prefix];
            let mut remaining = rest;
            while let Some((prefix, rest)) = remaining.split_once('+')
                && !prefix.is_empty()
                && matches!(prefix, "shift" | "ctrl" | "control" | "alt")
            {
                parts.push(prefix);
                remaining = rest;
            }
            for prefix in parts {
                match prefix {
                    "shift" => modifiers |= KeyModifiers::SHIFT,
                    "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
                    "alt" => modifiers |= KeyModifiers::ALT,
                    _ => return Err(format!("unknown modifier in key spec `{spec}`")),
                }
            }
            code_part = remaining;
        }
        if code_part.is_empty() {
            return Err(format!("empty key code in key spec `{spec}`"));
        }

        let code = match code_part {
            "space" => KeyCode::Char(' '),
            "enter" | "return" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "tab" => KeyCode::Tab,
            "backtab" => KeyCode::BackTab,
            "backspace" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" | "pgup" => KeyCode::PageUp,
            "pagedown" | "pgdn" => KeyCode::PageDown,
            "f1" => KeyCode::F(1),
            "f2" => KeyCode::F(2),
            "f3" => KeyCode::F(3),
            "f4" => KeyCode::F(4),
            "f5" => KeyCode::F(5),
            s if s.chars().count() == 1 => KeyCode::Char(s.chars().next().unwrap()),
            _ => return Err(format!("unknown key in key spec `{spec}`")),
        };
        Ok(Self::new(code, modifiers))
    }

    /// Whether this key matches a pressed event, tolerating terminals that
    /// report SHIFT on non-letter chars like `+`.
    pub fn matches(self, event: Key) -> bool {
        if self == event {
            return true;
        }
        // `+` and friends are often delivered as `Char('+') + SHIFT`.
        if self.modifiers.is_empty()
            && event.modifiers == KeyModifiers::SHIFT
            && !matches!(self.code, KeyCode::Char(c) if c.is_ascii_alphabetic())
            && self.code == event.code
        {
            return true;
        }
        false
    }

    /// Human-readable form for hints and the help overlay.
    pub fn display(self) -> String {
        let mut out = String::new();
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            out.push_str("ctrl+");
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            out.push_str("alt+");
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            out.push_str("shift+");
        }
        out.push_str(&match self.code {
            KeyCode::Char(' ') => "space".to_string(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Enter => "enter".into(),
            KeyCode::Esc => "esc".into(),
            KeyCode::Tab => "tab".into(),
            KeyCode::BackTab => "shift+tab".into(),
            KeyCode::Backspace => "backspace".into(),
            KeyCode::Delete => "delete".into(),
            KeyCode::Up => "up".into(),
            KeyCode::Down => "down".into(),
            KeyCode::Left => "left".into(),
            KeyCode::Right => "right".into(),
            KeyCode::Home => "home".into(),
            KeyCode::End => "end".into(),
            KeyCode::PageUp => "pgup".into(),
            KeyCode::PageDown => "pgdn".into(),
            KeyCode::F(n) => format!("f{n}"),
            other => format!("{other:?}"),
        });
        out
    }
}

/// Default bindings: `(scope, key spec, action)`.
fn defaults() -> Vec<(Scope, Key, Action)> {
    use Action::*;
    let raw: Vec<(Scope, &str, Action)> = vec![
        // Global transport
        (Scope::Global, "space", PlayPause),
        (Scope::Global, "z", Prev),
        (Scope::Global, "x", Play),
        (Scope::Global, "c", Pause),
        (Scope::Global, "v", Stop),
        (Scope::Global, "b", Next),
        (Scope::Global, "l", Open),
        (Scope::Global, "s", ToggleShuffle),
        (Scope::Global, "r", ToggleRepeat),
        // Volume: arrows while the player is focused, +/- anywhere
        (Scope::Global, "+", VolumeUp),
        (Scope::Global, "=", VolumeUp),
        (Scope::Global, "-", VolumeDown),
        (Scope::Player, "up", VolumeUp),
        (Scope::Player, "down", VolumeDown),
        // Seek: player and playlist focus
        (Scope::Player, "left", SeekBack),
        (Scope::Player, "right", SeekFwd),
        (Scope::Playlist, "left", SeekBack),
        (Scope::Playlist, "right", SeekFwd),
        // EQ focus
        (Scope::Eq, "left", BandLeft),
        (Scope::Eq, "right", BandRight),
        (Scope::Eq, "up", BandUp),
        (Scope::Eq, "down", BandDown),
        // Playlist focus
        (Scope::Playlist, "up", CursorUp),
        (Scope::Playlist, "down", CursorDown),
        (Scope::Playlist, "enter", Select),
        (Scope::Playlist, "pgup", PageUp),
        (Scope::Playlist, "pgdn", PageDown),
        (Scope::Playlist, "home", JumpHome),
        (Scope::Playlist, "end", JumpEnd),
        (Scope::Playlist, "a", Add),
        (Scope::Playlist, "d", Del),
        (Scope::Playlist, "/", Find),
        // Layout and misc
        (Scope::Global, "1", TogglePlayer),
        (Scope::Global, "2", ToggleEq),
        (Scope::Global, "3", TogglePlaylist),
        (Scope::Global, "shift+l", ToggleColumns),
        (Scope::Global, "shift+o", CycleOrder),
        (Scope::Global, "ctrl+t", ToggleTimeMode),
        (Scope::Global, "tab", FocusNext),
        (Scope::Global, "backtab", FocusPrev),
        (Scope::Global, "?", Help),
        (Scope::Global, "q", Quit),
        (Scope::Global, "ctrl+c", Quit),
    ];
    raw.into_iter()
        .map(|(scope, spec, action)| {
            let key = Key::parse(spec).expect("default key specs are valid");
            (scope, key, action)
        })
        .collect()
}

/// The active keymap: `(scope, key) → Action`.
#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: Vec<(Scope, Key, Action)>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self::defaults()
    }
}

impl Keymap {
    pub fn defaults() -> Self {
        Self {
            bindings: defaults(),
        }
    }

    /// Resolve a pressed key against the focused panel, then global scope.
    pub fn resolve(&self, focus: Scope, event: KeyEvent) -> Option<Action> {
        let key = Key::from_event(event);
        self.bindings
            .iter()
            .find(|(scope, k, _)| *scope == focus && k.matches(key))
            .or_else(|| {
                self.bindings
                    .iter()
                    .find(|(scope, k, _)| *scope == Scope::Global && k.matches(key))
            })
            .map(|(_, _, action)| *action)
    }

    /// First (default-order) key bound to an action, for on-screen hints.
    pub fn hint(&self, action: Action) -> Option<String> {
        self.bindings
            .iter()
            .find(|(_, _, a)| *a == action)
            .map(|(_, k, _)| k.display())
    }

    /// Every key bound to an action, for the help overlay.
    pub fn keys_for(&self, action: Action) -> Vec<String> {
        self.bindings
            .iter()
            .filter(|(_, _, a)| *a == action)
            .map(|(_, k, _)| k.display())
            .collect()
    }

    /// Build a keymap from `[keys]` config overrides.
    ///
    /// Spec syntax is `"key"` (bound in the action's own scope) or
    /// `"scope:key"` (e.g. `"global:+"`). Conflicts within one scope keep the
    /// defaults for the conflicting actions and are reported as strings.
    pub fn from_overrides(overrides: &HashMap<String, Vec<String>>) -> (Self, Vec<String>) {
        let mut bindings = defaults();
        let mut conflicts = Vec::new();

        for (name, specs) in overrides {
            let Some(action) = action_by_name(name) else {
                conflicts.push(format!("unknown action `{name}` in [keys], ignored"));
                continue;
            };
            let primary_scope = action.scope();
            let mut new_bindings = Vec::new();
            let mut bad_spec = false;

            for spec in specs {
                let (scope, key_spec) = match spec.split_once(':') {
                    Some((scope_name, key_spec)) => match scope_name {
                        "global" => (Scope::Global, key_spec),
                        "player" => (Scope::Player, key_spec),
                        "eq" => (Scope::Eq, key_spec),
                        "playlist" => (Scope::Playlist, key_spec),
                        _ => {
                            conflicts.push(format!("unknown scope `{scope_name}` in `{spec}`"));
                            bad_spec = true;
                            continue;
                        }
                    },
                    None => (primary_scope, spec.as_str()),
                };
                match Key::parse(key_spec) {
                    Ok(key) => new_bindings.push((scope, key, action)),
                    Err(err) => {
                        conflicts.push(err);
                        bad_spec = true;
                    }
                }
            }

            if bad_spec || new_bindings.is_empty() {
                continue;
            }

            // Conflict check inside each scope the override touches.
            for (scope, key, _) in &new_bindings {
                let clash = bindings
                    .iter()
                    .any(|(s, k, a)| s == scope && k.matches(*key) && a != &action);
                if clash {
                    let other = bindings
                        .iter()
                        .find(|(s, k, a)| s == scope && k.matches(*key) && a != &action)
                        .map(|(_, _, a)| a.name())
                        .unwrap_or("?");
                    conflicts.push(format!(
                        "{} bound to `{}` conflicts with {} in {scope:?} scope; keeping defaults for both",
                        action.name(),
                        key.display(),
                        other
                    ));
                    // Keep defaults for this action entirely.
                    bindings.retain(|(_, _, a)| a != &action);
                    bindings.extend(defaults().into_iter().filter(|(_, _, a)| a == &action));
                    bad_spec = true;
                    break;
                }
            }
            if bad_spec {
                continue;
            }

            bindings.retain(|(_, _, a)| a != &action);
            bindings.extend(new_bindings);
        }

        (Self { bindings }, conflicts)
    }
}

fn action_by_name(name: &str) -> Option<Action> {
    Action::all().iter().copied().find(|a| a.name() == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn key_specs_parse_and_display() {
        assert_eq!(Key::parse("z").unwrap().code, KeyCode::Char('z'));
        assert_eq!(Key::parse("space").unwrap().code, KeyCode::Char(' '));
        let shift_l = Key::parse("shift+l").unwrap();
        assert_eq!(shift_l.modifiers, KeyModifiers::SHIFT);
        assert_eq!(shift_l.display(), "shift+l");
        assert_eq!(Key::parse("ctrl+t").unwrap().display(), "ctrl+t");
    }

    #[test]
    fn defaults_resolve_in_focus_scope() {
        let map = Keymap::defaults();
        // up: volume in player focus, cursor in playlist focus
        assert_eq!(
            map.resolve(Scope::Player, press(KeyCode::Up)),
            Some(Action::VolumeUp)
        );
        assert_eq!(
            map.resolve(Scope::Playlist, press(KeyCode::Up)),
            Some(Action::CursorUp)
        );
        assert_eq!(
            map.resolve(Scope::Eq, press(KeyCode::Up)),
            Some(Action::BandUp)
        );
        // Global keys work from any focus
        assert_eq!(
            map.resolve(Scope::Playlist, press(KeyCode::Char(' '))),
            Some(Action::PlayPause)
        );
        assert_eq!(
            map.resolve(Scope::Player, press(KeyCode::Char('q'))),
            Some(Action::Quit)
        );
    }

    #[test]
    fn shift_letter_normalizes() {
        let map = Keymap::defaults();
        let event = KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT);
        assert_eq!(
            map.resolve(Scope::Global, event),
            Some(Action::ToggleColumns)
        );
    }

    #[test]
    fn remap_shows_in_hints_and_works() {
        let mut overrides = HashMap::new();
        overrides.insert("next".to_string(), vec!["b".into(), "n".into()]);
        let (map, conflicts) = Keymap::from_overrides(&overrides);
        assert!(conflicts.is_empty(), "{conflicts:?}");
        assert_eq!(
            map.resolve(Scope::Global, press(KeyCode::Char('n'))),
            Some(Action::Next)
        );
        let hints = map.keys_for(Action::Next);
        assert_eq!(hints, vec!["b".to_string(), "n".to_string()]);
    }

    #[test]
    fn conflicting_remap_reports_and_keeps_defaults() {
        let mut overrides = HashMap::new();
        // `n` is free, but steal `b` (already Next) for play_pause → conflict
        overrides.insert("play_pause".to_string(), vec!["b".into()]);
        let (map, conflicts) = Keymap::from_overrides(&overrides);
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert!(conflicts[0].contains("conflicts"));
        // Defaults kept: space still plays/pauses, b still next
        assert_eq!(
            map.resolve(Scope::Global, press(KeyCode::Char(' '))),
            Some(Action::PlayPause)
        );
        assert_eq!(
            map.resolve(Scope::Global, press(KeyCode::Char('b'))),
            Some(Action::Next)
        );
    }

    #[test]
    fn unknown_action_reported() {
        let mut overrides = HashMap::new();
        overrides.insert("warp".to_string(), vec!["w".into()]);
        let (_, conflicts) = Keymap::from_overrides(&overrides);
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].contains("unknown action"));
    }
}
