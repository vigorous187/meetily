use log::info;
use tauri::{AppHandle, Emitter, Manager};

use super::manager::DatabaseManager;
use super::repositories::summary::SummaryProcessesRepository;
use crate::state::AppState;

/// Initialize database on app startup
/// Handles first launch detection and conditional initialization
pub async fn initialize_database_on_startup(app: &AppHandle) -> Result<(), String> {
    // Check if this is the first launch (no database exists yet)
    let is_first_launch = DatabaseManager::is_first_launch(app)
        .await
        .map_err(|e| format!("Failed to check first launch status: {}", e))?;

    // AppState must exist before setup starts search backfill, including on a
    // brand-new install. The onboarding commands may reopen the same database,
    // but setup consumers can now safely access the managed pool immediately.
    let db_manager = DatabaseManager::new_from_app_handle(app)
        .await
        .map_err(|e| format!("Failed to initialize database manager: {}", e))?;

    if !is_first_launch {
        // Repair permissions from older builds without walking arbitrary user
        // folders. Only database-linked meeting directories below approved
        // roots are considered, and the hardener never follows symlinks.
        match sqlx::query_scalar::<_, Option<String>>(
            "SELECT folder_path FROM meetings WHERE folder_path IS NOT NULL",
        )
        .fetch_all(db_manager.pool())
        .await
        {
            Ok(folders) => {
                for folder in folders.into_iter().flatten() {
                    if let Err(error) = crate::path_security::harden_existing_meeting_storage(
                        app,
                        std::path::Path::new(&folder),
                    ) {
                        log::warn!("Could not repair one existing meeting folder: {error}");
                    }
                }
            }
            Err(error) => log::warn!("Could not enumerate existing meeting folders: {error}"),
        }
    }

    let interrupted =
        SummaryProcessesRepository::mark_abandoned_processes_interrupted(db_manager.pool())
            .await
            .map_err(|e| format!("Failed to repair abandoned summary processes: {e}"))?;
    if interrupted > 0 {
        info!("Marked {interrupted} abandoned summary generation(s) as interrupted");
    }

    app.manage(AppState { db_manager });
    info!("Database initialized successfully");

    if is_first_launch {
        info!("First launch detected - will notify window when ready");
        let app_handle = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            if let Err(error) = app_handle.emit("first-launch-detected", ()) {
                log::warn!("Failed to emit first-launch-detected event: {error}");
            }
        });
    }

    Ok(())
}
