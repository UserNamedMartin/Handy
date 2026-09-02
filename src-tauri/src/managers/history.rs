use anyhow::{anyhow, Result};
use chrono::{DateTime, Local, Utc};
use log::{debug, error, info};
use rusqlite::{params, Connection, OptionalExtension};
use rusqlite_migration::{Migrations, M};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::fs;
use std::path::PathBuf;
use tauri::AppHandle;
use tauri_specta::Event;

/// Database migrations for transcription history.
/// Each migration is applied in order. The library tracks which migrations
/// have been applied using SQLite's user_version pragma.
///
/// Note: For users upgrading from tauri-plugin-sql, migrate_from_tauri_plugin_sql()
/// converts the old _sqlx_migrations table tracking to the user_version pragma,
/// ensuring migrations don't re-run on existing databases.
static MIGRATIONS: &[M] = &[
    M::up(
        "CREATE TABLE IF NOT EXISTS transcription_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            file_name TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            saved BOOLEAN NOT NULL DEFAULT 0,
            title TEXT NOT NULL,
            transcription_text TEXT NOT NULL
        );",
    ),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_processed_text TEXT;"),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_process_prompt TEXT;"),
    M::up("ALTER TABLE transcription_history ADD COLUMN post_process_requested BOOLEAN NOT NULL DEFAULT 0;"),
    // Fork feature: per-dictation usage accounting, so the app can report how
    // much you dictate and what the paid backends actually cost. Nullable
    // throughout — entries written before this migration have no such data, and
    // the aggregates must not pretend otherwise.
    M::up("ALTER TABLE transcription_history ADD COLUMN duration_ms INTEGER;"),
    M::up("ALTER TABLE transcription_history ADD COLUMN model_id TEXT;"),
    M::up("ALTER TABLE transcription_history ADD COLUMN engine TEXT;"),
    M::up("ALTER TABLE transcription_history ADD COLUMN cost_usd REAL;"),
    // Usage accounting moves out of `transcription_history`, because that table
    // is a *cache* — `cleanup_by_count` trims it to `history_limit` (200) and
    // deletes the audio with it. Reporting usage from it meant the report
    // silently forgot the past: at ~150 dictations a day it remembered about a
    // day and a half, a month's retrospective could never show a month, and a
    // day already reported would shrink every time it was looked at. Observed
    // on a real store hours apart: 2026-08-31 went from 61 dictations / 25.6 min
    // to 3 / 11.1 min, and 2026-08-30 (123 dictations, 81.9 min) vanished
    // outright. The three that survived were the ones starred by hand, which
    // pruning spares — so the "usage" left standing was just the starred rows.
    //
    // These rows are append-only and never pruned. They are ~40 bytes each;
    // at his rate that is under 2 MB a year, which buys a report that does not
    // lie.
    M::up(
        "CREATE TABLE IF NOT EXISTS usage_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp INTEGER NOT NULL,
            duration_ms INTEGER,
            model_id TEXT,
            engine TEXT,
            cost_usd REAL
        );",
    ),
    M::up("CREATE INDEX IF NOT EXISTS idx_usage_events_timestamp ON usage_events(timestamp);"),
    // Seed from whatever history still holds. It cannot bring back what pruning
    // already deleted — that is gone with its audio — but it means the report
    // starts from the surviving entries rather than from zero.
    M::up(
        "INSERT INTO usage_events (timestamp, duration_ms, model_id, engine, cost_usd)
         SELECT timestamp, duration_ms, model_id, engine, cost_usd
         FROM transcription_history;",
    ),
];

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct PaginatedHistory {
    pub entries: Vec<HistoryEntry>,
    pub has_more: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(tag = "action")]
pub enum HistoryUpdatePayload {
    #[serde(rename = "added")]
    Added { entry: HistoryEntry },
    #[serde(rename = "updated")]
    Updated { entry: HistoryEntry },
    #[serde(rename = "deleted")]
    Deleted { id: i64 },
    #[serde(rename = "toggled")]
    Toggled { id: i64 },
}

/// What one dictation consumed. Recorded alongside the transcript so the usage
/// screen can answer "how much do I dictate" and "what did the paid backends
/// cost" without re-deriving it from audio files that get pruned.
#[derive(Clone, Debug, Default)]
pub struct DictationUsage {
    /// Length of the captured audio — what providers bill on.
    pub duration_ms: Option<i64>,
    /// Catalog id of the model that produced the transcript.
    pub model_id: Option<String>,
    /// "local" or "cloud". Cheap to group on and stable even if a model id is
    /// later renamed or dropped from the catalog.
    pub engine: Option<String>,
    /// Our estimate in USD; `None` for local models, which cost nothing.
    /// Derived from billed duration and the published rate — no provider
    /// exposes a spend API, so this differs from an invoice by rounding.
    pub cost_usd: Option<f64>,
}

/// One day or month of dictation activity.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct UsageBucket {
    /// `YYYY-MM-DD` for daily buckets, `YYYY-MM` for monthly ones, in local time.
    pub period: String,
    pub dictations: i64,
    pub seconds: f64,
    pub cost_usd: f64,
    /// How many dictations in this bucket actually carried a duration, so the UI
    /// can mark a partially-recorded period rather than show a false dip.
    pub measured: i64,
}

/// Lifetime totals plus a per-model split.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct UsageSummary {
    pub dictations: i64,
    pub seconds: f64,
    pub cost_usd: f64,
    pub measured: i64,
    pub per_model: Vec<UsageByModel>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct UsageByModel {
    pub model_id: String,
    pub engine: String,
    pub dictations: i64,
    pub seconds: f64,
    pub cost_usd: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct HistoryEntry {
    pub id: i64,
    pub file_name: String,
    pub timestamp: i64,
    pub saved: bool,
    pub title: String,
    pub transcription_text: String,
    pub post_processed_text: Option<String>,
    pub post_process_prompt: Option<String>,
    pub post_process_requested: bool,
}

pub struct HistoryManager {
    app_handle: AppHandle,
    recordings_dir: PathBuf,
    db_path: PathBuf,
}

impl HistoryManager {
    pub fn new(app_handle: &AppHandle) -> Result<Self> {
        // Create recordings directory in app data dir
        let app_data_dir = crate::portable::app_data_dir(app_handle)?;
        let recordings_dir = app_data_dir.join("recordings");
        let db_path = app_data_dir.join("history.db");

        // Ensure recordings directory exists
        if !recordings_dir.exists() {
            fs::create_dir_all(&recordings_dir)?;
            debug!("Created recordings directory: {:?}", recordings_dir);
        }

        let manager = Self {
            app_handle: app_handle.clone(),
            recordings_dir,
            db_path,
        };

        // Initialize database and run migrations synchronously
        manager.init_database()?;

        Ok(manager)
    }

    fn init_database(&self) -> Result<()> {
        info!("Initializing database at {:?}", self.db_path);

        let mut conn = Connection::open(&self.db_path)?;

        // Handle migration from tauri-plugin-sql to rusqlite_migration
        // tauri-plugin-sql used _sqlx_migrations table, rusqlite_migration uses user_version pragma
        self.migrate_from_tauri_plugin_sql(&conn)?;

        // Create migrations object and run to latest version
        let migrations = Migrations::new(MIGRATIONS.to_vec());

        // Validate migrations in debug builds
        #[cfg(debug_assertions)]
        migrations.validate().expect("Invalid migrations");

        // Get current version before migration
        let version_before: i32 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        debug!("Database version before migration: {}", version_before);

        // Apply any pending migrations
        migrations.to_latest(&mut conn)?;

        // Get version after migration
        let version_after: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

        if version_after > version_before {
            info!(
                "Database migrated from version {} to {}",
                version_before, version_after
            );
        } else {
            debug!("Database already at latest version {}", version_after);
        }

        Ok(())
    }

    /// Migrate from tauri-plugin-sql's migration tracking to rusqlite_migration's.
    /// tauri-plugin-sql used a _sqlx_migrations table, while rusqlite_migration uses
    /// SQLite's user_version pragma. This function checks if the old system was in use
    /// and sets the user_version accordingly so migrations don't re-run.
    fn migrate_from_tauri_plugin_sql(&self, conn: &Connection) -> Result<()> {
        // Check if the old _sqlx_migrations table exists
        let has_sqlx_migrations: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='_sqlx_migrations'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false);

        if !has_sqlx_migrations {
            return Ok(());
        }

        // Check current user_version
        let current_version: i32 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

        if current_version > 0 {
            // Already migrated to rusqlite_migration system
            return Ok(());
        }

        // Get the highest version from the old migrations table
        let old_version: i32 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations WHERE success = 1",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if old_version > 0 {
            info!(
                "Migrating from tauri-plugin-sql (version {}) to rusqlite_migration",
                old_version
            );

            // Set user_version to match the old migration state
            conn.pragma_update(None, "user_version", old_version)?;

            // Optionally drop the old migrations table (keeping it doesn't hurt)
            // conn.execute("DROP TABLE IF EXISTS _sqlx_migrations", [])?;

            info!(
                "Migration tracking converted: user_version set to {}",
                old_version
            );
        }

        Ok(())
    }

    fn get_connection(&self) -> Result<Connection> {
        Ok(Connection::open(&self.db_path)?)
    }

    fn map_history_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEntry> {
        Ok(HistoryEntry {
            id: row.get("id")?,
            file_name: row.get("file_name")?,
            timestamp: row.get("timestamp")?,
            saved: row.get("saved")?,
            title: row.get("title")?,
            transcription_text: row.get("transcription_text")?,
            post_processed_text: row.get("post_processed_text")?,
            post_process_prompt: row.get("post_process_prompt")?,
            post_process_requested: row.get("post_process_requested")?,
        })
    }

    /// Dictation activity per local-time day, oldest first, for the last
    /// `days` days. Days with no dictations are simply absent — the UI fills
    /// the gaps, so a quiet day is never confused with missing data.
    pub fn usage_daily(&self, days: u32) -> Result<Vec<UsageBucket>> {
        let since = Utc::now().timestamp() - (days as i64) * 86_400;
        self.usage_buckets("date(timestamp, 'unixepoch', 'localtime')", Some(since))
    }

    /// Dictation activity per local-time month, oldest first. `months` bounds
    /// the window loosely (31-day months) — the grouping itself is exact.
    pub fn usage_monthly(&self, months: u32) -> Result<Vec<UsageBucket>> {
        let since = Utc::now().timestamp() - (months as i64) * 31 * 86_400;
        self.usage_buckets(
            "strftime('%Y-%m', timestamp, 'unixepoch', 'localtime')",
            Some(since),
        )
    }

    fn usage_buckets(&self, period_expr: &str, since: Option<i64>) -> Result<Vec<UsageBucket>> {
        let conn = self.get_connection()?;
        Self::usage_buckets_with_conn(&conn, period_expr, since)
    }

    fn usage_buckets_with_conn(
        conn: &Connection,
        period_expr: &str,
        since: Option<i64>,
    ) -> Result<Vec<UsageBucket>> {
        // `period_expr` is a fixed literal chosen by the two callers above, never
        // user input, so interpolating it into the SQL is safe.
        let sql = format!(
            "SELECT {period} AS period,
                    COUNT(*) AS dictations,
                    COALESCE(SUM(duration_ms), 0) / 1000.0 AS seconds,
                    COALESCE(SUM(cost_usd), 0) AS cost_usd,
                    SUM(CASE WHEN duration_ms IS NOT NULL THEN 1 ELSE 0 END) AS measured
             FROM usage_events
             WHERE timestamp >= ?1
             GROUP BY period
             ORDER BY period ASC",
            period = period_expr
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![since.unwrap_or(0)], |row| {
            Ok(UsageBucket {
                period: row.get("period")?,
                dictations: row.get("dictations")?,
                seconds: row.get("seconds")?,
                cost_usd: row.get("cost_usd")?,
                measured: row.get("measured")?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Lifetime totals and the per-model split.
    pub fn usage_summary(&self) -> Result<UsageSummary> {
        let conn = self.get_connection()?;
        Self::usage_summary_with_conn(&conn)
    }

    fn usage_summary_with_conn(conn: &Connection) -> Result<UsageSummary> {
        let (dictations, seconds, cost_usd, measured) = conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(duration_ms), 0) / 1000.0,
                    COALESCE(SUM(cost_usd), 0),
                    SUM(CASE WHEN duration_ms IS NOT NULL THEN 1 ELSE 0 END)
             FROM usage_events",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get::<_, Option<i64>>(3)?)),
        )?;

        let mut stmt = conn.prepare(
            "SELECT COALESCE(model_id, 'unknown') AS model_id,
                    COALESCE(engine, 'unknown') AS engine,
                    COUNT(*) AS dictations,
                    COALESCE(SUM(duration_ms), 0) / 1000.0 AS seconds,
                    COALESCE(SUM(cost_usd), 0) AS cost_usd
             FROM usage_events
             GROUP BY model_id, engine
             ORDER BY seconds DESC",
        )?;
        let per_model = stmt
            .query_map([], |row| {
                Ok(UsageByModel {
                    model_id: row.get("model_id")?,
                    engine: row.get("engine")?,
                    dictations: row.get("dictations")?,
                    seconds: row.get("seconds")?,
                    cost_usd: row.get("cost_usd")?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(UsageSummary {
            dictations,
            seconds,
            cost_usd,
            measured: measured.unwrap_or(0),
            per_model,
        })
    }

    pub fn recordings_dir(&self) -> &std::path::Path {
        &self.recordings_dir
    }

    /// Write one dictation: the history row (a trimmed cache) and the usage row
    /// (an append-only ledger). Returns the **history** entry's id.
    ///
    /// Static and connection-taking so the id contract below is testable.
    #[allow(clippy::too_many_arguments)]
    fn insert_dictation_with_conn(
        conn: &Connection,
        file_name: &str,
        timestamp: i64,
        title: &str,
        transcription_text: &str,
        post_process_requested: bool,
        post_processed_text: Option<&str>,
        post_process_prompt: Option<&str>,
        usage: &DictationUsage,
    ) -> Result<i64> {
        conn.execute(
            "INSERT INTO transcription_history (
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested,
                duration_ms,
                model_id,
                engine,
                cost_usd
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                file_name,
                timestamp,
                false,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested,
                usage.duration_ms,
                &usage.model_id,
                &usage.engine,
                usage.cost_usd,
            ],
        )?;

        // Read the id BEFORE the ledger insert below — `last_insert_rowid()` is
        // per-connection, not per-table, so taking it afterwards would hand the
        // history entry the usage row's id and break every operation that
        // addresses an entry by id.
        let entry_id = conn.last_insert_rowid();

        // The usage ledger is deliberately a second row rather than a column on
        // the one above: `transcription_history` is trimmed to `history_limit`
        // and this must outlive it.
        conn.execute(
            "INSERT INTO usage_events (timestamp, duration_ms, model_id, engine, cost_usd)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                timestamp,
                usage.duration_ms,
                &usage.model_id,
                &usage.engine,
                usage.cost_usd,
            ],
        )?;

        Ok(entry_id)
    }

    /// Save a new history entry to the database.
    /// The WAV file should already have been written to the recordings directory.
    pub fn save_entry(
        &self,
        file_name: String,
        transcription_text: String,
        post_process_requested: bool,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
        usage: DictationUsage,
    ) -> Result<HistoryEntry> {
        let timestamp = Utc::now().timestamp();
        let title = self.format_timestamp_title(timestamp);

        let conn = self.get_connection()?;
        let entry_id = Self::insert_dictation_with_conn(
            &conn,
            &file_name,
            timestamp,
            &title,
            &transcription_text,
            post_process_requested,
            post_processed_text.as_deref(),
            post_process_prompt.as_deref(),
            &usage,
        )?;

        let entry = HistoryEntry {
            id: entry_id,
            file_name,
            timestamp,
            saved: false,
            title,
            transcription_text,
            post_processed_text,
            post_process_prompt,
            post_process_requested,
        };

        debug!("Saved history entry with id {}", entry.id);

        self.cleanup_old_entries()?;

        // Emit typed event for real-time frontend updates
        if let Err(e) = (HistoryUpdatePayload::Added {
            entry: entry.clone(),
        })
        .emit(&self.app_handle)
        {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(entry)
    }

    /// Update an existing history entry with new transcription results (used by retry).
    pub fn update_transcription(
        &self,
        id: i64,
        transcription_text: String,
        post_processed_text: Option<String>,
        post_process_prompt: Option<String>,
    ) -> Result<HistoryEntry> {
        let conn = self.get_connection()?;
        let updated = conn.execute(
            "UPDATE transcription_history
             SET transcription_text = ?1,
                 post_processed_text = ?2,
                 post_process_prompt = ?3
             WHERE id = ?4",
            params![
                transcription_text,
                post_processed_text,
                post_process_prompt,
                id
            ],
        )?;

        if updated == 0 {
            return Err(anyhow!("History entry {} not found", id));
        }

        let entry = conn
            .query_row(
                "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested
                 FROM transcription_history WHERE id = ?1",
                params![id],
                Self::map_history_entry,
            )?;

        debug!("Updated transcription for history entry {}", id);

        if let Err(e) = (HistoryUpdatePayload::Updated {
            entry: entry.clone(),
        })
        .emit(&self.app_handle)
        {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(entry)
    }

    pub fn cleanup_old_entries(&self) -> Result<()> {
        let retention_period = crate::settings::get_recording_retention_period(&self.app_handle);

        match retention_period {
            crate::settings::RecordingRetentionPeriod::Never => {
                // Don't delete anything
                Ok(())
            }
            crate::settings::RecordingRetentionPeriod::PreserveLimit => {
                // Use the old count-based logic with history_limit
                let limit = crate::settings::get_history_limit(&self.app_handle);
                self.cleanup_by_count(limit)
            }
            _ => {
                // Use time-based logic
                self.cleanup_by_time(retention_period)
            }
        }
    }

    fn delete_entries_and_files(&self, entries: &[(i64, String)]) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }

        let conn = self.get_connection()?;
        let mut deleted_count = 0;

        for (id, file_name) in entries {
            // Delete database entry
            conn.execute(
                "DELETE FROM transcription_history WHERE id = ?1",
                params![id],
            )?;

            // Delete WAV file
            let file_path = self.recordings_dir.join(file_name);
            if file_path.exists() {
                if let Err(e) = fs::remove_file(&file_path) {
                    error!("Failed to delete WAV file {}: {}", file_name, e);
                } else {
                    debug!("Deleted old WAV file: {}", file_name);
                    deleted_count += 1;
                }
            }
        }

        Ok(deleted_count)
    }

    fn cleanup_by_count(&self, limit: usize) -> Result<()> {
        let conn = self.get_connection()?;

        // Get all entries that are not saved, ordered by timestamp desc
        let mut stmt = conn.prepare(
            "SELECT id, file_name FROM transcription_history WHERE saved = 0 ORDER BY timestamp DESC"
        )?;

        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>("id")?, row.get::<_, String>("file_name")?))
        })?;

        let mut entries: Vec<(i64, String)> = Vec::new();
        for row in rows {
            entries.push(row?);
        }

        if entries.len() > limit {
            let entries_to_delete = &entries[limit..];
            let deleted_count = self.delete_entries_and_files(entries_to_delete)?;

            if deleted_count > 0 {
                debug!("Cleaned up {} old history entries by count", deleted_count);
            }
        }

        Ok(())
    }

    fn cleanup_by_time(
        &self,
        retention_period: crate::settings::RecordingRetentionPeriod,
    ) -> Result<()> {
        let conn = self.get_connection()?;

        // Calculate cutoff timestamp (current time minus retention period)
        let now = Utc::now().timestamp();
        let cutoff_timestamp = match retention_period {
            crate::settings::RecordingRetentionPeriod::Days3 => now - (3 * 24 * 60 * 60), // 3 days in seconds
            crate::settings::RecordingRetentionPeriod::Weeks2 => now - (2 * 7 * 24 * 60 * 60), // 2 weeks in seconds
            crate::settings::RecordingRetentionPeriod::Months3 => now - (3 * 30 * 24 * 60 * 60), // 3 months in seconds (approximate)
            _ => unreachable!("Should not reach here"),
        };

        // Get all unsaved entries older than the cutoff timestamp
        let mut stmt = conn.prepare(
            "SELECT id, file_name FROM transcription_history WHERE saved = 0 AND timestamp < ?1",
        )?;

        let rows = stmt.query_map(params![cutoff_timestamp], |row| {
            Ok((row.get::<_, i64>("id")?, row.get::<_, String>("file_name")?))
        })?;

        let mut entries_to_delete: Vec<(i64, String)> = Vec::new();
        for row in rows {
            entries_to_delete.push(row?);
        }

        let deleted_count = self.delete_entries_and_files(&entries_to_delete)?;

        if deleted_count > 0 {
            debug!(
                "Cleaned up {} old history entries based on retention period",
                deleted_count
            );
        }

        Ok(())
    }

    pub async fn get_history_entries(
        &self,
        cursor: Option<i64>,
        limit: Option<usize>,
    ) -> Result<PaginatedHistory> {
        let conn = self.get_connection()?;
        let limit = limit.map(|l| l.min(100));

        let mut entries: Vec<HistoryEntry> = match (cursor, limit) {
            (Some(cursor_id), Some(lim)) => {
                let fetch_count = (lim + 1) as i64;
                let mut stmt = conn.prepare(
                    "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested
                     FROM transcription_history
                     WHERE id < ?1
                     ORDER BY id DESC
                     LIMIT ?2",
                )?;
                let result = stmt
                    .query_map(params![cursor_id, fetch_count], Self::map_history_entry)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            }
            (None, Some(lim)) => {
                let fetch_count = (lim + 1) as i64;
                let mut stmt = conn.prepare(
                    "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested
                     FROM transcription_history
                     ORDER BY id DESC
                     LIMIT ?1",
                )?;
                let result = stmt
                    .query_map(params![fetch_count], Self::map_history_entry)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            }
            (_, None) => {
                let mut stmt = conn.prepare(
                    "SELECT id, file_name, timestamp, saved, title, transcription_text, post_processed_text, post_process_prompt, post_process_requested
                     FROM transcription_history
                     ORDER BY id DESC",
                )?;
                let result = stmt
                    .query_map([], Self::map_history_entry)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            }
        };

        let has_more = limit.is_some_and(|lim| entries.len() > lim);
        if has_more {
            entries.pop();
        }

        Ok(PaginatedHistory { entries, has_more })
    }

    #[cfg(test)]
    fn get_latest_entry_with_conn(conn: &Connection) -> Result<Option<HistoryEntry>> {
        let mut stmt = conn.prepare(
            "SELECT
                id,
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested
             FROM transcription_history
             ORDER BY timestamp DESC
             LIMIT 1",
        )?;

        let entry = stmt.query_row([], Self::map_history_entry).optional()?;
        Ok(entry)
    }

    /// Get the latest entry with non-empty transcription text.
    pub fn get_latest_completed_entry(&self) -> Result<Option<HistoryEntry>> {
        let conn = self.get_connection()?;
        Self::get_latest_completed_entry_with_conn(&conn)
    }

    fn get_latest_completed_entry_with_conn(conn: &Connection) -> Result<Option<HistoryEntry>> {
        let mut stmt = conn.prepare(
            "SELECT
                id,
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested
             FROM transcription_history
             WHERE transcription_text != ''
             ORDER BY timestamp DESC
             LIMIT 1",
        )?;

        let entry = stmt.query_row([], Self::map_history_entry).optional()?;
        Ok(entry)
    }

    pub async fn toggle_saved_status(&self, id: i64) -> Result<()> {
        let conn = self.get_connection()?;

        // Get current saved status
        let current_saved: bool = conn.query_row(
            "SELECT saved FROM transcription_history WHERE id = ?1",
            params![id],
            |row| row.get("saved"),
        )?;

        let new_saved = !current_saved;

        conn.execute(
            "UPDATE transcription_history SET saved = ?1 WHERE id = ?2",
            params![new_saved, id],
        )?;

        debug!("Toggled saved status for entry {}: {}", id, new_saved);

        // Emit history updated event
        if let Err(e) = (HistoryUpdatePayload::Toggled { id }).emit(&self.app_handle) {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(())
    }

    pub fn get_audio_file_path(&self, file_name: &str) -> PathBuf {
        self.recordings_dir.join(file_name)
    }

    pub async fn get_entry_by_id(&self, id: i64) -> Result<Option<HistoryEntry>> {
        let conn = self.get_connection()?;
        let mut stmt = conn.prepare(
            "SELECT
                id,
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested
             FROM transcription_history
             WHERE id = ?1",
        )?;

        let entry = stmt.query_row([id], Self::map_history_entry).optional()?;

        Ok(entry)
    }

    pub async fn delete_entry(&self, id: i64) -> Result<()> {
        let conn = self.get_connection()?;

        // Get the entry to find the file name
        if let Some(entry) = self.get_entry_by_id(id).await? {
            // Delete the audio file first
            let file_path = self.get_audio_file_path(&entry.file_name);
            if file_path.exists() {
                if let Err(e) = fs::remove_file(&file_path) {
                    error!("Failed to delete audio file {}: {}", entry.file_name, e);
                    // Continue with database deletion even if file deletion fails
                }
            }
        }

        // Delete from database
        conn.execute(
            "DELETE FROM transcription_history WHERE id = ?1",
            params![id],
        )?;

        debug!("Deleted history entry with id: {}", id);

        // Emit history updated event
        if let Err(e) = (HistoryUpdatePayload::Deleted { id }).emit(&self.app_handle) {
            error!("Failed to emit history-updated event: {}", e);
        }

        Ok(())
    }

    fn format_timestamp_title(&self, timestamp: i64) -> String {
        if let Some(utc_datetime) = DateTime::from_timestamp(timestamp, 0) {
            // Convert UTC to local timezone
            let local_datetime = utc_datetime.with_timezone(&Local);
            local_datetime.format("%B %e, %Y - %l:%M%p").to_string()
        } else {
            format!("Recording {}", timestamp)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{params, Connection};

    fn setup_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            "CREATE TABLE transcription_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_name TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                saved BOOLEAN NOT NULL DEFAULT 0,
                title TEXT NOT NULL,
                transcription_text TEXT NOT NULL,
                post_processed_text TEXT,
                post_process_prompt TEXT,
                post_process_requested BOOLEAN NOT NULL DEFAULT 0,
                duration_ms INTEGER,
                model_id TEXT,
                engine TEXT,
                cost_usd REAL
            );",
        )
        .expect("create transcription_history table");
        conn.execute_batch(
            "CREATE TABLE usage_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL,
                duration_ms INTEGER,
                model_id TEXT,
                engine TEXT,
                cost_usd REAL
            );",
        )
        .expect("create usage_events table");
        conn
    }

    fn insert_usage(conn: &Connection, timestamp: i64, duration_ms: Option<i64>, cost: Option<f64>) {
        conn.execute(
            "INSERT INTO usage_events (timestamp, duration_ms, model_id, engine, cost_usd)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                timestamp,
                duration_ms,
                "gemini-3.5-transcribe-live",
                "cloud",
                cost,
            ],
        )
        .expect("insert usage event");
    }

    fn insert_entry(conn: &Connection, timestamp: i64, text: &str, post_processed: Option<&str>) {
        conn.execute(
            "INSERT INTO transcription_history (
                file_name,
                timestamp,
                saved,
                title,
                transcription_text,
                post_processed_text,
                post_process_prompt,
                post_process_requested
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                format!("handy-{}.wav", timestamp),
                timestamp,
                false,
                format!("Recording {}", timestamp),
                text,
                post_processed,
                Option::<String>::None,
                false,
            ],
        )
        .expect("insert history entry");
    }

    /// `last_insert_rowid()` is per-connection, not per-table, so writing the
    /// usage row before reading it would hand the history entry the ledger's id
    /// — and every operation that addresses an entry by id (delete, star) would
    /// then act on the wrong row, or on nothing. Introduced and caught by
    /// reading; this is here so it cannot come back quietly.
    #[test]
    fn the_returned_id_addresses_the_history_row_not_the_ledger_row() {
        let conn = setup_conn();
        // Give the ledger a head start, so the two tables' rowids cannot
        // coincide and a mix-up would be invisible.
        for i in 0..5 {
            insert_usage(&conn, 1_788_000_000 + i, Some(1_000), None);
        }

        let usage = DictationUsage {
            duration_ms: Some(4_200),
            model_id: Some("gemini-3.5-transcribe-live".to_string()),
            engine: Some("cloud".to_string()),
            cost_usd: Some(0.00063),
        };
        let id = HistoryManager::insert_dictation_with_conn(
            &conn,
            "handy-1.wav",
            1_788_100_000,
            "Recording",
            "привет",
            false,
            None,
            None,
            &usage,
        )
        .expect("insert dictation");

        let text: String = conn
            .query_row(
                "SELECT transcription_text FROM transcription_history WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .expect("the id must address the history row");
        assert_eq!(text, "привет");

        // And the ledger got its own row for the same dictation.
        let events: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(events, 6);
    }

    /// The bug this table exists for: usage was reported `FROM
    /// transcription_history`, which `cleanup_by_count` trims to
    /// `history_limit`. A day already reported therefore shrank every time it
    /// was looked at, and eventually vanished — on a real store, 2026-08-31 went
    /// from 61 dictations / 25.6 min to 3 / 11.1 min within hours, and the three
    /// survivors were the hand-starred rows that pruning spares.
    #[test]
    fn pruning_history_does_not_change_the_usage_report() {
        let conn = setup_conn();
        for i in 0..10 {
            let ts = 1_788_000_000 + i * 60;
            insert_entry(&conn, ts, "text", None);
            insert_usage(&conn, ts, Some(30_000), Some(0.0045));
        }

        let before = HistoryManager::usage_summary_with_conn(&conn).expect("summary before");
        assert_eq!(before.dictations, 10);
        assert_eq!(before.measured, 10);
        assert!((before.seconds - 300.0).abs() < 1e-6);

        // Pruning keeps the newest two, exactly as cleanup_by_count would.
        conn.execute(
            "DELETE FROM transcription_history WHERE id NOT IN (
                 SELECT id FROM transcription_history ORDER BY timestamp DESC LIMIT 2
             )",
            [],
        )
        .expect("prune history");
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM transcription_history", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );

        let after = HistoryManager::usage_summary_with_conn(&conn).expect("summary after");
        assert_eq!(
            after.dictations, before.dictations,
            "usage must survive the history cache being trimmed"
        );
        assert!((after.seconds - before.seconds).abs() < 1e-6);
        assert!((after.cost_usd - before.cost_usd).abs() < 1e-9);
    }

    /// Entries written before usage accounting existed have no duration, and the
    /// report must read them as untimed rather than as a quiet day.
    #[test]
    fn usage_counts_untimed_events_without_inventing_duration() {
        let conn = setup_conn();
        insert_usage(&conn, 1_788_000_000, Some(12_000), Some(0.0018));
        insert_usage(&conn, 1_788_000_060, None, None);

        let summary = HistoryManager::usage_summary_with_conn(&conn).expect("summary");
        assert_eq!(summary.dictations, 2);
        assert_eq!(summary.measured, 1, "only one event carried a duration");
        assert!((summary.seconds - 12.0).abs() < 1e-6);
    }

    /// Daily buckets group by local-time day and come back oldest first.
    #[test]
    fn usage_buckets_group_by_period_and_survive_pruning() {
        let conn = setup_conn();
        // Two events a day apart; both far enough in the past to be stable.
        insert_usage(&conn, 1_788_000_000, Some(60_000), Some(0.009));
        insert_usage(&conn, 1_788_000_000 + 86_400, Some(30_000), Some(0.0045));

        let buckets =
            HistoryManager::usage_buckets_with_conn(&conn, "date(timestamp, 'unixepoch')", None)
                .expect("buckets");
        assert_eq!(buckets.len(), 2, "one bucket per day");
        assert_eq!(buckets[0].dictations, 1);
        assert!(buckets[0].period < buckets[1].period, "oldest first");
    }

    #[test]
    fn get_latest_entry_returns_none_when_empty() {
        let conn = setup_conn();
        let entry = HistoryManager::get_latest_entry_with_conn(&conn).expect("fetch latest entry");
        assert!(entry.is_none());
    }

    #[test]
    fn get_latest_entry_returns_newest_entry() {
        let conn = setup_conn();
        insert_entry(&conn, 100, "first", None);
        insert_entry(&conn, 200, "second", Some("processed"));

        let entry = HistoryManager::get_latest_entry_with_conn(&conn)
            .expect("fetch latest entry")
            .expect("entry exists");

        assert_eq!(entry.timestamp, 200);
        assert_eq!(entry.transcription_text, "second");
        assert_eq!(entry.post_processed_text.as_deref(), Some("processed"));
    }

    #[test]
    fn get_latest_completed_entry_skips_empty_entries() {
        let conn = setup_conn();
        insert_entry(&conn, 100, "completed", None);
        insert_entry(&conn, 200, "", None);

        let entry = HistoryManager::get_latest_completed_entry_with_conn(&conn)
            .expect("fetch latest completed entry")
            .expect("completed entry exists");

        assert_eq!(entry.timestamp, 100);
        assert_eq!(entry.transcription_text, "completed");
    }
}
