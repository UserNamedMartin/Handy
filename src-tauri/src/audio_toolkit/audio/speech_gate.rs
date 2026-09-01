//! Did anyone actually speak?
//!
//! Answered from the audio, before a single byte goes to a provider. The
//! alternative — send it and see whether a transcript comes back — makes a
//! *timer* the judge, and a timer cannot tell "there was nothing to say" from
//! "the answer is late". Getting that backwards is expensive in both directions:
//! it either burns ~10 s and a billed request on a key brushed by accident, or
//! it throws away a real dictation because the network hiccuped.
//!
//! The shape is taken from Google's own Gemini dictation client
//! (`google-gemini/jot-gemini-transcribe-macOS`, `DictationCoordinator`), which
//! gates on energy before upload for the same reason. Two of its rules are
//! deliberately asymmetric and kept that way here: **every clause below can only
//! ever prevent a discard, never cause one.** A wasted round trip costs a
//! fraction of a cent; a discarded session costs the user's words.

use super::gain::{peak, to_dbfs};

/// Shorter than this cannot contain a word. Jot refuses to upload these at all
/// — the API errors on them, which surfaced to its users as a hard failure.
pub const MIN_SENDABLE_SECS: f32 = 0.4;

/// Absolute peak below which audio is a candidate for "nothing here".
///
/// Jot's equivalent is `0.06` on its own level curve, ≈ -58 dBFS. Measured
/// against 200 of Martin's real dictations, the *quietest* recording that
/// produced text peaks at -34.5 dBFS, so this sits 24 dB below anything he has
/// ever actually said — which is the point. It is not meant to catch quiet
/// speech; it is meant to catch a dead microphone.
///
/// **Know what this does not do.** His room tone peaks around -46 dBFS (median
/// of the quietest second of twelve long dictations), comfortably above this
/// line, so holding the key in silence still reaches the provider. That is
/// deliberate, not an oversight: room tone peaks only ~10 dB under his quietest
/// speech, and the obvious richer features do not separate them either — the
/// share of frames lifted 10 dB over the room floor runs 0.25-0.74 for speech
/// and 0.00-0.53 for pauses, which overlaps. Any threshold cutting through that
/// would sometimes discard real words, and no clause here is allowed to do
/// that. A deliberate silent hold is bounded by `tail_first_wait` instead.
pub const SILENCE_PEAK_DBFS: f32 = -58.0;

/// How far above the room a quiet peak must rise to count as speech anyway.
/// Someone talking softly in a quiet place clears this easily.
pub const ROOM_SNR_DB: f32 = 6.0;

/// The room is the quietest tenth of the recording, not its minimum: one
/// anomalous frame should not define it.
const ROOM_FLOOR_PERCENTILE: f64 = 0.10;

/// Below this many frames a percentile is meaningless, and an unmeasurable room
/// means we do not get to guess.
const MIN_FLOOR_FRAMES: usize = 8;

const FLOOR_FRAME_SECS: f32 = 0.1;

/// Finite stand-in for the -inf that digital silence produces. Without it a
/// muted microphone has no measurable level at all, the room comes back
/// unmeasurable, and the "we do not get to guess" clause keeps audio that is
/// provably empty. Jot's level curve floors it for the same reason.
const FLOOR_DBFS: f32 = -100.0;

/// dBFS with a finite floor.
fn level_dbfs(amp: f32) -> f32 {
    to_dbfs(amp).max(FLOOR_DBFS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoSpeechReason {
    /// Too short to hold a word — a key brushed, not a dictation.
    TooShort,
    /// Silent in absolute terms and no louder than the room around it.
    Silent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeechVerdict {
    /// Speech-shaped energy is present. Transcribe it — and if the transcript
    /// comes back empty, that is a dropped transcript worth retrying, not
    /// silence.
    Speech,
    /// Nothing was said. Costs nothing to conclude, so conclude it before
    /// opening a socket.
    NoSpeech(NoSpeechReason),
}

impl SpeechVerdict {
    pub fn is_speech(self) -> bool {
        matches!(self, SpeechVerdict::Speech)
    }
}

/// The room's level in dBFS: the 10th percentile of 100 ms frame RMS.
///
/// `None` when there is too little audio to say anything about the room.
fn room_floor_dbfs(samples: &[f32], sample_rate: u32) -> Option<f32> {
    let frame = (sample_rate as f32 * FLOOR_FRAME_SECS) as usize;
    if frame == 0 {
        return None;
    }
    let mut frames: Vec<f32> = samples
        .chunks_exact(frame)
        .map(|c| level_dbfs(super::gain::rms(c)))
        .collect();
    if frames.len() < MIN_FLOOR_FRAMES {
        return None;
    }
    frames.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((frames.len() - 1) as f64 * ROOM_FLOOR_PERCENTILE).round() as usize;
    Some(frames[idx])
}

/// Classify a finished recording.
pub fn classify(samples: &[f32], sample_rate: u32) -> SpeechVerdict {
    if sample_rate == 0 {
        return SpeechVerdict::Speech;
    }
    let secs = samples.len() as f32 / sample_rate as f32;
    if secs < MIN_SENDABLE_SECS {
        return SpeechVerdict::NoSpeech(NoSpeechReason::TooShort);
    }

    let peak_db = level_dbfs(peak(samples));
    // Loud enough on its own — no need to consult the room.
    if peak_db >= SILENCE_PEAK_DBFS {
        return SpeechVerdict::Speech;
    }
    // Quiet. Before calling it silence, ask the room — and if the room cannot be
    // measured, keep the audio rather than guess.
    let Some(floor_db) = room_floor_dbfs(samples, sample_rate) else {
        return SpeechVerdict::Speech;
    };
    if peak_db - floor_db >= ROOM_SNR_DB {
        // Quiet in absolute terms but clearly above the room: someone speaking
        // softly in a quiet place, not a dead microphone.
        return SpeechVerdict::Speech;
    }
    SpeechVerdict::NoSpeech(NoSpeechReason::Silent)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 16_000;

    fn dbfs_to_amp(db: f32) -> f32 {
        10f32.powf(db / 20.0)
    }

    /// Deterministic pseudo-noise at a given RMS — no rand dependency, and the
    /// same sequence every run.
    fn noise(secs: f32, rms_dbfs: f32) -> Vec<f32> {
        let n = (SR as f32 * secs) as usize;
        let amp = dbfs_to_amp(rms_dbfs);
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        (0..n)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let unit = (state >> 11) as f64 / (1u64 << 53) as f64;
                ((unit as f32) * 2.0 - 1.0) * amp * 1.732
            })
            .collect()
    }

    fn tone(secs: f32, peak_dbfs: f32) -> Vec<f32> {
        let n = (SR as f32 * secs) as usize;
        let amp = dbfs_to_amp(peak_dbfs);
        (0..n)
            .map(|i| amp * (i as f32 * 0.05).sin())
            .collect()
    }

    #[test]
    fn a_brushed_key_is_too_short_to_hold_a_word() {
        assert_eq!(
            classify(&tone(0.25, -20.0), SR),
            SpeechVerdict::NoSpeech(NoSpeechReason::TooShort)
        );
    }

    #[test]
    fn a_dead_microphone_is_silence() {
        // Digital silence, long enough that duration is not what decides it.
        assert_eq!(
            classify(&vec![0.0; SR as usize * 3], SR),
            SpeechVerdict::NoSpeech(NoSpeechReason::Silent)
        );
    }

    /// Steady room tone has no dynamics: its peak sits on its own floor. Speech
    /// is the opposite — it towers over the gaps between its own words, which is
    /// what the room comparison is really measuring.
    #[test]
    fn flat_room_tone_never_rises_above_itself() {
        let flat = vec![dbfs_to_amp(-70.0); SR as usize * 3];
        assert_eq!(
            classify(&flat, SR),
            SpeechVerdict::NoSpeech(NoSpeechReason::Silent)
        );
    }

    /// The quietest recording in Martin's 200-dictation history peaks at
    /// -34.5 dBFS. Nothing he has ever actually said may be discarded.
    #[test]
    fn real_speech_levels_are_never_discarded() {
        for peak_dbfs in [-34.5, -32.0, -25.0, -10.0] {
            assert_eq!(
                classify(&tone(2.0, peak_dbfs), SR),
                SpeechVerdict::Speech,
                "peak {peak_dbfs} dBFS must survive"
            );
        }
    }

    /// Can-only-prevent-a-discard clause: quiet in absolute terms, but clearly
    /// above a very quiet room.
    #[test]
    fn soft_speech_in_a_quiet_room_survives_on_its_lift_above_the_room() {
        let mut samples = noise(2.0, -95.0);
        // A brief utterance well under the absolute threshold, ~25 dB over the room.
        for (i, s) in tone(0.5, -70.0).into_iter().enumerate() {
            samples[i] = s;
        }
        assert_eq!(classify(&samples, SR), SpeechVerdict::Speech);
    }

    /// Can-only-prevent-a-discard clause: too little audio to measure the room
    /// means we do not get to guess.
    #[test]
    fn an_unmeasurable_room_keeps_the_audio() {
        // Long enough to pass the duration gate, too few frames for a percentile.
        let samples = vec![0.000_01f32; (SR as f32 * 0.5) as usize];
        assert!(room_floor_dbfs(&samples, SR).is_none());
        assert_eq!(classify(&samples, SR), SpeechVerdict::Speech);
    }

    #[test]
    fn a_nonsense_sample_rate_never_discards() {
        assert_eq!(classify(&[0.0; 100], 0), SpeechVerdict::Speech);
    }
}
