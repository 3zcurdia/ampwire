//! Jellyfin client (plan 08): paged library reads plus the raw response for
//! the original file (`static=true`, never a server transcode).
//!
//! Credentials live **only** here (`Client.key`), and only ever leave this
//! module as the `Authorization: MediaBrowser Token="…"` header — never in a
//! URL, log line, notice or `Debug` print. Every error text is a fixed string
//! or an HTTP status code, so a key can never leak through an error either.
//!
//! Blocking `ureq` (3.x): all of this runs on worker threads (the library
//! loader, the cache downloader, the engine thread for `stream`), never on
//! the UI thread.

pub mod cache;
pub mod http_source;

#[cfg(test)]
pub(crate) mod test_server;

use std::io::Read;
use std::sync::mpsc::Sender;
use std::time::Duration;

use serde::Deserialize;

use crate::config::JellyfinConfig;
use crate::player::{AlbumPos, Track, TrackSource};

/// Server-side page size (the plan's 500 items per request).
pub const PAGE_SIZE: usize = 500;
/// Connect + handshake budget.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Read budget: response headers and each body read (the plan's 15 s).
const READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Largest JSON body read into memory (audit S5): a page of 500 items with
/// `MediaSources` is a few MiB; anything past this is not a library page.
const MAX_JSON_BYTES: u64 = 32 * 1024 * 1024;

/// The raw HTTP response, so callers can read `Content-Range` (streaming)
/// or stream the body straight to disk (cache downloads).
pub type Response = ureq::http::Response<ureq::Body>;

/// Messages the library-load thread posts back to the UI thread. `Page`
/// batches arrive progressively (the playlist fills while the rest of the
/// server is still being fetched); `Done` and `Error` are terminal.
#[derive(Debug)]
pub enum JfMsg {
    Page {
        tracks: Vec<Track>,
        /// Tracks handed over so far (including this page).
        loaded: usize,
        /// Server-side total (`TotalRecordCount`).
        total: usize,
    },
    Done,
    Error(String),
}

/// One `/Items` page, mapped.
#[derive(Debug)]
pub struct LibraryPage {
    pub tracks: Vec<Track>,
    /// The server's `TotalRecordCount`.
    pub total: usize,
    /// Items on the wire, before malformed ones were dropped.
    pub items: usize,
}

/// A Jellyfin server + API key. Cheap to clone (shared `ureq` connection
/// pool); `Clone` lets the engine hold one for `stream` mode while the UI
/// holds another.
#[derive(Clone)]
pub struct Client {
    base: String,
    key: String,
    agent: ureq::Agent,
    /// [`MAX_JSON_BYTES`]; tests shrink it.
    max_json_bytes: u64,
}

impl Client {
    /// Build from `[jellyfin]`: timeouts per the plan (connect 10 s, read
    /// 15 s) and a URL whose trailing slash is already trimmed by config.
    pub fn new(cfg: &JellyfinConfig) -> Client {
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(READ_TIMEOUT))
            .timeout_recv_body(Some(READ_TIMEOUT))
            .build()
            .new_agent();
        Client {
            base: cfg.url.trim_end_matches('/').to_string(),
            key: cfg.api_key.clone(),
            agent,
            max_json_bytes: MAX_JSON_BYTES,
        }
    }

    /// The only place the key becomes a wire value: an HTTP header.
    fn auth_header(&self) -> String {
        format!("MediaBrowser Token=\"{}\"", self.key)
    }

    /// `GET {url}{path}` with the auth header, JSON-decoded. Error text is
    /// fixed — no URL, no query string, no key (plan §2).
    fn get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, String> {
        let url = format!("{}{path}", self.base);
        let response = self
            .agent
            .get(&url)
            .header("Authorization", &self.auth_header())
            .call()
            .map_err(map_error)?;
        // Bounded: one byte past the cap proves the body is too big, so a
        // hostile server cannot make us buffer without limit (audit S5).
        let mut body = response
            .into_body()
            .into_reader()
            .take(self.max_json_bytes + 1);
        let mut text = Vec::new();
        body.read_to_end(&mut text)
            .map_err(|_| "jellyfin: cannot reach server".to_string())?;
        if text.len() as u64 > self.max_json_bytes {
            return Err("jellyfin: bad response from server".to_string());
        }
        serde_json::from_slice(&text).map_err(|_| "jellyfin: bad response from server".to_string())
    }

    /// Resolve the configured `user` name (else the first user) to its id.
    pub fn user_id(&self, name: Option<&str>) -> Result<String, String> {
        let users: Vec<User> = self.get_json("/Users")?;
        if let Some(name) = name
            && let Some(found) = users.iter().find(|u| {
                u.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
            })
        {
            return Ok(found.id.clone());
        }
        if name.is_some() {
            // A configured name that matches nobody is a config typo: do
            // not silently play as a different account.
            return Err(format!(
                "jellyfin: user \"{}\" not found",
                name.unwrap_or_default()
            ));
        }
        users
            .into_iter()
            .find(|u| !u.id.is_empty())
            .map(|u| u.id)
            .ok_or_else(|| "jellyfin: no users on the server".to_string())
    }

    /// One page of audio items, server-sorted (`AlbumArtist, Album,
    /// ParentIndexNumber, IndexNumber, SortName`), mapped to [`Track`]s,
    /// plus `TotalRecordCount` and how many items the page really carried
    /// (malformed ones are dropped from `tracks`).
    pub fn page(&self, user_id: &str, start: usize, limit: usize) -> Result<LibraryPage, String> {
        let path = format!(
            "/Items?userId={user_id}&Recursive=true&IncludeItemTypes=Audio\
             &SortBy=AlbumArtist,Album,ParentIndexNumber,IndexNumber,SortName\
             &SortOrder=Ascending&Fields=MediaSources\
             &StartIndex={start}&Limit={limit}"
        );
        let page: ItemsPage = self.get_json(&path)?;
        let items = page.items.len();
        let total = page.total_record_count;
        let tracks = page.items.into_iter().filter_map(track_from_item).collect();
        Ok(LibraryPage {
            tracks,
            total,
            items,
        })
    }

    /// `GET /Audio/{id}/stream?static=true` — the original file, optionally
    /// from `range_from` bytes on (`Range: bytes=N-`). Callers inspect the
    /// status: `206` proves range support, `200` (whole body) does not.
    pub fn open_original(
        &self,
        item_id: &ItemId,
        range_from: Option<u64>,
    ) -> Result<Response, String> {
        let url = format!("{}/Audio/{item_id}/stream?static=true", self.base);
        let mut request = self
            .agent
            .get(&url)
            .header("Authorization", &self.auth_header());
        if let Some(from) = range_from {
            request = request.header("Range", &format!("bytes={from}-"));
        }
        request.call().map_err(map_error)
    }
}

/// Every transport/HTTP failure becomes one of three fixed strings — never
/// the error's own text (it can embed a URI, and the URI must not carry the
/// key… but we also never print a URL at all).
fn map_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::StatusCode(401) | ureq::Error::StatusCode(403) => {
            "jellyfin: invalid API key".to_string()
        }
        ureq::Error::StatusCode(code) => format!("jellyfin: HTTP {code}"),
        // Connect failures, timeouts, DNS, TLS, socket errors.
        _ => "jellyfin: cannot reach server".to_string(),
    }
}

/// Fetch the whole audio library in the background, 500 items at a time,
/// posting [`JfMsg::Page`] after each page and a terminal `Done`/`Error`.
/// The receiver keeps working meanwhile — the playlist stays usable.
pub fn spawn_library_load(client: Client, user: Option<String>, tx: Sender<JfMsg>) {
    let _ = std::thread::Builder::new()
        .name("ampwire-jellyfin-load".into())
        .spawn(move || {
            let user_id = match client.user_id(user.as_deref()) {
                Ok(id) => id,
                Err(reason) => {
                    let _ = tx.send(JfMsg::Error(reason));
                    return;
                }
            };
            let mut start = 0usize;
            let mut loaded = 0usize;
            loop {
                let LibraryPage {
                    tracks,
                    total,
                    items,
                } = match client.page(&user_id, start, PAGE_SIZE) {
                    Ok(page) => page,
                    Err(reason) => {
                        let _ = tx.send(JfMsg::Error(reason));
                        return;
                    }
                };
                loaded += tracks.len();
                if tx
                    .send(JfMsg::Page {
                        tracks,
                        loaded,
                        total,
                    })
                    .is_err()
                {
                    return; // the app is gone
                }
                start += PAGE_SIZE;
                // Stop at the server's total, or on a page with no items on
                // the wire (a server whose total lies would otherwise be
                // paged for ever — audit S5). A page whose items were all
                // malformed is not empty: the walk goes on.
                if start >= total || items == 0 {
                    break;
                }
            }
            let _ = tx.send(JfMsg::Done);
        })
        .expect("spawn the jellyfin library loader");
}

// --- wire types: PascalCase, everything optional, unknown fields ignored ---

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct User {
    id: String,
    name: Option<String>,
}

/// `GET /Items` page body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ItemsPage {
    #[serde(default)]
    items: Vec<Item>,
    #[serde(default)]
    total_record_count: usize,
}

/// One audio item. Field names follow the Jellyfin API's PascalCase; every
/// field is optional so a sparse item degrades instead of failing the page.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Item {
    id: Option<String>,
    name: Option<String>,
    #[serde(default)]
    artists: Option<Vec<String>>,
    album_artist: Option<String>,
    album: Option<String>,
    index_number: Option<i64>,
    parent_index_number: Option<i64>,
    run_time_ticks: Option<i64>,
    container: Option<String>,
    #[serde(default)]
    media_sources: Option<Vec<MediaSource>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MediaSource {
    bitrate: Option<u64>,
    container: Option<String>,
}

/// A Jellyfin item id that is safe as a URL path segment **and** as a file
/// name inside the cache dir: 1–64 ASCII alphanumerics, `-` or `_` (real ids
/// are 32 hex digits, Emby's are numeric). Only [`ItemId::parse`] builds
/// one, and it runs at the wire boundary ([`track_from_item`]), so no
/// server string can steer a download out of the cache dir (audit S1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ItemId(String);

impl ItemId {
    const MAX_LEN: usize = 64;

    pub fn parse(raw: &str) -> Option<ItemId> {
        let valid = !raw.is_empty()
            && raw.len() <= Self::MAX_LEN
            && raw
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        valid.then(|| ItemId(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<str> for ItemId {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for ItemId {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// Tests name their fixtures inline (`"abc".into()`); a bad name panics.
#[cfg(test)]
impl From<&str> for ItemId {
    fn from(raw: &str) -> ItemId {
        ItemId::parse(raw).unwrap_or_else(|| panic!("invalid test item id {raw:?}"))
    }
}

/// File extension for a container name (`"m4a"` → `"m4a"`, a comma list →
/// its first segment, junk/missing → `"bin"`): ASCII alphanumerics only, so
/// a hostile server string can never escape the cache directory.
pub(crate) fn container_ext(container: Option<&str>) -> String {
    let cleaned: String = container
        .unwrap_or("bin")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if cleaned.is_empty() {
        "bin".into()
    } else {
        cleaned.to_lowercase()
    }
}

/// Map one server item to a [`Track`]; `None` when the item cannot be
/// played or identified (no `Id` / `Name`).
fn track_from_item(item: Item) -> Option<Track> {
    let item_id = ItemId::parse(item.id?.trim())?;
    let title = item.name?.trim().to_string();
    if title.is_empty() {
        return None;
    }

    // Plan 12: album position, so the data matches local/SMB rows. The
    // server already sorted on it; the client never re-sorts these.
    let pos = AlbumPos {
        album: item
            .album
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string),
        album_artist: item
            .album_artist
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string),
        disc: item
            .parent_index_number
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0),
        number: item
            .index_number
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0),
    };

    // `Artists[0]`, else `AlbumArtist`, else no artist line.
    let artist = item
        .artists
        .and_then(|artists| {
            artists
                .into_iter()
                .find(|name| !name.trim().is_empty())
                .map(|name| name.trim().to_string())
        })
        .or_else(|| {
            item.album_artist
                .filter(|name| !name.trim().is_empty())
                .map(|name| name.trim().to_string())
        });

    // `RunTimeTicks` is in 100 ns ticks (÷ 10_000_000 = seconds).
    let duration = item
        .run_time_ticks
        .filter(|ticks| *ticks > 0)
        .map(|ticks| Duration::from_secs_f64(ticks as f64 / 10_000_000.0));

    // The media source's container (`m4a`, `flac`, …) is a usable file
    // extension; the item-level one is a comma list (`mov,mp4,m4a,…`), of
    // which only the first segment can stand in.
    let media = item
        .media_sources
        .and_then(|sources| sources.into_iter().next());
    let container = media
        .as_ref()
        .and_then(|source| source.container.clone())
        .or_else(|| {
            item.container
                .as_deref()
                .map(|list| list.split(',').next().unwrap_or("").trim().to_string())
        })
        .filter(|c| !c.is_empty());

    // `MediaSources[0].Bitrate` is bits/s; the format line wants kbps.
    let bitrate_kbps = media
        .and_then(|source| source.bitrate)
        .map(|bps| (bps / 1000) as u32)
        .filter(|kbps| *kbps > 0);

    Some(Track {
        id: Track::next_id(),
        source: TrackSource::Remote { item_id, container },
        artist,
        title: Some(title),
        duration,
        bitrate_kbps,
        pos,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::config::JellyfinMode;
    use crate::jellyfin::test_server::{KEY, MockServer};
    use crate::player::TrackSource;

    /// The fixture page, mapped (plan 08, step 7): two items, one missing
    /// `Artists` (falls back to `AlbumArtist`), one missing `RunTimeTicks`,
    /// an unknown extra field that must not break the parse.
    #[test]
    fn the_library_page_parses_and_maps_every_field() {
        let server = MockServer::start();
        let LibraryPage { tracks, total, .. } =
            server.client(KEY).page("user1", 0, PAGE_SIZE).unwrap();

        assert_eq!(total, 600, "TotalRecordCount");
        assert_eq!(tracks.len(), 2, "both items mapped");

        let first = &tracks[0];
        assert_eq!(first.remote_item_id(), Some("item1"));
        assert_eq!(first.artist.as_deref(), Some("The Album Artist"));
        assert_eq!(first.title.as_deref(), Some("Song One"));
        assert_eq!(first.duration, Some(Duration::from_secs(250)));
        assert_eq!(first.bitrate_kbps, Some(320), "320000 bits/s → 320 kbps");
        assert!(!matches!(
            first.source,
            crate::player::TrackSource::Local(_)
        ));
        assert!(
            matches!(&first.source, TrackSource::Remote { item_id, container }
                if item_id == "item1" && container.as_deref() == Some("mp3")),
            "container from MediaSources[0]"
        );
        // Plan 12: the album-position fields map like the local/SMB rows.
        assert_eq!(first.pos.album.as_deref(), Some("The Album"));
        assert_eq!(first.pos.album_artist.as_deref(), Some("The Album Artist"));
        assert_eq!(first.pos.disc, Some(2), "ParentIndexNumber");
        assert_eq!(first.pos.number, Some(3), "IndexNumber");

        let second = &tracks[1];
        assert_eq!(second.remote_item_id(), Some("item2"));
        assert_eq!(second.artist.as_deref(), Some("Artist Two"), "Artists[0]");
        assert_eq!(second.duration, None, "missing RunTimeTicks → no duration");
        assert_eq!(second.bitrate_kbps, Some(1000));
        assert_eq!(
            second.pos,
            AlbumPos::default(),
            "no position tags → all None"
        );
    }

    /// A second page past the items is empty but keeps the same total (the
    /// loader then stops).
    #[test]
    fn later_pages_are_empty_and_keep_the_total() {
        let server = MockServer::start();
        let LibraryPage { tracks, total, .. } =
            server.client(KEY).page("user1", 500, PAGE_SIZE).unwrap();
        assert!(tracks.is_empty());
        assert_eq!(total, 600);
    }

    /// Plan 08, step 7 (a): every request authenticates in the header —
    /// and no request *line* ever carries the key.
    #[test]
    fn every_request_carries_the_header_and_never_the_key() {
        let server = MockServer::start();
        let client = server.client(KEY);
        client.user_id(None).expect("/Users answers");
        client.page("user1", 0, PAGE_SIZE).expect("/Items answers");
        client
            .open_original(&"item1".into(), Some(0))
            .expect("stream answers");

        let log = server.log();
        assert_eq!(log.len(), 3, "one log entry per request: {log:?}");
        let want = format!("MediaBrowser Token=\"{KEY}\"");
        for logged in &log {
            assert!(
                logged.line.starts_with("GET /"),
                "a plain request line: {}",
                logged.line
            );
            assert!(
                !logged.line.contains(KEY),
                "the key must never appear in a request line: {}",
                logged.line
            );
            assert_eq!(
                logged.auth.as_deref(),
                Some(want.as_str()),
                "every request carries the auth header: {}",
                logged.line
            );
        }
    }

    /// Plan 08, step 7 (c): the worker posts `Page, Page, Done` in order.
    #[test]
    fn the_library_load_posts_pages_then_done_in_order() {
        let server = MockServer::start();
        let (tx, rx) = mpsc::channel();
        spawn_library_load(server.client(KEY), None, tx);

        let mut messages = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(message) => {
                    let done = matches!(message, JfMsg::Done);
                    messages.push(message);
                    if done {
                        break;
                    }
                }
                Err(reason) => panic!("loader did not finish: {reason:?} after {messages:?}"),
            }
        }
        assert_eq!(messages.len(), 3, "Page, Page, Done: {messages:?}");

        match &messages[0] {
            JfMsg::Page {
                tracks,
                loaded,
                total,
            } => {
                assert_eq!(tracks.len(), 2, "first page carries the items");
                assert_eq!(*loaded, 2);
                assert_eq!(*total, 600);
            }
            other => panic!("first message is a Page, got {other:?}"),
        }
        match &messages[1] {
            JfMsg::Page {
                tracks,
                loaded,
                total,
            } => {
                assert!(tracks.is_empty(), "second page is past the items");
                assert_eq!(*loaded, 2, "loaded counts tracks, not pages");
                assert_eq!(*total, 600);
            }
            other => panic!("second message is a Page, got {other:?}"),
        }
        assert!(matches!(messages[2], JfMsg::Done), "…then Done");
    }

    /// Audit S5: a body past the JSON cap is refused, never buffered whole.
    #[test]
    fn an_oversized_json_body_is_refused() {
        let server = MockServer::start();
        let mut client = server.client(KEY);
        client.max_json_bytes = 4096;
        assert!(client.page("user1", 0, PAGE_SIZE).is_ok(), "fits the cap");
        server.set_items_padding(8192);
        assert_eq!(
            client
                .page("user1", 0, PAGE_SIZE)
                .expect_err("over the cap"),
            "jellyfin: bad response from server"
        );
    }

    /// Audit S5: a server whose `TotalRecordCount` lies (huge) must not
    /// keep the loader paging through empty pages for ever — the first
    /// empty page ends the walk, as the loop's comment always promised.
    #[test]
    fn a_lying_total_stops_at_the_first_empty_page() {
        let server = MockServer::start();
        server.set_items_total(1_000_000_000);
        let (tx, rx) = mpsc::channel();
        spawn_library_load(server.client(KEY), None, tx);
        let mut pages = 0;
        loop {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(JfMsg::Page { .. }) => {
                    pages += 1;
                    assert!(pages <= 10, "still paging after {pages} pages");
                }
                Ok(JfMsg::Done) => break,
                Ok(JfMsg::Error(reason)) => panic!("unexpected error {reason}"),
                Err(reason) => panic!("loader did not stop: {reason:?} after {pages} pages"),
            }
        }
        assert_eq!(pages, 2, "the items page, then the empty one");
        let items_requests = server
            .log()
            .iter()
            .filter(|logged| logged.line.contains("/Items"))
            .count();
        assert_eq!(items_requests, 2, "no request past the empty page");
    }

    /// Plan 08, step 7 (d): 401 → the fixed message, key nowhere in it.
    #[test]
    fn a_401_says_invalid_api_key_without_leaking_the_key() {
        let server = MockServer::start();
        server.set_auth_fail(true);
        let client = server.client(KEY);

        let err = client.user_id(None).expect_err("401 must fail");
        assert_eq!(err, "jellyfin: invalid API key");
        assert!(!err.contains(KEY), "no key in the error: {err}");
        assert!(!err.contains(&server.base), "no URL in the error: {err}");

        // The same for the raw stream call.
        let err = client
            .open_original(&"item1".into(), None)
            .expect_err("401");
        assert_eq!(err, "jellyfin: invalid API key");
    }

    /// Unreachable servers and unknown users map to fixed, key-free texts.
    #[test]
    fn transport_and_user_errors_are_fixed_messages() {
        // Nothing listens on the discard port.
        let config = JellyfinConfig {
            url: "http://127.0.0.1:9".into(),
            api_key: KEY.into(),
            api_key_file: None,
            user: None,
            mode: JellyfinMode::Cache,
            cache_ahead: 3,
            cache_behind: 3,
            cache_dir: None,
            notice: None,
        };
        let err = Client::new(&config)
            .user_id(None)
            .expect_err("refused connection");
        assert_eq!(err, "jellyfin: cannot reach server");
        assert!(!err.contains(KEY));

        // A configured name that matches nobody: no silent account swap.
        let server = MockServer::start();
        let err = server
            .client(KEY)
            .user_id(Some("someone-else"))
            .expect_err("unknown user");
        assert_eq!(err, "jellyfin: user \"someone-else\" not found");

        // …while the configured name resolves, and omitting it picks the
        // first user.
        assert_eq!(
            server.client(KEY).user_id(Some("tester")).as_deref(),
            Ok("user1")
        );
        assert_eq!(server.client(KEY).user_id(None).as_deref(), Ok("user1"));
    }

    /// An item whose only interesting field is its `Id`.
    fn item_with_id(id: &str) -> Item {
        Item {
            id: Some(id.to_string()),
            name: Some("Song".into()),
            artists: None,
            album_artist: None,
            album: None,
            index_number: None,
            parent_index_number: None,
            run_time_ticks: None,
            container: Some("flac".into()),
            media_sources: None,
        }
    }

    /// Audit S1: the `Id` becomes a URL path segment *and* a file name in
    /// the cache dir, so anything that could leave that dir (or change the
    /// URL) is dropped at the wire boundary. Real Jellyfin ids are 32 hex
    /// digits; Emby-style numeric ids and the fixtures' short names pass.
    #[test]
    fn hostile_item_ids_are_dropped_at_the_boundary() {
        for hostile in [
            "../../../etc/evil",
            "/abs/path",
            "..",
            "a/b",
            "a\\b",
            "a.b",
            "%2e%2e",
            "id?x=1",
            "id with space",
            &"a".repeat(65),
        ] {
            assert!(
                track_from_item(item_with_id(hostile)).is_none(),
                "hostile id {hostile:?} must not become a track"
            );
        }
        for fine in [
            "0123456789abcdef0123456789abcdef",
            "12345",
            "item1",
            "a-b_c",
        ] {
            assert!(
                track_from_item(item_with_id(fine)).is_some(),
                "id {fine:?} is valid"
            );
        }
    }

    /// Small helper: the item id behind a remote track.
    trait RemoteItemId {
        fn remote_item_id(&self) -> Option<&str>;
    }
    impl RemoteItemId for Track {
        fn remote_item_id(&self) -> Option<&str> {
            match &self.source {
                TrackSource::Remote { item_id, .. } => Some(item_id.as_str()),
                TrackSource::Local(_) | TrackSource::Smb { .. } => None,
            }
        }
    }

    /// The plan's *"manual — needs a real server"* checks (step 7),
    /// scripted: library page → cache window slides on a real server →
    /// directory gone on exit → `Range` streaming decodes → the engine
    /// really plays a remote track. Ignored by default so plain `cargo
    /// test` never leaves localhost.
    ///
    /// ```sh
    /// AMWIRE_JELLYFIN_URL=http://jellyfin.lan:8096 \
    /// AMWIRE_JELLYFIN_API_KEY=… \
    /// cargo test -- --ignored jellyfin_real_server
    /// ```
    #[test]
    #[ignore = "needs a real Jellyfin server (AMWIRE_JELLYFIN_URL + _API_KEY)"]
    fn jellyfin_real_server_smoke() {
        use std::io::Read;
        use std::sync::Arc;
        use std::time::Instant;

        use self::cache::{Cache, RemoteRef};
        use self::http_source::HttpRangeSource;
        use crate::audio::{AudioOut, Engine, EngineCmd, EngineEvent, SharedParams};

        let (Ok(url), Ok(key)) = (
            std::env::var("AMWIRE_JELLYFIN_URL"),
            std::env::var("AMWIRE_JELLYFIN_API_KEY"),
        ) else {
            eprintln!("skipping: set AMWIRE_JELLYFIN_URL and AMWIRE_JELLYFIN_API_KEY");
            return;
        };
        let config = JellyfinConfig {
            url,
            api_key: key,
            api_key_file: None,
            user: None,
            mode: JellyfinMode::Cache,
            cache_ahead: 3,
            cache_behind: 3,
            cache_dir: None,
            notice: None,
        };
        assert!(config.is_complete());
        let client = Client::new(&config);

        // 1. The real library pages in.
        let user = client.user_id(None).expect("users on a real server");
        let LibraryPage { tracks, total, .. } =
            client.page(&user, 0, PAGE_SIZE).expect("first page");
        assert!(total > 0 && !tracks.is_empty(), "library of {total}");
        eprintln!("library: {total} tracks, first page has {}", tracks.len());
        let mut refs: Vec<RemoteRef> = Vec::new();
        for track in &tracks {
            if let Some(found) = RemoteRef::from_track(track) {
                if !refs.iter().any(|have| have.item_id == found.item_id) {
                    refs.push(found);
                }
                if refs.len() == 3 {
                    break;
                }
            }
        }
        assert_eq!(refs.len(), 3, "three distinct remote items");

        // 2. Cache mode: the window downloads, slides, and its directory
        //    is gone once the cache drops (the "quit → dir gone" check).
        let parent = tempfile::tempdir().expect("temp parent");
        {
            let mut cache =
                Cache::new(client.clone(), Some(parent.path().to_path_buf())).expect("cache dir");
            let wait_ready = |cache: &mut Cache, ids: &[ItemId]| {
                let started = Instant::now();
                while started.elapsed() < Duration::from_secs(60) {
                    cache.poll();
                    if ids.iter().all(|id| cache.path_if_ready(id).is_some()) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                panic!("timed out waiting for {ids:?}");
            };

            let three: Vec<ItemId> = refs.iter().map(|r| r.item_id.clone()).collect();
            cache.set_window(refs.clone());
            wait_ready(&mut cache, &three);
            for id in &three {
                let path = cache.path_if_ready(id).expect("ready");
                assert!(path.is_file(), "{path:?} exists");
                assert!(
                    std::fs::metadata(&path).expect("stat").len() > 1024,
                    "real bytes"
                );
            }
            eprintln!(
                "cache window of 3 ready: {:?}",
                cache.dir().file_name().unwrap().to_string_lossy()
            );

            // Slide: drop the first, add a fourth item from later in the page.
            let mut next_refs = refs[1..].to_vec();
            for track in &tracks[3..] {
                if let Some(found) = RemoteRef::from_track(track)
                    && !refs.iter().any(|have| have.item_id == found.item_id)
                {
                    next_refs.push(found);
                    break;
                }
            }
            assert_eq!(next_refs.len(), 3, "replacement window");
            let dropped = refs[0].item_id.clone();
            let added = next_refs.last().expect("a fourth item").item_id.clone();
            cache.set_window(next_refs);
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(60) {
                cache.poll();
                if cache.path_if_ready(&added).is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            assert!(cache.path_if_ready(&added).is_some(), "new item arrived");
            assert_eq!(cache.path_if_ready(&dropped), None, "dropped item evicted");
            assert!(
                !cache.dir().join(format!("{dropped}.part")).exists(),
                "no partials left"
            );
        } // Drop: Shutdown + join + remove_dir_all
        assert!(
            parent.path().read_dir().expect("parent").next().is_none(),
            "the cache directory is gone, like on quit"
        );

        // 3. Stream mode: seekable `Range` reads that decode.
        let item = &refs[0];
        let mut source = HttpRangeSource::open(client.clone(), &item.item_id)
            .expect("the real server answers 206");
        let mut bytes = Vec::new();
        source.read_to_end(&mut bytes).expect("stream reads");
        let len = symphonia::core::io::MediaSource::byte_len(&source).expect("length");
        assert_eq!(bytes.len() as u64, len, "the whole original file");

        let source = HttpRangeSource::open(client.clone(), &item.item_id).expect("206 again");
        let mut decoder = crate::audio::Decoder::open_source(
            Box::new(source),
            item.container.as_deref(),
            Duration::ZERO,
        )
        .expect("the streamed original decodes");
        let info = decoder.info().clone();
        let mut samples = Vec::new();
        while decoder.read(&mut samples).expect("decodes") {}
        assert!(!samples.is_empty());
        eprintln!(
            "streamed: {} {} Hz {}ch, {} samples",
            info.codec,
            info.sample_rate,
            info.channels,
            samples.len()
        );

        // 4. The engine really plays a remote track (audible for a moment;
        //    the device smoke test in `audio::engine` covers the rest).
        let engine = Engine::spawn(
            Arc::new(SharedParams::new()),
            Some(crate::audio::StreamSetup {
                client,
                fallback_parent: None,
            }),
            None,
        );
        engine.send(EngineCmd::LoadRemote {
            item_id: item.item_id.clone(),
            container: item.container.clone(),
            start: Duration::ZERO,
        });
        let started = Instant::now();
        let mut outcome = None;
        while outcome.is_none() && started.elapsed() < Duration::from_secs(60) {
            for event in engine.poll() {
                match event {
                    EngineEvent::Started(info, _handle) => outcome = Some(Ok(info)),
                    EngineEvent::Error(reason) => outcome = Some(Err(reason)),
                    EngineEvent::Buffering
                    | EngineEvent::Notice(_)
                    | EngineEvent::Finished
                    | EngineEvent::Advanced(_) => {}
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        match outcome {
            Some(Ok(info)) => eprintln!("engine started a remote stream: {info:?}"),
            Some(Err(reason)) => panic!("remote playback failed: {reason}"),
            None => panic!("no Started within 60 s"),
        }
        drop(engine); // Shutdown + join: playback stops
    }
}
