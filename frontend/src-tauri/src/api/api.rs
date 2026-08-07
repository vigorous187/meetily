use log::{debug as log_debug, error as log_error, info as log_info, warn as log_warn};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_store::StoreExt;

use crate::{
    database::{
        models::MeetingModel,
        repositories::{
            meeting::MeetingsRepository, setting::SettingsRepository,
            transcript::TranscriptsRepository,
        },
    },
    semantic_search::{
        ChunkingOptions, LocalEmbeddingProvider, ReindexOutcome, SearchDocument, SearchOptions,
        SemanticSearchService, TranscriptSourceSegment, MINILM_MODEL_DIRECTORY,
    },
    state::AppState,
    summary::CustomOpenAIConfig,
};

// Hardcoded server URL
const APP_SERVER_URL: &str = "http://localhost:5167";

pub(crate) fn validate_local_ollama_endpoint(
    endpoint: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(endpoint) = endpoint.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let parsed = url::Url::parse(endpoint)
        .map_err(|_| "Ollama endpoint must be a valid local HTTP URL".to_string())?;
    if parsed.scheme() != "http" || !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("Ollama endpoint must be an unauthenticated local HTTP URL".to_string());
    }
    let is_loopback = parsed.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    if !is_loopback {
        return Err("Ollama endpoint must use localhost or a loopback IP address".to_string());
    }
    Ok(Some(endpoint.trim_end_matches('/').to_string()))
}

#[cfg(test)]
mod local_search_api_tests {
    use super::validate_local_meeting_id;

    #[test]
    fn meeting_index_ids_are_tightly_scoped() {
        assert_eq!(
            validate_local_meeting_id("meeting-14ca249c-3e8d-4bb2").unwrap(),
            "meeting-14ca249c-3e8d-4bb2"
        );
        assert!(validate_local_meeting_id("").is_err());
        assert!(validate_local_meeting_id("note-14ca249c").is_err());
        assert!(validate_local_meeting_id("meeting-../../private").is_err());
        assert!(validate_local_meeting_id("meeting-id; DELETE FROM meetings").is_err());
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ApiResponse<T> {
    pub success: bool,
    pub data: Option<T>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Meeting {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TranscriptSearchResult {
    pub id: String,
    pub title: String,
    #[serde(rename = "matchContext")]
    pub match_context: String,
    pub timestamp: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticIndexStatus {
    pub meeting_id: String,
    pub chunk_count: usize,
    pub changed: bool,
}

const MAX_LOCAL_SEARCH_QUERY_CHARS: usize = 512;
const MAX_MEETING_ID_CHARS: usize = 128;

#[derive(Debug, Serialize, Deserialize)]
pub struct ProfileRequest {
    pub email: String,
    pub license_key: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SaveProfileRequest {
    pub id: String,
    pub email: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateProfileRequest {
    pub email: String,
    pub license_key: String,
    pub company: String,
    pub position: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ModelConfig {
    pub provider: String,
    pub model: String,
    #[serde(rename = "whisperModel")]
    pub whisper_model: String,
    #[serde(rename = "apiKey")]
    pub api_key: Option<String>,
    #[serde(rename = "ollamaEndpoint")]
    pub ollama_endpoint: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SaveModelConfigRequest {
    pub provider: String,
    pub model: String,
    #[serde(rename = "whisperModel")]
    pub whisper_model: String,
    #[serde(rename = "apiKey")]
    pub api_key: Option<String>,
    #[serde(rename = "ollamaEndpoint")]
    pub ollama_endpoint: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GetApiKeyRequest {
    pub provider: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TranscriptConfig {
    pub provider: String,
    pub model: String,
    #[serde(rename = "apiKey")]
    pub api_key: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SaveTranscriptConfigRequest {
    pub provider: String,
    pub model: String,
    #[serde(rename = "apiKey")]
    pub api_key: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeleteMeetingRequest {
    pub meeting_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MeetingDetails {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub transcripts: Vec<MeetingTranscript>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MeetingTranscript {
    pub id: String,
    pub text: String,
    pub timestamp: String,
    // Recording-relative timestamps for audio-transcript synchronization
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_start_time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_end_time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speaker_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speaker_name: Option<String>,
}

/// Meeting metadata without transcripts (for pagination)
#[derive(Debug, Serialize, Deserialize)]
pub struct MeetingMetadata {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder_path: Option<String>,
}

/// Paginated transcripts response with total count
#[derive(Debug, Serialize, Deserialize)]
pub struct PaginatedTranscriptsResponse {
    pub transcripts: Vec<MeetingTranscript>,
    pub total_count: i64,
    pub has_more: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SaveMeetingTitleRequest {
    pub meeting_id: String,
    pub title: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SaveMeetingSummaryRequest {
    pub meeting_id: String,
    pub summary: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SaveTranscriptRequest {
    pub meeting_title: String,
    pub transcripts: Vec<TranscriptSegment>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub id: String,
    pub text: String,
    pub timestamp: String,
    // NEW: Recording-relative timestamps for playback synchronization
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_start_time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_end_time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    #[serde(default = "default_transcript_source")]
    pub source: String,
    #[serde(default)]
    pub speaker_id: Option<String>,
}

fn default_transcript_source() -> String {
    "unknown".to_string()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: Option<String>,
    pub email: String,
    pub license_key: String,
    pub company: Option<String>,
    pub position: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub is_licensed: bool,
}

// Helper function to get auth token from store (optional)
#[allow(dead_code)]
async fn get_auth_token<R: Runtime>(app: &AppHandle<R>) -> Option<String> {
    let store = match app.store("store.json") {
        Ok(store) => store,
        Err(_) => return None,
    };

    match store.get("authToken") {
        Some(token) => {
            if let Some(token_str) = token.as_str() {
                log_info!("Found auth token");
                Some(token_str.to_string())
            } else {
                log_warn!("Auth token is not a string");
                None
            }
        }
        None => {
            log_warn!("No auth token found in store");
            None
        }
    }
}

// Helper function to get server address - now hardcoded
async fn get_server_address<R: Runtime>(_app: &AppHandle<R>) -> Result<String, String> {
    log_info!("Using hardcoded server URL: {}", APP_SERVER_URL);
    Ok(APP_SERVER_URL.to_string())
}

// Generic API call function with optional authentication
async fn make_api_request<R: Runtime, T: for<'de> Deserialize<'de>>(
    app: &AppHandle<R>,
    endpoint: &str,
    method: &str,
    body: Option<&str>,
    additional_headers: Option<HashMap<String, String>>,
    auth_token: Option<String>, // Pass auth token from frontend
) -> Result<T, String> {
    let client = reqwest::Client::new();
    let server_url = get_server_address(app).await?;

    let url = format!("{}{}", server_url, endpoint);
    log_info!("Making {} request to: {}", method, url);

    let mut request = match method.to_uppercase().as_str() {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
        "PUT" => client.put(&url),
        "DELETE" => client.delete(&url),
        _ => return Err(format!("Unsupported HTTP method: {}", method)),
    };

    // Add authorization header if auth token is provided
    if let Some(token) = auth_token {
        log_info!("Adding authorization header");
        request = request.header("Authorization", format!("Bearer {}", token));
    } else {
        log_warn!("No auth token provided, making unauthenticated request");
    }

    request = request.header("Content-Type", "application/json");

    // Add additional headers if provided
    if let Some(headers) = additional_headers {
        for (key, value) in headers {
            request = request.header(&key, &value);
        }
    }

    // Add body if provided
    if let Some(body_str) = body {
        request = request.body(body_str.to_string());
    }

    let response = request.send().await.map_err(|e| {
        let error_msg = format!("Request failed: {}", e);
        log_error!("{}", error_msg);
        error_msg
    })?;

    let status = response.status();
    log_info!("Response status: {}", status);

    if !status.is_success() {
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        log_error!("API request failed with HTTP status {}", status);
        return Err(format!("HTTP {}: {}", status, error_text));
    }

    let response_text = response.text().await.map_err(|e| {
        let error_msg = format!("Failed to read response: {}", e);
        log_error!("{}", error_msg);
        error_msg
    })?;

    serde_json::from_str(&response_text).map_err(|e| {
        let error_msg = format!("Failed to parse JSON: {}", e);
        log_error!("{}", error_msg);
        error_msg
    })
}

// API Commands for Tauri

#[tauri::command]
pub async fn api_get_meetings<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    auth_token: Option<String>,
) -> Result<Vec<Meeting>, String> {
    log_info!(
        "api_get_meetings called with auth_token(native) : {}",
        auth_token.is_some()
    );
    let pool = state.db_manager.pool();
    let meetings: Result<Vec<MeetingModel>, sqlx::Error> =
        MeetingsRepository::get_meetings(pool).await;

    match meetings {
        Ok(meeting_models) => {
            log_info!("Successfully got {} meetings", meeting_models.len());

            let result: Vec<Meeting> = meeting_models
                .into_iter()
                .map(|m| Meeting {
                    id: m.id,
                    title: m.title,
                })
                .collect();
            Ok(result)
        }
        Err(e) => {
            log_error!("Error getting meetings: {}", e);
            Err(e.to_string())
        }
    }
}

#[tauri::command]
pub async fn api_search_transcripts<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    query: String,
    auth_token: Option<String>,
) -> Result<Vec<TranscriptSearchResult>, String> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    if query.chars().count() > MAX_LOCAL_SEARCH_QUERY_CHARS || query.contains('\0') {
        return Err(format!(
            "Search query must be between 1 and {MAX_LOCAL_SEARCH_QUERY_CHARS} characters."
        ));
    }

    log_info!(
        "api_search_transcripts called with {} query characters, auth_token: {}",
        query.chars().count(),
        auth_token.is_some()
    );

    let pool = state.db_manager.pool();

    // Keep the local index current. Reindexing is content-hash based, so unchanged
    // meetings do not repeat embedding work or database writes.
    let provider = local_search_provider(&app)?;
    if let Err(error) = ensure_local_search_index(pool, &provider).await {
        log_warn!(
            "Local semantic index refresh failed; using keyword fallback: {}",
            error
        );
    }

    let semantic_results = SemanticSearchService::new(pool, &provider, ChunkingOptions::default())
        .map_err(|error| error.to_string())?
        .search(query, SearchOptions::default())
        .await;

    if let Ok(results) = semantic_results {
        if !results.is_empty() {
            return Ok(results
                .into_iter()
                .map(|result| TranscriptSearchResult {
                    id: result.meeting_id,
                    title: result.title,
                    match_context: result.snippet,
                    timestamp: result
                        .audio_timestamp
                        .map(|seconds| format!("{seconds:.3}"))
                        .unwrap_or_default(),
                })
                .collect());
        }
    }

    match TranscriptsRepository::search_transcripts(pool, query).await {
        Ok(results) => {
            log_info!(
                "Search completed successfully with {} results.",
                results.len()
            );
            Ok(results)
        }
        Err(e) => {
            log_error!("Local transcript search failed: {}", e);
            Err(format!("Failed to search transcripts: {}", e))
        }
    }
}

/// Rebuilds one meeting's private on-device search document. The meeting ID is
/// constrained before it reaches SQLite, and all transcript/model data stays
/// inside the local application data directory.
#[tauri::command]
pub async fn api_index_meeting_for_search<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    meeting_id: String,
) -> Result<SemanticIndexStatus, String> {
    let meeting_id = validate_local_meeting_id(&meeting_id)?;
    let provider = local_search_provider(&app)?;
    index_local_meeting(state.db_manager.pool(), &provider, meeting_id)
        .await
        .map_err(|error| format!("Unable to update the local meeting search index: {error}"))
}

fn validate_local_meeting_id(meeting_id: &str) -> Result<&str, String> {
    let meeting_id = meeting_id.trim();
    if meeting_id.is_empty()
        || meeting_id.chars().count() > MAX_MEETING_ID_CHARS
        || !meeting_id.starts_with("meeting-")
        || !meeting_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err("Invalid local meeting identifier.".to_string());
    }
    Ok(meeting_id)
}

fn local_search_provider<R: Runtime>(
    app: &AppHandle<R>,
) -> Result<LocalEmbeddingProvider, String> {
    let model_directory = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("Unable to locate the local app data directory: {error}"))?
        .join(MINILM_MODEL_DIRECTORY);
    let (provider, model_error) = LocalEmbeddingProvider::verified_or_keyword(&model_directory);
    if let Some(error) = model_error {
        log_warn!(
            "Verified MiniLM model unavailable; using local FTS keyword search: {}",
            error
        );
    }
    Ok(provider)
}

async fn load_local_search_document(
    pool: &sqlx::SqlitePool,
    meeting_id: &str,
) -> anyhow::Result<SearchDocument> {
    let title = sqlx::query_scalar::<_, String>("SELECT title FROM meetings WHERE id = ?")
        .bind(meeting_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("meeting was not found"))?;

    #[derive(sqlx::FromRow)]
    struct SegmentRow {
        text: String,
        audio_start_time: Option<f64>,
        audio_end_time: Option<f64>,
    }

    let segments = sqlx::query_as::<_, SegmentRow>(
        "SELECT transcript AS text, audio_start_time, audio_end_time
         FROM transcripts
         WHERE meeting_id = ?
         ORDER BY audio_start_time, timestamp, id",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| TranscriptSourceSegment {
        text: row.text,
        audio_start_time: row.audio_start_time,
        audio_end_time: row.audio_end_time,
    })
    .collect();

    Ok(SearchDocument {
        meeting_id: meeting_id.to_owned(),
        title,
        segments,
    })
}

async fn index_local_meeting<P: crate::semantic_search::EmbeddingProvider>(
    pool: &sqlx::SqlitePool,
    provider: &P,
    meeting_id: &str,
) -> anyhow::Result<SemanticIndexStatus> {
    let document = load_local_search_document(pool, meeting_id).await?;
    let service = SemanticSearchService::new(pool, provider, ChunkingOptions::default())?;
    let outcome = service.reindex(&document).await?;
    let (chunk_count, changed) = match outcome {
        ReindexOutcome::Indexed { chunk_count } => (chunk_count, true),
        ReindexOutcome::Unchanged => {
            let chunk_count = sqlx::query_scalar::<_, i64>(
                "SELECT chunk_count FROM semantic_search_documents WHERE meeting_id = ?",
            )
            .bind(meeting_id)
            .fetch_optional(pool)
            .await?
            .unwrap_or(0)
            .max(0) as usize;
            (chunk_count, false)
        }
    };
    Ok(SemanticIndexStatus {
        meeting_id: meeting_id.to_owned(),
        chunk_count,
        changed,
    })
}

async fn ensure_local_search_index<P: crate::semantic_search::EmbeddingProvider>(
    pool: &sqlx::SqlitePool,
    provider: &P,
) -> anyhow::Result<()> {
    #[derive(sqlx::FromRow)]
    struct SearchRow {
        meeting_id: String,
        title: String,
        text: String,
        audio_start_time: Option<f64>,
        audio_end_time: Option<f64>,
    }

    let rows = sqlx::query_as::<_, SearchRow>(
        "SELECT m.id AS meeting_id, m.title, t.transcript AS text,
                t.audio_start_time, t.audio_end_time
         FROM meetings m
         JOIN transcripts t ON t.meeting_id = m.id
         ORDER BY m.id, t.audio_start_time, t.timestamp",
    )
    .fetch_all(pool)
    .await?;

    let mut documents: HashMap<String, SearchDocument> = HashMap::new();
    for row in rows {
        let document = documents
            .entry(row.meeting_id.clone())
            .or_insert_with(|| SearchDocument {
                meeting_id: row.meeting_id,
                title: row.title,
                segments: Vec::new(),
            });
        document.segments.push(TranscriptSourceSegment {
            text: row.text,
            audio_start_time: row.audio_start_time,
            audio_end_time: row.audio_end_time,
        });
    }

    let service = SemanticSearchService::new(pool, provider, ChunkingOptions::default())?;
    for document in documents.values() {
        service.reindex(document).await?;
    }
    Ok(())
}

pub(crate) async fn backfill_local_search_index<R: Runtime>(
    app: &AppHandle<R>,
    pool: &sqlx::SqlitePool,
) -> anyhow::Result<()> {
    let provider = local_search_provider(app).map_err(anyhow::Error::msg)?;
    ensure_local_search_index(pool, &provider).await
}

#[tauri::command]
pub async fn api_get_profile<R: Runtime>(
    app: AppHandle<R>,
    email: String,
    license_key: String,
    auth_token: Option<String>,
) -> Result<Profile, String> {
    log_info!(
        "api_get_profile called, auth_token: {}",
        auth_token.is_some()
    );

    let profile_request = ProfileRequest { email, license_key };
    let body = serde_json::to_string(&profile_request).map_err(|e| e.to_string())?;

    make_api_request::<R, Profile>(&app, "/get-profile", "POST", Some(&body), None, auth_token)
        .await
}

#[tauri::command]
pub async fn api_save_profile<R: Runtime>(
    app: AppHandle<R>,
    id: String,
    email: String,
    auth_token: Option<String>,
) -> Result<serde_json::Value, String> {
    log_info!(
        "api_save_profile called, auth_token: {}",
        auth_token.is_some()
    );

    let save_request = SaveProfileRequest { id, email };
    let body = serde_json::to_string(&save_request).map_err(|e| e.to_string())?;

    make_api_request::<R, serde_json::Value>(
        &app,
        "/save-profile",
        "POST",
        Some(&body),
        None,
        auth_token,
    )
    .await
}

#[tauri::command]
pub async fn api_update_profile<R: Runtime>(
    app: AppHandle<R>,
    email: String,
    license_key: String,
    company: String,
    position: String,
    auth_token: Option<String>,
) -> Result<serde_json::Value, String> {
    log_info!(
        "api_update_profile called, auth_token: {}",
        auth_token.is_some()
    );

    let update_request = UpdateProfileRequest {
        email,
        license_key,
        company,
        position,
    };
    let body = serde_json::to_string(&update_request).map_err(|e| e.to_string())?;

    make_api_request::<R, serde_json::Value>(
        &app,
        "/update-profile",
        "POST",
        Some(&body),
        None,
        auth_token,
    )
    .await
}

#[tauri::command]
pub async fn api_get_model_config<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    _auth_token: Option<String>,
) -> Result<Option<ModelConfig>, String> {
    let pool = state.db_manager.pool();

    match SettingsRepository::get_model_config(pool).await {
        Ok(Some(config)) => {
            if !matches!(config.provider.as_str(), "builtin-ai" | "ollama") {
                return Err("Cloud summary providers are disabled in this local-only build".to_string());
            }
            let ollama_endpoint = if config.provider == "ollama" {
                validate_local_ollama_endpoint(config.ollama_endpoint.as_deref())?
            } else {
                None
            };
            Ok(Some(ModelConfig {
                provider: config.provider,
                model: config.model,
                whisper_model: config.whisper_model,
                api_key: None,
                ollama_endpoint,
            }))
        }
        Ok(None) => {
            log_warn!("⚠️ No model config found in database - database may be empty or settings table not initialized");
            Ok(None)
        }
        Err(e) => {
            log_error!("❌ Failed to get model config from database: {}", e);
            Err(e.to_string())
        }
    }
}

#[tauri::command]
pub async fn api_save_model_config<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    provider: String,
    model: String,
    whisper_model: String,
    api_key: Option<String>,
    ollama_endpoint: Option<String>,
    _auth_token: Option<String>,
) -> Result<serde_json::Value, String> {
    let pool = state.db_manager.pool();

    if !matches!(provider.as_str(), "builtin-ai" | "ollama") {
        return Err("Cloud summary providers are disabled in this local-only build".to_string());
    }
    if api_key.as_deref().is_some_and(|key| !key.trim().is_empty()) {
        return Err("API keys are disabled in this local-only build".to_string());
    }
    let ollama_endpoint = if provider == "ollama" {
        validate_local_ollama_endpoint(ollama_endpoint.as_deref())?
    } else {
        None
    };

    if let Err(e) = SettingsRepository::save_model_config(
        pool,
        &provider,
        &model,
        &whisper_model,
        ollama_endpoint.as_deref(),
    )
    .await
    {
        log_error!("❌ Failed to save model config to database: {}", e);
        return Err(e.to_string());
    }

    // Trigger graceful shutdown of built-in AI sidecar if it's running
    // This ensures that if the user switched models/providers, the old one is cleaned up
    // The shutdown happens in the background, so it won't block the UI
    if let Err(e) = crate::summary::summary_engine::client::shutdown_sidecar_gracefully().await {
        log_warn!("Failed to initiate graceful sidecar shutdown: {}", e);
    }

    Ok(
        serde_json::json!({ "status": "success", "message": "Model configuration saved successfully" }),
    )
}

#[tauri::command]
pub async fn api_get_api_key<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    provider: String,
    _auth_token: Option<String>,
) -> Result<String, String> {
    match SettingsRepository::get_api_key(&state.db_manager.pool(), &provider).await {
        Ok(key) => Ok(key.unwrap_or_default()),
        Err(e) => {
            log_error!("Failed to get API key for provider '{}': {}", &provider, e);
            Err(e.to_string())
        }
    }
}

#[tauri::command]
pub async fn api_get_transcript_config<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    _auth_token: Option<String>,
) -> Result<Option<TranscriptConfig>, String> {
    let pool = state.db_manager.pool();

    match SettingsRepository::get_transcript_config(pool).await {
        Ok(Some(config)) => {
            if !matches!(config.provider.as_str(), "localWhisper" | "parakeet") {
                return Err("Cloud transcription providers are disabled in this local-only build".to_string());
            }
            Ok(Some(TranscriptConfig {
                provider: config.provider,
                model: config.model,
                api_key: None,
            }))
        }
        Ok(None) => {
            log_info!("No transcript config found, returning default.");
            Ok(Some(TranscriptConfig {
                provider: "parakeet".to_string(),
                model: crate::config::DEFAULT_PARAKEET_MODEL.to_string(),
                api_key: None,
            }))
        }
        Err(e) => {
            log_error!("Failed to get transcript config: {}", e);
            Err(e.to_string())
        }
    }
}

#[tauri::command]
pub async fn api_save_transcript_config<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    provider: String,
    model: String,
    api_key: Option<String>,
    _auth_token: Option<String>,
) -> Result<serde_json::Value, String> {
    let pool = state.db_manager.pool();

    if !matches!(provider.as_str(), "localWhisper" | "parakeet") {
        return Err("Cloud transcription providers are disabled in this local-only build".to_string());
    }
    if api_key.as_deref().is_some_and(|key| !key.trim().is_empty()) {
        return Err("API keys are disabled in this local-only build".to_string());
    }

    if let Err(e) = SettingsRepository::save_transcript_config(pool, &provider, &model).await {
        log_error!("Failed to save transcript config: {}", e);
        return Err(e.to_string());
    }

    Ok(
        serde_json::json!({ "status": "success", "message": "Transcript configuration saved successfully" }),
    )
}

#[tauri::command]
pub async fn api_get_transcript_api_key<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    provider: String,
    _auth_token: Option<String>,
) -> Result<String, String> {
    match SettingsRepository::get_transcript_api_key(&state.db_manager.pool(), &provider).await {
        Ok(key) => Ok(key.unwrap_or_default()),
        Err(e) => {
            log_error!(
                "Failed to get transcript API key for provider '{}': {}",
                &provider,
                e
            );
            Err(e.to_string())
        }
    }
}

#[tauri::command]
pub async fn api_delete_api_key<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    provider: String,
    _auth_token: Option<String>,
) -> Result<(), String> {
    match SettingsRepository::delete_api_key(&state.db_manager.pool(), &provider).await {
        Ok(_) => Ok(()),
        Err(e) => {
            log_error!(
                "Failed to delete API key for provider '{}': {}",
                &provider,
                e
            );
            Err(e.to_string())
        }
    }
}

#[tauri::command]
pub async fn api_delete_meeting<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    meeting_id: String,
    auth_token: Option<String>,
) -> Result<serde_json::Value, String> {
    log_info!(
        "api_delete_meeting called for meeting_id(native): {}, auth_token: {}",
        meeting_id,
        auth_token.is_some()
    );

    let pool = state.db_manager.pool();

    match MeetingsRepository::delete_meeting(pool, &meeting_id).await {
        Ok(true) => {
            log_info!("Successfully deleted meeting {}", meeting_id);
            Ok(serde_json::json!({
                "status": "success",
                "message": "Meeting deleted successfully"
            }))
        }
        Ok(false) => {
            log_warn!("Meeting not found or already deleted: {}", meeting_id);
            Err(format!(
                "Meeting not found or could not be deleted: {}",
                meeting_id
            ))
        }
        Err(e) => {
            log_error!("Error deleting meeting {}: {}", meeting_id, e);
            Err(format!("Failed to delete meeting: {}", e))
        }
    }
}

#[tauri::command]
pub async fn api_get_meeting<R: Runtime>(
    _app: AppHandle<R>,
    meeting_id: String,
    state: tauri::State<'_, AppState>,
    auth_token: Option<String>,
) -> Result<MeetingDetails, String> {
    log_info!(
        "api_get_meeting called(native) for meeting_id: {}, auth_token: {}",
        meeting_id,
        auth_token.is_some()
    );

    let pool = state.db_manager.pool();

    match MeetingsRepository::get_meeting(pool, &meeting_id).await {
        Ok(Some(meeting)) => {
            log_info!("Successfully retrieved meeting {}", meeting_id);
            Ok(meeting)
        }
        Ok(None) => {
            log_warn!("Meeting not found: {}", meeting_id);
            Err(format!("Meeting not found: {}", meeting_id))
        }
        Err(e) => {
            log_error!("Error retrieving meeting {}: {}", meeting_id, e);
            Err(format!("Failed to retrieve meeting: {}", e))
        }
    }
}

/// Get meeting metadata without transcripts (for pagination)
#[tauri::command]
pub async fn api_get_meeting_metadata<R: Runtime>(
    _app: AppHandle<R>,
    meeting_id: String,
    state: tauri::State<'_, AppState>,
) -> Result<MeetingMetadata, String> {
    log_info!(
        "api_get_meeting_metadata called for meeting_id: {}",
        meeting_id
    );

    let pool = state.db_manager.pool();

    match MeetingsRepository::get_meeting_metadata(pool, &meeting_id).await {
        Ok(Some(meeting)) => {
            log_info!("Successfully retrieved meeting metadata {}", meeting_id);
            Ok(MeetingMetadata {
                id: meeting.id,
                title: meeting.title,
                created_at: meeting.created_at.0.to_rfc3339(),
                updated_at: meeting.updated_at.0.to_rfc3339(),
                folder_path: meeting.folder_path,
            })
        }
        Ok(None) => {
            log_warn!("Meeting not found: {}", meeting_id);
            Err(format!("Meeting not found: {}", meeting_id))
        }
        Err(e) => {
            log_error!("Error retrieving meeting metadata {}: {}", meeting_id, e);
            Err(format!("Failed to retrieve meeting metadata: {}", e))
        }
    }
}

/// Get paginated transcripts for a meeting
#[tauri::command]
pub async fn api_get_meeting_transcripts<R: Runtime>(
    _app: AppHandle<R>,
    meeting_id: String,
    limit: i64,
    offset: i64,
    state: tauri::State<'_, AppState>,
) -> Result<PaginatedTranscriptsResponse, String> {
    log_info!(
        "api_get_meeting_transcripts called for meeting_id: {}, limit: {}, offset: {}",
        meeting_id,
        limit,
        offset
    );

    let pool = state.db_manager.pool();

    match MeetingsRepository::get_meeting_transcripts_paginated(pool, &meeting_id, limit, offset)
        .await
    {
        Ok((transcripts, total_count)) => {
            log_info!(
                "Successfully retrieved {} transcripts for meeting {} (total: {})",
                transcripts.len(),
                meeting_id,
                total_count
            );

            // Convert Transcript to MeetingTranscript
            let meeting_transcripts = transcripts
                .into_iter()
                .map(|t| MeetingTranscript {
                    id: t.id,
                    text: t.transcript,
                    timestamp: t.timestamp,
                    audio_start_time: t.audio_start_time,
                    audio_end_time: t.audio_end_time,
                    duration: t.duration,
                    speaker_name: t
                        .speaker_name
                        .or_else(|| default_speaker_name(&t.source, t.speaker_id.as_deref())),
                    source: t.source,
                    speaker_id: t.speaker_id,
                })
                .collect::<Vec<_>>();

            let has_more = (offset + meeting_transcripts.len() as i64) < total_count;

            Ok(PaginatedTranscriptsResponse {
                transcripts: meeting_transcripts,
                total_count,
                has_more,
            })
        }
        Err(e) => {
            log_error!(
                "Error retrieving transcripts for meeting {}: {}",
                meeting_id,
                e
            );
            Err(format!("Failed to retrieve transcripts: {}", e))
        }
    }
}

fn default_speaker_name(source: &str, speaker_id: Option<&str>) -> Option<String> {
    match speaker_id {
        Some("you") => Some("You".to_string()),
        Some("remote") => Some("Remote speaker".to_string()),
        Some(id) if id.starts_with("speaker-") => Some(id.replace('-', " ")),
        _ if source == "mic" => Some("You".to_string()),
        _ if source == "system" => Some("Remote speaker".to_string()),
        _ => None,
    }
}

#[tauri::command]
pub async fn api_save_meeting_title<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    meeting_id: String,
    title: String,
    auth_token: Option<String>,
) -> Result<serde_json::Value, String> {
    log_info!(
        "api_save_meeting_title called for meeting_id: {}, auth_token: {}",
        meeting_id,
        auth_token.is_some()
    );
    let pool = state.db_manager.pool();
    match MeetingsRepository::update_meeting_title(pool, &meeting_id, &title).await {
        Ok(true) => {
            if let Ok(provider) = local_search_provider(&app) {
                if let Err(error) = index_local_meeting(pool, &provider, &meeting_id).await {
                    log_warn!("Unable to refresh local search after title update: {}", error);
                }
            }
            log_info!("Successfully saved meeting title");
            Ok(serde_json::json!({"message": "Meeting title saved successfully"}))
        }
        Ok(false) => {
            log_error!("No meeting found with id {}", meeting_id);
            Err(format!("No meeting found with id {}", meeting_id))
        }
        Err(e) => {
            log_error!("Failed to update meeting {}", e);
            Err(format!("Failed to update meeting: {}", e))
        }
    }
}

#[tauri::command]
pub async fn api_rename_meeting_speaker<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    meeting_id: String,
    speaker_id: String,
    display_name: String,
) -> Result<(), String> {
    let display_name = display_name.trim();
    if meeting_id.trim().is_empty() || speaker_id.trim().is_empty() {
        return Err("Meeting and speaker IDs are required.".to_string());
    }
    if display_name.is_empty() || display_name.chars().count() > 80 {
        return Err("Speaker name must be between 1 and 80 characters.".to_string());
    }
    let result = sqlx::query(
        "UPDATE meeting_speakers
         SET display_name = ?, updated_at = CURRENT_TIMESTAMP
         WHERE meeting_id = ? AND speaker_id = ?",
    )
    .bind(display_name)
    .bind(&meeting_id)
    .bind(&speaker_id)
    .execute(state.db_manager.pool())
    .await
    .map_err(|error| error.to_string())?;
    if result.rows_affected() == 0 {
        return Err("Speaker was not found for this meeting.".to_string());
    }
    Ok(())
}

#[tauri::command]
pub async fn api_save_transcript<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    meeting_title: String,
    transcripts: Vec<serde_json::Value>,
    folder_path: Option<String>,
    auth_token: Option<String>,
) -> Result<serde_json::Value, String> {
    log_info!(
        "api_save_transcript called with {} transcripts, folder present: {}, auth_token: {}",
        transcripts.len(),
        folder_path.is_some(),
        auth_token.is_some()
    );

    let folder_path = match folder_path {
        Some(path) => Some(
            crate::path_security::validate_existing_approved_directory(
                &app,
                std::path::Path::new(&path),
            )?
            .to_string_lossy()
            .into_owned(),
        ),
        None => None,
    };

    // Convert serde_json::Value to TranscriptSegment
    let transcripts_to_save: Vec<TranscriptSegment> = transcripts
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            log_error!("Failed to parse transcript segments: {}", e);
            format!(
                "Invalid transcript data format: {}. Please check the data structure.",
                e
            )
        })?;

    let pool = state.db_manager.pool();
    let diarization_ranges = crate::diarization::runtime::pending_for(folder_path.as_deref());

    // Now, call the repository with the correctly typed data.
    match TranscriptsRepository::save_transcript(
        pool,
        &meeting_title,
        &transcripts_to_save,
        folder_path.clone(),
        &diarization_ranges,
    )
    .await
    {
        Ok(meeting_id) => {
            crate::diarization::runtime::discard_pending(folder_path.as_deref());
            log_info!(
                "Successfully saved transcript and created meeting with id: {}",
                meeting_id
            );

            // Index only after the transcript transaction commits. Search index
            // failures never roll back or lose a successfully saved meeting.
            if let Ok(provider) = local_search_provider(&app) {
                if let Err(error) = index_local_meeting(pool, &provider, &meeting_id).await {
                    log_warn!("Unable to index newly saved meeting locally: {}", error);
                }
            }
            Ok(serde_json::json!({
                "status": "success",
                "message": "Transcript saved successfully",
                "meeting_id": meeting_id
            }))
        }
        Err(e) => {
            log_error!("Error saving transcript: {}", e);
            Err(format!("Failed to save transcript: {}", e))
        }
    }
}

/// Opens the meeting's recording folder in the system file explorer
#[tauri::command]
pub async fn open_meeting_folder<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    meeting_id: String,
) -> Result<(), String> {
    let pool = state.db_manager.pool();

    // Get meeting with folder_path
    let meeting: Option<MeetingModel> = sqlx::query_as(
        "SELECT id, title, created_at, updated_at, folder_path FROM meetings WHERE id = ?",
    )
    .bind(&meeting_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("Database error: {}", e))?;

    match meeting {
        Some(m) => {
            if let Some(folder_path) = m.folder_path {
                let folder_path = crate::path_security::validate_existing_approved_directory(
                    &app,
                    std::path::Path::new(&folder_path),
                )?;

                // Open folder based on OS
                #[cfg(target_os = "macos")]
                {
                    std::process::Command::new("open")
                        .arg("--")
                        .arg(&folder_path)
                        .spawn()
                        .map_err(|e| format!("Failed to open folder: {}", e))?;
                }

                #[cfg(target_os = "windows")]
                {
                    std::process::Command::new("explorer")
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

                Ok(())
            } else {
                log_warn!("Meeting {} has no folder_path set", meeting_id);
                Err("Recording folder path not available for this meeting".to_string())
            }
        }
        None => {
            log_warn!("Meeting not found: {}", meeting_id);
            Err("Meeting not found".to_string())
        }
    }
}

// Simple test command to check backend connectivity
#[tauri::command]
pub async fn test_backend_connection<R: Runtime>(
    app: AppHandle<R>,
    auth_token: Option<String>,
) -> Result<String, String> {
    log_debug!("Testing backend connection...");

    let client = reqwest::Client::new();
    let server_url = get_server_address(&app).await?;

    let mut request = client.get(&format!("{}/docs", server_url));

    if let Some(token) = auth_token {
        request = request.header("Authorization", format!("Bearer {}", token));
    }

    match request.send().await {
        Ok(response) => {
            let status = response.status();
            log_debug!("Backend responded with status: {}", status);
            Ok(format!("Backend is reachable. Status: {}", status))
        }
        Err(e) => {
            let error_msg = format!("Failed to connect to backend: {}", e);
            log_debug!("{}", error_msg);
            Err(error_msg)
        }
    }
}

#[tauri::command]
pub async fn debug_backend_connection<R: Runtime>(app: AppHandle<R>) -> Result<String, String> {
    log_debug!("=== DEBUG: Testing backend connection ===");

    // Test 1: Check server address from store
    let server_url = match get_server_address(&app).await {
        Ok(url) => {
            log_debug!("✓ Server URL from store: {}", url);
            url
        }
        Err(e) => {
            log_error!("✗ Failed to get server URL: {}", e);
            return Err(format!("Failed to get server URL: {}", e));
        }
    };

    // Test 2: Make a simple HTTP request to the backend
    let client = reqwest::Client::new();
    let test_url = format!("{}/docs", server_url); // Try the docs endpoint which should be public

    log_debug!("Testing connection to: {}", test_url);

    match client.get(&test_url).send().await {
        Ok(response) => {
            let status = response.status();
            log_debug!("✓ Backend responded with status: {}", status);
            Ok(format!(
                "Backend connection successful! Status: {}, URL: {}",
                status, server_url
            ))
        }
        Err(e) => {
            log_error!("✗ Backend connection failed: {}", e);
            Err(format!("Backend connection failed: {}", e))
        }
    }
}

#[tauri::command]
pub async fn open_external_url(url: String) -> Result<(), String> {
    use std::process::Command;

    let parsed = url::Url::parse(&url).map_err(|_| "Invalid external URL".to_string())?;
    if parsed.scheme() != "https" {
        return Err("Only HTTPS external URLs are allowed".to_string());
    }

    let allowed_hosts = ["github.com", "meetily.zackriya.com", "ollama.com"];
    let host = parsed
        .host_str()
        .ok_or_else(|| "External URL has no host".to_string())?;
    if !allowed_hosts.contains(&host) {
        return Err(format!("External host is not allowed: {host}"));
    }

    let result = if cfg!(target_os = "windows") {
        // Avoid cmd.exe so URL metacharacters can never be interpreted as shell syntax.
        Command::new("explorer.exe").arg(&url).output()
    } else if cfg!(target_os = "macos") {
        Command::new("open").arg("--").arg(&url).output()
    } else {
        // Linux and other Unix-like systems
        Command::new("xdg-open").arg(&url).output()
    };

    match result {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("Failed to open URL: {}", e)),
    }
}

// ===== CUSTOM OPENAI API COMMANDS =====

/// Saves the custom OpenAI configuration
/// This configuration is stored as JSON and includes endpoint, apiKey, model, and optional parameters
#[tauri::command]
pub async fn api_save_custom_openai_config<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    endpoint: String,
    api_key: Option<String>,
    model: String,
    max_tokens: Option<i32>,
    temperature: Option<f32>,
    top_p: Option<f32>,
) -> Result<serde_json::Value, String> {
    log_info!(
        "api_save_custom_openai_config called: endpoint='{}', model='{}'",
        &endpoint,
        &model
    );

    // Validate required fields
    if endpoint.trim().is_empty() {
        return Err("Endpoint URL is required".to_string());
    }
    if model.trim().is_empty() {
        return Err("Model name is required".to_string());
    }

    // Validate endpoint URL format
    if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
        return Err("Endpoint must start with http:// or https://".to_string());
    }

    // Validate optional numeric parameters
    if let Some(temp) = temperature {
        if !(0.0..=2.0).contains(&temp) {
            return Err("Temperature must be between 0.0 and 2.0".to_string());
        }
    }
    if let Some(top) = top_p {
        if !(0.0..=1.0).contains(&top) {
            return Err("Top P must be between 0.0 and 1.0".to_string());
        }
    }
    if let Some(tokens) = max_tokens {
        if tokens < 1 {
            return Err("Max tokens must be at least 1".to_string());
        }
    }

    let config = CustomOpenAIConfig {
        endpoint: endpoint.trim().to_string(),
        api_key: api_key.filter(|k| !k.trim().is_empty()),
        model: model.trim().to_string(),
        max_tokens,
        temperature,
        top_p,
    };

    let pool = state.db_manager.pool();

    match SettingsRepository::save_custom_openai_config(pool, &config).await {
        Ok(()) => Ok(serde_json::json!({
            "status": "success",
            "message": "Custom OpenAI configuration saved successfully"
        })),
        Err(e) => {
            log_error!("❌ Failed to save custom OpenAI config: {}", e);
            Err(format!("Failed to save custom OpenAI configuration: {}", e))
        }
    }
}

/// Gets the custom OpenAI configuration
#[tauri::command]
pub async fn api_get_custom_openai_config<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
) -> Result<Option<CustomOpenAIConfig>, String> {
    let pool = state.db_manager.pool();

    match SettingsRepository::get_custom_openai_config(pool).await {
        Ok(config) => Ok(config),
        Err(e) => {
            log_error!("❌ Failed to get custom OpenAI config: {}", e);
            Err(format!("Failed to get custom OpenAI configuration: {}", e))
        }
    }
}

/// Tests the connection to a custom OpenAI-compatible endpoint
/// Makes a minimal request to verify the endpoint is reachable and responds correctly
#[tauri::command]
pub async fn api_test_custom_openai_connection<R: Runtime>(
    _app: AppHandle<R>,
    endpoint: String,
    api_key: Option<String>,
    model: String,
) -> Result<serde_json::Value, String> {
    log_info!(
        "api_test_custom_openai_connection called: endpoint='{}', model='{}'",
        &endpoint,
        &model
    );

    // Validate endpoint URL format
    if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
        return Err("Endpoint must start with http:// or https://".to_string());
    }

    // Build the URL - append /chat/completions to the base endpoint
    let url = format!("{}/chat/completions", endpoint.trim_end_matches('/'));

    // Create a minimal test request
    let test_request = serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "user",
                "content": "Hi"
            }
        ],
        "max_tokens": 5
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    let mut request = client
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&test_request);

    // Add authorization if API key provided
    if let Some(key) = api_key.filter(|k| !k.trim().is_empty()) {
        request = request.header("Authorization", format!("Bearer {}", key));
    }

    match request.send().await {
        Ok(response) => {
            let status = response.status();
            let response_text = response.text().await.unwrap_or_default();

            if status.is_success() {
                // Parse response as JSON to verify it's a valid OpenAI-compatible response
                match serde_json::from_str::<serde_json::Value>(&response_text) {
                    Ok(json) => {
                        // Verify the response has the expected OpenAI structure
                        if let Some(choices) = json.get("choices") {
                            if let Some(choices_array) = choices.as_array() {
                                if !choices_array.is_empty() {
                                    // Verify the first choice has the required message structure
                                    if let Some(first_choice) = choices_array.get(0) {
                                        // Check if message.content field exists (can be empty string)
                                        let has_message_structure = first_choice
                                            .get("message")
                                            .and_then(|m| {
                                                m.get("content")
                                                    .or_else(|| m.get("reasoning_content"))
                                            })
                                            .is_some();

                                        if has_message_structure {
                                            log_info!("✅ Custom OpenAI connection test successful - response validated");
                                            return Ok(serde_json::json!({
                                                "status": "success",
                                                "message": "Connection successful and response validated",
                                                "http_status": status.as_u16()
                                            }));
                                        }
                                    }
                                }
                            }
                        }

                        // Response was 200 but doesn't match OpenAI format
                        log_warn!(
                            "⚠️ Endpoint returned 200 but response doesn't match OpenAI format"
                        );
                        Err("Endpoint is reachable but doesn't appear to be OpenAI-compatible. Response is missing 'choices' array or 'message.content' / 'message.reasoning_content' field.".to_string())
                    }
                    Err(e) => {
                        log_warn!(
                            "⚠️ Endpoint returned 200 but response is not valid JSON: {}",
                            e
                        );
                        Err(format!(
                            "Endpoint is reachable but returned invalid JSON: {}",
                            e
                        ))
                    }
                }
            } else {
                log_warn!(
                    "⚠️ Custom OpenAI connection test failed with status {}",
                    status
                );
                Err(format!("Connection failed with status {}", status))
            }
        }
        Err(e) => {
            log_error!("❌ Custom OpenAI connection test failed: {}", e);
            if e.is_timeout() {
                Err("Connection timed out. Please check the endpoint URL.".to_string())
            } else if e.is_connect() {
                Err("Could not connect to endpoint. Please verify the URL is correct and the server is running.".to_string())
            } else {
                Err(format!("Connection failed: {}", e))
            }
        }
    }
}

#[cfg(test)]
mod local_only_tests {
    use super::validate_local_ollama_endpoint;

    #[test]
    fn ollama_endpoint_accepts_only_loopback_http() {
        assert_eq!(validate_local_ollama_endpoint(None).unwrap(), None);
        assert_eq!(
            validate_local_ollama_endpoint(Some("http://localhost:11434/"))
                .unwrap()
                .as_deref(),
            Some("http://localhost:11434")
        );
        assert!(validate_local_ollama_endpoint(Some("http://127.0.0.1:11434")).is_ok());
        assert!(validate_local_ollama_endpoint(Some("https://localhost:11434")).is_err());
        assert!(validate_local_ollama_endpoint(Some("http://192.168.1.50:11434")).is_err());
        assert!(validate_local_ollama_endpoint(Some("http://user@localhost:11434")).is_err());
    }
}
