use std::{
    env,
    ffi::OsString,
    fmt,
    fs::{self, File},
    io::{self, BufReader, Read},
    path::{Path, PathBuf},
    process::ExitCode,
};

use serde::Serialize;
use sha2::{Digest, Sha256};

const OUTPUT_VERSION: u8 = 1;
const SAMPLE_RATE_HZ: u32 = 16_000;
const MAX_AUDIO_SECONDS: u64 = 4 * 60 * 60;
const MAX_AUDIO_SAMPLES: u64 = SAMPLE_RATE_HZ as u64 * MAX_AUDIO_SECONDS;
const MAX_NUM_CLUSTERS: i32 = 64;
const MAX_TURNS: usize = 100_000;

const SEGMENTATION_SIZE: u64 = 5_992_913;
const SEGMENTATION_SHA256: &str =
    "220ad67ca923bef2fa91f2390c786097bf305bceb5e261d4af67b38e938e1079";
const EMBEDDING_SIZE: u64 = 39_593_761;
const EMBEDDING_SHA256: &str = "1a331345f04805badbb495c775a6ddffcdd1a732567d5ec8b3d5749e3c7a5e4b";

const USAGE: &str = "usage: diarization-helper --audio <16k-mono-wav> \
--segmentation-model <verified-model.onnx> --embedding-model <verified-model.onnx> \
--num-clusters <-1|1..64>";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cli {
    audio: PathBuf,
    segmentation_model: PathBuf,
    embedding_model: PathBuf,
    num_clusters: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorCode {
    Usage,
    InvalidPath,
    InvalidModel,
    InvalidAudio,
    Engine,
    Output,
}

impl ErrorCode {
    fn exit_code(self) -> u8 {
        match self {
            Self::Usage => 2,
            Self::InvalidPath | Self::InvalidModel | Self::InvalidAudio => 3,
            Self::Engine => 4,
            Self::Output => 5,
        }
    }
}

#[derive(Debug)]
struct HelperError {
    code: ErrorCode,
    message: String,
}

impl HelperError {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for HelperError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

#[derive(Debug)]
struct ValidatedInputs {
    audio: AudioData,
    #[cfg_attr(not(feature = "sherpa"), allow(dead_code))]
    segmentation_model: PathBuf,
    #[cfg_attr(not(feature = "sherpa"), allow(dead_code))]
    embedding_model: PathBuf,
    num_clusters: i32,
}

#[derive(Debug)]
struct AudioData {
    samples: Vec<f32>,
    duration_ms: u64,
}

#[derive(Debug, Clone, Copy)]
struct RawTurn {
    start_seconds: f32,
    end_seconds: f32,
    cluster_index: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Turn {
    start_ms: u64,
    end_ms: u64,
    cluster_index: u32,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct Response {
    version: u8,
    turns: Vec<Turn>,
}

fn main() -> ExitCode {
    if env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--help")) {
        eprintln!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    match run(env::args_os().skip(1)) {
        Ok(response) => match serde_json::to_writer(io::stdout().lock(), &response) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("failed to write JSON response: {error}");
                ExitCode::from(ErrorCode::Output.exit_code())
            }
        },
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(error.code.exit_code())
        }
    }
}

fn run(arguments: impl Iterator<Item = OsString>) -> Result<Response, HelperError> {
    let cli = parse_cli(arguments)?;
    let inputs = validate_inputs(cli)?;
    let raw_turns = run_engine(&inputs)?;
    let turns = validate_and_sort_turns(raw_turns, inputs.audio.duration_ms, inputs.num_clusters)?;
    Ok(Response {
        version: OUTPUT_VERSION,
        turns,
    })
}

fn parse_cli(arguments: impl Iterator<Item = OsString>) -> Result<Cli, HelperError> {
    let mut audio = None;
    let mut segmentation_model = None;
    let mut embedding_model = None;
    let mut num_clusters = None;
    let mut arguments = arguments.peekable();

    while let Some(flag) = arguments.next() {
        let target = match flag.to_str() {
            Some("--audio") => &mut audio,
            Some("--segmentation-model") => &mut segmentation_model,
            Some("--embedding-model") => &mut embedding_model,
            Some("--num-clusters") => {
                if num_clusters.is_some() {
                    return Err(usage_error("--num-clusters was provided more than once"));
                }
                let value = arguments
                    .next()
                    .ok_or_else(|| usage_error("--num-clusters requires a value"))?;
                let value = value
                    .to_str()
                    .ok_or_else(|| usage_error("--num-clusters is not valid UTF-8"))?
                    .parse::<i32>()
                    .map_err(|_| usage_error("--num-clusters must be an integer"))?;
                if value != -1 && !(1..=MAX_NUM_CLUSTERS).contains(&value) {
                    return Err(usage_error(format!(
                        "--num-clusters must be -1 or between 1 and {MAX_NUM_CLUSTERS}"
                    )));
                }
                num_clusters = Some(value);
                continue;
            }
            Some(other) => return Err(usage_error(format!("unknown argument: {other}"))),
            None => return Err(usage_error("argument name is not valid UTF-8")),
        };

        if target.is_some() {
            return Err(usage_error(format!(
                "{} was provided more than once",
                flag.to_string_lossy()
            )));
        }
        *target = Some(PathBuf::from(arguments.next().ok_or_else(|| {
            usage_error(format!("{} requires a path", flag.to_string_lossy()))
        })?));
    }

    Ok(Cli {
        audio: audio.ok_or_else(|| usage_error("missing --audio"))?,
        segmentation_model: segmentation_model
            .ok_or_else(|| usage_error("missing --segmentation-model"))?,
        embedding_model: embedding_model.ok_or_else(|| usage_error("missing --embedding-model"))?,
        num_clusters: num_clusters.ok_or_else(|| usage_error("missing --num-clusters"))?,
    })
}

fn usage_error(message: impl Into<String>) -> HelperError {
    HelperError::new(ErrorCode::Usage, format!("{}\n{USAGE}", message.into()))
}

fn validate_inputs(cli: Cli) -> Result<ValidatedInputs, HelperError> {
    let audio_path = canonical_regular_file(&cli.audio, "audio")?;
    let segmentation_model = canonical_regular_file(&cli.segmentation_model, "segmentation model")?;
    let embedding_model = canonical_regular_file(&cli.embedding_model, "embedding model")?;

    verify_file(
        &segmentation_model,
        SEGMENTATION_SIZE,
        SEGMENTATION_SHA256,
        "segmentation model",
    )?;
    verify_file(
        &embedding_model,
        EMBEDDING_SIZE,
        EMBEDDING_SHA256,
        "embedding model",
    )?;

    Ok(ValidatedInputs {
        audio: read_validated_wav(&audio_path)?,
        segmentation_model,
        embedding_model,
        num_clusters: cli.num_clusters,
    })
}

fn canonical_regular_file(path: &Path, label: &str) -> Result<PathBuf, HelperError> {
    let canonical = fs::canonicalize(path).map_err(|error| {
        HelperError::new(
            ErrorCode::InvalidPath,
            format!("cannot resolve {label} path: {error}"),
        )
    })?;
    let metadata = fs::metadata(&canonical).map_err(|error| {
        HelperError::new(
            ErrorCode::InvalidPath,
            format!("cannot inspect {label} path: {error}"),
        )
    })?;
    if !metadata.is_file() {
        return Err(HelperError::new(
            ErrorCode::InvalidPath,
            format!("{label} path is not a regular file"),
        ));
    }
    Ok(canonical)
}

fn verify_file(
    path: &Path,
    expected_size: u64,
    expected_sha256: &str,
    label: &str,
) -> Result<(), HelperError> {
    let metadata = fs::metadata(path).map_err(|error| {
        HelperError::new(
            ErrorCode::InvalidModel,
            format!("cannot inspect {label}: {error}"),
        )
    })?;
    if metadata.len() != expected_size {
        return Err(HelperError::new(
            ErrorCode::InvalidModel,
            format!(
                "{label} has size {}, expected {expected_size}",
                metadata.len()
            ),
        ));
    }

    let file = File::open(path).map_err(|error| {
        HelperError::new(
            ErrorCode::InvalidModel,
            format!("cannot open {label}: {error}"),
        )
    })?;
    let mut reader = BufReader::new(file);
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer).map_err(|error| {
            HelperError::new(
                ErrorCode::InvalidModel,
                format!("cannot hash {label}: {error}"),
            )
        })?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let actual = format!("{:x}", digest.finalize());
    if actual != expected_sha256 {
        return Err(HelperError::new(
            ErrorCode::InvalidModel,
            format!("{label} failed SHA-256 verification"),
        ));
    }
    Ok(())
}

fn read_validated_wav(path: &Path) -> Result<AudioData, HelperError> {
    let mut reader = hound::WavReader::open(path).map_err(|error| {
        HelperError::new(
            ErrorCode::InvalidAudio,
            format!("cannot read WAV header: {error}"),
        )
    })?;
    let spec = reader.spec();
    if spec.channels != 1
        || spec.sample_rate != SAMPLE_RATE_HZ
        || spec.bits_per_sample != 16
        || spec.sample_format != hound::SampleFormat::Int
    {
        return Err(HelperError::new(
            ErrorCode::InvalidAudio,
            format!(
                "audio must be mono 16-bit PCM at {SAMPLE_RATE_HZ} Hz; got {} channel(s), {} Hz, {}-bit {:?}",
                spec.channels, spec.sample_rate, spec.bits_per_sample, spec.sample_format
            ),
        ));
    }

    let sample_count = u64::from(reader.duration());
    if sample_count == 0 {
        return Err(HelperError::new(
            ErrorCode::InvalidAudio,
            "audio contains no samples",
        ));
    }
    if sample_count > i32::MAX as u64 {
        return Err(HelperError::new(
            ErrorCode::InvalidAudio,
            "audio exceeds the sherpa-onnx i32 sample limit",
        ));
    }
    if sample_count > MAX_AUDIO_SAMPLES {
        return Err(HelperError::new(
            ErrorCode::InvalidAudio,
            format!("audio exceeds the {MAX_AUDIO_SECONDS}-second safety limit"),
        ));
    }

    let capacity = usize::try_from(sample_count).map_err(|_| {
        HelperError::new(
            ErrorCode::InvalidAudio,
            "audio sample count does not fit in memory on this platform",
        )
    })?;
    let mut samples = Vec::with_capacity(capacity);
    for sample in reader.samples::<i16>() {
        let sample = sample.map_err(|error| {
            HelperError::new(
                ErrorCode::InvalidAudio,
                format!("invalid PCM sample data: {error}"),
            )
        })?;
        samples.push(f32::from(sample) / 32_768.0);
    }
    if samples.len() != capacity {
        return Err(HelperError::new(
            ErrorCode::InvalidAudio,
            "WAV sample count does not match its header",
        ));
    }

    let duration_ms = sample_count
        .saturating_mul(1_000)
        .div_ceil(SAMPLE_RATE_HZ as u64);
    Ok(AudioData {
        samples,
        duration_ms,
    })
}

#[cfg(feature = "sherpa")]
fn run_engine(inputs: &ValidatedInputs) -> Result<Vec<RawTurn>, HelperError> {
    use sherpa_onnx::{
        FastClusteringConfig, OfflineSpeakerDiarization, OfflineSpeakerDiarizationConfig,
        OfflineSpeakerSegmentationModelConfig, OfflineSpeakerSegmentationPyannoteModelConfig,
        SpeakerEmbeddingExtractorConfig,
    };

    let config = OfflineSpeakerDiarizationConfig {
        segmentation: OfflineSpeakerSegmentationModelConfig {
            pyannote: OfflineSpeakerSegmentationPyannoteModelConfig {
                model: Some(inputs.segmentation_model.to_string_lossy().into_owned()),
            },
            num_threads: 1,
            debug: false,
            provider: Some("cpu".to_string()),
        },
        embedding: SpeakerEmbeddingExtractorConfig {
            model: Some(inputs.embedding_model.to_string_lossy().into_owned()),
            num_threads: 1,
            debug: false,
            provider: Some("cpu".to_string()),
        },
        clustering: FastClusteringConfig {
            num_clusters: inputs.num_clusters,
            threshold: 0.5,
        },
        min_duration_on: 0.3,
        min_duration_off: 0.5,
    };

    let diarizer = OfflineSpeakerDiarization::create(&config).ok_or_else(|| {
        HelperError::new(ErrorCode::Engine, "failed to initialize local diarization")
    })?;
    if diarizer.sample_rate() != SAMPLE_RATE_HZ as i32 {
        return Err(HelperError::new(
            ErrorCode::Engine,
            format!(
                "diarization model expects {} Hz, not {SAMPLE_RATE_HZ} Hz",
                diarizer.sample_rate()
            ),
        ));
    }

    let result = diarizer.process(&inputs.audio.samples).ok_or_else(|| {
        HelperError::new(ErrorCode::Engine, "local diarization processing failed")
    })?;
    let turns = result.sort_by_start_time();
    if turns.len() > MAX_TURNS {
        return Err(HelperError::new(
            ErrorCode::Output,
            format!("engine returned more than {MAX_TURNS} turns"),
        ));
    }
    Ok(turns
        .into_iter()
        .map(|turn| RawTurn {
            start_seconds: turn.start,
            end_seconds: turn.end,
            cluster_index: turn.speaker,
        })
        .collect())
}

#[cfg(not(feature = "sherpa"))]
fn run_engine(_inputs: &ValidatedInputs) -> Result<Vec<RawTurn>, HelperError> {
    Err(HelperError::new(
        ErrorCode::Engine,
        "helper was built without the sherpa feature",
    ))
}

fn validate_and_sort_turns(
    raw_turns: Vec<RawTurn>,
    audio_duration_ms: u64,
    requested_num_clusters: i32,
) -> Result<Vec<Turn>, HelperError> {
    if raw_turns.len() > MAX_TURNS {
        return Err(HelperError::new(
            ErrorCode::Output,
            format!("engine returned more than {MAX_TURNS} turns"),
        ));
    }

    let maximum_end_ms = audio_duration_ms.saturating_add(1_000);
    let mut turns = Vec::with_capacity(raw_turns.len());
    for raw in raw_turns {
        if !raw.start_seconds.is_finite()
            || !raw.end_seconds.is_finite()
            || raw.start_seconds < 0.0
            || raw.end_seconds < raw.start_seconds
        {
            return Err(HelperError::new(
                ErrorCode::Output,
                "engine returned an invalid time range",
            ));
        }
        if raw.cluster_index < 0 || raw.cluster_index >= MAX_NUM_CLUSTERS {
            return Err(HelperError::new(
                ErrorCode::Output,
                "engine returned an out-of-range speaker index",
            ));
        }
        if requested_num_clusters > 0 && raw.cluster_index >= requested_num_clusters {
            return Err(HelperError::new(
                ErrorCode::Output,
                "engine returned a speaker index outside --num-clusters",
            ));
        }

        let start_ms = seconds_to_ms(raw.start_seconds)?;
        let end_ms = seconds_to_ms(raw.end_seconds)?;
        if end_ms > maximum_end_ms {
            return Err(HelperError::new(
                ErrorCode::Output,
                "engine returned a range beyond the audio duration",
            ));
        }
        turns.push(Turn {
            start_ms,
            end_ms,
            cluster_index: raw.cluster_index as u32,
        });
    }

    turns.sort_by(|left, right| {
        left.start_ms
            .cmp(&right.start_ms)
            .then(left.end_ms.cmp(&right.end_ms))
            .then(left.cluster_index.cmp(&right.cluster_index))
    });
    Ok(turns)
}

fn seconds_to_ms(seconds: f32) -> Result<u64, HelperError> {
    let milliseconds = f64::from(seconds) * 1_000.0;
    if !milliseconds.is_finite() || milliseconds < 0.0 || milliseconds > u64::MAX as f64 {
        return Err(HelperError::new(
            ErrorCode::Output,
            "engine returned an unrepresentable timestamp",
        ));
    }
    Ok(milliseconds.round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn args(values: &[&str]) -> impl Iterator<Item = OsString> {
        values
            .iter()
            .map(|value| OsString::from(*value))
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn parses_complete_cli() {
        let cli = parse_cli(args(&[
            "--audio",
            "/tmp/audio.wav",
            "--segmentation-model",
            "/tmp/segment.onnx",
            "--embedding-model",
            "/tmp/embed.onnx",
            "--num-clusters",
            "-1",
        ]))
        .unwrap();
        assert_eq!(cli.audio, PathBuf::from("/tmp/audio.wav"));
        assert_eq!(cli.num_clusters, -1);
    }

    #[test]
    fn rejects_missing_duplicate_and_unbounded_arguments() {
        assert_eq!(parse_cli(args(&[])).unwrap_err().code, ErrorCode::Usage);
        assert_eq!(
            parse_cli(args(&[
                "--audio",
                "a",
                "--audio",
                "b",
                "--segmentation-model",
                "s",
                "--embedding-model",
                "e",
                "--num-clusters",
                "-1",
            ]))
            .unwrap_err()
            .code,
            ErrorCode::Usage
        );
        assert_eq!(
            parse_cli(args(&[
                "--audio",
                "a",
                "--segmentation-model",
                "s",
                "--embedding-model",
                "e",
                "--num-clusters",
                "65",
            ]))
            .unwrap_err()
            .code,
            ErrorCode::Usage
        );
    }

    #[test]
    fn verifies_exact_file_size_and_hash() {
        let file = NamedTempFile::new().unwrap();
        fs::write(file.path(), b"verified bytes").unwrap();
        let expected = format!("{:x}", Sha256::digest(b"verified bytes"));
        verify_file(file.path(), 14, &expected, "test model").unwrap();
        assert_eq!(
            verify_file(file.path(), 13, &expected, "test model")
                .unwrap_err()
                .code,
            ErrorCode::InvalidModel
        );
        assert_eq!(
            verify_file(file.path(), 14, &"0".repeat(64), "test model")
                .unwrap_err()
                .code,
            ErrorCode::InvalidModel
        );
    }

    #[test]
    fn accepts_only_mono_16khz_16bit_pcm_wav() {
        let valid = write_wav(hound::WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE_HZ,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        });
        let audio = read_validated_wav(valid.path()).unwrap();
        assert_eq!(audio.samples.len(), 160);
        assert_eq!(audio.duration_ms, 10);

        let stereo = write_wav(hound::WavSpec {
            channels: 2,
            sample_rate: SAMPLE_RATE_HZ,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        });
        assert_eq!(
            read_validated_wav(stereo.path()).unwrap_err().code,
            ErrorCode::InvalidAudio
        );
    }

    fn write_wav(spec: hound::WavSpec) -> NamedTempFile {
        let file = NamedTempFile::new().unwrap();
        let mut writer = hound::WavWriter::new(file.reopen().unwrap(), spec).unwrap();
        let samples = if spec.channels == 1 { 160 } else { 320 };
        for _ in 0..samples {
            writer.write_sample(0_i16).unwrap();
        }
        writer.finalize().unwrap();
        file
    }

    #[test]
    fn sorts_and_rounds_valid_turns_deterministically() {
        let turns = validate_and_sort_turns(
            vec![
                RawTurn {
                    start_seconds: 1.0,
                    end_seconds: 2.0,
                    cluster_index: 1,
                },
                RawTurn {
                    start_seconds: 0.0006,
                    end_seconds: 0.9996,
                    cluster_index: 0,
                },
            ],
            2_000,
            -1,
        )
        .unwrap();
        assert_eq!(
            turns,
            vec![
                Turn {
                    start_ms: 1,
                    end_ms: 1_000,
                    cluster_index: 0,
                },
                Turn {
                    start_ms: 1_000,
                    end_ms: 2_000,
                    cluster_index: 1,
                },
            ]
        );
    }

    #[test]
    fn rejects_invalid_engine_ranges_and_clusters() {
        for raw in [
            RawTurn {
                start_seconds: -1.0,
                end_seconds: 1.0,
                cluster_index: 0,
            },
            RawTurn {
                start_seconds: 2.0,
                end_seconds: 1.0,
                cluster_index: 0,
            },
            RawTurn {
                start_seconds: 0.0,
                end_seconds: 1.0,
                cluster_index: -1,
            },
            RawTurn {
                start_seconds: 0.0,
                end_seconds: 3.1,
                cluster_index: 0,
            },
        ] {
            assert_eq!(
                validate_and_sort_turns(vec![raw], 2_000, -1)
                    .unwrap_err()
                    .code,
                ErrorCode::Output
            );
        }
    }

    #[test]
    fn emits_exact_versioned_json_shape() {
        let response = Response {
            version: OUTPUT_VERSION,
            turns: vec![Turn {
                start_ms: 0,
                end_ms: 1_000,
                cluster_index: 0,
            }],
        };
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"version":1,"turns":[{"start_ms":0,"end_ms":1000,"cluster_index":0}]}"#
        );
    }
}
