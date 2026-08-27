//! macOS signal adapter.
//!
//! Process discovery uses the already-present `sysinfo` dependency. Audio,
//! recording, dictation, and optional window-title evidence are supplied by
//! small integration interfaces rather than inspected through private APIs.

use std::collections::HashSet;
use std::convert::Infallible;
use std::ffi::{c_char, c_void, CStr};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessesToUpdate, System};

use super::permissions::{PermissionKind, PermissionState, PermissionStatus};
use super::signals::{
    candidate_from_meeting_title, candidate_from_process_name, is_active_call_helper,
    is_supported_browser, CandidateApp, ObservedApplication, SignalProvider, SignalSnapshot,
};

#[derive(Clone, Default)]
pub struct RuntimeActivityFlags {
    recording_active: Arc<AtomicBool>,
    dictation_active: Arc<AtomicBool>,
}

impl RuntimeActivityFlags {
    pub fn set_recording_active(&self, active: bool) {
        self.recording_active.store(active, Ordering::Release);
    }

    pub fn set_dictation_active(&self, active: bool) {
        self.dictation_active.store(active, Ordering::Release);
    }

    fn recording_active(&self) -> bool {
        self.recording_active.load(Ordering::Acquire)
    }

    fn dictation_active(&self) -> bool {
        self.dictation_active.load(Ordering::Acquire)
    }
}

/// Optional source for window titles/bundle identifiers.
///
/// The default source returns nothing. A macOS UI adapter can populate these
/// observations using an already-authorized window API without changing the
/// detector policy or process scanner.
pub trait WindowContextSource: Send {
    fn observed_windows(&mut self) -> Vec<ObservedApplication>;
}

pub struct NoWindowContextSource;

impl WindowContextSource for NoWindowContextSource {
    fn observed_windows(&mut self) -> Vec<ObservedApplication> {
        Vec::new()
    }
}

/// Reads all eligible public Core Graphics windows and locally classifies
/// supported browser tabs. Permission requests are explicit commands; sampling
/// never interrupts the user with a system prompt.
#[derive(Default)]
pub struct MacOsWindowContextSource;

impl MacOsWindowContextSource {
    pub fn new() -> Self {
        Self
    }
}

impl WindowContextSource for MacOsWindowContextSource {
    fn observed_windows(&mut self) -> Vec<ObservedApplication> {
        let mut observations = meeting_windows();
        observations.extend(browser_meeting_contexts());
        observations
    }
}

fn meeting_window_observation(
    owner_pid: Option<i32>,
    owner_name: &str,
    window_title: Option<&str>,
) -> Option<ObservedApplication> {
    let window_title = window_title?.trim();
    let meeting_context = candidate_from_meeting_title(window_title)?;

    let normalized_owner = owner_name.trim().to_lowercase();
    let bundle_identifier = supported_browser_bundle(owner_name).map(str::to_string);
    let is_browser = is_supported_browser(
        &normalized_owner,
        bundle_identifier
            .as_deref()
            .unwrap_or_default()
            .to_lowercase()
            .as_str(),
    );
    if !is_browser && candidate_from_process_name(owner_name).is_none() {
        return None;
    }

    Some(ObservedApplication {
        process_id: owner_pid,
        process_name: owner_name.trim().to_string(),
        bundle_identifier,
        executable_path: None,
        window_title: Some(window_title.to_string()),
        meeting_context: Some(meeting_context),
        // Core Graphics orders on-screen windows front-to-back, but a window
        // list alone does not establish which application is active. Avoid
        // inferring foreground state and let native clients retain precedence.
        is_frontmost: false,
        is_using_system_audio: false,
        is_audio_input_active: false,
        is_audio_output_active: false,
        is_active_call_helper: false,
    })
}

fn supported_browser_bundle(owner_name: &str) -> Option<&'static str> {
    match owner_name.trim().to_lowercase().as_str() {
        "google chrome" | "chrome" | "google chrome helper" => Some("com.google.Chrome"),
        "safari" | "safari web content" => Some("com.apple.Safari"),
        "microsoft edge" | "microsoft edge helper" => Some("com.microsoft.edgemac"),
        "brave browser" | "brave browser helper" => Some("com.brave.Browser"),
        "arc" | "arc helper" => Some("company.thebrowser.Browser"),
        "firefox" | "firefoxcp web content" => Some("org.mozilla.firefox"),
        "vivaldi" | "vivaldi helper" => Some("com.vivaldi.Vivaldi"),
        "opera" | "opera helper" => Some("com.operasoftware.Opera"),
        "orion" => Some("com.kagi.kagimacOS"),
        _ => None,
    }
}

fn supported_window_owner(owner_name: &str) -> bool {
    supported_browser_bundle(owner_name).is_some()
        || candidate_from_process_name(owner_name).is_some()
}

fn meeting_windows() -> Vec<ObservedApplication> {
    if !super::permissions::screen_recording_granted() {
        return Vec::new();
    }

    // 0 requests all eligible windows, including minimized/background windows
    // and windows in other Spaces. No window images are captured.
    const ALL_WINDOWS: u32 = 0;
    const EXCLUDE_DESKTOP_ELEMENTS: u32 = 1 << 4;

    let window_array =
        unsafe { CGWindowListCopyWindowInfo(ALL_WINDOWS | EXCLUDE_DESKTOP_ELEMENTS, 0) };
    let Some(window_array) = OwnedCf::new(window_array) else {
        return Vec::new();
    };

    let count = unsafe { CFArrayGetCount(window_array.as_ptr()) };
    if count <= 0 {
        return Vec::new();
    }

    let mut observations = Vec::new();
    for index in 0..count {
        let dictionary = unsafe { CFArrayGetValueAtIndex(window_array.as_ptr(), index) };
        if dictionary.is_null()
            || unsafe { CFGetTypeID(dictionary) } != unsafe { CFDictionaryGetTypeID() }
        {
            continue;
        }

        // Read the owner first and reject every unrelated window before
        // accessing its title. Unrelated window titles never enter Rust memory.
        let Some(owner_name) = cf_dictionary_string(dictionary, unsafe { kCGWindowOwnerName })
        else {
            continue;
        };
        if !supported_window_owner(&owner_name) {
            continue;
        }

        let owner_pid = cf_dictionary_i32(dictionary, unsafe { kCGWindowOwnerPID });
        let title = cf_dictionary_string(dictionary, unsafe { kCGWindowName });
        if let Some(observation) =
            meeting_window_observation(owner_pid, &owner_name, title.as_deref())
        {
            observations.push(observation);
        }
    }

    observations
}

#[derive(Clone, Copy)]
struct BrowserSpec {
    process_name: &'static str,
    bundle_identifier: &'static str,
    apple_script_name: &'static str,
}

const AUTOMATION_BROWSERS: &[BrowserSpec] = &[
    BrowserSpec { process_name: "Safari", bundle_identifier: "com.apple.Safari", apple_script_name: "Safari" },
    BrowserSpec { process_name: "Google Chrome", bundle_identifier: "com.google.Chrome", apple_script_name: "Google Chrome" },
    BrowserSpec { process_name: "Microsoft Edge", bundle_identifier: "com.microsoft.edgemac", apple_script_name: "Microsoft Edge" },
    BrowserSpec { process_name: "Brave Browser", bundle_identifier: "com.brave.Browser", apple_script_name: "Brave Browser" },
    BrowserSpec { process_name: "Arc", bundle_identifier: "company.thebrowser.Browser", apple_script_name: "Arc" },
    BrowserSpec { process_name: "Vivaldi", bundle_identifier: "com.vivaldi.Vivaldi", apple_script_name: "Vivaldi" },
    BrowserSpec { process_name: "Opera", bundle_identifier: "com.operasoftware.Opera", apple_script_name: "Opera" },
];

static AUTOMATION_PERMISSION: LazyLock<Mutex<PermissionState>> = LazyLock::new(|| {
    Mutex::new(PermissionState {
        kind: PermissionKind::BrowserAutomation,
        status: PermissionStatus::NotDetermined,
        error_code: None,
        message: "Open a supported browser to check local meeting-tab access.".to_string(),
    })
});
static AUTOMATION_PROBE_ALLOWED: AtomicBool = AtomicBool::new(false);

pub(crate) fn browser_automation_permission_state() -> PermissionState {
    AUTOMATION_PERMISSION.lock().map(|state| state.clone()).unwrap_or(PermissionState {
        kind: PermissionKind::BrowserAutomation,
        status: PermissionStatus::Unavailable,
        error_code: Some("browser_automation_state_unavailable".to_string()),
        message: "Browser Automation status is unavailable.".to_string(),
    })
}

pub(crate) fn probe_browser_automation() -> Vec<ObservedApplication> {
    AUTOMATION_PROBE_ALLOWED.store(true, Ordering::Release);
    browser_meeting_contexts()
}

fn browser_meeting_contexts() -> Vec<ObservedApplication> {
    if !AUTOMATION_PROBE_ALLOWED.load(Ordering::Acquire) {
        return Vec::new();
    }
    let running: HashSet<String> = System::new_all()
        .processes()
        .values()
        .map(|process| process.name().to_string_lossy().to_lowercase())
        .collect();
    let active_specs: Vec<_> = AUTOMATION_BROWSERS
        .iter()
        .copied()
        .filter(|spec| running.contains(&spec.process_name.to_lowercase()))
        .collect();
    if active_specs.is_empty() {
        set_automation_permission(PermissionStatus::NotDetermined, None,
            "Open a supported browser to check local meeting-tab access.");
        return Vec::new();
    }

    let mut observations = Vec::new();
    let mut succeeded = false;
    let mut denied = false;
    for spec in active_specs {
        match classify_browser_tabs(spec) {
            Ok(candidate) => {
                succeeded = true;
                if let Some(meeting_context) = candidate {
                    observations.push(ObservedApplication {
                        process_name: spec.process_name.to_string(),
                        bundle_identifier: Some(spec.bundle_identifier.to_string()),
                        meeting_context: Some(meeting_context),
                        ..ObservedApplication::default()
                    });
                }
            }
            Err(BrowserAutomationError::Denied) => denied = true,
            Err(BrowserAutomationError::Unavailable) => {}
        }
    }

    if denied {
        set_automation_permission(PermissionStatus::Denied,
            Some("browser_automation_permission_denied"),
            "Browser Automation access is denied. Enable Meetily Plus under Automation in System Settings.");
    } else if succeeded {
        set_automation_permission(PermissionStatus::Granted, None,
            "Local browser meeting-tab classification is available.");
    } else {
        set_automation_permission(PermissionStatus::Unavailable,
            Some("browser_automation_unavailable"),
            "Running browsers do not expose a compatible local tab interface.");
    }
    observations
}

fn set_automation_permission(status: PermissionStatus, error_code: Option<&str>, message: &str) {
    if let Ok(mut state) = AUTOMATION_PERMISSION.lock() {
        *state = PermissionState {
            kind: PermissionKind::BrowserAutomation,
            status,
            error_code: error_code.map(str::to_string),
            message: message.to_string(),
        };
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrowserAutomationError { Denied, Unavailable }

fn classify_browser_tabs(spec: BrowserSpec) -> Result<Option<CandidateApp>, BrowserAutomationError> {
    let output = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(browser_classification_script(spec.apple_script_name))
        .output()
        .map_err(|_| BrowserAutomationError::Unavailable)?;
    if !output.status.success() {
        // -1743 is errAEEventNotPermitted. Never log stderr because it may
        // contain a browser-supplied value on future macOS versions.
        let denied = String::from_utf8_lossy(&output.stderr).contains("-1743");
        return Err(if denied { BrowserAutomationError::Denied } else { BrowserAutomationError::Unavailable });
    }
    let token = String::from_utf8_lossy(&output.stdout);
    Ok(candidate_from_automation_token(token.trim()))
}

fn browser_classification_script(browser_name: &str) -> String {
    // AppleScript returns a fixed token only; raw URLs/titles/codes never cross
    // the process boundary into Meetily or its logs.
    format!(r#"tell application "{browser_name}"
set tabUrls to {{}}
repeat with browserWindow in windows
repeat with browserTab in tabs of browserWindow
set end of tabUrls to URL of browserTab
end repeat
end repeat
end tell
repeat with tabUrl in tabUrls
if tabUrl contains "//meet.google.com/" then return "googleMeet"
if tabUrl contains "//zoom.us/" then return "zoom"
if tabUrl contains "//teams.microsoft.com/" then return "microsoftTeams"
if tabUrl contains ".webex.com/" then return "ciscoWebex"
if tabUrl contains "//meet.jit.si/" then return "jitsiMeet"
if tabUrl contains "//whereby.com/" then return "whereby"
if tabUrl contains "//meet.goto.com/" then return "goToMeeting"
if tabUrl contains "//v.ringcentral.com/" then return "ringCentral"
if tabUrl contains "//riverside.fm/" then return "riverside"
if tabUrl contains "//dialpad.com/meetings/" then return "dialpad"
end repeat
return """#)
}

fn candidate_from_automation_token(token: &str) -> Option<CandidateApp> {
    match token {
        "googleMeet" => Some(CandidateApp::GoogleMeet),
        "zoom" => Some(CandidateApp::Zoom),
        "microsoftTeams" => Some(CandidateApp::MicrosoftTeams),
        "ciscoWebex" => Some(CandidateApp::CiscoWebex),
        "jitsiMeet" => Some(CandidateApp::JitsiMeet),
        "whereby" => Some(CandidateApp::Whereby),
        "goToMeeting" => Some(CandidateApp::GoToMeeting),
        "ringCentral" => Some(CandidateApp::RingCentral),
        "riverside" => Some(CandidateApp::Riverside),
        "dialpad" => Some(CandidateApp::Dialpad),
        _ => None,
    }
}

fn cf_dictionary_value(dictionary: CfTypeRef, key: CfTypeRef) -> Option<CfTypeRef> {
    if key.is_null() {
        return None;
    }
    let mut value: CfTypeRef = std::ptr::null();
    let present = unsafe { CFDictionaryGetValueIfPresent(dictionary, key, &mut value) };
    (present != 0 && !value.is_null()).then_some(value)
}

fn cf_dictionary_string(dictionary: CfTypeRef, key: CfTypeRef) -> Option<String> {
    let value = cf_dictionary_value(dictionary, key)?;
    if unsafe { CFGetTypeID(value) } != unsafe { CFStringGetTypeID() } {
        return None;
    }

    const UTF8: u32 = 0x0800_0100;
    let direct = unsafe { CFStringGetCStringPtr(value, UTF8) };
    if !direct.is_null() {
        return unsafe { CStr::from_ptr(direct) }
            .to_str()
            .ok()
            .map(ToOwned::to_owned);
    }

    let length = unsafe { CFStringGetLength(value) };
    if length < 0 {
        return None;
    }
    let maximum = unsafe { CFStringGetMaximumSizeForEncoding(length, UTF8) };
    if maximum < 0 {
        return None;
    }
    let capacity = usize::try_from(maximum).ok()?.checked_add(1)?;
    let mut buffer = vec![0_u8; capacity];
    let copied = unsafe {
        CFStringGetCString(
            value,
            buffer.as_mut_ptr().cast::<c_char>(),
            capacity as isize,
            UTF8,
        )
    };
    if copied == 0 {
        return None;
    }

    unsafe { CStr::from_ptr(buffer.as_ptr().cast::<c_char>()) }
        .to_str()
        .ok()
        .map(ToOwned::to_owned)
}

fn cf_dictionary_i32(dictionary: CfTypeRef, key: CfTypeRef) -> Option<i32> {
    let value = cf_dictionary_value(dictionary, key)?;
    if unsafe { CFGetTypeID(value) } != unsafe { CFNumberGetTypeID() } {
        return None;
    }
    let mut result = 0_i32;
    // kCFNumberSInt32Type
    let copied = unsafe { CFNumberGetValue(value, 3, (&mut result as *mut i32).cast()) };
    (copied != 0).then_some(result)
}

type CfTypeRef = *const c_void;
type CfArrayRef = *const c_void;

struct OwnedCf(CfTypeRef);

impl OwnedCf {
    fn new(value: CfTypeRef) -> Option<Self> {
        (!value.is_null()).then_some(Self(value))
    }

    fn as_ptr(&self) -> CfTypeRef {
        self.0
    }
}

impl Drop for OwnedCf {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> CfArrayRef;
    static kCGWindowOwnerName: CfTypeRef;
    static kCGWindowOwnerPID: CfTypeRef;
    static kCGWindowName: CfTypeRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(value: CfTypeRef);
    fn CFGetTypeID(value: CfTypeRef) -> usize;
    fn CFArrayGetCount(array: CfArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CfArrayRef, index: isize) -> CfTypeRef;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFDictionaryGetValueIfPresent(
        dictionary: CfTypeRef,
        key: CfTypeRef,
        value: *mut CfTypeRef,
    ) -> u8;
    fn CFStringGetTypeID() -> usize;
    fn CFStringGetLength(value: CfTypeRef) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCStringPtr(value: CfTypeRef, encoding: u32) -> *const c_char;
    fn CFStringGetCString(
        value: CfTypeRef,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> u8;
    fn CFNumberGetTypeID() -> usize;
    fn CFNumberGetValue(number: CfTypeRef, number_type: isize, value: *mut c_void) -> u8;
}

pub struct MacOsSignalProvider<W = NoWindowContextSource> {
    system: System,
    flags: RuntimeActivityFlags,
    window_context: W,
    started_at: Instant,
    last_context_refresh: Option<Instant>,
    last_audio_refresh: Option<Instant>,
    cached_context: Vec<ObservedApplication>,
    cached_audio: Vec<crate::audio::system_detector::AudioProcessActivity>,
}

const CONTEXT_REFRESH_INTERVAL: Duration = Duration::from_secs(3);
const AUDIO_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

impl MacOsSignalProvider<NoWindowContextSource> {
    pub fn new(flags: RuntimeActivityFlags) -> Self {
        Self::with_window_context(flags, NoWindowContextSource)
    }
}

impl<W: WindowContextSource> MacOsSignalProvider<W> {
    pub fn with_window_context(flags: RuntimeActivityFlags, window_context: W) -> Self {
        Self {
            system: System::new(),
            flags,
            window_context,
            started_at: Instant::now(),
            last_context_refresh: None,
            last_audio_refresh: None,
            cached_context: Vec::new(),
            cached_audio: Vec::new(),
        }
    }

    pub fn activity_flags(&self) -> RuntimeActivityFlags {
        self.flags.clone()
    }
}

impl<W: WindowContextSource> SignalProvider for MacOsSignalProvider<W> {
    type Error = Infallible;

    fn sample(&mut self) -> Result<SignalSnapshot, Self::Error> {
        let now = Instant::now();
        let context_is_stale = self.last_context_refresh.map_or(true, |last| {
            now.duration_since(last) >= CONTEXT_REFRESH_INTERVAL
        });
        if context_is_stale {
            self.system.refresh_processes(ProcessesToUpdate::All, true);

            let mut applications: Vec<_> = self
                .system
                .processes()
                .iter()
                // Do not retain unrelated process names or browser command lines.
                // Browser meeting detection requires explicit window-title evidence.
                .filter(|(_, process)| likely_meeting_process(&process.name().to_string_lossy()))
                .map(|(pid, process)| {
                    let process_name = process.name().to_string_lossy().into_owned();
                    ObservedApplication {
                        process_id: pid_to_i32(*pid),
                        is_active_call_helper: is_active_call_helper(&process_name),
                        process_name,
                        bundle_identifier: None,
                        executable_path: process
                            .exe()
                            .map(|path| path.to_string_lossy().into_owned()),
                        window_title: None,
                        meeting_context: None,
                        is_frontmost: false,
                        is_using_system_audio: false,
                        is_audio_input_active: false,
                        is_audio_output_active: false,
                    }
                })
                .collect();
            applications.extend(self.window_context.observed_windows());
            self.cached_context = applications;
            self.last_context_refresh = Some(now);
        }

        if self.last_audio_refresh.map_or(true, |last| {
            now.duration_since(last) >= AUDIO_REFRESH_INTERVAL
        }) {
            self.cached_audio = crate::audio::system_detector::list_audio_process_activity();
            self.last_audio_refresh = Some(now);
        }

        Ok(SignalSnapshot {
            observed_at: self.started_at.elapsed(),
            applications: correlate_audio(&self.cached_context, &self.cached_audio),
            recording_active: self.flags.recording_active(),
            dictation_active: self.flags.dictation_active(),
        })
    }
}

fn pid_to_i32(pid: Pid) -> Option<i32> {
    i32::try_from(pid.as_u32()).ok()
}

fn correlate_audio(
    context: &[ObservedApplication],
    audio: &[crate::audio::system_detector::AudioProcessActivity],
) -> Vec<ObservedApplication> {
    let mut applications = context.to_vec();

    for activity in audio {
        let (host_name, host_bundle) = host_audio_identity(
            &activity.process_name,
            activity.bundle_identifier.as_deref(),
        );
        let mut matched = false;
        for app in &mut applications {
            let same_host = app.process_id == Some(activity.pid)
                || app.bundle_identifier.as_deref().is_some_and(|bundle| {
                    bundle.eq_ignore_ascii_case(host_bundle.as_deref().unwrap_or_default())
                })
                || app.process_name.eq_ignore_ascii_case(&host_name);
            if same_host {
                matched = true;
                app.is_audio_input_active |= activity.input_active;
                app.is_audio_output_active |= activity.output_active;
                app.is_using_system_audio |= activity.output_active;
            }
        }
        if !matched && likely_meeting_process(&host_name) {
            applications.push(ObservedApplication {
                process_id: Some(activity.pid),
                process_name: host_name,
                bundle_identifier: host_bundle,
                is_using_system_audio: activity.output_active,
                is_audio_input_active: activity.input_active,
                is_audio_output_active: activity.output_active,
                is_active_call_helper: is_active_call_helper(&activity.process_name),
                ..ObservedApplication::default()
            });
        }
    }
    applications
}

fn host_audio_identity(process_name: &str, bundle: Option<&str>) -> (String, Option<String>) {
    let name = process_name.to_lowercase();
    let bundle_lower = bundle.unwrap_or_default().to_lowercase();
    let mapped = if bundle_lower.starts_with("com.google.chrome") || name.contains("chrome helper") {
        Some(("Google Chrome", "com.google.Chrome"))
    } else if bundle_lower.starts_with("com.microsoft.edgemac") || name.contains("edge helper") {
        Some(("Microsoft Edge", "com.microsoft.edgemac"))
    } else if bundle_lower.starts_with("com.brave.browser") || name.contains("brave browser helper") {
        Some(("Brave Browser", "com.brave.Browser"))
    } else if bundle_lower.starts_with("company.thebrowser.browser") || name.contains("arc helper") {
        Some(("Arc", "company.thebrowser.Browser"))
    } else if bundle_lower.starts_with("com.vivaldi.vivaldi") || name.contains("vivaldi helper") {
        Some(("Vivaldi", "com.vivaldi.Vivaldi"))
    } else if bundle_lower.starts_with("com.operasoftware.opera") || name.contains("opera helper") {
        Some(("Opera", "com.operasoftware.Opera"))
    } else if bundle_lower.starts_with("com.microsoft.teams") || name.contains("teams helper") {
        Some(("Microsoft Teams", "com.microsoft.teams2"))
    } else if bundle_lower.starts_with("com.tinyspeck.slackmacgap") || name.contains("slack helper") {
        Some(("Slack", "com.tinyspeck.slackmacgap"))
    } else if bundle_lower.starts_with("com.hnc.discord") || name.contains("discord helper") {
        Some(("Discord", "com.hnc.discord"))
    } else if bundle_lower.starts_with("us.zoom") || is_active_call_helper(process_name) {
        Some(("zoom.us", "us.zoom.xos"))
    } else {
        None
    };
    mapped
        .map(|(host, id)| (host.to_string(), Some(id.to_string())))
        .unwrap_or_else(|| (process_name.to_string(), bundle.map(str::to_string)))
}

fn likely_meeting_process(process_name: &str) -> bool {
    let normalized = process_name.trim().to_lowercase();
    candidate_from_process_name(&normalized).is_some()
        || is_supported_browser(&normalized, "")
        || supported_browser_bundle(&normalized).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::system_detector::AudioProcessActivity;
    use crate::meeting_detection::signals::{classify_meeting_evidence, EvidenceConfidence};

    #[test]
    fn accepts_meeting_titles_from_supported_browsers() {
        let chrome = meeting_window_observation(
            Some(42),
            "Google Chrome",
            Some("Daily sync - Google Meet"),
        )
        .expect("supported Meet window");
        assert_eq!(chrome.process_id, Some(42));
        assert_eq!(chrome.process_name, "Google Chrome");
        assert_eq!(
            chrome.bundle_identifier.as_deref(),
            Some("com.google.Chrome")
        );

        assert!(
            meeting_window_observation(None, "Safari", Some("meet.google.com/abc-defg-hij")).is_some()
        );
        assert!(meeting_window_observation(None, "Firefox", Some("Zoom Meeting")).is_some());
    }

    #[test]
    fn rejects_unrelated_and_unsupported_window_titles() {
        assert!(meeting_window_observation(None, "Google Chrome", Some("Inbox - Gmail")).is_none());
        assert!(meeting_window_observation(None, "TextEdit", Some("Standup - Google Meet")).is_none());
        assert!(meeting_window_observation(None, "Safari", None).is_none());
        assert!(meeting_window_observation(None, "Safari", Some("  ")).is_none());
    }

    #[test]
    fn accepts_filtered_native_meeting_windows() {
        let zoom = meeting_window_observation(None, "zoom.us", Some("Zoom Meeting"))
            .expect("active Zoom meeting window");
        assert_eq!(zoom.process_name, "zoom.us");
        assert!(zoom.window_title.is_some());

        assert!(meeting_window_observation(None, "zoom.us", Some("Zoom Workplace")).is_none());
    }

    #[test]
    fn recognizes_zoom_call_only_helpers() {
        for helper in ["CptHost", "zCCIMeetingHost", "caphost"] {
            assert!(likely_meeting_process(helper));
            assert!(is_active_call_helper(helper));
            assert_eq!(
                candidate_from_process_name(helper),
                Some(super::super::CandidateApp::Zoom)
            );
        }

        assert!(!likely_meeting_process("ZoomUpdater"));
    }

    #[test]
    fn automation_tokens_never_accept_raw_urls() {
        assert_eq!(candidate_from_automation_token("googleMeet"), Some(CandidateApp::GoogleMeet));
        assert_eq!(candidate_from_automation_token("https://meet.google.com/secret"), None);
        let script = browser_classification_script("Safari");
        assert!(script.contains("return \"googleMeet\""));
        assert!(!script.contains("return tabUrl"));
    }

    #[test]
    fn maps_renderer_helpers_to_host_applications() {
        assert_eq!(
            host_audio_identity("Google Chrome Helper (Renderer)", Some("com.google.Chrome.helper")),
            ("Google Chrome".to_string(), Some("com.google.Chrome".to_string()))
        );
        assert_eq!(
            host_audio_identity("Slack Helper", Some("com.tinyspeck.slackmacgap.helper")),
            ("Slack".to_string(), Some("com.tinyspeck.slackmacgap".to_string()))
        );
    }

    #[test]
    fn correlates_browser_context_with_helper_audio() {
        let context = vec![ObservedApplication {
            process_name: "Google Chrome".to_string(),
            bundle_identifier: Some("com.google.Chrome".to_string()),
            meeting_context: Some(CandidateApp::GoogleMeet),
            ..ObservedApplication::default()
        }];
        let audio = vec![AudioProcessActivity {
            pid: 99,
            process_name: "Google Chrome Helper (Renderer)".to_string(),
            bundle_identifier: Some("com.google.Chrome.helper".to_string()),
            input_active: false,
            output_active: true,
        }];
        let evidence = classify_meeting_evidence(&correlate_audio(&context, &audio))
            .expect("meeting evidence");
        assert_eq!(evidence.candidate, CandidateApp::GoogleMeet);
        assert_eq!(evidence.confidence, EvidenceConfidence::High);
    }

    #[test]
    fn simultaneous_browser_input_output_is_high_confidence_without_context() {
        let context = vec![ObservedApplication {
            process_id: Some(10),
            process_name: "Safari".to_string(),
            bundle_identifier: Some("com.apple.Safari".to_string()),
            ..ObservedApplication::default()
        }];
        let audio = vec![AudioProcessActivity {
            pid: 10,
            process_name: "Safari".to_string(),
            bundle_identifier: Some("com.apple.Safari".to_string()),
            input_active: true,
            output_active: true,
        }];
        let evidence = classify_meeting_evidence(&correlate_audio(&context, &audio))
            .expect("browser evidence");
        assert_eq!(evidence.candidate, CandidateApp::BrowserCall);
        assert_eq!(evidence.confidence, EvidenceConfidence::High);
    }

    #[test]
    #[ignore = "requires an interactive macOS window server session"]
    fn system_window_source_returns_only_filtered_meeting_context() {
        let mut source = MacOsWindowContextSource::new();
        for observation in source.observed_windows() {
            assert!(supported_window_owner(&observation.process_name));
            assert!(observation.meeting_context.is_some());
        }
    }
}
