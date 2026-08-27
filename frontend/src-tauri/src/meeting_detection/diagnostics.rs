use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime};

use chrono::Utc;
use serde::Serialize;
use tauri::{AppHandle, Manager, Runtime};

use super::CandidateApp;

const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_FILES: usize = 7;
const MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

static JOURNAL: LazyLock<Mutex<Option<SanitizedJournal>>> =
    LazyLock::new(|| Mutex::new(None));

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalEvent {
    DetectorStarted,
    DetectorStopped,
    WorkerRestarted,
    StateTransition,
    EvidenceObserved,
    RecordingAttempt,
    RecordingCommitted,
    RecordingStopped,
    SaveResult,
    PermissionChanged,
    Error,
}

/// Diagnostic payload with a deliberately closed set of privacy-safe fields.
/// Free-form messages, paths, URLs, titles, transcripts, participants and audio
/// metadata have no representation in this type.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalEntry {
    timestamp: String,
    event: JournalEvent,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<SafeCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<SafeCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recording_id: Option<SafeCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    candidate: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attempt: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_match: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_active: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_active: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    degraded_reasons: Option<Vec<SafeCode>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<SafeCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    success: Option<bool>,
}

impl JournalEntry {
    pub fn new(event: JournalEvent) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339(),
            event,
            state: None,
            session_id: None,
            recording_id: None,
            candidate: None,
            attempt: None,
            context_match: None,
            input_active: None,
            output_active: None,
            degraded_reasons: None,
            error_code: None,
            success: None,
        }
    }

    pub fn state(mut self, value: &str) -> Self {
        self.state = Some(SafeCode::new(value));
        self
    }

    pub fn session_id(mut self, value: &str) -> Self {
        self.session_id = Some(SafeCode::new(value));
        self
    }

    pub fn recording_id(mut self, value: &str) -> Self {
        self.recording_id = Some(SafeCode::new(value));
        self
    }

    pub fn candidate(mut self, value: CandidateApp) -> Self {
        self.candidate = Some(value.display_name().to_string());
        self
    }

    pub fn attempt(mut self, value: u32) -> Self {
        self.attempt = Some(value);
        self
    }

    pub fn evidence(mut self, context: bool, input: bool, output: bool) -> Self {
        self.context_match = Some(context);
        self.input_active = Some(input);
        self.output_active = Some(output);
        self
    }

    pub fn degraded_reasons(mut self, values: &[String]) -> Self {
        self.degraded_reasons = Some(values.iter().map(|value| SafeCode::new(value)).collect());
        self
    }

    pub fn error_code(mut self, value: &str) -> Self {
        self.error_code = Some(SafeCode::new(value));
        self
    }

    pub fn success(mut self, value: bool) -> Self {
        self.success = Some(value);
        self
    }
}

#[derive(Clone, Debug, Serialize)]
struct SafeCode(String);

impl SafeCode {
    fn new(value: &str) -> Self {
        let normalized = value.trim();
        let valid = !normalized.is_empty()
            && normalized.len() <= 64
            && normalized.chars().all(|character| {
                character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || matches!(character, '_' | '-')
            });
        if !valid {
            Self("invalid_code".to_string())
        } else {
            Self(normalized.to_string())
        }
    }
}

struct SanitizedJournal {
    directory: PathBuf,
}

impl SanitizedJournal {
    fn append(&self, entry: &JournalEntry) -> Result<(), String> {
        fs::create_dir_all(&self.directory).map_err(|_| "diagnostic_directory_unavailable")?;
        self.prune_expired();
        let current = self.path(0);
        if current.metadata().map(|metadata| metadata.len()).unwrap_or(0) >= MAX_FILE_BYTES {
            self.rotate();
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path(0))
            .map_err(|_| "diagnostic_file_unavailable")?;
        serde_json::to_writer(&mut file, entry).map_err(|_| "diagnostic_serialize_failed")?;
        file.write_all(b"\n").map_err(|_| "diagnostic_write_failed")
    }

    fn path(&self, index: usize) -> PathBuf {
        if index == 0 {
            self.directory.join("auto-capture.jsonl")
        } else {
            self.directory.join(format!("auto-capture.{index}.jsonl"))
        }
    }

    fn rotate(&self) {
        for index in (1..MAX_FILES).rev() {
            let _ = fs::rename(self.path(index - 1), self.path(index));
        }
    }

    fn prune_expired(&self) {
        let now = SystemTime::now();
        for index in 0..MAX_FILES {
            let path = self.path(index);
            let expired = path
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > MAX_AGE);
            if expired {
                let _ = fs::remove_file(path);
            }
        }
    }
}

pub fn initialize(app_data_dir: &Path) -> Result<(), String> {
    let journal = SanitizedJournal {
        directory: app_data_dir.join("logs").join("auto-capture"),
    };
    fs::create_dir_all(&journal.directory).map_err(|_| "diagnostic_directory_unavailable")?;
    *JOURNAL.lock().map_err(|_| "diagnostic_state_unavailable")? = Some(journal);
    Ok(())
}

pub fn record(entry: JournalEntry) {
    if let Ok(journal) = JOURNAL.lock() {
        if let Some(journal) = journal.as_ref() {
            let _ = journal.append(&entry);
        }
    }
}

#[tauri::command]
pub fn export_auto_capture_diagnostics<R: Runtime>(app: AppHandle<R>) -> Result<String, String> {
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|_| "diagnostic_directory_unavailable")?;
    let source = app_data.join("logs").join("auto-capture");
    let exports = app_data.join("diagnostics-exports");
    fs::create_dir_all(&exports).map_err(|_| "diagnostic_export_directory_unavailable")?;
    let destination = exports.join(format!(
        "auto-capture-{}.jsonl",
        Utc::now().format("%Y%m%dT%H%M%SZ")
    ));
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&destination)
        .map_err(|_| "diagnostic_export_create_failed")?;
    for index in (0..MAX_FILES).rev() {
        let path = if index == 0 {
            source.join("auto-capture.jsonl")
        } else {
            source.join(format!("auto-capture.{index}.jsonl"))
        };
        if let Ok(mut input) = fs::File::open(path) {
            let mut buffer = Vec::new();
            input.read_to_end(&mut buffer).map_err(|_| "diagnostic_export_read_failed")?;
            output.write_all(&buffer).map_err(|_| "diagnostic_export_write_failed")?;
        }
    }
    Ok(destination.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_form_codes_are_reduced_to_safe_tokens() {
        let entry = JournalEntry::new(JournalEvent::Error)
            .state("Recording State /Users/example")
            .error_code("https://meet.google.com/secret-code");
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("/Users"));
        assert!(!json.contains("https"));
        assert!(!json.contains("secret-code"));
    }

    #[test]
    fn rotation_keeps_seven_files() {
        let directory = tempfile::tempdir().unwrap();
        let journal = SanitizedJournal { directory: directory.path().to_path_buf() };
        fs::create_dir_all(&journal.directory).unwrap();
        for index in 0..(MAX_FILES + 3) {
            fs::write(journal.path(0), format!("{index}".repeat(MAX_FILE_BYTES as usize / 2 + 1))).unwrap();
            journal.rotate();
        }
        let count = (0..(MAX_FILES + 2)).filter(|index| journal.path(*index).exists()).count();
        assert!(count <= MAX_FILES);
    }
}
