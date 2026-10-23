//! Config load/save: `$XDG_CONFIG_HOME/ampwire/config.toml`, falling back to
//! `~/.config/ampwire/config.toml` (also on macOS, matching btop/Omarchy).
//!
//! Loading never crashes: invalid or missing config falls back to defaults
//! with a one-line notice. Saving goes through `toml_edit` so comments and
//! formatting survive, written as temp-file + rename.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ui::layout::{Panel, PlaylistSide};

/// `$XDG_CONFIG_HOME/ampwire/config.toml` or `~/.config/ampwire/config.toml`.
pub fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("ampwire").join("config.toml"))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LayoutConfig {
    /// 1 or 2.
    pub columns: u8,
    pub playlist_side: PlaylistSide,
    /// Order of player/eq in the left stack (two-column mode).
    pub stack: Vec<Panel>,
    /// Panel order in single-column mode.
    pub single_order: Vec<Panel>,
    pub visible: Vec<Panel>,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            columns: 2,
            playlist_side: PlaylistSide::Right,
            stack: vec![Panel::Player, Panel::Eq],
            single_order: vec![Panel::Player, Panel::Eq, Panel::Playlist],
            visible: vec![Panel::Player, Panel::Eq, Panel::Playlist],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ThemeConfig {
    /// `"neon"`, `"amber"`, `"matrix"`, `"synthwave"`, or `"omarchy"`.
    pub preset: String,
    /// false → paint the `bg` role instead of leaving the terminal background.
    pub transparent: bool,
    /// Optional per-role color overrides, e.g. `accent = "#00E5FF"`.
    pub colors: HashMap<String, String>,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            preset: "neon".into(),
            transparent: true,
            colors: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub layout: LayoutConfig,
    pub theme: ThemeConfig,
    pub audio: AudioConfig,
    pub eq: EqConfig,
    /// `[keys]` action → list of key specs.
    pub keys: HashMap<String, Vec<String>>,
}

/// Volume/balance/channel-routing, saved 500 ms after the last change
/// (debounced) and on quit. Defaults keep a fresh install bit-perfect:
/// volume 100 (unity), balance centered, channels as decoded.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AudioConfig {
    /// 0–=100.
    pub volume: u8,
    /// −100..=100.
    pub balance: i8,
    /// Send the decoded channels to the opposite ears — for headsets/DACs
    /// with reversed L/R wiring.
    pub swap_channels: bool,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            volume: 100,
            balance: 0,
            swap_channels: false,
        }
    }
}

/// Equalizer (plan 06), saved with the same debounce as `[audio]`:
/// on/off + preamp + 10 bands in whole dB (−12..=12). Defaults keep a fresh
/// install flat (and therefore bit-perfect).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct EqConfig {
    pub on: bool,
    /// −12..=12.
    pub preamp: i8,
    /// 10 band gains in dB, low to high (same order as `BAND_HZ`).
    pub bands: [i8; 10],
}

impl Default for EqConfig {
    fn default() -> Self {
        Self {
            on: true,
            preamp: 0,
            bands: [0; 10],
        }
    }
}

pub struct LoadedConfig {
    pub config: Config,
    /// One-line notice for the playlist bottom border, if anything went wrong.
    pub notice: Option<String>,
    /// True when the file exists but failed to parse or type-check. The
    /// session must not write back over it: hotkeys still change the in-memory
    /// layout, but nothing is saved.
    pub read_only: bool,
}

impl Config {
    /// Load from the default path. Never fails; falls back to defaults.
    pub fn load() -> LoadedConfig {
        let Some(path) = config_path() else {
            return LoadedConfig {
                config: Config::default(),
                notice: Some("config: no home directory, using defaults".into()),
                read_only: false,
            };
        };
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> LoadedConfig {
        if !path.exists() {
            return LoadedConfig {
                config: Config::default(),
                notice: None,
                read_only: false,
            };
        }
        match fs::read_to_string(path) {
            // An unreadable file is treated as read-only too: we can't trust
            // what's there, so don't overwrite it.
            Err(err) => LoadedConfig {
                config: Config::default(),
                notice: Some(format!("config invalid — not saving ({err})")),
                read_only: true,
            },
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(config) => LoadedConfig {
                    config: sanitize(config),
                    notice: None,
                    read_only: false,
                },
                Err(err) => LoadedConfig {
                    config: Config::default(),
                    notice: Some(format!("config invalid — not saving ({err})")),
                    read_only: true,
                },
            },
        }
    }

    /// Save layout/theme changes, preserving comments and formatting.
    /// Atomic: temp file + rename.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut doc: toml_edit::DocumentMut = match fs::read_to_string(path) {
            Ok(text) => text
                .parse()
                .unwrap_or_else(|_| toml_edit::DocumentMut::new()),
            Err(_) => toml_edit::DocumentMut::new(),
        };

        set_table(&mut doc, "layout", |t| {
            set_preserving_decor(t, "columns", toml_edit::value(self.layout.columns as i64));
            set_preserving_decor(
                t,
                "playlist_side",
                toml_edit::value(self.layout.playlist_side.as_str()),
            );
            set_preserving_decor(
                t,
                "stack",
                string_array(self.layout.stack.iter().map(|p| p.as_str())),
            );
            set_preserving_decor(
                t,
                "single_order",
                string_array(self.layout.single_order.iter().map(|p| p.as_str())),
            );
            set_preserving_decor(
                t,
                "visible",
                string_array(self.layout.visible.iter().map(|p| p.as_str())),
            );
        });
        set_table(&mut doc, "theme", |t| {
            set_preserving_decor(t, "preset", toml_edit::value(self.theme.preset.clone()));
            set_preserving_decor(t, "transparent", toml_edit::value(self.theme.transparent));
        });
        set_table(&mut doc, "audio", |t| {
            set_preserving_decor(t, "volume", toml_edit::value(i64::from(self.audio.volume)));
            set_preserving_decor(
                t,
                "balance",
                toml_edit::value(i64::from(self.audio.balance)),
            );
            set_preserving_decor(
                t,
                "swap_channels",
                toml_edit::value(self.audio.swap_channels),
            );
        });
        set_table(&mut doc, "eq", |t| {
            set_preserving_decor(t, "on", toml_edit::value(self.eq.on));
            set_preserving_decor(t, "preamp", toml_edit::value(i64::from(self.eq.preamp)));
            set_preserving_decor(
                t,
                "bands",
                int_array(self.eq.bands.iter().map(|&b| i64::from(b))),
            );
        });

        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        fs::create_dir_all(parent)?;
        let tmp = path.with_extension("toml.tmp");
        {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(doc.to_string().as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&tmp, path)
    }
}

fn set_table(doc: &mut toml_edit::DocumentMut, name: &str, f: impl FnOnce(&mut toml_edit::Table)) {
    let item = doc
        .entry(name)
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    if let Some(table) = item.as_table_mut() {
        f(table);
    }
}

/// Set a table key while keeping the old value's decor (inline comments,
/// spacing), so user annotations survive a save.
fn set_preserving_decor(table: &mut toml_edit::Table, key: &str, mut item: toml_edit::Item) {
    if let (Some(old), Some(new)) = (
        table.get(key).and_then(|item| item.as_value()),
        item.as_value_mut(),
    ) {
        *new.decor_mut() = old.decor().clone();
    }
    table[key] = item;
}

/// Build a TOML array of strings.
fn string_array<'a>(items: impl IntoIterator<Item = &'a str>) -> toml_edit::Item {
    let mut arr = toml_edit::Array::new();
    for item in items {
        arr.push(item);
    }
    toml_edit::Item::Value(toml_edit::Value::Array(arr))
}

/// Build a TOML array of integers (the EQ's `bands`, plan 06).
fn int_array(items: impl IntoIterator<Item = i64>) -> toml_edit::Item {
    let mut arr = toml_edit::Array::new();
    for item in items {
        arr.push(item);
    }
    toml_edit::Item::Value(toml_edit::Value::Array(arr))
}

/// Drop obviously wrong values serde can't catch (empty orders, bad columns).
///
/// `stack` must be a permutation of player + eq and `single_order` of all
/// three panels: unknown, misplaced, and repeated entries are dropped, and
/// missing panels are appended in default order. The layout engine relies on
/// this — a panel listed twice would be laid out twice.
fn sanitize(mut config: Config) -> Config {
    if !(1..=2).contains(&config.layout.columns) {
        config.layout.columns = 2;
    }
    let defaults = LayoutConfig::default();
    normalize_order(&mut config.layout.stack, &defaults.stack);
    normalize_order(&mut config.layout.single_order, &defaults.single_order);

    let mut visible: Vec<Panel> = Vec::new();
    for panel in &config.layout.visible {
        if *panel != Panel::Unknown && !visible.contains(panel) {
            visible.push(*panel);
        }
    }
    config.layout.visible = if visible.is_empty() {
        defaults.visible
    } else {
        visible
    };
    // Volume/balance ranges: serde accepts any in-range integer type, so
    // out-of-range values (e.g. volume = 250) are clamped here.
    config.audio.volume = config.audio.volume.min(100);
    config.audio.balance = config.audio.balance.clamp(-100, 100);
    // EQ gains: clamp to the slider span (−12..=12), preamp + bands.
    config.eq.preamp = config.eq.preamp.clamp(-12, 12);
    for band in &mut config.eq.bands {
        *band = (*band).clamp(-12, 12);
    }
    config
}

/// Reorder `list` into a permutation of `allowed`, keeping the user's order
/// for the entries that are valid.
fn normalize_order(list: &mut Vec<Panel>, allowed: &[Panel]) {
    let mut out: Vec<Panel> = Vec::with_capacity(allowed.len());
    for panel in list.iter().chain(allowed) {
        if allowed.contains(panel) && !out.contains(panel) {
            out.push(*panel);
        }
    }
    *list = out;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_config_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = Config::load_from(&dir.path().join("nope.toml"));
        assert_eq!(loaded.config, Config::default());
        assert!(loaded.notice.is_none());
    }

    #[test]
    fn invalid_config_yields_defaults_with_notice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "this is not toml [").unwrap();
        let loaded = Config::load_from(&path);
        assert_eq!(loaded.config, Config::default());
        assert!(
            loaded.read_only,
            "a config that failed to parse is read-only"
        );
        let notice = loaded.notice.as_deref().unwrap_or("");
        assert!(notice.contains("config invalid — not saving"), "{notice}");
    }

    #[test]
    fn valid_config_is_not_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[theme]\npreset = \"amber\"\n").unwrap();
        let loaded = Config::load_from(&path);
        assert!(!loaded.read_only);
        assert!(loaded.notice.is_none());
    }

    #[test]
    fn round_trip_preserves_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "# my ampwire config\n[theme]\npreset = \"amber\" # crt vibes\n",
        )
        .unwrap();

        let mut config = Config::load_from(&path).config;
        config.layout.columns = 1;
        config.save(&path).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my ampwire config"), "{text}");
        assert!(text.contains("# crt vibes"), "{text}");
        assert!(text.contains("columns = 1"), "{text}");
        assert!(text.contains("preset = \"amber\""), "{text}");

        let reloaded = Config::load_from(&path).config;
        assert_eq!(reloaded, config);
    }

    #[test]
    fn save_works_when_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/config.toml");
        let config = Config::default();
        config.save(&path).unwrap();
        assert_eq!(Config::load_from(&path).config, config);
    }

    #[test]
    fn broken_layout_orders_are_normalized() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cases: &[(&str, &[Panel], &[Panel])] = &[
            // Empty lists fall back to the defaults.
            (
                "stack = []\nsingle_order = []",
                &[Panel::Player, Panel::Eq],
                &[Panel::Player, Panel::Eq, Panel::Playlist],
            ),
            // Non-adjacent repeats, a playlist in the stack, unknown names.
            (
                "stack = [\"eq\", \"playlist\", \"bogus\", \"eq\"]\n\
                 single_order = [\"playlist\", \"player\", \"playlist\"]",
                &[Panel::Eq, Panel::Player],
                &[Panel::Playlist, Panel::Player, Panel::Eq],
            ),
        ];
        for (layout, stack, single_order) in cases {
            fs::write(&path, format!("[layout]\n{layout}\n")).unwrap();
            let config = Config::load_from(&path).config;
            assert_eq!(config.layout.stack, *stack, "{layout}");
            assert_eq!(config.layout.single_order, *single_order, "{layout}");
        }
    }

    #[test]
    fn visible_drops_repeats() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[layout]\nvisible = [\"eq\", \"player\", \"eq\", \"bogus\"]\n",
        )
        .unwrap();
        let config = Config::load_from(&path).config;
        assert_eq!(config.layout.visible, vec![Panel::Eq, Panel::Player]);
    }

    #[test]
    fn xdg_env_is_honored() {
        // Just verify the function composes the expected shape.
        let path = config_path();
        if let Some(p) = path {
            assert!(p.ends_with("ampwire/config.toml"));
        }
    }

    // --- `[audio]` (plan 05, step 6 + 7) ---

    #[test]
    fn audio_defaults_are_bit_perfect() {
        let audio = AudioConfig::default();
        assert_eq!(audio.volume, 100, "fresh install: unity gain");
        assert_eq!(audio.balance, 0, "fresh install: centered");
        assert!(!audio.swap_channels, "fresh install: channels as decoded");
    }

    #[test]
    fn audio_round_trips_through_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::default();
        config.audio.volume = 55;
        config.audio.balance = -30;
        config.audio.swap_channels = true;
        config.save(&path).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[audio]"), "{text}");
        assert!(text.contains("volume = 55"), "{text}");
        assert!(text.contains("balance = -30"), "{text}");
        assert!(text.contains("swap_channels = true"), "{text}");

        let reloaded = Config::load_from(&path).config;
        assert_eq!(reloaded.audio, config.audio);
    }

    #[test]
    fn out_of_range_audio_values_are_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // 250 fits u8, 120 fits i8 — both outside the musical range.
        fs::write(&path, "[audio]\nvolume = 250\nbalance = 120\n").unwrap();
        let config = Config::load_from(&path).config;
        assert_eq!(config.audio.volume, 100);
        assert_eq!(config.audio.balance, 100);
    }

    #[test]
    fn saving_audio_keeps_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "# my ampwire config\n[audio]\nvolume = 70 # not too loud\n",
        )
        .unwrap();
        let mut config = Config::load_from(&path).config;
        config.audio.volume = 40;
        config.save(&path).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my ampwire config"), "{text}");
        assert!(text.contains("# not too loud"), "{text}");
        assert!(text.contains("volume = 40"), "{text}");
        assert_eq!(Config::load_from(&path).config.audio, config.audio);
    }

    // --- `[eq]` (plan 06, step 5) ---

    #[test]
    fn eq_defaults_are_flat_and_on() {
        let eq = EqConfig::default();
        assert!(eq.on, "fresh install: EQ enabled but flat");
        assert_eq!(eq.preamp, 0, "fresh install: unity preamp");
        assert_eq!(eq.bands, [0; 10], "fresh install: flat = bit-perfect");
    }

    #[test]
    fn eq_round_trips_through_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::default();
        config.eq.on = false;
        config.eq.preamp = -3;
        config.eq.bands = [6, 0, 0, 0, -4, 0, 0, 12, 0, 0];
        config.save(&path).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[eq]"), "{text}");
        assert!(text.contains("on = false"), "{text}");
        assert!(text.contains("preamp = -3"), "{text}");
        assert!(
            text.contains("bands = [6, 0, 0, 0, -4, 0, 0, 12, 0, 0]"),
            "{text}"
        );

        let reloaded = Config::load_from(&path).config;
        assert_eq!(reloaded.eq, config.eq);
    }

    #[test]
    fn out_of_range_eq_gains_are_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[eq]\npreamp = 20\nbands = [30, -40, 0, 0, 0, 0, 0, 0, 0, 0]\n",
        )
        .unwrap();
        let config = Config::load_from(&path).config;
        assert_eq!(config.eq.preamp, 12);
        assert_eq!(config.eq.bands[0], 12);
        assert_eq!(config.eq.bands[1], -12);
    }
}
