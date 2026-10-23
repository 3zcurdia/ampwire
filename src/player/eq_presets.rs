//! EQ presets (plan 07): named curves of preamp + 10 band gains in whole dB.
//!
//! These are tasteful starting points in the Winamp spirit, not copies of
//! its numbers. The preamp is negative on boosted presets so the summed
//! output stays inside ±1.0 and cannot clip.

/// One named preset: preamp + 10 band gains (band order 60, 170, 310, 600,
/// 1k, 3k, 6k, 12k, 14k, 16k Hz — same as `BAND_LABELS[1..]`).
pub struct Preset {
    pub name: &'static str,
    pub preamp: i8,
    pub bands: [i8; 10],
}

pub const PRESETS: [Preset; 10] = [
    Preset {
        name: "Flat",
        preamp: 0,
        bands: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    },
    Preset {
        name: "Full Bass",
        preamp: -4,
        bands: [8, 8, 6, 3, 1, 0, 0, 0, 0, 0],
    },
    Preset {
        name: "Full Treble",
        preamp: -4,
        bands: [0, 0, 0, 0, 0, 1, 3, 6, 8, 8],
    },
    Preset {
        name: "Full Bass & Treble",
        preamp: -4,
        bands: [7, 5, 2, -2, -4, -3, 0, 4, 6, 7],
    },
    Preset {
        name: "Jazz",
        preamp: -2,
        bands: [3, 2, 0, 2, -2, -2, 0, 2, 3, 4],
    },
    Preset {
        name: "Rock",
        preamp: -3,
        bands: [5, 3, 2, -1, -3, -2, 1, 3, 5, 6],
    },
    Preset {
        name: "Blues",
        preamp: -2,
        bands: [3, 2, 0, -1, -1, 0, 1, 3, 4, 4],
    },
    Preset {
        name: "Pop",
        preamp: -2,
        bands: [-1, 2, 4, 5, 3, 0, -1, -1, -1, -1],
    },
    Preset {
        name: "Classical",
        preamp: 0,
        bands: [0, 0, 0, 0, 0, 0, -4, -4, -4, -6],
    },
    Preset {
        name: "Folk",
        preamp: -2,
        bands: [3, 2, 0, 0, 1, 2, 3, 3, 2, 1],
    },
];

impl Preset {
    /// The full 11-value EQ payload: `[preamp, bands…]`, exactly the shape
    /// `App::ui.eq_bands` and `PlayerCommand::SetEq` use.
    pub fn gains(&self) -> [i8; 11] {
        let mut gains = [0i8; 11];
        gains[0] = self.preamp;
        gains[1..].copy_from_slice(&self.bands);
        gains
    }
}

/// Index of the preset whose gains are exactly `gains`, if any — used to
/// preselect the cursor on open and to mark the preset the EQ came from.
pub fn find_matching(gains: &[i8; 11]) -> Option<usize> {
    PRESETS.iter().position(|p| &p.gains() == gains)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_presets_with_unique_nonempty_names() {
        assert_eq!(PRESETS.len(), 10);
        for (i, preset) in PRESETS.iter().enumerate() {
            assert!(!preset.name.is_empty(), "preset {i} name empty");
            let first = PRESETS.iter().position(|p| p.name == preset.name);
            assert_eq!(first, Some(i), "duplicate name {}", preset.name);
        }
    }

    #[test]
    fn every_gain_is_within_the_eq_range() {
        for preset in &PRESETS {
            assert!(
                (-12..=12).contains(&preset.preamp),
                "{} preamp {}",
                preset.name,
                preset.preamp
            );
            for (band, db) in preset.bands.iter().enumerate() {
                assert!(
                    (-12..=12).contains(db),
                    "{} band {band} = {db}",
                    preset.name
                );
            }
        }
    }

    #[test]
    fn flat_is_all_zeros() {
        let flat = &PRESETS[0];
        assert_eq!(flat.name, "Flat");
        assert_eq!(flat.preamp, 0);
        assert_eq!(flat.bands, [0i8; 10]);
    }

    #[test]
    fn gains_prepend_the_preamp() {
        let gains = PRESETS[3].gains(); // Full Bass & Treble
        assert_eq!(gains[0], PRESETS[3].preamp);
        assert_eq!(&gains[1..], &PRESETS[3].bands);
    }

    #[test]
    fn find_matching_finds_a_preset_and_misses_a_custom_curve() {
        assert_eq!(find_matching(&PRESETS[3].gains()), Some(3));
        // Every preset round-trips through find_matching.
        for (i, preset) in PRESETS.iter().enumerate() {
            assert_eq!(find_matching(&preset.gains()), Some(i), "{}", preset.name);
        }
        // A custom curve (preamp tweaked) matches nothing.
        let mut custom = PRESETS[3].gains();
        custom[0] += 1;
        assert_eq!(find_matching(&custom), None);
    }
}
