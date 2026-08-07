//! Local-only speaker diarization domain and orchestration.
//!
//! This module deliberately has no database, Tauri, or network dependencies. Audio is
//! supplied by the caller and every engine implementation must run locally.

mod adapter;
mod domain;
mod jobs;
mod mapping;
pub mod runtime;

pub use adapter::{LocalSpeakerTurn, SherpaOnnxAdapter, SherpaOnnxBackend};
pub use domain::{
    AudioSource, DiarizationError, DiarizationRange, DiarizationRequest, DiarizationResult,
    LabeledTranscriptSegment, LocalAudioInput, Speaker, SpeakerKind, TranscriptSegment,
};
pub use jobs::{DiarizationJobManager, DiarizationJobState, JobManagerError, JobOutcome};
pub use mapping::{map_speakers, MappedTranscript, MappingConfig};

use async_trait::async_trait;

/// A local speaker-diarization provider.
///
/// Implementations receive in-memory audio and must not upload it or depend on a remote
/// service. The trait is intentionally independent of sherpa-onnx so an adapter can be
/// integrated without leaking engine-specific types into the rest of the application.
#[async_trait]
pub trait Diarizer: Send + Sync {
    async fn diarize(
        &self,
        request: DiarizationRequest,
    ) -> Result<DiarizationResult, DiarizationError>;

    fn provider_name(&self) -> &'static str;
}
