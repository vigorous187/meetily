use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use once_cell::sync::Lazy;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager, Runtime};
use tempfile::TempPath;
use tokio::{io::AsyncReadExt, process::Command};

use super::{DiarizationRange, DiarizationResult};

const TARGET_SAMPLE_RATE: u32 = 16_000;
const MAX_CAPTURE_SECONDS: u64 = 4 * 60 * 60;
const HELPER_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_STDOUT_BYTES: usize = 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_TURNS: usize = 50_000;
const PENDING_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_PENDING_MEETINGS: usize = 16;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const REVIEWED_HELPERS: &[(u64, &str)] = &[
    // Reviewed build input before Tauri applies the bundle's ad-hoc signature.
    (
        23_505_600,
        "78ec589bdd38c8d041d6cf5c49c852022c6d996bdf10ef106bb8376040038001",
    ),
    // The same helper after deterministic entitlement-free hardened-runtime
    // signing by the reviewed release script.
    (
        23_369_056,
        "03d245d0c69d60b6cae1f1b8e41d18bb7a1d1cda073d831735f882186a3f6773",
    ),
];

const MODEL_DIRECTORY: &str = "diarization/sherpa-onnx-1.13.4";
const SEGMENTATION_MODEL: VerifiedModel = VerifiedModel {
    file_name: "pyannote-segmentation-model.onnx",
    size: 5_992_913,
    sha256: "220ad67ca923bef2fa91f2390c786097bf305bceb5e261d4af67b38e938e1079",
};
const EMBEDDING_MODEL: VerifiedModel = VerifiedModel {
    file_name: "3dspeaker_speech_eres2net_base_sv_zh-cn_3dspeaker_16k.onnx",
    size: 39_593_761,
    sha256: "1a331345f04805badbb495c775a6ddffcdd1a732567d5ec8b3d5749e3c7a5e4b",
};

#[derive(Clone)]
pub struct SystemAudioSink {
    inner: Arc<Mutex<WavCaptureInner>>,
}

pub struct SystemAudioCapture {
    temp_path: TempPath,
    sink: SystemAudioSink,
}

struct WavCaptureInner {
    file: Option<File>,
    samples_written: u64,
    source_rate: u32,
    phase: u64,
    bucket_sum: f64,
    bucket_count: u32,
    failed: Option<String>,
}

impl SystemAudioCapture {
    pub fn new() -> Result<Self, String> {
        let temporary = tempfile::Builder::new()
            .prefix("meetily-system-audio-")
            .suffix(".wav")
            .tempfile()
            .map_err(|error| format!("could not create private temporary audio: {error}"))?;
        let (mut file, temp_path) = temporary.into_parts();
        file.write_all(&[0_u8; 44])
            .map_err(|error| format!("could not initialize temporary audio: {error}"))?;
        let inner = WavCaptureInner {
            file: Some(file),
            samples_written: 0,
            source_rate: 0,
            phase: 0,
            bucket_sum: 0.0,
            bucket_count: 0,
            failed: None,
        };
        Ok(Self {
            temp_path,
            sink: SystemAudioSink {
                inner: Arc::new(Mutex::new(inner)),
            },
        })
    }

    pub fn sink(&self) -> SystemAudioSink {
        self.sink.clone()
    }

    fn finalize(&mut self) -> Result<Option<u64>, String> {
        let mut inner = self
            .sink
            .inner
            .lock()
            .map_err(|_| "temporary audio writer lock was poisoned".to_string())?;
        if let Some(error) = inner.failed.take() {
            inner.file.take();
            return Err(error);
        }
        inner.flush_bucket()?;
        let samples_written = inner.samples_written;
        if samples_written == 0 {
            inner.file.take();
            return Ok(None);
        }
        let data_bytes = samples_written
            .checked_mul(2)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| "temporary audio exceeded the WAV size limit".to_string())?;
        let mut file = inner
            .file
            .take()
            .ok_or_else(|| "temporary audio was already finalized".to_string())?;
        write_wav_header(&mut file, data_bytes)?;
        file.flush()
            .map_err(|error| format!("could not flush temporary audio: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("could not sync temporary audio: {error}"))?;
        drop(file);
        Ok(Some(
            samples_written.saturating_mul(1000) / u64::from(TARGET_SAMPLE_RATE),
        ))
    }

    pub async fn finish<R: Runtime>(
        mut self,
        app: &AppHandle<R>,
    ) -> Result<Vec<DiarizationRange>, String> {
        let Some(duration_ms) = self.finalize()? else {
            return Ok(Vec::new());
        };
        run_helper(app, self.temp_path.as_ref(), duration_ms).await
    }
}

impl SystemAudioSink {
    pub fn write_samples(&self, samples: &[f32], source_rate: u32) -> Result<(), String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "temporary audio writer lock was poisoned".to_string())?;
        if inner.failed.is_some() {
            return Ok(());
        }
        if let Err(error) = inner.write_samples(samples, source_rate) {
            inner.failed = Some(error.clone());
            return Err(error);
        }
        Ok(())
    }
}

impl WavCaptureInner {
    fn write_samples(&mut self, samples: &[f32], source_rate: u32) -> Result<(), String> {
        if source_rate == 0 {
            return Err("system audio had a zero sample rate".to_string());
        }
        if source_rate < TARGET_SAMPLE_RATE {
            return Err(format!(
                "system audio sample rate {source_rate} Hz is below the supported 16 kHz minimum"
            ));
        }
        if self.source_rate != source_rate {
            self.flush_bucket()?;
            self.source_rate = source_rate;
            self.phase = 0;
        }
        for &sample in samples {
            if !sample.is_finite() {
                return Err("system audio contained a non-finite sample".to_string());
            }
            self.bucket_sum += f64::from(sample);
            self.bucket_count = self.bucket_count.saturating_add(1);
            self.phase = self.phase.saturating_add(u64::from(TARGET_SAMPLE_RATE));
            if self.phase >= u64::from(source_rate) {
                self.phase -= u64::from(source_rate);
                self.flush_bucket()?;
            }
        }
        Ok(())
    }

    fn flush_bucket(&mut self) -> Result<(), String> {
        if self.bucket_count == 0 {
            return Ok(());
        }
        let maximum_samples = u64::from(TARGET_SAMPLE_RATE) * MAX_CAPTURE_SECONDS;
        if self.samples_written >= maximum_samples {
            return Err("system audio exceeded the four-hour safety limit".to_string());
        }
        let average = (self.bucket_sum / f64::from(self.bucket_count)).clamp(-1.0, 1.0);
        let pcm = (average * f64::from(i16::MAX)).round() as i16;
        self.file
            .as_mut()
            .ok_or_else(|| "temporary audio was already finalized".to_string())?
            .write_all(&pcm.to_le_bytes())
            .map_err(|error| format!("could not write temporary system audio: {error}"))?;
        self.samples_written += 1;
        self.bucket_sum = 0.0;
        self.bucket_count = 0;
        Ok(())
    }
}

fn write_wav_header(file: &mut File, data_bytes: u32) -> Result<(), String> {
    let riff_size = data_bytes
        .checked_add(36)
        .ok_or_else(|| "temporary audio exceeded the WAV size limit".to_string())?;
    let byte_rate = TARGET_SAMPLE_RATE * 2;
    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&riff_size.to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16_u32.to_le_bytes());
    header.extend_from_slice(&1_u16.to_le_bytes());
    header.extend_from_slice(&1_u16.to_le_bytes());
    header.extend_from_slice(&TARGET_SAMPLE_RATE.to_le_bytes());
    header.extend_from_slice(&byte_rate.to_le_bytes());
    header.extend_from_slice(&2_u16.to_le_bytes());
    header.extend_from_slice(&16_u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&data_bytes.to_le_bytes());
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.write_all(&header))
        .map_err(|error| format!("could not finalize temporary audio header: {error}"))
}

#[derive(Clone, Copy)]
struct VerifiedModel {
    file_name: &'static str,
    size: u64,
    sha256: &'static str,
}

async fn run_helper<R: Runtime>(
    app: &AppHandle<R>,
    audio_path: &Path,
    duration_ms: u64,
) -> Result<Vec<DiarizationRange>, String> {
    let model_root = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("could not resolve local app data: {error}"))?
        .join(MODEL_DIRECTORY);
    let segmentation = verify_model(
        model_root.join(SEGMENTATION_MODEL.file_name),
        SEGMENTATION_MODEL,
    )
    .await?;
    let embedding =
        verify_model(model_root.join(EMBEDDING_MODEL.file_name), EMBEDDING_MODEL).await?;
    let helper = tokio::task::spawn_blocking(resolve_helper_path)
        .await
        .map_err(|error| format!("helper verification task failed: {error}"))??;

    let mut child = Command::new(&helper)
        .arg("--audio")
        .arg(audio_path)
        .arg("--segmentation-model")
        .arg(&segmentation)
        .arg("--embedding-model")
        .arg(&embedding)
        .arg("--num-clusters")
        .arg("-1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("could not start local diarization helper: {error}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "helper stdout was unavailable".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "helper stderr was unavailable".to_string())?;
    let stdout_task = tokio::spawn(read_bounded(stdout, MAX_STDOUT_BYTES));
    let stderr_task = tokio::spawn(read_bounded(stderr, MAX_STDERR_BYTES));

    let status = match tokio::time::timeout(HELPER_TIMEOUT, child.wait()).await {
        Ok(result) => {
            result.map_err(|error| format!("local diarization helper failed to run: {error}"))?
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err("local diarization helper exceeded the five-minute limit".to_string());
        }
    };
    let stdout = stdout_task
        .await
        .map_err(|error| format!("helper stdout reader failed: {error}"))??;
    let stderr = stderr_task
        .await
        .map_err(|error| format!("helper stderr reader failed: {error}"))??;
    if !status.success() {
        // Never propagate helper stderr: upstream callers may persist errors in
        // logs, and native libraries can echo local paths or audio metadata.
        return Err(format!(
            "local diarization helper exited unsuccessfully (status: {}; diagnostic bytes suppressed: {})",
            status,
            stderr.len()
        ));
    }
    parse_helper_output(&stdout, duration_ms)
}

async fn read_bounded<R>(mut reader: R, limit: usize) -> Result<Vec<u8>, String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|error| format!("could not read helper output: {error}"))?;
        if read == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(read) > limit {
            return Err(format!(
                "local diarization helper output exceeded {limit} bytes"
            ));
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

async fn verify_model(path: PathBuf, expected: VerifiedModel) -> Result<PathBuf, String> {
    tokio::task::spawn_blocking(move || {
        let canonical = path
            .canonicalize()
            .map_err(|error| format!("required local model is unavailable: {error}"))?;
        let metadata = canonical
            .metadata()
            .map_err(|error| format!("could not inspect local model: {error}"))?;
        if !metadata.is_file() || metadata.len() != expected.size {
            return Err(format!(
                "local model failed size verification: {}",
                expected.file_name
            ));
        }
        let mut file = File::open(&canonical)
            .map_err(|error| format!("could not open local model: {error}"))?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|error| format!("could not read local model: {error}"))?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        let actual = format!("{:x}", hasher.finalize());
        if actual != expected.sha256 {
            return Err(format!(
                "local model failed SHA-256 verification: {}",
                expected.file_name
            ));
        }
        Ok(canonical)
    })
    .await
    .map_err(|error| format!("model verification task failed: {error}"))?
}

fn resolve_helper_path() -> Result<PathBuf, String> {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("MEETILY_DIARIZATION_HELPER") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(
                "MEETILY_DIARIZATION_HELPER must be an absolute development path".to_string(),
            );
        }
        return canonical_file(path, None);
    }

    let executable = std::env::current_exe()
        .map_err(|error| format!("could not resolve application executable: {error}"))?;
    let executable_directory = executable
        .parent()
        .ok_or_else(|| "application executable has no parent directory".to_string())?
        .canonicalize()
        .map_err(|error| format!("could not verify application directory: {error}"))?;
    #[cfg(windows)]
    let file_name = "diarization-helper.exe";
    #[cfg(not(windows))]
    let file_name = "diarization-helper";
    canonical_file(
        executable_directory.join(file_name),
        Some(&executable_directory),
    )
}

fn canonical_file(path: PathBuf, required_parent: Option<&Path>) -> Result<PathBuf, String> {
    let link_metadata = path
        .symlink_metadata()
        .map_err(|error| format!("local diarization helper is unavailable: {error}"))?;
    if link_metadata.file_type().is_symlink() {
        return Err("local diarization helper must not be a symbolic link".to_string());
    }
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("local diarization helper is unavailable: {error}"))?;
    if let Some(parent) = required_parent {
        if canonical.parent() != Some(parent) {
            return Err(
                "local diarization helper resolved outside the application directory".to_string(),
            );
        }
    }
    let metadata = canonical
        .metadata()
        .map_err(|error| format!("could not inspect local diarization helper: {error}"))?;
    if !metadata.is_file() {
        return Err("local diarization helper is not a regular file".to_string());
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    verify_helper_contents(&canonical, &metadata)?;
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    return Err(
        "local diarization helper has no pinned verification record for this platform".to_string(),
    );
    Ok(canonical)
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn verify_helper_contents(path: &Path, metadata: &std::fs::Metadata) -> Result<(), String> {
    let candidates: Vec<_> = REVIEWED_HELPERS
        .iter()
        .filter(|(size, _)| *size == metadata.len())
        .collect();
    if candidates.is_empty() {
        return Err("local diarization helper failed size verification".to_string());
    }
    let mut file = File::open(path)
        .map_err(|error| format!("could not open local diarization helper: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("could not read local diarization helper: {error}"))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let actual_hash = format!("{:x}", hasher.finalize());
    if !candidates
        .iter()
        .any(|(_, expected_hash)| actual_hash == *expected_hash)
    {
        return Err("local diarization helper failed SHA-256 verification".to_string());
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HelperOutput {
    version: u32,
    turns: Vec<HelperTurn>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HelperTurn {
    start_ms: u64,
    end_ms: u64,
    cluster_index: usize,
}

fn parse_helper_output(bytes: &[u8], duration_ms: u64) -> Result<Vec<DiarizationRange>, String> {
    let output: HelperOutput = serde_json::from_slice(bytes)
        .map_err(|error| format!("local diarization helper returned invalid JSON: {error}"))?;
    if output.version != 1 {
        return Err("local diarization helper returned an unsupported output version".to_string());
    }
    if output.turns.len() > MAX_TURNS {
        return Err("local diarization helper returned too many speaker turns".to_string());
    }
    // The helper permits up to one second of model-window padding after the final sample.
    let maximum_end = duration_ms.saturating_add(1_000);
    let mut ranges = Vec::with_capacity(output.turns.len());
    for turn in output.turns {
        if turn.start_ms >= turn.end_ms || turn.end_ms > maximum_end || turn.cluster_index > 255 {
            return Err("local diarization helper returned an invalid speaker turn".to_string());
        }
        ranges.push(DiarizationRange {
            start_ms: turn.start_ms,
            end_ms: turn.end_ms,
            cluster: format!("cluster-{}", turn.cluster_index),
        });
    }
    ranges.sort_by(|left, right| {
        left.start_ms
            .cmp(&right.start_ms)
            .then(left.end_ms.cmp(&right.end_ms))
            .then(left.cluster.cmp(&right.cluster))
    });
    Ok(ranges)
}

struct PendingEntry {
    inserted: Instant,
    ranges: Vec<DiarizationRange>,
}

static PENDING: Lazy<Mutex<HashMap<String, PendingEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub fn store_pending(folder: &Path, ranges: Vec<DiarizationRange>) {
    let Ok(mut pending) = PENDING.lock() else {
        return;
    };
    pending.retain(|_, entry| entry.inserted.elapsed() < PENDING_TTL);
    if pending.len() >= MAX_PENDING_MEETINGS {
        if let Some(oldest) = pending
            .iter()
            .min_by_key(|(_, entry)| entry.inserted)
            .map(|(key, _)| key.clone())
        {
            pending.remove(&oldest);
        }
    }
    pending.insert(
        folder.to_string_lossy().into_owned(),
        PendingEntry {
            inserted: Instant::now(),
            ranges,
        },
    );
}

pub fn pending_for(folder: Option<&str>) -> Vec<DiarizationRange> {
    let Some(folder) = folder else {
        return Vec::new();
    };
    let Ok(mut pending) = PENDING.lock() else {
        return Vec::new();
    };
    pending.retain(|_, entry| entry.inserted.elapsed() < PENDING_TTL);
    pending
        .get(folder)
        .map(|entry| entry.ranges.clone())
        .unwrap_or_default()
}

pub fn discard_pending(folder: Option<&str>) {
    if let (Some(folder), Ok(mut pending)) = (folder, PENDING.lock()) {
        pending.remove(folder);
    }
}

pub async fn process_cached_ranges(
    meeting_id: &str,
    segments: Vec<super::TranscriptSegment>,
    ranges: Vec<DiarizationRange>,
) -> super::JobOutcome {
    struct CachedDiarizer(DiarizationResult);
    #[async_trait::async_trait]
    impl super::Diarizer for CachedDiarizer {
        async fn diarize(
            &self,
            _request: super::DiarizationRequest,
        ) -> Result<DiarizationResult, super::DiarizationError> {
            Ok(self.0.clone())
        }

        fn provider_name(&self) -> &'static str {
            "verified-local-sidecar"
        }
    }

    let manager = super::DiarizationJobManager::new(
        Arc::new(CachedDiarizer(DiarizationResult { ranges })),
        1,
        super::MappingConfig::default(),
    )
    .expect("fixed non-zero diarization capacity");
    manager
        .process(
            super::DiarizationRequest {
                meeting_id: meeting_id.to_string(),
                system_audio: super::LocalAudioInput {
                    samples: vec![0.0],
                    sample_rate_hz: TARGET_SAMPLE_RATE,
                },
            },
            segments,
        )
        .await
        .expect("a fresh one-slot diarization manager cannot be busy")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_strict_valid_output() {
        let output = br#"{"version":1,"turns":[{"start_ms":20,"end_ms":900,"cluster_index":2}]}"#;
        let ranges = parse_helper_output(output, 1_000).unwrap();
        assert_eq!(ranges[0].cluster, "cluster-2");
    }

    #[test]
    fn rejects_unknown_fields_and_out_of_bounds_turns() {
        assert!(
            parse_helper_output(br#"{"version":1,"turns":[],"unexpected":true}"#, 1_000).is_err()
        );
        assert!(parse_helper_output(
            br#"{"version":1,"turns":[{"start_ms":0,"end_ms":2001,"cluster_index":0}]}"#,
            1_000
        )
        .is_err());
    }

    #[test]
    fn writes_mono_16khz_pcm_wav_and_temp_is_removed() {
        let path;
        {
            let mut capture = SystemAudioCapture::new().unwrap();
            path = capture.temp_path.to_path_buf();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(&path).unwrap().permissions().mode() & 0o077,
                    0
                );
            }
            capture
                .sink()
                .write_samples(&vec![0.25; 48_000], 48_000)
                .unwrap();
            let duration = capture.finalize().unwrap().unwrap();
            assert_eq!(duration, 1_000);
            let bytes = std::fs::read(&path).unwrap();
            assert_eq!(&bytes[0..4], b"RIFF");
            assert_eq!(&bytes[8..12], b"WAVE");
            assert_eq!(
                u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
                16_000
            );
            assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 1);
            assert_eq!(bytes.len(), 44 + 32_000);
        }
        assert!(!path.exists());
    }

    #[test]
    fn rejects_audio_below_target_rate_instead_of_mis_resampling() {
        let capture = SystemAudioCapture::new().unwrap();
        let error = capture.sink().write_samples(&[0.0; 8], 8_000).unwrap_err();
        assert!(error.contains("below the supported 16 kHz minimum"));
    }

    #[cfg(all(unix, target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn rejects_symbolic_link_helper_before_canonicalization() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        std::fs::write(&target, b"not a helper").unwrap();
        let link = directory.path().join("helper");
        symlink(&target, &link).unwrap();
        let error = canonical_file(link, None).unwrap_err();
        assert!(error.contains("must not be a symbolic link"));
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn rejects_helper_with_wrong_size() {
        let directory = tempfile::tempdir().unwrap();
        let helper = directory.path().join("helper");
        std::fs::write(&helper, b"not a helper").unwrap();
        let error = canonical_file(helper, None).unwrap_err();
        assert!(error.contains("failed size verification"));
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn accepts_reviewed_packaged_helper() {
        let helper = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("binaries/diarization-helper-aarch64-apple-darwin");
        assert!(canonical_file(helper, None).is_ok());
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    #[ignore = "requires a signed Tauri app bundle"]
    fn accepts_reviewed_signed_bundle_helper() {
        let helper = std::env::var("MEETILY_PACKAGED_DIARIZATION_HELPER")
            .expect("MEETILY_PACKAGED_DIARIZATION_HELPER must name the bundled helper");
        assert!(canonical_file(PathBuf::from(helper), None).is_ok());
    }

    #[tokio::test]
    async fn cached_ranges_flow_through_job_mapping() {
        let outcome = process_cached_ranges(
            "meeting-a",
            vec![super::super::TranscriptSegment {
                id: "remote".to_string(),
                meeting_id: "meeting-a".to_string(),
                text: "hello".to_string(),
                start_ms: 0,
                end_ms: 500,
                source: super::super::AudioSource::System,
            }],
            vec![DiarizationRange {
                start_ms: 0,
                end_ms: 600,
                cluster: "cluster-4".to_string(),
            }],
        )
        .await;
        assert_eq!(outcome.segments[0].speaker.name, "Speaker 1");
        assert!(matches!(
            outcome.state,
            super::super::DiarizationJobState::Completed { .. }
        ));
    }
}
