//! Symphonia-backed decoder: one file in, interleaved `f32` samples out.
//!
//! Pure decode — no audio device, no threads — so it is unit-testable on any
//! machine (see `tests/fixtures/`).
//!
//! Recipe (symphonia 0.6): probe the container → pick the audio track → build
//! the codec decoder → loop `next_packet()` converting each decoded buffer to
//! interleaved `f32`.

use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;

use symphonia::core::codecs::audio::well_known::{
    CODEC_ID_ALAC, CODEC_ID_FLAC, CODEC_ID_PCM_F64BE, CODEC_ID_PCM_S32LE,
};
use symphonia::core::codecs::{
    CodecParameters, audio::AudioCodecId, audio::AudioDecoder, audio::AudioDecoderOptions,
    audio::CODEC_ID_NULL_AUDIO,
};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, TimeBase};

/// How many consecutive demux errors `read` tolerates before declaring the
/// file corrupt. Each error is supposed to consume input, but a guarantee
/// keeps a malformed file from spinning forever.
const MAX_PACKET_ERRORS: usize = 4096;

/// The exact [`EngineCmd::Load`] failure a seek produced (some VBR MP3s
/// cannot seek); the player restarts the track from 0 instead of skipping it.
pub const SEEK_UNSUPPORTED: &str = "seek not supported";

/// Everything the player needs to know about a stream before playing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    /// Codec name, upper-cased: `"FLAC"`, `"MP3"`, `"AAC"`, `"ALAC"`, `"PCM"`.
    pub codec: String,
    /// FLAC, ALAC or linear PCM (A-law/µ-law and ADPCM count as lossy).
    pub lossless: bool,
    pub sample_rate: u32,
    pub channels: u16,
    /// `codec_params.bits_per_sample`, when the codec reports one.
    pub bit_depth: Option<u8>,
    /// `num_frames / sample_rate` (timebase fallback), when known.
    pub duration: Option<Duration>,
}

/// A demuxed + decoded file. One [`Decoder`] per track being played.
pub struct Decoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    info: StreamInfo,
    /// Track time base, to convert a packet timestamp into seconds.
    time_base: Option<TimeBase>,
    /// Where the decoder really landed after [`Decoder::open_at`] (the
    /// container rounds to a packet boundary; use this, not the request).
    start_offset: Duration,
    /// First packet not yet decoded after a seek: its timestamp refines
    /// `start_offset` (the demuxer can report the frame *before* the one
    /// whose samples actually come out).
    refine_offset: bool,
    /// Reused conversion buffer: one decoded packet of interleaved `f32`.
    packet: Vec<f32>,
}

impl Decoder {
    /// Open a file and seek to `start` before returning. `Duration::ZERO`
    /// skips the seek entirely (the common case). A failed seek is an error:
    /// some VBR MP3s cannot seek, and the player then restarts the track
    /// from 0 with a notice (plan §step 4).
    pub fn open_at(path: &Path, start: Duration) -> Result<Decoder, String> {
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let ext = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_string);
        Self::open_source(Box::new(file), ext.as_deref(), start)
    }

    /// Open any symphonia [`MediaSource`](symphonia::core::io::MediaSource)
    /// — a local file (plan 03), or plan 08's seekable HTTP `Range` stream —
    /// and seek to `start` first. `ext_hint` is the container name (`"m4a"`)
    /// used as a probe hint when the source has no file name to take it from.
    pub fn open_source(
        source: Box<dyn symphonia::core::io::MediaSource>,
        ext_hint: Option<&str>,
        start: Duration,
    ) -> Result<Decoder, String> {
        let mss = MediaSourceStream::new(source, MediaSourceStreamOptions::default());

        let mut hint = Hint::new();
        if let Some(ext) = ext_hint {
            hint.with_extension(ext);
        }
        let mut format = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|e| e.to_string())?;

        // Read everything we need from the track *before* `format` is mutably
        // borrowed for packet reading (and moved into `self`).
        let (track_id, params, duration, time_base) = {
            let track = format
                .default_track(TrackType::Audio)
                .ok_or_else(|| "no audio track".to_string())?;
            let Some(CodecParameters::Audio(params)) = track.codec_params.clone() else {
                return Err("track has no audio parameters".into());
            };
            if params.codec == CODEC_ID_NULL_AUDIO {
                return Err("no codec identified".into());
            }
            let sample_rate = params
                .sample_rate
                .ok_or_else(|| "unknown sample rate".to_string())?;
            let duration = match (
                track.num_frames.filter(|frames| *frames > 0),
                track.time_base,
                track.duration,
            ) {
                (Some(frames), _, _) => {
                    Some(Duration::from_secs_f64(frames as f64 / sample_rate as f64))
                }
                (None, Some(time_base), Some(ticks)) => time_base
                    .calc_duration(ticks)
                    .map(|time| Duration::from_secs_f64(time.as_secs_f64())),
                _ => None,
            };
            (track.id, params, duration, track.time_base)
        };

        let channels = params.channels.as_ref().map_or(0, |c| c.count());
        if channels == 0 || channels > u16::MAX as usize {
            return Err("unknown channel count".into());
        }
        let info = StreamInfo {
            codec: codec_label(params.codec),
            lossless: is_lossless(params.codec),
            sample_rate: params.sample_rate.expect("checked above"),
            channels: channels as u16,
            bit_depth: params.bits_per_sample.and_then(|b| u8::try_from(b).ok()),
            duration,
        };

        let mut codec = symphonia::default::get_codecs()
            .make_audio_decoder(&params, &AudioDecoderOptions::default())
            .map_err(|e| e.to_string())?;

        // Seek after probing, before the reader is wrapped in `self`. The
        // seeked-to timestamp — not the request — is the real start offset.
        let start_offset = if start.is_zero() {
            Duration::ZERO
        } else {
            let time = Time::try_from_secs_f64(start.as_secs_f64())
                .ok_or_else(|| SEEK_UNSUPPORTED.to_string())?;
            let seeked = format
                .seek(
                    SeekMode::Accurate,
                    SeekTo::Time {
                        time,
                        track_id: Some(track_id),
                    },
                )
                .map_err(|_| SEEK_UNSUPPORTED.to_string())?;
            codec.reset();
            match time_base {
                Some(time_base) => time_base
                    .calc_time(seeked.actual_ts)
                    .map(|time| Duration::from_secs_f64(time.as_secs_f64()))
                    .unwrap_or(start),
                None => start,
            }
        };

        Ok(Decoder {
            format,
            decoder: codec,
            track_id,
            info,
            time_base,
            start_offset,
            refine_offset: !start.is_zero(),
            packet: Vec::new(),
        })
    }

    pub fn info(&self) -> &StreamInfo {
        &self.info
    }

    /// Where this decoder really started: refined from the first packet
    /// actually decoded (a container seek can land mid-frame, so the
    /// demuxer's timestamp may precede the audio by up to one frame).
    pub fn start_offset(&self) -> Duration {
        self.start_offset
    }

    /// Decode the next packet and append interleaved `f32` samples to `out`.
    ///
    /// `Ok(true)` = samples were produced, `Ok(false)` = end of stream.
    pub fn read(&mut self, out: &mut Vec<f32>) -> Result<bool, String> {
        let mut packet_errors = 0;
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(packet)) => packet,
                // Clean end of the container (symphonia 0.6 signals EOF as `None`).
                Ok(None) => return Ok(false),
                Err(SymphoniaError::DecodeError(_)) | Err(SymphoniaError::Unsupported(_)) => {
                    packet_errors += 1;
                    if packet_errors >= MAX_PACKET_ERRORS {
                        return Err("corrupt stream".into());
                    }
                    continue;
                }
                Err(SymphoniaError::IoError(e)) if e.kind() == ErrorKind::UnexpectedEof => {
                    return Ok(false);
                }
                Err(e) => return Err(e.to_string()),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    if decoded.is_empty() {
                        continue; // skip packet: no frames produced
                    }
                    // The first packet that really decodes pins down where
                    // the audio starts (a demuxer can report the frame
                    // *before* the one whose samples come out).
                    if self.refine_offset {
                        if let Some(time_base) = self.time_base
                            && let Some(time) = time_base.calc_time(packet.pts)
                        {
                            self.start_offset = Duration::from_secs_f64(time.as_secs_f64());
                        }
                        self.refine_offset = false;
                    }
                    // `decoded` borrows only `self.decoder`; `self.packet` and
                    // `out` are disjoint, so this compiles without a method
                    // taking `&mut self` (which would overlap the borrow).
                    decoded.copy_to_vec_interleaved::<f32>(&mut self.packet);
                    out.extend_from_slice(&self.packet);
                    return Ok(true);
                }
                // A damaged packet is skipped; the decoder consumed it, so the
                // format reader keeps making progress.
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(SymphoniaError::IoError(e)) if e.kind() == ErrorKind::UnexpectedEof => {
                    return Ok(false);
                }
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

/// Display name for a codec: `"PCM"` for the linear-PCM ids, otherwise the
/// registered decoder's short name upper-cased (`flac` → `FLAC`).
fn codec_label(id: AudioCodecId) -> String {
    if is_linear_pcm(id) {
        return "PCM".into();
    }
    match symphonia::default::get_codecs().get_audio_decoder(id) {
        Some(reg) => reg.codec.info.short_name.to_uppercase(),
        None => "UNKNOWN".into(),
    }
}

/// Linear PCM spans `CODEC_ID_PCM_S32LE`(0x100)..=`CODEC_ID_PCM_F64BE`(0x123).
/// A-law/µ-law (0x124/0x125) sit right after that range and stay lossy, as do
/// the ADPCM ids (0x203+) and every compressed codec.
fn is_linear_pcm(id: AudioCodecId) -> bool {
    (CODEC_ID_PCM_S32LE..=CODEC_ID_PCM_F64BE).contains(&id)
}

fn is_lossless(id: AudioCodecId) -> bool {
    id == CODEC_ID_FLAC || id == CODEC_ID_ALAC || is_linear_pcm(id)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// `tests/fixtures/<name>` — unit tests run from the crate root.
    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    /// Open + drain a fixture: total interleaved samples and its stream info.
    fn decode_all(name: &str) -> (usize, StreamInfo) {
        let mut decoder = Decoder::open_at(&fixture(name), Duration::ZERO)
            .unwrap_or_else(|e| panic!("{name} must open: {e}"));
        let info = decoder.info().clone();
        let mut samples = Vec::new();
        loop {
            match decoder.read(&mut samples) {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => panic!("{name} must decode: {e}"),
            }
        }
        (samples.len(), info)
    }

    #[test]
    fn every_fixture_opens_with_the_right_stream_info() {
        // name, codec, lossless, expected bit depth.
        let expectations: [(&str, &str, bool, Option<u8>); 5] = [
            ("tone.wav", "PCM", true, Some(16)),
            ("tone.flac", "FLAC", true, Some(16)),
            // symphonia's ALAC path doesn't publish `bits_per_sample` (the
            // container and the decoder both report `None`), so the format
            // line shows rate + channels only until plan 05 enriches it.
            ("tone_alac.m4a", "ALAC", true, None),
            ("tone.mp3", "MP3", false, None),
            ("tone_aac.m4a", "AAC", false, None),
        ];
        for (name, codec, lossless, bit_depth) in expectations {
            let (_, info) = decode_all(name);
            assert_eq!(info.codec, codec, "codec of {name}");
            assert_eq!(info.lossless, lossless, "lossless flag of {name}");
            assert_eq!(info.sample_rate, 44_100, "rate of {name}");
            assert_eq!(info.channels, 2, "channels of {name}");
            assert_eq!(info.bit_depth, bit_depth, "bit depth of {name}");
            let duration = info.duration.unwrap_or_else(|| panic!("{name} duration"));
            assert!(
                (0.45..=0.55).contains(&duration.as_secs_f64()),
                "duration of {name}: {duration:?}"
            );
        }
    }

    #[test]
    fn every_fixture_yields_about_half_a_second_of_stereo_samples() {
        // 0.5 s × 44 100 Hz × 2 ch.
        const EXPECTED: usize = 44_100;
        for name in [
            "tone.wav",
            "tone.flac",
            "tone.mp3",
            "tone_aac.m4a",
            "tone_alac.m4a",
        ] {
            let (samples, info) = decode_all(name);
            // Lossless containers decode sample-exact. MP3 honours its LAME
            // gapless info, leaving only encoder priming (the plan's ±5 %).
            // AAC-in-MP4 keeps every whole 1024-sample frame *and* the
            // encoder's priming — symphonia's isomp4 reader does no gapless
            // trimming ("Gapless: No") — which is +6.8 % on this 0.5 s clip
            // (47 104 vs 44 100 samples); on a real track it is negligible.
            let slack = match info.codec.as_str() {
                "AAC" => 0.08,
                "MP3" => 0.05,
                _ => 0.0,
            };
            let low = (EXPECTED as f64 * (1.0 - slack)) as usize;
            let high = (EXPECTED as f64 * (1.0 + slack)) as usize;
            assert!(
                (low..=high).contains(&samples),
                "{name}: {samples} samples, expected {EXPECTED} ±{:.0}% ({low}..={high})",
                slack * 100.0
            );
        }
    }

    #[test]
    fn a_text_file_disguised_as_mp3_is_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("not-audio.mp3");
        std::fs::write(&path, "these words are definitely not an MPEG stream").unwrap();
        assert!(
            Decoder::open_at(&path, Duration::ZERO).is_err(),
            "unrecognized input must be an error, not a panic"
        );
        // A path that does not exist is an error too.
        assert!(Decoder::open_at(Path::new("/does/not/exist.wav"), Duration::ZERO).is_err());
    }

    #[test]
    fn read_appends_and_reports_end_of_stream_once() {
        let mut decoder =
            Decoder::open_at(&fixture("tone.wav"), Duration::ZERO).expect("wav opens");
        let mut samples = Vec::new();
        let mut chunks = 0;
        while decoder.read(&mut samples).expect("wav decodes") {
            chunks += 1;
            assert!(chunks < 10_000, "read must eventually report EOF");
        }
        assert!(chunks > 0, "0.5 s of PCM arrives in several packets");
        // A second EOF stays an EOF.
        assert!(!decoder.read(&mut samples).expect("already at EOF"));
        assert_eq!(samples.len(), 44_100, "no extra samples after EOF");
    }

    /// Decode everything a freshly-opened (and seeked) decoder yields.
    fn decode_from(name: &str, start: Duration) -> Result<usize, String> {
        let mut decoder = Decoder::open_at(&fixture(name), start)?;
        let mut samples = Vec::new();
        while decoder.read(&mut samples).expect("decodes after the seek") {}
        Ok(samples.len())
    }

    #[test]
    fn seeking_to_250ms_yields_about_half_the_samples() {
        // The WAV is packetized finely, so a 250 ms seek on the 0.5 s
        // fixture is sample-accurate enough for the plan's ±10 %.
        let full = decode_from("tone.wav", Duration::ZERO).expect("full decode");
        let half = decode_from("tone.wav", Duration::from_millis(250)).expect("seek works");
        assert!(
            (0.9..=1.1).contains(&(half as f64 * 2.0 / full as f64)),
            "{half} of {full} samples after a 250 ms seek"
        );
    }

    #[test]
    fn a_seek_lands_before_the_target_and_stays_self_consistent() {
        // Compressed containers seek to the frame *containing* the target
        // (FLAC/ALAC frames here are 8192 samples ≈ 186 ms, so playback can
        // start up to one frame early — never late). What must hold is that
        // the reported offset is honest: offset + decoded == full length.
        for name in ["tone.flac", "tone.mp3", "tone_aac.m4a", "tone_alac.m4a"] {
            let mut decoder = Decoder::open_at(&fixture(name), Duration::from_millis(250))
                .unwrap_or_else(|e| panic!("{name} must seek: {e}"));
            let reported = decoder.start_offset();
            assert!(
                reported <= Duration::from_millis(250),
                "{name}: seek must not start after the target ({reported:?})"
            );
            let mut samples = Vec::new();
            while decoder.read(&mut samples).expect("decodes after the seek") {}
            let info = decoder.info();
            let rate = info.sample_rate as f64 * info.channels as f64;
            let decoded = Duration::from_secs_f64(samples.len() as f64 / rate);
            let total = info.duration.expect("fixture duration");
            assert!(
                (reported + decoded).abs_diff(total) < Duration::from_millis(50),
                "{name}: offset {reported:?} + decoded {decoded:?} != {total:?}"
            );
        }
    }

    #[test]
    fn a_full_length_seek_never_panics() {
        // Past the end: either a clean error or (at worst) a near-empty
        // stream — never a panic.
        for start in [Duration::from_secs(5), Duration::from_secs(3_600)] {
            match decode_from("tone.wav", start) {
                Ok(samples) => assert!(samples < 44_100, "at most a tail: {samples}"),
                Err(reason) => assert!(!reason.is_empty(), "a real error message"),
            }
        }
    }

    #[test]
    fn seeking_past_the_end_is_a_clean_error_not_a_panic() {
        let err = Decoder::open_at(&fixture("tone.wav"), Duration::from_secs(10))
            .err()
            .expect("seeking past the end cannot yield a decoder");
        assert!(
            err.contains("seek") || err.contains("out-of-range"),
            "clean seek error: {err}"
        );
    }
}
