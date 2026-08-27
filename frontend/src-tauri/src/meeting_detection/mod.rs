//! Local-only meeting detection.
//!
//! The detector deliberately separates signal collection from policy. Platform
//! adapters report observable context, while [`LocalMeetingDetector`] decides
//! whether that evidence is strong enough to request a recording prompt.
//! An inactive process on its own is never sufficient. A filtered meeting
//! window or recognized system-audio producer can request a prompt.

mod coordinator;
mod detector;
pub mod autostart;
pub mod diagnostics;
pub mod permissions;
pub mod runtime;
mod signals;

#[cfg(target_os = "macos")]
mod macos;

pub use detector::{
    DetectorConfig, DetectorEvent, LocalMeetingDetector, MeetingDetector, MeetingDetectorState,
};
pub use coordinator::{
    AutoCaptureCoordinator, AutoCaptureError, AutoCaptureHealth, AutoCapturePhase,
    AutoCaptureStatusChanged, CoordinatorAction, MeetingSessionIdSource,
    UuidMeetingSessionIdSource,
};
pub use runtime::{
    dismiss_meeting_detection, is_meeting_detection_running, start_meeting_detection,
    stop_meeting_detection,
};
pub use signals::{
    classify_active_candidate, classify_candidate, classify_meeting_evidence, CandidateApp,
    CandidateEvidence, EvidenceConfidence, ObservedApplication, SignalProvider, SignalSnapshot,
};

pub use permissions::{
    get_auto_capture_permissions, request_auto_capture_permission, PermissionKind, PermissionState,
    PermissionStatus,
};
pub use autostart::{
    get_launch_at_login_status, set_launch_at_login, LaunchAtLoginStatus,
};

#[cfg(target_os = "macos")]
pub use macos::{
    MacOsSignalProvider, MacOsWindowContextSource, NoWindowContextSource, RuntimeActivityFlags,
    WindowContextSource,
};
