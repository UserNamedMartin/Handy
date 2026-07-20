//! Auto-gain for quiet / whispered speech.
//!
//! Whispered dictation sits ~15 dB below normal voice (measured: whisper
//! ~-54 dBFS RMS, normal ~-39). At that level the Silero VAD gate drops most
//! of it before it reaches Whisper, so quiet dictation "doesn't transcribe".
//! (The Whisper model itself handles quiet speech fine — the loss is purely at
//! the VAD gate.) This boosts a whispered utterance up so it survives the gate.
//!
//! It is **conditional**: an utterance already at normal-speech loudness is
//! left bit-identical, so normal dictation is untouched — there is no mode to
//! toggle. Whether to boost is decided by the utterance's own level.
//!
//! Constants come from the sweep in `examples/whisper_gain_sweep.rs` (peak→-3
//! dBFS gave the best balance of VAD pass-rate and WER without clipping).

/// Utterances whose RMS is at or above this are treated as normal voice and
/// left untouched; quieter ones are treated as whisper and boosted.
pub const WHISPER_LEVEL_DBFS: f32 = -45.0;
/// Peak-normalization target for a boosted whisper (headroom below 0 dBFS so
/// the boost never clips).
pub const BOOST_TARGET_DBFS: f32 = -3.0;
/// Hard cap on the boost, so a near-silent clip (mic barely picking anything
/// up) isn't amplified without bound.
pub const MAX_BOOST_DB: f32 = 40.0;

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt()
}

fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |m, x| m.max(x.abs()))
}

fn to_dbfs(amp: f32) -> f32 {
    if amp > 0.0 {
        20.0 * amp.log10()
    } else {
        f32::NEG_INFINITY
    }
}

/// Decide the gain (in dB) to apply to `samples`. Returns `(gain_db,
/// classified_as_whisper)`. `gain_db == 0.0` means "leave untouched".
pub fn autogain_db(samples: &[f32]) -> (f32, bool) {
    if samples.is_empty() {
        return (0.0, false);
    }
    let level = to_dbfs(rms(samples));
    if level >= WHISPER_LEVEL_DBFS {
        return (0.0, false); // normal voice → untouched
    }
    let peak_amp = peak(samples);
    if peak_amp <= 0.0 {
        return (0.0, false); // pure silence → nothing to boost
    }
    let pk = to_dbfs(peak_amp);
    // Boost the peak up to the target, never attenuate, never exceed the cap.
    let gain = (BOOST_TARGET_DBFS - pk).clamp(0.0, MAX_BOOST_DB);
    (gain, true)
}

/// Apply the conditional whisper auto-gain. Normal-volume audio is returned
/// unchanged; a whispered utterance is peak-normalized up so it survives the
/// VAD gate. Output is clamped to [-1, 1].
pub fn whisper_autogain(samples: &[f32]) -> Vec<f32> {
    let (gain_db, _) = autogain_db(samples);
    if gain_db <= 0.0 {
        return samples.to_vec();
    }
    let lin = 10f32.powf(gain_db / 20.0);
    samples.iter().map(|&x| (x * lin).clamp(-1.0, 1.0)).collect()
}

/// Like [`whisper_autogain`] but also returns the applied gain (dB) and whether
/// the utterance was classified as a whisper — for debug logging.
pub fn whisper_autogain_with_meta(samples: &[f32]) -> (Vec<f32>, f32, bool) {
    let (gain_db, is_whisper) = autogain_db(samples);
    if gain_db <= 0.0 {
        return (samples.to_vec(), gain_db, is_whisper);
    }
    let lin = 10f32.powf(gain_db / 20.0);
    let out = samples.iter().map(|&x| (x * lin).clamp(-1.0, 1.0)).collect();
    (out, gain_db, is_whisper)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sine at a given peak amplitude; its RMS is amp/sqrt(2).
    fn sine(peak_amp: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| peak_amp * (i as f32 * 0.2).sin())
            .collect()
    }

    fn dbfs_to_amp(db: f32) -> f32 {
        10f32.powf(db / 20.0)
    }

    #[test]
    fn normal_voice_is_untouched() {
        // RMS ~ -39 dBFS (peak ~ -36) — well above the whisper threshold.
        let amp = dbfs_to_amp(-36.0);
        let s = sine(amp, 16_000);
        let (gain, is_whisper) = autogain_db(&s);
        assert_eq!(gain, 0.0);
        assert!(!is_whisper);
        // bit-identical passthrough
        assert_eq!(whisper_autogain(&s), s);
    }

    /// Whisper-like signal: high crest factor (like real speech) — peak
    /// ~-30 dBFS but RMS ~-50 dBFS, so it's classified whisper AND its boost to
    /// -3 dBFS peak (~+27 dB) stays under the cap.
    fn whisper_like(n: usize) -> Vec<f32> {
        let peak_amp = dbfs_to_amp(-30.0);
        let active = n / 50; // ~2% active → RMS ~20 dB below peak
        (0..n)
            .map(|i| if i < active { peak_amp * (i as f32 * 0.3).sin() } else { 0.0 })
            .collect()
    }

    #[test]
    fn whisper_is_boosted_to_target_without_clipping() {
        let s = whisper_like(16_000);
        let (gain, is_whisper) = autogain_db(&s);
        assert!(is_whisper);
        assert!(gain > 0.0, "expected a boost, got {gain}");
        let out = whisper_autogain(&s);
        let out_peak_dbfs = to_dbfs(peak(&out));
        // peak lands at the target (a hair under, never over → no clipping)
        assert!(
            (out_peak_dbfs - BOOST_TARGET_DBFS).abs() < 0.5,
            "peak {out_peak_dbfs} dBFS, want ~{BOOST_TARGET_DBFS}"
        );
        assert!(peak(&out) <= 1.0);
    }

    #[test]
    fn boost_is_capped_for_near_silence() {
        // extremely quiet (peak ~ -80 dBFS) → boost would be ~77 dB, capped.
        let s = sine(dbfs_to_amp(-80.0), 16_000);
        let (gain, _) = autogain_db(&s);
        assert!(gain <= MAX_BOOST_DB + 0.001, "gain {gain} exceeded cap");
    }

    #[test]
    fn empty_and_silent_are_noops() {
        assert_eq!(autogain_db(&[]).0, 0.0);
        assert_eq!(whisper_autogain(&[]), Vec::<f32>::new());
        let zeros = vec![0.0f32; 480];
        // pure silence: rms 0 → treated as whisper but peak 0 → gain 0 (no divide blowup)
        let (gain, _) = autogain_db(&zeros);
        assert_eq!(gain, 0.0);
        assert_eq!(whisper_autogain(&zeros), zeros);
    }
}
