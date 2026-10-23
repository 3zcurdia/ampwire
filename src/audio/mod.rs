//! Real audio: decode → ring buffer → output device (plan 03).
//!
//! Three threads are involved in playback:
//!
//! 1. the **UI thread** ([`crate::app::App`]) only ever sends [`engine::EngineCmd`]s
//!    and polls [`engine::EngineEvent`]s,
//! 2. the **engine thread** (this module) owns the cpal `Stream` *and* the
//!    decoder — on macOS the cpal stream is `!Send`, so everything audio-device
//!    related is created, used and dropped on that one thread,
//! 3. **cpal's audio callback** thread, which pops `f32` samples from the SPSC
//!    ring buffer (atomics only — never allocates, locks or touches disk).
//!
//! Sample path: `Decoder` → interleaved `f32` → [`rtrb`] ring buffer → cpal
//! callback. No resampling: the stream opens at the track's own sample rate
//! and channel count.
//!
//! Plan 04 sync: the callback counts the frames it really delivered into an
//! `AtomicU64` (the clock cannot drift, and pauses freeze it for free) and
//! taps one mono sample per frame into a second small ring — the UI thread
//! runs the FFT ([`analysis::Analyzer`]) on those, never on the audio thread.

pub mod analysis;
pub mod decoder;
pub mod engine;

pub use analysis::{Analyzer, BANDS};
pub use decoder::{Decoder, SEEK_UNSUPPORTED, StreamInfo};
pub use engine::{AudioOut, Engine, EngineCmd, EngineEvent, PlaybackHandle};
