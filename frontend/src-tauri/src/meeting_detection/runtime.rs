use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Runtime};

use super::{DetectorEvent, LocalMeetingDetector, MeetingDetector};

static RUNNING: AtomicBool = AtomicBool::new(false);
static DISMISS_REQUESTED: AtomicBool = AtomicBool::new(false);
static WORKER: LazyLock<Mutex<Option<DetectorWorker>>> = LazyLock::new(|| Mutex::new(None));

struct DetectorWorker {
    stop: Arc<AtomicBool>,
    owner_thread: JoinHandle<()>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MeetingLifecyclePayload {
    application: String,
}

#[tauri::command]
pub fn start_meeting_detection<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    let mut worker = WORKER
        .lock()
        .map_err(|_| "Meeting detector state is unavailable".to_string())?;

    if worker
        .as_ref()
        .is_some_and(|worker| !worker.owner_thread.is_finished())
    {
        return Ok(());
    }
    if let Some(stale) = worker.take() {
        let _ = stale.owner_thread.join();
    }

    if RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let owner_thread = std::thread::Builder::new()
            .name("meetily-meeting-detector".to_string())
            .spawn(move || run_macos_detector(app, thread_stop))
            .map_err(|error| {
                RUNNING.store(false, Ordering::SeqCst);
                format!("Could not start meeting detection: {error}")
            })?;
        *worker = Some(DetectorWorker { stop, owner_thread });
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        RUNNING.store(false, Ordering::SeqCst);
        return Err("Automatic meeting detection is currently available on macOS.".to_string());
    }

    Ok(())
}

#[tauri::command]
pub fn stop_meeting_detection() -> Result<(), String> {
    RUNNING.store(false, Ordering::SeqCst);
    let worker = WORKER
        .lock()
        .map_err(|_| "Meeting detector state is unavailable".to_string())?
        .take();
    if let Some(worker) = worker {
        worker.stop.store(true, Ordering::Release);
        worker
            .owner_thread
            .join()
            .map_err(|_| "Meeting detector thread terminated unexpectedly".to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub fn dismiss_meeting_detection() {
    DISMISS_REQUESTED.store(true, Ordering::SeqCst);
}

#[tauri::command]
pub fn is_meeting_detection_running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

#[cfg(target_os = "macos")]
fn run_macos_detector<R: Runtime>(app: AppHandle<R>, stop: Arc<AtomicBool>) {
    use super::{MacOsSignalProvider, MacOsWindowContextSource, RuntimeActivityFlags};

    let flags = RuntimeActivityFlags::default();
    let provider =
        MacOsSignalProvider::with_window_context(flags.clone(), MacOsWindowContextSource::new());
    let mut detector = LocalMeetingDetector::new(provider, Default::default());

    while RUNNING.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
        let recording_active = crate::audio::recording_commands::is_recording_active();
        let dictation_active = crate::dictation::is_active();
        flags.set_recording_active(recording_active);
        flags.set_dictation_active(dictation_active);
        if DISMISS_REQUESTED.swap(false, Ordering::SeqCst) {
            detector.dismiss_prompt();
        }

        match detector.poll() {
            Ok(Some(DetectorEvent::MeetingStarted { candidate })) => {
                let _ = app.emit(
                    "meeting-detected",
                    MeetingLifecyclePayload {
                        application: candidate.display_name().to_string(),
                    },
                );
            }
            Ok(Some(DetectorEvent::MeetingEnded { candidate })) => {
                let _ = app.emit(
                    "meeting-ended",
                    MeetingLifecyclePayload {
                        application: candidate.display_name().to_string(),
                    },
                );
            }
            Ok(None) => {}
            Err(error) => match error {},
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    RUNNING.store(false, Ordering::Release);
}
