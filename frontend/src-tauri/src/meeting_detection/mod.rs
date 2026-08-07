//! Local-only meeting detection.
//!
//! The detector deliberately separates signal collection from policy. Platform
//! adapters report observable context, while [`LocalMeetingDetector`] decides
//! whether that evidence is strong enough to request a recording prompt.
//! An inactive process on its own is never sufficient. A filtered meeting
//! window or recognized system-audio producer can request a prompt.

mod detector;
pub mod runtime;
mod signals;

#[cfg(target_os = "macos")]
mod macos;

pub use detector::{
    DetectorConfig, DetectorEvent, LocalMeetingDetector, MeetingDetector, MeetingDetectorState,
};
pub use runtime::{
    dismiss_meeting_detection, is_meeting_detection_running, start_meeting_detection,
    stop_meeting_detection,
};
pub use signals::{
    classify_active_candidate, classify_candidate, CandidateApp, ObservedApplication,
    SignalProvider, SignalSnapshot,
};

#[cfg(target_os = "macos")]
pub use macos::{
    MacOsSignalProvider, MacOsWindowContextSource, NoWindowContextSource, RuntimeActivityFlags,
    WindowContextSource,
};
