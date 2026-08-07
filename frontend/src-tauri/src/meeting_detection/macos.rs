//! macOS signal adapter.
//!
//! Process discovery uses the already-present `sysinfo` dependency. Audio,
//! recording, dictation, and optional window-title evidence are supplied by
//! small integration interfaces rather than inspected through private APIs.

use std::collections::HashSet;
use std::convert::Infallible;
use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sysinfo::{ProcessesToUpdate, System};

use super::signals::{
    candidate_from_meeting_title, candidate_from_process_name, is_active_call_helper,
    is_supported_browser, ObservedApplication, SignalProvider, SignalSnapshot,
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

/// Reads the public Core Graphics window list without capturing window images.
///
/// macOS may omit window titles when Screen Recording access has not already
/// been granted. That is treated as an empty observation: this adapter never
/// requests the permission and never interrupts the user with a system prompt.
#[derive(Default)]
pub struct MacOsWindowContextSource;

impl MacOsWindowContextSource {
    pub fn new() -> Self {
        Self
    }
}

impl WindowContextSource for MacOsWindowContextSource {
    fn observed_windows(&mut self) -> Vec<ObservedApplication> {
        meeting_windows()
    }
}

fn meeting_window_observation(
    owner_name: &str,
    window_title: Option<&str>,
) -> Option<ObservedApplication> {
    let window_title = window_title?.trim();
    if window_title.is_empty() || candidate_from_meeting_title(window_title).is_none() {
        return None;
    }

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
        process_name: owner_name.trim().to_string(),
        bundle_identifier,
        executable_path: None,
        window_title: Some(window_title.to_string()),
        // Core Graphics orders on-screen windows front-to-back, but a window
        // list alone does not establish which application is active. Avoid
        // inferring foreground state and let native clients retain precedence.
        is_frontmost: false,
        is_using_system_audio: false,
        is_active_call_helper: false,
    })
}

fn supported_browser_bundle(owner_name: &str) -> Option<&'static str> {
    match owner_name.trim().to_lowercase().as_str() {
        "google chrome" | "chrome" => Some("com.google.Chrome"),
        "safari" => Some("com.apple.Safari"),
        "microsoft edge" => Some("com.microsoft.edgemac"),
        "brave browser" => Some("com.brave.Browser"),
        "arc" => Some("company.thebrowser.Browser"),
        "firefox" => Some("org.mozilla.firefox"),
        "vivaldi" => Some("com.vivaldi.Vivaldi"),
        "opera" => Some("com.operasoftware.Opera"),
        "orion" => Some("com.kagi.kagimacOS"),
        _ => None,
    }
}

fn supported_window_owner(owner_name: &str) -> bool {
    supported_browser_bundle(owner_name).is_some()
        || candidate_from_process_name(owner_name).is_some()
}

fn meeting_windows() -> Vec<ObservedApplication> {
    // On-screen windows only; no image capture and no permission preflight.
    const ON_SCREEN_ONLY: u32 = 1 << 0;
    const EXCLUDE_DESKTOP_ELEMENTS: u32 = 1 << 4;

    let window_array =
        unsafe { CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP_ELEMENTS, 0) };
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

        let title = cf_dictionary_string(dictionary, unsafe { kCGWindowName });
        if let Some(observation) = meeting_window_observation(&owner_name, title.as_deref()) {
            observations.push(observation);
        }
    }

    observations
}

fn cf_dictionary_string(dictionary: CfTypeRef, key: CfTypeRef) -> Option<String> {
    if key.is_null() {
        return None;
    }

    let mut value: CfTypeRef = std::ptr::null();
    let present = unsafe { CFDictionaryGetValueIfPresent(dictionary, key, &mut value) };
    if present == 0
        || value.is_null()
        || unsafe { CFGetTypeID(value) } != unsafe { CFStringGetTypeID() }
    {
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
}

pub struct MacOsSignalProvider<W = NoWindowContextSource> {
    system: System,
    flags: RuntimeActivityFlags,
    window_context: W,
    started_at: Instant,
    last_context_refresh: Option<Instant>,
    cached_applications: Vec<ObservedApplication>,
}

const CONTEXT_REFRESH_INTERVAL: Duration = Duration::from_secs(3);

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
            cached_applications: Vec::new(),
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

            let audio_candidates: HashSet<_> =
                crate::audio::system_detector::list_system_audio_using_apps()
                    .into_iter()
                    .filter_map(|name| candidate_from_process_name(&name))
                    .collect();

            let mut applications: Vec<_> = self
                .system
                .processes()
                .values()
                // Do not retain unrelated process names or browser command lines.
                // Browser meeting detection requires explicit window-title evidence.
                .filter(|process| likely_meeting_process(&process.name().to_string_lossy()))
                .map(|process| {
                    let process_name = process.name().to_string_lossy().into_owned();
                    let candidate = candidate_from_process_name(&process_name);
                    ObservedApplication {
                        is_active_call_helper: is_active_call_helper(&process_name),
                        process_name,
                        bundle_identifier: None,
                        executable_path: process
                            .exe()
                            .map(|path| path.to_string_lossy().into_owned()),
                        window_title: None,
                        is_frontmost: false,
                        is_using_system_audio: candidate
                            .is_some_and(|candidate| audio_candidates.contains(&candidate)),
                    }
                })
                .collect();
            applications.extend(self.window_context.observed_windows());
            self.cached_applications = applications;
            self.last_context_refresh = Some(now);
        }

        Ok(SignalSnapshot {
            observed_at: self.started_at.elapsed(),
            applications: self.cached_applications.clone(),
            recording_active: self.flags.recording_active(),
            dictation_active: self.flags.dictation_active(),
        })
    }
}

fn likely_meeting_process(process_name: &str) -> bool {
    let normalized = process_name.trim().to_lowercase();
    candidate_from_process_name(&normalized).is_some() || is_supported_browser(&normalized, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_meeting_titles_from_supported_browsers() {
        let chrome = meeting_window_observation("Google Chrome", Some("Daily sync - Google Meet"))
            .expect("supported Meet window");
        assert_eq!(chrome.process_name, "Google Chrome");
        assert_eq!(
            chrome.bundle_identifier.as_deref(),
            Some("com.google.Chrome")
        );

        assert!(
            meeting_window_observation("Safari", Some("meet.google.com/abc-defg-hij")).is_some()
        );
        assert!(meeting_window_observation("Firefox", Some("Zoom Meeting")).is_some());
    }

    #[test]
    fn rejects_unrelated_and_unsupported_window_titles() {
        assert!(meeting_window_observation("Google Chrome", Some("Inbox - Gmail")).is_none());
        assert!(meeting_window_observation("TextEdit", Some("Standup - Google Meet")).is_none());
        assert!(meeting_window_observation("Safari", None).is_none());
        assert!(meeting_window_observation("Safari", Some("  ")).is_none());
    }

    #[test]
    fn accepts_filtered_native_meeting_windows() {
        let zoom = meeting_window_observation("zoom.us", Some("Zoom Meeting"))
            .expect("active Zoom meeting window");
        assert_eq!(zoom.process_name, "zoom.us");
        assert!(zoom.window_title.is_some());

        assert!(meeting_window_observation("zoom.us", Some("Zoom Workplace")).is_none());
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
    #[ignore = "requires an interactive macOS window server session"]
    fn system_window_source_returns_only_filtered_meeting_context() {
        let mut source = MacOsWindowContextSource::new();
        for observation in source.observed_windows() {
            assert!(supported_window_owner(&observation.process_name));
            assert!(observation
                .window_title
                .as_deref()
                .and_then(candidate_from_meeting_title)
                .is_some());
        }
    }
}
