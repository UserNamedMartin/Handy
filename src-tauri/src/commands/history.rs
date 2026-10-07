use crate::actions::process_transcription_output;
use crate::managers::{
    history::{HistoryManager, PaginatedHistory, UsageBucket, UsageSummary},
    transcription::TranscriptionManager,
};
use std::sync::Arc;
use tauri::{AppHandle, State};

#[tauri::command]
#[specta::specta]
pub async fn get_history_entries(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    cursor: Option<i64>,
    limit: Option<usize>,
) -> Result<PaginatedHistory, String> {
    history_manager
        .get_history_entries(cursor, limit)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn toggle_history_entry_saved(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    id: i64,
) -> Result<(), String> {
    history_manager
        .toggle_saved_status(id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn get_audio_file_path(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    file_name: String,
) -> Result<String, String> {
    let path = history_manager.get_audio_file_path(&file_name);
    path.to_str()
        .ok_or_else(|| "Invalid file path".to_string())
        .map(|s| s.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn delete_history_entry(
    _app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    id: i64,
) -> Result<(), String> {
    history_manager
        .delete_entry(id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn retry_history_entry_transcription(
    app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    transcription_manager: State<'_, Arc<TranscriptionManager>>,
    id: i64,
) -> Result<(), String> {
    let entry = history_manager
        .get_entry_by_id(id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("History entry {} not found", id))?;

    let audio_path = history_manager.get_audio_file_path(&entry.file_name);
    let samples = crate::audio_toolkit::read_wav_samples(&audio_path)
        .map_err(|e| format!("Failed to load audio: {}", e))?;

    if samples.is_empty() {
        return Err("Recording has no audio samples".to_string());
    }

    transcription_manager.initiate_model_load();

    // Billed on the audio it sends, so the ledger gets a row of its own. This
    // used to spend money invisibly: re-transcribing a long dictation a few
    // times cost real cents that the usage screen never showed.
    let seconds = samples.len() as f64 / 16_000.0;

    let tm = Arc::clone(&transcription_manager);
    let transcription = tauri::async_runtime::spawn_blocking(move || tm.transcribe(samples))
        .await
        .map_err(|e| format!("Transcription task panicked: {}", e))?;

    // A model is only loaded once `transcribe` has run, so read it here rather
    // than before — and read it whether or not the request succeeded, because a
    // failed cloud request that reached the provider is still a request. Cost
    // is charged only for a request that produced a transcript, matching what
    // the dictation path does with a failed transcription.
    let model_id = transcription_manager
        .get_current_model()
        .map(|id| crate::cloud::batch_sibling(&id).to_string());
    if let Some(model_id) = model_id {
        let usage = crate::managers::history::DictationUsage {
            duration_ms: Some((seconds * 1000.0).round() as i64),
            engine: Some(crate::cloud::engine_kind(&model_id).to_string()),
            cost_usd: transcription
                .as_ref()
                .ok()
                .and_then(|_| crate::cloud::estimate_cost_usd(&model_id, seconds)),
            model_id: Some(model_id),
        };
        if let Err(err) = history_manager.record_usage(&usage) {
            log::error!("Failed to record a re-transcription in the usage ledger: {}", err);
        }
    }

    let transcription = transcription.map_err(|e| e.to_string())?;

    if transcription.is_empty() {
        return Err("Recording contains no speech".to_string());
    }

    let processed =
        process_transcription_output(&app, &transcription, entry.post_process_requested).await;
    history_manager
        .update_transcription(
            id,
            transcription,
            processed.post_processed_text,
            processed.post_process_prompt,
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn update_history_limit(
    app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    limit: usize,
) -> Result<(), String> {
    let mut settings = crate::settings::get_settings(&app);
    settings.history_limit = limit;
    crate::settings::write_settings(&app, settings);

    history_manager
        .cleanup_old_entries()
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn update_recording_retention_period(
    app: AppHandle,
    history_manager: State<'_, Arc<HistoryManager>>,
    period: String,
) -> Result<(), String> {
    use crate::settings::RecordingRetentionPeriod;

    let retention_period = match period.as_str() {
        "never" => RecordingRetentionPeriod::Never,
        "preserve_limit" => RecordingRetentionPeriod::PreserveLimit,
        "days3" => RecordingRetentionPeriod::Days3,
        "weeks2" => RecordingRetentionPeriod::Weeks2,
        "months3" => RecordingRetentionPeriod::Months3,
        _ => return Err(format!("Invalid retention period: {}", period)),
    };

    let mut settings = crate::settings::get_settings(&app);
    settings.recording_retention_period = retention_period;
    crate::settings::write_settings(&app, settings);

    history_manager
        .cleanup_old_entries()
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// Dictation activity per local-time day for the usage screen.
#[tauri::command]
#[specta::specta]
pub async fn get_usage_daily(
    history_manager: State<'_, Arc<HistoryManager>>,
    days: Option<u32>,
) -> Result<Vec<UsageBucket>, String> {
    history_manager
        .usage_daily(days.unwrap_or(90))
        .map_err(|e| e.to_string())
}

/// Dictation activity per local-time month — the spend retrospective.
#[tauri::command]
#[specta::specta]
pub async fn get_usage_monthly(
    history_manager: State<'_, Arc<HistoryManager>>,
    months: Option<u32>,
) -> Result<Vec<UsageBucket>, String> {
    history_manager
        .usage_monthly(months.unwrap_or(12))
        .map_err(|e| e.to_string())
}

/// Totals plus the per-model split since `since` (unix seconds; the start of
/// the month for the overview's "this month"), or lifetime when `None`.
#[tauri::command]
#[specta::specta]
pub async fn get_usage_summary(
    history_manager: State<'_, Arc<HistoryManager>>,
    since: Option<i64>,
) -> Result<UsageSummary, String> {
    history_manager
        .usage_summary(since)
        .map_err(|e| e.to_string())
}
