use std::time::Duration;

use super::signals::{
    classify_meeting_evidence, CandidateApp, EvidenceConfidence, SignalProvider,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DetectorConfig {
    pub sustained_meeting_activity: Duration,
    pub meeting_end_grace_period: Duration,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            sustained_meeting_activity: Duration::from_secs(4),
            // Meeting windows and audio indicators can briefly disappear while
            // an app changes views. Do not stop a recording on a short dropout.
            meeting_end_grace_period: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DetectorEvent {
    MeetingStarted { candidate: CandidateApp },
    MeetingEnded { candidate: CandidateApp },
    PossibleMeeting { candidate: CandidateApp },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MeetingDetectorState {
    Idle,
    Observing {
        candidate: CandidateApp,
        active_since: Duration,
    },
    Active {
        candidate: CandidateApp,
    },
    Ending {
        candidate: CandidateApp,
        inactive_since: Duration,
    },
    Dismissed {
        candidate: CandidateApp,
    },
    Suppressed,
}

pub trait MeetingDetector {
    type Error;

    fn poll(&mut self) -> Result<Option<DetectorEvent>, Self::Error>;

    /// Retained for command compatibility. A dismissed active meeting will not
    /// trigger again until its evidence disappears.
    fn dismiss_prompt(&mut self);

    fn state(&self) -> MeetingDetectorState;
}

/// Local meeting lifecycle policy. It only emits lifecycle events; the UI owns
/// recording permissions, device selection, persistence, and notifications.
pub struct LocalMeetingDetector<P> {
    provider: P,
    config: DetectorConfig,
    state: MeetingDetectorState,
    last_observed_at: Duration,
    possible_candidate: Option<CandidateApp>,
}

impl<P> LocalMeetingDetector<P> {
    pub fn new(provider: P, config: DetectorConfig) -> Self {
        Self {
            provider,
            config,
            state: MeetingDetectorState::Idle,
            last_observed_at: Duration::ZERO,
            possible_candidate: None,
        }
    }

    pub fn into_provider(self) -> P {
        self.provider
    }

    /// Reconcile a backend-owned automatic recording after a detector worker
    /// restart. The next missing-evidence sample enters the normal end grace
    /// period instead of orphaning the recording forever.
    pub fn restore_active(&mut self, candidate: CandidateApp) {
        self.state = MeetingDetectorState::Active { candidate };
        self.possible_candidate = None;
    }
}

fn candidates_compatible(active: CandidateApp, observed: CandidateApp) -> bool {
    active == observed
        || active == CandidateApp::BrowserCall
        || observed == CandidateApp::BrowserCall
}

impl<P: SignalProvider> MeetingDetector for LocalMeetingDetector<P> {
    type Error = P::Error;

    fn poll(&mut self) -> Result<Option<DetectorEvent>, Self::Error> {
        let snapshot = self.provider.sample()?;
        let now = snapshot.observed_at;

        // A provider restart or broken monotonic source cannot inherit stale
        // meeting activity.
        if now < self.last_observed_at {
            self.state = MeetingDetectorState::Idle;
        }
        self.last_observed_at = now;

        // Dictation and meeting recording share the microphone. Suppress a new
        // meeting while dictation is active, but never suppress an already
        // active meeting merely because recording has started; end detection
        // must keep running throughout the recording.
        if snapshot.dictation_active
            && !matches!(
                self.state,
                MeetingDetectorState::Active { .. } | MeetingDetectorState::Ending { .. }
            )
        {
            self.state = MeetingDetectorState::Suppressed;
            return Ok(None);
        }
        if self.state == MeetingDetectorState::Suppressed {
            self.state = MeetingDetectorState::Idle;
        }

        let evidence = classify_meeting_evidence(&snapshot.applications);
        let candidate = evidence.and_then(|value| {
            (value.confidence == EvidenceConfidence::High).then_some(value.candidate)
        });
        let sustaining_candidate = evidence.and_then(|value| {
            (value.confidence == EvidenceConfidence::High
                || value.input_active
                || value.output_active)
                .then_some(value.candidate)
        });

        if evidence.is_none() {
            self.possible_candidate = None;
        } else if evidence.is_some_and(|value| value.confidence == EvidenceConfidence::High) {
            self.possible_candidate = None;
        } else if !matches!(
            self.state,
            MeetingDetectorState::Active { .. } | MeetingDetectorState::Ending { .. }
        ) {
            let possible = evidence.expect("possible evidence is present").candidate;
            if self.possible_candidate != Some(possible) {
                self.possible_candidate = Some(possible);
                return Ok(Some(DetectorEvent::PossibleMeeting { candidate: possible }));
            }
        }

        match (self.state, candidate, sustaining_candidate) {
            (MeetingDetectorState::Idle | MeetingDetectorState::Suppressed, Some(candidate), _) => {
                self.state = MeetingDetectorState::Observing {
                    candidate,
                    active_since: now,
                };
                Ok(None)
            }
            (MeetingDetectorState::Idle | MeetingDetectorState::Suppressed, None, _) => Ok(None),

            (
                MeetingDetectorState::Observing {
                    candidate: active,
                    active_since,
                },
                Some(candidate),
                _,
            ) if active == candidate => {
                if now.saturating_sub(active_since) >= self.config.sustained_meeting_activity {
                    self.state = MeetingDetectorState::Active { candidate };
                    Ok(Some(DetectorEvent::MeetingStarted { candidate }))
                } else {
                    Ok(None)
                }
            }
            (MeetingDetectorState::Observing { .. }, Some(candidate), _) => {
                self.state = MeetingDetectorState::Observing {
                    candidate,
                    active_since: now,
                };
                Ok(None)
            }
            (MeetingDetectorState::Observing { .. }, None, _) => {
                self.state = MeetingDetectorState::Idle;
                Ok(None)
            }

            (MeetingDetectorState::Active { candidate: active }, _, Some(candidate))
                if candidates_compatible(active, candidate) =>
            {
                Ok(None)
            }
            (MeetingDetectorState::Active { candidate }, _, _) => {
                self.state = MeetingDetectorState::Ending {
                    candidate,
                    inactive_since: now,
                };
                Ok(None)
            }
            (
                MeetingDetectorState::Ending {
                    candidate: active, ..
                },
                _,
                Some(candidate),
            ) if candidates_compatible(active, candidate) => {
                self.state = MeetingDetectorState::Active { candidate: active };
                Ok(None)
            }
            (
                MeetingDetectorState::Ending {
                    candidate,
                    inactive_since,
                },
                _,
                _,
            ) => {
                if now.saturating_sub(inactive_since) >= self.config.meeting_end_grace_period {
                    self.state = MeetingDetectorState::Idle;
                    Ok(Some(DetectorEvent::MeetingEnded { candidate }))
                } else {
                    Ok(None)
                }
            }
            (MeetingDetectorState::Dismissed { candidate: active }, Some(candidate), _)
                if active == candidate =>
            {
                Ok(None)
            }
            (MeetingDetectorState::Dismissed { .. }, Some(candidate), _) => {
                self.state = MeetingDetectorState::Observing {
                    candidate,
                    active_since: now,
                };
                Ok(None)
            }
            (MeetingDetectorState::Dismissed { .. }, None, _) => {
                self.state = MeetingDetectorState::Idle;
                Ok(None)
            }
        }
    }

    fn dismiss_prompt(&mut self) {
        if let MeetingDetectorState::Active { candidate } = self.state {
            self.state = MeetingDetectorState::Dismissed { candidate };
        }
    }

    fn state(&self) -> MeetingDetectorState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::convert::Infallible;

    use super::*;
    use crate::meeting_detection::signals::{ObservedApplication, SignalSnapshot};

    struct FakeProvider {
        samples: VecDeque<SignalSnapshot>,
    }

    impl FakeProvider {
        fn new(samples: Vec<SignalSnapshot>) -> Self {
            Self {
                samples: samples.into(),
            }
        }
    }

    impl SignalProvider for FakeProvider {
        type Error = Infallible;

        fn sample(&mut self) -> Result<SignalSnapshot, Self::Error> {
            Ok(self.samples.pop_front().expect("test sample available"))
        }
    }

    fn sample(seconds: u64, app: Option<&str>, active_evidence: bool) -> SignalSnapshot {
        SignalSnapshot {
            observed_at: Duration::from_secs(seconds),
            applications: app
                .map(|process_name| {
                    vec![ObservedApplication {
                        process_name: process_name.to_string(),
                        is_using_system_audio: active_evidence,
                        ..ObservedApplication::default()
                    }]
                })
                .unwrap_or_default(),
            recording_active: false,
            dictation_active: false,
        }
    }

    fn detector(samples: Vec<SignalSnapshot>) -> LocalMeetingDetector<FakeProvider> {
        LocalMeetingDetector::new(
            FakeProvider::new(samples),
            DetectorConfig {
                sustained_meeting_activity: Duration::from_secs(4),
                meeting_end_grace_period: Duration::from_secs(15),
            },
        )
    }

    #[test]
    fn process_context_alone_never_starts() {
        let mut detector = detector(vec![
            sample(0, Some("zoom.us"), false),
            sample(10, Some("zoom.us"), false),
        ]);
        assert_eq!(detector.poll().unwrap(), None);
        assert_eq!(detector.poll().unwrap(), None);
    }

    #[test]
    fn sustained_activity_starts_once() {
        let mut detector = detector(vec![
            sample(0, Some("zoom.us"), true),
            sample(3, Some("zoom.us"), true),
            sample(4, Some("zoom.us"), true),
            sample(20, Some("zoom.us"), true),
        ]);

        assert_eq!(detector.poll().unwrap(), None);
        assert_eq!(detector.poll().unwrap(), None);
        assert_eq!(
            detector.poll().unwrap(),
            Some(DetectorEvent::MeetingStarted {
                candidate: CandidateApp::Zoom
            })
        );
        assert_eq!(detector.poll().unwrap(), None);
    }

    #[test]
    fn recording_does_not_disable_end_detection() {
        let mut recording = sample(5, Some("zoom.us"), true);
        recording.recording_active = true;
        let mut ending = sample(6, None, false);
        ending.recording_active = true;
        let mut ended = sample(21, None, false);
        ended.recording_active = true;
        let mut detector = detector(vec![
            sample(0, Some("zoom.us"), true),
            sample(4, Some("zoom.us"), true),
            recording,
            ending,
            ended,
        ]);

        assert_eq!(detector.poll().unwrap(), None);
        assert!(matches!(
            detector.poll().unwrap(),
            Some(DetectorEvent::MeetingStarted { .. })
        ));
        assert_eq!(detector.poll().unwrap(), None);
        assert_eq!(detector.poll().unwrap(), None);
        assert_eq!(
            detector.poll().unwrap(),
            Some(DetectorEvent::MeetingEnded {
                candidate: CandidateApp::Zoom
            })
        );
    }

    #[test]
    fn brief_signal_dropout_does_not_end_meeting() {
        let mut detector = detector(vec![
            sample(0, Some("zoom.us"), true),
            sample(4, Some("zoom.us"), true),
            sample(5, None, false),
            sample(14, None, false),
            sample(15, Some("zoom.us"), true),
        ]);

        assert_eq!(detector.poll().unwrap(), None);
        assert!(detector.poll().unwrap().is_some());
        assert_eq!(detector.poll().unwrap(), None);
        assert_eq!(detector.poll().unwrap(), None);
        assert_eq!(detector.poll().unwrap(), None);
        assert_eq!(
            detector.state(),
            MeetingDetectorState::Active {
                candidate: CandidateApp::Zoom
            }
        );
    }

    #[test]
    fn filtered_browser_meeting_has_full_lifecycle() {
        let browser_sample = |seconds| SignalSnapshot {
            observed_at: Duration::from_secs(seconds),
            applications: vec![ObservedApplication {
                process_name: "Google Chrome".to_string(),
                window_title: Some("Daily sync - Google Meet".to_string()),
                is_audio_output_active: true,
                ..ObservedApplication::default()
            }],
            recording_active: false,
            dictation_active: false,
        };
        let mut detector = detector(vec![
            browser_sample(0),
            browser_sample(4),
            sample(5, None, false),
            sample(20, None, false),
        ]);

        assert_eq!(detector.poll().unwrap(), None);
        assert_eq!(
            detector.poll().unwrap(),
            Some(DetectorEvent::MeetingStarted {
                candidate: CandidateApp::GoogleMeet
            })
        );
        assert_eq!(detector.poll().unwrap(), None);
        assert_eq!(
            detector.poll().unwrap(),
            Some(DetectorEvent::MeetingEnded {
                candidate: CandidateApp::GoogleMeet
            })
        );
    }

    #[test]
    fn browser_output_only_notifies_once_without_starting() {
        let browser_audio = |seconds| SignalSnapshot {
            observed_at: Duration::from_secs(seconds),
            applications: vec![ObservedApplication {
                process_name: "Google Chrome".to_string(),
                is_audio_output_active: true,
                ..ObservedApplication::default()
            }],
            recording_active: false,
            dictation_active: false,
        };
        let mut detector = detector(vec![browser_audio(0), browser_audio(8)]);

        assert_eq!(
            detector.poll().unwrap(),
            Some(DetectorEvent::PossibleMeeting {
                candidate: CandidateApp::BrowserCall,
            })
        );
        assert_eq!(detector.poll().unwrap(), None);
        assert_eq!(detector.state(), MeetingDetectorState::Idle);
    }

    #[test]
    fn established_browser_survives_context_loss_with_correlated_audio() {
        let context_and_audio = |seconds| SignalSnapshot {
            observed_at: Duration::from_secs(seconds),
            applications: vec![ObservedApplication {
                process_name: "Firefox".to_string(),
                meeting_context: Some(CandidateApp::GoogleMeet),
                is_audio_output_active: true,
                ..ObservedApplication::default()
            }],
            recording_active: false,
            dictation_active: false,
        };
        let audio_only = |seconds| SignalSnapshot {
            observed_at: Duration::from_secs(seconds),
            applications: vec![ObservedApplication {
                process_name: "Firefox".to_string(),
                is_audio_output_active: true,
                ..ObservedApplication::default()
            }],
            recording_active: false,
            dictation_active: false,
        };
        let mut detector = detector(vec![context_and_audio(0), context_and_audio(4), audio_only(20)]);

        assert_eq!(detector.poll().unwrap(), None);
        assert!(matches!(detector.poll().unwrap(), Some(DetectorEvent::MeetingStarted { .. })));
        assert_eq!(detector.poll().unwrap(), None);
        assert_eq!(
            detector.state(),
            MeetingDetectorState::Active {
                candidate: CandidateApp::GoogleMeet,
            }
        );
    }

    #[test]
    fn restored_recording_ends_after_absence_grace() {
        let mut detector = detector(vec![sample(0, None, false), sample(15, None, false)]);
        detector.restore_active(CandidateApp::Zoom);

        assert_eq!(detector.poll().unwrap(), None);
        assert_eq!(
            detector.poll().unwrap(),
            Some(DetectorEvent::MeetingEnded {
                candidate: CandidateApp::Zoom,
            })
        );
    }

    #[test]
    fn dictation_suppresses_only_new_meetings() {
        let mut dictating = sample(0, Some("zoom.us"), true);
        dictating.dictation_active = true;
        let mut detector = detector(vec![
            dictating,
            sample(1, Some("zoom.us"), true),
            sample(5, Some("zoom.us"), true),
        ]);

        assert_eq!(detector.poll().unwrap(), None);
        assert_eq!(detector.state(), MeetingDetectorState::Suppressed);
        assert_eq!(detector.poll().unwrap(), None);
        assert!(detector.poll().unwrap().is_some());
    }
}
