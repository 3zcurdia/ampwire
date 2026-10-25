# Plan 01 — Basic layout (UI shell, no audio)

**Status:** ✅ done — 2026-10-22, 087a1c3. Built in phases 0–3
(`e09e6de`…`087a1c3`); `tests/golden/` holds the reference frames.
**Do not re-implement**; only fix regressions.

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
- **Visibility**: `1` `2` `3` show or hide each panel, as in btop.
  Hiding the playlist leaves the player + EQ stack alone: the playlist
  column disappears and the stack widens to the full terminal width
  (the 52-column column is a two-column-mode constraint only). See §12
  for the gap: the vertical space freed by a hidden panel is currently
  left blank.

### 2.2 Automatic fallbacks (pure function, unit-tested)

`compute_layout(area, prefs, visibility)` is pure: same inputs, same
rects. The fallbacks apply in this order:

1. **Too small.** Below 40×3 → a centered `terminal too small (need
   40×3)` message and nothing else.
2. **Narrow terminal.** Width below `52 + 40 = 92` → single column,
   whatever the setting.
3. **Two-column.** The left stack is 52 columns wide. The player keeps
   its natural height (11 rows) and the EQ takes the rest; when fewer
   than 5 rows are left for the EQ it is dropped rather than squashed.
4. **Short terminal (single column).** First shrink the playlist to its
   minimum height (5 rows), then hide the EQ.
5. **Very short terminal.** The player drops to its **compact** form:
   3 rows with time, marquee, and seek bar.

Automatic fallbacks never overwrite the saved layout preference. Making
the terminal bigger brings the chosen layout back.

#### Layout constants (`src/ui/layout.rs`)

| Constant | Value | Meaning |
|---|---|---|
| `NATURAL_LEFT_WIDTH` | 52 | Width of the player + EQ column |
| `MIN_PLAYLIST_WIDTH` | 40 | Below this, two columns don't fit |
| `MIN_WIDTH` / `MIN_HEIGHT` | 40 / 3 | Below this, only the "too small" message |
| `PLAYER_HEIGHT` | 11 | Natural player height (9 inner rows + 2 borders) |
| `COMPACT_PLAYER_HEIGHT` | 3 | Compact player |
| `EQ_HEIGHT` | 9 | Natural EQ height |
| `MIN_EQ_TOTAL` | 5 | Below this the EQ is dropped, not squashed |
| `MIN_PLAYLIST_HEIGHT` | 5 | Playlist minimum |
| `MIN_PANEL_HEIGHT` | 3 | A panel shorter than this is dropped |

### 2.3 Mockup: two-column at 104×18

This is the reference frame — byte-identical to `tests/golden/104x18.txt`,
which is produced by `cargo run -- --dump 104x18`.

```
╭─¹player──────────────────────────────────────────╮╭─³playlist────────────────────────────────────────╮
│▶ ▀█  ▄ ▀▀█ █ █   ▁▁▁▃▁▁ ▁▃▃                      ││  1. Aphex Twin - Xtal                      4:51 █│
│  ▄█▄ ▄ █▄▄ ▀▀█ ▂▂██████▇███▇▆▆▄▂▄▄▄▃▃▃  ▁ ▁▁ ▁▁▂▁││  2. Boards of Canada - Roygbiv             2:31 █│
│3. Daft Punk - Veridis Quo (5:45)                 ││▸ 3. Daft Punk - Veridis Quo                5:45 █│
│FLAC  24/96  stereo  bit-perfect      shuf  rep   ││  4. Massive Attack - Teardrop              5:29 █│
│━━━━━━━━━●━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ 1:24/5:45 ││  5. Portishead - Roads                     5:05 █│
│vol ━━━━━━━━━━━━━━●━━━━━━━  bal ━━━━━━━━━●━━━━━━━━││  6. Burial - Archangel                     3:58 █│
│╭──────╮╭──────╮╭───────╮╭──────╮╭──────╮╭───────╮││  7. Air - La Femme d'Argent                7:11 █│
││  z|◀ ││  x▶  ││   c‖  ││  v■  ││  b▶| ││   l▲  │││  8. Moderat - A New Error                  5:46 █│
│╰──────╯╰──────╯╰───────╯╰──────╯╰──────╯╰───────╯││  9. Bonobo - Kerala                        4:05 █│
╰──────────────────────────────────────────────────╯│ 10. Tycho - Awake                          4:44 █│
╭─²eq──────────────────────────────────────────────╮│ 11. Röyksopp - Eple                        3:39 █│
│on  auto  presets 60Hz +3dB  ⠒⠒⠒⠒⠒⠒⠒⠤⠤⠤⠤⠤⠤⠤⠒⠒⠒⠒⠒⠒⠒││ 12. Kiasmos - Blurred EP                   6:07 ││
│ +12│   │    │   │    │   │    │   │    │   │    │││ 13. Jon Hopkins - Open Eye Signal          7:48 ││
│   0●   ●    ●   ●    ●   ●    ●   ●    ●   ●    ●││ 14. Four Tet - Two Thousand                5:12 ││
│ -12│   │    │   │    │   │    │   │    │   │    │││ 15. Floating Points - Ratio                6:30 ││
│   pre  60  170 310  600  1k   3k  6k  12k 14k 16k││ 16. Caribou - Sun                          4:57 ││
╰──────────────────────────────────────────────────╯╰─ a add ─ d del ─ / find ─ 24 · 1:56:43 ──────────╯
```
What each part shows:

- **Player** (11 rows).
  - Play-state glyph `▶` / `‖` / `■` (accent while playing, glow
    paused, muted stopped), then the 2-row half-block time digits.
    `Ctrl+T` toggles elapsed or remaining time; the digits blink
    while paused.
  - Spectrum on the right of the digits: eighth-block bars colored from
    the theme's `viz` gradient (primary → glow → accent) with aqua peak
    caps. More columns → more bars, never stretched bars.
  - Marquee: scrolls only when the title doesn't fit, looping with
    Winamp's ` *** ` separator.
  - Format line (hi-fi first): `FLAC  24/96  stereo  bit-perfect`, with
    `shuf` and `rep` right-aligned and lit aqua when on.
  - Seek bar with elapsed / total time, then `vol` and `bal` sliders.
  - Transport: six boxed buttons (prev / play / pause / stop / next /
    eject), each with its hotkey letter drawn in accent before the
    glyph.
- **EQ** (9 rows by default, taller when the terminal has room).
  - `on` / `auto` / `presets` indicators, then a readout of the focused
    band (`60Hz +3dB`), then the braille curve of the 10 band values.
  - A left dB scale (`+12` / `0` / `-12`) and 11 sliders (preamp + 10
    bands) at ±12 dB. The focused band's knob is aqua.
  - Band labels under the sliders; labels that would collide are
    skipped rather than run together.
- **Playlist**.
  - `▸` and aqua text mark the playing track. The cursor row gets a
    `muted`-tinted background.
  - Durations are right-aligned and titles are truncated by display width.
  - The scrollbar appears only when the list overflows.
  - The bottom border shows key hints, track count, and total time:
    `─ a add ─ d del ─ / find ─ 24 · 1:56:43 ─`.

The focused panel's border and label use `glow`. Unfocused panels use
`muted`.

### 2.4 Glyph rules

- Use only non-emoji code points that are 1 cell wide: box drawing, block
  elements (`▀▄█▁…▇░`), braille, the superscripts `¹²³`, and a short vetted
  icon list: `▶ ◀ ‖ ■ ▲ ▸ ● ━`.
- Never use emoji-presentation characters (`⏮ ⏸ ⏹ ⚡`). Their width differs
  between terminals.
- Don't require a Nerd Font.
- The vetted glyphs live in `src/ui/theme/mod.rs::glyphs`. A unit test uses
  `unicode-width` to assert that every glyph constant is exactly 1 cell
  wide.

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

The `viz` gradient is 8 steps from `primary` through `glow` to `accent`, low
frequency first, with the peak cap in `accent`.

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

- **Live reload.** Once a second on the tick — and only while the preset
  is `omarchy` — the theme directory's canonical path and `colors.toml`'s
  mtime are compared. A change re-maps the theme on the next frame, with
  no restart needed and no file-watcher dependency.
- **Fallback.** If Omarchy isn't present (e.g. on macOS) or the parse
  fails, use `neon` silently. Nothing is logged.
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
  A config that failed to parse makes the session **read-only**: hotkeys
  still change the in-memory layout, but the broken file is never
  overwritten (`config invalid — not saving`).
- **Sanitizing.** Unknown, repeated, or misplaced entries in `stack`,
  `single_order`, and `visible` are dropped and the defaults are appended,
  so a hand-edited config can't produce a broken layout.
- **Dependencies:** `serde`, `toml`, `toml_edit` and `unicode-width` —
  the last one at runtime, not dev: the widgets truncate by display width.
- **CLI flags:** `--demo` (mock playlist, default layout, ignores and never
  writes the user config) and `--dump WxH [--frame N]` (render one frame to
  stdout without entering raw mode — see §9).

## 5. Keys (all remappable)

Defaults follow Winamp. Space is play/pause. The table is the source of
truth: `src/keymap.rs::defaults()`.

| Key | Action | Scope |
|---|---|---|
| `Space` | play / pause | global |
| `z` `x` `c` `v` `b` | prev / play / pause / stop / next | global |
| `l` | open / eject (stub in this phase) | global |
| `s` / `r` | shuffle / repeat | global |
| `+` / `=` / `-` | volume up / down | global |
| `←` / `→` | seek −5 s / +5 s | player, playlist |
| `←` / `→` | select EQ band | eq |
| `↑` / `↓` | volume + / − | player |
| `↑` / `↓` | move cursor | playlist |
| `↑` / `↓` | band gain ± 1 dB | eq |
| `Enter` | play track under cursor | playlist |
| `PgUp` `PgDn` `Home` `End` | page / jump | playlist |
| `a` `d` `/` | add / delete / find (stubs) | playlist |
| `Ctrl+T` | elapsed ↔ remaining time | global |
| `Tab` / `Shift+Tab` | focus next / previous panel | global |
| `1` `2` `3` | show / hide player, eq, playlist | global |
| `Shift+L` | toggle 1 ↔ 2 columns (saved) | global |
| `Shift+O` | cycle panel order (saved) | global |
| `?` | key-help overlay, generated from the live keymap | global |
| `q`, `Ctrl+C` | quit immediately | global |

The `?` overlay is 62 columns wide and centered: `Esc` (or `?`) closes it,
`↑`/`↓` scroll it, and any other key closes it and is swallowed.

Remapping looks like this:

```toml
[keys]
play_pause = ["space"]
next       = ["b", "n"]
toggle_columns = ["shift+l"]
```

- Keys are parsed into crossterm `KeyEvent`s. Specs are `space`, `enter`,
  `esc`, `tab`, `backtab`, `up`/`down`/`left`/`right`, `home`, `end`,
  `pgup`/`pgdn`, `f1`–`f5`, a single character, or `shift+x` / `ctrl+x` /
  `alt+x`. An uppercase letter implies Shift, so `"L"` == `"shift+l"`.
- A spec may be prefixed with a scope: `"global:+"`.
- If two actions share a key in the same scope, the config load reports the
  conflict and **both** actions revert to their defaults. Swapping two keys
  is not a conflict.
- The `?` overlay and the hotkey letters drawn in the UI read from the active
  keymap, so a remap shows up everywhere.

## 6. Module layout

```
src/
  main.rs              CLI flags (--demo / --dump), theme resolution, ratatui::run
  app.rs               App: UiState + snapshot source, event loop, tick, dispatch
  action.rs            enum Action (every remappable command)
  keymap.rs            defaults, config parsing, (scope, KeyEvent) → Action
  config.rs            load / save config.toml (toml_edit), XDG paths
  player/
    mod.rs             PlayerSnapshot (view model) + PlayerCommand
    mock.rs            MockPlayer: fake playlist, ticking clock, spectrum bins
  ui/
    mod.rs             render(frame, &App) + the golden-frame test
    layout.rs          pure fn compute_layout(area, &LayoutPrefs, &Visibility)
    theme/
      mod.rs           Theme (roles), presets, 256/16-color downgrade, glyphs
      omarchy.rs       colors.toml parsing, role mapping, change detection
    chrome.rs          rounded panel with superscript hotkey label + bottom hints
    player.rs          full and compact variants
    equalizer.rs
    playlist.rs
    help.rs            `?` overlay
    widgets/
      mod.rs
      lcd_digits.rs    2-row half-block digits
      marquee.rs
      spectrum.rs      eighth-block bars with peak caps
      slider.rs        HSlider / VSlider
      eq_curve.rs      braille polyline
tests/
  golden/              reference frames: 40x3, 52x12, 80x24, 104x18, 160x45
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
    event::poll(time_until_next_frame)?;      // ~33 ms → 30 fps
    //   Key → keymap::resolve(focus, key) → app.dispatch(action)
    app.tick(elapsed);                        // clock + spectrum every frame
    //   marquee every 200 ms · LCD blink every 500 ms · Omarchy every 1 s
    if app.should_quit { break }
}
```

- `ratatui::run` handles raw mode, the alternate screen, and restoring the
  terminal on panic. No mouse capture is needed in this phase.
- The frame deadline is tracked separately from the tick, so an idle app
  blocks in `poll` instead of spinning. A draw woken early by input never
  pushes the animation clock into the future.
- Each cadence (marquee, blink, Omarchy) has its own accumulator driven by
  the frame `dt`, which makes them deterministic and testable.
- Layout is recomputed from `frame.area()` every frame, so resizing needs no
  special handling. When a fallback drops the focused panel, the focus moves
  to a panel that is actually drawn.

## 8. Milestones

All green on `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
and `cargo test` (139 tests: 138 run, 1 ignored), plus a manual `cargo run` in
a real TTY.

**M0 — Skeleton** ✅ Loop with `q` / `Ctrl+C` quit, the `PlayerSnapshot` and
`MockPlayer` types, and 24 fake tracks.

**M1 — Theme and chrome** ✅ `Theme` roles, the `neon` / `amber` / `matrix` /
`synthwave` presets, color-depth downgrade, rounded panel with a superscript
label, focused (`glow`) vs unfocused (`muted`), bottom-border hints, and the
glyph-width test.

**M2 — Layout engine and config** ✅ `compute_layout` covering every variant
and fallback in §2, a property test over every size and preference
combination, and `L` / `O` / `1` `2` `3` saved through `toml_edit` with
comments intact. Fallbacks never overwrite the saved preference.

**M3 — Player panel** ✅ LCD digits, state glyph, spectrum, marquee, format
line, sliders, and the boxed transport row with accent hotkey letters. Full
and compact variants, driven by the tick. Verified against
`tests/golden/104x18.txt`.

**M4 — EQ panel** ✅ 11 `VSlider`s that grow with the available height, the
braille curve, the dB scale, the band labels, and band focus and adjust.

**M5 — Playlist panel** ✅ Playing and cursor styles, scroll that follows the
cursor, a scrollbar only when the list overflows, and bottom-border totals.

**M6 — Keymap** ✅ Full default keymap, `[keys]` remapping with conflict
detection, the `?` overlay, and on-screen hints driven by the keymap, with a
table test mapping `(scope, KeyEvent)` to `Action`.

**M7 — Omarchy** ✅ Fixture-based parser tests, role mapping, the 1 Hz change
check, and the silent fallback to `neon`.

**M8 — Polish** ✅ A `--demo` flag, a `--dump WxH` frame printer, the golden
frame suite, and a visual pass over the five reference sizes. The manual
sweep across Ghostty / kitty / Alacritty / iTerm2 / Terminal.app / tmux is
the only manual step left.

## 9. Testing strategy

- **Golden frames.** `tests/golden/*.txt` holds the exact rendered output at
  40×3, 52×12, 80×24, 104×18 and 160×45, produced by the deterministic mock
  (`App::deterministic()`). `cargo run -- --dump WxH [--frame N]` prints the
  same text; `UPDATE_GOLDEN=1 cargo test golden` regenerates the files.
  Never edit a golden file to make a test pass — regenerate it and eyeball
  the diff.
- **Layout.** `compute_layout` is pure, so its tests are `assert_eq!` on
  `Rect`s, plus a property test that sweeps every size from 1×1 to 200×60
  against 24 preference variants and 7 visibility combinations and asserts
  that no panel overlaps, leaves the area, or renders shorter than 3 rows.
- **Widgets and panels.** Render into a `Buffer` or `TestBackend` and check
  colors on key cells only: the focused border, the playing row, the knobs.
  A sweep test renders every layout/visibility combination at nine sizes to
  prove the renderer never panics.
- **Glyphs.** `unicode-width` asserts that every glyph constant is exactly
  1 cell wide (`src/ui/theme/mod.rs`).
- **Keymap and config.** A table test maps `(scope, KeyEvent)` to `Action` for
  every default; conflict and round-trip tests use `toml_edit`.
- **Omarchy.** Fixture directory through `AMPWIRE_OMARCHY_DIR`.
- **Manual.** `cargo run` only in a real TTY, never in pipes or CI.

## 10. Out of scope (later plans)

- Real audio (Symphonia + cpal/rodio), gapless playback, and the bit-perfect
  path. The `bit-perfect` flag is mock data for now.
- Jellyfin client and library browser.
- Adding and removing tracks from the file system (`a` / `d` / `/` are stubs).
- Real EQ on/off and balance control (the indicators and the slider exist;
  the actions don't — see §12).
- Mouse support.
- Media keys and MPRIS / macOS Now Playing.

## 11. Resolved open points

1. **Omarchy theme path.** ✅ Confirmed in the Omarchy repo: the live theme
   directory is `~/.local/state/omarchy/current/theme` (a real directory,
   atomically swapped), and `colors.toml` uses named keys. See §3.3.
2. **Panel order.** ✅ Fixed presets: `O` cycles playlist right ↔ left and
   swaps the player/EQ stack (four combinations in two-column mode).

## 12. Known deviations (left by plan 01)

Plan 01 is done. These gaps are in the code today; they are recorded here so
a later plan can pick them up:

1. **Space freed by a hidden panel is dropped.** In single-column mode the
   leftover height only ever goes to the playlist, so hiding the playlist
   (`3`) leaves blank rows at the bottom (`src/ui/layout.rs`).
2. **Hiding the playlist widens the stack.** The two-column path is only
   taken when the playlist is visible, so the 52-column column becomes the
   full terminal width instead of the stack rendering alone (§2.1).
3. **The EQ `on` / `auto` / `presets` indicators are decorative.** No action
   or key changes them; `UiState::eq_on` / `eq_auto` are set once at startup.
4. **The balance slider is display-only.** `PlayerSnapshot::balance` is
   always 0 — there is no action for it yet.
5. **The Omarchy fallback does not log the reason**, contrary to §3.3's
   original wording; the app has no logging at all.
