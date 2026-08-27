//! Atomic ownership for every live recording session.
//!
//! The audio pipeline remains responsible for device and persistence work. This
//! module owns the decision to start or stop it, and only publishes ownership
//! after the pipeline confirms that capture is active.

use serde::Serialize;
use std::sync::LazyLock;
use tauri::{AppHandle, Emitter, Runtime};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::audio::recording_commands;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum RecordingOrigin {
    Manual,
    Automatic {
        meeting_session_id: String,
        candidate: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TranscriptionStatus {
    Ready,
    Degraded,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartReceipt {
    pub recording_id: String,
    pub origin: RecordingOrigin,
    pub transcription_status: TranscriptionStatus,
    pub degraded_reasons: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingSessionError {
    pub code: &'static str,
    pub message: String,
    pub transient: bool,
}

impl std::fmt::Display for RecordingSessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[derive(Clone, Debug)]
struct ActiveRecording {
    receipt: StartReceipt,
}

#[derive(Default)]
struct AuthorityState {
    active: Option<ActiveRecording>,
}

static AUTHORITY: LazyLock<Mutex<AuthorityState>> =
    LazyLock::new(|| Mutex::new(AuthorityState::default()));

pub async fn start_manual<R: Runtime>(
    app: AppHandle<R>,
    mic_device_name: Option<String>,
    system_device_name: Option<String>,
    meeting_name: Option<String>,
) -> Result<StartReceipt, RecordingSessionError> {
    start(
        app,
        RecordingOrigin::Manual,
        mic_device_name,
        system_device_name,
        meeting_name,
    )
    .await
}

pub async fn start_automatic<R: Runtime>(
    app: AppHandle<R>,
    meeting_session_id: String,
    candidate: String,
) -> Result<StartReceipt, RecordingSessionError> {
    start(
        app,
        RecordingOrigin::Automatic {
            meeting_session_id,
            candidate: candidate.clone(),
        },
        None,
        None,
        Some(candidate),
    )
    .await
}

async fn start<R: Runtime>(
    app: AppHandle<R>,
    origin: RecordingOrigin,
    mic_device_name: Option<String>,
    system_device_name: Option<String>,
    meeting_name: Option<String>,
) -> Result<StartReceipt, RecordingSessionError> {
    let mut authority = AUTHORITY.lock().await;
    reconcile_locked(&mut authority);
    if authority.active.is_some() || recording_commands::is_recording_active() {
        return Err(RecordingSessionError {
            code: "recording_already_active",
            message: "A recording is already in progress".to_string(),
            transient: false,
        });
    }

    let start_result = match (mic_device_name, system_device_name) {
        (None, None) => {
            recording_commands::start_recording_with_meeting_name(app.clone(), meeting_name).await
        }
        (mic_device_name, system_device_name) => {
            recording_commands::start_recording_with_devices_and_meeting(
                app.clone(),
                mic_device_name,
                system_device_name,
                meeting_name,
            )
            .await
        }
    };
    start_result.map_err(|error| classify_start_error(&error))?;

    if !recording_commands::is_recording_active() {
        return Err(RecordingSessionError {
            code: "recording_not_active_after_start",
            message: "Audio capture did not become active after startup".to_string(),
            transient: true,
        });
    }

    let degraded_reasons = recording_commands::current_degraded_reasons();
    let receipt = StartReceipt {
        recording_id: Uuid::new_v4().to_string(),
        origin,
        transcription_status: if degraded_reasons.is_empty() {
            TranscriptionStatus::Ready
        } else {
            TranscriptionStatus::Degraded
        },
        degraded_reasons,
    };
    authority.active = Some(ActiveRecording {
        receipt: receipt.clone(),
    });
    if let Err(error) = app.emit("recording-started", &receipt) {
        // Ownership and live streams are already committed. Event delivery is
        // advisory and must never trigger a second recording attempt.
        log::warn!("Recording acknowledged but receipt delivery failed: {error}");
    }
    Ok(receipt)
}

pub async fn stop_manual<R: Runtime>(
    app: AppHandle<R>,
    args: recording_commands::RecordingArgs,
) -> Result<(), RecordingSessionError> {
    let mut authority = AUTHORITY.lock().await;
    reconcile_locked(&mut authority);
    if !recording_commands::is_recording_active() {
        authority.active = None;
        return Ok(());
    }
    recording_commands::stop_recording(app, args)
        .await
        .map_err(|error| RecordingSessionError {
            code: "recording_stop_failed",
            message: error,
            transient: true,
        })?;
    authority.active = None;
    Ok(())
}

pub async fn stop_automatic<R: Runtime>(
    app: AppHandle<R>,
    meeting_session_id: &str,
    recording_id: &str,
) -> Result<(), RecordingSessionError> {
    let mut authority = AUTHORITY.lock().await;
    reconcile_locked(&mut authority);
    let owned = authority.active.as_ref().is_some_and(|active| {
        active.receipt.recording_id == recording_id
            && matches!(
                &active.receipt.origin,
                RecordingOrigin::Automatic {
                    meeting_session_id: active_session,
                    ..
                } if active_session == meeting_session_id
            )
    });
    if !owned {
        return Err(RecordingSessionError {
            code: "recording_ownership_mismatch",
            message: "The automatic stop no longer owns the active recording".to_string(),
            transient: false,
        });
    }

    recording_commands::stop_recording(
        app,
        recording_commands::RecordingArgs {
            // The audio pipeline persists to its configured meeting folder; the
            // compatibility field is intentionally unused by that implementation.
            save_path: String::new(),
        },
    )
    .await
    .map_err(|error| RecordingSessionError {
        code: "recording_stop_failed",
        message: error,
        transient: true,
    })?;
    authority.active = None;
    Ok(())
}

pub async fn active_receipt() -> Option<StartReceipt> {
    let mut authority = AUTHORITY.lock().await;
    reconcile_locked(&mut authority);
    authority.active.as_ref().map(|active| active.receipt.clone())
}

fn reconcile_locked(authority: &mut AuthorityState) {
    if !recording_commands::is_recording_active() {
        authority.active = None;
    }
}

fn classify_start_error(error: &str) -> RecordingSessionError {
    let normalized = error.to_ascii_lowercase();
    if normalized.contains("dictation") {
        RecordingSessionError {
            code: "dictation_active",
            message: error.to_string(),
            transient: true,
        }
    } else if normalized.contains("permission") || normalized.contains("not permitted") {
        RecordingSessionError {
            code: "audio_permission_denied",
            message: error.to_string(),
            transient: false,
        }
    } else if normalized.contains("model") || normalized.contains("downloading") {
        RecordingSessionError {
            code: "transcription_unavailable",
            message: error.to_string(),
            transient: false,
        }
    } else if normalized.contains("microphone") && normalized.contains("available") {
        RecordingSessionError {
            code: "microphone_unavailable",
            message: error.to_string(),
            transient: false,
        }
    } else if normalized.contains("already in progress") {
        RecordingSessionError {
            code: "recording_already_active",
            message: error.to_string(),
            transient: false,
        }
    } else {
        RecordingSessionError {
            code: "recording_start_failed",
            message: error.to_string(),
            transient: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_retryable_and_actionable_start_failures() {
        assert!(classify_start_error("device temporarily busy").transient);
        assert!(!classify_start_error("microphone permission denied").transient);
        assert_eq!(
            classify_start_error("model is downloading").code,
            "transcription_unavailable"
        );
    }

    #[test]
    fn automatic_origin_serializes_without_ambiguous_ownership() {
        let origin = RecordingOrigin::Automatic {
            meeting_session_id: "meeting-1".to_string(),
            candidate: "Zoom".to_string(),
        };
        let value = serde_json::to_value(origin).expect("serializable origin");
        assert_eq!(value["type"], "automatic");
        assert_eq!(value["meetingSessionId"], "meeting-1");
    }
}
