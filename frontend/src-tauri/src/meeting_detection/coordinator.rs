use serde::Serialize;
use std::time::Duration;
use uuid::Uuid;

use super::CandidateApp;
use crate::recording_session::{RecordingOrigin, StartReceipt};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AutoCapturePhase {
    Disabled,
    Observing,
    Starting,
    RetryScheduled,
    Recording,
    Stopping,
    NeedsAction,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoCaptureStatusChanged {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
    pub state: AutoCapturePhase,
    pub attempt: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_retry_at_ms: Option<u64>,
    pub degraded_reasons: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoCaptureHealth {
    pub enabled: bool,
    pub detector_running: bool,
    #[serde(flatten)]
    pub status: AutoCaptureStatusChanged,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_result: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutoCaptureError {
    pub code: &'static str,
    pub message: String,
    pub transient: bool,
}

impl AutoCaptureError {
    pub fn transient(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            transient: true,
        }
    }

    pub fn needs_action(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            transient: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CoordinatorAction {
    Start {
        meeting_session_id: String,
        candidate: CandidateApp,
        attempt: u32,
    },
    Stop {
        meeting_session_id: String,
        recording_id: String,
    },
}

pub trait MeetingSessionIdSource: Send {
    fn next_id(&mut self) -> String;
}

#[derive(Default)]
pub struct UuidMeetingSessionIdSource;

impl MeetingSessionIdSource for UuidMeetingSessionIdSource {
    fn next_id(&mut self) -> String {
        Uuid::new_v4().to_string()
    }
}

#[derive(Clone, Debug)]
struct Occurrence {
    session_id: String,
    candidate: CandidateApp,
    active: bool,
    handled_by_manual_recording: bool,
}

pub struct AutoCaptureCoordinator<I = UuidMeetingSessionIdSource> {
    enabled: bool,
    detector_running: bool,
    phase: AutoCapturePhase,
    occurrence: Option<Occurrence>,
    attempt: u32,
    recording_id: Option<String>,
    next_retry_at: Option<Duration>,
    degraded_reasons: Vec<String>,
    error_code: Option<String>,
    message: String,
    last_result: Option<String>,
    id_source: I,
}

impl Default for AutoCaptureCoordinator<UuidMeetingSessionIdSource> {
    fn default() -> Self {
        Self::new(UuidMeetingSessionIdSource)
    }
}

impl<I: MeetingSessionIdSource> AutoCaptureCoordinator<I> {
    pub fn new(id_source: I) -> Self {
        Self {
            enabled: false,
            detector_running: false,
            phase: AutoCapturePhase::Disabled,
            occurrence: None,
            attempt: 0,
            recording_id: None,
            next_retry_at: None,
            degraded_reasons: Vec::new(),
            error_code: None,
            message: "Automatic capture is disabled".to_string(),
            last_result: None,
            id_source,
        }
    }

    pub fn health(&self) -> AutoCaptureHealth {
        AutoCaptureHealth {
            enabled: self.enabled,
            detector_running: self.detector_running,
            status: self.status(),
            last_result: self.last_result.clone(),
        }
    }

    pub fn status(&self) -> AutoCaptureStatusChanged {
        AutoCaptureStatusChanged {
            session_id: self.occurrence.as_ref().map(|value| value.session_id.clone()),
            candidate: self
                .occurrence
                .as_ref()
                .map(|value| value.candidate.display_name().to_string()),
            state: self.phase,
            attempt: self.attempt,
            recording_id: self.recording_id.clone(),
            next_retry_at_ms: self.next_retry_at.map(duration_millis),
            degraded_reasons: self.degraded_reasons.clone(),
            error_code: self.error_code.clone(),
            message: self.message.clone(),
        }
    }

    pub fn set_detector_running(&mut self, running: bool) {
        self.detector_running = running;
        if self.enabled && !running && self.phase != AutoCapturePhase::Stopping {
            self.phase = AutoCapturePhase::Failed;
            self.error_code = Some("detector_worker_stopped".to_string());
            self.message = "Automatic capture detector stopped unexpectedly".to_string();
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) -> Option<CoordinatorAction> {
        self.enabled = enabled;
        self.error_code = None;
        self.next_retry_at = None;
        if enabled {
            if self.phase == AutoCapturePhase::Disabled {
                self.phase = AutoCapturePhase::Observing;
            }
            self.message = "Watching for a meeting".to_string();
            return None;
        }

        if let (Some(occurrence), Some(recording_id)) =
            (self.occurrence.as_mut(), self.recording_id.clone())
        {
            occurrence.active = false;
            self.phase = AutoCapturePhase::Stopping;
            self.message = "Stopping the automatic recording".to_string();
            return Some(CoordinatorAction::Stop {
                meeting_session_id: occurrence.session_id.clone(),
                recording_id,
            });
        }

        if let Some(occurrence) = self.occurrence.as_mut() {
            occurrence.active = false;
        }
        self.phase = AutoCapturePhase::Disabled;
        self.message = "Automatic capture is disabled".to_string();
        None
    }

    pub fn meeting_started(
        &mut self,
        candidate: CandidateApp,
        recording_active: bool,
        dictation_active: bool,
    ) -> Option<CoordinatorAction> {
        if !self.enabled {
            return None;
        }
        if self
            .occurrence
            .as_ref()
            .is_some_and(|current| current.active && current.candidate == candidate)
        {
            return None;
        }

        self.occurrence = Some(Occurrence {
            session_id: self.id_source.next_id(),
            candidate,
            active: true,
            handled_by_manual_recording: recording_active,
        });
        self.attempt = 0;
        self.recording_id = None;
        self.next_retry_at = None;
        self.degraded_reasons.clear();
        self.error_code = None;

        if recording_active {
            self.phase = AutoCapturePhase::Observing;
            self.last_result = Some("manual_recording_overlap".to_string());
            self.message = "Meeting is already covered by a manual recording".to_string();
            return None;
        }
        if dictation_active {
            self.phase = AutoCapturePhase::RetryScheduled;
            self.message = "Waiting for dictation to finish".to_string();
            return None;
        }
        self.begin_start()
    }

    pub fn meeting_ended(&mut self, candidate: CandidateApp) -> Option<CoordinatorAction> {
        let Some(occurrence) = self.occurrence.as_mut() else {
            return None;
        };
        if occurrence.candidate != candidate || !occurrence.active {
            return None;
        }
        occurrence.active = false;
        self.next_retry_at = None;

        if let Some(recording_id) = self.recording_id.clone() {
            self.phase = AutoCapturePhase::Stopping;
            self.message = "Meeting ended; saving the automatic recording".to_string();
            return Some(CoordinatorAction::Stop {
                meeting_session_id: occurrence.session_id.clone(),
                recording_id,
            });
        }

        if self.phase != AutoCapturePhase::Starting {
            self.finish_occurrence();
        } else {
            self.message = "Meeting ended while recording startup was completing".to_string();
        }
        None
    }

    pub fn tick(
        &mut self,
        now: Duration,
        recording_active: bool,
        dictation_active: bool,
    ) -> Option<CoordinatorAction> {
        let Some(occurrence) = self.occurrence.as_mut() else {
            return None;
        };
        if !self.enabled || !occurrence.active || occurrence.handled_by_manual_recording {
            return None;
        }

        if self.phase == AutoCapturePhase::Recording && !recording_active {
            self.recording_id = None;
            self.phase = AutoCapturePhase::Failed;
            self.error_code = Some("recording_stopped_unexpectedly".to_string());
            self.message = "The automatic recording stopped unexpectedly".to_string();
            self.last_result = Some("recording_stopped_unexpectedly".to_string());
            return None;
        }
        if self.phase != AutoCapturePhase::RetryScheduled {
            return None;
        }
        if recording_active {
            occurrence.handled_by_manual_recording = true;
            self.next_retry_at = None;
            self.phase = AutoCapturePhase::Observing;
            self.message = "Meeting is already covered by a manual recording".to_string();
            self.last_result = Some("manual_recording_overlap".to_string());
            return None;
        }
        if dictation_active {
            self.message = "Waiting for dictation to finish".to_string();
            return None;
        }
        if self.next_retry_at.map_or(true, |deadline| now >= deadline) {
            return self.begin_start();
        }
        None
    }

    pub fn start_succeeded(
        &mut self,
        meeting_session_id: &str,
        receipt: StartReceipt,
    ) -> Option<CoordinatorAction> {
        let receipt_session = match &receipt.origin {
            RecordingOrigin::Automatic {
                meeting_session_id,
                ..
            } => Some(meeting_session_id.as_str()),
            RecordingOrigin::Manual => None,
        };
        if receipt_session != Some(meeting_session_id) {
            self.phase = AutoCapturePhase::Failed;
            self.error_code = Some("start_receipt_mismatch".to_string());
            self.message = "Recorder acknowledgement did not match the detected meeting".to_string();
            return None;
        }

        let Some(occurrence) = self.occurrence.as_ref() else {
            return Some(CoordinatorAction::Stop {
                meeting_session_id: meeting_session_id.to_string(),
                recording_id: receipt.recording_id,
            });
        };
        if occurrence.session_id != meeting_session_id {
            return Some(CoordinatorAction::Stop {
                meeting_session_id: meeting_session_id.to_string(),
                recording_id: receipt.recording_id,
            });
        }

        self.recording_id = Some(receipt.recording_id.clone());
        self.degraded_reasons = receipt.degraded_reasons;
        self.next_retry_at = None;
        self.error_code = None;
        if !occurrence.active || !self.enabled {
            self.phase = AutoCapturePhase::Stopping;
            self.message = "Startup completed after the meeting ended; stopping safely".to_string();
            return Some(CoordinatorAction::Stop {
                meeting_session_id: meeting_session_id.to_string(),
                recording_id: receipt.recording_id,
            });
        }

        self.phase = AutoCapturePhase::Recording;
        self.message = if self.degraded_reasons.is_empty() {
            "Meeting is recording automatically".to_string()
        } else {
            "Meeting is recording with reduced audio coverage".to_string()
        };
        self.last_result = Some("recording_started".to_string());
        None
    }

    pub fn start_failed(
        &mut self,
        meeting_session_id: &str,
        error: AutoCaptureError,
        now: Duration,
    ) {
        let Some(occurrence) = self.occurrence.as_ref() else {
            return;
        };
        if occurrence.session_id != meeting_session_id {
            return;
        }
        if !occurrence.active || !self.enabled {
            self.finish_occurrence();
            return;
        }

        if error.code == "recording_already_active" {
            if let Some(occurrence) = self.occurrence.as_mut() {
                occurrence.handled_by_manual_recording = true;
            }
            self.phase = AutoCapturePhase::Observing;
            self.next_retry_at = None;
            self.error_code = None;
            self.message = "Meeting is already covered by a manual recording".to_string();
            self.last_result = Some("manual_recording_overlap".to_string());
            return;
        }

        self.error_code = Some(error.code.to_string());
        self.message = error.message;
        self.last_result = Some(error.code.to_string());
        if error.transient {
            let delay = retry_delay_after_attempt(self.attempt);
            self.next_retry_at = Some(now.saturating_add(delay));
            self.phase = AutoCapturePhase::RetryScheduled;
        } else {
            self.next_retry_at = None;
            self.phase = AutoCapturePhase::NeedsAction;
        }
    }

    pub fn readiness_changed(&mut self) -> Option<CoordinatorAction> {
        if self.phase != AutoCapturePhase::NeedsAction
            || !self.enabled
            || !self.occurrence.as_ref().is_some_and(|value| value.active)
        {
            return None;
        }
        self.error_code = None;
        self.begin_start()
    }

    pub fn stop_finished(
        &mut self,
        meeting_session_id: &str,
        recording_id: &str,
        result: Result<(), AutoCaptureError>,
    ) {
        let matches = self.occurrence.as_ref().is_some_and(|occurrence| {
            occurrence.session_id == meeting_session_id
                && self.recording_id.as_deref() == Some(recording_id)
        });
        if !matches {
            return;
        }

        match result {
            Ok(()) => {
                self.last_result = Some("recording_saved".to_string());
                self.finish_occurrence();
            }
            Err(error) => {
                self.phase = AutoCapturePhase::Failed;
                self.error_code = Some(error.code.to_string());
                self.message = error.message;
                self.last_result = Some(error.code.to_string());
            }
        }
    }

    fn begin_start(&mut self) -> Option<CoordinatorAction> {
        let occurrence = self.occurrence.as_ref()?;
        self.attempt = self.attempt.saturating_add(1);
        self.phase = AutoCapturePhase::Starting;
        self.next_retry_at = None;
        self.error_code = None;
        self.message = "Starting automatic recording".to_string();
        Some(CoordinatorAction::Start {
            meeting_session_id: occurrence.session_id.clone(),
            candidate: occurrence.candidate,
            attempt: self.attempt,
        })
    }

    fn finish_occurrence(&mut self) {
        self.occurrence = None;
        self.attempt = 0;
        self.recording_id = None;
        self.next_retry_at = None;
        self.degraded_reasons.clear();
        self.error_code = None;
        self.phase = if self.enabled {
            AutoCapturePhase::Observing
        } else {
            AutoCapturePhase::Disabled
        };
        self.message = if self.enabled {
            "Watching for a meeting".to_string()
        } else {
            "Automatic capture is disabled".to_string()
        };
    }
}

fn retry_delay_after_attempt(attempt: u32) -> Duration {
    match attempt {
        0 | 1 => Duration::from_secs(2),
        2 => Duration::from_secs(5),
        3 => Duration::from_secs(15),
        _ => Duration::from_secs(60),
    }
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording_session::TranscriptionStatus;
    use std::collections::VecDeque;

    struct FakeIds(VecDeque<String>);

    impl MeetingSessionIdSource for FakeIds {
        fn next_id(&mut self) -> String {
            self.0.pop_front().expect("test meeting id available")
        }
    }

    fn coordinator() -> AutoCaptureCoordinator<FakeIds> {
        AutoCaptureCoordinator::new(FakeIds(VecDeque::from(["meeting-1".to_string()])))
    }

    fn receipt(recording_id: &str) -> StartReceipt {
        StartReceipt {
            recording_id: recording_id.to_string(),
            origin: RecordingOrigin::Automatic {
                meeting_session_id: "meeting-1".to_string(),
                candidate: "Zoom".to_string(),
            },
            transcription_status: TranscriptionStatus::Ready,
            degraded_reasons: Vec::new(),
        }
    }

    fn enable_and_start(coordinator: &mut AutoCaptureCoordinator<FakeIds>) -> CoordinatorAction {
        coordinator.set_enabled(true);
        coordinator
            .meeting_started(CandidateApp::Zoom, false, false)
            .expect("start action")
    }

    #[test]
    fn claims_ownership_only_after_acknowledgement() {
        let mut coordinator = coordinator();
        let action = enable_and_start(&mut coordinator);
        assert!(matches!(action, CoordinatorAction::Start { .. }));
        assert_eq!(coordinator.health().status.state, AutoCapturePhase::Starting);
        assert_eq!(coordinator.health().status.recording_id, None);

        coordinator.start_succeeded("meeting-1", receipt("recording-1"));
        assert_eq!(coordinator.health().status.state, AutoCapturePhase::Recording);
        assert_eq!(
            coordinator.health().status.recording_id.as_deref(),
            Some("recording-1")
        );
    }

    #[test]
    fn retries_transient_failures_at_bounded_delays() {
        let mut coordinator = coordinator();
        enable_and_start(&mut coordinator);
        coordinator.start_failed(
            "meeting-1",
            AutoCaptureError::transient("audio_busy", "Audio is busy"),
            Duration::from_secs(10),
        );
        assert_eq!(coordinator.health().status.next_retry_at_ms, Some(12_000));
        assert!(coordinator.tick(Duration::from_secs(11), false, false).is_none());
        assert!(matches!(
            coordinator.tick(Duration::from_secs(12), false, false),
            Some(CoordinatorAction::Start { attempt: 2, .. })
        ));

        coordinator.start_failed(
            "meeting-1",
            AutoCaptureError::transient("audio_busy", "Audio is busy"),
            Duration::from_secs(12),
        );
        assert_eq!(coordinator.health().status.next_retry_at_ms, Some(17_000));
    }

    #[test]
    fn waits_for_readiness_after_permission_failure() {
        let mut coordinator = coordinator();
        enable_and_start(&mut coordinator);
        coordinator.start_failed(
            "meeting-1",
            AutoCaptureError::needs_action("microphone_denied", "Allow microphone access"),
            Duration::ZERO,
        );
        assert_eq!(coordinator.health().status.state, AutoCapturePhase::NeedsAction);
        assert!(coordinator.tick(Duration::from_secs(60), false, false).is_none());
        assert!(matches!(
            coordinator.readiness_changed(),
            Some(CoordinatorAction::Start { attempt: 2, .. })
        ));
    }

    #[test]
    fn end_during_start_stops_only_the_acknowledged_recording() {
        let mut coordinator = coordinator();
        enable_and_start(&mut coordinator);
        assert!(coordinator.meeting_ended(CandidateApp::Zoom).is_none());
        let action = coordinator
            .start_succeeded("meeting-1", receipt("recording-1"))
            .expect("conditional stop");
        assert_eq!(
            action,
            CoordinatorAction::Stop {
                meeting_session_id: "meeting-1".to_string(),
                recording_id: "recording-1".to_string(),
            }
        );
    }

    #[test]
    fn stale_end_for_another_candidate_is_ignored() {
        let mut coordinator = coordinator();
        enable_and_start(&mut coordinator);
        coordinator.start_succeeded("meeting-1", receipt("recording-1"));
        assert!(coordinator.meeting_ended(CandidateApp::FaceTime).is_none());
        assert_eq!(coordinator.health().status.state, AutoCapturePhase::Recording);
    }

    #[test]
    fn manual_overlap_is_handled_without_adoption() {
        let mut coordinator = coordinator();
        coordinator.set_enabled(true);
        assert!(coordinator
            .meeting_started(CandidateApp::Zoom, true, false)
            .is_none());
        assert_eq!(coordinator.health().status.recording_id, None);
        assert!(coordinator.meeting_ended(CandidateApp::Zoom).is_none());
    }

    #[test]
    fn dictation_defers_then_starts_immediately_after_release() {
        let mut coordinator = coordinator();
        coordinator.set_enabled(true);
        coordinator.meeting_started(CandidateApp::Zoom, false, true);
        assert_eq!(
            coordinator.health().status.state,
            AutoCapturePhase::RetryScheduled
        );
        assert!(coordinator.tick(Duration::from_secs(1), false, true).is_none());
        assert!(matches!(
            coordinator.tick(Duration::from_secs(2), false, false),
            Some(CoordinatorAction::Start { attempt: 1, .. })
        ));
    }

    #[test]
    fn disabling_cancels_retry_and_stops_owned_recording() {
        let mut retrying = coordinator();
        enable_and_start(&mut retrying);
        retrying.start_failed(
            "meeting-1",
            AutoCaptureError::transient("audio_busy", "Audio is busy"),
            Duration::ZERO,
        );
        assert!(retrying.set_enabled(false).is_none());
        assert!(retrying.tick(Duration::from_secs(10), false, false).is_none());
        assert_eq!(retrying.health().status.state, AutoCapturePhase::Disabled);

        let mut recording = coordinator();
        enable_and_start(&mut recording);
        recording.start_succeeded("meeting-1", receipt("recording-1"));
        assert!(matches!(
            recording.set_enabled(false),
            Some(CoordinatorAction::Stop { .. })
        ));
    }

    #[test]
    fn stale_start_ack_is_conditionally_stopped() {
        let mut coordinator = coordinator();
        enable_and_start(&mut coordinator);
        let action = coordinator
            .start_succeeded("old-meeting", StartReceipt {
                recording_id: "old-recording".to_string(),
                origin: RecordingOrigin::Automatic {
                    meeting_session_id: "old-meeting".to_string(),
                    candidate: "Zoom".to_string(),
                },
                transcription_status: TranscriptionStatus::Ready,
                degraded_reasons: Vec::new(),
            })
            .expect("stale recording cleanup");
        assert!(matches!(action, CoordinatorAction::Stop { .. }));
        assert_eq!(coordinator.health().status.session_id.as_deref(), Some("meeting-1"));
    }
}
