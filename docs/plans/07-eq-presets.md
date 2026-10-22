# Plan 07 — Equalizer presets screen

**Status:** ⬜ not started — safe to pick up (needs plan 06 done).

Goal: press `p` to open a centered list of presets. Moving the cursor
**previews** the preset live (the EQ sliders and the sound change), `Enter`
keeps it, `Esc` restores what you had.

Prerequisites: plan 06 merged (`SetEq`, `ui.eq_bands`, `ui.eq_on`, config persistence).

## Decisions

| Topic | Decision |
|---|---|
| Presets | Flat, Full Bass, Full Treble, Full Bass & Treble, Jazz, Rock, Blues, Pop, Classical, Folk (10, in this order). |
| User presets | Not in this plan. |
| UI | Modal overlay, same style as the `?` help overlay (rounded border, `Glow` colour). |
| Opening key | `p` (global). Also closes the overlay (acts like `Esc`). |
| Behavior | `↑`/`↓` move + live preview · `Home`/`End`/`PgUp`/`PgDn` jump · `Enter` apply and close · `Esc` or `p` revert and close. |
| EQ on/off | Previewing or applying a preset turns the EQ **on**. `Esc` restores the previous on/off state too. |
| Persistence | Nothing extra: applying a preset changes the bands, and plan 06 already persists them. |

## Preset data

Create `src/player/eq_presets.rs` (declare it in `src/player/mod.rs`). Values are dB; band order is
60, 170, 310, 600, 1k, 3k, 6k, 12k, 14k, 16k. Preamp is negative on boosted presets to avoid clipping.

```rust
pub struct Preset { pub name: &'static str, pub preamp: i8, pub bands: [i8; 10] }

pub const PRESETS: [Preset; 10] = [
    Preset { name: "Flat",                preamp:  0, bands: [ 0,  0,  0,  0,  0,  0,  0,  0,  0,  0] },
    Preset { name: "Full Bass",           preamp: -4, bands: [ 8,  8,  6,  3,  1,  0,  0,  0,  0,  0] },
    Preset { name: "Full Treble",         preamp: -4, bands: [ 0,  0,  0,  0,  0,  1,  3,  6,  8,  8] },
    Preset { name: "Full Bass & Treble",  preamp: -4, bands: [ 7,  5,  2, -2, -4, -3,  0,  4,  6,  7] },
    Preset { name: "Jazz",                preamp: -2, bands: [ 3,  2,  0,  2, -2, -2,  0,  2,  3,  4] },
    Preset { name: "Rock",                preamp: -3, bands: [ 5,  3,  2, -1, -3, -2,  1,  3,  5,  6] },
    Preset { name: "Blues",               preamp: -2, bands: [ 3,  2,  0, -1, -1,  0,  1,  3,  4,  4] },
    Preset { name: "Pop",                 preamp: -2, bands: [-1,  2,  4,  5,  3,  0, -1, -1, -1, -1] },
    Preset { name: "Classical",           preamp:  0, bands: [ 0,  0,  0,  0,  0,  0, -4, -4, -4, -6] },
    Preset { name: "Folk",                preamp: -2, bands: [ 3,  2,  0,  0,  1,  2,  3,  3,  2,  1] },
];

impl Preset { pub fn gains(&self) -> [i8; 11] { /* [preamp, bands...] */ } }
pub fn find_matching(gains: &[i8; 11]) -> Option<usize>;   // index of an identical preset
```
These are starting points. You may retune a number, but every value must stay within −12..=12 and the unit tests below must still pass.

## Files

`src/player/eq_presets.rs` (new), `src/ui/eq_presets.rs` (new overlay), `src/ui/mod.rs`, `src/app.rs`,
`src/action.rs`, `src/keymap.rs`, `src/ui/equalizer.rs`, `README.md`.

## Steps

### 1. Data module
Add `eq_presets.rs` exactly as above, with `gains()` and `find_matching()`.

### 2. State (`src/app.rs`)
```rust
pub struct PresetPicker { pub cursor: usize, pub saved_gains: [i8; 11], pub saved_on: bool }
// in UiState:
pub presets: Option<PresetPicker>,        // default None
```

### 3. Action + keymap (README rule 8)
`Action::EqPresets` (`"eq_presets"`, `Scope::Global`, default key `"p"`, description "equalizer presets").
Update the help overlay row generation only if it needs a section (it is generated from `Action::all()`).

### 4. Open / key handling
- `dispatch(EqPresets)`: if open → `cancel_presets()`; else open:
  `cursor = find_matching(&ui.eq_bands).unwrap_or(0)`, save `saved_gains = ui.eq_bands`, `saved_on = ui.eq_on`.
  Opening does **not** change the EQ yet.
- In `handle_key`, **before** the keymap and next to the existing help-overlay handling, if `ui.presets.is_some()`:
  - `Up` / `Down` → move the cursor (wrap-around is not needed; clamp), then `preview()`.
  - `Home` / `End` / `PageUp` / `PageDown` (10 rows) → jump, then `preview()`.
  - `Enter` → `ui.presets = None`, `notice = "eq preset: <name>"`. Keep the previewed values.
  - `Esc` or the key bound to `EqPresets` → `cancel_presets()`.
  - `Ctrl+C` → still quits. Every other key is swallowed.
- `preview()`: `ui.eq_bands = PRESETS[cursor].gains(); ui.eq_on = true; self.send_eq()` (from plan 06; this also marks the config dirty).
- `cancel_presets()`: restore `saved_gains` / `saved_on`, `send_eq()`, `ui.presets = None`.
- Make sure `dispatch`'s existing "any other key closes help" rule is untouched, and that opening presets closes the help overlay first.

### 5. Overlay rendering (`src/ui/eq_presets.rs`)
Copy the structure of `src/ui/help.rs` (`centered`, `Clear`, rounded `Block`, `Role::Glow` border):
- Width 30, height `PRESETS.len() + 4`, clamped to the terminal (if the terminal is shorter, scroll like the help overlay: keep the cursor visible with a scroll offset).
- Title ` eq presets `, footer ` ↑↓ preview · enter apply · esc cancel ` (shorten to ` enter · esc ` if the width is < 34).
- One row per preset: `▸ Name` for the cursor row (reverse/`Accent` style), two spaces otherwise;
  a `●` at the right edge on the row that equals the currently saved EQ (`find_matching(&saved_gains)`) so the user sees what they came from.
- Render it in `ui::render` right after the help overlay branch, so it is drawn on top of everything.
- In `src/ui/equalizer.rs`, when `ui.presets.is_some()` render the word `presets` in the toggle row with the `Accent` style (it already shows a `presets` label — read the code and reuse it).

### 6. Tests
- Data: `PRESETS.len() == 10`; names unique and non-empty; every gain (preamp and bands) within −12..=12; `Flat` is all zeros; `find_matching(&PRESETS[3].gains()) == Some(3)`; a custom curve → `None`.
- App behavior (use the mock backend and a recording player if available):
  - `p` opens; the cursor starts on the matching preset or 0; the bands are unchanged on open.
  - `Down` → `ui.eq_bands == PRESETS[cursor].gains()` and `eq_on == true`.
  - `Esc` after previewing restores the exact previous bands **and** the previous `eq_on` (test with `eq_on = false` initially).
  - `Enter` keeps the preset, closes the overlay, sets the notice.
  - While open, normal keys (e.g. `Space`, `b`) do **not** reach the transport; `Ctrl+C` still quits.
  - `p` pressed while open cancels.
- Render test: draw the overlay on a `TestBackend` at 80×24 and assert the buffer text contains `Full Bass & Treble`, `Jazz`, `Folk` and `eq presets`;
  at 40×8 the overlay does not panic and the cursor row is visible.
- Keymap tables/conflicts updated; goldens unchanged (the overlay is closed in all of them).
- Manual: open `p` while music plays, arrow through the list and hear it change, `Esc` restores, `Enter` keeps it across a restart.

### 7. Docs
README keys table: `p` presets, and the overlay keys.

## Done when
The behavior in the Decisions table works end to end and the three cargo checks pass.

## Assumptions I made (tell me to change)
- Preset numbers are tasteful defaults, not a copy of Winamp's.
- Applying a preset also switches the EQ on.
- The picker does not remember the last-used preset's *name* (only the bands persist).
