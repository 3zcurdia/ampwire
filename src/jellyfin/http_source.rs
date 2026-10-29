//! Seekable HTTP source for `mode = "stream"` / `"stream_pure"` (plan 08,
//! step 6): the original file over `Range` requests, nothing stored.
//!
//! Symphonia sees a normal `Read + Seek` file backed by the network:
//! [`Seek`] only moves a cursor (no request), [`Read`] opens the connection
//! lazily at that cursor — and re-opens it only when a read lands somewhere
//! the open body cannot serve (backwards, or more than 64 KiB ahead).
//! Forward seeks inside that margin just discard bytes from the body that
//! is already flowing.
//!
//! The open body sits in a `Mutex` only because symphonia's [`MediaSource`]
//! must be `Sync` and a boxed reader is not; `Read` has `&mut self`, so it
//! reaches the body through `Mutex::get_mut` and never actually locks.

use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use symphonia::core::io::MediaSource;

use super::cache::{MAX_FILE_BYTES, RemoteRef, download_original};
use super::{Client, ItemId};
use crate::cache_window::DownloadError;

/// Forward seeks up to this distance are served by discarding from the
/// open body instead of re-opening a connection (the plan's 64 KiB).
const FORWARD_SEEK_LIMIT: u64 = 64 * 1024;
/// Read-ahead per open connection (the plan's 256 KiB).
const READ_BUFFER: usize = 256 * 1024;

/// Why a stream could not be opened.
#[derive(Debug, PartialEq, Eq)]
pub enum OpenError {
    /// The server answered a `Range` request with the whole body (`200`):
    /// it does not support ranges, so seeking over HTTP is impossible —
    /// the caller falls back to downloading this one track ("cache mode
    /// for this track", plan §6).
    NoRanges,
    /// Anything else, already mapped to a fixed message that carries no
    /// URL and no key.
    Failed(String),
}

/// The open body plus how far it has actually been handed out.
struct BodyState {
    reader: Option<Box<dyn Read + Send>>,
    body_pos: u64,
}

/// A `Read + Seek` view of `GET /Audio/{id}/stream?static=true`.
pub struct HttpRangeSource {
    client: Client,
    item_id: ItemId,
    /// Total length from the probe's `Content-Range` (`byte_len`).
    len: u64,
    /// Logical cursor: moved by `Seek`, advanced by `Read`.
    pos: u64,
    /// How far forward a seek may still be served by discarding from the
    /// open body ([`FORWARD_SEEK_LIMIT`] in production; tests shrink it so a
    /// 14 KiB fixture can exercise the far-forward re-open path).
    margin: u64,
    body: Mutex<BodyState>,
}

impl HttpRangeSource {
    /// Probe the item once (`Range: bytes=0-`) to learn its length and to
    /// prove the server answers `206`. `200` (whole body) → [`OpenError::NoRanges`].
    /// The probe's body *is* the stream from offset 0, so it becomes the
    /// first reader — reading straight through costs this one request
    /// (audit P4).
    pub fn open(client: Client, item_id: &ItemId) -> Result<HttpRangeSource, OpenError> {
        let response = client
            .open_original(item_id, Some(0))
            .map_err(OpenError::Failed)?;
        if response.status().as_u16() != 206 {
            return Err(OpenError::NoRanges);
        }
        let len = content_range_total(&response).ok_or(OpenError::NoRanges)?;
        let reader: Box<dyn Read + Send> = Box::new(BufReader::with_capacity(
            READ_BUFFER,
            response.into_body().into_reader(),
        ));
        Ok(HttpRangeSource {
            client,
            item_id: item_id.clone(),
            len,
            pos: 0,
            margin: FORWARD_SEEK_LIMIT,
            body: Mutex::new(BodyState {
                reader: Some(reader),
                body_pos: 0,
            }),
        })
    }

    /// Test hook: shrink the forward-discard margin, so a small fixture can
    /// exercise the far-forward *re-open* path.
    #[cfg(test)]
    pub(crate) fn set_forward_margin(&mut self, margin: u64) {
        self.margin = margin;
    }
}

impl Read for HttpRangeSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.pos >= self.len {
            return Ok(0); // clean EOF (a seek past the end never loops)
        }
        // `&mut self`: no other borrow can exist, so no locking either.
        let state = self.body.get_mut().expect("http source body lock");

        // Re-open when the open body cannot serve the cursor: nothing open
        // yet, a backwards move, or a forward jump past the discard margin.
        let reopen = match &state.reader {
            None => true,
            Some(_) if state.body_pos > self.pos => true,
            Some(_) if self.pos - state.body_pos > self.margin => true,
            Some(_) => false,
        };
        if reopen {
            let response = self
                .client
                .open_original(&self.item_id, Some(self.pos))
                .map_err(io::Error::other)?;
            if response.status().as_u16() != 206 {
                // The server stopped honouring ranges mid-track: the
                // decoder sees a read error (never a corrupt byte stream).
                return Err(io::Error::other(
                    "jellyfin: server did not honour the range request",
                ));
            }
            state.reader = Some(Box::new(BufReader::with_capacity(
                READ_BUFFER,
                response.into_body().into_reader(),
            )));
            state.body_pos = self.pos;
        } else if state.body_pos < self.pos {
            // Cheap forward seek: drop exactly the bytes up to the cursor
            // from the body that is already open.
            let gap = (self.pos - state.body_pos) as usize;
            let mut discarded = 0usize;
            let reader = state.reader.as_mut().expect("open body");
            while discarded < gap {
                let mut scratch = [0u8; 8192];
                let want = (gap - discarded).min(scratch.len());
                let read = reader.read(&mut scratch[..want])?;
                if read == 0 {
                    break; // body ended before the cursor (server change)
                }
                discarded += read;
            }
            state.body_pos += discarded as u64;
            if state.body_pos != self.pos {
                return Err(io::Error::other("jellyfin: stream position lost"));
            }
        }

        let reader = state.reader.as_mut().expect("open body");
        // Never read past the probe's length: the next read is EOF, not a
        // request for bytes the server may not have.
        let want = out.len().min((self.len - self.pos) as usize);
        let read = reader.read(&mut out[..want])?;
        state.body_pos += read as u64;
        self.pos += read as u64;
        Ok(read)
    }
}

impl Seek for HttpRangeSource {
    /// Move the cursor only — no request happens here (the plan's design);
    /// the next `Read` re-opens at the new position when it has to.
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(pos) => pos as i128,
            SeekFrom::End(offset) => self.len as i128 + offset as i128,
            SeekFrom::Current(offset) => self.pos as i128 + offset as i128,
        };
        self.pos = target.max(0) as u64;
        Ok(self.pos)
    }
}

impl MediaSource for HttpRangeSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

/// Whole-file fallback for a server that ignores `Range` (plan 08 §6:
/// "fall back to **cache mode for this track**"): download the original
/// into `dir` (the engine's own `ampwire-jellyfin-<pid>`, removed when the
/// engine exits) and hand back the local path, so decoding, seeking, EQ and
/// the spectrum tap all work unchanged. Same code path as the cache window
/// ([`download_original`]: reuse, `.part` → rename, 512 MiB cap, key-free
/// errors). Blocking — the engine runs it on its own thread and sets
/// `cancel` to abandon it (audit P2).
pub fn download_fallback(
    client: &Client,
    item_id: &ItemId,
    container: Option<&str>,
    dir: &Path,
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    let item = RemoteRef {
        item_id: item_id.clone(),
        container: container.map(str::to_string),
    };
    download_original(client, &item, dir, MAX_FILE_BYTES, || {
        cancel.load(Ordering::Relaxed)
    })
    .map_err(|error| match error {
        DownloadError::Interrupted => "jellyfin: download cancelled".to_string(),
        DownloadError::Failed(reason) => reason,
    })
}

/// `Content-Range: bytes 0-1/8223082` → `8223082`.
fn content_range_total(response: &ureq::http::Response<ureq::Body>) -> Option<u64> {
    response
        .headers()
        .get("content-range")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit('/').next())
        .and_then(|total| total.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::jellyfin::test_server::{KEY, MockServer};

    /// The bytes the mock server streams (`tests/fixtures/tone.flac`).
    fn fixture_bytes() -> Vec<u8> {
        std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.flac"),
        )
        .expect("tone.flac fixture")
    }

    /// Plan 08, step 7 (g): byte-identical reads, backwards and forwards,
    /// and never a request for a mere `Seek`.
    #[test]
    fn the_source_reads_and_seeks_exactly_like_the_file_on_disk() {
        let server = MockServer::start();
        let client = server.client(KEY);
        let on_disk = fixture_bytes();

        let mut source = HttpRangeSource::open(client.clone(), &"any".into()).expect("206 server");
        assert_eq!(
            MediaSource::byte_len(&source),
            Some(on_disk.len() as u64),
            "length from the probe's Content-Range"
        );
        let probe_requests = server.log().len();

        // Whole file, byte for byte.
        let mut all = Vec::new();
        source.read_to_end(&mut all).expect("reads");
        assert_eq!(all, on_disk, "identical to the file on disk");

        // Backwards seek → the next read re-opens at the cursor.
        source.seek(SeekFrom::Start(10)).expect("seek");
        let mut buf = [0u8; 64];
        source.read_exact(&mut buf).expect("read after seek");
        assert_eq!(&buf[..], &on_disk[10..74]);

        // Forward seek inside the margin → discard, no new request.
        let before_discard = server.log().len();
        source.seek(SeekFrom::Current(100)).expect("seek");
        source.read_exact(&mut buf).expect("read after seek");
        assert_eq!(&buf[..], &on_disk[174..238]);
        assert_eq!(
            server.log().len(),
            before_discard,
            "a small forward seek discards instead of re-opening"
        );

        // Beyond the margin → re-open (the test hook shrinks it so the
        // 14 KiB fixture can reach it).
        source.set_forward_margin(16);
        let before_far = server.log().len();
        source.seek(SeekFrom::Current(1_000)).expect("seek");
        let mut tail = Vec::new();
        source.read_to_end(&mut tail).expect("read to the end");
        assert_eq!(server.log().len(), before_far + 1, "re-opened once");
        assert_eq!(
            tail,
            on_disk[238 + 1_000..],
            "…and the far-forward bytes are exact"
        );

        // A plain `Seek` never talks to the network.
        let before_seek = server.log().len();
        source.seek(SeekFrom::Start(0)).expect("seek");
        source.seek(SeekFrom::End(0)).expect("seek");
        source.seek(SeekFrom::Current(-7)).expect("seek");
        assert_eq!(server.log().len(), before_seek, "no request on seek alone");
        assert!(probe_requests >= 1, "the probe ran first");

        // Every request that *did* happen carried the header, never the key.
        for logged in server.log() {
            assert!(!logged.line.contains(KEY), "key in a line: {}", logged.line);
            assert_eq!(
                logged.auth.as_deref(),
                Some(format!("MediaBrowser Token=\"{KEY}\"").as_str()),
                "missing auth on: {}",
                logged.line
            );
        }
    }

    /// Plan 08, step 7 (g): the source decodes through symphonia exactly
    /// like the file would.
    #[test]
    fn decoder_open_source_decodes_the_stream() {
        let server = MockServer::start();
        let source = HttpRangeSource::open(server.client(KEY), &"any".into()).expect("206 server");
        let mut decoder =
            crate::audio::Decoder::open_source(Box::new(source), Some("flac"), Duration::ZERO)
                .expect("the streamed file decodes");

        let info = decoder.info().clone();
        assert_eq!(info.codec, "FLAC");
        assert!(info.lossless, "the fixture is lossless");
        assert_eq!(info.sample_rate, 44_100);

        let mut samples = Vec::new();
        while decoder.read(&mut samples).expect("decodes") {}
        assert!(
            samples.len() > 40_000,
            "≈0.5 s of stereo audio over HTTP: {} samples",
            samples.len()
        );
    }

    /// Plan 08, step 7 (g): a server that answers `200` to the probe
    /// triggers the fallback — and the fallback really downloads the file.
    #[test]
    fn a_server_without_range_support_triggers_the_download_fallback() {
        let server = MockServer::start();
        server.set_no_ranges(true);
        let client = server.client(KEY);

        let probe = HttpRangeSource::open(client.clone(), &"any".into());
        let error = match probe {
            Err(error) => error,
            Ok(_) => panic!("a 200 answer must not yield a seekable source"),
        };
        assert_eq!(error, OpenError::NoRanges, "200 instead of 206");

        // "Cache mode for this track": download once, bytes identical.
        let dir = tempfile::tempdir().expect("temp dir");
        let never = AtomicBool::new(false);
        let path = download_fallback(&client, &"any".into(), Some("flac"), dir.path(), &never)
            .expect("downloads");
        assert_eq!(
            std::fs::read(&path).expect("readable"),
            fixture_bytes(),
            "the fallback file is the original"
        );
        assert_eq!(path, dir.path().join("any.flac"), "named after the item");
        // Reuse on a second play: no second download.
        let requests = server.log().len();
        let again = download_fallback(&client, &"any".into(), Some("flac"), dir.path(), &never)
            .expect("reuses");
        assert_eq!(again, path, "same file");
        assert_eq!(server.log().len(), requests, "nothing re-fetched");
    }

    /// Audit P4: the probe's `bytes=0-` answer already *is* the body from
    /// offset 0 — opening and then reading straight through costs one
    /// request, not a probe that is thrown away plus a re-open.
    #[test]
    fn opening_and_reading_through_is_a_single_request() {
        let server = MockServer::start();
        let mut source =
            HttpRangeSource::open(server.client(KEY), &"any".into()).expect("206 server");
        let mut all = Vec::new();
        source.read_to_end(&mut all).expect("reads");
        assert_eq!(all, fixture_bytes());
        assert_eq!(
            server.log().len(),
            1,
            "probe body reused: {:?}",
            server.log()
        );
    }

    /// Audit P2: the engine cancels a fallback download (Stop, Next, quit)
    /// and nothing half-written survives.
    #[test]
    fn a_cancelled_fallback_download_leaves_nothing_behind() {
        let server = MockServer::start();
        server.set_no_ranges(true);
        let dir = tempfile::tempdir().expect("temp dir");
        let cancel = AtomicBool::new(true);
        let result = download_fallback(
            &server.client(KEY),
            &"any".into(),
            Some("flac"),
            dir.path(),
            &cancel,
        );
        assert_eq!(result, Err("jellyfin: download cancelled".to_string()));
        assert!(
            dir.path().read_dir().unwrap().next().is_none(),
            "no .part left"
        );
    }
}
