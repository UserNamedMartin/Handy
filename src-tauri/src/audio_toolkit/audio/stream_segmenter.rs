//! Pure segmentation state machine for whisper "pseudo-streaming".
//!
//! whisper is not a streaming model — it transcribes fixed windows offline. But
//! on long dictations we can still hide the latency: while the user keeps
//! talking, close off completed clauses at natural pauses and transcribe them in
//! the background, so at key-release only the final (open) segment is left to
//! transcribe. Measured on a 195-clip corpus, cutting at silence with a ~10 s
//! minimum segment keeps quality identical to a single-shot transcription (even
//! slightly better — each segment stays fresh, avoiding whisper's long-form
//! punctuation collapse) while cutting the post-release wait from many seconds
//! to ~1 s on multi-minute clips. Cutting more aggressively (short min segment)
//! measurably hurt punctuation, so the minimum is deliberately generous; that
//! same generous minimum is what stops a mid-sentence "thinking" pause from
//! closing a segment early, so no separate semantic endpointer is needed.
//!
//! This type is intentionally pure and I/O-free: it is fed one boolean per audio
//! frame (speech vs. silence, as decided by the VAD upstream) and reports where
//! a segment should close. All timing is expressed in frames; the caller maps
//! frames to sample offsets. Kept separate from the recorder so the boundary
//! logic — the bug-prone part — is unit-testable in isolation.

/// Tuning for the segmenter. All durations are milliseconds.
#[derive(Debug, Clone, Copy)]
pub struct StreamSegmenterConfig {
    /// Audio frame size the VAD/recorder works in (Handy uses 30 ms).
    pub frame_ms: usize,
    /// A segment may only close once it holds at least this much *speech-started*
    /// audio. Generous on purpose (~10 s): short segments hurt punctuation at the
    /// joins, and a long minimum also means a brief "thinking" pause can't close a
    /// segment mid-sentence.
    pub min_segment_ms: usize,
    /// Contiguous trailing silence that closes a segment (once past the minimum).
    pub close_on_silence_ms: usize,
}

impl Default for StreamSegmenterConfig {
    fn default() -> Self {
        Self {
            frame_ms: 30,
            min_segment_ms: 10_000,
            close_on_silence_ms: 700,
        }
    }
}

/// Emitted when a segment boundary is reached.
#[derive(Debug, PartialEq, Eq)]
pub struct SegmentClose {
    /// Total frames accumulated in the just-closed segment (including the trailing
    /// silence that triggered the close).
    pub frames: usize,
    /// Trailing silence frames at the end of the segment. The caller can trim
    /// these so the segment audio ends on the last speech frame.
    pub trailing_silence_frames: usize,
}

pub struct StreamSegmenter {
    cfg: StreamSegmenterConfig,
    frames_in_segment: usize,
    trailing_silence_frames: usize,
    saw_speech: bool,
}

impl StreamSegmenter {
    pub fn new(cfg: StreamSegmenterConfig) -> Self {
        Self {
            cfg,
            frames_in_segment: 0,
            trailing_silence_frames: 0,
            saw_speech: false,
        }
    }

    #[inline]
    fn min_segment_frames(&self) -> usize {
        self.cfg.min_segment_ms.div_ceil(self.cfg.frame_ms)
    }

    #[inline]
    fn close_on_silence_frames(&self) -> usize {
        self.cfg.close_on_silence_ms.div_ceil(self.cfg.frame_ms)
    }

    /// Feed one frame. Returns `Some(SegmentClose)` if this frame completes a
    /// segment — the caller should hand the segment's samples off to background
    /// transcription and continue with a fresh segment. The segmenter resets its
    /// own counters when it reports a close.
    pub fn push(&mut self, is_speech: bool) -> Option<SegmentClose> {
        self.frames_in_segment += 1;
        if is_speech {
            self.saw_speech = true;
            self.trailing_silence_frames = 0;
        } else {
            self.trailing_silence_frames += 1;
        }

        let long_enough = self.frames_in_segment >= self.min_segment_frames();
        let paused = self.trailing_silence_frames >= self.close_on_silence_frames();
        if self.saw_speech && long_enough && paused {
            let close = SegmentClose {
                frames: self.frames_in_segment,
                trailing_silence_frames: self.trailing_silence_frames,
            };
            self.frames_in_segment = 0;
            self.trailing_silence_frames = 0;
            self.saw_speech = false;
            Some(close)
        } else {
            None
        }
    }

    /// Frames accumulated in the current (still-open) segment. At key-release the
    /// caller finalizes this remaining segment.
    pub fn open_frames(&self) -> usize {
        self.frames_in_segment
    }

    /// Whether the open segment contains any speech yet (an all-silence tail at
    /// the end need not be transcribed).
    pub fn open_has_speech(&self) -> bool {
        self.saw_speech
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> StreamSegmenterConfig {
        // 30 ms frames, 10 s min segment, 700 ms silence to close.
        StreamSegmenterConfig {
            frame_ms: 30,
            min_segment_ms: 10_000,
            close_on_silence_ms: 700,
        }
    }
    fn ms(n: usize) -> usize {
        n.div_ceil(30)
    } // frames covering n ms (matches the impl's div_ceil thresholds)

    fn feed(seg: &mut StreamSegmenter, is_speech: bool, frames: usize) -> Vec<SegmentClose> {
        let mut closes = Vec::new();
        for _ in 0..frames {
            if let Some(c) = seg.push(is_speech) {
                closes.push(c);
            }
        }
        closes
    }

    #[test]
    fn silence_only_never_closes() {
        let mut s = StreamSegmenter::new(cfg());
        assert!(feed(&mut s, false, ms(30_000)).is_empty());
        assert_eq!(s.open_has_speech(), false);
    }

    #[test]
    fn speech_shorter_than_min_does_not_close_on_pause() {
        let mut s = StreamSegmenter::new(cfg());
        // 5 s speech, then a full second of silence — still under the 10 s min.
        assert!(feed(&mut s, true, ms(5_000)).is_empty());
        assert!(feed(&mut s, false, ms(1_000)).is_empty());
    }

    #[test]
    fn thinking_pause_under_threshold_does_not_close() {
        let mut s = StreamSegmenter::new(cfg());
        feed(&mut s, true, ms(12_000)); // past the min
        // 400 ms hesitation — below the 700 ms close threshold.
        assert!(feed(&mut s, false, ms(400)).is_empty());
        // resumes speaking, keeps going.
        assert!(feed(&mut s, true, ms(3_000)).is_empty());
    }

    #[test]
    fn closes_once_past_min_then_silence() {
        let mut s = StreamSegmenter::new(cfg());
        feed(&mut s, true, ms(12_000)); // 12 s speech, past the 10 s min
        let closes = feed(&mut s, false, ms(700)); // 700 ms silence closes it
        assert_eq!(closes.len(), 1);
        let c = &closes[0];
        assert_eq!(c.trailing_silence_frames, ms(700));
        // 12 s speech + 700 ms silence, in frames
        assert_eq!(c.frames, ms(12_000) + ms(700));
        // fresh segment afterwards
        assert_eq!(s.open_frames(), 0);
        assert_eq!(s.open_has_speech(), false);
    }

    #[test]
    fn multiple_segments_each_need_their_own_min() {
        let mut s = StreamSegmenter::new(cfg());
        // segment 1
        feed(&mut s, true, ms(11_000));
        assert_eq!(feed(&mut s, false, ms(700)).len(), 1);
        // segment 2: a short 4 s clause + pause must NOT close (own min applies)
        feed(&mut s, true, ms(4_000));
        assert!(feed(&mut s, false, ms(1_000)).is_empty());
        // more speech pushes it past the min, next pause closes it
        feed(&mut s, true, ms(7_000));
        assert_eq!(feed(&mut s, false, ms(700)).len(), 1);
    }

    #[test]
    fn open_segment_tracks_remaining_tail() {
        let mut s = StreamSegmenter::new(cfg());
        feed(&mut s, true, ms(11_000));
        feed(&mut s, false, ms(700)); // closes segment 1
        feed(&mut s, true, ms(2_000)); // open tail = 2 s of speech
        assert_eq!(s.open_frames(), ms(2_000));
        assert!(s.open_has_speech());
    }
}
