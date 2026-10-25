//! Native Samba (plan 10): `smb://server/share/path` works as a file URL
//! everywhere a local path works, with no kernel mount.
//!
//! The server is a *filesystem*, not an API (plan 08's Jellyfin): discovery
//! is a recursive directory walk over the share, playback either downloads
//! into a rolling cache window (cache mode) or reads straight through a
//! seekable source ([`source`], stream mode).
//!
//! The `smb` crate (pure Rust, SMB2/3) runs in its **sync** (`multi_threaded`)
//! threading model, so every call in this module happens on a worker or the
//! engine thread — never on the UI thread, and never inside the audio
//! callback (plans-README rules 5 and 6).

pub mod source;

use std::fmt;
use std::time::Duration;

use smb::{Client, ClientConfig, ConnectionConfig, UncPath};

/// How long a connect/login attempt may take before it gives up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Where and how to log in to one SMB server.
///
/// Credentials live here and travel only as far as `Client::share_connect` —
/// never in a URL, a notice, an error line or a `Debug` print (plan 08's
/// credential rule, plan 10 "Auth" row).
#[derive(Clone, PartialEq, Eq)]
pub struct SmbClientCfg {
    /// Host or IP from the URL (`hyperion.local`), already canonical (the
    /// `smb://` parser lowercases it) before it lands here.
    pub server: String,
    /// Empty means guest/anonymous.
    pub username: String,
    /// Never printed: the [`Debug`] impl redacts it.
    password: String,
    /// Optional NTLM domain/workgroup.
    pub domain: Option<String>,
}

impl SmbClientCfg {
    /// Credentials for `server`. `password` may be empty (guest).
    pub fn new(
        server: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            server: server.into(),
            username: username.into(),
            password: password.into(),
            domain: None,
        }
    }

    /// Connect and log in to `share`. The error is a fixed line: it carries
    /// no share name, no path and no credential (plan 10 "Auth" row).
    pub fn connect(&self, share: &str) -> Result<Client, String> {
        self.connect_raw(share)
            .map_err(|_| "smb: cannot connect".to_string())
    }

    /// [`connect`][Self::connect] with the crate's own error, for tests and
    /// for callers that log the reason themselves. Never shown to the user.
    pub(crate) fn connect_raw(&self, share: &str) -> smb::Result<Client> {
        let unc = UncPath::new(&self.server)?.with_share(share)?;
        let login = self.login_name();
        // A Samba share that maps anonymous logins onto a guest account
        // answers the session setup *unsigned* (`server signing = auto`),
        // which the client rejects unless it is told to accept that. Only
        // guest/anonymous logins get that allowance — a real account still
        // expects a signed session.
        let allow_guest =
            login.is_empty() || login.trim_start_matches('/').eq_ignore_ascii_case("guest");
        let connection = ConnectionConfig {
            timeout: Some(CONNECT_TIMEOUT),
            allow_unsigned_guest_access: allow_guest,
            ..Default::default()
        };
        // Plan 10: DFS referrals are out of scope — a DFS path must surface
        // as an error instead of being resolved behind our back.
        let config = ClientConfig {
            dfs: false,
            connection,
            ..Default::default()
        };
        let client = Client::new(config);
        client.share_connect(&unc, &login, self.password.clone())?;
        Ok(client)
    }

    /// `DOMAIN\user` when a domain is configured, else the bare user name
    /// (empty → anonymous/guest).
    fn login_name(&self) -> String {
        match self.domain.as_deref() {
            Some(domain) if !domain.is_empty() => format!("{}\\{}", domain, self.username),
            _ => self.username.clone(),
        }
    }
}

/// The password is the one field that must never reach a log line, a notice
/// or a `Debug` dump (plan 10 "Auth" row).
impl fmt::Debug for SmbClientCfg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmbClientCfg")
            .field("server", &self.server)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("domain", &self.domain)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Real-server smoke harness (plan 10, steps 0 and 6)
// ---------------------------------------------------------------------------
//
// The `#[ignore]`d tests below talk to a live Samba server and are skipped
// unless the `AMPWIRE_SMB_TEST_*` variables point at one:
//
// ```sh
// AMPWIRE_SMB_TEST_SERVER=hyperion.local AMPWIRE_SMB_TEST_SHARE=datastore \
// AMPWIRE_SMB_TEST_PATH=media/music/tree63 \
// cargo test smb_spike -- --ignored --nocapture
// ```
//
// The harness is `#[cfg(test)]` only: no server, account or URL from a
// developer machine ever ships in the binary.

#[cfg(test)]
pub(crate) mod smoke {
    use std::path::PathBuf;

    use smb::{
        DirAccessMask, Directory, FileCreateArgs, FileFullDirectoryInformation, Resource, UncPath,
    };

    use super::*;

    /// One entry found by a walk.
    pub(crate) struct Entry {
        /// Path relative to the walk root, `/`-separated.
        pub path: String,
        pub size: u64,
        pub is_dir: bool,
    }

    /// Where the smoke tests point. `path` is the directory *inside* the
    /// share (`""` = share root).
    pub(crate) struct Target {
        pub server: String,
        pub share: String,
        pub path: String,
        /// Empty → try the guest candidates in order.
        pub username: String,
        pub password: String,
    }

    /// The `AMPWIRE_SMB_TEST_*` environment, or `None` (tests then skip
    /// with a note instead of failing on machines without a server).
    pub(crate) fn target() -> Option<Target> {
        let server = std::env::var("AMPWIRE_SMB_TEST_SERVER").ok()?;
        let share = std::env::var("AMPWIRE_SMB_TEST_SHARE").ok()?;
        if server.is_empty() || share.is_empty() {
            return None;
        }
        Some(Target {
            server,
            share,
            path: std::env::var("AMPWIRE_SMB_TEST_PATH").unwrap_or_default(),
            username: std::env::var("AMPWIRE_SMB_TEST_USER").unwrap_or_default(),
            password: std::env::var("AMPWIRE_SMB_TEST_PASSWORD").unwrap_or_default(),
        })
    }

    /// Connect + log in, trying the guest candidates when no user is given
    /// (a NAS often maps anonymous access onto a guest account). Prints
    /// which login worked so the report records it.
    pub(crate) fn session(target: &Target) -> Option<(Client, UncPath)> {
        let base =
            match UncPath::new(&target.server).and_then(|base| base.with_share(&target.share)) {
                Ok(base) => base,
                Err(error) => {
                    eprintln!("smb_spike: bad server/share name: {error}");
                    return None;
                }
            };
        let unc = if target.path.is_empty() {
            base
        } else {
            base.with_path(&target.path)
        };

        let candidates: Vec<String> = if target.username.is_empty() {
            ["guest", "", "GUEST", "/GUEST"]
                .iter()
                .map(|s| (*s).to_string())
                .collect()
        } else {
            vec![target.username.clone()]
        };

        for user in candidates {
            let cfg = SmbClientCfg::new(&target.server, &user, &target.password);
            match cfg.connect_raw(&target.share) {
                Ok(client) => {
                    let shown = if user.is_empty() {
                        "<anonymous>"
                    } else {
                        user.as_str()
                    };
                    eprintln!(
                        "smb_spike: connected to //{}/{} as {shown}",
                        target.server, target.share
                    );
                    return Some((client, unc.clone()));
                }
                Err(error) => eprintln!("smb_spike: login as `{user}` failed: {error}"),
            }
        }
        eprintln!("smb_spike: no login worked");
        None
    }

    /// Recursively list `unc` (never following reparse points/symlinks —
    /// plan 10 "Out of scope"), stopping after `limit` entries.
    pub(crate) fn walk(client: &Client, unc: &UncPath, limit: usize) -> Vec<Entry> {
        let mut out = Vec::new();
        walk_into(client, unc, unc, "", limit, &mut out);
        out
    }

    /// `root` never changes (child paths are relative to it); `unc` is the
    /// directory currently open, `prefix` its path relative to `root`.
    fn walk_into(
        client: &Client,
        root: &UncPath,
        unc: &UncPath,
        prefix: &str,
        limit: usize,
        out: &mut Vec<Entry>,
    ) {
        if out.len() >= limit {
            return;
        }
        let access = DirAccessMask::new()
            .with_list_directory(true)
            .with_synchronize(true)
            .into();
        let args = FileCreateArgs::make_open_existing(access);
        let dir = match client.create_file(unc, &args) {
            Ok(Resource::Directory(dir)) => dir,
            Ok(_) => {
                eprintln!("smb_spike: not a directory: {prefix}");
                return;
            }
            Err(error) => {
                eprintln!("smb_spike: cannot list `{prefix}`: {error}");
                return;
            }
        };

        let mut children: Vec<Entry> = Vec::new();
        {
            let query = match Directory::query::<FileFullDirectoryInformation>(&dir, "*") {
                Ok(query) => query,
                Err(error) => {
                    eprintln!("smb_spike: query `{prefix}` failed: {error}");
                    let _ = dir.close();
                    return;
                }
            };
            for entry in query {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        eprintln!("smb_spike: entry in `{prefix}` failed: {error}");
                        continue;
                    }
                };
                let name = entry.file_name.to_string();
                if name == "." || name == ".." {
                    continue;
                }
                // Never follow symlinks/junctions (plan 10, out of scope).
                if entry.file_attributes.reparse_point() {
                    continue;
                }
                let path = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                children.push(Entry {
                    path,
                    size: entry.end_of_file,
                    is_dir: entry.file_attributes.directory(),
                });
            }
        }
        let _ = dir.close();

        for child in children {
            if out.len() >= limit {
                return;
            }
            out.push(Entry {
                path: child.path.clone(),
                size: child.size,
                is_dir: child.is_dir,
            });
            if child.is_dir {
                let nested = root.clone().with_add_path(&child.path);
                walk_into(client, root, &nested, &child.path, limit, out);
            }
        }
    }

    /// The largest audio file under `unc` — the streaming/seek subject.
    /// Returns `None` when nothing there could last 10 s.
    pub(crate) fn find_long_audio(client: &Client, unc: &UncPath) -> Option<String> {
        walk(client, unc, 5_000)
            .into_iter()
            .filter(|entry| !entry.is_dir && entry.size >= 1_500_000 && is_audio(&entry.path))
            .max_by_key(|entry| entry.size)
            .map(|entry| entry.path)
    }

    /// `wav/mp3/m4a/flac`, case-insensitive — the library scan's own rule.
    pub(crate) fn is_audio(path: &str) -> bool {
        crate::library::is_audio(std::path::Path::new(path))
    }

    /// `tests/fixtures/<name>` as a local file (for byte/sample comparisons).
    pub(crate) fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    pub(crate) fn local_len(name: &str) -> usize {
        std::fs::read(fixture(name)).expect("local fixture").len()
    }

    /// What the same fixture decodes to from disk — the number an SMB read
    /// must reproduce exactly.
    pub(crate) fn local_samples(name: &str) -> usize {
        let mut decoder =
            crate::audio::Decoder::open_at(&fixture(name), Duration::ZERO).expect("opens");
        let mut samples = Vec::new();
        while decoder.read(&mut samples).expect("decodes") {}
        samples.len()
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};
    use std::time::Instant;

    use smb::{FileAccessMask, FileCreateArgs, GetLen, ReadAt, Resource};

    use super::*;
    use crate::smb::smoke::{self, session, target};

    #[test]
    fn client_cfg_redacts_the_password_in_debug() {
        let cfg = SmbClientCfg::new("nas.local", "luis", "hunter2");
        let shown = format!("{cfg:?}");
        assert!(shown.contains("<redacted>"), "{shown}");
        assert!(!shown.contains("hunter2"), "password leaked: {shown}");
        // …and the real value is still there for `share_connect`.
        assert_eq!(cfg.password, "hunter2");
    }

    #[test]
    fn a_domain_prefixes_the_login_name() {
        let mut cfg = SmbClientCfg::new("nas", "luis", "");
        assert_eq!(cfg.login_name(), "luis");
        cfg.domain = Some("WORKGROUP".into());
        assert_eq!(cfg.login_name(), "WORKGROUP\\luis");
        cfg.domain = Some(String::new());
        assert_eq!(cfg.login_name(), "luis");
    }

    /// A connect that fails must not echo the server's message (it can
    /// carry share names), only the fixed line.
    #[test]
    fn connect_errors_are_fixed_lines() {
        let cfg = SmbClientCfg::new("127.0.0.1", "nobody", "secret");
        // Port 1: connection refused on loopback — no server involved.
        match cfg.connect_raw("nowhere") {
            Ok(_) => panic!("nothing listens on 127.0.0.1:445 in tests"),
            Err(error) => {
                let raw = error.to_string();
                assert!(!raw.contains("secret"), "password in error: {raw}");
            }
        }
        let shown = cfg.connect("nowhere").err().expect("must fail");
        assert_eq!(shown, "smb: cannot connect");
    }

    // --- plan 10, step 0: the spike against a real Samba server -------------

    /// Step 0.2 (extra data point): throughput per read size. The link is
    /// round-trip bound if bigger reads scale — that decides the cache
    /// chunk size (step 5) and how much headroom the stream ring needs
    /// (step 6).
    #[test]
    #[ignore = "needs a live Samba server (see module docs)"]
    fn smb_spike_throughput_by_read_size() {
        let Some(target) = target() else {
            return eprintln!("smb_spike: AMPWIRE_SMB_TEST_SERVER/SHARE not set, skipped");
        };
        let (client, unc) = session(&target).expect("connect");
        let Some(file) = smoke::find_long_audio(&client, &unc) else {
            return eprintln!("smb_spike: no audio file long enough to stream, skipped");
        };
        let path = unc.clone().with_add_path(&file.replace('/', "\\"));
        let mut source = crate::smb::source::SmbFileSource::open(&client, &path)
            .unwrap_or_else(|e| panic!("open {file}: {e}"));

        for chunk in [64 * 1024usize, 256 * 1024, 1024 * 1024] {
            let mut buf = vec![0u8; chunk];
            let started = Instant::now();
            let mut bytes = 0u64;
            source.seek(SeekFrom::Start(0)).expect("rewind");
            while started.elapsed().as_secs() < 5 {
                match source.read(&mut buf) {
                    Ok(0) => {
                        source.seek(SeekFrom::Start(0)).expect("rewind");
                    }
                    Ok(read) => bytes += read as u64,
                    Err(error) => panic!("read failed after {bytes} bytes: {error}"),
                }
            }
            let secs = started.elapsed().as_secs_f64();
            eprintln!(
                "smb_spike: {:>4} KiB reads → {:.1} MiB/s",
                chunk / 1024,
                (bytes as f64 / (1024.0 * 1024.0)) / secs
            );
        }
    }

    /// Step 0.2: connect with a user (or the guest candidates), then walk
    /// the share recursively.
    #[test]
    #[ignore = "needs a live Samba server (see module docs)"]
    fn smb_spike_connects_and_lists_recursively() {
        let Some(target) = target() else {
            return eprintln!("smb_spike: AMPWIRE_SMB_TEST_SERVER/SHARE not set, skipped");
        };
        let (client, unc) = session(&target).expect("a login must work");

        let entries = smoke::walk(&client, &unc, 5_000);
        let dirs = entries.iter().filter(|e| e.is_dir).count();
        let files = entries.len() - dirs;
        let audio = entries
            .iter()
            .filter(|e| !e.is_dir && smoke::is_audio(&e.path))
            .count();
        let bytes: u64 = entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
        eprintln!(
            "smb_spike: {dirs} dirs, {files} files ({audio} audio), {bytes} bytes under {}",
            if target.path.is_empty() {
                "the share root".to_string()
            } else {
                target.path.clone()
            }
        );
        assert!(dirs + files > 0, "the walk must find something");
        for entry in entries.iter().take(5) {
            eprintln!(
                "smb_spike:   {}{} ({} bytes)",
                entry.path,
                if entry.is_dir { "/" } else { "" },
                entry.size
            );
        }
    }

    /// Step 0.2: `read_at` at 0, at end-of-file, the file's length, a
    /// `Seek(End)` and a backward seek — on a raw handle, straight from the
    /// crate (this is what the stream-mode source is built on).
    #[test]
    #[ignore = "needs a live Samba server (see module docs)"]
    fn smb_spike_reads_at_offsets_and_seeks_both_ways() {
        let Some(target) = target() else {
            return eprintln!("smb_spike: AMPWIRE_SMB_TEST_SERVER/SHARE not set, skipped");
        };
        let (client, unc) = session(&target).expect("connect");
        let entries = smoke::walk(&client, &unc, 5_000);
        let Some(file) = entries
            .iter()
            .filter(|e| !e.is_dir && e.size > 0)
            .max_by_key(|e| e.size)
        else {
            return eprintln!("smb_spike: no files on the share, skipped");
        };
        eprintln!("smb_spike: reading {}", file.path);

        let path = unc.clone().with_add_path(&file.path.replace('/', "\\"));
        let handle = match client.create_file(
            &path,
            &FileCreateArgs::make_open_existing(FileAccessMask::new().with_generic_read(true)),
        ) {
            Ok(Resource::File(handle)) => handle,
            Ok(_) => panic!("{} is not a regular file", file.path),
            Err(error) => panic!("open {}: {error}", file.path),
        };

        // Length.
        let len = handle.get_len().expect("length");
        assert_eq!(len, file.size, "the listing's size is the real size");

        // `read_at(0)`: the first bytes arrive.
        let mut head = [0u8; 64];
        let read = handle.read_at(&mut head, 0).expect("read at 0");
        assert_eq!(read, 64, "the head of a non-empty file");
        assert!(head.iter().any(|b| *b != 0), "all-zero head looks wrong");

        // `read_at` at end-of-file: no bytes, no error.
        let mut tail = [0u8; 64];
        assert_eq!(
            handle.read_at(&mut tail, len).expect("read at EOF"),
            0,
            "EOF is Ok(0), not an error"
        );

        // `Seek(End)` + backward seek: the same bytes come back.
        let mut here = [0u8; 32];
        let at = len.saturating_sub(32);
        let read = handle.read_at(&mut here, at).expect("read near the end");
        assert_eq!(read as u64, len - at, "the last {read} bytes");
        let mut again = [0u8; 32];
        handle.read_at(&mut again, at).expect("backward read");
        assert_eq!(here, again, "positioned reads are repeatable");

        // And a backward jump after a far-forward read.
        let mut far = [0u8; 32];
        handle.read_at(&mut far, 0).expect("back to the start");
        assert_eq!(&far[..], &head[..32], "0 is still 0 after reading the end");

        handle.close().expect("close");
    }

    /// Step 0.2: a 30 s streaming read with no disconnect, and the
    /// throughput it sustains (recorded in the plan).
    #[test]
    #[ignore = "needs a live Samba server (see module docs)"]
    fn smb_spike_streams_for_thirty_seconds_without_disconnecting() {
        let Some(target) = target() else {
            return eprintln!("smb_spike: AMPWIRE_SMB_TEST_SERVER/SHARE not set, skipped");
        };
        let (client, unc) = session(&target).expect("connect");
        let Some(file) = smoke::find_long_audio(&client, &unc) else {
            return eprintln!("smb_spike: no audio file long enough to stream, skipped");
        };
        eprintln!("smb_spike: streaming `{file}` for 30 s");

        let path = unc.clone().with_add_path(&file.replace('/', "\\"));
        let mut source = crate::smb::source::SmbFileSource::open(&client, &path)
            .unwrap_or_else(|e| panic!("open {file}: {e}"));

        const CHUNK: usize = 64 * 1024;
        let mut buf = vec![0u8; CHUNK];
        let started = Instant::now();
        let mut bytes = 0u64;
        let mut passes = 0u64;
        while started.elapsed().as_secs() < 30 {
            match source.read(&mut buf) {
                // End of the file: start over, so the 30 s run covers as
                // many positioned reads as the file allows.
                Ok(0) => {
                    passes += 1;
                    source.seek(SeekFrom::Start(0)).expect("rewind");
                }
                Ok(read) => bytes += read as u64,
                Err(error) => panic!("stream broke after {bytes} bytes: {error}"),
            }
        }
        let elapsed = started.elapsed().as_secs_f64();
        eprintln!(
            "smb_spike: {:.1} MiB in {:.1} s = {:.1} MiB/s over {} passes",
            bytes as f64 / (1024.0 * 1024.0),
            elapsed,
            (bytes as f64 / (1024.0 * 1024.0)) / elapsed,
            passes.max(1)
        );
        assert!(
            bytes > 1024 * 1024,
            "30 s of reads moved only {bytes} bytes"
        );
    }
}
