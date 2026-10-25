//! Disk scanner: find audio files under files/directories, read their tags,
//! and hand back `Track`s. Runs on a background thread ([`spawn_scan`]) so
//! the render loop never blocks on I/O; progress arrives on a channel.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use walkdir::WalkDir;

use crate::player::{Track, TrackSource};

/// The extensions we treat as audio, case-insensitive.
const EXTENSIONS: [&str; 4] = ["wav", "mp3", "m4a", "flac"];

/// Case-insensitive extension check against [`EXTENSIONS`].
pub fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// macOS AppleDouble sidecars (`._name.flac`): metadata stubs that inherit
/// the real file's extension but are never playable audio. Other hidden
/// files still load (plan 02's "hidden files are included" stands).
pub fn is_apple_double(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("._"))
}

/// The tag fields every source fills the same way (a local file, plan 02;
/// an `smb://` file, plan 10): artist, title, duration, and the lossy-line
/// bitrate — the tagged one, else an estimate from size and duration.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TagFields {
    pub artist: Option<String>,
    pub title: Option<String>,
    pub duration: Option<Duration>,
    pub bitrate_kbps: Option<u32>,
}

/// Apply [`read_track`]'s mapping to any file lofty could parse: tags first,
/// a zero duration counts as unknown, and `bytes` (when known) is what the
/// bitrate estimate falls back to.
pub fn tag_fields(tagged: &lofty::file::TaggedFile, bytes: Option<u64>) -> TagFields {
    use lofty::prelude::*;

    let mut fields = TagFields::default();

    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        fields.artist = clean(tag.artist().as_deref());
        fields.title = clean(tag.title().as_deref());
    }

    let duration = tagged.properties().duration();
    if !duration.is_zero() {
        fields.duration = Some(duration);
    }
    fields.bitrate_kbps = tagged
        .properties()
        .audio_bitrate()
        .filter(|b| *b > 0)
        .or_else(|| estimate_bitrate(duration, bytes));
    fields
}

/// Lossy-line fallback: bytes × 8 ÷ seconds, rounded to whole kbps.
fn estimate_bitrate(duration: Duration, bytes: Option<u64>) -> Option<u32> {
    let secs = duration.as_secs_f64();
    if secs <= 0.0 {
        return None;
    }
    let kbps = (bytes? as f64 * 8.0 / secs / 1000.0).round() as u64;
    u32::try_from(kbps).ok().filter(|b| *b > 0)
}

/// Read tags + duration for one file. **Never fails**: an unreadable or
/// tag-less file still yields a track (all fields `None`) — the file may
/// still play; plan 03 handles real decode errors.
pub fn read_track(path: &Path) -> Track {
    let mut fields = TagFields::default();
    if let Ok(tagged) = lofty::read_from_path(path) {
        let bytes = std::fs::metadata(path).ok().map(|meta| meta.len());
        fields = tag_fields(&tagged, bytes);
    }

    Track {
        id: Track::next_id(),
        source: TrackSource::Local(path.to_path_buf()),
        artist: fields.artist,
        title: fields.title,
        duration: fields.duration,
        bitrate_kbps: fields.bitrate_kbps,
    }
}

/// Trim, and treat empty/whitespace-only strings as missing tags.
fn clean(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Messages sent from a scan thread back to the app.
#[derive(Debug)]
pub enum ScanMsg {
    Progress {
        found: usize,
    },
    Done {
        tracks: Vec<Track>,
        skipped: usize,
    },
    /// The scan could not even start — so far only an `smb://` worker
    /// sends this, with one fixed line (`smb: cannot connect`,
    /// `not found: smb://…`); the local scan never fails this way (plan
    /// 10, step 3).
    Failed {
        reason: String,
    },
}

/// Scan `paths` on a new thread, reporting every 50 files and a final
/// `Done`. Directories are walked recursively without following symlinks;
/// `Done.tracks` is unsorted (the player sorts in `append_sorted`).
/// Unreadable entries increment `skipped`; this never panics.
pub fn spawn_scan(paths: Vec<PathBuf>, tx: mpsc::Sender<ScanMsg>) {
    std::thread::spawn(move || {
        let mut tracks = Vec::new();
        let mut skipped = 0usize;

        for path in paths {
            if path.is_dir() {
                for entry in WalkDir::new(&path).follow_links(false).into_iter() {
                    match entry {
                        Ok(entry)
                            if entry.file_type().is_file()
                                && is_audio(entry.path())
                                && !is_apple_double(entry.path()) =>
                        {
                            tracks.push(read_track(entry.path()));
                            if tracks.len() % 50 == 0 {
                                let _ = tx.send(ScanMsg::Progress {
                                    found: tracks.len(),
                                });
                            }
                        }
                        // Non-audio files and `._` sidecars: silently dropped.
                        Ok(_) => {}
                        Err(_) => skipped += 1,
                    }
                }
            } else if is_audio(&path) && !is_apple_double(&path) {
                tracks.push(read_track(&path));
                if tracks.len() % 50 == 0 {
                    let _ = tx.send(ScanMsg::Progress {
                        found: tracks.len(),
                    });
                }
            } else {
                // A plain file that isn't audio, is a `._` sidecar, or
                // doesn't exist.
                skipped += 1;
            }
        }

        let _ = tx.send(ScanMsg::Done { tracks, skipped });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a 1-second, 16-bit PCM WAV (mono, 8 kHz) header + data by hand.
    fn write_wav(path: &Path, secs: u32) {
        let sample_rate: u32 = 8_000;
        let channels: u16 = 1;
        let bits: u16 = 16;
        let data_len = sample_rate * secs * u32::from(channels) * u32::from(bits) / 8;
        let byte_rate = sample_rate * u32::from(channels) * u32::from(bits) / 8;
        let block_align = channels * bits / 8;

        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
        bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&byte_rate.to_le_bytes());
        bytes.extend_from_slice(&block_align.to_le_bytes());
        bytes.extend_from_slice(&bits.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        bytes.extend(std::iter::repeat_n(0u8, data_len as usize));
        std::fs::write(path, bytes).unwrap();
    }

    /// Run a synchronous scan and return the result (tests only).
    fn scan(paths: Vec<PathBuf>) -> (Vec<Track>, usize) {
        let (tx, rx) = mpsc::channel();
        spawn_scan(paths, tx);
        let mut result = None;
        while let Ok(msg) = rx.recv() {
            if let ScanMsg::Done { tracks, skipped } = msg {
                result = Some((tracks, skipped));
                break;
            }
        }
        result.expect("scan always sends Done")
    }

    #[test]
    fn extension_check_is_case_insensitive() {
        assert!(is_audio(Path::new("/m/song.mp3")));
        assert!(is_audio(Path::new("/m/song.MP3")));
        assert!(is_audio(Path::new("/m/Song.FlAc")));
        assert!(is_audio(Path::new("/m/song.m4a")));
        assert!(is_audio(Path::new("/m/song.wav")));
        assert!(!is_audio(Path::new("/m/cover.jpg")));
        assert!(!is_audio(Path::new("/m/playlist.m3u")));
        assert!(!is_audio(Path::new("/m/noext")));
    }

    #[test]
    fn scan_finds_nested_dirs_and_ignores_non_audio() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.path().join("one.mp3"), b"").unwrap();
        std::fs::write(nested.join("two.flac"), b"").unwrap();
        std::fs::write(dir.path().join("cover.jpg"), b"").unwrap();
        std::fs::write(dir.path().join("readme.txt"), b"").unwrap();

        let (tracks, skipped) = scan(vec![dir.path().to_path_buf()]);
        assert_eq!(tracks.len(), 2, "recursive, audio only: {tracks:?}");
        // The two non-audio *files at the root* are ignored during the walk
        // (directories skip non-audio silently).
        assert_eq!(skipped, 0, "walk skips non-audio without counting");
        let mut names: Vec<String> = tracks.iter().map(|t| t.display_name()).collect();
        names.sort();
        assert_eq!(names, ["one", "two"]);
    }

    #[test]
    fn mixed_case_extension_is_scanned() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("A.MP3"), b"").unwrap();
        let (tracks, _) = scan(vec![dir.path().to_path_buf()]);
        assert_eq!(tracks.len(), 1, "{tracks:?}");
        assert_eq!(tracks[0].display_name(), "A");
    }

    #[test]
    fn appledouble_sidecars_are_ignored_in_a_walk() {
        // The real-world case: an external volume with `._` metadata stubs
        // that inherit the `.flac` extension.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("1-01 Song.flac"), b"").unwrap();
        std::fs::write(dir.path().join("._1-01 Song.flac"), [0u8; 4096]).unwrap();
        std::fs::write(dir.path().join("._2-01 Other.flac"), [0u8; 4096]).unwrap();
        std::fs::write(dir.path().join(".hidden.flac"), b"").unwrap();

        let (tracks, skipped) = scan(vec![dir.path().to_path_buf()]);
        let mut names: Vec<String> = tracks.iter().map(|t| t.display_name()).collect();
        names.sort();
        // Sidecars dropped; a deliberately hidden track still loads.
        assert_eq!(names, [".hidden", "1-01 Song"], "{names:?}");
        assert_eq!(skipped, 0, "sidecars are silently ignored in a walk");
    }

    #[test]
    fn appledouble_passed_directly_counts_as_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("._x.flac");
        std::fs::write(&junk, [0u8; 16]).unwrap();

        let (tracks, skipped) = scan(vec![junk]);
        assert!(tracks.is_empty(), "{tracks:?}");
        assert_eq!(skipped, 1, "an explicit junk arg is reported");
    }

    #[test]
    fn non_audio_top_level_file_counts_as_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let txt = dir.path().join("notes.txt");
        std::fs::write(&txt, b"hi").unwrap();
        let (_tracks, skipped) = scan(vec![txt]);
        assert_eq!(skipped, 1);
    }

    #[test]
    fn zero_byte_mp3_still_yields_a_track_named_after_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("01 intro.mp3");
        std::fs::write(&path, b"").unwrap();

        let track = read_track(&path);
        assert_eq!(track.display_name(), "01 intro", "tag-less → file stem");
        assert_eq!(track.artist, None);
        assert_eq!(track.title, None);
        assert_eq!(track.duration, None, "0 bytes has no duration");
        assert_eq!(track.sort_key(), path.to_string_lossy().to_lowercase());
    }

    #[test]
    fn wav_duration_is_read_from_the_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("beep.wav");
        write_wav(&path, 1);

        let track = read_track(&path);
        let duration = track.duration.expect("WAV has a duration");
        assert!(
            duration.as_millis().abs_diff(1000) <= 50,
            "≈1 s, got {duration:?}"
        );
    }

    #[test]
    fn wav_bitrate_is_estimated_from_size_and_duration() {
        // The hand-written WAV has no bitrate tag: 8000 Hz × 1 ch × 16 bit
        // = 128 kbps, which the estimate must recover.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("beep.wav");
        write_wav(&path, 1);

        let track = read_track(&path);
        let kbps = track.bitrate_kbps.expect("bitrate estimate");
        assert!(
            (120..=135).contains(&kbps),
            "≈128 kbps from size/duration, got {kbps}"
        );
    }

    #[test]
    fn spawn_scan_reports_progress_every_50_files() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..120 {
            std::fs::write(dir.path().join(format!("track{i:03}.mp3")), b"").unwrap();
        }
        let (tx, rx) = mpsc::channel();
        spawn_scan(vec![dir.path().to_path_buf()], tx);
        let mut progress = 0;
        let mut done = None;
        while let Ok(msg) = rx.recv() {
            match msg {
                ScanMsg::Progress { found } => {
                    progress += 1;
                    assert_eq!(found % 50, 0, "progress at multiples of 50");
                }
                ScanMsg::Done { tracks, skipped } => {
                    done = Some((tracks, skipped));
                    break;
                }
                // Only an `smb://` worker sends this (plan 10, step 3).
                ScanMsg::Failed { reason } => panic!("the local scan never fails: {reason}"),
            }
        }
        let (tracks, _) = done.expect("Done");
        assert_eq!(tracks.len(), 120);
        assert_eq!(progress, 2, "50-file and 100-file progress ticks");
    }
}
