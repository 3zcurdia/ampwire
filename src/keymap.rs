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

    /// Parse a config key spec: `"z"`, `"L"` (= Shift+L), `"space"`,
    /// `"shift+l"`, `"ctrl+t"`. A lone `+` is the plus key, not a modifier
    /// prefix. An uppercase letter with no explicit modifier implies Shift.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        let mut modifiers = KeyModifiers::NONE;
        let mut code_part = spec;

        // Only split on '+' when a modifier name precedes it. Modifier names
        // are matched case-insensitively.
        if let Some((prefix, rest)) = code_part.split_once('+')
            && !prefix.is_empty()
            && !rest.is_empty()
            && !rest.starts_with('+')
            && matches!(
                prefix.to_ascii_lowercase().as_str(),
                "shift" | "ctrl" | "control" | "alt"
            )
        {
            let mut parts = vec![prefix];
            let mut remaining = rest;
            while let Some((prefix, rest)) = remaining.split_once('+')
                && !prefix.is_empty()
                && matches!(
                    prefix.to_ascii_lowercase().as_str(),
                    "shift" | "ctrl" | "control" | "alt"
                )
            {
                parts.push(prefix);
                remaining = rest;
            }
            for prefix in parts {
                match prefix.to_ascii_lowercase().as_str() {
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

        // Named keys are case-insensitive; match on the lowercased form.
        let named = code_part.to_ascii_lowercase();
        let code = match named.as_str() {
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
            _ => {
                // A single character. An uppercase letter implies Shift, so
                // `"L"` means Shift+L (and its code is the lowercase letter,
                // matching how terminals report it).
                let mut chars = code_part.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if c.is_ascii_uppercase() => {
                        modifiers |= KeyModifiers::SHIFT;
                        KeyCode::Char(c.to_ascii_lowercase())
                    }
                    (Some(c), None) => KeyCode::Char(c),
                    _ => return Err(format!("unknown key in key spec `{spec}`")),
                }
            }
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
    /// `"scope:key"` (e.g. `"global:+"`). Overrides are applied in sorted
    /// action-name order so the result is deterministic. Then conflicts are
    /// detected against the *final* map: any key bound to two different
    /// actions in one scope reverts **every** action involved back to its
    /// defaults, and each such revert is reported.
    pub fn from_overrides(overrides: &HashMap<String, Vec<String>>) -> (Self, Vec<String>) {
        let mut messages = Vec::new();

        // 1. Parse each override, in sorted action-name order.
        let mut names: Vec<&String> = overrides.keys().collect();
        names.sort();
        let mut parsed: Vec<(Action, Vec<(Scope, Key)>)> = Vec::new();
        for name in names {
            let specs = &overrides[name];
            let Some(action) = action_by_name(name) else {
                messages.push(format!("unknown action `{name}` in [keys], ignored"));
                continue;
            };
            let primary_scope = action.scope();
            let mut keys = Vec::new();
            let mut bad = false;
            for spec in specs {
                let (scope, key_spec) = match spec.split_once(':') {
                    Some((scope_name, key_spec)) => match scope_name {
                        "global" => (Scope::Global, key_spec),
                        "player" => (Scope::Player, key_spec),
                        "eq" => (Scope::Eq, key_spec),
                        "playlist" => (Scope::Playlist, key_spec),
                        _ => {
                            messages.push(format!("unknown scope `{scope_name}` in `{spec}`"));
                            bad = true;
                            continue;
                        }
                    },
                    None => (primary_scope, spec.as_str()),
                };
                match Key::parse(key_spec) {
                    Ok(key) => keys.push((scope, key)),
                    Err(err) => {
                        messages.push(err);
                        bad = true;
                    }
                }
            }
            if bad || keys.is_empty() {
                continue;
            }
            parsed.push((action, keys));
        }

        // 2. Build the final map: defaults, with each overridden action's
        //    bindings replaced by its override.
        let mut use_default: std::collections::HashSet<Action> = std::collections::HashSet::new();
        loop {
            let bindings = build_bindings(&parsed, &use_default);
            let conflicts = find_conflicts(&bindings);
            if conflicts.is_empty() {
                break;
            }
            // Revert every action involved in any conflict to its defaults.
            let mut any_new = false;
            for (scope, key, actions) in &conflicts {
                let names: Vec<&str> = actions.iter().map(|a| a.name()).collect();
                messages.push(format!(
                    "{} all bound to `{}` in {scope:?} scope; reverting to defaults",
                    names.join(", "),
                    key.display(),
                ));
                for action in actions {
                    if use_default.insert(*action) {
                        any_new = true;
                    }
                }
            }
            if !any_new {
                // Every conflicted action is already on defaults (which are
                // conflict-free), so we can't be stuck.
                break;
            }
        }

        (
            Self {
                bindings: build_bindings(&parsed, &use_default),
            },
            messages,
        )
    }
}

/// Assemble the binding list: defaults, with each non-reverted override
/// replacing its action's default bindings.
fn build_bindings(
    parsed: &[(Action, Vec<(Scope, Key)>)],
    use_default: &std::collections::HashSet<Action>,
) -> Vec<(Scope, Key, Action)> {
    let mut bindings = Vec::new();
    for (scope, key, action) in defaults() {
        let overridden = parsed.iter().any(|(a, _)| *a == action);
        if overridden && !use_default.contains(&action) {
            continue; // replaced below
        }
        bindings.push((scope, key, action));
    }
    for (action, keys) in parsed {
        if use_default.contains(action) {
            continue;
        }
        for (scope, key) in keys {
            bindings.push((*scope, *key, *action));
        }
    }
    bindings
}

/// Find every `(scope, key)` bound to more than one distinct action.
fn find_conflicts(bindings: &[(Scope, Key, Action)]) -> Vec<(Scope, Key, Vec<Action>)> {
    let mut groups: Vec<(Scope, Key, Vec<Action>)> = Vec::new();
    for (scope, key, action) in bindings {
        if let Some(g) = groups
            .iter_mut()
            .find(|(s, k, _)| s == scope && k.matches(*key))
        {
            if !g.2.contains(action) {
                g.2.push(*action);
            }
        } else {
            groups.push((*scope, *key, vec![*action]));
        }
    }
    groups.retain(|(_, _, actions)| actions.len() > 1);
    groups
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
    fn conflicting_remap_reverts_every_action_involved() {
        let mut overrides = HashMap::new();
        // Steal `b` (already Next) for play_pause → both revert to defaults.
        overrides.insert("play_pause".to_string(), vec!["b".into()]);
        let (map, messages) = Keymap::from_overrides(&overrides);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].contains("reverting to defaults"),
            "{}",
            messages[0]
        );
        assert!(messages[0].contains("next"), "{}", messages[0]);
        assert!(messages[0].contains("play_pause"), "{}", messages[0]);
        // Both reverted: space still plays/pauses, b still next.
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
    fn swapping_two_keys_is_not_a_conflict() {
        // Swap next= z and prev= b. Each key ends up on exactly one action.
        let mut overrides = HashMap::new();
        overrides.insert("next".to_string(), vec!["z".into()]);
        overrides.insert("prev".to_string(), vec!["b".into()]);
        let (map, messages) = Keymap::from_overrides(&overrides);
        assert!(messages.is_empty(), "{messages:?}");
        assert_eq!(
            map.resolve(Scope::Global, press(KeyCode::Char('z'))),
            Some(Action::Next)
        );
        assert_eq!(
            map.resolve(Scope::Global, press(KeyCode::Char('b'))),
            Some(Action::Prev)
        );
    }

    #[test]
    fn override_replaces_all_default_keys_for_an_action() {
        // next defaults to `b`; remap to just `n` → `b` is freed entirely.
        let mut overrides = HashMap::new();
        overrides.insert("next".to_string(), vec!["n".into()]);
        let (map, messages) = Keymap::from_overrides(&overrides);
        assert!(messages.is_empty(), "{messages:?}");
        assert_eq!(
            map.resolve(Scope::Global, press(KeyCode::Char('b'))),
            None,
            "`b` was Next's default and is now unbound"
        );
        assert_eq!(
            map.resolve(Scope::Global, press(KeyCode::Char('n'))),
            Some(Action::Next)
        );
        assert_eq!(map.hint(Action::Next).as_deref(), Some("n"));
    }

    #[test]
    fn uppercase_letter_spec_implies_shift() {
        assert_eq!(Key::parse("L").unwrap(), Key::parse("shift+l").unwrap());
        // Lowercase stays unshifted.
        let l = Key::parse("l").unwrap();
        assert!(l.modifiers.is_empty());
        // Uppercase non-letters are not shifted (they already encode it).
        assert!(Key::parse("+").unwrap().modifiers.is_empty());
    }

    #[test]
    fn unknown_action_reported() {
        let mut overrides = HashMap::new();
        overrides.insert("warp".to_string(), vec!["w".into()]);
        let (_, messages) = Keymap::from_overrides(&overrides);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("unknown action"));
    }

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    /// Table test (plan §5): every default key resolves to the right action in
    /// the right scope.
    #[test]
    fn every_plan_default_resolves() {
        let map = Keymap::defaults();
        let c = KeyCode::Char;
        let table: &[(Scope, KeyEvent, Action)] = &[
            // Transport (global)
            (Scope::Global, press(c(' ')), Action::PlayPause),
            (Scope::Global, press(c('z')), Action::Prev),
            (Scope::Global, press(c('x')), Action::Play),
            (Scope::Global, press(c('c')), Action::Pause),
            (Scope::Global, press(c('v')), Action::Stop),
            (Scope::Global, press(c('b')), Action::Next),
            (Scope::Global, press(c('l')), Action::Open),
            (Scope::Global, press(c('s')), Action::ToggleShuffle),
            (Scope::Global, press(c('r')), Action::ToggleRepeat),
            // Volume: +/- anywhere
            (Scope::Global, press(c('+')), Action::VolumeUp),
            (Scope::Global, press(c('-')), Action::VolumeDown),
            // Seek: player and playlist focus
            (Scope::Player, press(KeyCode::Left), Action::SeekBack),
            (Scope::Player, press(KeyCode::Right), Action::SeekFwd),
            (Scope::Playlist, press(KeyCode::Left), Action::SeekBack),
            (Scope::Playlist, press(KeyCode::Right), Action::SeekFwd),
            // EQ band selection
            (Scope::Eq, press(KeyCode::Left), Action::BandLeft),
            (Scope::Eq, press(KeyCode::Right), Action::BandRight),
            // Volume arrows in player focus
            (Scope::Player, press(KeyCode::Up), Action::VolumeUp),
            (Scope::Player, press(KeyCode::Down), Action::VolumeDown),
            // Cursor in playlist focus
            (Scope::Playlist, press(KeyCode::Up), Action::CursorUp),
            (Scope::Playlist, press(KeyCode::Down), Action::CursorDown),
            // EQ band gain in eq focus
            (Scope::Eq, press(KeyCode::Up), Action::BandUp),
            (Scope::Eq, press(KeyCode::Down), Action::BandDown),
            // Playlist controls
            (Scope::Playlist, press(KeyCode::Enter), Action::Select),
            (Scope::Playlist, press(KeyCode::PageUp), Action::PageUp),
            (Scope::Playlist, press(KeyCode::PageDown), Action::PageDown),
            (Scope::Playlist, press(KeyCode::Home), Action::JumpHome),
            (Scope::Playlist, press(KeyCode::End), Action::JumpEnd),
            // Global misc
            (
                Scope::Global,
                ctrl(KeyCode::Char('t')),
                Action::ToggleTimeMode,
            ),
            (Scope::Global, press(KeyCode::Tab), Action::FocusNext),
            (Scope::Global, press(KeyCode::BackTab), Action::FocusPrev),
            (Scope::Global, press(c('1')), Action::TogglePlayer),
            (Scope::Global, press(c('2')), Action::ToggleEq),
            (Scope::Global, press(c('3')), Action::TogglePlaylist),
            (Scope::Global, shift(c('l')), Action::ToggleColumns),
            (Scope::Global, shift(c('o')), Action::CycleOrder),
            (Scope::Global, press(c('?')), Action::Help),
            (Scope::Global, press(c('q')), Action::Quit),
            (Scope::Global, ctrl(c('c')), Action::Quit),
        ];
        for (scope, key, action) in table {
            assert_eq!(
                map.resolve(*scope, *key),
                Some(*action),
                "{scope:?} + {:?} should be {:?}",
                key,
                action
            );
        }
    }

    #[test]
    fn remap_next_to_n_updates_hints_and_transport() {
        // remap `next = ["n"]`: `b` no longer maps to Next, and the transport
        // row shows `n`.
        let mut overrides = HashMap::new();
        overrides.insert("next".to_string(), vec!["n".into()]);
        let (map, messages) = Keymap::from_overrides(&overrides);
        assert!(messages.is_empty(), "{messages:?}");
        assert_eq!(
            map.resolve(Scope::Global, press(KeyCode::Char('b'))),
            None,
            "`b` no longer maps to Next"
        );
        assert_eq!(
            map.resolve(Scope::Global, press(KeyCode::Char('n'))),
            Some(Action::Next)
        );
        assert_eq!(map.hint(Action::Next).as_deref(), Some("n"));

        // The transport row (drawn from keymap.hint) shows the remapped key.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let app = crate::app::App::new(
            crate::config::Config::default(),
            map,
            crate::ui::theme::neon(),
            false,
        );
        let backend = TestBackend::new(52, 9);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| crate::ui::player::render(&app, f.area(), f.buffer_mut(), false))
            .unwrap();
        let buf = terminal.backend().buffer();
        // The transport row is the last inner row; find `n` immediately left
        // of the next glyph `▶`.
        let mut found = false;
        for y in 0..9u16 {
            let row: Vec<String> = (0..52u16)
                .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                .collect();
            for i in 0..row.len().saturating_sub(1) {
                if row[i] == "n" && row[i + 1] == "▶" {
                    found = true;
                }
            }
        }
        assert!(found, "transport row shows the remapped `n` for next");
    }

    #[test]
    fn swapping_next_and_prev_reports_no_conflict() {
        // swap `next = ["z"]`, `prev = ["b"]`: no conflict, each key lands on
        // exactly one action.
        let mut overrides = HashMap::new();
        overrides.insert("next".to_string(), vec!["z".into()]);
        overrides.insert("prev".to_string(), vec!["b".into()]);
        let (map, messages) = Keymap::from_overrides(&overrides);
        assert!(messages.is_empty(), "{messages:?}");
        assert_eq!(
            map.resolve(Scope::Global, press(KeyCode::Char('z'))),
            Some(Action::Next)
        );
        assert_eq!(
            map.resolve(Scope::Global, press(KeyCode::Char('b'))),
            Some(Action::Prev)
        );
    }
}
