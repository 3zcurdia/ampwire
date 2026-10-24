# Plan 08 — Jellyfin: load the whole server library and play it

**Status:** ✅ done — 2026-10-23, f68124c. 357 tests pass (3 `#[ignore]`d: a
visual dump, the audio-device smoke test, and the real-server Jellyfin smoke),
goldens unchanged, `cargo fmt --check` + `clippy --all-targets -- -D warnings`
clean. Built in steps 0–8 (`2a61909`…`f68124c`); Step 0's spike passed against
a real server (see **Spike result**), so all three modes shipped, and the
real-server smoke ran to completion against that same server (library → cache
window → `Range` decode → device playback). Deviations annotated at the end.

Goal: with a `[jellyfin]` section in `config.toml`, pressing `j` fetches every
audio track on the server into the playlist (appended to whatever is there).
Those tracks play like local files, in one of three modes:

| `mode` | What happens | EQ / volume / balance |
|---|---|---|
| `cache` (default) | Original files are downloaded to a **temp directory**, a rolling window of **3 tracks ahead and 3 behind** the current one. Playback reads the local file. | all work |
| `stream` | Original file is streamed over HTTP (seekable, `Range` requests), nothing stored. | all work |
| `stream_pure` | Same as `stream`, but the **EQ is forced off** while a Jellyfin track plays, so the only thing between the server's bytes and the speakers is the decoder (plus volume/balance if you moved them). | EQ bypassed |

Prerequisites: plans 03 (engine, decoder, `PlayOrder`), 04 (seek) and 05
(`Track.bitrate_kbps`, `SharedParams`) and 06 (EQ) merged.

## Research findings (read before coding)

- **Can we stream and still use the EQ? Yes.** The EQ is local DSP applied to *decoded* samples in the audio callback. It does not care whether the bytes came from a file, a download or an HTTP stream. So `stream` with the EQ on is technically fine.
- **Why `stream_pure` is still useful:** it gives a guaranteed "server bytes → decoder → speakers" path, with the bit-perfect marker accurate.
- **Local transcoding is never needed or used.** We always decode locally (a terminal app has no other way to make sound). "No transcoding" means we never ask the *server* to transcode either: always `static=true` (original file).
- **UNVERIFIED — must be checked first (Step 0):** I could not confirm from public docs that Jellyfin answers HTTP `Range` requests (`206 Partial Content`) on `GET /Audio/{id}/stream?static=true`. Seekable streaming depends on it. The Jellyfin and Emby docs only say `static=true` sends the original file untouched. Emby's docs warn that progressive (non-HLS) transcoded audio is hard to seek, but that concerns transcoding, not `static`.
- symphonia can read from any `MediaSource`; seekable sources must implement `Read + Seek` and report `is_seekable() == true` (and ideally `byte_len()`). Non-seekable sources work only for formats that don't need random access, and MP4/M4A often does (the index `moov` box may be at the end).
- Sources: [Jellyfin SDK – getAudioStream](https://kotlin-sdk.jellyfin.org/dokka/jellyfin-api/org.jellyfin.sdk.api.operations/-audio-api/get-audio-stream.html), [getUniversalAudioStream](https://kotlin-sdk.jellyfin.org/dokka/jellyfin-api/org.jellyfin.sdk.api.operations/-audio-api/get-universal-audio-stream.html), [Emby audio streaming notes](https://betadev.emby.media/doc/restapi/Audio-Streaming.html), [Symphonia media sources](https://www.mintlify.com/pdeljanov/Symphonia/concepts/media-sources) (third-party mirror — re-check against docs.rs for the version you use).

## Decisions

| Topic | Decision |
|---|---|
| Auth | API key from the Jellyfin dashboard. Config only; no login UI. |
| Credentials | Sent **only in an HTTP header**, never in a URL, log, notice or `Debug` print. |
| HTTP client | `ureq` (blocking). Check whether the resolved version is 2.x or 3.x and follow its docs. Timeouts: connect 10 s, read 15 s. |
| JSON | `serde_json` + `serde`. |
| Library load | Worker thread, pages of 500, server-side sort, tracks appended **progressively**. Trigger: `j`. No network call at startup. |
| Mixing | Remote tracks are appended to the same playlist. Pressing `j` again first removes the existing remote tracks, then reloads. |
| Cache dir | `std::env::temp_dir()/ampwire-jellyfin-<pid>/` created with mode `0700` on unix. Optional override `cache_dir` in config. |
| Cache cleanup | (1) On normal exit the app deletes its dir. (2) On startup the app deletes sibling `ampwire-jellyfin-*` dirs whose last-modified time is older than 24 h (left by crashes). (3) The OS also cleans its temp dir. Never touch a dir you did not create by that name pattern. |
| Cache window | `cache_ahead = 3`, `cache_behind = 3` (config, 0–10). "Ahead/behind" = the next/previous **remote** tracks in *play order* (`PlayOrder::upcoming/previous`, so shuffle is respected). |
| Window priority | current track → ahead (nearest first) → behind (nearest first). |
| Size caps | 512 MiB per file. Window total therefore ≤ 7 × 512 MiB in the worst case; no other cap in v1. |
| Partial files | Downloads go to `<item_id>.part`, atomically renamed to `<item_id>.<ext>` when complete. A `.part` file is never decoded. |
| Server transcode | Never. Always `static=true`. |

## Config

```toml
[jellyfin]
url = "http://192.168.1.10:8096"   # trailing slash is trimmed
api_key = "0123456789abcdef..."
user = "luis"             # optional; first user is used if omitted
mode = "cache"            # "cache" | "stream" | "stream_pure"
cache_ahead = 3           # cache mode only
cache_behind = 3
# cache_dir = "/var/tmp"  # optional; parent of ampwire-jellyfin-<pid>
```
`Config.jellyfin: Option<JellyfinConfig>` (`#[serde(default)]`; `mode` is an enum with `#[serde(rename_all = "snake_case")]`, unknown value → `cache` + notice).
`Config::save` must leave `[jellyfin]` untouched (add a test). Implement `Debug` for `JellyfinConfig` manually, printing `api_key: "<redacted>"`.
README: recommend `chmod 600 config.toml`.

## API reference (verify each call at https://api.jellyfin.org)

- Auth header on every call: `Authorization: MediaBrowser Token="<api_key>"` (fall back to `X-Emby-Token: <api_key>` only if the docs for your server version say so).
- Connectivity: `GET {url}/System/Info/Public` (no auth).
- Users: `GET {url}/Users` → `Id`, `Name`. Pick `user` by name, else first.
- Library page: `GET {url}/Items?userId={uid}&Recursive=true&IncludeItemTypes=Audio&SortBy=Artist,SortName&SortOrder=Ascending&Fields=MediaSources&StartIndex={n}&Limit=500`
  → `{ "Items": [...], "TotalRecordCount": N }`. Item fields: `Id`, `Name`, `Artists[]`, `AlbumArtist`, `RunTimeTicks` (÷ 10_000_000 = s), `Container`, `MediaSources[0].Bitrate` (bits/s ÷ 1000 = kbps).
- Original file: `GET {url}/Audio/{itemId}/stream?static=true` + auth header.

## Files

`Cargo.toml`, `src/config.rs`, `src/main.rs`, `src/app.rs`, `src/action.rs`, `src/keymap.rs`, `src/player/mod.rs`, `src/player/playlist.rs`,
`src/player/real.rs`, `src/audio/decoder.rs`, `src/audio/engine.rs`, `src/audio/dsp.rs`, `README.md`, `AGENTS.md`.
New: `src/jellyfin/mod.rs` (client + library load), `src/jellyfin/cache.rs` (temp-dir window), `src/jellyfin/http_source.rs` (seekable stream).

## Steps

### 0. Verification spike (do this first; write the result into this file)
Against a real server (ask the user for a URL + key if you don't have one; if you cannot test, **skip `stream`/`stream_pure` and ship `cache` only**, leaving
`mode = "stream*"` returning the notice `jellyfin: streaming not verified, using cache`):
```
curl -s -D - -o /dev/null -H 'Authorization: MediaBrowser Token="KEY"' -H 'Range: bytes=0-1' 'URL/Audio/ITEM/stream?static=true'
```
- Expect `HTTP/1.1 206`, `Content-Range: bytes 0-1/TOTAL`, `Accept-Ranges: bytes`. Record what you get under a new heading "Spike result" at the bottom of this file.
- If the answer is `200` with the whole body, streaming with seeking is **not** possible: ship `cache` only and treat `stream*` as above.
- Also test `Range: bytes=-65536` (the tail) — M4A seeks to the end.

### 1. Types (`src/player/mod.rs`)
Extend `TrackSource` with `Remote { item_id: String, container: Option<String> }`. Remote `Track`: `artist` ← `Artists.first()` else `AlbumArtist`; `title` ← `Name`;
`duration` from ticks; `bitrate_kbps` from the media source. `display_name()` for remote never shows a path (fallback = `Name`). `sort_key()` for remote = `"{artist} - {title}"` lowercase.
Add `Track::is_remote()`. Fix every `match track.source` (the compiler lists them).

### 2. Client (`src/jellyfin/mod.rs`; `mod jellyfin;` in `main.rs`)
```rust
#[derive(Clone)] pub struct Client { base: String, key: String, agent: ureq::Agent }
impl Client {
    pub fn new(cfg: &JellyfinConfig) -> Client;
    pub fn user_id(&self, name: Option<&str>) -> Result<String, String>;
    pub fn page(&self, user_id: &str, start: usize, limit: usize) -> Result<(Vec<Track>, usize), String>;
    pub fn open_original(&self, item_id: &str, range_from: Option<u64>) -> Result<Response, String>;  // raw response for download or streaming
}
```
- Every JSON struct: `#[serde(rename_all = "PascalCase")]`, every field `Option`/`#[serde(default)]`.
- Error mapping: connect/timeout → `jellyfin: cannot reach server`; 401/403 → `jellyfin: invalid API key`; else `jellyfin: HTTP <code>`. **No URL query string or key in any error text.**
- `spawn_library_load(client, user, tx)` thread: user id → loop pages until `start >= total` or an empty page → `JfMsg::{Page{tracks, loaded, total}, Done, Error(String)}`.

### 3. Playlist ops
- `Playlist::remove_where(f) -> bool` (true if the current track was removed; keeps `current` consistent).
- `PlayerCommand::RemoveRemote`; if the current track was remote → stop (same as `RemoveTrack`).
- `PlayerCommand::AppendInOrder(Vec<Track>)`: push without sorting (server order). After every playlist mutation `RealPlayer` already calls `order.rebuild` (plan 03).

### 4. App wiring (`j` key)
`Action::LoadJellyfin` (`"jellyfin"`, global, `"j"`, "load jellyfin library"). README rule 8.
`App` fields `jf_rx: Option<Receiver<JfMsg>>`, `jf_first_page: bool`. On `j`: no config → notice `jellyfin: add [jellyfin] url and api_key to config.toml`; already loading → `jellyfin: already loading`.
`tick` → `poll_jellyfin()`: first `Page` sends `RemoveRemote` once, then `AppendInOrder`, notice `jellyfin: loading {loaded}/{total}`; `Done` → `jellyfin: loaded {n} tracks`; `Error(e)` → notice `e`, keep what was appended.
The playlist stays fully usable while loading.

### 5. Cache mode — rolling window in a temp dir (`src/jellyfin/cache.rs`)
A dedicated **downloader thread** (never the engine thread, so Stop/Next stay responsive during downloads):
```rust
pub struct RemoteRef { pub item_id: String, pub container: Option<String> }
pub enum CacheCmd { Window(Vec<RemoteRef>) /* priority order: current, ahead…, behind… */, Shutdown }
pub enum CacheEvent { Ready { item_id: String, path: PathBuf }, Failed { item_id: String, error: String } }
pub struct Cache { tx, rx, dir: PathBuf, ready: HashSet<String> /* item ids on disk */ }
impl Cache {
    pub fn new(client: Client, dir_parent: Option<PathBuf>) -> io::Result<Cache>;   // creates ampwire-jellyfin-<pid>, sweeps stale dirs (24 h)
    pub fn set_window(&self, wanted: Vec<RemoteRef>);
    pub fn path_if_ready(&self, item_id: &str) -> Option<PathBuf>;
    pub fn poll(&mut self) -> Vec<CacheEvent>;
}
impl Drop for Cache { /* send Shutdown, join, remove_dir_all(self.dir) — ignore errors */ }
```
Downloader loop, on each `Window(wanted)`:
1. **Evict**: delete every cached file whose item id is not in `wanted`. Ignore deletion errors (Windows may keep a playing file locked) and retry on the next window.
2. **Download** missing items in priority order, one at a time: `client.open_original(id, None)` → stream the body in 256 KiB chunks into `<id>.part` (refuse if `Content-Length` > 512 MiB or the running size exceeds it); on success `rename` to `<id>.<container or "bin">`, send `Ready`.
3. Between chunks, `try_recv` for a newer `Window`: if the in-flight item is still in the new list, keep downloading; otherwise delete its `.part` and restart from step 1 with the new list.
4. Failure → delete the `.part`, send `Failed`, continue with the next item.

When does `RealPlayer` set the window? On **every** track change, shuffle/repeat toggle, playlist mutation, and when a library page batch finishes:
```
wanted = [current if remote]
       + order.upcoming(N).filter(remote).take(cache_ahead)       // N large enough to find `cache_ahead` remote tracks; use len
       + order.previous(N).filter(remote).take(cache_behind)
```
Playing a remote track in cache mode (`RealPlayer::start_track`):
- `cache.path_if_ready(id)` is `Some(path)` → `Load { source: Path(path), start }` (same as a local file — seek, EQ, tap all work unchanged).
- Otherwise: state Playing, notice `buffering…`, remember `pending_remote = Some(id)`. When `CacheEvent::Ready{id}` matches → send `Load`. `Failed{id}` matching → the plan-03 error/skip path.
  If the user moves on before it arrives, `pending_remote` is overwritten; a stale `Ready` is ignored.
- Seek in cache mode is a normal local-file seek.

### 6. Stream modes (only if Step 0 proved `206`)
`src/jellyfin/http_source.rs`:
```rust
pub struct HttpRangeSource { client: Client, item_id: String, len: u64, pos: u64, body: Option<Box<dyn Read + Send>>, body_pos: u64 }
impl Read for HttpRangeSource  // if body is None or body_pos != pos → open_original(id, Some(pos)), expect 206; then read through a 256 KiB BufReader-style buffer
impl Seek for HttpRangeSource  // only updates `pos` (supports Start/Current/End using `len`); the next read re-opens at `pos` — no request on seek itself
impl symphonia::core::io::MediaSource for HttpRangeSource { fn is_seekable(&self) -> bool { true } fn byte_len(&self) -> Option<u64> { Some(self.len) } }
```
- `len` comes from the first `Range: bytes=0-0` probe (`Content-Range: bytes 0-0/TOTAL`). If the probe is not `206` → fall back to **cache mode for this track** and set the notice `jellyfin: server does not support ranges, downloading`.
- Forward reads of up to 64 KiB past the current body position are served by reading and discarding instead of re-opening (cheap small seeks).
- `Decoder::open_source(Box<dyn MediaSource>, ext_hint)` (refactor `open`/`open_at` to call it).
- `EngineCmd::LoadRemote { item_id, container, start }` — the engine builds the `HttpRangeSource` itself (give it `Option<Client>` at spawn: credentials never live in `Track`).
  The probe + `Decoder::open_source` blocks the engine thread for ≈ 1 round trip; send `EngineEvent::Buffering` first so the UI shows `buffering…`.
- In stream modes use a bigger ring buffer: `rate * channels * 2` (≈ 2 s) to survive network hiccups. A read stall can block the engine thread up to the 15 s read timeout (Stop/Next wait); this is a known v1 limit — note it in the README ("use cache mode on slow links").
- **`stream_pure`**: add `eq_bypass: AtomicBool` to `SharedParams`. `RealPlayer` sets it `true` when the current track is remote and `mode == stream_pure`, `false` otherwise. `Dsp` treats `eq_on = enabled && any_nonzero && !eq_bypass`.
  `bit_perfect` uses the same condition. First time per session notice: `stream pure: eq bypassed`. The EQ panel and its settings are untouched (they apply again on local tracks).
- No cache directory or window is created in stream modes; `Cache` is only constructed when `mode == cache`.

### 7. Tests (no real server)
- JSON fixture (2 items; one missing `Artists`, one missing `RunTimeTicks`, one unknown extra field) parses; mapping and `TotalRecordCount` correct.
- Tiny mock HTTP server in the test (`TcpListener` on `127.0.0.1:0`) serving `/Users`, `/Items` pages, and `/Audio/x/stream` with **real Range support** (206 + `Content-Range`) over the bytes of `tests/fixtures/tone.flac`. Assert:
  (a) every request carries the `Authorization` header; (b) **no request line contains the key**; (c) library load yields `Page, Page, Done` in order; (d) a `401` gives `jellyfin: invalid API key` with no key in the text;
  (e) cache: `set_window` of 3 items downloads them, `path_if_ready` is `Some`, a shifted window evicts the dropped file and downloads the new one, `.part` never appears in `path_if_ready`, dropping `Cache` removes the dir;
  (f) the 512 MiB cap is enforced (cap injected as a parameter, set to 1 KiB in the test);
  (g) `HttpRangeSource` reads the whole file identically to the file on disk, seeks backwards and forwards, and `Decoder::open_source` decodes it; a server that answers `200` to the probe triggers the fallback.
- Window logic as a pure function `compute_window(order, tracks, current, ahead, behind) -> Vec<RemoteRef>`: ordered playback, shuffle, mixed local/remote lists (local tracks are skipped and don't count), start/end of list, `ahead = behind = 0`.
- Cleanup: create a fake stale `ampwire-jellyfin-99999` dir with an old mtime inside a temp parent → the startup sweep removes it; a fresh one and an unrelated dir are left alone.
- Config: `[jellyfin]` survives `Config::save`; unknown `mode` → cache; `Debug` output has no key. Playlist: `remove_where`, `AppendInOrder`. App: `j` without config / `j` twice / first page triggers `RemoveRemote` once.
- `stream_pure`: with `eq_bypass` set the buffer is bit-identical even when EQ gains are non-zero.
- Manual (needs a real server): cache mode — play, watch `ls $TMPDIR/ampwire-jellyfin-*` show ≤ 7 files that slide as you press next/prev and under shuffle; quit → dir gone. Stream mode — seek works, unplug network → error notice and skip. `stream_pure` — EQ has no effect on remote tracks and returns on local ones.

### 8. Docs
README: `[jellyfin]` block, `j` key, the three modes (with the "EQ works while streaming; `stream_pure` bypasses it" explanation), temp-dir location and cleanup, `chmod 600`. AGENTS.md: add the `jellyfin` module to the boundaries.

## Done when
`j` fills the playlist progressively while the UI stays responsive; in `cache` mode the temp dir holds the current track plus up to 3 ahead and 3 behind and is removed on exit; in `stream*` modes (if Step 0 passed) playback and seeking work with no files written; the key never appears in any output; the three cargo checks pass.

## Assumptions I made (tell me to change)
- "3 ahead / 3 behind" counts remote tracks only, in play order (so it follows shuffle).
- Streaming has a blocking-read limitation (transport commands may wait up to the read timeout during a network stall).
- No auto-load at startup — you press `j`.
- Remote items keep the server's `Artist, SortName` order.

## Spike result

**PASSED — 2026-10-23, against `http://hyperion.local:8096` (Jellyfin Server
12.2.0, Kestrel), auth header `Authorization: MediaBrowser Token="<key>"`.**

- `GET /System/Info/Public` → `200` without auth (connectivity check).
- `GET /Users` → `200`, users parsed (`Id` + `Name`).
- Library page (`/Items?…IncludeItemTypes=Audio…`) → `200`,
  `TotalRecordCount: 2523`, items carry `Id`, `Name`, `Artists[]`,
  `AlbumArtist`, `RunTimeTicks`, `Container`, `MediaSources[0].{Bitrate,
  Container}` as documented.
- **`GET /Audio/<id>/stream?static=true` with `Range: bytes=0-1`:**
  ```
  HTTP/1.1 206 Partial Content
  Content-Length: 2
  Content-Type: audio/mp4
  Accept-Ranges: bytes
  Content-Range: bytes 0-1/8223082
  ```
- **`Range: bytes=-65536` (tail, the M4A end-seek):**
  ```
  HTTP/1.1 206 Partial Content
  Content-Length: 65536
  Accept-Ranges: bytes
  Content-Range: bytes 8157546-8223081/8223082
  ```

Verdict: the server answers `206` + `Content-Range` + `Accept-Ranges: bytes`
for both head and tail ranges → seekable streaming is possible, so **all three
modes (`cache`, `stream`, `stream_pure`) are implemented** (no
"streaming not verified" fallback needed).

## Deviations

Everything the plan asked for is in; these are the places the implementation
diverges in detail, newest concerns first grouped by area:

1. **Interactive checks replaced by a scripted smoke test.** The plan's
   *"Manual (needs a real server)"* list (watch the temp dir slide under
   next/prev/shuffle, seek over the network, hear the `stream_pure` bypass) is
   covered by the env-gated `#[ignore]`d test
   `jellyfin::tests::jellyfin_real_server_smoke` (library paging → a real
   cache window downloading/evicting → dir removed on drop → `Range`
   read + symphonia decode → one real `EngineCmd::LoadRemote` playing through
   the device), plus the deterministic unit tests for the sliding-window
   logic (`compute_window`: ordered/shuffle/mixed/edges/zero) and the EQ
   bypass (`dsp`, `real.rs`). Run it with
   `AMWIRE_JELLYFIN_URL=… AMWIRE_JELLYFIN_API_KEY=… cargo test -- --ignored jellyfin_real_server`.
   **Ran to completion against the live server (2026-10-23)**: the 2 523-track
   library paged, a real cache window of 3 downloaded then slid (dropped item
   evicted, directory removed on drop), the whole original streamed back
   byte-exact and decoded (`AAC 44100 Hz 2ch`, 22 784 000 samples ≈ 258 s),
   and `EngineCmd::LoadRemote` really started playback
   (`StreamInfo { codec: "AAC", sample_rate: 44100, duration: 258.31 s }`).
   What stays *interactive-only* by nature: watching the directory slide while
   pressing next/prev in the TUI and ear-checking `stream_pure` — the logic
   behind each is unit-tested (`compute_window`, `dsp::eq_bypass`,
   `real.rs` buffering/stream tests).
2. **Range probe sends `bytes=0-`, not `bytes=0-0`.** `open_original`'s
   `range_from: Option<u64>` builds `bytes={from}-` (plan §API). The probe's
   purpose — status `206` + total length from `Content-Range` after the last
   `/` — is identical; the real-server spike answered both forms with `206`.
3. **Unknown `mode` needs a string-level parse.** A serde enum would abort the
   whole config on an unknown variant, so `JellyfinConfig` deserializes
   through a private `JellyfinConfigRepr` (`#[serde(from)]`) where `mode` is
   an `Option<String>`; the `From` conversion maps `cache`/`stream`/
   `stream_pure`, downgrades anything else to `Cache` **with** the notice
   `jellyfin: unknown mode "x", using cache`, and trims the URL's trailing
   slash. `JellyfinMode` itself keeps `#[serde(rename_all = "snake_case")]`
   names for its `Serialize` side.
4. **A configured-but-unknown `user` is an error**, not "fall back to the
   first user": `jellyfin: user "<name>" not found`. Silently playing as a
   different account because of a typo seemed worse than failing; omitting
   `user` still picks the first user, exactly as planned.
5. **Container comes from `MediaSources[0].Container` first** (e.g. `m4a`) —
   the item-level `Container` is a comma list (`mov,mp4,m4a,…`) and only its
   first segment can act as a file extension. All extension values are
   sanitized to ASCII alphanumerics (`bin` fallback), so a hostile server
   string cannot escape the cache directory.
6. **Paging advances `StartIndex` by the page size (500)**, not by the number
   of *mapped* tracks — a malformed item dropped during mapping can neither
   stall nor duplicate the walk. The stop conditions are unchanged: `start ≥
   TotalRecordCount`, or an empty page.
7. **An incomplete `[jellyfin]` section** (present, but empty `url`/`api_key`)
   shows the same notice as no section at all:
   `jellyfin: add [jellyfin] url and api_key to config.toml`.
8. **`Cache` internals.** `ready` is a `Mutex<HashMap<id, PathBuf>>` rather
   than a `HashSet`, which is what lets `set_window` keep the plan's
   `&self` signature (bookkeeping prune + send) and lets `path_if_ready`
   remember each item's extension; it also re-checks `path.exists()` and
   drops stale entries (eviction can race bookkeeping by one tick). `Cache`
   additionally owns a `JoinHandle` — `Drop` needs to join before
   `remove_dir_all`.
9. **Cache unavailable (temp dir not creatable):** a remote track in `cache`
   mode stops with `jellyfin: cache unavailable` instead of taking the
   plan-03 skip path — the error would fire *inside* `load_track` (re-entrant
   advance), and skipping would just churn through every remote track for the
   same unfixable reason. Download `Failed` events still use the plan-03
   skip path exactly (`skipped <name>: <reason>`).
10. **Notice lifecycle.** Notices persist until replaced in this codebase, so
    two timings were pinned down: `buffering…` is retired by the engine's
    `Started` (only if it is still the current notice — anything else queued
    meanwhile keeps its place), and `stream pure: eq bypassed` is raised at
    `Started` (first remote stream of the session) rather than at `Load`, so
    it can neither be clobbered by nor hide `EngineEvent::Buffering`.
11. **The `200`-to-`Range` fallback lives in the engine.** For a server that
    ignores `Range`, the engine downloads that one original into
    `ampwire-jellyfin-<pid>/` (same `0700` dir, same 512 MiB cap) and plays
    it as a local file, preceded by the plan's notice
    `jellyfin: server does not support ranges, downloading`. This is the
    plan's *"cache mode for this track"* without constructing a `Cache` —
    the plan forbids a cache directory in stream modes, so this directory is
    the documented exception, reached only from `OpenError::NoRanges`.
12. **`HttpRangeSource`'s body sits behind a `Mutex<BodyState>`** — symphonia
    `MediaSource: Read + Seek + Send + Sync`, so the plan's
    `body: Option<Box<dyn Read + Send>>` field alone would not be `Sync`.
    Behaviour is as specified: `Seek` moves only the cursor (never a
    request), `Read` re-opens when the open body cannot serve it (nothing
    open, backwards, or farther forward than the 64 KiB margin), otherwise
    discards forward; reads never run past the probe's length.
13. **`EngineEvent` gained `Buffering` (as planned) and `Notice(String)`** —
    the latter carries the range-fallback message from the engine thread to
    the player's notice slot.
14. **`app::Backend` variants are boxed** (`Mock(Box<MockPlayer>)`,
    `Real(Box<RealPlayer>)`): plan 08's cache state grew `RealPlayer` past
    clippy's `large_enum_variant` threshold, and boxing only the real side
    would have inverted the complaint.
15. **Test-only hook:** `HttpRangeSource::set_forward_margin` shrinks the
    64 KiB discard margin so the 14 KiB fixture can exercise the
    far-forward re-open path; production behaviour is untouched.
16. **`Config::save` / `[jellyfin]`** needed no code change — the save path
    only ever writes the `layout`/`theme`/`audio`/`eq` tables — but the
    plan-required regression test (`save_leaves_the_jellyfin_table_untouched`)
    is in place, along with the `cache_ahead`/`cache_behind` 0–10 clamp in
    `sanitize` (the plan listed the range only in passing).

Plan assumptions (remote-only counting in play order, the stream-mode
blocking-read limitation — now also documented in the README as *"use cache
mode on slow links"*, no startup auto-load, server order preserved) were kept
as-is.
