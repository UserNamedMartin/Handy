//! Fork feature: rich per-dictation debug bundles for offline analysis.
//!
//! When `settings.debug_capture` is on (default), every transcription writes a
//! `<app_data>/debug/<timestamp>/` folder containing:
//!   - `raw.wav`   — the untouched raw capture (16 kHz mono), before auto-gain
//!                   and VAD; this is exactly what the tuning harnesses consume.
//!   - `meta.json` — timings, signal stats, the auto-gain decision, VAD stats,
//!                   the transcript, and a settings snapshot.
//! Bundles are pruned to the newest `debug_capture_limit` (raw audio is large).
//! This is separate from the native history (`history.db` + `recordings/`),
//! which is left untouched.

use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};

/// Root of the debug bundles, sibling to the native `recordings/` dir.
pub fn debug_root(recordings_dir: &Path) -> PathBuf {
    recordings_dir
        .parent()
        .unwrap_or(recordings_dir)
        .join("debug")
}

/// Write one debug bundle (`raw.wav` + `meta.json`) and prune old ones.
/// `raw` must be 16 kHz mono. Errors are returned so the caller can log-and-ignore
/// (debug capture must never break a transcription).
pub fn write_bundle(
    root: &Path,
    timestamp: i64,
    raw: &[f32],
    meta: &serde_json::Value,
    limit: usize,
) -> Result<()> {
    let dir = root.join(timestamp.to_string());
    fs::create_dir_all(&dir)?;
    crate::audio_toolkit::save_wav_file(&dir.join("raw.wav"), raw)
        .map_err(|e| anyhow::anyhow!("save raw.wav: {e}"))?;
    fs::write(dir.join("meta.json"), serde_json::to_string_pretty(meta)?)?;
    prune(root, limit);
    Ok(())
}

/// Keep only the newest `limit` bundle folders (by timestamp name), removing the
/// oldest. Best-effort: any IO error is swallowed.
fn prune(root: &Path, limit: usize) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut dirs: Vec<(i64, PathBuf)> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .filter_map(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.parse::<i64>().ok())
                .map(|ts| (ts, p.clone()))
        })
        .collect();
    if dirs.len() <= limit {
        return;
    }
    dirs.sort_by_key(|(ts, _)| *ts); // oldest first
    let remove = dirs.len() - limit;
    for (_, path) in dirs.into_iter().take(remove) {
        let _ = fs::remove_dir_all(&path);
    }
}
