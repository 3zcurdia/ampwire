//! The downloader loop shared by the Jellyfin and SMB caches (plans 08 and
//! 10): wait for a rolling window, evict what it dropped, fetch its items in
//! priority order, and start over at once when a newer window arrives.
//!
//! Each cache supplies only what really differs: how to evict, how to match
//! an item against a window, how to download one item, and what event to
//! report. Nothing here touches the network, the filesystem or the player.

use std::sync::mpsc::{Receiver, TryRecvError};

/// Player → downloader thread: the new rolling window, or shutdown.
#[derive(Debug)]
pub enum WindowCmd<R> {
    /// The window to keep, in priority order (the current track first).
    Window(Vec<R>),
    Shutdown,
}

/// What the loop does after one item.
pub enum Flow {
    /// Finished (or failed) this item: continue down the list.
    Continue,
    /// A newer window arrived: start over from its first item.
    Restart,
    /// Shutdown: leave the loop.
    Stop,
}

/// Why one item did not come down. The caller's `interrupt` asked to stop
/// (the `.part` file is already gone), or the item failed for a reason the
/// user may see — key-free and fixed-text.
#[derive(Debug)]
pub enum DownloadError {
    Interrupted,
    Failed(String),
}

/// The downloader thread: wait for a window → evict → fetch in priority
/// order, restarting whenever a newer window arrives. `evict` drops whatever
/// the window no longer wants; `fetch` downloads one item (see
/// [`fetch_one`]) and returns how the loop continues.
pub fn run<R: Clone>(
    rx: &Receiver<WindowCmd<R>>,
    mut evict: impl FnMut(&[R]),
    mut fetch: impl FnMut(&R, &Receiver<WindowCmd<R>>, &mut Vec<R>) -> Flow,
) {
    let mut wanted: Vec<R>;
    loop {
        // 1. Block until the player hands over a window.
        match rx.recv() {
            Ok(WindowCmd::Window(list)) => {
                evict(&list);
                wanted = list;
            }
            Ok(WindowCmd::Shutdown) | Err(_) => return,
        }

        // 2. Fetch the list in priority order (current first).
        let mut index = 0;
        while index < wanted.len() {
            // A newer window that arrived between items preempts ours.
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    WindowCmd::Window(list) => {
                        evict(&list);
                        wanted = list;
                        index = 0;
                    }
                    WindowCmd::Shutdown => return,
                }
            }
            let item = wanted[index].clone();
            match fetch(&item, rx, &mut wanted) {
                Flow::Continue => index += 1,
                Flow::Restart => index = 0,
                Flow::Stop => return,
            }
        }
        // 3. Window satisfied → back to step 1 and wait.
    }
}

/// Run one download while watching for a newer window. `download` receives
/// the interrupt check it must poll between chunks: a newer window that
/// still wants this item lets it finish (the window is taken over at once);
/// one that drops it abandons it; shutdown stops it. Returns how the loop
/// continues together with the download's own result.
pub fn fetch_one<R: Clone, T>(
    rx: &Receiver<WindowCmd<R>>,
    wanted: &mut Vec<R>,
    mut evict: impl FnMut(&[R]),
    is_item: impl Fn(&R) -> bool,
    download: impl FnOnce(&mut dyn FnMut() -> bool) -> Result<T, DownloadError>,
) -> (Flow, Result<T, DownloadError>) {
    let mut flow = Flow::Continue;
    let result = download(&mut || match rx.try_recv() {
        Ok(WindowCmd::Window(list)) => {
            evict(&list);
            let still_wanted = list.iter().any(&is_item);
            *wanted = list;
            flow = Flow::Restart;
            !still_wanted // still wanted: finish this file, then re-check
        }
        Ok(WindowCmd::Shutdown) | Err(TryRecvError::Disconnected) => {
            flow = Flow::Stop;
            true
        }
        Err(TryRecvError::Empty) => false,
    });
    (flow, result)
}
