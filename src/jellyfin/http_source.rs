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
//! The type sits behind a `Mutex` because symphonia's [`MediaSource`] must
//! be `Sync`; the lock covers only bookkeeping and the socket read — never
//! the UI thread (decoding happens on the engine thread).

use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::sync::Mutex;

use symphonia::core::io::MediaSource;

use super::Client;

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
    item_id: String,
    /// Total length from the probe's `Content-Range` (`byte_len`).
    len: u64,
    /// Logical cursor: moved by `Seek`, advanced by `Read`.
    pos: u64,
    body: Mutex<BodyState>,
}

impl HttpRangeSource {
    /// Probe the item once (`Range: bytes=0-`) to learn its length and to
    /// prove the server answers `206`. `200` (whole body) → [`OpenError::NoRanges`].
    pub fn open(client: Client, item_id: &str) -> Result<HttpRangeSource, OpenError> {
        let response = client
            .open_original(item_id, Some(0))
            .map_err(OpenError::Failed)?;
        if response.status().as_u16() != 206 {
            return Err(OpenError::NoRanges);
        }
        let len = content_range_total(&response).ok_or(OpenError::NoRanges)?;
        Ok(HttpRangeSource {
            client,
            item_id: item_id.to_string(),
            len,
            pos: 0,
            body: Mutex::new(BodyState {
                reader: None,
                body_pos: 0,
            }),
        })
    }
}

impl Read for HttpRangeSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.pos >= self.len {
            return Ok(0); // clean EOF (a seek past the end never loops)
        }
        let mut state = self.body.lock().expect("http source body lock");

        // Re-open when the open body cannot serve the cursor: nothing open
        // yet, a backwards move, or a forward jump past the discard margin.
        let reopen = match &state.reader {
            None => true,
            Some(_) if state.body_pos > self.pos => true,
            Some(_) if self.pos - state.body_pos > FORWARD_SEEK_LIMIT => true,
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
/// "fall back to **cache mode for this track**"): download the original once
/// into `ampwire-jellyfin-<pid>/<id>.<ext>` and hand back the local path, so
/// decoding, seeking, EQ and the spectrum tap all work unchanged. A file an
/// earlier play of the same item left behind is reused. Same 512 MiB cap as
/// the cache, same key-free error texts.
pub fn download_fallback(
    client: &Client,
    item_id: &str,
    container: Option<&str>,
) -> Result<std::path::PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("ampwire-jellyfin-{}", std::process::id()));
    std::fs::create_dir_all(&dir)
        .map_err(|reason| format!("jellyfin: cannot create the temp dir ({reason})"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }

    let path = dir.join(format!("{}.{}", item_id, super::container_ext(container)));
    if path.is_file() {
        return Ok(path);
    }

    let part = dir.join(format!("{item_id}.part"));
    let mut reader = {
        let response = client.open_original(item_id, None)?;
        response
            .into_body()
            .into_reader()
            .take(super::cache::MAX_FILE_BYTES + 1)
    };
    let mut file = std::fs::File::create(&part)
        .map_err(|reason| format!("jellyfin: cannot write the temp file ({reason})"))?;
    let copied = std::io::copy(&mut reader, &mut file)
        .map_err(|_| "jellyfin: cannot reach server".to_string())?;
    drop(file);
    if copied > super::cache::MAX_FILE_BYTES {
        let _ = std::fs::remove_file(&part);
        return Err(format!(
            "file is over the {} MiB limit",
            super::cache::MAX_FILE_BYTES / (1024 * 1024)
        ));
    }
    std::fs::rename(&part, &path)
        .map_err(|reason| format!("jellyfin: cannot write the temp file ({reason})"))?;
    Ok(path)
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
