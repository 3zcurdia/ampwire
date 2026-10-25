//! Rolling download window for `[smb] mode = "cache"` (plan 10, step 5) —
//! the plan 08 Jellyfin window with SMB identity.
//!
//! One dedicated **downloader thread** (never the engine thread, so Stop and
//! Next stay responsive during downloads) owns a private
//! `ampwire-smb-<pid>` directory (`0700` on unix): it keeps the current SMB
//! track plus the next `cache_ahead` and previous `cache_behind` SMB tracks
//! in play order on disk, evicting everything else.
//!
//! Wire protocol: [`SmbCache`] (UI side) sends [`SmbCacheCmd::Window`]s over
//! one channel and reads [`SmbCacheEvent`]s from another. Downloads land in
//! `<hash>.part` and are renamed only when complete — a `.part` file is
//! never reported ready and therefore never decoded.
//!
//! The directory helpers (`create_own_dir`, `create_fresh_file`,
//! `sweep_stale_dirs`, `size_label`) live with the first cache that needed
//! them — plan 08's module — parameterized by prefix, so both windows share
//! one implementation of the security-sensitive bits (audit S4) instead of
//! drifting apart.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::jellyfin::cache::{create_fresh_file, create_own_dir, size_label};
use crate::player::order::PlayOrder;
use crate::player::{Track, TrackSource};

use super::source::SmbFileSource;
use super::{SmbClientCfg, SmbUrl};

/// Hard per-file cap: 512 MiB (the plan's v1 limit; the window is therefore
/// at most 7 × 512 MiB in the worst case).
pub const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
/// Read/write chunk while downloading (the plan's 256 KiB).
const CHUNK: usize = 256 * 1024;
/// Directories left behind by a crash are swept after this long.
pub const STALE_AGE: Duration = Duration::from_secs(24 * 3600);
/// Directory-name prefix this app creates — the *only* thing the sweep and
/// the eviction will ever touch by name.
pub const DIR_PREFIX: &str = "ampwire-smb-";

/// One SMB track the cache should hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmbRef {
    pub url: SmbUrl,
}

impl SmbRef {
    /// The ref behind a [`Track`], or `None` for local/Jellyfin tracks.
    pub fn from_track(track: &Track) -> Option<SmbRef> {
        match &track.source {
            TrackSource::Smb { url } => Some(SmbRef { url: url.clone() }),
            _ => None,
        }
    }
}

/// UI → downloader thread.
#[derive(Debug)]
pub enum SmbCacheCmd {
    /// New wanted set, already in priority order: current, ahead (nearest
    /// first), behind (nearest first).
    Window(Vec<SmbRef>),
    Shutdown,
}

/// Downloader → UI thread.
#[derive(Debug)]
pub enum SmbCacheEvent {
    Ready { url: SmbUrl, path: PathBuf },
    Failed { url: SmbUrl, error: String },
}

/// What the downloader should do after one item (internal).
enum Flow {
    /// Finished (or failed) this item: continue down the list.
    Continue,
    /// A newer window arrived: start over from its first item.
    Restart,
    /// Shutdown: leave the loop.
    Stop,
}

/// Why [`download_file`] produced no file.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DownloadError {
    /// The caller's `interrupt` asked to stop; the `.part` is gone.
    Interrupted,
    /// A key-free, user-facing reason (`smb: cannot connect`, …).
    Failed(String),
}

/// The temp-dir window. Dropping it stops the thread **and** removes the
/// directory it created (errors ignored — the OS sweep is the backstop).
pub struct SmbCache {
    tx: Sender<SmbCacheCmd>,
    rx: Receiver<SmbCacheEvent>,
    join: Option<JoinHandle<()>>,
    dir: PathBuf,
    /// URL → file on disk, kept in step with the window. A small mutex
    /// (not `&mut`) so [`SmbCache::set_window`] can stay `&self` and be
    /// called straight from an immutable borrow (as the plan specifies).
    ready: Mutex<HashMap<SmbUrl, PathBuf>>,
}

impl SmbCache {
    /// Create `ampwire-smb-<pid>` under `dir_parent` (the system temp dir
    /// when `None`) and spawn the downloader. Crash leftovers are swept once
    /// per session by the player, in every mode ([`sweep_stale_dirs`]).
    pub fn new(cfg: SmbClientCfg, dir_parent: Option<PathBuf>) -> std::io::Result<SmbCache> {
        Self::with_limit(cfg, dir_parent, MAX_FILE_BYTES)
    }

    /// [`SmbCache::new`] with an injectable size cap (the test uses 1 KiB).
    pub(crate) fn with_limit(
        cfg: SmbClientCfg,
        dir_parent: Option<PathBuf>,
        max_bytes: u64,
    ) -> std::io::Result<SmbCache> {
        let parent = dir_parent.unwrap_or_else(std::env::temp_dir);
        let dir = create_own_dir(&parent, DIR_PREFIX)?;

        let (cmd_tx, cmd_rx) = channel();
        let (ev_tx, ev_rx) = channel();
        let thread_dir = dir.clone();
        let join = std::thread::Builder::new()
            .name("ampwire-smb-cache".into())
            .spawn(move || run(cfg, thread_dir, max_bytes, cmd_rx, ev_tx))?;

        Ok(SmbCache {
            tx: cmd_tx,
            rx: ev_rx,
            join: Some(join),
            dir,
            ready: Mutex::new(HashMap::new()),
        })
    }

    /// Hand over a new wanted set (priority order). Bookkeeping for URLs
    /// about to be evicted is dropped right here; the thread deletes their
    /// files in the same pass.
    pub fn set_window(&self, wanted: Vec<SmbRef>) {
        {
            let mut ready = self.ready.lock().expect("smb cache ready lock");
            ready.retain(|url, _| wanted.iter().any(|ref_| ref_.url == *url));
        }
        let _ = self.tx.send(SmbCacheCmd::Window(wanted));
    }

    /// The local file for `url`, if it finished downloading (and still
    /// exists — eviction can race a stale bookkeeping entry).
    pub fn path_if_ready(&self, url: &SmbUrl) -> Option<PathBuf> {
        let mut ready = self.ready.lock().expect("smb cache ready lock");
        let path = ready.get(url)?.clone();
        if path.exists() {
            Some(path)
        } else {
            ready.remove(url);
            None
        }
    }

    /// Drain downloader events (also folds `Ready` into the bookkeeping).
    pub fn poll(&mut self) -> Vec<SmbCacheEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            if let SmbCacheEvent::Ready { url, path } = &event {
                self.ready
                    .lock()
                    .expect("smb cache ready lock")
                    .insert(url.clone(), path.clone());
            }
            events.push(event);
        }
        events
    }

    /// The directory this cache owns (tests read it back).
    #[cfg(test)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Test support: pretend `url` is already on disk, so the player's
    /// cache-hit path can be exercised without a server.
    #[cfg(test)]
    pub fn insert_ready(&mut self, url: &SmbUrl, path: PathBuf) {
        self.ready
            .lock()
            .expect("smb cache ready lock")
            .insert(url.clone(), path);
    }
}

impl Drop for SmbCache {
    fn drop(&mut self) {
        let _ = self.tx.send(SmbCacheCmd::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        // Only *our* directory — never its parent, never a sibling's.
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The window in play order: current (if SMB) → `ahead` next SMB tracks →
/// `behind` previous ones, nearest first, deduplicated. Local and Jellyfin
/// tracks are skipped and never counted, so shuffle (which reorders the play
/// sequence, not the playlist) is respected automatically.
pub fn compute_smb_window(
    order: &PlayOrder,
    tracks: &[Track],
    current: Option<usize>,
    ahead: usize,
    behind: usize,
) -> Vec<SmbRef> {
    let mut seen: HashSet<usize> = HashSet::new();
    let mut wanted: Vec<usize> = Vec::new();

    let mut take = |index: usize, wanted: &mut Vec<usize>| {
        // Only *SMB* tracks are downloadable here: a Jellyfin row in the
        // same playlist has its own window (plan 10, step 2).
        if seen.insert(index) && tracks.get(index).is_some_and(Track::is_smb) {
            wanted.push(index);
        }
    };

    if let Some(current) = current {
        take(current, &mut wanted);
    }
    // `N` large enough to find `ahead` *SMB* tracks among everything.
    let mut count = 0;
    for index in order.upcoming(tracks.len()) {
        if count >= ahead {
            break;
        }
        let before = wanted.len();
        take(index, &mut wanted);
        if wanted.len() > before {
            count += 1;
        }
    }
    let mut count = 0;
    for index in order.previous(tracks.len()) {
        if count >= behind {
            break;
        }
        let before = wanted.len();
        take(index, &mut wanted);
        if wanted.len() > before {
            count += 1;
        }
    }

    wanted
        .into_iter()
        .filter_map(|index| SmbRef::from_track(&tracks[index]))
        .collect()
}

/// The downloader thread: wait for a window → evict → fetch in priority
/// order, restarting whenever a newer window arrives.
fn run(
    cfg: SmbClientCfg,
    dir: PathBuf,
    max_bytes: u64,
    rx: Receiver<SmbCacheCmd>,
    tx: Sender<SmbCacheEvent>,
) {
    let mut wanted: Vec<SmbRef>;
    loop {
        // 1. Block until the player hands over a window.
        match rx.recv() {
            Ok(SmbCacheCmd::Window(list)) => {
                evict(&dir, &list);
                wanted = list;
            }
            Ok(SmbCacheCmd::Shutdown) | Err(_) => return,
        }

        // 2. Fetch the list in priority order (current first).
        let mut index = 0;
        while index < wanted.len() {
            // A newer window that arrived between items preempts ours.
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    SmbCacheCmd::Window(list) => {
                        evict(&dir, &list);
                        wanted = list;
                        index = 0;
                    }
                    SmbCacheCmd::Shutdown => return,
                }
            }
            let item = wanted[index].clone();
            match fetch(&cfg, &dir, &item, max_bytes, &rx, &tx, &mut wanted) {
                Flow::Continue => index += 1,
                Flow::Restart => index = 0,
                Flow::Stop => return,
            }
        }
        // 3. Window satisfied → back to step 1 and wait.
    }
}

/// Fetch one file for the window: [`download_file`], checking for a newer
/// window between chunks — keep going if the file is still wanted, abandon
/// it otherwise — then report `Ready`/`Failed`.
fn fetch(
    cfg: &SmbClientCfg,
    dir: &Path,
    item: &SmbRef,
    max_bytes: u64,
    rx: &Receiver<SmbCacheCmd>,
    tx: &Sender<SmbCacheEvent>,
    wanted: &mut Vec<SmbRef>,
) -> Flow {
    let mut flow = Flow::Continue;
    let result = download_file(cfg, item, dir, max_bytes, || match rx.try_recv() {
        Ok(SmbCacheCmd::Window(list)) => {
            evict(dir, &list);
            let still_wanted = list.iter().any(|ref_| ref_.url == item.url);
            *wanted = list;
            flow = Flow::Restart;
            !still_wanted // still wanted: finish this file, then re-check
        }
        Ok(SmbCacheCmd::Shutdown) | Err(TryRecvError::Disconnected) => {
            flow = Flow::Stop;
            true
        }
        Err(TryRecvError::Empty) => false,
    });
    match result {
        Ok(path) => {
            let _ = tx.send(SmbCacheEvent::Ready {
                url: item.url.clone(),
                path,
            });
        }
        Err(DownloadError::Interrupted) => {}
        Err(DownloadError::Failed(error)) => {
            let _ = tx.send(SmbCacheEvent::Failed {
                url: item.url.clone(),
                error,
            });
        }
    }
    flow
}

/// The one way an SMB original becomes a local file — reuse
/// `<dir>/<hash>.<ext>` when it is already there, else connect, open the
/// source and stream it into a fresh `<hash>.part` (never through a planted
/// symlink), enforce `max_bytes`, and rename only when complete: a `.part`
/// is never handed out. `interrupt` runs between chunks; `true` abandons the
/// download.
///
/// One connection per file: a session dropped mid-window never poisons the
/// next download, and reconnecting costs one round trip.
pub(crate) fn download_file(
    cfg: &SmbClientCfg,
    item: &SmbRef,
    dir: &Path,
    max_bytes: u64,
    mut interrupt: impl FnMut() -> bool,
) -> Result<PathBuf, DownloadError> {
    let url = &item.url;
    let path = dir.join(format!("{}.{}", hash_of(url), ext_for(url)));
    if path.is_file() {
        return Ok(path); // an earlier window (or play) already has it
    }
    // A shutdown — or a window that no longer wants this file — before we
    // even connect: never start a round trip the player already gave up on
    // (`Drop` joins this thread, so an unneeded connect would hold it).
    if interrupt() {
        return Err(DownloadError::Interrupted);
    }
    let failed = |reason: String| Err(DownloadError::Failed(reason));

    let client = SmbClientCfg {
        server: url.server.clone(),
        ..cfg.clone()
    }
    .connect(&url.share)
    .map_err(DownloadError::Failed)?;
    let unc = url
        .unc()
        .map_err(|_| DownloadError::Failed("smb: cannot open file".into()))?;
    let mut source = SmbFileSource::open(&client, &unc)
        .map_err(|error| DownloadError::Failed(error.to_string()))?;

    // Refuse up front when the server declares an over-cap file.
    let len = source.len();
    if len > max_bytes {
        return failed(format!(
            "file is {} — over the {} cache limit",
            size_label(len),
            size_label(max_bytes)
        ));
    }

    let part = dir.join(format!("{}.part", hash_of(url)));
    let mut file = match create_fresh_file(&part) {
        Ok(file) => file,
        Err(reason) => return failed(format!("cache write failed: {reason}")),
    };
    // Every early exit below takes the `.part` with it.
    let abandon = |result: Result<PathBuf, DownloadError>| {
        let _ = fs::remove_file(&part);
        result
    };

    let mut buffer = vec![0u8; CHUNK];
    let mut written: u64 = 0;
    loop {
        if interrupt() {
            return abandon(Err(DownloadError::Interrupted));
        }
        match source.read(&mut buffer) {
            Ok(0) => break, // clean end of file
            Ok(n) => {
                written += n as u64;
                if written > max_bytes {
                    return abandon(failed(format!(
                        "file is over the {} cache limit",
                        size_label(max_bytes)
                    )));
                }
                if let Err(reason) = file.write_all(&buffer[..n]) {
                    return abandon(failed(format!("cache write failed: {reason}")));
                }
            }
            // Transport failure mid-body (the fixed text keeps any URL or
            // credential out of the notice).
            Err(_) => return abandon(failed("smb: read failed".into())),
        }
    }
    drop(file);

    // Complete: `.part` → `<hash>.<ext>` in one rename.
    if let Err(reason) = fs::rename(&part, &path) {
        return abandon(failed(format!("cache write failed: {reason}")));
    }
    Ok(path)
}

/// Delete every cached file (and `.part`) whose URL is not in `wanted`.
/// Errors are ignored on purpose: a file locked by a playing stream simply
/// survives until the next window retries.
fn evict(dir: &Path, wanted: &[SmbRef]) {
    let keep: HashSet<String> = wanted.iter().map(|ref_| hash_of(&ref_.url)).collect();
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.path().is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some((stem, _ext)) = name.split_once('.') else {
            continue; // not one of ours (`<hash>.<ext>` / `<hash>.part`)
        };
        if !keep.contains(stem) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// `<16 hex digits>` of the canonical URL: a file name that is safe on any
/// file system, unique per URL, and carries no server/share/path text (so
/// no credential-shaped name ever reaches the disk).
fn hash_of(url: &SmbUrl) -> String {
    let mut hasher = DefaultHasher::new();
    url.canonical().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// The file extension of the URL's last path segment, ASCII alphanumeric
/// (the decoder probe's hint), falling back to `bin`.
fn ext_for(url: &SmbUrl) -> String {
    url.extension().unwrap_or_else(|| "bin".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jellyfin::cache::sweep_stale_dirs;
    use crate::player::TrackSource;

    /// `smb://nas/Music/<path>`.
    fn url(path: &str) -> SmbUrl {
        SmbUrl {
            server: "nas".into(),
            share: "Music".into(),
            path: path.into(),
        }
    }

    fn smb_track(path: &str) -> Track {
        Track {
            id: Track::next_id(),
            source: TrackSource::Smb { url: url(path) },
            artist: None,
            title: None,
            duration: None,
            bitrate_kbps: None,
        }
    }

    fn local_track(path: &str) -> Track {
        Track {
            id: Track::next_id(),
            source: TrackSource::Local(PathBuf::from(path)),
            artist: None,
            title: None,
            duration: None,
            bitrate_kbps: None,
        }
    }

    fn jellyfin_track(id: &str) -> Track {
        Track {
            id: Track::next_id(),
            source: TrackSource::Remote {
                item_id: id.into(),
                container: None,
            },
            artist: None,
            title: None,
            duration: None,
            bitrate_kbps: None,
        }
    }

    /// The window follows *play* order and counts only SMB rows, in
    /// priority order: current → ahead (nearest first) → behind.
    #[test]
    fn the_window_is_play_order_and_only_smb_tracks() {
        let mut order = PlayOrder::new();
        let tracks = vec![
            smb_track("a.flac"),     // 0
            local_track("/m/b.mp3"), // 1
            smb_track("c.flac"),     // 2
            jellyfin_track("j"),     // 3
            smb_track("d.flac"),     // 4
            smb_track("e.flac"),     // 5
        ];
        order.appended(tracks.len(), Some(0), &mut crate::player::rng::Rng::new(1));

        // current = 0, two ahead → 2 and 4 (1 and 3 are not SMB).
        let window = compute_smb_window(&order, &tracks, Some(0), 2, 0);
        assert_eq!(
            window.iter().map(|r| r.url.canonical()).collect::<Vec<_>>(),
            vec![
                "smb://nas/Music/a.flac",
                "smb://nas/Music/c.flac",
                "smb://nas/Music/d.flac"
            ],
            "current first, then the next SMB tracks — never a local or Jellyfin row"
        );

        // Behind, nearest first: from 4 backwards → 2, then 0. `previous()`
        // reads the *order's own cursor*, so the test moves it the way the
        // player does when a track starts.
        order.rebuild(
            tracks.len(),
            Some(4),
            false,
            &mut crate::player::rng::Rng::new(1),
        );
        let window = compute_smb_window(&order, &tracks, Some(4), 0, 2);
        assert_eq!(
            window.iter().map(|r| r.url.canonical()).collect::<Vec<_>>(),
            vec![
                "smb://nas/Music/d.flac",
                "smb://nas/Music/c.flac",
                "smb://nas/Music/a.flac"
            ]
        );

        // Nothing current (the order's cursor sits "before the start"):
        // the window starts at the top of the play order, and the look-behind
        // finds nothing before it.
        order.rebuild(
            tracks.len(),
            None,
            false,
            &mut crate::player::rng::Rng::new(1),
        );
        let window = compute_smb_window(&order, &tracks, None, 1, 1);
        assert_eq!(
            window.iter().map(|r| r.url.canonical()).collect::<Vec<_>>(),
            vec!["smb://nas/Music/a.flac"],
            "{window:?}"
        );

        // A window over a playlist with no SMB rows at all is empty — the
        // downloader then just evicts everything.
        let only_local = vec![local_track("/m/a.mp3")];
        let mut order = PlayOrder::new();
        order.appended(1, Some(0), &mut crate::player::rng::Rng::new(1));
        assert!(compute_smb_window(&order, &only_local, Some(0), 3, 3).is_empty());
    }

    /// Eviction deletes files (and `.part`s) whose URL left the window, and
    /// never touches anything that is not one of ours by name.
    #[test]
    fn eviction_takes_only_our_files_that_left_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let keep = hash_of(&url("keep.flac"));
        let gone = hash_of(&url("gone.flac"));
        fs::write(dir.path().join(format!("{keep}.flac")), b"keep").unwrap();
        fs::write(dir.path().join(format!("{gone}.flac")), b"gone").unwrap();
        fs::write(dir.path().join(format!("{gone}.part")), b"partial").unwrap();
        fs::write(dir.path().join("readme.txt"), b"not ours").unwrap();
        fs::write(dir.path().join("noext"), b"not ours either").unwrap();

        evict(
            dir.path(),
            &[SmbRef {
                url: url("keep.flac"),
            }],
        );

        let mut names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        // Everything shaped like `<something>.<ext>` and no longer in the
        // window goes — `readme.txt` included: it looks exactly like one of
        // ours, and the directory is ours alone (the plan's rule is "evict
        // what is not wanted"). A name *without* an extension cannot be one
        // of ours and is left alone.
        assert_eq!(names, vec![format!("{keep}.flac"), "noext".to_string()]);
        assert!(
            !dir.path().join(format!("{gone}.part")).exists(),
            "the partial file went too"
        );
    }

    /// Plan 10, step 5: a `.part` file is never ready — the bookkeeping
    /// only ever holds paths a `Ready` event delivered, and those come from
    /// the rename that happens *after* the last byte.
    #[test]
    fn a_part_file_is_never_reported_ready() {
        let parent = tempfile::tempdir().unwrap();
        let mut cache = SmbCache::new(
            SmbClientCfg::new("127.0.0.1", "", ""),
            Some(parent.path().to_path_buf()),
        )
        .expect("cache dir");
        let target = url("album/song.flac");

        // The in-flight name for this URL sits in the directory…
        let part = cache.dir().join(format!("{}.part", hash_of(&target)));
        fs::write(&part, b"half a file").unwrap();
        // …and is invisible to the player.
        assert_eq!(cache.path_if_ready(&target), None, "a .part is never ready");
        cache.poll();
        assert_eq!(cache.path_if_ready(&target), None, "still nothing ready");

        // A `Ready` event is the only thing that can make a path appear, and
        // it carries the *renamed* file.
        let ready_path = cache.dir().join(format!("{}.flac", hash_of(&target)));
        fs::rename(&part, &ready_path).unwrap();
        let url2 = target.clone();
        cache.insert_ready(&url2, ready_path.clone());
        assert_eq!(cache.path_if_ready(&target), Some(ready_path.clone()));

        // Evicting the URL drops the bookkeeping with it.
        cache.set_window(Vec::new());
        assert_eq!(cache.path_if_ready(&target), None, "evicted");
    }

    /// The window's file name: a hash of the canonical URL plus the real
    /// extension (the decoder probe's hint), with no share text on disk.
    #[test]
    fn file_names_are_hashes_with_the_real_extension() {
        let flac = url("Album/01 Song.FLAC");
        assert_eq!(ext_for(&flac), "flac", "extension, lowercased");
        assert_eq!(hash_of(&flac).len(), 16, "16 hex digits");
        assert_eq!(hash_of(&flac), hash_of(&flac), "stable per URL");
        assert_ne!(hash_of(&flac), hash_of(&url("Album/02 Song.FLAC")));
        assert!(
            !hash_of(&flac).contains('/'),
            "safe as a file name: {}",
            hash_of(&flac)
        );
        // No extension at all → the fallback the decoder can still sniff.
        assert_eq!(ext_for(&url("Album/noext")), "bin");
        // A path with no last segment is not a panic either.
        assert_eq!(ext_for(&url("")), "bin");
    }

    /// Plan 10, step 5: the startup sweep removes only stale
    /// `ampwire-smb-<pid>` directories — never a live one, never an
    /// unrelated name, and never the *Jellyfin* window's directory (which
    /// carries a different prefix, in the same temp dir).
    #[test]
    fn the_startup_sweep_removes_only_stale_smb_dirs() {
        let parent = tempfile::tempdir().unwrap();
        let stale = parent.path().join("ampwire-smb-99999");
        let fresh = parent.path().join("ampwire-smb-88888");
        let unrelated = parent.path().join("unrelated");
        let not_a_pid = parent.path().join("ampwire-smb-newest");
        let other_window = parent.path().join("ampwire-jellyfin-99999");
        for dir in [&stale, &fresh, &unrelated, &not_a_pid, &other_window] {
            fs::create_dir_all(dir).unwrap();
        }
        age_dir(&stale, Duration::from_secs(25 * 3600)); // > 24 h
        age_dir(&not_a_pid, Duration::from_secs(25 * 3600)); // wrong pattern
        age_dir(&other_window, Duration::from_secs(25 * 3600)); // other window

        let removed = sweep_stale_dirs(parent.path(), STALE_AGE, DIR_PREFIX);
        assert_eq!(removed, 1, "exactly the stale crash leftover");
        assert!(!stale.exists(), "crash leftover swept");
        assert!(fresh.exists(), "a live cache is never touched");
        assert!(unrelated.exists(), "name pattern is strict");
        assert!(not_a_pid.exists(), "only `<pid>` suffixes count as ours");
        assert!(
            other_window.exists(),
            "the Jellyfin window has its own prefix"
        );
    }

    /// Backdate a directory so the sweep sees it as a crash leftover.
    fn age_dir(dir: &Path, age: Duration) {
        let then = std::time::SystemTime::now() - age;
        fs::File::open(dir)
            .expect("open the dir to age it")
            .set_modified(then)
            .expect("set the dir mtime");
    }

    /// The cache owns a private directory named after this process, and
    /// removes it (and every file in it) when it goes away.
    #[test]
    fn cache_creates_its_private_dir_and_removes_it_on_drop() {
        let parent = tempfile::tempdir().unwrap();
        let dir;
        {
            let cache = SmbCache::new(
                SmbClientCfg::new("127.0.0.1", "", ""),
                Some(parent.path().to_path_buf()),
            )
            .expect("cache dir is creatable");
            dir = cache.dir().to_path_buf();
            assert!(dir.is_dir());
            assert_eq!(dir.parent(), Some(parent.path()));
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            assert!(
                name.strip_prefix(DIR_PREFIX)
                    .is_some_and(|pid| pid == std::process::id().to_string()),
                "named after this process: {name}"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(&dir).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o700, "private to the user");
            }
            fs::write(dir.join("leftover.flac"), b"x").unwrap();
        }
        assert!(!dir.exists(), "the directory goes with the cache");
    }

    /// `SmbCacheEvent::Failed` is what the player turns into the plan-03
    /// skip path, so the shapes are pinned here (the events themselves are
    /// Plan 10, step 5 (real server): the downloader really turns a share
    /// file into `<hash>.<ext>`, reuses it on a second call, and refuses an
    /// over-cap file before writing a byte.
    #[test]
    #[ignore = "needs a live Samba server (see module docs)"]
    fn smb_spike_cache_downloads_a_share_file() {
        let Some(target) = crate::smb::smoke::target() else {
            return eprintln!("smb_spike: AMPWIRE_SMB_TEST_SERVER/SHARE not set, skipped");
        };
        let root = SmbUrl {
            server: target.server.clone(),
            share: target.share.clone(),
            path: target.path.clone(),
        };
        let cfg = SmbClientCfg::for_url(&root, None);
        let client = cfg.connect(&root.share).expect("connect");
        let unc = root.unc().expect("unc");
        // Bounded: the target may be a whole share.
        let entries = crate::smb::smoke::walk(&client, &unc, 200);
        let Some(file) = entries
            .iter()
            .filter(|entry| !entry.is_dir && crate::smb::smoke::is_audio(&entry.path))
            .max_by_key(|entry| entry.size)
        else {
            return eprintln!("smb_spike: no audio file under this path, skipped");
        };
        // The walk reports paths relative to the URL's own directory.
        let path = if root.path.is_empty() {
            file.path.clone()
        } else if file.path.is_empty() {
            root.path.clone()
        } else {
            format!("{}/{}", root.path, file.path)
        };
        let url = SmbUrl {
            server: root.server.clone(),
            share: root.share.clone(),
            path,
        };
        eprintln!("smb_spike: downloading {}", url.canonical());
        let item = SmbRef { url: url.clone() };

        let parent = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();
        let downloaded =
            download_file(&cfg, &item, parent.path(), MAX_FILE_BYTES, || false).expect("downloads");
        eprintln!(
            "smb_spike: {} bytes in {:?}",
            fs::metadata(&downloaded).expect("size").len(),
            started.elapsed()
        );
        assert_eq!(
            downloaded,
            parent
                .path()
                .join(format!("{}.{}", hash_of(&url), ext_for(&url))),
            "<hash>.<ext>, never the share's own name"
        );
        assert_eq!(
            fs::metadata(&downloaded).expect("size").len(),
            file.size,
            "every byte arrived"
        );
        assert!(
            fs::read_dir(parent.path())
                .expect("dir")
                .flatten()
                .all(|entry| !entry.file_name().to_string_lossy().ends_with(".part")),
            "no .part survives a completed download"
        );

        // Reuse: the file is already there, so the second call returns it
        // without touching the network (it is instant).
        let again =
            download_file(&cfg, &item, parent.path(), MAX_FILE_BYTES, || false).expect("reuse");
        assert_eq!(again, downloaded, "an earlier window already has it");

        // Over-cap: refused before a byte is written.
        let other = tempfile::tempdir().unwrap();
        let error = download_file(&cfg, &item, other.path(), 10, || false)
            .expect_err("the cap must refuse it");
        match error {
            DownloadError::Failed(reason) => {
                assert!(reason.contains("over the"), "reason: {reason}");
                assert!(!reason.contains("nas") && !reason.contains("/"), "{reason}");
            }
            other => panic!("expected a Failed cap notice: {other:?}"),
        }
        assert!(
            fs::read_dir(other.path())
                .expect("dir")
                .flatten()
                .next()
                .is_none(),
            "nothing written for a refused file"
        );
    }

    /// plain data, and the fixed lines come from [`download_file`]).
    #[test]
    fn events_carry_the_url_and_a_line_the_player_can_show() {
        let ready = SmbCacheEvent::Ready {
            url: url("a.flac"),
            path: PathBuf::from("/tmp/x.flac"),
        };
        let failed = SmbCacheEvent::Failed {
            url: url("a.flac"),
            error: "smb: cannot connect".into(),
        };
        match ready {
            SmbCacheEvent::Ready { url, path } => {
                assert_eq!(url.canonical(), "smb://nas/Music/a.flac");
                assert_eq!(path, PathBuf::from("/tmp/x.flac"));
            }
            other => panic!("expected Ready: {other:?}"),
        }
        match failed {
            SmbCacheEvent::Failed { error, .. } => {
                assert!(
                    !error.contains("nas") && !error.contains("Music"),
                    "{error}"
                );
            }
            other => panic!("expected Failed: {other:?}"),
        }
        // The command shapes the player sends.
        let window = SmbCacheCmd::Window(vec![SmbRef { url: url("a.flac") }]);
        assert!(matches!(window, SmbCacheCmd::Window(ref w) if w.len() == 1));
        assert!(matches!(SmbCacheCmd::Shutdown, SmbCacheCmd::Shutdown));
    }
}
