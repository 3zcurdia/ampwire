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
    /// `[jellyfin]` (plan 08): `None` when the section is absent, so `j`
    /// can tell "not configured" from "configured".
    pub jellyfin: Option<JellyfinConfig>,
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

/// How a Jellyfin track is fetched (plan 08). Unknown strings in the config
/// downgrade to [`Cache`](JellyfinMode::Cache) with a notice (see the `From`
/// conversion below), so a typo never fails the whole config parse.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JellyfinMode {
    /// Download to a temp dir, 3 remote tracks ahead/behind (the default).
    #[default]
    Cache,
    /// Stream the original file over HTTP `Range` requests, store nothing.
    Stream,
    /// Stream, and bypass the EQ while a remote track plays.
    StreamPure,
}

/// `[jellyfin]`: the server, the key, and how tracks are fetched.
///
/// Never written back — `Config::save` leaves the table untouched (the key
/// lives only in the user's own file) — and never printed: the `Debug` impl
/// below redacts `api_key`.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(from = "JellyfinConfigRepr")]
pub struct JellyfinConfig {
    /// e.g. `http://192.168.1.10:8096`; trailing slash trimmed.
    pub url: String,
    /// API key from the Jellyfin dashboard. Header only, never a URL.
    /// After loading it holds the *effective* key: `AMPWIRE_JELLYFIN_API_KEY`
    /// beats `api_key_file`, which beats `api_key` (audit D6).
    pub api_key: String,
    /// A file holding the key (whitespace trimmed), so it can stay out of
    /// the config this app rewrites; relative paths are relative to the
    /// config file's directory.
    pub api_key_file: Option<std::path::PathBuf>,
    /// Optional user name; the first user is used when omitted.
    pub user: Option<String>,
    pub mode: JellyfinMode,
    /// `cache` mode only: how many *remote* tracks to prefetch ahead.
    pub cache_ahead: usize,
    pub cache_behind: usize,
    /// Parent of `ampwire-jellyfin-<pid>`; the system temp dir by default.
    pub cache_dir: Option<std::path::PathBuf>,
    /// One-line notice when the file held an unrecognized `mode`.
    #[serde(skip)]
    pub notice: Option<String>,
}

impl JellyfinConfig {
    /// A usable client needs at least a URL and a key.
    pub fn is_complete(&self) -> bool {
        !self.url.trim().is_empty() && !self.api_key.trim().is_empty()
    }
}

impl std::fmt::Debug for JellyfinConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JellyfinConfig")
            .field("url", &self.url)
            .field("api_key", &"<redacted>")
            .field("api_key_file", &self.api_key_file)
            .field("user", &self.user)
            .field("mode", &self.mode)
            .field("cache_ahead", &self.cache_ahead)
            .field("cache_behind", &self.cache_behind)
            .field("cache_dir", &self.cache_dir)
            .finish_non_exhaustive()
    }
}

/// What the TOML holds: every field optional/defaulted, `mode` as a raw
/// string so a typo can be reported instead of rejected (serde would abort
/// the whole config on an unknown enum variant).
#[derive(Deserialize)]
#[serde(default)]
struct JellyfinConfigRepr {
    url: String,
    api_key: String,
    api_key_file: Option<std::path::PathBuf>,
    user: Option<String>,
    mode: Option<String>,
    cache_ahead: usize,
    cache_behind: usize,
    cache_dir: Option<std::path::PathBuf>,
}

impl Default for JellyfinConfigRepr {
    fn default() -> Self {
        Self {
            url: String::new(),
            api_key: String::new(),
            api_key_file: None,
            user: None,
            mode: None,
            cache_ahead: 3,
            cache_behind: 3,
            cache_dir: None,
        }
    }
}

impl From<JellyfinConfigRepr> for JellyfinConfig {
    fn from(repr: JellyfinConfigRepr) -> Self {
        let (mode, notice) = match repr.mode.as_deref() {
            None | Some("cache") => (JellyfinMode::Cache, None),
            Some("stream") => (JellyfinMode::Stream, None),
            Some("stream_pure") => (JellyfinMode::StreamPure, None),
            Some(other) => (
                JellyfinMode::Cache,
                Some(format!("jellyfin: unknown mode \"{other}\", using cache")),
            ),
        };
        Self {
            url: repr.url.trim_end_matches('/').to_string(),
            api_key: repr.api_key,
            api_key_file: repr.api_key_file,
            user: repr.user,
            mode,
            cache_ahead: repr.cache_ahead,
            cache_behind: repr.cache_behind,
            cache_dir: repr.cache_dir,
            notice,
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
        let mut loaded = Self::load_from(&path);
        apply_env_api_key(&mut loaded.config, std::env::var(API_KEY_ENV).ok());
        loaded
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
                Ok(mut config) => {
                    // A bad `[jellyfin] mode` is downgraded to `cache` with a
                    // one-line notice; everything else still loads. So does
                    // an unreadable `api_key_file`.
                    let key_notice = read_api_key_file(&mut config, path);
                    let notice = key_notice
                        .or_else(|| config.jellyfin.as_ref().and_then(|jf| jf.notice.clone()));
                    LoadedConfig {
                        config: sanitize(config),
                        notice,
                        read_only: false,
                    }
                }
                Err(err) => LoadedConfig {
                    config: Config::default(),
                    notice: Some(format!(
                        "config invalid — not saving ({})",
                        describe_parse_error(&err, &text)
                    )),
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
        // The file may hold `[jellyfin] api_key` (README: `chmod 600`), so
        // the replacement inherits the original's permissions — set on the
        // temp file *before* any byte is written, never widened by the
        // rename. A stale temp file from a crashed save is removed first:
        // its mode is unknown.
        let original = fs::metadata(path).ok().map(|meta| meta.permissions());
        let _ = fs::remove_file(&tmp);
        {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            if let Some(perms) = &original {
                use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
                options.mode(perms.mode() & 0o777);
            }
            let mut file = options.open(&tmp)?;
            if let Some(perms) = original {
                // `mode()` above is filtered by the umask; this is exact.
                file.set_permissions(perms)?;
            }
            file.write_all(doc.to_string().as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&tmp, path)
    }
}

/// The environment variable that supplies the Jellyfin key (audit D6).
pub const API_KEY_ENV: &str = "AMPWIRE_JELLYFIN_API_KEY";

/// `[jellyfin] api_key_file` → `api_key` (trimmed). Relative paths resolve
/// against the config file's directory. Returns the notice for a file that
/// cannot be read — never its path's contents, and the key stays as it was.
fn read_api_key_file(config: &mut Config, config_path: &Path) -> Option<String> {
    let jf = config.jellyfin.as_mut()?;
    let file = jf.api_key_file.as_ref()?;
    let file = match config_path.parent() {
        Some(dir) if file.is_relative() => dir.join(file),
        _ => file.clone(),
    };
    match fs::read_to_string(file) {
        Ok(key) if !key.trim().is_empty() => {
            jf.api_key = key.trim().to_string();
            None
        }
        _ => Some("jellyfin: cannot read api_key_file".into()),
    }
}

/// `AMPWIRE_JELLYFIN_API_KEY`, when set and not blank, replaces the key of
/// an existing `[jellyfin]` section (it never creates one: a key without a
/// URL configures nothing). Never written back by [`Config::save`].
pub fn apply_env_api_key(config: &mut Config, value: Option<String>) {
    if let (Some(jf), Some(value)) = (config.jellyfin.as_mut(), value)
        && !value.trim().is_empty()
    {
        jf.api_key = value.trim().to_string();
    }
}

/// `line 3, column 24: invalid basic string` — where and what, on one line.
/// toml's own `Display` quotes the offending source line, and serde quotes
/// bad values in backticks; either can be the `[jellyfin] api_key`, so
/// neither reaches the notice (audit S3).
fn describe_parse_error(err: &toml::de::Error, text: &str) -> String {
    let first_line = err.message().lines().next().unwrap_or("").trim();
    // Odd segments sit between backticks: a quoted value.
    let message: String = first_line
        .split('`')
        .enumerate()
        .map(|(i, part)| if i % 2 == 1 { "`…`" } else { part })
        .collect();
    match err.span() {
        Some(span) => {
            let before = text.get(..span.start).unwrap_or(text);
            let line = before.matches('\n').count() + 1;
            let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
            format!("line {line}, column {column}: {message}")
        }
        None => message,
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
    // Jellyfin cache window: the plan's 0–10 range (defaults 3/3).
    if let Some(jf) = config.jellyfin.as_mut() {
        jf.cache_ahead = jf.cache_ahead.min(10);
        jf.cache_behind = jf.cache_behind.min(10);
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

    /// Audit S3: toml's own error text quotes the offending source line —
    /// which can be the `api_key` line. The notice must carry where and
    /// what, on one line, never the source text (AGENTS: the key is never
    /// in a notice).
    #[test]
    fn a_parse_error_notice_never_quotes_the_config_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[jellyfin]\nurl = \"http://x\"\napi_key = \"SECRETKEY123\ncache_ahead = 3\n",
        )
        .unwrap();
        let loaded = Config::load_from(&path);
        assert!(loaded.read_only);
        let notice = loaded.notice.expect("a notice");
        assert!(!notice.contains("SECRETKEY123"), "key leaked: {notice}");
        assert!(!notice.contains('\n'), "one line: {notice:?}");
        assert!(
            notice.starts_with("config invalid — not saving"),
            "{notice}"
        );
        assert!(notice.contains("line 3"), "says where: {notice}");

        // A type error quotes the bad *value* — redacted the same way.
        fs::write(&path, "[jellyfin]\napi_key = 12345678\n").unwrap();
        let notice = Config::load_from(&path).notice.expect("a notice");
        assert!(!notice.contains("12345678"), "value leaked: {notice}");
        assert!(notice.contains("line 2"), "{notice}");
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

    // --- `[jellyfin]` (plan 08, step 7) ---

    const JF_TOML: &str = "\
# my server
[jellyfin]
url = \"http://jellyfin.lan:8096/\" # trailing slash
api_key = \"0123456789abcdef\"
user = \"luis\"
mode = \"stream\"
cache_ahead = 5
cache_behind = 2
";

    /// Audit D6: the key can live outside the auto-saved config — in a file
    /// of its own (`api_key_file`, relative to the config's directory) …
    #[test]
    fn api_key_file_supplies_the_key_and_is_never_saved_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(dir.path().join("jellyfin.key"), "  filekey123\n").unwrap();
        fs::write(
            &path,
            "[jellyfin]\nurl = \"http://x\"\napi_key_file = \"jellyfin.key\"\n",
        )
        .unwrap();
        let loaded = Config::load_from(&path);
        assert_eq!(loaded.notice, None);
        let jf = loaded.config.jellyfin.clone().expect("section");
        assert_eq!(jf.api_key, "filekey123", "trimmed file contents");
        assert!(jf.is_complete());
        assert!(
            !format!("{jf:?}").contains("filekey123"),
            "Debug redacts it"
        );

        let mut config = loaded.config;
        config.audio.volume = 40;
        config.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("filekey123"), "never written back: {text}");
        assert!(text.contains("api_key_file = \"jellyfin.key\""), "{text}");
    }

    #[test]
    fn an_unreadable_api_key_file_is_a_notice_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[jellyfin]\nurl = \"http://x\"\napi_key_file = \"missing.key\"\n",
        )
        .unwrap();
        let loaded = Config::load_from(&path);
        assert!(!loaded.read_only, "the rest of the config still loads");
        assert_eq!(
            loaded.notice.as_deref(),
            Some("jellyfin: cannot read api_key_file")
        );
        assert!(!loaded.config.jellyfin.unwrap().is_complete());
    }

    /// … or in the environment, which wins over both config fields.
    #[test]
    fn the_environment_key_overrides_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, JF_TOML).unwrap();
        let mut loaded = Config::load_from(&path);
        apply_env_api_key(&mut loaded.config, Some("envkey456".into()));
        assert_eq!(
            loaded.config.jellyfin.as_ref().unwrap().api_key,
            "envkey456"
        );
        // Unset or blank: the config's key stands.
        apply_env_api_key(&mut loaded.config, Some("  ".into()));
        apply_env_api_key(&mut loaded.config, None);
        assert_eq!(
            loaded.config.jellyfin.as_ref().unwrap().api_key,
            "envkey456"
        );
        // No `[jellyfin]` section: the variable alone configures nothing.
        let mut bare = Config::default();
        apply_env_api_key(&mut bare, Some("envkey456".into()));
        assert!(bare.jellyfin.is_none());
    }

    #[test]
    fn jellyfin_section_parses_and_normalizes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, JF_TOML).unwrap();
        let loaded = Config::load_from(&path);
        assert!(loaded.notice.is_none(), "{:?}", loaded.notice);
        let jf = loaded.config.jellyfin.expect("[jellyfin] parsed");
        assert_eq!(jf.url, "http://jellyfin.lan:8096", "slash trimmed");
        assert_eq!(jf.api_key, "0123456789abcdef");
        assert_eq!(jf.user.as_deref(), Some("luis"));
        assert_eq!(jf.mode, JellyfinMode::Stream);
        assert_eq!(jf.cache_ahead, 5);
        assert_eq!(jf.cache_behind, 2);
        assert!(jf.is_complete());
    }

    #[test]
    fn missing_jellyfin_section_is_none_and_defaults_are_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[theme]\npreset = \"amber\"\n").unwrap();
        assert!(Config::load_from(&path).config.jellyfin.is_none());

        // A section without `mode` (or the other optional keys) is cache mode.
        fs::write(
            &path,
            "[jellyfin]\nurl = \"http://x:8096\"\napi_key = \"k\"\n",
        )
        .unwrap();
        let jf = Config::load_from(&path).config.jellyfin.expect("parsed");
        assert_eq!(jf.mode, JellyfinMode::Cache);
        assert_eq!((jf.cache_ahead, jf.cache_behind), (3, 3), "plan defaults");
        assert_eq!(jf.user, None);
    }

    #[test]
    fn unknown_mode_falls_back_to_cache_with_a_notice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[jellyfin]\nurl = \"http://x:8096\"\napi_key = \"k\"\nmode = \"turbo\"\n",
        )
        .unwrap();
        let loaded = Config::load_from(&path);
        // The rest of the config survives; only the mode is downgraded.
        assert!(!loaded.read_only, "a bad mode must not kill the config");
        let jf = loaded.config.jellyfin.expect("still parsed");
        assert_eq!(jf.mode, JellyfinMode::Cache);
        let notice = loaded.notice.expect("mode notice");
        assert_eq!(notice, "jellyfin: unknown mode \"turbo\", using cache");
    }

    #[test]
    fn save_leaves_the_jellyfin_table_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, JF_TOML).unwrap();

        let mut config = Config::load_from(&path).config;
        config.layout.columns = 1;
        config.audio.volume = 70;
        config.save(&path).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[jellyfin]"), "{text}");
        assert!(text.contains("api_key = \"0123456789abcdef\""), "{text}");
        assert!(text.contains("# trailing slash"), "{text}");
        assert!(text.contains("columns = 1"), "{text}");
        // …and it reloads to the same thing (the key round-trips).
        let reloaded = Config::load_from(&path).config;
        assert_eq!(reloaded.jellyfin, config.jellyfin);
    }

    /// Audit S2: the README says `chmod 600 config.toml` (it holds the API
    /// key); a debounced save must not hand back a world-readable file.
    #[cfg(unix)]
    #[test]
    fn save_keeps_the_files_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, JF_TOML).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let mut config = Config::load_from(&path).config;
        config.audio.volume = 70;
        config.save(&path).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "save widened the mode to {mode:o}");
        // A sibling temp file is never left behind either.
        assert!(!path.with_extension("toml.tmp").exists());
    }

    #[test]
    fn jellyfin_debug_redacts_the_api_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, JF_TOML).unwrap();
        let config = Config::load_from(&path).config;
        let printed = format!("{config:?}");
        assert!(
            !printed.contains("0123456789abcdef"),
            "the key must never appear in a Debug print: {printed}"
        );
        assert!(printed.contains("<redacted>"), "{printed}");
        // The same holds for the section on its own.
        let jf = config.jellyfin.expect("parsed");
        assert!(!format!("{jf:?}").contains("0123456789abcdef"));
    }

    #[test]
    fn out_of_range_cache_windows_are_clamped_to_ten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[jellyfin]\nurl = \"http://x:8096\"\napi_key = \"k\"\ncache_ahead = 99\ncache_behind = 50\n",
        )
        .unwrap();
        let jf = Config::load_from(&path).config.jellyfin.expect("parsed");
        assert_eq!(jf.cache_ahead, 10);
        assert_eq!(jf.cache_behind, 10);
    }
}
