# Plan 10 — Native Samba: pass `smb://` anywhere a file path works

**Status:** 🟨 in progress — opencode agent, since 2026-10-24.

Goal: `smb://server/share/path` works as a **file URL** — everywhere a local
path works today, with no kernel mount:

```sh
cargo run -- smb://nas/Music
cargo run -- ~/Music smb://nas/More-Music   # mixed local + remote
```

In-app `l` (Open/replace) and `a` (Add/append) accept the same URLs. A share
directory scans recursively into the playlist; a single-file URL loads that
one track. Playback, seek, EQ, gapless and the format line behave like local
files.

Prerequisites: plans 03 (engine, decoder, `PlayOrder`), 04 (seek/clock), 08
(remote-track pattern: `TrackSource::Remote`, worker-thread load, cache vs
stream, credential hygiene), 09 (gapless `Queue` + boundary) merged.

## Why this shape

Today `smb://` cannot work:

- `src/main.rs:112-117` maps every CLI arg via `PathBuf::from`.
- `src/app.rs:705-720` `submit_prompt` does `expand_tilde` + `path.exists()`
  — an `smb://` string always becomes `not found`.
- `src/library/mod.rs:97` `spawn_scan(Vec<PathBuf>)` uses `WalkDir` + `std::fs`
  only; `src/player/mod.rs:134` has no SMB identity; `src/audio/decoder.rs:104`
  `open_at(&Path)` and `src/audio/engine.rs:44` `Load{path}` are file-only.

Jellyfin (plan 08) is the template: worker-thread discovery, `TrackSource`
variant, engine-owned credentials, seekable `MediaSource` for stream mode,
rolling-window cache for cache mode, gapless `QueueSource` extension. SMB
differs in one way: the server is a filesystem, not an API — discovery is a
recursive directory walk, not paged JSON.

## Research findings (verify in step 0, do not assume)

- SMB2/3 servers support random-access reads (read at offset), so a seekable
  `Read + Seek` source over SMB is natural — no `Range`-fallback dance like
  `src/jellyfin/http_source.rs:31-42`. What must be verified is the **crate**,
  not the protocol: directory listing, positioned read, and `Seek(End)`
  (symphonia probes the end for M4A) against a real Samba server.
- `lofty` reads tags from any `Read + Seek` (`read_from`), not just
  `read_from_path` (`src/library/mod.rs:42`). Tags can be read without
  downloading the whole file — but confirm per-format footer/header reads do
  not degenerate into full-file downloads over SMB.
- Symphonia needs `MediaSource: Read + Seek + Send + Sync` with
  `is_seekable() == true` and ideally `byte_len()`
  (`src/audio/decoder.rs:117-121`). A thin SMB wrapper qualifies if the crate
  gives `len` + `read_at`.
- Credential rule carries over from plan 08: password lives only in the
  client/setup object, travels never in a URL, notice, error text or `Debug`.

## Decisions

| Topic | Decision |
|---|---|
| URL form | `smb://server/share/path/to/dir-or-file`. Scheme match is case-insensitive, `server` is host/`IP`, `share` is mandatory, `path` may be empty (= share root). Percent-decoding for `%20` etc. `..` segments, empty server/share, and embedded credentials (`smb://user:pass@…`) are rejected — credentials come only from config/env (row below). |
| Where URLs are accepted | Everywhere a path is: CLI positionals, `l` prompt (Replace), `a` prompt (Append). New helper `parse_load_input(text) -> LoadInput::{Local(PathBuf), Smb(SmbUrl)}`; `smb://` bypasses `expand_tilde` and the `path.exists()` gate in `submit_prompt`. Mixed `Vec<LoadInput>` allowed on one command line. |
| Single file vs dir | Extension check reuses `library::is_audio` (case-insensitive `wav/mp3/m4a/flac`); `._` AppleDouble sidecars ignored (same rule as `src/library/mod.rs:25`). Non-audio single URL counts as `skipped` (same notice path as local). |
| Auth | `[smb]` config + `AMPWIRE_SMB_PASSWORD` > `password_file` > `password` (mirrors Jellyfin `api_key` precedence). Optional `username` (default `guest`), `domain`/`workgroup` optional. Manual `Debug` redacts password; every error is a fixed string (`smb: …`) carrying no URL query, password, or share internals. Recommend `chmod 600 config.toml`. |
| Modes | `mode = "cache"` (default, download-then-play, robust on slow links) and `mode = "stream"` (seekable SMB source, nothing stored). Same names/semantics as Jellyfin; `cache` is the MVP if step 0 forces a cut. |
| Scan threading | `spawn_smb_scan` on a worker thread, same `ScanMsg::{Progress, Done}` protocol as `src/library/mod.rs:87`, polled from `App::tick`. UI never blocks on network. |
| Playback cache | Dedicated downloader thread into `ampwire-smb-<pid>/` (`0700` unix), `<hash>.part` → rename when complete, rolling window current + 3 ahead + 3 behind in *play* order (shuffle respected). Same eviction/priority rules as `src/jellyfin/cache.rs:1-12`. Engine then gets plain `Load{path}` — seek/EQ/spectrum/gapless unchanged. |
| Playback stream | `SmbFileSource: MediaSource` wrapping positioned SMB reads; `Decoder::open_source` (existing, `src/audio/decoder.rs:117`); new `EngineCmd::LoadSmb` + `QueueSource::Smb`; `streaming=true` doubles the ring to ≈2 s like `src/audio/engine.rs:868`. Credentials live in engine-owned `SmbSetup`, never in `Track`. |
| Gapless | `RealPlayer::queue_source`/`refresh_queue` (`src/player/real.rs:824-868`) handle SMB: cache mode queues only `path_if_ready()`, stream mode always queueable. Boundary clock (`PlaybackHandle::boundary`) unchanged. |
| Notices | `smb: …` prefix. Reuse local wording where possible: `loading… N files`, `no audio files found`, `not found: smb://…`, `already loading`, `skipped <name>: <reason>`. Never quote passwords. |
| Out of scope | SMB1, DFS referrals, Kerberos/SSO, symlink following (never follow, like `WalkDir::follow_links(false)`), server-side transcoding (always original bytes, decoded locally), resampling (hi-fi rule 7 stands). |

## Config

```toml
[smb]
server = "192.168.1.10"   # default when the URL gives only share/path
share = "Music"            # default when the URL gives only path
username = "luis"          # default "guest"
# password = "…"           # or password_file, or AMPWIRE_SMB_PASSWORD (precedence in that order)
# password_file = "smb.pass"
# domain = "WORKGROUP"
mode = "cache"             # "cache" | "stream"
cache_ahead = 3            # cache mode only, 0–10
cache_behind = 3
# cache_dir = "/var/tmp"   # optional parent of ampwire-smb-<pid>
```

`Config.smb: Option<SmbConfig>` (`#[serde(default)]`, unknown `mode` →
`cache` + notice, same pattern as `JellyfinMode`). `Config::save` leaves
`[smb]` untouched (add test, same as `[jellyfin]`). Startup sweeps stale
`ampwire-smb-<pid>` dirs older than 24 h (same helper shape as
`sweep_stale_dirs`).

## Files

`Cargo.toml`, `src/config.rs`, `src/main.rs`, `src/app.rs`,
`src/player/mod.rs`, `src/player/real.rs`, `src/audio/decoder.rs` (no change
expected — verify), `src/audio/engine.rs`, `src/library/mod.rs` (reuse
`is_audio`/sidecar helpers), `README.md`, `AGENTS.md`.
New: `src/smb/mod.rs` (URL parse, client, `SmbSetup`), `src/smb/scan.rs`
(worker walk + tags), `src/smb/cache.rs` (rolling downloader, cache mode),
`src/smb/source.rs` (seekable `MediaSource`, stream mode).

## Steps

### 0. Spike — pick the SMB crate (do this first; record result here)

1. `cargo add smb` (pure-Rust candidate) and one `libsmbclient`-binding
   fallback; read docs.rs for the resolved versions (plans-README rule 1 —
   APIs differ per minor).
2. Against a real Samba server verify: connect with user/pass, recursive
   listing of a music share, open + `read_at` at 0 and at end-of-file, file
   length, and a 30 s streaming read with no disconnect. Record throughput
   and whether `Seek(End)` + backward seek work.
3. Verify `Decoder::open_source` over the wrapper for one `flac` + one `m4a`
   from `tests/fixtures/` hosted on the share (seek ±5 s, decode to EOF).
4. If no crate passes: stop, ship nothing, document the blocker here. Do not
   invent an SMB client.

#### Result — done, 2026-10-24

Server: a real **Samba 4.25** at `smb://hyperion.local/datastore` (music
tree `media/music/tree63`), reachable as **guest only** (`/GUEST`,
anonymous) — no account was available, so a *user/pass* login was **not**
exercised (the same `share_connect` call, unknown-gap accepted per the
owner). Candidates spiked: `smb` 0.12.1 and `pavao` 0.3.1
(`libsmbclient` 4.25.0, `pkg-config` found the system lib).

| Check (step 0.2/0.3) | `smb` 0.12.1 — pure Rust | `pavao` 0.3.1 — libsmbclient |
|---|---|---|
| connect (guest/anonymous) | ✅ | ✅ |
| recursive listing of a music share | ✅ 4 dirs / 63 files / 46 audio | ✅ identical counts |
| `read_at` at 0 | ✅ | ➖ **no positioned-read API** (`seek` + `read` on a shared fd only) |
| `read_at` at end-of-file | ✅ `Ok(0)` | ✅ `Read` → `Ok(0)` |
| file length | ✅ `get_len()` (known at open) | ✅ `stat().size` / `list_dirplus` |
| `Seek(End)` + backward seek | ✅ | ✅ |
| 30 s streaming read, no disconnect | ✅ 36.9 MiB / 30.0 s, 3 passes, never dropped | ➖ 5 s probe only |
| throughput, by read size | **1.3 / 4.1 / 8.8 MiB/s** at 64 KiB / 256 KiB / 1 MiB (round-trip bound) | 16.4 MiB/s at 1 MiB |
| `Decoder::open_source` over the wrapper: `tone.flac`, `tone_aac.m4a`, `tone_alac.m4a` from `tests/fixtures/` hosted on `//hyperion.local/shared/ampwire_spike/` | ✅ sample-exact vs the local file; `byte_len` correct; on a real song: open at 0 → open at +5 s (fewer samples, offset ≤ 5 s) → open at 0 again (same count) | ➖ `SmbFile<'a>` borrows its client, so it can never be a `'static` symphonia `MediaSource` |

**Chosen: `smb` 0.12.1** —
`smb = { version = "0.12.1", default-features = false, features = ["multi_threaded", "sign", "encrypt"] }`,
i.e. the crate's **sync (`multi_threaded`)** threading model: no tokio, no
NetBIOS transport, no compression feature. The fallback lost because:

1. `pavao::SmbFile<'a>` borrows its `SmbClient` → not `'static`, so it
   cannot be the `Box<dyn MediaSource>` step 6 needs without leaking the
   client;
2. pavao takes **one process-wide lock for every operation on every client**
   (its own docs: "a blocking SMB request therefore delays every other
   Pavão SMB operation") — the scan thread, the cache downloader and the
   engine thread would serialize each other (plans-README rule 5);
3. libsmbclient has no positioned read, while SMB2 `READ` is positioned
   natively — exactly what step 0.2 asks to verify;
4. build cost: pavao makes `libsmbclient` a hard build dependency of every
   ampwire build (its `vendored` feature compiles Samba from source instead);
   `smb` is pure Rust. It was `cargo add`ed, spiked live, then `cargo
   remove`d.

What the spike produced (kept as the base for the later steps):

- `src/smb/mod.rs::SmbClientCfg` (connect + redacted `Debug`) and
  `src/smb/source.rs::SmbFileSource` (`Read` + `Seek` + `MediaSource`,
  closes its handle on drop) — step 6's source, written early because
  step 0.3 needs a wrapper to decode through.
- Guest login on this server needs
  `ConnectionConfig::allow_unsigned_guest_access = true` **and** the login
  string `/GUEST` (the crate's guest spelling); `guest`, `GUEST` and `""`
  all fail ("session setup succeeded before SSPI authentication completed" /
  "empty identity"). The smoke harness tries candidates in that order.
- Real-server checks are `#[ignore]`d unit tests:
  `AMPWIRE_SMB_TEST_SERVER=hyperion.local AMPWIRE_SMB_TEST_SHARE=datastore AMPWIRE_SMB_TEST_PATH=media/music/tree63 cargo test smb_spike -- --ignored --nocapture`
  (listing / positioned reads / 30 s stream / ±5 s seek), and
  `…_SHARE=shared AMPWIRE_SMB_TEST_PATH=ampwire_spike` for the fixture
  decode. The five `tests/fixtures/*` files were uploaded to
  `//hyperion.local/shared/ampwire_spike/` with:
  `smbclient //hyperion.local/shared -N -c 'cd ampwire_spike; put tests/fixtures/tone.flac tone.flac; …'`
  (that share is writable for guests; `datastore` is read-only).
- Numbers worth keeping: symphonia's `MediaSourceStream` read-ahead caps at
  **64 KiB** per read, so stream mode on this link tops out at ≈1.3 MiB/s —
  still ~2.3× the heaviest 24/96 FLAC (~0.6 MiB/s) — which is why
  `decoder.rs` stays untouched (as the plan predicted) and cache mode stays
  the default. The cache's 256 KiB chunks land at ~4 MiB/s.
- Not exercised (out of scope per the plan): SMB1, DFS referrals
  (`dfs: false` is set, so a DFS path errors instead of resolving),
  Kerberos/SSO, symlinks (reparse points are skipped by the test walk),
  server-side transcoding (always original bytes).

### 1. URL + `LoadInput` (`src/smb/mod.rs`, `src/main.rs`, `src/app.rs`)

```rust
pub struct SmbUrl { pub server: String, pub share: String, pub path: String }
impl SmbUrl { pub fn parse(text: &str) -> Result<SmbUrl, String>; pub fn canonical(&self) -> String; }
pub enum LoadInput { Local(PathBuf), Smb(SmbUrl) }
pub fn parse_load_input(text: &str) -> Result<LoadInput, String>;
```

- `parse`: trim, case-insensitive `smb://` prefix, split server/share/path,
  percent-decode, reject `..`, empty server/share, `user:pass@`.
- `canonical()`: `smb://server/share/path` (lowercased server, no trailing
  slash except root) — the only string shown in notices and used as cache key.
- `main.rs`: CLI args map through `parse_load_input` (invalid `smb://` →
  `eprintln!("ampwire: …")` + exit 2, same as `--dump` errors). Mixed
  local + SMB allowed.
- `app.rs::submit_prompt`: try `parse_load_input` first; `Smb` skips
  `expand_tilde`/`exists()` and calls `load_inputs(vec![…], mode)`.
- `App::load_paths(Vec<PathBuf>)` becomes `load_inputs(Vec<LoadInput>)`
  (keep `load_paths` as a thin wrapper if tests use it). Empty vec → no-op.
  Tests: parse table (case, `%20`, trailing slash, rejections), CLI mixed
  routing, prompt `not found: smb://…` on parse failure.

### 2. `TrackSource::Smb` (`src/player/mod.rs`)

```rust
pub enum TrackSource {
    Local(PathBuf),
    Remote { .. },          // unchanged (plan 08)
    Smb { url: SmbUrl },    // canonical server/share/path; no credentials
}
```

- `SmbUrl: Clone + PartialEq + Eq` (store split fields, not the raw string).
- `display_name()`: tags first, else file stem of `url.path` (same rule as
  local; remote-Jellyfin fallback untouched). `sort_key()`: tags first, else
  `canonical().to_lowercase()`. `is_remote()` → true for `Smb` (drives cache
  window + any remote-only UI rule).
- Fix every `match source` (compiler lists them: `playlist.rs`, `real.rs`,
  `jellyfin/cache.rs::from_track`, engine `QueueSource` builder).
- Tests next to the code: stem fallback, `?` on empty path, sort/lowercase.

### 3. SMB scan worker (`src/smb/scan.rs`)

```rust
pub fn spawn_smb_scan(url: SmbUrl, client_cfg: SmbClientCfg, tx: Sender<ScanMsg>) // reuses library::ScanMsg
```

- Runs on its own thread (plans-README rule 5). Recursive walk via the crate
  from step 0, never following symlinks; `Progress{found}` every 50 files;
  final `Done{tracks, skipped}`. Unreadable entries → `skipped`.
- Per file: extension + `._` filter via shared `library::{is_audio}` helpers
  (extract to shared spot if private), then `lofty::read_from(remote_reader)`
  for artist/title/duration/bitrate (same mapping as
  `src/library/mod.rs:34-76`). Never fails the scan: tag-less → stem-named
  track (still playable).
- `App::poll_smb` mirrors `poll_scan`/`poll_jellyfin`: progress notice
  `loading… N files`, `Done` → `ReplaceTracks`/`AddTracks` per `ScanMode`.
  Concurrent local + SMB loads: run both threads, merge `Done`s (or refuse
  with `already loading` if simpler — decide in implementation, document
  here).
- Tests: fixture tree with nested dirs, non-audio ignored, `._` ignored,
  hidden `.foo.flac` kept, zero-byte file still yields a track, progress ticks
  at 50/100 (mirror `src/library/mod.rs:309-334`).

  **Decisions taken here (2026-10-25):**
  - *Concurrent local + SMB*: **run both, merge the `Done`s.**
    `App::load_inputs` starts the disk scanner plus one
    `smb::scan::spawn_smb_scan` thread per `smb://` URL over a **single**
    `mpsc` channel (`mpsc::Sender` is `Clone`), and `App::poll_scan` keeps
    the receiver open until `scan_pending` workers have reported. The first
    non-empty `Done` applies the mode the load was requested with and every
    later one appends — otherwise the second half would wipe the first — and
    `no audio files found` is only said when *every* worker came back empty.
    The plan's simpler alternative still holds for **re-entrancy**: a second
    load while any worker runs is refused with `already loading`.
  - The plan's `App::poll_smb` was not needed: both workers talk to one
    receiver, so `poll_scan` alone drains `Progress`/`Done`/`Failed` for
    local *and* share scans (see above).
  - `ScanMsg` gained a third variant, `Failed { reason }`: a worker that
    could not even start (connect refused, root not found) needs to show one
    fixed line, which `Done { tracks: [], … }` cannot carry. The local scan
    never sends it.
  - *Timing on the real server* (`media/music/tree63`, 46 files / 209 MB,
    ~48 ms RTT): one tag read costs **16 round trips / 722 KB / ≈0.77 s**,
    so a full scan is latency-bound (~0.7 s per file), not bandwidth-bound —
    a 256 KiB read-ahead moved it to 16 reads / 2.9 MB / 0.90 s (the reads
    are seek-triggered, so a bigger buffer only pulls more bytes). Hence the
    `smb://` URL is announced as `loading… <canonical>` immediately,
    before the first 50-file progress tick.
  - `lofty::read_from` takes a `std::fs::File`, *not* any `Read + Seek` —
    the plan's research note was optimistic; the scan uses
    `lofty::probe::Probe::new(reader).guess_file_type()?.read()`, which does
    accept any reader.

### 4. Credentials + config (`src/config.rs`, `src/smb/mod.rs`)

- `SmbConfig{server, share, username, password, password_file, domain, mode,
  cache_ahead/behind, cache_dir, notice}` with `mode: SmbMode{Cache, Stream}`
  (`snake_case`, unknown → `cache` + notice). Effective password resolution
  `env > password_file (trimmed, relative to config dir) > password`.
- Manual `Debug` prints `password: "<redacted>"`. `Config::save` never touches
  `[smb]` (test: round-trip keeps password + comments). Parse-error path never
  quotes the password. `is_complete()` = server/share-or-URL present; missing
  section → `smb://` URLs still parse but playback reports
  `smb: add [smb] credentials to config.toml`.
- Tests: precedence, redaction, save-preservation, unknown-mode notice.

### 5. Cache mode (`src/smb/cache.rs`, `src/player/real.rs`)

Copy the `src/jellyfin/cache.rs` wire protocol with SMB identity:

```rust
pub struct SmbRef { pub url: SmbUrl }
pub enum SmbCacheCmd { Window(Vec<SmbRef>), Shutdown }
pub enum SmbCacheEvent { Ready{ url: SmbUrl, path: PathBuf }, Failed{ url: SmbUrl, error: String } }
```

- Downloader thread, `ampwire-smb-<pid>/` (`0700`), `<hash>.part` → rename to
  `<hash>.<ext>`, 512 MiB/file cap, 256 KiB chunks, `try_recv` for newer
  windows mid-download (keep if still wanted, else delete `.part` + restart).
  Evict files not in `wanted`. `Drop` sends `Shutdown`, joins, removes dir.
- `RealPlayer::new(smb: Option<&SmbConfig>)`: sweep stale dirs every mode;
  `cache` mode creates the window. `update_smb_window()` on every track
  change / order / playlist mutation: current + `upcoming(N)` remotes +
  `previous(N)` remotes (play order, shuffle respected).
- `start_smb`: `path_if_ready()` → `EngineCmd::Load{path}`; else `buffering…`
  + `pending_smb` until `Ready` (same state machine as
  `src/player/real.rs:775-814` `start_remote`). `Failed` → plan-03 skip path.
- Tests: window priority/eviction, `.part`-never-ready, stale sweep by name
  pattern only.

### 6. Stream mode (`src/smb/source.rs`, `src/audio/engine.rs`)

```rust
pub struct SmbFileSource { /* positioned SMB reads, len, pos */ }
impl Read + Seek for SmbFileSource // + MediaSource (is_seekable true, byte_len)
pub enum EngineCmd { … LoadSmb{ url: SmbUrl, start: Duration }, … }
pub enum QueueSource { …, Smb(SmbUrl) }
pub struct SmbSetup { pub cfg: SmbClientCfg } // credentials, engine-owned
```

- `open_at(url, start)` on the engine thread (blocks there, never UI thread):
  `Buffering` event first, then `Decoder::open_source(Box::new(source),
  ext_hint, start)`. `streaming=true` ring ≈2 s (`engine.rs:868` branch).
- Seek = `Decoder::open_at` equivalent over the SMB source (reload at offset);
  `SeekUnsupported` restarts track (existing `PlayError` variant, no
  string-matching).
- `Drop`/connection loss mid-play → `EngineEvent::Error("smb: …")` fixed
  string; Stop/Next stay responsive (no join on network).
- Tests: `Read`/`Seek` cursor math (forward-discard limit if any, backward
  re-open, EOF, seek-past-end), `Decoder` over the source for wav/flac/mp3/m4a
  fixtures via a fake SMB backend; `#[ignore]` real-server smoke only.

### 7. Gapless + transport (`src/player/real.rs`, `src/audio/engine.rs`)

- `queue_source(i)`: `Smb` → cache mode `Local(path_if_ready()?)`, stream
  mode `Smb(url)`. `refresh_queue()` unchanged otherwise (end-of-order,
  repeat-wrap, stopped → `Unqueue`).
- Engine `Queue{source: Smb,…}` pre-opens on the engine thread; incompatible
  rate/channels → held `Error` at the boundary (today's skip path).
  Every `Load`/`LoadSmb`/`Stop` drops the queue.
- `tick`/`check_boundary` treat SMB-started and SMB-advanced tracks exactly
  like local/Jellyfin (clock = `played >= boundary`, never the event arrival).
- Tests: queued-compatible switch keeps one stream (fake `AudioOut`),
  incompatible falls back to `Finished → Load`, repeat-wrap queues nothing.

### 8. Docs + hygiene

- `README.md`: Quickstart `smb://` example, `[smb]` block, `cache`/`stream`
  table row, `chmod 600` note (extends the `[jellyfin]` paragraph).
- `AGENTS.md`: boundaries line (`smb/` module), thread list, `TrackSource`
  + `QueueSource` + `EngineCmd` additions, `0600`/`0700` note extended to
  `ampwire-smb-<pid>`.
- `docs/plans/README.md`: add plan 10 row; flip **Status** to ✅ done with date
  + commit in the same commit that finishes it (plans-README rule).
- Final gate (plans-README): `cargo fmt --check`, `cargo clippy
  --all-targets -- -D warnings`, `cargo test` green; goldens unchanged
  (mock player untouched).
