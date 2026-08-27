use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionKind {
    ScreenRecording,
    BrowserAutomation,
    AudioCapture,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionStatus {
    Granted,
    Denied,
    NotDetermined,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionState {
    pub kind: PermissionKind,
    pub status: PermissionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    pub message: String,
}

impl PermissionState {
    fn new(
        kind: PermissionKind,
        status: PermissionStatus,
        error_code: Option<&str>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            status,
            error_code: error_code.map(str::to_string),
            message: message.into(),
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn screen_recording_granted() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn screen_recording_granted() -> bool {
    true
}

pub fn permission_state(kind: PermissionKind) -> PermissionState {
    match kind {
        PermissionKind::ScreenRecording => {
            if screen_recording_granted() {
                PermissionState::new(
                    kind,
                    PermissionStatus::Granted,
                    None,
                    "Screen Recording access is granted.",
                )
            } else {
                PermissionState::new(
                    kind,
                    PermissionStatus::Denied,
                    Some("screen_recording_permission_denied"),
                    "Screen Recording access is required to inspect supported meeting windows.",
                )
            }
        }
        PermissionKind::BrowserAutomation => {
            #[cfg(target_os = "macos")]
            {
                super::macos::browser_automation_permission_state()
            }
            #[cfg(not(target_os = "macos"))]
            {
                PermissionState::new(
                    kind,
                    PermissionStatus::Unavailable,
                    Some("browser_automation_unavailable"),
                    "Browser Automation is available only on macOS.",
                )
            }
        }
        PermissionKind::AudioCapture => PermissionState::new(
            kind,
            PermissionStatus::NotDetermined,
            Some("audio_capture_permission_unverifiable"),
            "macOS does not expose a reliable Audio Capture preflight. Meetily verifies non-silent system audio after recording starts.",
        ),
    }
}

#[tauri::command]
pub fn get_auto_capture_permissions() -> Vec<PermissionState> {
    [
        PermissionKind::ScreenRecording,
        PermissionKind::BrowserAutomation,
        PermissionKind::AudioCapture,
    ]
    .into_iter()
    .map(permission_state)
    .collect()
}

#[tauri::command]
pub async fn request_auto_capture_permission(kind: PermissionKind) -> PermissionState {
    match kind {
        PermissionKind::ScreenRecording => {
            #[cfg(target_os = "macos")]
            {
                let granted = tokio::task::spawn_blocking(|| unsafe {
                    CGRequestScreenCaptureAccess()
                })
                .await
                .unwrap_or(false);
                if granted {
                    PermissionState::new(
                        kind,
                        PermissionStatus::Granted,
                        None,
                        "Screen Recording access is granted.",
                    )
                } else {
                    PermissionState::new(
                        kind,
                        PermissionStatus::Denied,
                        Some("screen_recording_permission_denied"),
                        "Screen Recording access was not granted. Enable Meetily Plus in System Settings and restart the app.",
                    )
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                permission_state(kind)
            }
        }
        PermissionKind::BrowserAutomation => {
            #[cfg(target_os = "macos")]
            {
                let _ = tokio::task::spawn_blocking(super::macos::probe_browser_automation).await;
            }
            permission_state(kind)
        }
        PermissionKind::AudioCapture => {
            let _ = tokio::task::spawn_blocking(crate::audio::permissions::trigger_system_audio_permission)
                .await;
            permission_state(kind)
        }
    }
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_capture_status_does_not_claim_tap_creation_proves_permission() {
        let state = permission_state(PermissionKind::AudioCapture);
        assert_eq!(state.status, PermissionStatus::NotDetermined);
        assert_eq!(
            state.error_code.as_deref(),
            Some("audio_capture_permission_unverifiable")
        );
    }
}
