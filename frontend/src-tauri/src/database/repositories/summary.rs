use crate::database::models::SummaryProcess;
use chrono::Utc;
use serde_json::Value;
use sqlx::SqlitePool;
use tracing::{error, info as log_info};
use uuid::Uuid;

pub struct SummaryProcessesRepository;

impl SummaryProcessesRepository {
    /// Retrieves the current summary process state for a given meeting ID.
    pub async fn get_summary_data(
        pool: &SqlitePool,
        meeting_id: &str,
    ) -> Result<Option<SummaryProcess>, sqlx::Error> {
        sqlx::query_as::<_, SummaryProcess>("SELECT * FROM summary_processes WHERE meeting_id = ?")
            .bind(meeting_id)
            .fetch_optional(pool)
            .await
    }

    pub async fn update_meeting_summary(
        pool: &SqlitePool,
        meeting_id: &str,
        summary: &Value,
    ) -> Result<bool, sqlx::Error> {
        let mut transaction = pool.begin().await?;

        let meeting_exists: bool = sqlx::query("SELECT 1 FROM meetings WHERE id = ?")
            .bind(meeting_id)
            .fetch_optional(&mut *transaction)
            .await?
            .is_some();

        if !meeting_exists {
            log_info!(
                "Attempted to save summary for a non-existent meeting_id: {}",
                meeting_id
            );
            transaction.rollback().await?;
            return Ok(false);
        }

        let result_json = match serde_json::to_string(summary) {
            Ok(result) => result,
            Err(_) => {
                error!("Can't convert the json to string for saving to Database");
                transaction.rollback().await?;
                return Ok(false);
            }
        };
        let now = Utc::now();
        let generation_id = format!("manual-{}", Uuid::new_v4());

        sqlx::query(
            r#"
            INSERT INTO summary_processes (
                meeting_id, generation_id, status, created_at, updated_at,
                start_time, end_time, result, error
            )
            VALUES (?, ?, 'completed', ?, ?, ?, ?, ?, NULL)
            ON CONFLICT(meeting_id) DO UPDATE SET
                generation_id = excluded.generation_id,
                status = 'completed',
                updated_at = excluded.updated_at,
                end_time = excluded.end_time,
                result = excluded.result,
                error = NULL,
                result_backup = NULL,
                result_backup_timestamp = NULL
            "#,
        )
        .bind(meeting_id)
        .bind(generation_id)
        .bind(now)
        .bind(now)
        .bind(now)
        .bind(now)
        .bind(result_json)
        .execute(&mut *transaction)
        .await?;

        sqlx::query("UPDATE meetings SET updated_at = ? WHERE id = ?")
            .bind(now)
            .bind(meeting_id)
            .execute(&mut *transaction)
            .await?;

        transaction.commit().await?;

        log_info!(
            "Successfully updated summary and timestamp for meeting_id: {}",
            meeting_id
        );
        Ok(true)
    }

    pub async fn get_summary_data_for_meeting(
        pool: &SqlitePool,
        meeting_id: &str,
    ) -> Result<Option<SummaryProcess>, sqlx::Error> {
        sqlx::query_as::<_, SummaryProcess>("SELECT * FROM summary_processes WHERE meeting_id = ?")
            .bind(meeting_id)
            .fetch_optional(pool)
            .await
    }

    pub async fn create_or_reset_process(
        pool: &SqlitePool,
        meeting_id: &str,
        generation_id: &str,
    ) -> Result<(), sqlx::Error> {
        log_info!(
            "Creating or resetting summary process for meeting_id: {}",
            meeting_id
        );
        let now = Utc::now();
        sqlx::query(
            r#"
            INSERT INTO summary_processes (
                meeting_id, generation_id, status, created_at, updated_at,
                start_time, end_time, result, error
            )
            VALUES (?, ?, 'pending', ?, ?, ?, NULL, NULL, NULL)
            ON CONFLICT(meeting_id) DO UPDATE SET
                generation_id = excluded.generation_id,
                status = 'pending',
                updated_at = excluded.updated_at,
                start_time = excluded.start_time,
                end_time = NULL,
                chunk_count = 0,
                processing_time = 0.0,
                result_backup = result,
                result_backup_timestamp = excluded.updated_at,
                result = result,
                error = NULL
            "#,
        )
        .bind(meeting_id)
        .bind(generation_id)
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await?;
        log_info!(
            "Backed up existing summary before regeneration for meeting_id: {}",
            meeting_id
        );
        Ok(())
    }

    pub async fn update_process_completed(
        pool: &SqlitePool,
        meeting_id: &str,
        generation_id: &str,
        result: Value, // Keep this as Value to handle both old and new formats if needed
        chunk_count: i64,
        processing_time: f64,
    ) -> Result<bool, sqlx::Error> {
        let now = Utc::now();
        let result_str = serde_json::to_string(&result)
            .map_err(|e| sqlx::Error::Protocol(format!("Failed to serialize result: {}", e)))?;

        let result = sqlx::query(
            r#"
            UPDATE summary_processes
            SET status = 'completed', result = ?, updated_at = ?, end_time = ?, chunk_count = ?, processing_time = ?, error = NULL, result_backup = NULL, result_backup_timestamp = NULL
            WHERE meeting_id = ? AND generation_id = ?
              AND LOWER(status) IN ('pending', 'processing')
            "#
        )
        .bind(result_str)
        .bind(now)
        .bind(now)
        .bind(chunk_count)
        .bind(processing_time)
        .bind(meeting_id)
        .bind(generation_id)
        .execute(pool)
        .await?;
        log_info!(
            "Summary completed and backup cleared for meeting_id: {}",
            meeting_id
        );
        Ok(result.rows_affected() == 1)
    }

    pub async fn update_process_failed(
        pool: &SqlitePool,
        meeting_id: &str,
        generation_id: &str,
        error: &str,
    ) -> Result<bool, sqlx::Error> {
        let now = Utc::now();

        // Restore from backup if it exists, otherwise keep current result
        let result = sqlx::query(
            r#"
            UPDATE summary_processes
            SET
                status = 'failed',
                error = ?,
                updated_at = ?,
                end_time = ?,
                result = COALESCE(result_backup, result),
                result_backup = NULL,
                result_backup_timestamp = NULL
            WHERE meeting_id = ? AND generation_id = ?
              AND LOWER(status) IN ('pending', 'processing')
            "#,
        )
        .bind(error)
        .bind(now)
        .bind(now)
        .bind(meeting_id)
        .bind(generation_id)
        .execute(pool)
        .await?;
        log_info!(
            "Summary generation failed and backup restored for meeting_id: {}",
            meeting_id
        );
        Ok(result.rows_affected() == 1)
    }

    pub async fn update_process_cancelled(
        pool: &SqlitePool,
        meeting_id: &str,
        generation_id: &str,
    ) -> Result<bool, sqlx::Error> {
        let now = Utc::now();

        // Restore from backup if it exists, otherwise keep current result
        let result = sqlx::query(
            r#"
            UPDATE summary_processes
            SET
                status = 'cancelled',
                updated_at = ?,
                end_time = ?,
                error = 'Generation was cancelled by user',
                result = COALESCE(result_backup, result),
                result_backup = NULL,
                result_backup_timestamp = NULL
            WHERE meeting_id = ? AND generation_id = ?
              AND LOWER(status) IN ('pending', 'processing')
            "#,
        )
        .bind(now)
        .bind(now)
        .bind(meeting_id)
        .bind(generation_id)
        .execute(pool)
        .await?;
        log_info!(
            "Marked summary process as cancelled and restored backup for meeting_id: {}",
            meeting_id
        );
        Ok(result.rows_affected() == 1)
    }

    /// Restore the last completed result for work abandoned by a prior app
    /// process. The generation ID is retained so an existing poller can observe
    /// the terminal `interrupted` state after restart.
    pub async fn mark_abandoned_processes_interrupted(
        pool: &SqlitePool,
    ) -> Result<u64, sqlx::Error> {
        let now = Utc::now();
        let result = sqlx::query(
            r#"
            UPDATE summary_processes
            SET status = 'interrupted',
                error = 'Summary generation was interrupted when Meetily exited',
                updated_at = ?,
                end_time = ?,
                result = COALESCE(result_backup, result),
                result_backup = NULL,
                result_backup_timestamp = NULL
            WHERE LOWER(status) IN ('pending', 'processing')
            "#,
        )
        .bind(now)
        .bind(now)
        .execute(pool)
        .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(
            r#"
            CREATE TABLE meetings (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE summary_processes (
                meeting_id TEXT PRIMARY KEY,
                generation_id TEXT,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                error TEXT,
                result TEXT,
                start_time TEXT,
                end_time TEXT,
                chunk_count INTEGER DEFAULT 0,
                processing_time REAL DEFAULT 0.0,
                metadata TEXT,
                result_backup TEXT,
                result_backup_timestamp TEXT
            );
            INSERT INTO meetings VALUES (
                'meeting-a', 'Meeting A', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP
            );
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    #[tokio::test]
    async fn superseded_generation_cannot_complete() {
        let pool = test_pool().await;
        SummaryProcessesRepository::create_or_reset_process(&pool, "meeting-a", "gen-a")
            .await
            .unwrap();
        SummaryProcessesRepository::create_or_reset_process(&pool, "meeting-a", "gen-b")
            .await
            .unwrap();

        assert!(!SummaryProcessesRepository::update_process_completed(
            &pool,
            "meeting-a",
            "gen-a",
            serde_json::json!({"markdown": "stale"}),
            1,
            1.0,
        )
        .await
        .unwrap());
        assert!(SummaryProcessesRepository::update_process_completed(
            &pool,
            "meeting-a",
            "gen-b",
            serde_json::json!({"markdown": "current"}),
            1,
            1.0,
        )
        .await
        .unwrap());

        let row: (String, String, String) = sqlx::query_as(
            "SELECT generation_id, status, result FROM summary_processes WHERE meeting_id = ?",
        )
        .bind("meeting-a")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "gen-b");
        assert_eq!(row.1, "completed");
        assert!(row.2.contains("current"));
    }

    #[tokio::test]
    async fn manual_summary_upserts_without_transcript_chunks() {
        let pool = test_pool().await;
        assert!(SummaryProcessesRepository::update_meeting_summary(
            &pool,
            "meeting-a",
            &serde_json::json!({"markdown": "manual"}),
        )
        .await
        .unwrap());

        let row = SummaryProcessesRepository::get_summary_data_for_meeting(&pool, "meeting-a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "completed");
        assert!(row.generation_id.unwrap().starts_with("manual-"));
    }

    #[tokio::test]
    async fn startup_repair_restores_backup_and_marks_interrupted() {
        let pool = test_pool().await;
        sqlx::query(
            r#"
            INSERT INTO summary_processes (
                meeting_id, generation_id, status, created_at, updated_at,
                result, result_backup
            ) VALUES (?, ?, 'processing', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, NULL, ?)
            "#,
        )
        .bind("meeting-a")
        .bind("gen-a")
        .bind(r#"{"markdown":"previous"}"#)
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(
            SummaryProcessesRepository::mark_abandoned_processes_interrupted(&pool)
                .await
                .unwrap(),
            1
        );
        let row: (String, String, Option<String>) = sqlx::query_as(
            "SELECT status, result, result_backup FROM summary_processes WHERE meeting_id = ?",
        )
        .bind("meeting-a")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "interrupted");
        assert!(row.1.contains("previous"));
        assert!(row.2.is_none());
    }
}
