use log::{error, info};
use serde::Serialize;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, SqliteConnection};
use std::collections::HashSet;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use tauri::{AppHandle, Emitter, Manager};

use super::manager::DatabaseManager;
use crate::state::AppState;

static APPROVED_LEGACY_DATABASES: LazyLock<Mutex<HashSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
const MAX_LEGACY_DATABASE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

async fn validate_legacy_database(path: &std::path::Path) -> Result<(PathBuf, u64), String> {
    if path.extension().and_then(|value| value.to_str()) != Some("db") {
        return Err("Legacy database must have a .db extension".to_string());
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect legacy database: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Legacy database must be a regular non-symlink file".to_string());
    }
    if metadata.len() < 100 || metadata.len() > MAX_LEGACY_DATABASE_BYTES {
        return Err("Legacy database size is outside the supported range".to_string());
    }
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("Could not verify legacy database: {error}"))?;
    let mut header = [0_u8; 16];
    std::fs::File::open(&canonical)
        .and_then(|mut file| file.read_exact(&mut header))
        .map_err(|error| format!("Could not read legacy database header: {error}"))?;
    if &header != b"SQLite format 3\0" {
        return Err("Selected file is not a SQLite database".to_string());
    }

    let options = SqliteConnectOptions::new()
        .filename(&canonical)
        .read_only(true)
        .create_if_missing(false);
    let mut connection = SqliteConnection::connect_with(&options)
        .await
        .map_err(|error| format!("Could not open legacy database safely: {error}"))?;
    let expected_tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('meetings', 'transcripts')",
    )
    .fetch_one(&mut connection)
    .await
    .map_err(|error| format!("Could not inspect legacy database schema: {error}"))?;
    if expected_tables != 2 {
        return Err("Selected database does not contain the required Meetily schema".to_string());
    }
    Ok((canonical, metadata.len()))
}

fn approve_legacy_database(path: PathBuf) {
    APPROVED_LEGACY_DATABASES.lock().unwrap().insert(path);
}

#[derive(Serialize)]
pub struct DatabaseCheckResult {
    pub exists: bool,
    pub size: u64,
}

/// Check if this is the first launch (no database exists yet)
#[tauri::command]
pub async fn check_first_launch(app: AppHandle) -> Result<bool, String> {
    DatabaseManager::is_first_launch(&app)
        .await
        .map_err(|e| format!("Failed to check first launch: {}", e))
}

/// Open a dialog to select a folder or file for legacy database import
#[tauri::command]
pub async fn select_legacy_database_path(app: AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;

    info!("Opening dialog to select legacy database location");

    let file_path = app
        .dialog()
        .file()
        .add_filter("Database Files", &["db"])
        .blocking_pick_file();

    if let Some(path) = file_path {
        let selected_path = path
            .as_path()
            .ok_or_else(|| "Selected database must be a local file".to_string())?;
        let (canonical, _) = validate_legacy_database(selected_path).await?;
        approve_legacy_database(canonical.clone());
        Ok(Some(canonical.to_string_lossy().into_owned()))
    } else {
        info!("User cancelled file selection");
        Ok(None)
    }
}

/// Detect legacy database from a selected path (root repo, backend folder, or db file)
#[tauri::command]
pub async fn detect_legacy_database(selected_path: String) -> Result<Option<String>, String> {
    let (canonical, _) = validate_legacy_database(std::path::Path::new(&selected_path)).await?;
    if !APPROVED_LEGACY_DATABASES
        .lock()
        .unwrap()
        .contains(&canonical)
    {
        return Err("Select the legacy database through Meetily's file picker first".to_string());
    }
    Ok(Some(canonical.to_string_lossy().into_owned()))
}

/// Check for legacy database in the default app data directory
#[tauri::command]
pub async fn check_default_legacy_database(app: AppHandle) -> Result<Option<String>, String> {
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to get app data dir: {}", e))?;

    let legacy_db = app_data_dir.join("meeting_minutes.db");
    if !legacy_db.exists() {
        return Ok(None);
    }
    let (canonical, _) = validate_legacy_database(&legacy_db).await?;
    approve_legacy_database(canonical.clone());
    Ok(Some(canonical.to_string_lossy().into_owned()))
}

/// Check if the Homebrew database exists and return its size
/// This is specifically for detecting old Python backend installations
#[tauri::command]
pub async fn check_homebrew_database(path: String) -> Result<Option<DatabaseCheckResult>, String> {
    const HOMEBREW_DATABASES: &[&str] = &[
        "/opt/homebrew/var/meetily/meeting_minutes.db",
        "/usr/local/var/meetily/meeting_minutes.db",
    ];
    if !HOMEBREW_DATABASES.contains(&path.as_str()) {
        return Err("Unsupported Homebrew database location".to_string());
    }
    let db_path = PathBuf::from(path);
    if !db_path.exists() {
        return Ok(None);
    }
    let (canonical, size) = validate_legacy_database(&db_path).await?;
    approve_legacy_database(canonical);
    Ok(Some(DatabaseCheckResult { exists: true, size }))
}

/// Import legacy database and initialize the database manager
#[tauri::command]
pub async fn import_and_initialize_database(
    app: AppHandle,
    legacy_db_path: String,
) -> Result<(), String> {
    let (canonical, _) = validate_legacy_database(std::path::Path::new(&legacy_db_path)).await?;
    if !APPROVED_LEGACY_DATABASES.lock().unwrap().remove(&canonical) {
        return Err("Legacy database import was not approved by Meetily's import flow".to_string());
    }

    // Import and get initialized manager
    let db_manager = DatabaseManager::import_legacy_database(&app, &canonical.to_string_lossy())
        .await
        .map_err(|e| {
            error!("Failed to import legacy database: {}", e);
            format!("Failed to import database: {}", e)
        })?;

    // Update app state with the new manager
    app.manage(AppState { db_manager });

    info!("Legacy database imported and initialized successfully");

    // Emit event to notify frontend that database is ready
    app.emit("database-initialized", ())
        .map_err(|e| format!("Failed to emit database-initialized event: {}", e))?;

    Ok(())
}

/// Initialize a fresh database (for users who don't want to import)
#[tauri::command]
pub async fn initialize_fresh_database(app: AppHandle) -> Result<(), String> {
    info!("Initializing fresh database");

    let db_manager = DatabaseManager::new_from_app_handle(&app)
        .await
        .map_err(|e| {
            error!("Failed to initialize fresh database: {}", e);
            format!("Failed to initialize database: {}", e)
        })?;

    // Update app state with the new manager
    app.manage(AppState {
        db_manager: db_manager.clone(),
    });

    // Set default model configuration for fresh installs
    let pool = db_manager.pool();

    let default_summary_model =
        crate::summary::summary_engine::commands::get_recommended_summary_model_for_current_system(
        )
        .unwrap_or("qwen3.5:2b");

    // Default Summary Model: Built-in AI (Qwen recommendation for this system)
    if let Err(e) = crate::database::repositories::setting::SettingsRepository::save_model_config(
        pool,
        "builtin-ai",
        default_summary_model,
        "large-v3", // Default whisper model (unused for builtin but required)
        None,
    )
    .await
    {
        error!("Failed to set default summary model config: {}", e);
    }

    // Default Transcription Model: Parakeet
    if let Err(e) =
        crate::database::repositories::setting::SettingsRepository::save_transcript_config(
            pool,
            "parakeet",
            crate::config::DEFAULT_PARAKEET_MODEL,
        )
        .await
    {
        error!("Failed to set default transcription model config: {}", e);
    }

    info!("Fresh database initialized successfully with default models");

    // Emit event to notify frontend that database is ready
    app.emit("database-initialized", ())
        .map_err(|e| format!("Failed to emit database-initialized event: {}", e))?;

    Ok(())
}

/// Get the database directory path
#[tauri::command]
pub async fn get_database_directory(app: AppHandle) -> Result<String, String> {
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to get app data dir: {}", e))?;

    Ok(app_data_dir.to_string_lossy().to_string())
}

/// Open the database folder in the system file explorer
#[tauri::command]
pub async fn open_database_folder(app: AppHandle) -> Result<(), String> {
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to get app data dir: {}", e))?;

    // Ensure directory exists before trying to open it
    if !app_data_dir.exists() {
        std::fs::create_dir_all(&app_data_dir)
            .map_err(|e| format!("Failed to create directory: {}", e))?;
    }

    let folder_path = app_data_dir.to_string_lossy().to_string();

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg(&folder_path)
            .spawn()
            .map_err(|e| format!("Failed to open folder: {}", e))?;
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&folder_path)
            .spawn()
            .map_err(|e| format!("Failed to open folder: {}", e))?;
    }

    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(&folder_path)
            .spawn()
            .map_err(|e| format!("Failed to open folder: {}", e))?;
    }

    info!("Opened database folder: {}", folder_path);
    Ok(())
}
