//! Rolling download window for `mode = "cache"` (plan 08, step 5).
//!
//! One dedicated **downloader thread** (never the engine thread, so Stop and
//! Next stay responsive during downloads) owns a private
//! `ampwire-jellyfin-<pid>` directory (`0700` on unix): it keeps the current
//! remote track plus the next `cache_ahead` and previous `cache_behind`
//! remote tracks in play order on disk, evicting everything else.
//!
//! Wire protocol: [`Cache`] (UI side) sends [`CacheCmd::Window`]s over one
//! channel and reads [`CacheEvent`]s from another. Downloads land in
//! `<id>.part` and are renamed only when complete — a `.part` file is never
//! reported as ready and therefore never decoded.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use crate::jellyfin::Client;
use crate::player::order::PlayOrder;
use crate::player::{Track, TrackSource};

/// Hard per-file cap: 512 MiB (the plan's v1 limit; the window is therefore
/// at most 7 × 512 MiB in the worst case).
pub const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
/// Read/write chunk while downloading (the plan's 256 KiB).
const CHUNK: usize = 256 * 1024;
/// Directories left behind by a crash are swept after this long.
pub const STALE_AGE: Duration = Duration::from_secs(24 * 3600);
/// Directory-name prefix this app creates — the *only* thing the sweep and
/// the eviction will ever touch by name.
const DIR_PREFIX: &str = "ampwire-jellyfin-";

/// One remote track the cache should hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRef {
    pub item_id: String,
    pub container: Option<String>,
}

impl RemoteRef {
    /// The ref behind a [`Track`], or `None` for local tracks.
    pub fn from_track(track: &Track) -> Option<RemoteRef> {
        match &track.source {
            TrackSource::Remote { item_id, container } => Some(RemoteRef {
                item_id: item_id.clone(),
                container: container.clone(),
            }),
            TrackSource::Local(_) => None,
        }
    }
}

/// UI → downloader thread.
#[derive(Debug)]
pub enum CacheCmd {
    /// New wanted set, already in priority order: current, ahead (nearest
    /// first), behind (nearest first).
    Window(Vec<RemoteRef>),
    Shutdown,
}

/// Downloader → UI thread.
#[derive(Debug)]
pub enum CacheEvent {
    Ready { item_id: String, path: PathBuf },
    Failed { item_id: String, error: String },
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

/// The temp-dir window. Dropping it stops the thread **and** removes the
/// directory it created (errors ignored — the OS sweep is the backstop).
pub struct Cache {
    tx: Sender<CacheCmd>,
    rx: Receiver<CacheEvent>,
    join: Option<JoinHandle<()>>,
    dir: PathBuf,
    /// Item id → file on disk, kept in step with the window. A small mutex
    /// (not `&mut`) so [`Cache::set_window`] can stay `&self` as the plan
    /// specifies and be called straight from an immutable borrow.
    ready: Mutex<HashMap<String, PathBuf>>,
}

impl Cache {
    /// Create `ampwire-jellyfin-<pid>` under `dir_parent` (the system temp
    /// dir when `None`), sweep stale siblings, spawn the downloader.
    pub fn new(client: Client, dir_parent: Option<PathBuf>) -> std::io::Result<Cache> {
        Self::with_limit(client, dir_parent, MAX_FILE_BYTES)
    }

    /// [`Cache::new`] with an injectable size cap (the test uses 1 KiB).
    pub(crate) fn with_limit(
        client: Client,
        dir_parent: Option<PathBuf>,
        max_bytes: u64,
    ) -> std::io::Result<Cache> {
        let parent = dir_parent.unwrap_or_else(std::env::temp_dir);
        sweep_stale_dirs(&parent, STALE_AGE);
        let dir = parent.join(format!("{DIR_PREFIX}{}", std::process::id()));
        fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Best-effort: the cache holds the user's music, not world data.
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        }

        let (cmd_tx, cmd_rx) = channel();
        let (ev_tx, ev_rx) = channel();
        let thread_dir = dir.clone();
        let join = std::thread::Builder::new()
            .name("ampwire-jellyfin-cache".into())
            .spawn(move || run(client, thread_dir, max_bytes, cmd_rx, ev_tx))?;

        Ok(Cache {
            tx: cmd_tx,
            rx: ev_rx,
            join: Some(join),
            dir,
            ready: Mutex::new(HashMap::new()),
        })
    }

    /// Hand over a new wanted set (priority order). Bookkeeping for ids
    /// about to be evicted is dropped right here; the thread deletes their
    /// files in the same pass.
    pub fn set_window(&self, wanted: Vec<RemoteRef>) {
        {
            let mut ready = self.ready.lock().expect("cache ready lock");
            ready.retain(|id, _| wanted.iter().any(|ref_| ref_.item_id == *id));
        }
        let _ = self.tx.send(CacheCmd::Window(wanted));
    }

    /// The local file for `item_id`, if it finished downloading (and still
    /// exists — eviction can race a stale bookkeeping entry).
    pub fn path_if_ready(&self, item_id: &str) -> Option<PathBuf> {
        let mut ready = self.ready.lock().expect("cache ready lock");
        let path = ready.get(item_id)?.clone();
        if path.exists() {
            Some(path)
        } else {
            ready.remove(item_id);
            None
        }
    }

    /// Drain downloader events (also folds `Ready` into the bookkeeping).
    pub fn poll(&mut self) -> Vec<CacheEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            if let CacheEvent::Ready { item_id, path } = &event {
                self.ready
                    .lock()
                    .expect("cache ready lock")
                    .insert(item_id.clone(), path.clone());
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

    /// Test support: pretend `item_id` is already on disk, so the player's
    /// cache-hit path can be exercised without a server.
    #[cfg(test)]
    pub fn insert_ready(&mut self, item_id: &str, path: PathBuf) {
        self.ready
            .lock()
            .expect("cache ready lock")
            .insert(item_id.to_string(), path);
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        let _ = self.tx.send(CacheCmd::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        // Only *our* directory — never its parent, never a sibling's.
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Delete sibling `ampwire-jellyfin-<pid>` directories older than `max_age`
/// (crash leftovers). Anything not matching our exact name pattern — other
/// processes' live caches, unrelated dirs — is left alone. Returns how many
/// were removed.
pub fn sweep_stale_dirs(parent: &Path, max_age: Duration) -> usize {
    let Ok(entries) = fs::read_dir(parent) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !is_our_dir_name(&name) || !entry.path().is_dir() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue; // a clock in the future: treat as fresh
        };
        if age >= max_age && fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// `ampwire-jellyfin-<pid>` and nothing else.
fn is_our_dir_name(name: &std::ffi::OsStr) -> bool {
    let Some(pid) = name.to_str().and_then(|n| n.strip_prefix(DIR_PREFIX)) else {
        return false;
    };
    !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())
}

/// The window in play order: current (if remote) → `ahead` next remote
/// tracks → `behind` previous ones, nearest first, deduplicated. Local
/// tracks are skipped and never counted, so shuffle (which reorders the
/// play sequence, not the playlist) is respected automatically.
pub fn compute_window(
    order: &PlayOrder,
    tracks: &[Track],
    current: Option<usize>,
    ahead: usize,
    behind: usize,
) -> Vec<RemoteRef> {
    let mut seen: HashSet<usize> = HashSet::new();
    let mut wanted: Vec<usize> = Vec::new();

    let mut take = |index: usize, wanted: &mut Vec<usize>| {
        if seen.insert(index) && tracks.get(index).is_some_and(Track::is_remote) {
            wanted.push(index);
        }
    };

    if let Some(current) = current {
        take(current, &mut wanted);
    }
    // `N` large enough to find `ahead` *remote* tracks among everything.
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
        .filter_map(|index| RemoteRef::from_track(&tracks[index]))
        .collect()
}

/// The downloader thread: wait for a window → evict → fetch in priority
/// order, restarting whenever a newer window arrives.
fn run(
    client: Client,
    dir: PathBuf,
    max_bytes: u64,
    rx: Receiver<CacheCmd>,
    tx: Sender<CacheEvent>,
) {
    let mut wanted: Vec<RemoteRef>;
    loop {
        // 1. Block until the player hands over a window.
        match rx.recv() {
            Ok(CacheCmd::Window(list)) => {
                evict(&dir, &list);
                wanted = list;
            }
            Ok(CacheCmd::Shutdown) | Err(_) => return,
        }

        // 2. Fetch the list in priority order (current first).
        let mut index = 0;
        while index < wanted.len() {
            // A newer window that arrived between items preempts ours.
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    CacheCmd::Window(list) => {
                        evict(&dir, &list);
                        wanted = list;
                        index = 0;
                    }
                    CacheCmd::Shutdown => return,
                }
            }
            let item = wanted[index].clone();
            match fetch(&client, &dir, &item, max_bytes, &rx, &tx, &mut wanted) {
                Flow::Continue => index += 1,
                Flow::Restart => index = 0,
                Flow::Stop => return,
            }
        }
        // 3. Window satisfied → back to step 1 and wait.
    }
}

/// Fetch one item: reuse the file if it is already there, else stream the
/// original into `<id>.part` (checking for newer windows between chunks),
/// then rename atomically and report `Ready`.
fn fetch(
    client: &Client,
    dir: &Path,
    item: &RemoteRef,
    max_bytes: u64,
    rx: &Receiver<CacheCmd>,
    tx: &Sender<CacheEvent>,
    wanted: &mut Vec<RemoteRef>,
) -> Flow {
    let path = dir.join(format!("{}.{}", item.item_id, ext_for(item)));
    if path.is_file() {
        // Already on disk (an earlier window had it): cheap re-report.
        let _ = tx.send(CacheEvent::Ready {
            item_id: item.item_id.clone(),
            path: path.clone(),
        });
        return Flow::Continue;
    }

    let failed = |error: String| {
        let _ = tx.send(CacheEvent::Failed {
            item_id: item.item_id.clone(),
            error,
        });
    };

    let response = match client.open_original(&item.item_id, None) {
        Ok(response) => response,
        Err(reason) => {
            failed(reason);
            return Flow::Continue;
        }
    };
    // Refuse up front when the server declares an over-cap file.
    if let Some(len) = content_length(&response)
        && len > max_bytes
    {
        failed(format!(
            "file is {} — over the {} cache limit",
            size_label(len),
            size_label(max_bytes)
        ));
        return Flow::Continue;
    }

    let part = dir.join(format!("{}.part", item.item_id));
    let mut reader = response.into_body().into_reader();
    let cleanup = |part: &Path| {
        let _ = fs::remove_file(part);
    };
    let mut file = match fs::File::create(&part) {
        Ok(file) => file,
        Err(reason) => {
            cleanup(&part);
            failed(format!("cache write failed: {reason}"));
            return Flow::Continue;
        }
    };

    let mut buffer = vec![0u8; CHUNK];
    let mut written: u64 = 0;
    let mut window_changed = false;
    loop {
        // A newer window (or shutdown) between chunks: apply it right away.
        match rx.try_recv() {
            Ok(CacheCmd::Window(list)) => {
                evict(dir, &list);
                let still_wanted = list.iter().any(|ref_| ref_.item_id == item.item_id);
                *wanted = list;
                if !still_wanted {
                    cleanup(&part);
                    return Flow::Restart;
                }
                window_changed = true; // finish this file, then re-check
            }
            Ok(CacheCmd::Shutdown) => {
                cleanup(&part);
                return Flow::Stop;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                cleanup(&part);
                return Flow::Stop;
            }
        }

        match reader.read(&mut buffer) {
            Ok(0) => break, // clean end of body
            Ok(n) => {
                written += n as u64;
                if written > max_bytes {
                    cleanup(&part);
                    failed(format!(
                        "file is over the {} cache limit",
                        size_label(max_bytes)
                    ));
                    return Flow::Continue;
                }
                if let Err(reason) = file.write_all(&buffer[..n]) {
                    cleanup(&part);
                    failed(format!("cache write failed: {reason}"));
                    return Flow::Continue;
                }
            }
            Err(_) => {
                // Transport failure mid-body (the fixed text keeps any URL
                // or key out of the notice).
                cleanup(&part);
                failed("jellyfin: cannot reach server".into());
                return Flow::Continue;
            }
        }
    }
    drop(file);

    // Complete: `.part` → `<id>.<ext>` in one rename, then announce it.
    if let Err(reason) = fs::rename(&part, &path) {
        cleanup(&part);
        failed(format!("cache write failed: {reason}"));
        return Flow::Continue;
    }
    let _ = tx.send(CacheEvent::Ready {
        item_id: item.item_id.clone(),
        path,
    });
    if window_changed {
        Flow::Restart
    } else {
        Flow::Continue
    }
}

/// Delete every cached file (and `.part`) whose item id is not in `wanted`.
/// Errors are ignored on purpose: a file locked by a playing stream (Windows)
/// simply survives until the next window retries.
fn evict(dir: &Path, wanted: &[RemoteRef]) {
    let keep: HashSet<&str> = wanted.iter().map(|r| r.item_id.as_str()).collect();
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
            continue; // not one of ours (`<id>.<ext>` / `<id>.part`)
        };
        if !keep.contains(stem) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// The file extension for an item: the media source's container (`m4a`),
/// stripped to ASCII alphanumerics, falling back to `bin`.
fn ext_for(item: &RemoteRef) -> String {
    super::container_ext(item.container.as_deref())
}

/// `512 MiB` / `14 KiB` / `900 B` — human caps for the cache-limit notices
/// (the injected test cap is 1 KiB, so MiB alone would round to zero).
fn size_label(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{} MiB", bytes / (1024 * 1024))
    } else if bytes >= 1024 {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

/// `Content-Length` of a response, when the server sent one.
fn content_length(response: &ureq::http::Response<ureq::Body>) -> Option<u64> {
    response
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::JellyfinMode;
    use crate::player::Track;
    use crate::player::order::PlayOrder;
    use crate::player::rng::Rng;

    fn local(path: &str) -> Track {
        Track {
            id: Track::next_id(),
            source: TrackSource::Local(PathBuf::from(path)),
            artist: None,
            title: Some(path.to_string()),
            duration: None,
            bitrate_kbps: None,
        }
    }

    fn remote(id: &str) -> Track {
        Track {
            id: Track::next_id(),
            source: TrackSource::Remote {
                item_id: id.to_string(),
                container: Some("flac".into()),
            },
            artist: Some(format!("artist-{id}")),
            title: Some(format!("title-{id}")),
            duration: None,
            bitrate_kbps: None,
        }
    }

    /// Ordered play order over `len` tracks, cursor on `current`.
    fn ordered(len: usize, current: Option<usize>) -> PlayOrder {
        let mut rng = Rng::new(1);
        let mut order = PlayOrder::new();
        order.rebuild(len, current, false, &mut rng);
        order
    }

    /// Shuffled play order over `len` tracks, cursor on `current`.
    fn shuffled(len: usize, current: Option<usize>, seed: u64) -> PlayOrder {
        let mut rng = Rng::new(seed);
        let mut order = PlayOrder::new();
        order.rebuild(len, current, true, &mut rng);
        order
    }

    fn ids(window: &[RemoteRef]) -> Vec<&str> {
        window.iter().map(|r| r.item_id.as_str()).collect()
    }

    // --- compute_window (plan 08, step 7) ---

    #[test]
    fn window_is_current_ahead_behind_and_skips_local_tracks() {
        // [L, R-a, L, R-b, R-c], current = the remote at index 1.
        let tracks = vec![
            local("/x/first.flac"),
            remote("a"),
            local("/x/third.flac"),
            remote("b"),
            remote("c"),
        ];
        let order = ordered(tracks.len(), Some(1));
        // Local tracks are skipped *and* don't count toward ahead/behind.
        assert_eq!(
            ids(&compute_window(&order, &tracks, Some(1), 3, 2)),
            ["a", "b", "c"],
            "current first, then the next remotes; the local behind is gone"
        );
        // The current track is in front even when ahead/behind are zero.
        assert_eq!(ids(&compute_window(&order, &tracks, Some(1), 0, 0)), ["a"]);
        // Behind reaches back over the local track at index 0 — but locals
        // never enter the window (cursor on the last track, as the order
        // would really be after playing up to it).
        let order_end = ordered(tracks.len(), Some(4));
        assert_eq!(
            ids(&compute_window(&order_end, &tracks, Some(4), 0, 3)),
            ["c", "b", "a"]
        );
    }

    #[test]
    fn window_with_nothing_current_starts_from_the_top() {
        let tracks = vec![local("/x/0.flac"), remote("a"), remote("b")];
        let order = ordered(tracks.len(), None);
        assert_eq!(
            ids(&compute_window(&order, &tracks, None, 2, 2)),
            ["a", "b"]
        );
        // Nothing current and no look-ahead: empty, never a panic.
        assert!(compute_window(&order, &tracks, None, 0, 0).is_empty());
    }

    #[test]
    fn window_at_the_list_edges_does_not_wrap() {
        // Current is the last track: nothing is ahead, only the previous.
        let tracks = vec![remote("a"), remote("b"), remote("c")];
        let order = ordered(tracks.len(), Some(2));
        assert_eq!(
            ids(&compute_window(&order, &tracks, Some(2), 3, 3)),
            ["c", "b", "a"]
        );
        assert_eq!(ids(&compute_window(&order, &tracks, Some(2), 3, 0)), ["c"]);

        // Current is the first: nothing is behind (the "previous" peek at
        // the start is the restart target, which is the current track).
        let order = ordered(tracks.len(), Some(0));
        assert_eq!(
            ids(&compute_window(&order, &tracks, Some(0), 2, 3)),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn window_follows_shuffle_not_playlist_order() {
        let tracks: Vec<Track> = (0..6).map(|i| remote(&format!("r{i}"))).collect();
        let order = shuffled(tracks.len(), Some(0), 7);
        let window = compute_window(&order, &tracks, Some(0), 6, 6);
        // All-remote list: the window is the whole play order, current first.
        let mut expected: Vec<String> = vec!["r0".to_string()];
        expected.extend(
            order
                .upcoming(tracks.len())
                .iter()
                .map(|&i| RemoteRef::from_track(&tracks[i]).expect("remote").item_id),
        );
        assert_eq!(
            ids(&window),
            expected.iter().map(String::as_str).collect::<Vec<_>>(),
            "window order = play order"
        );
        assert_eq!(window.len(), 6, "every remote track, deduplicated");
    }

    #[test]
    fn window_of_an_all_local_playlist_is_empty() {
        let tracks: Vec<Track> = (0..5).map(|i| local(&format!("/x/{i}.flac"))).collect();
        let order = ordered(tracks.len(), Some(2));
        assert!(compute_window(&order, &tracks, Some(2), 3, 3).is_empty());
        assert!(compute_window(&order, &tracks, None, 10, 10).is_empty());
    }

    // --- directory sweep + lifecycle (plan 08, Decisions) ---

    /// Age a directory to `age` (test support for the 24 h sweep).
    fn age_dir(path: &Path, age: Duration) {
        let then = SystemTime::now() - age;
        fs::File::open(path)
            .expect("open the dir to age it")
            .set_modified(then)
            .expect("set the dir mtime");
    }

    #[test]
    fn the_startup_sweep_removes_only_stale_our_dirs() {
        let parent = tempfile::tempdir().unwrap();
        let stale = parent.path().join("ampwire-jellyfin-99999");
        let fresh = parent.path().join("ampwire-jellyfin-88888");
        let unrelated = parent.path().join("unrelated");
        let not_a_pid = parent.path().join("ampwire-jellyfin-newest");
        for dir in [&stale, &fresh, &unrelated, &not_a_pid] {
            fs::create_dir_all(dir).unwrap();
        }
        age_dir(&stale, Duration::from_secs(25 * 3600)); // > 24 h
        age_dir(&not_a_pid, Duration::from_secs(25 * 3600)); // wrong pattern

        let removed = sweep_stale_dirs(parent.path(), STALE_AGE);
        assert_eq!(removed, 1, "exactly the stale crash leftover");
        assert!(!stale.exists(), "crash leftover swept");
        assert!(fresh.exists(), "a live cache is never touched");
        assert!(unrelated.exists(), "name pattern is strict");
        assert!(not_a_pid.exists(), "only `<pid>` suffixes count as ours");
    }

    #[test]
    fn cache_creates_its_private_dir_and_removes_it_on_drop() {
        let parent = tempfile::tempdir().unwrap();
        let cfg = crate::config::JellyfinConfig {
            url: "http://127.0.0.1:9".into(),
            api_key: "k".into(),
            user: None,
            mode: JellyfinMode::Cache,
            cache_ahead: 3,
            cache_behind: 3,
            cache_dir: None,
            notice: None,
        };
        let dir;
        {
            let cache = Cache::new(Client::new(&cfg), Some(parent.path().to_path_buf()))
                .expect("cache dir is creatable");
            dir = cache.dir().to_path_buf();
            assert!(dir.is_dir());
            assert_eq!(
                dir.parent(),
                Some(parent.path()),
                "parent override is honored"
            );
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            assert!(
                name.starts_with("ampwire-jellyfin-")
                    && name
                        .strip_prefix("ampwire-jellyfin-")
                        .is_some_and(|pid| pid == std::process::id().to_string()),
                "named after this process: {name}"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(&dir).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o700, "private to the user");
            }
        }
        assert!(!dir.exists(), "Drop removes the directory it created");
        assert!(
            parent.path().read_dir().unwrap().next().is_none(),
            "nothing left behind in the parent"
        );
    }

    // --- against the mock server (plan 08, step 7 e + f) ---

    use std::time::Instant;

    use crate::jellyfin::test_server::{KEY, MockServer};

    /// A wanted ref with the fixture's container.
    fn remote_ref(id: &str) -> RemoteRef {
        RemoteRef {
            item_id: id.to_string(),
            container: Some("flac".into()),
        }
    }

    /// Poll (draining events) until `pred` holds, or give up at the
    /// deadline. Returns what `pred` last saw.
    fn wait_for(cache: &mut Cache, deadline: Duration, pred: impl Fn(&mut Cache) -> bool) -> bool {
        let started = Instant::now();
        while started.elapsed() < deadline {
            cache.poll();
            if pred(cache) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// Plan 08, step 7 (e): a 3-item window downloads, a shifted window
    /// evicts the dropped item and fetches the new one, and items already
    /// on disk are re-reported without another HTTP request.
    #[test]
    fn the_window_downloads_evicts_and_reuses_files() {
        let server = MockServer::start();
        let parent = tempfile::tempdir().unwrap();
        let mut cache =
            Cache::new(server.client(KEY), Some(parent.path().to_path_buf())).expect("cache");

        cache.set_window(vec![remote_ref("a"), remote_ref("b"), remote_ref("c")]);
        assert!(
            wait_for(&mut cache, Duration::from_secs(10), |cache| {
                ["a", "b", "c"]
                    .iter()
                    .all(|id| cache.path_if_ready(id).is_some())
            }),
            "the three items download: {:?}",
            ["a", "b", "c"]
                .iter()
                .map(|id| cache.path_if_ready(id))
                .collect::<Vec<_>>()
        );
        for id in ["a", "b", "c"] {
            let path = cache.path_if_ready(id).expect("ready above");
            assert_eq!(
                path.file_name().unwrap().to_string_lossy(),
                format!("{id}.flac"),
                "named <id>.<container>"
            );
            assert!(path.is_file());
        }

        // Shift the window: `a` drops out, `d` joins.
        cache.set_window(vec![remote_ref("b"), remote_ref("c"), remote_ref("d")]);
        assert!(
            wait_for(&mut cache, Duration::from_secs(10), |cache| {
                cache.path_if_ready("d").is_some()
            }),
            "the new item downloads"
        );
        // The new window was applied *before* its download started, so
        // seeing `d` ready also proves `a` was evicted.
        assert_eq!(cache.path_if_ready("a"), None, "a left the window");
        assert!(!cache.dir().join("a.flac").exists(), "a's file is deleted");
        assert!(cache.dir().join("b.flac").exists(), "b survives");
        assert!(
            !cache.dir().join("a.part").exists(),
            "no stray partials either"
        );

        // Re-announcing the same window re-reports the files on disk —
        // a `Ready` arrives, but not a single new HTTP request.
        let requests_before = server.log().len();
        cache.set_window(vec![remote_ref("b"), remote_ref("c"), remote_ref("d")]);
        let started = Instant::now();
        let mut re_reported = false;
        while started.elapsed() < Duration::from_secs(5) && !re_reported {
            for event in cache.poll() {
                if let CacheEvent::Ready { item_id, .. } = event {
                    re_reported = re_reported || item_id == "b";
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(re_reported, "an on-disk item is re-announced as Ready");
        assert_eq!(
            server.log().len(),
            requests_before,
            "…without fetching it again"
        );
    }

    /// Plan 08, step 7 (e): a download in flight is a `.part` file, which
    /// is *never* reported ready (and therefore never decoded).
    #[test]
    fn a_part_file_is_never_reported_ready() {
        let server = MockServer::start();
        // Hold the body back long enough to observe the in-flight state.
        server.set_audio_delay(Duration::from_millis(1500));
        let parent = tempfile::tempdir().unwrap();
        let mut cache =
            Cache::new(server.client(KEY), Some(parent.path().to_path_buf())).expect("cache");

        cache.set_window(vec![remote_ref("slow")]);
        let part = cache.dir().join("slow.part");
        let started = Instant::now();
        while !part.exists() && started.elapsed() < Duration::from_secs(5) {
            cache.poll();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(part.exists(), "the download is really in flight");
        cache.poll();
        assert_eq!(
            cache.path_if_ready("slow"),
            None,
            "a .part file is never ready"
        );

        // Once complete (renamed atomically) it is.
        assert!(
            wait_for(&mut cache, Duration::from_secs(15), |cache| {
                cache.path_if_ready("slow").is_some()
            }),
            "the delayed download finishes"
        );
        assert!(!part.exists(), "the .part was renamed away");
        let ready = cache.path_if_ready("slow").expect("ready");
        assert_eq!(ready.file_name().unwrap().to_string_lossy(), "slow.flac");
    }

    /// Plan 08, step 7 (f): the size cap (injected as 1 KiB here) refuses
    /// the file before anything lands on disk.
    #[test]
    fn the_size_cap_refuses_a_bigger_file() {
        let server = MockServer::start();
        let parent = tempfile::tempdir().unwrap();
        let mut cache = Cache::with_limit(
            server.client(KEY),
            Some(parent.path().to_path_buf()),
            1024, // 1 KiB: the fixture is ~14 KiB
        )
        .expect("cache");

        cache.set_window(vec![remote_ref("big")]);
        let started = Instant::now();
        let mut error = None;
        while started.elapsed() < Duration::from_secs(5) && error.is_none() {
            for event in cache.poll() {
                if let CacheEvent::Failed {
                    item_id,
                    error: reason,
                } = event
                    && item_id == "big"
                {
                    error = Some(reason);
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let error = error.expect("the oversized download is refused");
        assert!(
            error.contains("over the 1 KiB cache limit"),
            "cap in the notice: {error}"
        );
        assert!(!error.contains(KEY), "no key in the error: {error}");

        assert_eq!(cache.path_if_ready("big"), None, "nothing became ready");
        assert!(!cache.dir().join("big.flac").exists(), "no file");
        assert!(!cache.dir().join("big.part").exists(), "no partial left");
    }
}
