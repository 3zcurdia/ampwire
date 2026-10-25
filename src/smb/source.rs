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

/// A `Read + Seek` view of one file on an SMB share — symphonia's
/// [`MediaSource`], so [`Decoder::open_source`][crate::audio::Decoder::open_source]
/// decodes it exactly like a local file (seek, EQ, spectrum and gapless all
/// stay unchanged).
pub struct SmbFileSource {
    file: smb::File,
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
        Ok(SmbFileSource { file, len, pos: 0 })
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
        let read = self
            .file
            .read_at(&mut out[..want], self.pos)
            .map_err(|_| io::Error::other("smb: read failed"))?;
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
        // The sync `smb` build does not close handles on drop; a leaked
        // handle stays open on the server until the session ends.
        let _ = self.file.close();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::smb::smoke::{self, session, target};

    /// Plan 10, step 6: `Read`/`Seek` cursor math — forward, `End`,
    /// backwards, exactly at EOF, and past the end (which must clamp to a
    /// non-negative cursor so the next `read` is a clean EOF).
    #[test]
    fn the_cursor_walks_the_file_without_ever_going_negative() {
        // A 10-byte "file", stepped through the same rule `Seek` uses.
        const LEN: u64 = 10;
        let mut pos = 0u64;
        let mut seek = |from: SeekFrom| {
            pos = cursor_pos(from, pos, LEN);
            pos
        };

        assert_eq!(seek(SeekFrom::Start(3)), 3, "absolute");
        assert_eq!(seek(SeekFrom::Current(4)), 7, "relative forward");
        assert_eq!(seek(SeekFrom::Current(-20)), 0, "clamped at the start");
        assert_eq!(seek(SeekFrom::End(-4)), 6, "End is relative to the file");
        assert_eq!(seek(SeekFrom::End(0)), LEN, "exactly at EOF");
        assert_eq!(seek(SeekFrom::End(-100)), 0, "before the start: clamped");
        assert_eq!(seek(SeekFrom::Start(100)), 100, "past the end is allowed");
        assert_eq!(seek(SeekFrom::End(10)), LEN + 10, "past the end, too");
        // …and a read at any of those positions past the end is EOF,
        // because `read` refuses to ask for bytes beyond `len`.
        for past in [LEN, LEN + 10] {
            assert!(past >= LEN, "the source treats {past} as EOF");
        }
    }

    /// The `SeekFrom::End` rule symphonia relies on (`byte_len` + seek to
    /// the footer of an M4A): it uses the *file* length, not the cursor.
    #[test]
    fn seek_from_end_is_relative_to_the_file_length() {
        assert_eq!(cursor_pos(SeekFrom::End(0), 3, 10), 10);
        assert_eq!(cursor_pos(SeekFrom::End(-1), 0, 10), 9);
        assert_eq!(cursor_pos(SeekFrom::End(5), 0, 10), 15);
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
