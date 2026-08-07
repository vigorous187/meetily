use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioSource {
    Microphone,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub id: String,
    pub meeting_id: String,
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub source: AudioSource,
}

impl TranscriptSegment {
    pub fn midpoint_ms(&self) -> u64 {
        midpoint(self.start_ms, self.end_ms)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiarizationRange {
    pub start_ms: u64,
    pub end_ms: u64,
    /// Engine-local label. It is converted to a deterministic Meetily speaker ID.
    pub cluster: String,
}

impl DiarizationRange {
    pub fn midpoint_ms(&self) -> u64 {
        midpoint(self.start_ms, self.end_ms)
    }

    pub fn is_valid(&self) -> bool {
        self.start_ms <= self.end_ms && !self.cluster.trim().is_empty()
    }
}

fn midpoint(start_ms: u64, end_ms: u64) -> u64 {
    start_ms.saturating_add(end_ms.saturating_sub(start_ms) / 2)
}

#[derive(Debug, Clone)]
pub struct LocalAudioInput {
    /// Mono, normalized PCM samples. The adapter validates that every sample is finite.
    pub samples: Vec<f32>,
    pub sample_rate_hz: u32,
}

#[derive(Debug, Clone)]
pub struct DiarizationRequest {
    pub meeting_id: String,
    /// System audio only. Microphone segments are always assigned to `You` directly.
    pub system_audio: LocalAudioInput,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiarizationResult {
    pub ranges: Vec<DiarizationRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerKind {
    You,
    Identified,
    RemoteFallback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Speaker {
    /// Stable for the same meeting and engine cluster, but not across meetings.
    pub id: String,
    pub name: String,
    pub kind: SpeakerKind,
    pub cluster: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabeledTranscriptSegment {
    pub id: String,
    pub meeting_id: String,
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub source: AudioSource,
    pub speaker: Speaker,
}

impl LabeledTranscriptSegment {
    pub(crate) fn from_raw(segment: TranscriptSegment, speaker: Speaker) -> Self {
        Self {
            id: segment.id,
            meeting_id: segment.meeting_id,
            text: segment.text,
            start_ms: segment.start_ms,
            end_ms: segment.end_ms,
            source: segment.source,
            speaker,
        }
    }
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum DiarizationError {
    #[error("local diarization is unavailable: {0}")]
    Unavailable(String),
    #[error("invalid diarization input: {0}")]
    InvalidInput(String),
    #[error("local diarization engine failed: {0}")]
    Engine(String),
}
