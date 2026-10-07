//! Tell the app when audio devices are plugged in or pulled out.
//!
//! cpal has no change notification, so automatic microphone selection would
//! otherwise only notice a new device at the next recording start — and only
//! by paying a full enumeration (~40-110ms) on every start to find out. CoreAudio
//! announces every change to its device list on `kAudioHardwarePropertyDevices`;
//! this listens there and calls back once per burst of changes (plugging in a
//! USB headset adds its input and its output a few milliseconds apart).

use std::time::Duration;

/// How long the device list must stay quiet before the callback runs. A USB
/// headset arrives as several separate notifications; one callback per plug is
/// what callers want.
const SETTLE: Duration = Duration::from_millis(400);

/// Start listening. `on_change` runs on a dedicated thread, never on CoreAudio's
/// notification thread, so it may enumerate devices and reopen streams.
///
/// Returns whether notifications will actually arrive. A caller that caches
/// device lookups must not trust the cache when this is false.
#[cfg(target_os = "macos")]
pub fn watch_device_changes<F>(on_change: F) -> bool
where
    F: Fn() + Send + 'static,
{
    use log::{debug, warn};
    use objc2_core_audio::{
        kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain,
        kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, AudioObjectAddPropertyListener,
        AudioObjectID, AudioObjectPropertyAddress,
    };
    use std::ffi::c_void;
    use std::ptr::NonNull;
    use std::sync::mpsc::{self, RecvTimeoutError, Sender};
    use std::sync::{Mutex, OnceLock};

    // The listener is a plain C function, so it reaches the channel through a
    // static rather than captured state. One watcher per process.
    static NOTIFY: OnceLock<Mutex<Sender<()>>> = OnceLock::new();

    unsafe extern "C-unwind" fn listener(
        _object: AudioObjectID,
        _count: u32,
        _addresses: NonNull<AudioObjectPropertyAddress>,
        _client: *mut c_void,
    ) -> i32 {
        if let Some(tx) = NOTIFY.get() {
            let _ = tx.lock().map(|tx| tx.send(()));
        }
        0
    }

    let (tx, rx) = mpsc::channel::<()>();
    if NOTIFY.set(Mutex::new(tx)).is_err() {
        warn!("device watch: already started; ignoring a second watcher");
        return false;
    }

    let address = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDevices,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let status = unsafe {
        AudioObjectAddPropertyListener(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&address),
            Some(listener),
            std::ptr::null_mut(),
        )
    };
    if status != 0 {
        warn!("device watch: CoreAudio refused the listener (OSStatus {status})");
        return false;
    }

    std::thread::Builder::new()
        .name("device-watch".into())
        .spawn(move || {
            while rx.recv().is_ok() {
                loop {
                    match rx.recv_timeout(SETTLE) {
                        Ok(()) => continue,
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                debug!("device watch: audio device list changed");
                on_change();
            }
        })
        .is_ok()
}

#[cfg(not(target_os = "macos"))]
pub fn watch_device_changes<F>(_on_change: F) -> bool
where
    F: Fn() + Send + 'static,
{
    false
}
