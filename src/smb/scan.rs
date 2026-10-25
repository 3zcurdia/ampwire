//! The share walk (plan 10, step 3): a recursive `smb://` directory walk on
//! its own worker thread, reporting through the *same* [`ScanMsg`]
//! protocol as the disk scan — so [`App::tick`] can poll both over one
//! channel and the UI never blocks on the network (plans-README rule 5).
//!
//! What the walk needs from a server sits behind [`ShareFs`]: production is
//! [`SmbShare`] (the `smb` crate), tests an in-memory tree — every scan
//! rule (nested dirs, `._` sidecars, hidden files, zero-byte files, the
//! 50-file progress ticks) is then checked without a network.
//!
//! Every path in [`ShareFs`] is the *share's own* path (from the share
//! root, `/`-separated); `""` is the root itself. The playlist row keeps
//! the full `smb://` URL, so playback never re-derives where it came from.

use std::io::{BufReader, Read, Seek};
use std::path::Path;
use std::sync::mpsc;

use crate::library::{self, ScanMsg};
use crate::player::{Track, TrackSource};

use super::source::{OpenError, SmbFileSource};
use super::{SmbClientCfg, SmbUrl};

/// A `Read + Seek` handle on remote bytes. Rust needs a *single* trait to
/// name in a trait object, so the two traits symphonia and lofty require
/// are bundled here (both are blanket-implemented for every reader).
pub(crate) trait ShareReader: Read + Seek {}
impl<T: Read + Seek + ?Sized> ShareReader for T {}

/// One entry of a share directory, addressed by its share-relative path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareEntry {
    /// `/`-separated path from the share root — what [`ShareFs::list`] and
    /// [`ShareFs::open`] take as their argument.
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
    /// A reparse point / symlink: never opened, never followed (plan 10,
    /// "Out of scope" — the same rule as `WalkDir::follow_links(false)`).
    pub is_symlink: bool,
}

/// What the walk needs from a share.
pub(crate) trait ShareFs {
    /// Entries of `dir`. Errors are the user-facing [`OpenError`] lines.
    fn list(&self, dir: &str) -> Result<Vec<ShareEntry>, OpenError>;

    /// Open a file for tag reading. Never panics; an error just means the
    /// track is built tag-less (the plan's "never fails the scan").
    fn open(&self, path: &str) -> Result<Box<dyn ShareReader>, OpenError>;
}

/// Scan one `smb://` URL on a new thread: `Progress` every 50 files and
/// exactly one final message ([`ScanMsg::Done`], or [`ScanMsg::Failed`]
/// when it cannot even start).
pub fn spawn_smb_scan(url: SmbUrl, client_cfg: SmbClientCfg, tx: mpsc::Sender<ScanMsg>) {
    std::thread::spawn(move || {
        let message = match SmbShare::connect(&url, &client_cfg) {
            Ok(share) => scan(&share, &url, &tx),
            Err(reason) => ScanMsg::Failed { reason },
        };
        let _ = tx.send(message);
    });
}

/// The walk itself, with the connection already open — `Err` carries the
/// one fixed line a root-level failure shows.
pub(crate) fn scan(share: &impl ShareFs, url: &SmbUrl, tx: &mpsc::Sender<ScanMsg>) -> ScanMsg {
    match walk(share, url, tx) {
        Ok((tracks, skipped)) => ScanMsg::Done { tracks, skipped },
        Err(reason) => ScanMsg::Failed { reason },
    }
}

/// Recursive walk of `url`. One directory handle at a time (never one per
/// nesting level): a deep tree costs one round trip per directory, not an
/// open handle per ancestor.
fn walk(
    share: &impl ShareFs,
    url: &SmbUrl,
    tx: &mpsc::Sender<ScanMsg>,
) -> Result<(Vec<Track>, usize), String> {
    let path = Path::new(&url.path);

    // `._` AppleDouble sidecars never play (the local scan's rule).
    if !url.path.is_empty() && library::is_apple_double(path) {
        return Ok((Vec::new(), 1));
    }
    // A single audio URL loads exactly that one track (plan: the extension
    // check is what tells a file from a directory).
    if !url.path.is_empty() && library::is_audio(path) {
        return match share.open(&url.path) {
            Ok(reader) => Ok((
                vec![track_for(
                    url_with_path(url, url.path.clone()),
                    Some(reader),
                    None,
                )],
                0,
            )),
            // The plan's `not found: smb://…`.
            Err(OpenError::NotFound) => Err(format!("not found: {}", url.canonical())),
            // Unreadable: counts as skipped, exactly like a non-audio
            // argument on the local path.
            Err(_) => Ok((Vec::new(), 1)),
        };
    }

    // Otherwise `url` is a directory: the root list decides between
    // "missing" (a fixed notice), "denied" (a fixed notice) and "nothing I
    // can read" (skipped — which is also where a *file* without an audio
    // extension ends up: a server will not list a file as a directory).
    let mut pending = match share.list(&url.path) {
        Ok(entries) => entries,
        Err(OpenError::NotFound) => return Err(format!("not found: {}", url.canonical())),
        Err(error @ OpenError::NO_ACCESS) => return Err(error.to_string()),
        Err(_) => return Ok((Vec::new(), 1)),
    };

    let mut tracks = Vec::new();
    let mut skipped = 0usize;
    while let Some(entry) = pending.pop() {
        if entry.is_symlink {
            continue; // never follow
        }
        if entry.is_dir {
            match share.list(&entry.path) {
                Ok(children) => pending.extend(children),
                // An unreadable directory is the skipped half (a directory
                // that vanished under us counts the same).
                Err(_) => skipped += 1,
            }
            continue;
        }
        let file = Path::new(&entry.path);
        if !library::is_audio(file) || library::is_apple_double(file) {
            continue; // silently dropped, like a non-audio file in a walk
        }
        let reader = share.open(&entry.path).ok();
        tracks.push(track_for(
            url_with_path(url, entry.path),
            reader,
            Some(entry.size),
        ));
        if tracks.len().is_multiple_of(50) {
            let _ = tx.send(ScanMsg::Progress {
                found: tracks.len(),
            });
        }
    }
    Ok((tracks, skipped))
}

/// The `smb://` URL of one walked file: the walk's server and share, with
/// the entry's own share-relative path.
fn url_with_path(root: &SmbUrl, path: String) -> SmbUrl {
    SmbUrl {
        server: root.server.clone(),
        share: root.share.clone(),
        path,
    }
}

/// Build one playlist row from whatever tags the reader yields. The
/// mapping is [`library::tag_fields`] — byte-for-byte the local scan's,
/// so an untagged file sorts and displays identically to its local twin.
fn track_for(url: SmbUrl, reader: Option<Box<dyn ShareReader>>, size: Option<u64>) -> Track {
    let mut fields = library::TagFields::default();
    if let Some(mut reader) = reader
        && let Some(tagged) = read_tags(&mut reader)
    {
        fields = library::tag_fields(&tagged, size);
    }
    Track {
        id: Track::next_id(),
        source: TrackSource::Smb { url },
        artist: fields.artist,
        title: fields.title,
        duration: fields.duration,
        bitrate_kbps: fields.bitrate_kbps,
    }
}

/// Read-ahead while parsing tags: every `Read` over SMB is one *positioned
/// request*, so lofty's default 8 KiB buffer turns a tag read into eight
/// round trips. 64 KiB matches what symphonia's `MediaSourceStream` asks
/// for and what the spike measured as the throughput knee.
const TAG_READAHEAD: usize = 64 * 1024;

/// lofty over any `Read + Seek` (`lofty::read_from` only accepts a real
/// `std::fs::File` — checked while writing this plan): sniff the
/// container, then parse. `None` for an empty or untaggable file.
fn read_tags(reader: &mut dyn ShareReader) -> Option<lofty::file::TaggedFile> {
    let probe = lofty::probe::Probe::new(BufReader::with_capacity(TAG_READAHEAD, reader))
        .guess_file_type()
        .ok()?;
    probe.read().ok()
}

// ---------------------------------------------------------------------------
// Production: the `smb` crate
// ---------------------------------------------------------------------------

/// [`ShareFs`] over one live session.
pub(crate) struct SmbShare {
    client: smb::Client,
    server: String,
    share: String,
}

impl SmbShare {
    /// Connect + log in (fixed error line, no credential in it). The URL's
    /// server always wins over any configured default.
    pub(crate) fn connect(url: &SmbUrl, cfg: &SmbClientCfg) -> Result<SmbShare, String> {
        let client = SmbClientCfg {
            server: url.server.clone(),
            ..cfg.clone()
        }
        .connect(&url.share)?;
        Ok(SmbShare {
            client,
            server: url.server.clone(),
            share: url.share.clone(),
        })
    }

    /// `\\server\share\path` for a share-relative path.
    fn unc(&self, path: &str) -> Result<smb::UncPath, smb::Error> {
        let unc = smb::UncPath::new(&self.server)?.with_share(&self.share)?;
        Ok(if path.is_empty() {
            unc
        } else {
            unc.with_path(path)
        })
    }
}

impl ShareFs for SmbShare {
    fn list(&self, dir: &str) -> Result<Vec<ShareEntry>, OpenError> {
        use smb::{
            DirAccessMask, Directory, FileCreateArgs, FileFullDirectoryInformation, Resource,
        };

        let unc = self.unc(dir).map_err(|e| OpenError::from_crate_list(&e))?;
        let access = DirAccessMask::new()
            .with_list_directory(true)
            .with_synchronize(true)
            .into();
        let resource = self
            .client
            .create_file(&unc, &FileCreateArgs::make_open_existing(access))
            .map_err(|e| OpenError::from_crate_list(&e))?;
        let Resource::Directory(directory) = resource else {
            // A *file* where a directory was expected: how a non-audio
            // single URL reaches the `skipped` path.
            return Err(OpenError::CANNOT_LIST);
        };

        let mut entries = Vec::new();
        {
            let query = Directory::query::<FileFullDirectoryInformation>(&directory, "*")
                .map_err(|e| OpenError::from_crate_list(&e))?;
            for entry in query {
                let entry = entry.map_err(|e| OpenError::from_crate_list(&e))?;
                let name = entry.file_name.to_string();
                if name == "." || name == ".." {
                    continue;
                }
                entries.push(ShareEntry {
                    path: if dir.is_empty() {
                        name
                    } else {
                        format!("{dir}/{name}")
                    },
                    size: entry.end_of_file,
                    is_dir: entry.file_attributes.directory(),
                    is_symlink: entry.file_attributes.reparse_point(),
                });
            }
        }
        // The sync `smb` build only *warns* on an un-closed handle.
        let _ = directory.close();
        Ok(entries)
    }

    fn open(&self, path: &str) -> Result<Box<dyn ShareReader>, OpenError> {
        let unc = self.unc(path).map_err(|e| OpenError::from_crate(&e))?;
        SmbFileSource::open(&self.client, &unc).map(|source| Box::new(source) as _)
    }
}

// ---------------------------------------------------------------------------
// Tests: an in-memory share
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::io::Cursor;

    use super::*;

    /// A tree built from file paths: every directory on the way is created
    /// and linked into its parent, exactly as a server would report it.
    /// Paths are share-relative, like [`ShareFs`]'s.
    #[derive(Default)]
    struct FakeShare {
        /// dir → its entries, or the error listing it fails with.
        dirs: BTreeMap<String, Result<Vec<ShareEntry>, OpenError>>,
        /// file → its bytes, or the error opening it fails with.
        files: HashMap<String, Result<Vec<u8>, OpenError>>,
    }

    impl FakeShare {
        fn new() -> Self {
            let mut share = Self::default();
            share.dirs.insert(String::new(), Ok(Vec::new()));
            share
        }

        /// A file with `bytes`; `path` may be nested.
        fn file(mut self, path: &str, bytes: &[u8]) -> Self {
            let parent = parent_of(path).to_string();
            self.ensure_dir(&parent);
            self.dirs
                .get_mut(&parent)
                .expect("parent exists")
                .as_mut()
                .expect("parent listable")
                .push(ShareEntry {
                    path: path.to_string(),
                    size: bytes.len() as u64,
                    is_dir: false,
                    is_symlink: false,
                });
            self.files.insert(path.to_string(), Ok(bytes.to_vec()));
            self
        }

        /// A directory whose listing fails.
        fn broken(mut self, path: &str, error: OpenError) -> Self {
            self.dirs.insert(path.to_string(), Err(error));
            self
        }

        fn ensure_dir(&mut self, dir: &str) {
            if self.dirs.contains_key(dir) {
                return;
            }
            self.dirs.insert(dir.to_string(), Ok(Vec::new()));
            if dir.is_empty() {
                return;
            }
            let parent = parent_of(dir).to_string();
            self.ensure_dir(&parent);
            self.dirs
                .get_mut(&parent)
                .expect("parent exists")
                .as_mut()
                .expect("parent listable")
                .push(ShareEntry {
                    path: dir.to_string(),
                    size: 0,
                    is_dir: true,
                    is_symlink: false,
                });
        }
    }

    impl ShareFs for FakeShare {
        fn list(&self, dir: &str) -> Result<Vec<ShareEntry>, OpenError> {
            if let Some(result) = self.dirs.get(dir) {
                return result.clone();
            }
            if self.files.contains_key(dir) {
                // A server will not list a file as a directory — which is
                // what a non-audio single URL looks like from here.
                return Err(OpenError::CANNOT_LIST);
            }
            Err(OpenError::NotFound)
        }

        fn open(&self, path: &str) -> Result<Box<dyn ShareReader>, OpenError> {
            if let Some(result) = self.files.get(path) {
                return result
                    .clone()
                    .map(|bytes| Box::new(Cursor::new(bytes)) as Box<dyn ShareReader>);
            }
            if self.dirs.contains_key(path) {
                return Err(OpenError::IS_DIRECTORY);
            }
            Err(OpenError::NotFound)
        }
    }

    fn parent_of(path: &str) -> &str {
        path.rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or("")
    }

    /// `smb://nas/Music/<path>`.
    fn url(path: &str) -> SmbUrl {
        SmbUrl {
            server: "nas".into(),
            share: "Music".into(),
            path: path.into(),
        }
    }

    /// Run the walk and split its final message from the progress ticks.
    fn run(share: &FakeShare, url: &SmbUrl) -> (ScanMsg, Vec<usize>) {
        let (tx, rx) = mpsc::channel();
        let message = scan(share, url, &tx);
        drop(tx);
        let progress = rx
            .iter()
            .filter_map(|msg| match msg {
                ScanMsg::Progress { found } => Some(found),
                _ => None,
            })
            .collect();
        (message, progress)
    }

    fn done(message: ScanMsg) -> (Vec<Track>, usize) {
        match message {
            ScanMsg::Done { tracks, skipped } => (tracks, skipped),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    /// The local scan's shape: nested dirs are walked, non-audio files are
    /// dropped without counting as skipped, and the rows are tag-less
    /// (stem-named) because the fake serves no tags.
    #[test]
    fn the_walk_finds_nested_dirs_and_ignores_non_audio() {
        let share = FakeShare::new()
            .file("one.mp3", b"")
            .file("a/b/two.flac", b"")
            .file("cover.jpg", b"")
            .file("readme.txt", b"")
            .file("a/mix.m4a", b"");

        let (message, progress) = run(&share, &url(""));
        let (tracks, skipped) = done(message);
        assert_eq!(tracks.len(), 3);
        assert_eq!(skipped, 0, "non-audio is dropped silently, not skipped");
        let mut names: Vec<String> = tracks.iter().map(Track::display_name).collect();
        names.sort();
        assert_eq!(names, ["mix", "one", "two"]);
        assert!(
            tracks.iter().all(|t| t.is_remote() && !t.is_jellyfin()),
            "every row is an SMB track"
        );
        assert!(progress.is_empty(), "fewer than 50 files");
    }

    /// Sidecars are dropped, a deliberately hidden track stays (plan 10
    /// "Single file vs dir" + plan 02's hidden-file rule).
    #[test]
    fn appledouble_sidecars_go_and_hidden_tracks_stay() {
        let share = FakeShare::new()
            .file("1-01 Song.flac", b"")
            .file("._1-01 Song.flac", b"")
            .file(".hidden.flac", b"");
        let (message, _) = run(&share, &url(""));
        let (tracks, skipped) = done(message);
        let mut names: Vec<String> = tracks.iter().map(Track::display_name).collect();
        names.sort();
        assert_eq!(names, [".hidden", "1-01 Song"], "{names:?}");
        assert_eq!(skipped, 0, "sidecars are ignored, not counted");
    }

    /// Plan 10, step 3: a zero-byte file still yields a (tag-less) track.
    #[test]
    fn a_zero_byte_file_still_yields_a_track() {
        let share = FakeShare::new().file("01 intro.mp3", b"");
        let (message, _) = run(&share, &url(""));
        let (tracks, _) = done(message);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].display_name(), "01 intro", "stem-named");
        assert_eq!(tracks[0].artist, None);
        assert_eq!(tracks[0].duration, None, "0 bytes has no duration");
    }

    /// Progress fires at 50 and 100 files — the local scan's cadence
    /// (mirrors `library::spawn_scan_reports_progress_every_50_files`).
    #[test]
    fn progress_ticks_every_fifty_files() {
        let mut share = FakeShare::new();
        for i in 0..120 {
            share = share.file(&format!("track{i:03}.mp3"), b"");
        }
        let (message, progress) = run(&share, &url(""));
        let (tracks, _) = done(message);
        assert_eq!(tracks.len(), 120);
        assert_eq!(progress, vec![50, 100], "one tick per 50 files");
    }

    /// An unreadable directory is `skipped`; the rest of the walk still
    /// completes.
    #[test]
    fn an_unreadable_directory_counts_as_skipped() {
        let share = FakeShare::new()
            .file("ok.mp3", b"")
            .file("locked/inside.mp3", b"")
            .broken("locked", OpenError::NO_ACCESS);
        let (message, _) = run(&share, &url(""));
        let (tracks, skipped) = done(message);
        assert_eq!(tracks.len(), 1, "the readable half still loads");
        assert_eq!(skipped, 1, "the locked directory is the skipped half");
    }

    /// Plan 10, "Single file vs dir": an audio URL is one track, a missing
    /// one is `not found: smb://…`, and a non-audio URL is `skipped` (the
    /// local `no audio files found` path).
    #[test]
    fn single_urls_split_into_track_not_found_and_skipped() {
        // One audio file → one row carrying its own full URL.
        let share = FakeShare::new().file("Album/song.flac", b"");
        let (message, _) = run(&share, &url("Album/song.flac"));
        let (tracks, skipped) = done(message);
        assert_eq!(tracks.len(), 1);
        assert_eq!(skipped, 0);
        assert_eq!(tracks[0].display_name(), "song", "the row names the file");
        match &tracks[0].source {
            TrackSource::Smb { url } => assert_eq!(
                url.canonical(),
                "smb://nas/Music/Album/song.flac",
                "the row carries its own full URL"
            ),
            other => panic!("not an smb row: {other:?}"),
        }

        // A missing file → the plan's `not found: smb://…`.
        let (message, _) = run(&share, &url("Album/gone.flac"));
        match message {
            ScanMsg::Failed { reason } => {
                assert_eq!(reason, "not found: smb://nas/Music/Album/gone.flac")
            }
            other => panic!("expected Failed, got {other:?}"),
        }

        // A non-audio file (no audio extension → walked as a directory,
        // which fails because it is a file) → skipped.
        let share = FakeShare::new().file("cover.jpg", b"");
        let (message, _) = run(&share, &url("cover.jpg"));
        let (tracks, skipped) = done(message);
        assert!(tracks.is_empty());
        assert_eq!(skipped, 1);

        // An `._` sidecar given directly → skipped, like the local rule.
        let share = FakeShare::new().file("._x.flac", b"");
        let (message, _) = run(&share, &url("._x.flac"));
        let (tracks, skipped) = done(message);
        assert!(tracks.is_empty());
        assert_eq!(skipped, 1);

        // A missing directory → `not found: smb://…` too.
        let (message, _) = run(&FakeShare::new(), &url("Nope"));
        match message {
            ScanMsg::Failed { reason } => assert_eq!(reason, "not found: smb://nas/Music/Nope"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// Tag reading runs over the *bytes* the share serves, through the very
    /// same mapping the local scan uses — so the row is identical to the
    /// one a local copy of the file would produce.
    #[test]
    fn tags_come_from_the_remote_bytes_with_the_local_mapping() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.flac");
        let bytes = std::fs::read(&fixture).expect("fixture");
        let share = FakeShare::new().file("album/tone.flac", &bytes);

        let (message, _) = run(&share, &url(""));
        let (tracks, _) = done(message);
        assert_eq!(tracks.len(), 1);
        let tagged = tracks.into_iter().next().expect("one row");

        // The same file, read from disk by the local scanner.
        let local = library::read_track(&fixture);
        assert_eq!(tagged.artist, local.artist, "artist");
        assert_eq!(tagged.title, local.title, "title");
        assert_eq!(tagged.duration, local.duration, "duration");
        assert_eq!(tagged.bitrate_kbps, local.bitrate_kbps, "bitrate line");
        assert!(
            tagged.duration.is_some(),
            "the fixture has a duration, so lofty parsed it"
        );
    }

    /// Symlinks/reparse points are reported but never followed — their
    /// subtree is never opened.
    #[test]
    fn symlinks_are_reported_but_never_followed() {
        let mut share = FakeShare::new().file("real/song.mp3", b"");
        share
            .dirs
            .get_mut("")
            .expect("root")
            .as_mut()
            .expect("root list")
            .push(ShareEntry {
                path: "link".into(),
                size: 0,
                is_dir: true,
                is_symlink: true,
            });
        let (message, _) = run(&share, &url(""));
        let (tracks, skipped) = done(message);
        assert_eq!(tracks.len(), 1, "only the real file");
        assert_eq!(skipped, 0, "the link is dropped, not counted");
    }

    /// Plan 10, step 3 (real server): the *production* [`SmbShare`] walks a
    /// live share — tags come from the remote bytes and every row carries
    /// its own full `smb://` URL.
    #[test]
    #[ignore = "needs a live Samba server (see module docs)"]
    fn smb_spike_scan_walks_a_real_share() {
        let Some(target) = crate::smb::smoke::target() else {
            return eprintln!("smb_spike: AMPWIRE_SMB_TEST_SERVER/SHARE not set, skipped");
        };
        let url = SmbUrl {
            server: target.server.clone(),
            share: target.share.clone(),
            path: target.path.clone(),
        };
        let cfg = SmbClientCfg::new(
            target.server.as_str(),
            target.username.as_str(),
            target.password.as_str(),
        );
        let (tx, rx) = mpsc::channel();
        let share = SmbShare::connect(&url, &cfg).expect("connect");
        let message = scan(&share, &url, &tx);
        drop(tx);
        let mut last_progress = 0usize;
        for msg in rx {
            if let ScanMsg::Progress { found } = msg {
                last_progress = found;
            }
        }
        match message {
            ScanMsg::Done { tracks, skipped } => {
                eprintln!(
                    "smb_spike: {} tracks ({} skipped); last progress tick at {}",
                    tracks.len(),
                    skipped,
                    last_progress
                );
                assert!(!tracks.is_empty(), "the share has audio files");
                assert!(
                    tracks.iter().all(Track::is_smb),
                    "every row came from the share"
                );
                assert!(
                    tracks.iter().any(|t| t.duration.is_some()),
                    "tags were read from the remote bytes"
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }
}
