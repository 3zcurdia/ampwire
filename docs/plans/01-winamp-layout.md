# Plan 01 — Basic layout (UI shell, no audio)

Goal: a Ratatui TUI with Winamp 2.x's *anatomy* (player, equalizer, playlist)
dressed in a **btop-style, cyberpunk-retro** look. Everything runs from the
keyboard and you quit with `q`. This phase uses **mock player state** only: no
audio, no Jellyfin, no file loading. The UI reads a snapshot type, so the real
`player`/`audio` threads can replace the mock later without touching render
code.

## 1. Look & feel decisions

| Topic | Decision |
|---|---|
| Panel borders | Thin **rounded** borders (`╭─╮ ╰─╯`), btop style. |
| Panel headers | **Hotkey + label** set into the top border: `¹player`, `²eq`, `³playlist`. No branding and no title text. |
| Window buttons | **None.** No close, minimize, or shade buttons. `q` quits immediately, like btop. |
| Bottom borders | Can hold btop-style key hints and status, e.g. `╰─ a add ─ d del ─ / find ── 48 · 3:12:40 ─╯`. |
| Retro feel | Comes from the panel contents: half-block LCD time digits, spectrum bars, scrolling marquee, Winamp transport glyphs, and EQ sliders. |
| Default theme | **Neon orange as the main color, aqua as the accent** (§3), on the terminal's own background (transparent). |
| Theming | Built-in presets, custom colors in the config, and **auto-follow of the active Omarchy theme** with live reload (§3.3). |
| Input | **Keyboard only** in this phase. Every action has its own key and every key can be remapped. Defaults follow Winamp, with Space for play/pause (§5). |
| Layout | **Two columns by default**: player and EQ on the left at a fixed natural width, the playlist filling the rest. A single-column mode and panel order can be switched by hotkey, and the choice is saved (§2). |

## 2. Layout

### 2.1 Variants

```
two-column (default)       two-column, swapped        single column
╭──────╮╭─────────────╮    ╭─────────────╮╭──────╮    ╭──────────────╮
│player││             │    │             ││player│    │    player    │
╰──────╯│             │    │             │╰──────╯    ╰──────────────╯
╭──────╮│  playlist   │    │  playlist   │╭──────╮    ╭──────────────╮
│  eq  ││             │    │             ││  eq  │    │      eq      │
│      ││             │    │             ││      │    ╰──────────────╯
╰──────╯╰─────────────╯    ╰─────────────╯╰──────╯    ╭──────────────╮
                                                      │   playlist   │
                                                      ╰──────────────╯
```

- **Two-column**:
  - The left column is a fixed **52 columns**, which is the natural width of
    the player and EQ.
  - The playlist takes all remaining width and the full height.
  - Spare height in the left column goes to the EQ panel, whose sliders get
    taller and gain resolution.
- **Single column**:
  - All panels stretch to the full terminal width.
  - Player and EQ keep their natural height, and the playlist fills the rest.
  - Stretched panels scale their contents: the spectrum gets more bars, the
    seek bar gets longer, and EQ sliders spread out evenly.
- **Order**:
  - In two-column mode, the playlist column can sit on the right (default) or
    the left.
  - Within a stack, player and EQ can swap places.
  - In single-column mode, the order of the three panels can be set.
- **Visibility**: `1` `2` `3` show or hide each panel, as in btop. Hidden
  panels give their space to the others. If the playlist is hidden in
  two-column mode, the left stack renders alone.

### 2.2 Automatic fallbacks (pure function, unit-tested)

Fallbacks apply in this order:

1. **Narrow terminal.** If the width is below `52 + 40` (left column plus the
   minimum playlist width), use single column, whatever the setting.
2. **Short terminal (single column).** First shrink the playlist to its minimum
   height (5 rows), then hide the EQ.
3. **Very short terminal.** The player drops to its **compact** form: 3 rows
   with time, marquee, and seek bar.
4. **Below 40×3.** Show a centered message: `terminal too small (need 40×3)`.

Automatic fallbacks never overwrite the saved layout preference. Making the
terminal bigger brings the chosen layout back.

### 2.3 Mockup: two-column at 104×18

```
╭─¹player──────────────────────────────────────────╮╭─³playlist────────────────────────────────────────╮
│ ▶ █▀█ ▀█  ▄ ▀▀█ ▀▀█    ▂▅▇█▆▃▁▂▄▆█▇▅▃▂▁▂▄▆▇█▇▅   ││  1. Aphex Twin - Xtal                      4:51 █│
│   █▄█ ▄█▄ ▄ █▄▄ ▄▄█    ████▇▅▃▁▃▆████▇▅▃▂▄▆████  ││  2. Boards of Canada - Roygbiv             2:31 █│
│ 3. Daft Punk - Veridis Quo (5:45) *** 4. Massiv  ││▸ 3. Daft Punk - Veridis Quo                5:45 █│
│ FLAC 24/96  stereo  bit-perfect      shuf  rep   ││  4. Massive Attack - Teardrop              5:29 █│
│ ━━━━━━━━━━━━━━━●━━━━━━━━━━━━━━━━━━━━  1:23/5:45  ││  5. Portishead - Roads                     5:05 █│
│ vol ━━━━━━━━━●━━━━━    bal ━━━━━━●━━━━━━         ││  6. Burial - Archangel                     3:58 ░│
│ z |◀   x ▶   c ‖   v ■   b ▶|   l ▲              ││  7. Air - La Femme d'Argent                7:11 ░│
╰──────────────────────────────────────────────────╯│  8. Moderat - A New Error                  5:46 ░│
╭─²eq──────────────────────────────────────────────╮│  9. Bonobo - Kerala                        4:05 ░│
│ on  auto  presets     60Hz +3dB  ⠤⠒⠉⠒⠤⣀⣀⠤⠒⠉⠉⠒⠤⣀  ││ 10. Tycho - Awake                          4:44 ░│
│ +12  │   │   │   █   │   │   │   │   │   │   │   ││ 11. Röyksopp - Eple                        3:39 ░│
│      │   █   │   │   █   │   │   │   │   │   │   ││ 12. Kiasmos - Blurred EP                   6:07 ░│
│   0  █   │   █   │   │   █   │   █   │   █   │   ││ 13. Jon Hopkins - Open Eye Signal          7:48 ░│
│      │   │   │   │   │   │   █   │   █   │   █   ││ 14. Four Tet - Two Thousand                5:12 ░│
│ -12  │   │   │   │   │   │   │   █   │   │   │   ││ 15. Floating Points - Ratio                6:30 ░│
│     pre  60 170 310 600  1k  3k  6k 12k 14k 16k  ││ 16. Caribou - Sun                          4:57 ░│
╰──────────────────────────────────────────────────╯╰─ a add ─ d del ─ / find ─── 48 · 3:12:40 ────────╯
```

What each part shows:

- **Player.**
  - Play-state glyph `▶` / `‖` / `■`, then the 2-row half-block time digits.
    `Ctrl+T` toggles elapsed or remaining time, and the digits blink while
    paused.
  - Spectrum: orange bars with aqua peak caps.
  - Marquee: scrolls only when the title doesn't fit.
  - Format line (hi-fi first): codec, bit depth / sample rate, channels, and a
    `bit-perfect` flag. `shuf` and `rep` light up aqua when on.
  - Seek bar with elapsed / total time, then volume and balance sliders.
  - Transport row with the **hotkey letter drawn in accent** before each glyph.
- **EQ.**
  - `on` / `auto` / `presets` toggles, plus a readout of the focused band.
  - Braille curve of the 10 band values.
  - Preamp plus 10 band sliders at ±12 dB. The focused band's knob is aqua.
- **Playlist.**
  - `▸` and aqua text mark the playing track. The cursor row gets a
    `muted`-tinted background.
  - Durations are right-aligned and titles are truncated by display width.
  - The scrollbar appears only when the list overflows.
  - The bottom border shows key hints, track count, and total time.

The focused panel's border and label use `glow`. Unfocused panels use `muted`.

### 2.4 Glyph rules

- Use only non-emoji code points that are 1 cell wide: box drawing, block
  elements (`▀▄█▁…▇░`), braille, the superscripts `¹²³`, and a short vetted
  icon list: `▶ ◀ ‖ ■ ▲ ▸ ● ━`.
- Never use emoji-presentation characters (`⏮ ⏸ ⏹ ⚡`). Their width differs
  between terminals.
- Don't require a Nerd Font.
- A unit test uses `unicode-width` to assert that every glyph constant is
  exactly 1 cell wide.

## 3. Style guide: cyberpunk retro

### 3.1 Semantic color roles

Widgets never use raw colors. They ask the `Theme` for a **role**:

| Role | Used for |
|---|---|
| `bg` | Panel background. `Color::Reset` by default, so the terminal or Omarchy transparency shows through. |
| `fg` | Body text such as playlist entries and labels. |
| `primary` | Borders of unfocused-but-visible chrome, LCD digits, slider tracks, spectrum bars. |
| `glow` | Focused panel border and label, plus the hotkey superscripts. |
| `accent` | "Live" items: playing track, knobs, spectrum peaks, enabled toggles, hotkey letters. |
| `muted` | Inactive borders, dim labels, unlit segments, and the cursor-row background tint. |
| `warn` | Errors and the "too small" message. |
| `viz` gradient | Spectrum colors, from low through mid to high plus a peak color. Derived from `primary`, `glow`, and `accent`. |

### 3.2 Default preset: `neon` (orange main, aqua accent)

| Role | Hex | |
|---|---|---|
| `primary` | `#FF6A00` | neon orange |
| `glow` | `#FF9E3D` | hot orange highlight |
| `accent` | `#00E5FF` | aqua |
| `muted` | `#7A3A10` | burnt orange, dim |
| `fg` | `#FFD9B8` | warm off-white |
| `warn` | `#FF2E63` | neon pink-red |
| `bg` | `Reset` | terminal background |

Other built-in presets show the theme system works. They are cheap to add:

- `amber`: single-color amber CRT.
- `matrix`: green-on-black, a nod to the classic Winamp LCD.
- `synthwave`: magenta with cyan.

Users can override any role in the config:

```toml
[theme]
preset = "neon"          # or "omarchy" (auto-follow, see 3.3), "amber", ...
transparent = true       # false → paint `bg`

[theme.colors]           # optional per-role overrides
accent = "#00E5FF"
```

Color depth:

- If `COLORTERM` isn't `truecolor` or `24bit`, roles are mapped to the nearest
  256-color index.
- In 16-color terminals, roles map to ANSI slots: orange → color3/11, aqua →
  color6/14.

### 3.3 Omarchy auto-follow (`preset = "omarchy"`)

> **Resolved (implemented per this research).** In `omacom/omarchy` (branch
> `quattro`, checked 2026-02), `bin/omarchy-theme-set` sets `CURRENT_THEME_PATH`
> to **`~/.local/state/omarchy/current/theme`** — a real directory swapped in
> atomically with `rm -rf` + `mv`, not a symlink under `~/.config`. Ampwire
> probes that path, falls back to the older `~/.config/omarchy/current/theme`,
> and honors `AMPWIRE_OMARCHY_DIR` for tests.
>
> Also: `colors.toml` uses **named** keys (`accent`, `muted`, `background`,
> `foreground`, `red`, `cyan`, `bright_*`, …), not `color0`–`color15`. The
> role mapping below uses the named keys, with `colorN` as a legacy fallback.

- **Source.** Read `colors.toml` from the active theme directory above.
- **Path conflict.** One guide put the generated theme at
  `~/.local/state/omarchy/current/theme` — confirmed correct; the
  `~/.config/omarchy/current/theme` variant is probed as a fallback.
- **Role mapping.**

  | Ampwire role | Omarchy source |
  |---|---|
  | `primary` | `accent` |
  | `glow` | lightened `accent` |
  | `accent` | `cyan` (or `bright`-ish alternative if too close to `primary`) |
  | `muted` | theme `muted`, or `color8` |
  | `fg` | `foreground` |
  | `warn` | `red`, or `color1` |
  | `bg` | stays transparent unless `transparent = false` |

- **Live reload.** Once a second on the tick, check the symlink target and the
  file's mtime. A change re-maps the theme on the next frame, with no restart
  needed. This avoids a file-watcher dependency.
- **Fallback.** If Omarchy isn't present (e.g. on macOS) or the parse fails,
  use `neon` silently and log the reason.
- **Testing.** An `AMPWIRE_OMARCHY_DIR` env var points at a fixture theme
  directory, so this can be tested on macOS.

## 4. Configuration and persistence

- **Location.** `$XDG_CONFIG_HOME/ampwire/config.toml`, falling back to
  `~/.config/ampwire/config.toml`. This applies on macOS too, matching btop
  and Omarchy conventions.
- **Contents.**
  - `[layout]`: `columns = 2`, `playlist_side = "right"`,
    `stack = ["player", "eq"]`, `single_order = ["player", "eq", "playlist"]`,
    `visible = ["player", "eq", "playlist"]`.
  - `[theme]` (§3), `[keys]` (§5).
- **Saving.** Layout hotkeys write the config right away: write a temp file,
  then rename it. Use **`toml_edit`** so the user's comments and formatting
  survive.
- **Robustness.** A missing or invalid config never crashes the app. Defaults
  apply, and a one-line notice appears in the playlist's bottom border.
- **New dependencies:** `serde`, `toml`, `toml_edit`, and `unicode-width`
  (dev). Nothing platform-specific is needed for this phase.

## 5. Keys (all remappable)

Defaults follow Winamp. Space is play/pause.

| Key | Action | Scope |
|---|---|---|
| `Space` | play / pause | global |
| `z` `x` `c` `v` `b` | prev / play / pause / stop / next | global |
| `l` | open / eject (stub in this phase) | global |
| `s` / `r` | shuffle / repeat | global |
| `←` / `→` | seek −5 s / +5 s | player, playlist |
| `←` / `→` | select EQ band | eq |
| `↑` / `↓` | volume + / − | player |
| `↑` / `↓` | move cursor | playlist |
| `↑` / `↓` | band gain ± 1 dB | eq |
| `+` / `-` | volume (works in any focus) | global |
| `Enter` | play track under cursor | playlist |
| `PgUp` `PgDn` `Home` `End` | page / jump | playlist |
| `Ctrl+T` | elapsed ↔ remaining time | global |
| `Tab` / `Shift+Tab` | focus next / previous panel | global |
| `1` `2` `3` | show / hide player, eq, playlist | global |
| `L` | toggle 1 ↔ 2 columns (saved) | global |
| `O` | cycle panel order (saved) | global |
| `?` | key-help overlay, generated from the live keymap | global |
| `q`, `Ctrl+C` | quit immediately | global |

Remapping looks like this:

```toml
[keys]
play_pause = ["space"]
next       = ["b", "n"]
toggle_columns = ["shift+l"]
```

- Keys are parsed into crossterm `KeyEvent`s.
- If two actions share a key in the same scope, the config load reports the
  conflict and the defaults are kept for those actions.
- The `?` overlay and the hotkey letters drawn in the UI read from the active
  keymap, so a remap shows up everywhere.

## 6. Module layout

```
src/
  main.rs              ratatui::run(|t| App::new(cfg, mock).run(t))
  app.rs               App: UiState + snapshot source, event loop, tick, dispatch
  action.rs            enum Action (every remappable command)
  keymap.rs            defaults, config parsing, (scope, KeyEvent) → Action
  config.rs            load / save config.toml (toml_edit), XDG paths
  player/
    mod.rs             PlayerSnapshot (view model) + PlayerCommand
    mock.rs            MockPlayer: fake playlist, ticking clock, spectrum bins
  ui/
    mod.rs             render(frame, &App)
    layout.rs          pure fn compute_layout(area, &LayoutPrefs, &Visibility)
    theme/
      mod.rs           Theme (roles), presets, 256/16-color downgrade
      omarchy.rs       colors.toml parsing, role mapping, change detection
    chrome.rs          rounded panel with superscript hotkey label + bottom hints
    player.rs          full and compact variants
    equalizer.rs
    playlist.rs
    help.rs            `?` overlay
    widgets/
      lcd_digits.rs    2-row half-block digits
      marquee.rs
      spectrum.rs      eighth-block bars with peak caps
      slider.rs        HSlider / VSlider
      eq_curve.rs      braille polyline
```

Rules:

- **Widgets** are small borrowed structs that implement
  `ratatui::widgets::Widget`. They own no state.
- **Data flow.** `ui` only reads `&PlayerSnapshot`, `&UiState`, and `&Theme`.
  Changes always go `Action → App::dispatch → PlayerCommand`. In this phase the
  commands go to `MockPlayer`; later they go to a channel to the audio thread.
- **`UiState`** holds the focused panel, visibility, layout prefs, playlist
  cursor and scroll, time mode, marquee offset, EQ band values, and the
  help-overlay flag.
- **crossterm.** Import it as `ratatui::crossterm` so the version always
  matches ratatui's, and drop the direct `crossterm` dependency.

## 7. Event loop

```text
loop {
    terminal.draw(|f| ui::render(f, &app))?;
    if event::poll(time_until_next_frame)? {        // ~33 ms → 30 fps
        Key | Resize → keymap::resolve(focus, key) → app.dispatch(action)
    }
    if tick_due { app.tick() }   // marquee, mock clock, spectrum, omarchy check (1 Hz)
    if app.should_quit { break }
}
```

- `ratatui::run` handles raw mode, the alternate screen, and restoring the
  terminal on panic. No mouse capture is needed in this phase.
- Draw on every pass. Ratatui diffs the buffers, so an unchanged frame is
  cheap. Layout is recomputed from `frame.area()` every frame, so resizing
  needs no special handling.

## 8. Milestones

Each milestone ends green on `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, and `cargo test`, plus a manual
`cargo run` in a real TTY.

**M0 — Skeleton**
- Loop with `q` / `Ctrl+C` quit, the `PlayerSnapshot` and `MockPlayer` types,
  and 15–50 fake tracks.
- ✅ Starts and quits cleanly. The terminal is restored after a forced panic.

**M1 — Theme and chrome**
- `Theme` roles, the `neon` preset and the other built-in presets, and the
  color-depth downgrade.
- Rounded panel with superscript label, focused (`glow`) and unfocused
  (`muted`) styles, and bottom-border hints.
- Glyph-width test.
- ✅ Three empty, labelled panels in the default colors. Tab moves the glow.

**M2 — Layout engine and config**
- `compute_layout` covering every variant and fallback in §2, with table tests
  at 160×45, 104×18, 91×30, 80×24, 52×12, and 40×3.
- `L`, `O`, and `1` `2` `3` work and are saved through `toml_edit`.
  Fallbacks never overwrite the saved preference.
- ✅ Resizing and toggling behave as §2 describes. The config round-trips with
  its comments intact.

**M3 — Player panel**
- LCD digits, state glyph, spectrum, marquee, format line, sliders, and the
  transport row with accent hotkey letters.
- Full and compact variants, driven by the tick.
- ✅ Matches the §2.3 mockup in a `TestBackend` buffer test. Animation is
  smooth at 30 fps with low idle CPU.

**M4 — EQ panel**
- 11 `VSlider`s that grow with the available height, the braille curve, the
  toggles, and band focus and adjust.
- ✅ Keyboard adjustment updates the sliders, curve, and readout.

**M5 — Playlist panel**
- Playing and cursor styles, scroll that follows the cursor, a scrollbar only
  when the list overflows, and bottom-border totals.
- ✅ Truncation is correct for CJK and emoji titles: by display width, never
  splitting a character.

**M6 — Keymap**
- Full default keymap, `[keys]` remapping with conflict detection, the `?`
  overlay, and on-screen hints driven by the keymap.
- ✅ A table test maps `(scope, KeyEvent)` to `Action`. A remapped key shows up
  in the UI hints.

**M7 — Omarchy**
- Fixture-based parser tests, role mapping, the 1 Hz change check, and the
  fallback to `neon`.
- ✅ Switching the fixture theme recolors the running app within about a
  second.

**M8 — Polish**
- A `--demo` flag, and final visual tuning in Ghostty, kitty, Alacritty,
  iTerm2, Terminal.app, and inside tmux.

## 9. Testing strategy

- **Layout**: `compute_layout` is pure, so its tests are `assert_eq!` on
  `Rect`s.
- **Widgets and panels**: render into a `Buffer` or `TestBackend` and compare
  with `Buffer::with_lines`. Check colors on key cells only: the focused
  border, the playing row, and the knobs.
- **Glyphs**: the `unicode-width` check over all glyph constants.
- **Keymap and config**: parse, conflict, and round-trip tests with
  `toml_edit`.
- **Omarchy**: fixture directory through `AMPWIRE_OMARCHY_DIR`.
- **Manual**: `cargo run` only in a real TTY, never in pipes or CI.

## 10. Out of scope (later plans)

- Real audio (Symphonia + cpal/rodio), gapless playback, and the bit-perfect
  path. The `bit-perfect` flag is mock data for now.
- Jellyfin client and library browser.
- Adding and removing tracks from the file system (`a` / `d` / `/` are stubs).
- Mouse support.
- Media keys and MPRIS / macOS Now Playing.

## 11. Resolved open points

1. **Omarchy theme path.** ✅ Confirmed in the Omarchy repo: the live theme
   directory is `~/.local/state/omarchy/current/theme` (a real directory,
   atomically swapped), and `colors.toml` uses named keys. See §3.3.
2. **Panel order.** ✅ Fixed presets: `O` cycles playlist right ↔ left and
   swaps the player/EQ stack (four combinations in two-column mode).
