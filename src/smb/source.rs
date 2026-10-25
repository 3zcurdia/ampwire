//! Seekable SMB source for `mode = "stream"` (plan 10, step 6): the original
//! bytes over positioned reads, nothing stored.
//!
//! SMB2/3 `READ` carries an offset, so this is far simpler than plan 08's
//! HTTP `Range` dance: [`Read`] issues one positioned read at the cursor,
//! [`Seek`] only moves the cursor — no connection to re-open and no bytes to
//! discard. The open handle is closed on drop (in the sync threading model
//! the `smb` crate only *warns* when a handle is dropped un-closed, so we
//! do it ourselves).

use std::fmt;
use std::io::{self, Read, Seek, SeekFrom};

use symphonia::core::io::MediaSource;

use smb::{Client, FileAccessMask, FileCreateArgs, GetLen, ReadAt, Resource, Status, UncPath};

/// Why a file could not be opened for streaming. Every message is fixed:
/// no path, no share name, no server and no credential ever travel in it
/// (plan 10 "Auth" and "Notices" rows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// The path does not exist on the share (the `not found: smb://…`
    /// notice path — the caller still has the canonical URL to show).
    NotFound,
    /// The one fixed line shown for anything else.
    Failed(&'static str),
}

impl OpenError {
    pub(crate) const CANNOT_OPEN: Self = OpenError::Failed("smb: cannot open file");
    pub(crate) const IS_DIRECTORY: Self = OpenError::Failed("smb: is a directory");
    pub(crate) const NO_ACCESS: Self = OpenError::Failed("smb: access denied");
    pub(crate) const CANNOT_LIST: Self = OpenError::Failed("smb: cannot list");

    /// Map a crate error onto the fixed line the user sees for a *file*.
    /// Matched on the NT status *code*, never on message text.
    pub(crate) fn from_crate(error: &smb::Error) -> OpenError {
        match error {
            smb::Error::ReceivedErrorMessage(status, _) => {
                Self::from_status(*status, Self::CANNOT_OPEN)
            }
            _ => Self::CANNOT_OPEN,
        }
    }

    /// The same mapping for a *directory* handle: anything that is not
    /// "missing" or "denied" reads as `smb: cannot list`.
    pub(crate) fn from_crate_list(error: &smb::Error) -> OpenError {
        match error {
            smb::Error::ReceivedErrorMessage(status, _) => {
                Self::from_status(*status, Self::CANNOT_LIST)
            }
            _ => Self::CANNOT_LIST,
        }
    }

    /// The status-code half of the mapping, with the line to use for
    /// every other failure. `pub(crate)` for tests: an `ErrorResponse`'s
    /// fields are private to the crate, so tests build the status alone.
    pub(crate) fn from_status(status: u32, fallback: OpenError) -> OpenError {
        if status == Status::ObjectNameNotFound as u32
            || status == Status::ObjectPathNotFound as u32
        {
            OpenError::NotFound
        } else if status == Status::AccessDenied as u32 {
            OpenError::NO_ACCESS
        } else {
            fallback
        }
    }
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::NotFound => f.write_str("smb: not found"),
            OpenError::Failed(line) => f.write_str(line),
        }
    }
}

/// Where the bytes come from: positioned reads plus an explicit close.
/// Production is an open `smb::File` ([`SmbHandle`]); tests the plan's
/// "fake SMB backend" — bytes in memory — so the cursor rules *and* the
/// decoder path run without a network.
pub(crate) trait PositionedFile: Send + Sync {
    /// Read up to `buf.len()` bytes starting at `offset` (one round trip on
    /// the wire), returning how many arrived; `0` at end of file.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;

    /// Release the server-side handle: the sync `smb` build only *warns*
    /// when a handle is dropped un-closed, so the source does it itself.
    fn close(&self);
}

/// The one line a failed read ever shows (plan 10 "Notices"): no server,
/// path, share or credential in it.
const READ_FAILED: &str = "smb: read failed";

/// The production backend: one open handle on the share.
struct SmbHandle(smb::File);

impl PositionedFile for SmbHandle {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.0
            .read_at(buf, offset)
            .map_err(|_| io::Error::other(READ_FAILED))
    }

    fn close(&self) {
        let _ = self.0.close();
    }
}

/// A `Read + Seek` view of one file on an SMB share — symphonia's
/// [`MediaSource`], so [`Decoder::open_source`][crate::audio::Decoder::open_source]
/// decodes it exactly like a local file (seek, EQ, spectrum and gapless all
/// stay unchanged).
pub struct SmbFileSource {
    file: Box<dyn PositionedFile>,
    /// Length from the open handle (`FILE_END_OF_FILE_INFORMATION`), so
    /// `byte_len` and `SeekFrom::End` never need a round trip.
    len: u64,
    /// Logical cursor: moved by `Seek`, advanced by `Read`.
    pos: u64,
}

impl SmbFileSource {
    /// Open `\\server\share\path` for reading. Runs network round trips —
    /// call it on a worker/engine thread, never on the UI thread.
    pub fn open(client: &Client, unc: &UncPath) -> Result<SmbFileSource, OpenError> {
        let access = FileAccessMask::new().with_generic_read(true);
        let args = FileCreateArgs::make_open_existing(access);
        let resource = client
            .create_file(unc, &args)
            .map_err(|error| OpenError::from_crate(&error))?;
        let file = match resource {
            Resource::File(file) => file,
            Resource::Directory(_) => return Err(OpenError::IS_DIRECTORY),
            Resource::Pipe(_) => return Err(OpenError::CANNOT_OPEN),
        };
        let len = file
            .get_len()
            .map_err(|error| OpenError::from_crate(&error))?;
        Ok(SmbFileSource {
            file: Box::new(SmbHandle(file)),
            len,
            pos: 0,
        })
    }

    /// Build the same cursor rules over another backend (plan 10, step 6's
    /// "fake SMB backend"; tests only).
    #[cfg(test)]
    pub(crate) fn with_backend(file: Box<dyn PositionedFile>, len: u64) -> SmbFileSource {
        SmbFileSource { file, len, pos: 0 }
    }

    /// The file's length in bytes (known at open — the cache refuses
    /// over-cap files before writing a single byte, plan 10, step 5).
    pub fn len(&self) -> u64 {
        self.len
    }
}

/// Where a `Seek` lands: relative to the file's length for `End`, to the
/// cursor for `Current`, never before byte 0 (a negative position would
/// wrap into a huge offset on the next positioned read).
fn cursor_pos(from: SeekFrom, pos: u64, len: u64) -> u64 {
    let target = match from {
        SeekFrom::Start(p) => p as i128,
        SeekFrom::End(offset) => len as i128 + offset as i128,
        SeekFrom::Current(offset) => pos as i128 + offset as i128,
    };
    target.max(0) as u64
}

impl Read for SmbFileSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.pos >= self.len {
            return Ok(0); // clean EOF (a seek past the end never loops)
        }
        // Never ask for bytes past the known length: those would be an
        // error instead of the EOF the demuxer expects.
        let want = out.len().min((self.len - self.pos) as usize);
        let read = self.file.read_at(&mut out[..want], self.pos)?;
        self.pos += read as u64;
        Ok(read)
    }
}

impl Seek for SmbFileSource {
    /// Move the cursor only — no request happens here; the next [`Read`]
    /// issues a positioned read at the new offset.
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.pos = cursor_pos(from, self.pos, self.len);
        Ok(self.pos)
    }
}

impl MediaSource for SmbFileSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

impl Drop for SmbFileSource {
    fn drop(&mut self) {
        // A handle leaked on drop stays open on the server until the
        // session ends — the backend closes it (no-op for the fake).
        self.file.close();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::smb::smoke::{self, session, target};

    /// The plan's "fake SMB backend": the same positioned-read contract the
    /// wire uses, served from memory.
    struct FakeFile(Vec<u8>);

    impl PositionedFile for FakeFile {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
            let start = offset.min(self.0.len() as u64) as usize;
            let end = start.saturating_add(buf.len()).min(self.0.len());
            let n = end - start;
            buf[..n].copy_from_slice(&self.0[start..end]);
            Ok(n)
        }

        fn close(&self) {}
    }

    /// The source over `bytes`, as the engine would build one.
    fn fake_source(bytes: &[u8]) -> SmbFileSource {
        SmbFileSource::with_backend(Box::new(FakeFile(bytes.to_vec())), bytes.len() as u64)
    }

    /// Plan 10, step 6: `Read`/`Seek` cursor math through the *real*
    /// implementation — forward, `End`, backwards, exactly at EOF, a
    /// partial read at the boundary, and past the end (which must be a
    /// clean `Ok(0)`, never a negative offset or an error).
    #[test]
    fn the_cursor_walks_the_file_without_ever_going_negative() {
        let mut source = fake_source(b"0123456789");
        assert_eq!(
            MediaSource::byte_len(&source),
            Some(10),
            "the length the source reports"
        );

        let mut buf = [0u8; 4];
        // Absolute, then forward from the cursor.
        assert_eq!(source.seek(SeekFrom::Start(3)).unwrap(), 3);
        source.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"3456");
        assert_eq!(source.seek(SeekFrom::Current(3)).unwrap(), 10);
        // `End` is relative to the *file* (symphonia seeks to an M4A's
        // footer with it), never to the cursor.
        assert_eq!(source.seek(SeekFrom::End(-4)).unwrap(), 6);
        source.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"6789");
        assert_eq!(source.seek(SeekFrom::End(0)).unwrap(), 10, "at EOF");
        assert_eq!(source.read(&mut buf).unwrap(), 0, "EOF is Ok(0)");

        // Backwards, clamped at both ends: a negative position would wrap
        // into a huge offset on the next positioned read.
        assert_eq!(source.seek(SeekFrom::Current(-100)).unwrap(), 0);
        assert_eq!(source.seek(SeekFrom::End(-100)).unwrap(), 0);
        source.read_exact(&mut buf[..2]).unwrap();
        assert_eq!(&buf[..2], b"01", "and reads from the start again");

        // Past the end: allowed as a cursor, reads as EOF, and a read that
        // starts inside the file never asks for bytes beyond it.
        assert_eq!(source.seek(SeekFrom::Start(100)).unwrap(), 100);
        assert_eq!(source.read(&mut buf).unwrap(), 0, "past the end: EOF");
        assert_eq!(source.seek(SeekFrom::Start(8)).unwrap(), 8);
        assert_eq!(source.read(&mut buf).unwrap(), 2, "clamped to the file");
        assert_eq!(&buf[..2], b"89");
        assert_eq!(source.read(&mut buf).unwrap(), 0, "…and then EOF");
    }

    /// Plan 10, step 6: `Decoder::open_source` over the source for *every*
    /// fixture format, through the fake backend — same codec, same sample
    /// count as reading the file off disk.
    #[test]
    fn the_decoder_reads_every_fixture_through_the_fake_backend() {
        let cases: [(&str, &str); 5] = [
            ("tone.wav", "PCM"),
            ("tone.flac", "FLAC"),
            ("tone.mp3", "MP3"),
            ("tone_aac.m4a", "AAC"),
            ("tone_alac.m4a", "ALAC"),
        ];
        for (name, codec) in cases {
            let bytes = std::fs::read(crate::smb::smoke::fixture(name)).expect("fixture");
            let source = fake_source(&bytes);
            assert_eq!(
                MediaSource::byte_len(&source),
                Some(bytes.len() as u64),
                "byte_len of {name}"
            );
            let ext = name.rsplit('.').next().expect("extension");
            let mut decoder =
                crate::audio::Decoder::open_source(Box::new(source), Some(ext), Duration::ZERO)
                    .unwrap_or_else(|e| panic!("{name} must decode: {e}"));
            assert_eq!(decoder.info().codec, codec, "codec of {name}");
            let mut samples = Vec::new();
            while decoder.read(&mut samples).expect("decodes") {}
            assert_eq!(
                samples.len(),
                crate::smb::smoke::local_samples(name),
                "{name}: samples through the source vs off disk"
            );
        }
    }

    /// Plan 10, step 6: the fixed error lines — no path, share or server.
    #[test]
    fn open_errors_are_fixed_lines() {
        let lines = [
            (OpenError::NotFound, "smb: not found"),
            (OpenError::CANNOT_OPEN, "smb: cannot open file"),
            (OpenError::IS_DIRECTORY, "smb: is a directory"),
            (OpenError::NO_ACCESS, "smb: access denied"),
        ];
        for (error, expected) in lines {
            let shown = error.to_string();
            assert_eq!(shown, expected);
            assert!(!shown.contains('/') && !shown.contains('\\'), "{shown}");
        }
    }

    /// Plan 10, step 6: NT status codes map to specific lines — matched on
    /// the code, never on message text — and transport errors are never
    /// shown raw either. `from_status` takes the line any *other* status
    /// gets, so a file and a directory can word it differently.
    #[test]
    fn status_codes_map_to_specific_lines() {
        let not_found =
            OpenError::from_status(Status::ObjectNameNotFound as u32, OpenError::CANNOT_OPEN);
        assert_eq!(not_found, OpenError::NotFound);
        let path_missing =
            OpenError::from_status(Status::ObjectPathNotFound as u32, OpenError::CANNOT_OPEN);
        assert_eq!(path_missing, OpenError::NotFound);
        assert_eq!(
            OpenError::from_status(Status::AccessDenied as u32, OpenError::CANNOT_OPEN),
            OpenError::NO_ACCESS
        );
        // A share of anything else keeps the caller's wording…
        assert_eq!(
            OpenError::from_status(Status::SharingViolation as u32, OpenError::CANNOT_OPEN),
            OpenError::CANNOT_OPEN
        );
        assert_eq!(
            OpenError::from_status(Status::SharingViolation as u32, OpenError::CANNOT_LIST),
            OpenError::CANNOT_LIST
        );
        let io = smb::Error::IoError(io::Error::other("connection reset by peer"));
        assert_eq!(OpenError::from_crate(&io), OpenError::CANNOT_OPEN);
        assert_eq!(OpenError::from_crate_list(&io), OpenError::CANNOT_LIST);
        // …and the lines themselves are fixed.
        assert_eq!(OpenError::CANNOT_LIST.to_string(), "smb: cannot list");
    }

    // --- real-server smoke (plan 10, steps 0 and 6) -------------------------
    //
    // `#[ignore]`d: they talk to a live Samba server. Run with, e.g.
    //
    // ```sh
    // AMPWIRE_SMB_TEST_SERVER=hyperion.local AMPWIRE_SMB_TEST_SHARE=shared \
    // AMPWIRE_SMB_TEST_PATH=ampwire_spike \
    // cargo test smb_spike -- --ignored --nocapture
    // ```

    /// Plan 10, step 0.3: `Decoder::open_source` over the wrapper for one
    /// flac + both m4as from `tests/fixtures/`, hosted on the share — same
    /// sample count as the local file, byte for byte through the decoder.
    #[test]
    #[ignore = "needs a live Samba server (see module docs)"]
    fn smb_spike_decoder_opens_the_hosted_fixtures() {
        let Some(target) = target() else {
            return eprintln!("smb_spike: AMPWIRE_SMB_TEST_SERVER/SHARE not set, skipped");
        };
        let (client, unc) = session(&target).expect("connect");

        for (name, codec) in [
            ("tone.flac", "FLAC"),
            ("tone_aac.m4a", "AAC"),
            ("tone_alac.m4a", "ALAC"),
        ] {
            let file = unc.clone().with_add_path(name);
            let source = match SmbFileSource::open(&client, &file) {
                Ok(source) => source,
                Err(OpenError::NotFound) => {
                    return eprintln!(
                        "smb_spike: no fixtures under this path; point AMPWIRE_SMB_TEST_PATH at \
                         the uploaded `tests/fixtures` files"
                    );
                }
                Err(error) => panic!("{name}: {error}"),
            };
            assert_eq!(
                MediaSource::byte_len(&source),
                Some(smoke::local_len(name) as u64),
                "length of {name}"
            );
            let ext = name.rsplit('.').next().expect("extension");
            let mut decoder =
                crate::audio::Decoder::open_source(Box::new(source), Some(ext), Duration::ZERO)
                    .unwrap_or_else(|e| panic!("{name} must decode: {e}"));
            assert_eq!(decoder.info().codec, codec, "codec of {name}");
            let mut samples = Vec::new();
            while decoder.read(&mut samples).expect("decodes") {}
            assert_eq!(
                samples.len(),
                smoke::local_samples(name),
                "{name}: samples over SMB vs the local file"
            );
        }
    }

    /// Plan 10, step 0.3: seek ±5 s on a real song (the fixtures are only
    /// 0.5 s long) — forward, then back to the start, decode to EOF both
    /// times.
    #[test]
    #[ignore = "needs a live Samba server (see module docs)"]
    fn smb_spike_decoder_seeks_forward_and_backward() {
        let Some(target) = target() else {
            return eprintln!("smb_spike: AMPWIRE_SMB_TEST_SERVER/SHARE not set, skipped");
        };
        let (client, unc) = session(&target).expect("connect");
        let Some(song) = smoke::find_long_audio(&client, &unc) else {
            return eprintln!("smb_spike: no audio file long enough on the share, skipped");
        };

        let mut decoder = open_song(&client, &unc, &song, Duration::ZERO)
            .unwrap_or_else(|e| panic!("{song} must decode: {e}"));
        let total = decoder
            .info()
            .duration
            .unwrap_or_else(|| panic!("{song} reports a duration"));
        assert!(total > Duration::from_secs(10), "{song} is only {total:?}");
        let mut from_start = Vec::new();
        while decoder.read(&mut from_start).expect("decodes") {}
        assert!(!from_start.is_empty());

        let mut decoder = match open_song(&client, &unc, &song, Duration::from_secs(5)) {
            Ok(decoder) => decoder,
            // Some VBR MP3s cannot seek — the player restarts the track
            // instead (plan 03's `SeekUnsupported` path), so the spike only
            // reports it.
            Err(crate::audio::PlayError::SeekUnsupported) => {
                return eprintln!("smb_spike: {song} cannot seek, skipped");
            }
            Err(error) => panic!("{song} must decode: {error}"),
        };
        assert!(
            decoder.start_offset() <= Duration::from_secs(5),
            "the seek must not land after the target"
        );
        let mut after_forward = Vec::new();
        while decoder
            .read(&mut after_forward)
            .expect("decodes after +5 s")
        {}
        assert!(
            after_forward.len() < from_start.len(),
            "a +5 s seek must leave fewer samples"
        );

        let mut decoder = open_song(&client, &unc, &song, Duration::ZERO)
            .unwrap_or_else(|e| panic!("{song} must decode: {e}"));
        let mut again = Vec::new();
        while decoder.read(&mut again).expect("decodes back at 0") {}
        assert_eq!(again.len(), from_start.len(), "back to the start");
    }

    /// Plan 10, step 6 (real server): an open source outlives the `Client`
    /// that opened it — the handle keeps the session alive on its own — so
    /// the engine can drop its login object as soon as `open` returns and
    /// never owns a connection per track.
    #[test]
    #[ignore = "needs a live Samba server (see module docs)"]
    fn smb_spike_source_outlives_its_client() {
        let Some(target) = target() else {
            return eprintln!("smb_spike: AMPWIRE_SMB_TEST_SERVER/SHARE not set, skipped");
        };
        let (client, unc) = session(&target).expect("connect");
        let file = unc.clone().with_add_path("tone.flac");
        let source = SmbFileSource::open(&client, &file).expect("open");
        drop(client); // the session must survive this

        let mut decoder =
            crate::audio::Decoder::open_source(Box::new(source), Some("flac"), Duration::ZERO)
                .expect("decodes after the client is gone");
        let mut samples = Vec::new();
        while decoder.read(&mut samples).expect("decodes") {}
        assert!(!samples.is_empty(), "read every sample with no client");
    }

    fn open_song(
        client: &Client,
        unc: &UncPath,
        name: &str,
        start: Duration,
    ) -> Result<crate::audio::Decoder, crate::audio::PlayError> {
        let file = unc.clone().with_add_path(name);
        let source = SmbFileSource::open(client, &file).unwrap_or_else(|e| panic!("{name}: {e}"));
        let ext = name.rsplit('.').next().unwrap_or("flac");
        crate::audio::Decoder::open_source(Box::new(source), Some(ext), start)
    }
}
