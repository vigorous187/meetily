use async_trait::async_trait;

use super::{
    DiarizationError, DiarizationRange, DiarizationRequest, DiarizationResult, Diarizer,
    LocalAudioInput,
};

/// Minimal output expected from a future sherpa-onnx Rust binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSpeakerTurn {
    pub start_ms: u64,
    pub end_ms: u64,
    pub cluster_index: usize,
}

/// Boundary around the actual sherpa-onnx model/session.
///
/// A concrete implementation should own the local segmentation and embedding sessions,
/// load models from verified local paths, and return engine cluster indices. It must not
/// accept URLs or perform downloads.
#[async_trait]
pub trait SherpaOnnxBackend: Send + Sync {
    async fn analyze(
        &self,
        audio: &LocalAudioInput,
    ) -> Result<Vec<LocalSpeakerTurn>, DiarizationError>;

    fn is_available(&self) -> bool;
}

/// Validating adapter that keeps sherpa-onnx-specific behavior out of the domain layer.
pub struct SherpaOnnxAdapter<B> {
    backend: B,
}

impl<B> SherpaOnnxAdapter<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }
}

#[async_trait]
impl<B> Diarizer for SherpaOnnxAdapter<B>
where
    B: SherpaOnnxBackend,
{
    async fn diarize(
        &self,
        request: DiarizationRequest,
    ) -> Result<DiarizationResult, DiarizationError> {
        if !self.backend.is_available() {
            return Err(DiarizationError::Unavailable(
                "sherpa-onnx models are not installed".to_string(),
            ));
        }

        validate_audio(&request.system_audio)?;

        let turns = self.backend.analyze(&request.system_audio).await?;
        let mut ranges = Vec::with_capacity(turns.len());
        for turn in turns {
            if turn.start_ms > turn.end_ms {
                return Err(DiarizationError::Engine(format!(
                    "engine returned an invalid range: {}..{}",
                    turn.start_ms, turn.end_ms
                )));
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

        Ok(DiarizationResult { ranges })
    }

    fn provider_name(&self) -> &'static str {
        "sherpa-onnx-local"
    }
}

fn validate_audio(audio: &LocalAudioInput) -> Result<(), DiarizationError> {
    if audio.sample_rate_hz == 0 {
        return Err(DiarizationError::InvalidInput(
            "sample rate must be greater than zero".to_string(),
        ));
    }
    if audio.samples.is_empty() {
        return Err(DiarizationError::InvalidInput(
            "system audio is empty".to_string(),
        ));
    }
    if audio.samples.iter().any(|sample| !sample.is_finite()) {
        return Err(DiarizationError::InvalidInput(
            "system audio contains a non-finite sample".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Backend {
        available: bool,
        turns: Vec<LocalSpeakerTurn>,
    }

    #[async_trait]
    impl SherpaOnnxBackend for Backend {
        async fn analyze(
            &self,
            _audio: &LocalAudioInput,
        ) -> Result<Vec<LocalSpeakerTurn>, DiarizationError> {
            Ok(self.turns.clone())
        }

        fn is_available(&self) -> bool {
            self.available
        }
    }

    fn request(samples: Vec<f32>) -> DiarizationRequest {
        DiarizationRequest {
            meeting_id: "meeting-a".to_string(),
            system_audio: LocalAudioInput {
                samples,
                sample_rate_hz: 16_000,
            },
        }
    }

    #[tokio::test]
    async fn unavailable_backend_fails_without_processing_audio() {
        let adapter = SherpaOnnxAdapter::new(Backend {
            available: false,
            turns: Vec::new(),
        });

        let error = adapter.diarize(request(vec![0.0])).await.unwrap_err();
        assert!(matches!(error, DiarizationError::Unavailable(_)));
    }

    #[tokio::test]
    async fn validates_audio_before_calling_backend() {
        let adapter = SherpaOnnxAdapter::new(Backend {
            available: true,
            turns: Vec::new(),
        });

        let error = adapter.diarize(request(vec![f32::NAN])).await.unwrap_err();
        assert!(matches!(error, DiarizationError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn converts_and_sorts_local_engine_turns() {
        let adapter = SherpaOnnxAdapter::new(Backend {
            available: true,
            turns: vec![
                LocalSpeakerTurn {
                    start_ms: 2_000,
                    end_ms: 3_000,
                    cluster_index: 1,
                },
                LocalSpeakerTurn {
                    start_ms: 0,
                    end_ms: 1_000,
                    cluster_index: 0,
                },
            ],
        });

        let result = adapter.diarize(request(vec![0.0])).await.unwrap();
        assert_eq!(result.ranges[0].cluster, "cluster-0");
        assert_eq!(result.ranges[1].cluster, "cluster-1");
    }
}
