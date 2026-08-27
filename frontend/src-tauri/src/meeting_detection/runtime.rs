use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_store::StoreExt;

use super::{
    AutoCaptureCoordinator, AutoCaptureError, AutoCaptureHealth, AutoCaptureStatusChanged,
    CoordinatorAction, DetectorEvent, LocalMeetingDetector, MeetingDetector,
};

const AUTO_CAPTURE_PREFERENCE: &str = "automatic_meeting_detection_enabled";

static RUNNING: AtomicBool = AtomicBool::new(false);
static DISMISS_REQUESTED: AtomicBool = AtomicBool::new(false);
static WORKER: LazyLock<Mutex<Option<DetectorWorker>>> = LazyLock::new(|| Mutex::new(None));
static COORDINATOR: LazyLock<Mutex<AutoCaptureCoordinator>> =
    LazyLock::new(|| Mutex::new(AutoCaptureCoordinator::default()));

struct DetectorWorker {
    stop: Arc<AtomicBool>,
    owner_thread: JoinHandle<()>,
}

enum RuntimeOutcome {
    Started {
        meeting_session_id: String,
        result: Result<crate::recording_session::StartReceipt, AutoCaptureError>,
    },
    Stopped {
        meeting_session_id: String,
        recording_id: String,
        result: Result<(), AutoCaptureError>,
    },
}

/// Start from Tauri setup when the persisted opt-in is enabled. This is
/// independent of React mounting, navigation, and window visibility.
pub fn initialize_auto_capture<R: Runtime>(app: AppHandle<R>) {
    let enabled = app
        .store("preferences.json")
        .ok()
        .and_then(|store| store.get(AUTO_CAPTURE_PREFERENCE))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if !enabled {
        return;
    }
    if let Ok(mut coordinator) = COORDINATOR.lock() {
        coordinator.set_enabled(true);
    }
    if let Err(error) = start_detector_worker(app.clone()) {
        mark_worker_stopped(&app, Some(error));
    }
}

/// Compatibility command retained for older frontends. New clients use
/// `set_auto_capture_enabled`, which also persists the preference.
#[tauri::command]
pub fn start_meeting_detection<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    COORDINATOR
        .lock()
        .map_err(|_| "Automatic capture state is unavailable".to_string())?
        .set_enabled(true);
    start_detector_worker(app)
}

#[tauri::command]
pub async fn stop_meeting_detection<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    let action = COORDINATOR
        .lock()
        .map_err(|_| "Automatic capture state is unavailable".to_string())?
        .set_enabled(false);
    if let Some(action) = action {
        execute_action_inline(action, app.clone()).await;
    }
    stop_detector_worker()
}

#[tauri::command]
pub fn dismiss_meeting_detection() {
    DISMISS_REQUESTED.store(true, Ordering::SeqCst);
}

#[tauri::command]
pub fn is_meeting_detection_running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

#[tauri::command]
pub fn get_auto_capture_health() -> Result<AutoCaptureHealth, String> {
    let mut coordinator = COORDINATOR
        .lock()
        .map_err(|_| "Automatic capture state is unavailable".to_string())?;
    coordinator.set_detector_running(RUNNING.load(Ordering::Acquire));
    Ok(coordinator.health())
}

#[tauri::command]
pub async fn set_auto_capture_enabled<R: Runtime>(
    app: AppHandle<R>,
    enabled: bool,
) -> Result<AutoCaptureHealth, String> {
    let store = app.store("preferences.json").map_err(|error| error.to_string())?;
    store.set(AUTO_CAPTURE_PREFERENCE, serde_json::Value::Bool(enabled));
    store.save().map_err(|error| error.to_string())?;

    if enabled {
        let launch_status = super::autostart::set_launch_at_login(app.clone(), true);
        if !launch_status.enabled {
            log::warn!(
                "Automatic capture enabled but launch at login could not be enabled: {}",
                launch_status.message
            );
        }
        COORDINATOR
            .lock()
            .map_err(|_| "Automatic capture state is unavailable".to_string())?
            .set_enabled(true);
        start_detector_worker(app.clone())?;
    } else {
        let action = COORDINATOR
            .lock()
            .map_err(|_| "Automatic capture state is unavailable".to_string())?
            .set_enabled(false);
        if let Some(action) = action {
            execute_action_inline(action, app.clone()).await;
        }
        stop_detector_worker()?;
    }
    emit_status(&app);
    get_auto_capture_health()
}

#[tauri::command]
pub fn notify_auto_capture_readiness_changed<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    let action = COORDINATOR
        .lock()
        .map_err(|_| "Automatic capture state is unavailable".to_string())?
        .readiness_changed();
    if let Some(action) = action {
        let action_app = app.clone();
        tauri::async_runtime::spawn(async move {
            execute_action_inline(action, action_app).await;
        });
    }
    emit_status(&app);
    Ok(())
}

fn start_detector_worker<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
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
        let worker_app = app.clone();
        let owner_thread = std::thread::Builder::new()
            .name("meetily-auto-capture".to_string())
            .spawn(move || run_macos_detector_supervised(worker_app, thread_stop))
            .map_err(|error| {
                RUNNING.store(false, Ordering::SeqCst);
                format!("Could not start automatic capture: {error}")
            })?;
        *worker = Some(DetectorWorker { stop, owner_thread });
        mark_worker_running(&app);
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        RUNNING.store(false, Ordering::SeqCst);
        return Err("Automatic meeting detection is currently available on macOS.".to_string());
    }
    Ok(())
}

fn stop_detector_worker() -> Result<(), String> {
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
    if let Ok(mut coordinator) = COORDINATOR.lock() {
        coordinator.set_detector_running(false);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn run_macos_detector_supervised<R: Runtime>(app: AppHandle<R>, stop: Arc<AtomicBool>) {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use super::diagnostics::{record, JournalEntry, JournalEvent};

    record(JournalEntry::new(JournalEvent::DetectorStarted));
    while RUNNING.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
        let result = catch_unwind(AssertUnwindSafe(|| {
            run_macos_detector(app.clone(), stop.clone())
        }));
        if result.is_ok() || !RUNNING.load(Ordering::Acquire) || stop.load(Ordering::Acquire) {
            break;
        }
        record(
            JournalEntry::new(JournalEvent::WorkerRestarted)
                .error_code("detector_worker_panicked"),
        );
        std::thread::sleep(Duration::from_secs(2));
    }
    RUNNING.store(false, Ordering::Release);
    record(JournalEntry::new(JournalEvent::DetectorStopped));
    mark_worker_stopped(&app, None);
}

#[cfg(target_os = "macos")]
fn run_macos_detector<R: Runtime>(app: AppHandle<R>, stop: Arc<AtomicBool>) {
    use super::{MacOsSignalProvider, MacOsWindowContextSource, RuntimeActivityFlags};

    let flags = RuntimeActivityFlags::default();
    let provider =
        MacOsSignalProvider::with_window_context(flags.clone(), MacOsWindowContextSource::new());
    let mut detector = LocalMeetingDetector::new(provider, Default::default());
    let started_at = Instant::now();
    let (outcome_tx, outcome_rx) = mpsc::channel();

    while RUNNING.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
        drain_outcomes(&app, &outcome_tx, &outcome_rx, started_at.elapsed());

        let recording_active = crate::audio::recording_commands::is_recording_active();
        let dictation_active = crate::dictation::is_active();
        flags.set_recording_active(recording_active);
        flags.set_dictation_active(dictation_active);
        if DISMISS_REQUESTED.swap(false, Ordering::SeqCst) {
            detector.dismiss_prompt();
        }

        let event = match detector.poll() {
            Ok(event) => event,
            Err(error) => match error {},
        };
        let had_event = event.is_some();
        let action = match event {
            Some(DetectorEvent::MeetingStarted { candidate }) => COORDINATOR
                .lock()
                .ok()
                .and_then(|mut coordinator| {
                    coordinator.meeting_started(candidate, recording_active, dictation_active)
                }),
            Some(DetectorEvent::MeetingEnded { candidate }) => COORDINATOR
                .lock()
                .ok()
                .and_then(|mut coordinator| coordinator.meeting_ended(candidate)),
            None => COORDINATOR.lock().ok().and_then(|mut coordinator| {
                coordinator.tick(started_at.elapsed(), recording_active, dictation_active)
            }),
        };
        if had_event || action.is_some() {
            emit_status(&app);
        }
        if let Some(action) = action {
            dispatch_action(app.clone(), outcome_tx.clone(), action);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn drain_outcomes<R: Runtime>(
    app: &AppHandle<R>,
    sender: &Sender<RuntimeOutcome>,
    receiver: &Receiver<RuntimeOutcome>,
    now: Duration,
) {
    while let Ok(outcome) = receiver.try_recv() {
        let follow_up = match outcome {
            RuntimeOutcome::Started {
                meeting_session_id,
                result,
            } => {
                let mut coordinator = match COORDINATOR.lock() {
                    Ok(coordinator) => coordinator,
                    Err(_) => continue,
                };
                match result {
                    Ok(receipt) => coordinator.start_succeeded(&meeting_session_id, receipt),
                    Err(error) => {
                        coordinator.start_failed(&meeting_session_id, error, now);
                        None
                    }
                }
            }
            RuntimeOutcome::Stopped {
                meeting_session_id,
                recording_id,
                result,
            } => {
                if let Ok(mut coordinator) = COORDINATOR.lock() {
                    coordinator.stop_finished(&meeting_session_id, &recording_id, result);
                }
                None
            }
        };
        emit_status(app);
        if let Some(action) = follow_up {
            dispatch_action(app.clone(), sender.clone(), action);
        }
    }
}

fn dispatch_action<R: Runtime>(
    app: AppHandle<R>,
    sender: Sender<RuntimeOutcome>,
    action: CoordinatorAction,
) {
    tauri::async_runtime::spawn(async move {
        let outcome = execute_action(action, app.clone()).await;
        if let Err(mpsc::SendError(outcome)) = sender.send(outcome) {
            // The detector may have been disabled while recorder startup was
            // in flight. Apply the acknowledgement anyway so a late success is
            // conditionally stopped instead of leaking an unowned recording.
            apply_detached_outcome(app, outcome).await;
        }
    });
}

async fn apply_detached_outcome<R: Runtime>(app: AppHandle<R>, outcome: RuntimeOutcome) {
    let follow_up = match outcome {
        RuntimeOutcome::Started {
            meeting_session_id,
            result,
        } => {
            let mut coordinator = match COORDINATOR.lock() {
                Ok(coordinator) => coordinator,
                Err(_) => return,
            };
            match result {
                Ok(receipt) => coordinator.start_succeeded(&meeting_session_id, receipt),
                Err(error) => {
                    coordinator.start_failed(&meeting_session_id, error, Duration::ZERO);
                    None
                }
            }
        }
        RuntimeOutcome::Stopped {
            meeting_session_id,
            recording_id,
            result,
        } => {
            if let Ok(mut coordinator) = COORDINATOR.lock() {
                coordinator.stop_finished(&meeting_session_id, &recording_id, result);
            }
            None
        }
    };
    if let Some(CoordinatorAction::Stop {
        meeting_session_id,
        recording_id,
    }) = follow_up
    {
        // A detached follow-up can only be the conditional cleanup of a late
        // successful start. There is intentionally no detached retry loop.
        let result = crate::recording_session::stop_automatic(
            app.clone(),
            &meeting_session_id,
            &recording_id,
        )
        .await
        .map_err(recording_error);
        if let Ok(mut coordinator) = COORDINATOR.lock() {
            coordinator.stop_finished(&meeting_session_id, &recording_id, result);
        }
    }
    emit_status(&app);
}

async fn execute_action_inline<R: Runtime>(action: CoordinatorAction, app: AppHandle<R>) {
    match execute_action(action, app.clone()).await {
        RuntimeOutcome::Started {
            meeting_session_id,
            result,
        } => {
            if let Ok(mut coordinator) = COORDINATOR.lock() {
                match result {
                    Ok(receipt) => {
                        coordinator.start_succeeded(&meeting_session_id, receipt);
                    }
                    Err(error) => {
                        coordinator.start_failed(&meeting_session_id, error, Duration::ZERO)
                    }
                }
            }
        }
        RuntimeOutcome::Stopped {
            meeting_session_id,
            recording_id,
            result,
        } => {
            if let Ok(mut coordinator) = COORDINATOR.lock() {
                coordinator.stop_finished(&meeting_session_id, &recording_id, result);
            }
        }
    }
    emit_status(&app);
}

async fn execute_action<R: Runtime>(
    action: CoordinatorAction,
    app: AppHandle<R>,
) -> RuntimeOutcome {
    match action {
        CoordinatorAction::Start {
            meeting_session_id,
            candidate,
            ..
        } => {
            let result = crate::recording_session::start_automatic(
                app.clone(),
                meeting_session_id.clone(),
                candidate.display_name().to_string(),
            )
            .await
            .map_err(recording_error);
            if let Ok(receipt) = &result {
                crate::tray::update_tray_menu(&app);
                let state = app.state::<
                    crate::notifications::commands::NotificationManagerState<R>,
                >();
                if let Err(error) =
                    crate::notifications::commands::show_recording_started_notification(
                        &app,
                        &state,
                        Some(candidate.display_name().to_string()),
                    )
                    .await
                {
                    log::warn!("Automatic recording started but notification failed: {error}");
                }
                log::info!("Automatic recording acknowledged: {}", receipt.recording_id);
            }
            RuntimeOutcome::Started {
                meeting_session_id,
                result,
            }
        }
        CoordinatorAction::Stop {
            meeting_session_id,
            recording_id,
        } => {
            let result = crate::recording_session::stop_automatic(
                app,
                &meeting_session_id,
                &recording_id,
            )
            .await
            .map_err(recording_error);
            RuntimeOutcome::Stopped {
                meeting_session_id,
                recording_id,
                result,
            }
        }
    }
}

fn recording_error(error: crate::recording_session::RecordingSessionError) -> AutoCaptureError {
    AutoCaptureError {
        code: error.code,
        message: error.message,
        transient: error.transient,
    }
}

fn mark_worker_running<R: Runtime>(app: &AppHandle<R>) {
    if let Ok(mut coordinator) = COORDINATOR.lock() {
        coordinator.set_detector_running(true);
    }
    emit_status(app);
}

fn mark_worker_stopped<R: Runtime>(app: &AppHandle<R>, error: Option<String>) {
    if let Ok(mut coordinator) = COORDINATOR.lock() {
        coordinator.set_detector_running(false);
    }
    if let Some(error) = error {
        log::error!("Automatic capture detector failed: {error}");
    }
    emit_status(app);
}

fn emit_status<R: Runtime>(app: &AppHandle<R>) {
    let status: Option<AutoCaptureStatusChanged> =
        COORDINATOR.lock().ok().map(|coordinator| coordinator.status());
    if let Some(status) = status {
        if let Err(error) = app.emit("auto-capture-status-changed", status) {
            log::warn!("Unable to emit automatic capture status: {error}");
        }
    }
}
