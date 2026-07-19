use crate::actions::ACTION_MAP;
use crate::managers::audio::AudioRecordingManager;
use crate::settings::ActivationMode;
use log::{debug, error, warn};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

const DEBOUNCE: Duration = Duration::from_millis(30);
const RELEASE_GRACE: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PttAction {
    Passthrough,
    DeferRelease,
    CancelRelease,
}

struct PendingRelease {
    binding_id: String,
    hotkey_string: String,
    deadline: Instant,
}

/// Commands processed sequentially by the coordinator thread.
enum Command {
    Input {
        binding_id: String,
        hotkey_string: String,
        is_pressed: bool,
        mode: ActivationMode,
    },
    Cancel {
        recording_was_active: bool,
    },
    /// Lock an active Hybrid recording hands-free (Space while holding the key).
    Latch,
    ProcessingFinished,
}

/// Pipeline lifecycle, owned exclusively by the coordinator thread.
enum Stage {
    Idle,
    Recording(String), // binding_id
    Processing,
}

fn classify_ptt_event(
    pending_release_binding: Option<&str>,
    is_pressed: bool,
    push_to_talk: bool,
    binding_id: &str,
    recording_binding: Option<&str>,
) -> PttAction {
    if !push_to_talk {
        return PttAction::Passthrough;
    }

    if is_pressed {
        if pending_release_binding == Some(binding_id) {
            PttAction::CancelRelease
        } else {
            PttAction::Passthrough
        }
    } else if recording_binding == Some(binding_id) && pending_release_binding.is_none() {
        PttAction::DeferRelease
    } else {
        PttAction::Passthrough
    }
}

/// How long a key must be held for a Hybrid-mode press to count as
/// "hold-to-talk" rather than a tap.
const HOLD_THRESHOLD: Duration = Duration::from_millis(300);
/// Max gap between the first tap's release and the second press for a
/// Hybrid-mode double-tap to latch hands-free recording.
const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(400);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HybridAction {
    None,
    Start,
    Stop,
}

/// State for a Hybrid-mode binding (hold-to-talk + double-tap-to-latch).
#[derive(Default)]
struct HybridState {
    /// If a quick tap just happened, when the double-tap window closes. While
    /// set, recording is running but not yet committed to hold or latch.
    pending_tap_deadline: Option<Instant>,
    /// Recording is locked hands-free (started by a double-tap).
    latched: bool,
    /// When the current recording's initiating press happened (to measure hold).
    press_start: Option<Instant>,
    /// Binding/hotkey owning the current Hybrid recording, so a lone-tap timeout
    /// can stop it without a live key event.
    binding_id: Option<String>,
    hotkey_string: Option<String>,
}

impl HybridState {
    fn reset(&mut self) {
        *self = HybridState::default();
    }
}

/// Decide what a Hybrid-mode press/release should do, mutating hybrid state.
///
/// - Press while idle -> start recording (could become a hold, a lone tap, or
///   the first tap of a double-tap).
/// - Release of a long press -> stop (hold-to-talk).
/// - Release of a quick tap -> keep recording, open the double-tap window.
/// - Second press within the window -> latch hands-free (keep recording).
/// - Press while latched -> stop (tap to finish).
///
/// A lone quick tap whose window expires with no second press is stopped by the
/// coordinator loop's timeout handling.
fn evaluate_hybrid(
    hybrid: &mut HybridState,
    stage: &Stage,
    binding_id: &str,
    is_pressed: bool,
    now: Instant,
    hold_threshold: Duration,
    double_tap_window: Duration,
) -> HybridAction {
    if is_pressed {
        match stage {
            Stage::Idle => {
                hybrid.press_start = Some(now);
                hybrid.latched = false;
                hybrid.pending_tap_deadline = None;
                HybridAction::Start
            }
            Stage::Recording(id) if id == binding_id => {
                if hybrid.latched {
                    // Tap to finish a hands-free (latched) recording.
                    HybridAction::Stop
                } else if hybrid.pending_tap_deadline.is_some_and(|d| now <= d) {
                    // Second tap within the window -> latch, keep recording.
                    hybrid.latched = true;
                    hybrid.pending_tap_deadline = None;
                    HybridAction::None
                } else {
                    // Extra press while already holding (e.g. key auto-repeat).
                    HybridAction::None
                }
            }
            _ => HybridAction::None,
        }
    } else {
        match stage {
            Stage::Recording(id) if id == binding_id && !hybrid.latched => {
                let held = hybrid
                    .press_start
                    .map(|t| now.saturating_duration_since(t))
                    .unwrap_or(Duration::ZERO);
                if held >= hold_threshold {
                    // Genuine hold -> stop & insert.
                    hybrid.pending_tap_deadline = None;
                    hybrid.press_start = None;
                    HybridAction::Stop
                } else {
                    // Quick tap: keep recording and wait for a possible second
                    // tap (double-tap to latch).
                    hybrid.pending_tap_deadline = Some(now + double_tap_window);
                    HybridAction::None
                }
            }
            _ => HybridAction::None,
        }
    }
}

/// Serialises all transcription lifecycle events through a single thread
/// to eliminate race conditions between keyboard shortcuts, signals, and
/// the async transcribe-paste pipeline.
pub struct TranscriptionCoordinator {
    tx: Sender<Command>,
}

pub fn is_transcribe_binding(id: &str) -> bool {
    id == "transcribe" || id == "transcribe_with_post_process" || id == "transcribe_toggle"
}

impl TranscriptionCoordinator {
    pub fn new(app: AppHandle) -> Self {
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut stage = Stage::Idle;
                let mut last_press: Option<Instant> = None;
                let mut pending_release: Option<PendingRelease> = None;
                let mut hybrid = HybridState::default();

                loop {
                    // Wake for the soonest pending deadline: a deferred
                    // push-to-talk release (X11 auto-repeat grace) or a Hybrid
                    // double-tap window waiting for a possible second tap.
                    let next_deadline = [
                        pending_release.as_ref().map(|p| p.deadline),
                        hybrid.pending_tap_deadline,
                    ]
                    .into_iter()
                    .flatten()
                    .min();

                    let cmd = if let Some(deadline) = next_deadline {
                        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                            Ok(cmd) => cmd,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                let now = Instant::now();

                                // Deferred push-to-talk release fires.
                                if pending_release.as_ref().is_some_and(|p| now >= p.deadline) {
                                    if let Some(pending) = pending_release.take() {
                                        if matches!(&stage, Stage::Recording(id) if id == &pending.binding_id)
                                        {
                                            stop(
                                                &app,
                                                &mut stage,
                                                &pending.binding_id,
                                                &pending.hotkey_string,
                                            );
                                        }
                                    }
                                }

                                // Hybrid double-tap window elapsed with no second
                                // tap: it was a lone quick tap -> stop & insert
                                // (a tiny clip, usually transcribes to nothing).
                                if hybrid.pending_tap_deadline.is_some_and(|d| now >= d) {
                                    hybrid.pending_tap_deadline = None;
                                    hybrid.press_start = None;
                                    if !hybrid.latched {
                                        if let (Some(bid), Some(hk)) =
                                            (hybrid.binding_id.clone(), hybrid.hotkey_string.clone())
                                        {
                                            if matches!(&stage, Stage::Recording(id) if id == &bid) {
                                                stop(&app, &mut stage, &bid, &hk);
                                            }
                                        }
                                        hybrid.binding_id = None;
                                        hybrid.hotkey_string = None;
                                    }
                                }
                                continue;
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    } else {
                        match rx.recv() {
                            Ok(cmd) => cmd,
                            Err(_) => break,
                        }
                    };

                    match cmd {
                        Command::Input {
                            binding_id,
                            hotkey_string,
                            is_pressed,
                            mode,
                        } => {
                            // Hybrid mode has its own state machine: hold to talk,
                            // double-tap to latch hands-free, tap to stop.
                            if matches!(mode, ActivationMode::Hybrid) {
                                match evaluate_hybrid(
                                    &mut hybrid,
                                    &stage,
                                    &binding_id,
                                    is_pressed,
                                    Instant::now(),
                                    HOLD_THRESHOLD,
                                    DOUBLE_TAP_WINDOW,
                                ) {
                                    HybridAction::Start => {
                                        start(&app, &mut stage, &binding_id, &hotkey_string);
                                        if matches!(&stage, Stage::Recording(id) if id == &binding_id)
                                        {
                                            hybrid.binding_id = Some(binding_id.clone());
                                            hybrid.hotkey_string = Some(hotkey_string.clone());
                                        } else {
                                            hybrid.reset();
                                        }
                                    }
                                    HybridAction::Stop => {
                                        stop(&app, &mut stage, &binding_id, &hotkey_string);
                                        hybrid.reset();
                                    }
                                    HybridAction::None => {}
                                }
                                continue;
                            }

                            // Non-hybrid: PushToTalk => hold; Toggle => press to
                            // toggle. (`resolve()` never yields Global here.)
                            let push_to_talk = matches!(mode, ActivationMode::PushToTalk);

                            let pending_release_binding = pending_release
                                .as_ref()
                                .map(|pending| pending.binding_id.as_str());
                            let recording_binding = match &stage {
                                Stage::Recording(id) => Some(id.as_str()),
                                _ => None,
                            };

                            match classify_ptt_event(
                                pending_release_binding,
                                is_pressed,
                                push_to_talk,
                                &binding_id,
                                recording_binding,
                            ) {
                                PttAction::CancelRelease => {
                                    pending_release = None;
                                    continue;
                                }
                                PttAction::DeferRelease => {
                                    pending_release = Some(PendingRelease {
                                        binding_id,
                                        hotkey_string,
                                        deadline: Instant::now() + RELEASE_GRACE,
                                    });
                                    continue;
                                }
                                PttAction::Passthrough => {}
                            }

                            // Debounce rapid-fire press events (key repeat / double-tap).
                            // Push-to-talk releases may be deferred above to absorb X11 auto-repeat.
                            if is_pressed {
                                let now = Instant::now();
                                if last_press.is_some_and(|t| now.duration_since(t) < DEBOUNCE) {
                                    debug!("Debounced press for '{binding_id}'");
                                    continue;
                                }
                                last_press = Some(now);
                            }

                            if push_to_talk {
                                if is_pressed && matches!(stage, Stage::Idle) {
                                    start(&app, &mut stage, &binding_id, &hotkey_string);
                                } else if !is_pressed
                                    && matches!(&stage, Stage::Recording(id) if id == &binding_id)
                                {
                                    stop(&app, &mut stage, &binding_id, &hotkey_string);
                                }
                            } else if is_pressed {
                                match &stage {
                                    Stage::Idle => {
                                        start(&app, &mut stage, &binding_id, &hotkey_string);
                                    }
                                    Stage::Recording(id) if id == &binding_id => {
                                        stop(&app, &mut stage, &binding_id, &hotkey_string);
                                    }
                                    _ => {
                                        debug!("Ignoring press for '{binding_id}': pipeline busy")
                                    }
                                }
                            }
                        }
                        Command::Cancel {
                            recording_was_active,
                        } => {
                            pending_release = None;
                            hybrid.reset();
                            // Don't reset during processing — wait for the pipeline to finish.
                            if !matches!(stage, Stage::Processing)
                                && (recording_was_active || matches!(stage, Stage::Recording(_)))
                            {
                                stage = Stage::Idle;
                            }
                        }
                        Command::Latch => {
                            // Lock a live Hybrid recording hands-free: releasing
                            // the key won't stop it; the next key press will.
                            if hybrid.binding_id.is_some()
                                && matches!(stage, Stage::Recording(_))
                            {
                                hybrid.latched = true;
                                hybrid.pending_tap_deadline = None;
                            }
                        }
                        Command::ProcessingFinished => {
                            stage = Stage::Idle;
                        }
                    }
                }
                debug!("Transcription coordinator exited");
            }));
            if let Err(e) = result {
                error!("Transcription coordinator panicked: {e:?}");
            }
        });

        Self { tx }
    }

    /// Send a keyboard/signal input event for a transcribe binding.
    /// For signal-based toggles, use `is_pressed: true` and `mode: ActivationMode::Toggle`.
    pub fn send_input(
        &self,
        binding_id: &str,
        hotkey_string: &str,
        is_pressed: bool,
        mode: ActivationMode,
    ) {
        if self
            .tx
            .send(Command::Input {
                binding_id: binding_id.to_string(),
                hotkey_string: hotkey_string.to_string(),
                is_pressed,
                mode,
            })
            .is_err()
        {
            warn!("Transcription coordinator channel closed");
        }
    }

    pub fn notify_cancel(&self, recording_was_active: bool) {
        if self
            .tx
            .send(Command::Cancel {
                recording_was_active,
            })
            .is_err()
        {
            warn!("Transcription coordinator channel closed");
        }
    }

    /// Lock the active Hybrid recording hands-free (Space while holding the key).
    pub fn notify_latch(&self) {
        if self.tx.send(Command::Latch).is_err() {
            warn!("Transcription coordinator channel closed");
        }
    }

    pub fn notify_processing_finished(&self) {
        if self.tx.send(Command::ProcessingFinished).is_err() {
            warn!("Transcription coordinator channel closed");
        }
    }
}

fn start(app: &AppHandle, stage: &mut Stage, binding_id: &str, hotkey_string: &str) {
    let Some(action) = ACTION_MAP.get(binding_id) else {
        warn!("No action in ACTION_MAP for '{binding_id}'");
        return;
    };
    action.start(app, binding_id, hotkey_string);
    if app
        .try_state::<Arc<AudioRecordingManager>>()
        .is_some_and(|a| a.is_recording())
    {
        *stage = Stage::Recording(binding_id.to_string());
    } else {
        debug!("Start for '{binding_id}' did not begin recording; staying idle");
    }
}

fn stop(app: &AppHandle, stage: &mut Stage, binding_id: &str, hotkey_string: &str) {
    let Some(action) = ACTION_MAP.get(binding_id) else {
        warn!("No action in ACTION_MAP for '{binding_id}'");
        return;
    };
    action.stop(app, binding_id, hotkey_string);
    *stage = Stage::Processing;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_to_talk_release_while_recording_defers_release() {
        assert_eq!(
            classify_ptt_event(None, false, true, "transcribe", Some("transcribe")),
            PttAction::DeferRelease
        );
    }

    #[test]
    fn push_to_talk_press_matching_pending_release_cancels_release() {
        assert_eq!(
            classify_ptt_event(
                Some("transcribe"),
                true,
                true,
                "transcribe",
                Some("transcribe")
            ),
            PttAction::CancelRelease
        );
    }

    #[test]
    fn toggle_mode_press_and_release_pass_through() {
        assert_eq!(
            classify_ptt_event(
                Some("transcribe"),
                true,
                false,
                "transcribe",
                Some("transcribe")
            ),
            PttAction::Passthrough
        );
        assert_eq!(
            classify_ptt_event(None, false, false, "transcribe", Some("transcribe")),
            PttAction::Passthrough
        );
    }

    #[test]
    fn press_for_different_binding_than_pending_release_passes_through() {
        assert_eq!(
            classify_ptt_event(
                Some("transcribe"),
                true,
                true,
                "transcribe_with_post_process",
                Some("transcribe")
            ),
            PttAction::Passthrough
        );
    }

    #[test]
    fn press_matching_pending_release_cancels_without_recording_state() {
        assert_eq!(
            classify_ptt_event(Some("transcribe"), true, true, "transcribe", None),
            PttAction::CancelRelease
        );
    }

    // ---------------------------------------------------------------------
    // Sequence-level regression coverage for issue #1539.
    //
    // Under X11 key auto-repeat, holding a push-to-talk key does not emit one
    // long press. It emits the initial press followed by a stream of
    // synthesized release/press pairs, then a single genuine release on key-up.
    // Before the fix, every synthesized release passed straight through and
    // stopped recording, so holding the key "rapidly toggled" recording on and
    // off. The fix defers each release for a short grace window and cancels it
    // when the matching auto-repeat press arrives.
    //
    // The unit tests above assert `classify_ptt_event` in isolation. The
    // simulator below threads that classifier through the same `pending_release`
    // / `stage` state transitions the coordinator loop performs (lines that
    // handle `Command::Input` and the `recv_timeout` grace expiry), so a whole
    // event burst can be exercised deterministically without a Tauri AppHandle
    // or real timers.
    // ---------------------------------------------------------------------

    const BINDING: &str = "transcribe";

    #[derive(Clone, Copy)]
    enum Ev {
        /// A key-down event (real initial press or a synthesized auto-repeat press).
        Press,
        /// A key-up event (synthesized auto-repeat release or the genuine key-up).
        Release,
        /// The `RELEASE_GRACE` window elapsed with no cancelling press arriving.
        Grace,
    }

    #[derive(Debug, PartialEq, Eq)]
    enum SimStage {
        Idle,
        Recording,
        Processing,
    }

    struct SimResult {
        starts: u32,
        stops: u32,
        stage: SimStage,
    }

    /// Mirror of the coordinator loop's decision logic for a single push-to-talk
    /// binding: it calls the real `classify_ptt_event` and applies the exact same
    /// Defer / Cancel / debounce / start / stop transitions.
    fn simulate(events: &[Ev]) -> SimResult {
        let mut stage = SimStage::Idle;
        let mut pending: Option<String> = None;
        let mut last_press_ms: Option<u64> = None;
        let mut clock_ms: u64 = 0;
        let mut starts = 0u32;
        let mut stops = 0u32;
        let debounce_ms = DEBOUNCE.as_millis() as u64;

        for ev in events {
            // Auto-repeat events arrive a few ms apart, well inside DEBOUNCE.
            clock_ms += 5;

            match ev {
                Ev::Grace => {
                    // Coordinator's `RecvTimeoutError::Timeout` arm: fire the
                    // deferred release iff we are still recording that binding.
                    if let Some(pending_binding) = pending.take() {
                        if stage == SimStage::Recording && pending_binding == BINDING {
                            stage = SimStage::Processing;
                            stops += 1;
                        }
                    }
                }
                Ev::Press | Ev::Release => {
                    let is_pressed = matches!(ev, Ev::Press);
                    let pending_binding = pending.as_deref();
                    let recording_binding = if stage == SimStage::Recording {
                        Some(BINDING)
                    } else {
                        None
                    };

                    match classify_ptt_event(
                        pending_binding,
                        is_pressed,
                        true, // push_to_talk
                        BINDING,
                        recording_binding,
                    ) {
                        PttAction::CancelRelease => {
                            pending = None;
                            continue;
                        }
                        PttAction::DeferRelease => {
                            pending = Some(BINDING.to_string());
                            continue;
                        }
                        PttAction::Passthrough => {}
                    }

                    if is_pressed {
                        if last_press_ms.is_some_and(|t| clock_ms - t < debounce_ms) {
                            continue;
                        }
                        last_press_ms = Some(clock_ms);
                    }

                    if is_pressed && stage == SimStage::Idle {
                        stage = SimStage::Recording;
                        starts += 1;
                    } else if !is_pressed && stage == SimStage::Recording {
                        stage = SimStage::Processing;
                        stops += 1;
                    }
                }
            }
        }

        SimResult {
            starts,
            stops,
            stage,
        }
    }

    /// Initial press plus several synthesized release/press pairs, as X11 emits
    /// while a push-to-talk key is held down.
    fn autorepeat_burst() -> Vec<Ev> {
        let mut events = vec![Ev::Press];
        for _ in 0..6 {
            events.push(Ev::Release);
            events.push(Ev::Press);
        }
        events
    }

    /// Regression for #1539: a burst of X11 auto-repeat release/press pairs must
    /// not stop recording. Before the fix the first synthesized release stopped
    /// recording immediately (stops == 1, stage left Recording), which produced
    /// the rapid on/off toggling. With the fix the releases are coalesced and
    /// recording stays continuously active for the whole burst.
    #[test]
    fn x11_autorepeat_burst_does_not_toggle_recording() {
        let result = simulate(&autorepeat_burst());
        assert_eq!(result.starts, 1, "recording should start exactly once");
        assert_eq!(
            result.stops, 0,
            "synthesized auto-repeat releases must not stop recording mid-burst"
        );
        assert_eq!(
            result.stage,
            SimStage::Recording,
            "recording must remain active across the entire auto-repeat burst"
        );
    }

    /// Complements the burst test: once the key is genuinely released and the
    /// grace window elapses with no re-press, recording stops exactly once. This
    /// proves the debounce only coalesces synthesized releases and does not wedge
    /// the coordinator or swallow the real key-up.
    #[test]
    fn genuine_release_after_grace_stops_recording_once() {
        let mut events = autorepeat_burst();
        events.push(Ev::Release); // genuine key-up
        events.push(Ev::Grace); // grace window elapses, no cancelling press
        let result = simulate(&events);
        assert_eq!(result.starts, 1, "recording should start exactly once");
        assert_eq!(
            result.stops, 1,
            "a genuine release should stop recording exactly once"
        );
        assert_eq!(result.stage, SimStage::Processing);
    }

    // ---------------------------------------------------------------------
    // Hybrid mode: hold-to-talk + double-tap-to-latch on one key.
    // ---------------------------------------------------------------------

    const HT: Duration = Duration::from_millis(300);
    const DTW: Duration = Duration::from_millis(400);
    const HB: &str = "transcribe";

    #[test]
    fn hybrid_hold_starts_then_stops_after_threshold() {
        let mut h = HybridState::default();
        let t0 = Instant::now();
        assert_eq!(
            evaluate_hybrid(&mut h, &Stage::Idle, HB, true, t0, HT, DTW),
            HybridAction::Start
        );
        let rec = Stage::Recording(HB.to_string());
        // Held well past the threshold: release stops (push-to-talk).
        assert_eq!(
            evaluate_hybrid(&mut h, &rec, HB, false, t0 + Duration::from_millis(500), HT, DTW),
            HybridAction::Stop
        );
    }

    #[test]
    fn hybrid_double_tap_latches_then_tap_stops() {
        let mut h = HybridState::default();
        let t0 = Instant::now();
        let rec = Stage::Recording(HB.to_string());

        // First tap: press starts recording, quick release opens the window.
        assert_eq!(
            evaluate_hybrid(&mut h, &Stage::Idle, HB, true, t0, HT, DTW),
            HybridAction::Start
        );
        assert_eq!(
            evaluate_hybrid(&mut h, &rec, HB, false, t0 + Duration::from_millis(50), HT, DTW),
            HybridAction::None
        );
        assert!(h.pending_tap_deadline.is_some());
        assert!(!h.latched);

        // Second press within the window latches hands-free.
        assert_eq!(
            evaluate_hybrid(&mut h, &rec, HB, true, t0 + Duration::from_millis(150), HT, DTW),
            HybridAction::None
        );
        assert!(h.latched);
        assert!(h.pending_tap_deadline.is_none());

        // Release while latched is ignored.
        assert_eq!(
            evaluate_hybrid(&mut h, &rec, HB, false, t0 + Duration::from_millis(200), HT, DTW),
            HybridAction::None
        );

        // A later single press stops the latched recording.
        assert_eq!(
            evaluate_hybrid(&mut h, &rec, HB, true, t0 + Duration::from_millis(3000), HT, DTW),
            HybridAction::Stop
        );
    }

    #[test]
    fn hybrid_lone_quick_tap_opens_window_without_latching() {
        let mut h = HybridState::default();
        let t0 = Instant::now();
        let rec = Stage::Recording(HB.to_string());
        assert_eq!(
            evaluate_hybrid(&mut h, &Stage::Idle, HB, true, t0, HT, DTW),
            HybridAction::Start
        );
        assert_eq!(
            evaluate_hybrid(&mut h, &rec, HB, false, t0 + Duration::from_millis(40), HT, DTW),
            HybridAction::None
        );
        // Still recording, waiting for a possible second tap; not latched.
        // (The coordinator loop stops it when the window expires.)
        assert!(h.pending_tap_deadline.is_some());
        assert!(!h.latched);
    }

    #[test]
    fn hybrid_press_after_window_does_not_latch() {
        let mut h = HybridState::default();
        let t0 = Instant::now();
        let rec = Stage::Recording(HB.to_string());
        evaluate_hybrid(&mut h, &Stage::Idle, HB, true, t0, HT, DTW);
        evaluate_hybrid(&mut h, &rec, HB, false, t0 + Duration::from_millis(40), HT, DTW);
        let late = t0 + Duration::from_millis(40) + DTW + Duration::from_millis(10);
        assert_eq!(
            evaluate_hybrid(&mut h, &rec, HB, true, late, HT, DTW),
            HybridAction::None
        );
        assert!(!h.latched);
    }
}
