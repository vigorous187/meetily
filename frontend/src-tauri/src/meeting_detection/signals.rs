use std::fmt;
use std::time::Duration;

/// A supported application or browser service that may be hosting a meeting.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CandidateApp {
    BrowserCall,
    Zoom,
    MicrosoftTeams,
    FaceTime,
    GoogleMeet,
    CiscoWebex,
    SlackHuddle,
    Discord,
    JitsiMeet,
    Whereby,
    GoToMeeting,
    RingCentral,
    Riverside,
    Dialpad,
}

impl CandidateApp {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::BrowserCall => "Browser meeting",
            Self::Zoom => "Zoom",
            Self::MicrosoftTeams => "Microsoft Teams",
            Self::FaceTime => "FaceTime",
            Self::GoogleMeet => "Google Meet",
            Self::CiscoWebex => "Cisco Webex",
            Self::SlackHuddle => "Slack Huddle",
            Self::Discord => "Discord voice call",
            Self::JitsiMeet => "Jitsi Meet",
            Self::Whereby => "Whereby",
            Self::GoToMeeting => "GoTo Meeting",
            Self::RingCentral => "RingCentral",
            Self::Riverside => "Riverside",
            Self::Dialpad => "Dialpad",
        }
    }

    fn rank(self) -> usize {
        match self {
            Self::Zoom => 0,
            Self::MicrosoftTeams => 1,
            Self::FaceTime => 2,
            Self::GoogleMeet => 3,
            Self::CiscoWebex => 4,
            Self::SlackHuddle => 5,
            Self::Discord => 6,
            Self::JitsiMeet => 7,
            Self::Whereby => 8,
            Self::GoToMeeting => 9,
            Self::RingCentral => 10,
            Self::Riverside => 11,
            Self::Dialpad => 12,
            Self::BrowserCall => 13,
        }
    }
}

impl fmt::Display for CandidateApp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.display_name())
    }
}

/// Process and optional window evidence observed locally.
///
/// Every field is intentionally metadata-only. The detector never receives
/// raw microphone or system-audio samples.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ObservedApplication {
    /// CoreAudio/NSWorkspace process identifier used only for live correlation.
    pub process_id: Option<i32>,
    pub process_name: String,
    pub bundle_identifier: Option<String>,
    pub executable_path: Option<String>,
    pub window_title: Option<String>,
    /// Meeting-domain classification produced in memory. Raw URLs are never
    /// returned by the browser Automation adapter.
    pub meeting_context: Option<CandidateApp>,
    pub is_frontmost: bool,
    /// Whether macOS reports this recognized app as currently producing audio.
    pub is_using_system_audio: bool,
    /// Whether CoreAudio reports the process or its mapped host consuming
    /// microphone/input audio.
    pub is_audio_input_active: bool,
    /// Whether CoreAudio reports the process or its mapped host producing
    /// output audio.
    pub is_audio_output_active: bool,
    /// Whether this is a provider helper that exists only during an active call.
    pub is_active_call_helper: bool,
}

/// One local sample consumed by the policy engine.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SignalSnapshot {
    /// Monotonic time since the provider was created.
    pub observed_at: Duration,
    pub applications: Vec<ObservedApplication>,
    pub recording_active: bool,
    pub dictation_active: bool,
}

/// Injectable source of detector evidence.
pub trait SignalProvider {
    type Error;

    fn sample(&mut self) -> Result<SignalSnapshot, Self::Error>;
}

/// Select the strongest supported meeting context, including inactive native
/// client processes. This is useful for diagnostics only.
pub fn classify_candidate(applications: &[ObservedApplication]) -> Option<CandidateApp> {
    classify(applications, false)
}

/// Select a candidate only when there is active meeting evidence.
///
/// A native process merely being open is insufficient. Evidence must be a
/// filtered meeting window or macOS reporting that recognized client as an
/// active system-audio producer, or a provider call-only helper. Browser
/// candidates always require a filtered meeting title/URL.
pub fn classify_active_candidate(applications: &[ObservedApplication]) -> Option<CandidateApp> {
    classify_meeting_evidence(applications)
        .filter(|evidence| evidence.confidence == EvidenceConfidence::High)
        .map(|evidence| evidence.candidate)
}

/// Privacy-safe confidence summary consumed by the detector/coordinator.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum EvidenceConfidence {
    None,
    Possible,
    High,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CandidateEvidence {
    pub candidate: CandidateApp,
    pub confidence: EvidenceConfidence,
    pub meeting_context: bool,
    pub input_active: bool,
    pub output_active: bool,
}

/// Select the strongest current evidence. Confidence, live audio and PID-host
/// correlation take precedence over the candidate's deterministic tie-breaker.
pub fn classify_meeting_evidence(
    applications: &[ObservedApplication],
) -> Option<CandidateEvidence> {
    applications
        .iter()
        .filter_map(application_evidence)
        .max_by_key(|evidence| {
            (
                evidence.confidence,
                evidence.input_active as u8 + evidence.output_active as u8,
                evidence.meeting_context,
                usize::MAX - evidence.candidate.rank(),
            )
        })
}

fn classify(applications: &[ObservedApplication], require_active: bool) -> Option<CandidateApp> {
    applications
        .iter()
        .filter(|application| {
            !require_active
                || application.is_using_system_audio
                || application.is_audio_input_active
                || application.is_audio_output_active
                || application.is_active_call_helper
                || application.window_title.is_some()
                || application.meeting_context.is_some()
        })
        .filter_map(|application| {
            classify_application(application).map(|candidate| {
                let frontmost_rank = usize::from(!application.is_frontmost);
                ((frontmost_rank, candidate.rank()), candidate)
            })
        })
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, candidate)| candidate)
}

fn application_evidence(application: &ObservedApplication) -> Option<CandidateEvidence> {
    let process_name = normalized(&application.process_name);
    let bundle = normalized(application.bundle_identifier.as_deref().unwrap_or_default());
    let executable = normalized(application.executable_path.as_deref().unwrap_or_default());
    let input_active = application.is_audio_input_active;
    let output_active = application.is_audio_output_active || application.is_using_system_audio;

    if is_supported_browser(&process_name, &bundle) {
        let context = application.meeting_context.or_else(|| {
            application
                .window_title
                .as_deref()
                .and_then(candidate_from_meeting_title)
        });
        let candidate = context.unwrap_or(CandidateApp::BrowserCall);
        let confidence = if (context.is_some() && (input_active || output_active))
            || (input_active && output_active)
        {
            EvidenceConfidence::High
        } else if context.is_some() || input_active || output_active {
            EvidenceConfidence::Possible
        } else {
            EvidenceConfidence::None
        };
        return (confidence != EvidenceConfidence::None).then_some(CandidateEvidence {
            candidate,
            confidence,
            meeting_context: context.is_some(),
            input_active,
            output_active,
        });
    }

    let candidate = application
        .meeting_context
        .or_else(|| candidate_from_native_identity(&process_name, &bundle, &executable))?;
    let has_context = application.meeting_context.is_some()
        || application
            .window_title
            .as_deref()
            .and_then(candidate_from_meeting_title)
            .is_some();
    let confidence = if application.is_active_call_helper
        || has_context
        || input_active
        || output_active
    {
        EvidenceConfidence::High
    } else {
        EvidenceConfidence::None
    };
    (confidence != EvidenceConfidence::None).then_some(CandidateEvidence {
        candidate,
        confidence,
        meeting_context: has_context,
        input_active,
        output_active,
    })
}

fn classify_application(application: &ObservedApplication) -> Option<CandidateApp> {
    let process_name = normalized(&application.process_name);
    let bundle = normalized(application.bundle_identifier.as_deref().unwrap_or_default());
    let executable = normalized(application.executable_path.as_deref().unwrap_or_default());

    if is_supported_browser(&process_name, &bundle) {
        return application.meeting_context.or_else(|| application
            .window_title
            .as_deref()
            .and_then(candidate_from_meeting_title));
    }

    candidate_from_native_identity(&process_name, &bundle, &executable)
}

pub(super) fn candidate_from_process_name(process_name: &str) -> Option<CandidateApp> {
    candidate_from_native_identity(&normalized(process_name), "", "")
}

pub(super) fn is_active_call_helper(process_name: &str) -> bool {
    matches!(
        normalized(process_name).as_str(),
        "cpthost" | "zccimeetinghost" | "caphost"
    )
}

fn candidate_from_native_identity(
    process_name: &str,
    bundle: &str,
    executable: &str,
) -> Option<CandidateApp> {
    if matches!(
        process_name,
        "zoom.us"
            | "zoom"
            | "zoom workplace"
            | "cpthost"
            | "zccimeetinghost"
            | "caphost"
            | "zoomaudiodevice"
            | "zoom audio device"
    ) || bundle == "us.zoom.xos"
        || matches!(
            bundle,
            "us.zoom.cpthost"
                | "us.zoom.zccimeetinghost"
                | "us.zoom.caphost"
                | "zoom.us.zoomaudiodevice"
        )
        || executable.ends_with("/zoom.us")
    {
        Some(CandidateApp::Zoom)
    } else if matches!(process_name, "microsoft teams" | "msteams" | "teams")
        || matches!(bundle, "com.microsoft.teams" | "com.microsoft.teams2")
        || executable.ends_with("/microsoft teams")
    {
        Some(CandidateApp::MicrosoftTeams)
    } else if process_name == "facetime"
        || bundle == "com.apple.facetime"
        || executable.ends_with("/facetime")
    {
        Some(CandidateApp::FaceTime)
    } else if matches!(
        process_name,
        "webex" | "webex meetings" | "cisco webex meetings" | "ciscocollabhost"
    ) || bundle.contains("webex")
    {
        Some(CandidateApp::CiscoWebex)
    } else if process_name == "slack" || bundle == "com.tinyspeck.slackmacgap" {
        Some(CandidateApp::SlackHuddle)
    } else if process_name == "discord" || bundle == "com.hnc.discord" {
        Some(CandidateApp::Discord)
    } else if matches!(process_name, "gotomeeting" | "goto" | "go to meeting")
        || bundle.contains("gotomeeting")
    {
        Some(CandidateApp::GoToMeeting)
    } else if process_name.contains("ringcentral") || bundle.contains("ringcentral") {
        Some(CandidateApp::RingCentral)
    } else if process_name == "dialpad" || bundle.contains("dialpad") {
        Some(CandidateApp::Dialpad)
    } else {
        None
    }
}

pub(super) fn candidate_from_meeting_title(window_title: &str) -> Option<CandidateApp> {
    let value = normalized(window_title);
    let contains_any = |patterns: &[&str]| patterns.iter().any(|pattern| value.contains(pattern));

    if contains_any(&["meet.google.com", "google meet"]) {
        Some(CandidateApp::GoogleMeet)
    } else if contains_any(&["zoom.us", "zoom meeting", "zoom webinar"]) {
        Some(CandidateApp::Zoom)
    } else if contains_any(&[
        "teams.microsoft.com",
        "microsoft teams meeting",
        "meeting | microsoft teams",
    ]) {
        Some(CandidateApp::MicrosoftTeams)
    } else if contains_any(&["webex.com", "webex meeting", "cisco webex meetings"]) {
        Some(CandidateApp::CiscoWebex)
    } else if contains_any(&["slack huddle", "huddle | slack", "huddle - slack"]) {
        Some(CandidateApp::SlackHuddle)
    } else if contains_any(&["meet.jit.si", "jitsi meet"]) {
        Some(CandidateApp::JitsiMeet)
    } else if contains_any(&["whereby.com", "whereby meeting"]) {
        Some(CandidateApp::Whereby)
    } else if contains_any(&["meet.goto.com", "goto meeting", "gotomeeting"]) {
        Some(CandidateApp::GoToMeeting)
    } else if contains_any(&[
        "v.ringcentral.com",
        "ringcentral meeting",
        "ringcentral video",
    ]) {
        Some(CandidateApp::RingCentral)
    } else if contains_any(&["riverside.fm", "riverside studio"]) {
        Some(CandidateApp::Riverside)
    } else if contains_any(&["dialpad.com/meetings", "dialpad meeting"]) {
        Some(CandidateApp::Dialpad)
    } else if contains_any(&["discord voice", "voice connected - discord"]) {
        Some(CandidateApp::Discord)
    } else {
        None
    }
}

pub(super) fn is_supported_browser(process_name: &str, bundle: &str) -> bool {
    matches!(
        process_name,
        "google chrome"
            | "chrome"
            | "safari"
            | "microsoft edge"
            | "brave browser"
            | "arc"
            | "firefox"
            | "vivaldi"
            | "opera"
            | "orion"
    ) || matches!(
        bundle,
        "com.google.chrome"
            | "com.apple.safari"
            | "com.microsoft.edgemac"
            | "com.brave.browser"
            | "company.thebrowser.browser"
            | "org.mozilla.firefox"
            | "com.vivaldi.vivaldi"
            | "com.operasoftware.opera"
            | "com.kagi.kagimacos"
    )
}

fn normalized(value: &str) -> String {
    value.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(process_name: &str) -> ObservedApplication {
        ObservedApplication {
            process_name: process_name.to_string(),
            ..ObservedApplication::default()
        }
    }

    fn browser(process_name: &str, title: &str) -> ObservedApplication {
        ObservedApplication {
            process_name: process_name.to_string(),
            window_title: Some(title.to_string()),
            is_audio_output_active: true,
            ..ObservedApplication::default()
        }
    }

    #[test]
    fn recognizes_major_native_clients() {
        let cases = [
            ("zoom.us", CandidateApp::Zoom),
            ("MSTeams", CandidateApp::MicrosoftTeams),
            ("FaceTime", CandidateApp::FaceTime),
            ("Webex", CandidateApp::CiscoWebex),
            ("Slack", CandidateApp::SlackHuddle),
            ("Discord", CandidateApp::Discord),
            ("GoToMeeting", CandidateApp::GoToMeeting),
            ("RingCentral", CandidateApp::RingCentral),
            ("Dialpad", CandidateApp::Dialpad),
        ];
        for (process, expected) in cases {
            assert_eq!(classify_candidate(&[app(process)]), Some(expected));
        }
    }

    #[test]
    fn native_process_requires_active_evidence() {
        assert_eq!(classify_active_candidate(&[app("zoom.us")]), None);

        let mut zoom = app("zoom.us");
        zoom.is_using_system_audio = true;
        assert_eq!(classify_active_candidate(&[zoom]), Some(CandidateApp::Zoom));
    }

    #[test]
    fn zoom_call_helpers_are_active_meeting_evidence() {
        for helper in ["CptHost", "zCCIMeetingHost", "caphost"] {
            let mut observation = app(helper);
            observation.is_active_call_helper = is_active_call_helper(helper);
            assert_eq!(
                classify_active_candidate(&[observation]),
                Some(CandidateApp::Zoom),
                "helper: {helper}"
            );
        }

        assert!(!is_active_call_helper("ZoomUpdater"));
        assert_eq!(classify_active_candidate(&[app("ZoomUpdater")]), None);
    }

    #[test]
    fn recognizes_major_browser_meeting_services() {
        let cases = [
            ("Daily sync - Google Meet", CandidateApp::GoogleMeet),
            ("Zoom Meeting", CandidateApp::Zoom),
            ("Meeting | Microsoft Teams", CandidateApp::MicrosoftTeams),
            ("Customer call - Webex Meeting", CandidateApp::CiscoWebex),
            ("Engineering - Slack Huddle", CandidateApp::SlackHuddle),
            ("meet.jit.si/team-room", CandidateApp::JitsiMeet),
            ("whereby.com/team-room", CandidateApp::Whereby),
            ("meet.goto.com/123", CandidateApp::GoToMeeting),
            ("RingCentral Video", CandidateApp::RingCentral),
            ("Riverside Studio", CandidateApp::Riverside),
            ("Dialpad Meeting", CandidateApp::Dialpad),
        ];
        for (title, expected) in cases {
            assert_eq!(
                classify_active_candidate(&[browser("Firefox", title)]),
                Some(expected),
                "title: {title}"
            );
        }
    }

    #[test]
    fn browser_without_meeting_evidence_is_not_a_candidate() {
        assert_eq!(classify_candidate(&[app("Google Chrome")]), None);
        assert_eq!(
            classify_active_candidate(&[browser("Safari", "Inbox - Gmail")]),
            None
        );
    }

    #[test]
    fn browser_title_without_audio_is_possible_but_does_not_start() {
        let observation = ObservedApplication {
            process_name: "Safari".to_string(),
            meeting_context: Some(CandidateApp::GoogleMeet),
            ..ObservedApplication::default()
        };
        let evidence = classify_meeting_evidence(&[observation]).expect("possible evidence");
        assert_eq!(evidence.confidence, EvidenceConfidence::Possible);
        assert_eq!(classify_active_candidate(&[browser("Safari", "Inbox")]), None);
    }

    #[test]
    fn browser_output_only_without_context_is_possible_not_high() {
        let observation = ObservedApplication {
            process_name: "Google Chrome".to_string(),
            is_audio_output_active: true,
            ..ObservedApplication::default()
        };
        let evidence = classify_meeting_evidence(&[observation]).expect("possible evidence");
        assert_eq!(evidence.candidate, CandidateApp::BrowserCall);
        assert_eq!(evidence.confidence, EvidenceConfidence::Possible);
        assert_eq!(classify_active_candidate(&[ObservedApplication {
            process_name: "Google Chrome".to_string(),
            is_audio_output_active: true,
            ..ObservedApplication::default()
        }]), None);
    }

    #[test]
    fn frontmost_candidate_wins() {
        let mut zoom = app("zoom.us");
        zoom.is_using_system_audio = true;
        let mut meet = browser("Safari", "Google Meet");
        meet.is_frontmost = true;

        assert_eq!(
            classify_active_candidate(&[zoom, meet]),
            Some(CandidateApp::GoogleMeet)
        );
    }
}
