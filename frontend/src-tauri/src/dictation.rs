//! Local push-to-talk dictation using Meetily's configured transcription model.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, Stream, StreamConfig};
use serde::Serialize;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tauri::{AppHandle, Runtime};

const MAX_DICTATION_SECONDS: usize = 300;
const CAPTURE_ACTIVE: u8 = 0;
const CAPTURE_STOPPED: u8 = 1;
const CAPTURE_TIMED_OUT: u8 = 2;

struct DictationSession {
    stop_tx: SyncSender<()>,
    owner_thread: JoinHandle<()>,
    samples: Arc<Mutex<Vec<f32>>>,
    sample_rate: u32,
    capture_state: Arc<AtomicU8>,
}

static SESSION: LazyLock<Mutex<Option<DictationSession>>> = LazyLock::new(|| Mutex::new(None));

pub fn is_active() -> bool {
    SESSION.lock().is_ok_and(|session| session.is_some())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DictationResult {
    pub text: String,
    pub copied_to_clipboard: bool,
    pub pasted: bool,
}

#[tauri::command]
pub async fn start_dictation() -> Result<(), String> {
    // Serialize the recording/dictation decision with meeting-recording startup.
    // Recording holds this same guard until IS_RECORDING is committed.
    let _capture_guard = crate::audio::common::acquire_engine_lifecycle_lock().await;
    if crate::audio::recording_commands::is_recording().await {
        return Err("Stop the meeting recording before starting dictation.".to_string());
    }
    let mut session = SESSION
        .lock()
        .map_err(|_| "Dictation state is unavailable")?;
    if session.is_some() {
        return Ok(());
    }

    let samples = Arc::new(Mutex::new(Vec::new()));
    let (stop_tx, stop_rx) = std::sync::mpsc::sync_channel(1);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let owner_samples = samples.clone();
    let capture_state = Arc::new(AtomicU8::new(CAPTURE_ACTIVE));
    let owner_state = capture_state.clone();
    let owner_thread = std::thread::Builder::new()
        .name("meetily-dictation-capture".to_string())
        .spawn(move || run_capture_owner(owner_samples, owner_state, ready_tx, stop_rx))
        .map_err(|error| format!("Could not start the dictation capture thread: {error}"))?;

    let sample_rate = match ready_rx.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(Ok(sample_rate)) => sample_rate,
        Ok(Err(error)) => {
            let _ = owner_thread.join();
            return Err(error);
        }
        Err(error) => {
            let _ = stop_tx.send(());
            // Do not let an unresponsive device API defeat the startup timeout.
            // Once the receiver is dropped, a late ready notification makes the
            // owner return and drop any stream it managed to create.
            drop(owner_thread);
            return Err(format!("Dictation microphone startup timed out: {error}"));
        }
    };
    *session = Some(DictationSession {
        stop_tx,
        owner_thread,
        samples,
        sample_rate,
        capture_state,
    });
    Ok(())
}

#[tauri::command]
pub async fn stop_dictation<R: Runtime>(app: AppHandle<R>) -> Result<DictationResult, String> {
    let session = SESSION
        .lock()
        .map_err(|_| "Dictation state is unavailable")?
        .take()
        .ok_or("Dictation is not active")?;
    let (mut samples, sample_rate) = tokio::task::spawn_blocking(move || finish_capture(session))
        .await
        .map_err(|error| format!("Dictation capture task failed: {error}"))??;
    if samples.len() < sample_rate as usize / 4 {
        zeroize_samples(&mut samples);
        return Err("Hold the shortcut a little longer and speak clearly.".to_string());
    }
    let audio = if sample_rate == 16_000 {
        std::mem::take(&mut samples)
    } else {
        let audio = crate::audio::audio_processing::resample_audio(&samples, sample_rate, 16_000);
        zeroize_samples(&mut samples);
        audio
    };

    let engine =
        crate::audio::transcription::engine::get_or_init_transcription_engine(&app).await?;
    let text = match engine {
        crate::audio::transcription::engine::TranscriptionEngine::Whisper(engine) => engine
            .transcribe_audio(audio, crate::get_language_preference_internal())
            .await
            .map_err(|error| error.to_string())?,
        crate::audio::transcription::engine::TranscriptionEngine::Parakeet(engine) => engine
            .transcribe_audio(audio)
            .await
            .map_err(|error| error.to_string())?,
        crate::audio::transcription::engine::TranscriptionEngine::Provider(_) => {
            return Err(
                "Dictation requires a downloaded local Whisper or Parakeet model; cloud transcription is disabled for privacy."
                    .to_string(),
            );
        }
    };
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err("No speech was detected.".to_string());
    }

    // Dictation is copy-only by default. Automatically issuing a synthetic paste
    // can target a different application if focus changes while transcription is
    // running and also requires broad macOS Accessibility permission.
    let copied_to_clipboard = copy_to_clipboard(&text);
    let pasted = false;
    Ok(DictationResult {
        text,
        copied_to_clipboard,
        pasted,
    })
}

/// Abandon an in-flight session without retaining or transcribing captured audio.
/// This is called when the UI provider unmounts (including app shutdown/reload).
#[tauri::command]
pub async fn cancel_dictation() -> Result<(), String> {
    let session = SESSION
        .lock()
        .map_err(|_| "Dictation state is unavailable")?
        .take();
    if let Some(session) = session {
        tokio::task::spawn_blocking(move || abandon_capture(session))
            .await
            .map_err(|error| format!("Dictation cleanup task failed: {error}"))?;
    }
    Ok(())
}

/// CPAL deliberately makes its platform stream non-Send because some backends
/// have thread-affine APIs. Create, operate, and drop the stream on this one
/// owner thread; only sample data and control messages cross thread boundaries.
fn run_capture_owner(
    samples: Arc<Mutex<Vec<f32>>>,
    capture_state: Arc<AtomicU8>,
    ready_tx: SyncSender<Result<u32, String>>,
    stop_rx: Receiver<()>,
) {
    match open_capture_stream(samples.clone()) {
        Ok((stream, sample_rate)) => {
            if ready_tx.send(Ok(sample_rate)).is_err() {
                clear_samples(&samples);
                return;
            }
            if stop_rx
                .recv_timeout(Duration::from_secs(MAX_DICTATION_SECONDS as u64))
                .is_ok()
            {
                capture_state.store(CAPTURE_STOPPED, Ordering::Release);
            } else {
                // Enforce a hard lifetime even if the shortcut release event is
                // lost. Clear private audio immediately before dropping CPAL.
                capture_state.store(CAPTURE_TIMED_OUT, Ordering::Release);
                clear_samples(&samples);
            }
            drop(stream);
        }
        Err(error) => {
            let _ = ready_tx.send(Err(error));
        }
    }
}

fn open_capture_stream(samples: Arc<Mutex<Vec<f32>>>) -> Result<(Stream, u32), String> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or("No microphone is available")?;
    let supported = device
        .default_input_config()
        .map_err(|error| format!("Could not read the default microphone configuration: {error}"))?;
    let channels = supported.channels();
    let sample_rate = supported.sample_rate().0;
    let sample_format = supported.sample_format();
    let config = StreamConfig {
        channels,
        sample_rate: supported.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };
    let maximum_samples = sample_rate as usize * MAX_DICTATION_SECONDS;
    let error_callback = |error| log::error!("Dictation microphone stream error: {error}");

    let stream = match sample_format {
        SampleFormat::F32 => {
            let target = samples.clone();
            device.build_input_stream(
                &config,
                move |data: &[f32], _| append_mono(data, channels, &target, maximum_samples),
                error_callback,
                None,
            )
        }
        SampleFormat::I16 => {
            let target = samples.clone();
            device.build_input_stream(
                &config,
                move |data: &[i16], _| {
                    let mut data: Vec<f32> = data.iter().map(|sample| sample.to_sample()).collect();
                    append_mono(&data, channels, &target, maximum_samples);
                    zeroize_samples(&mut data);
                },
                error_callback,
                None,
            )
        }
        SampleFormat::U16 => {
            let target = samples;
            device.build_input_stream(
                &config,
                move |data: &[u16], _| {
                    let mut data: Vec<f32> = data.iter().map(|sample| sample.to_sample()).collect();
                    append_mono(&data, channels, &target, maximum_samples);
                    zeroize_samples(&mut data);
                },
                error_callback,
                None,
            )
        }
        format => return Err(format!("Unsupported microphone sample format: {format:?}")),
    }
    .map_err(|error| format!("Could not open the microphone for dictation: {error}"))?;

    stream
        .play()
        .map_err(|error| format!("Could not start dictation capture: {error}"))?;
    Ok((stream, sample_rate))
}

fn finish_capture(session: DictationSession) -> Result<(Vec<f32>, u32), String> {
    let DictationSession {
        stop_tx,
        owner_thread,
        samples,
        sample_rate,
        capture_state,
    } = session;
    let _ = stop_tx.send(());
    let join_result = owner_thread
        .join()
        .map_err(|_| "Dictation capture thread panicked".to_string());
    join_result?;
    if capture_state.load(Ordering::Acquire) == CAPTURE_TIMED_OUT {
        clear_samples(&samples);
        return Err("Dictation stopped after the five-minute safety limit.".to_string());
    }
    let samples = Arc::try_unwrap(samples)
        .map_err(|_| "Dictation audio is still in use".to_string())?
        .into_inner()
        .map_err(|_| "Dictation audio could not be read".to_string())?;
    Ok((samples, sample_rate))
}

fn abandon_capture(session: DictationSession) {
    let DictationSession {
        stop_tx,
        owner_thread,
        samples,
        ..
    } = session;
    let _ = stop_tx.send(());
    let _ = owner_thread.join();
    clear_samples(&samples);
}

fn clear_samples(samples: &Arc<Mutex<Vec<f32>>>) {
    if let Ok(mut samples) = samples.lock() {
        zeroize_samples(&mut samples);
        samples.clear();
        samples.shrink_to_fit();
    }
}

pub(crate) fn zeroize_owned_audio(samples: &mut [f32]) {
    // Volatile writes prevent privacy cleanup from being elided as a dead store
    // immediately before the allocation is released.
    for sample in samples {
        unsafe { std::ptr::write_volatile(sample, 0.0) };
    }
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
}

pub(crate) struct ZeroizingAudio(Vec<f32>);

impl ZeroizingAudio {
    pub(crate) fn new(samples: Vec<f32>) -> Self {
        Self(samples)
    }
}

impl std::ops::Deref for ZeroizingAudio {
    type Target = [f32];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for ZeroizingAudio {
    fn drop(&mut self) {
        zeroize_owned_audio(&mut self.0);
    }
}

fn zeroize_samples(samples: &mut [f32]) {
    zeroize_owned_audio(samples);
}

fn append_mono(data: &[f32], channels: u16, target: &Arc<Mutex<Vec<f32>>>, maximum: usize) {
    let Ok(mut samples) = target.lock() else {
        return;
    };
    if samples.len() >= maximum || channels == 0 {
        return;
    }
    let remaining = maximum - samples.len();
    samples.extend(
        data.chunks(channels as usize)
            .take(remaining)
            .map(|frame| frame.iter().copied().sum::<f32>() / frame.len() as f32),
    );
}

fn copy_to_clipboard(text: &str) -> bool {
    let Ok(mut child) = Command::new("/usr/bin/pbcopy")
        .stdin(Stdio::piped())
        .spawn()
    else {
        return false;
    };
    let Some(stdin) = child.stdin.as_mut() else {
        return false;
    };
    if stdin.write_all(text.as_bytes()).is_err() {
        return false;
    }
    child.wait().is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clearing_samples_zeroizes_and_releases_buffer() {
        let samples = Arc::new(Mutex::new(vec![0.25, -0.5, 1.0]));
        clear_samples(&samples);
        let samples = samples.lock().unwrap();
        assert!(samples.is_empty());
        assert_eq!(samples.capacity(), 0);
    }
}
