use std::{collections::HashMap, sync::Arc};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{RwLock, Semaphore};

use super::{
    map_speakers, DiarizationRequest, Diarizer, LabeledTranscriptSegment, MappingConfig, Speaker,
    TranscriptSegment,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DiarizationJobState {
    Queued,
    Running,
    Completed {
        speaker_count: usize,
        suppressed_echo_segments: usize,
    },
    Fallback {
        reason: String,
        suppressed_echo_segments: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobOutcome {
    pub segments: Vec<LabeledTranscriptSegment>,
    pub speakers: Vec<Speaker>,
    pub state: DiarizationJobState,
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum JobManagerError {
    #[error("the local diarization queue is full")]
    Busy,
    #[error("maximum in-flight jobs must be greater than zero")]
    InvalidCapacity,
}

/// Bounded local job runner. There is no unbounded queue: callers receive `Busy` and can
/// retry later when all configured in-flight slots are occupied.
pub struct DiarizationJobManager {
    diarizer: Arc<dyn Diarizer>,
    slots: Arc<Semaphore>,
    states: RwLock<HashMap<String, DiarizationJobState>>,
    mapping_config: MappingConfig,
}

impl DiarizationJobManager {
    pub fn new(
        diarizer: Arc<dyn Diarizer>,
        maximum_in_flight_jobs: usize,
        mapping_config: MappingConfig,
    ) -> Result<Self, JobManagerError> {
        if maximum_in_flight_jobs == 0 {
            return Err(JobManagerError::InvalidCapacity);
        }
        Ok(Self {
            diarizer,
            slots: Arc::new(Semaphore::new(maximum_in_flight_jobs)),
            states: RwLock::new(HashMap::new()),
            mapping_config,
        })
    }

    pub async fn process(
        &self,
        request: DiarizationRequest,
        segments: Vec<TranscriptSegment>,
    ) -> Result<JobOutcome, JobManagerError> {
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| JobManagerError::Busy)?;
        let meeting_id = request.meeting_id.clone();
        self.set_state(&meeting_id, DiarizationJobState::Queued)
            .await;
        tokio::task::yield_now().await;
        self.set_state(&meeting_id, DiarizationJobState::Running)
            .await;

        let result = self.diarizer.diarize(request).await;
        let outcome = match result {
            Ok(result) if !result.ranges.is_empty() || !has_system_segments(&segments) => {
                let mapped =
                    map_speakers(&meeting_id, segments, &result.ranges, self.mapping_config);
                let state = DiarizationJobState::Completed {
                    speaker_count: mapped.speakers.len(),
                    suppressed_echo_segments: mapped.suppressed_echo_segments,
                };
                JobOutcome {
                    segments: mapped.segments,
                    speakers: mapped.speakers,
                    state,
                }
            }
            Ok(_) => fallback_outcome(
                &meeting_id,
                segments,
                self.mapping_config,
                "local engine returned no speaker ranges".to_string(),
            ),
            Err(error) => fallback_outcome(
                &meeting_id,
                segments,
                self.mapping_config,
                error.to_string(),
            ),
        };

        self.set_state(&meeting_id, outcome.state.clone()).await;
        drop(permit);
        Ok(outcome)
    }

    pub async fn state(&self, meeting_id: &str) -> Option<DiarizationJobState> {
        self.states.read().await.get(meeting_id).cloned()
    }

    pub async fn clear_state(&self, meeting_id: &str) {
        self.states.write().await.remove(meeting_id);
    }

    async fn set_state(&self, meeting_id: &str, state: DiarizationJobState) {
        self.states
            .write()
            .await
            .insert(meeting_id.to_string(), state);
    }
}

fn has_system_segments(segments: &[TranscriptSegment]) -> bool {
    segments
        .iter()
        .any(|segment| segment.source == super::AudioSource::System)
}

fn fallback_outcome(
    meeting_id: &str,
    segments: Vec<TranscriptSegment>,
    mapping_config: MappingConfig,
    reason: String,
) -> JobOutcome {
    let mapped = map_speakers(meeting_id, segments, &[], mapping_config);
    let state = DiarizationJobState::Fallback {
        reason,
        suppressed_echo_segments: mapped.suppressed_echo_segments,
    };
    JobOutcome {
        segments: mapped.segments,
        speakers: mapped.speakers,
        state,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use tokio::sync::Notify;

    use super::*;
    use crate::diarization::{
        AudioSource, DiarizationError, DiarizationRange, DiarizationResult, LocalAudioInput,
    };

    struct ResultDiarizer(Result<DiarizationResult, DiarizationError>);

    #[async_trait]
    impl Diarizer for ResultDiarizer {
        async fn diarize(
            &self,
            _request: DiarizationRequest,
        ) -> Result<DiarizationResult, DiarizationError> {
            self.0.clone()
        }

        fn provider_name(&self) -> &'static str {
            "test"
        }
    }

    struct BlockingDiarizer {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl Diarizer for BlockingDiarizer {
        async fn diarize(
            &self,
            _request: DiarizationRequest,
        ) -> Result<DiarizationResult, DiarizationError> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(DiarizationResult { ranges: Vec::new() })
        }

        fn provider_name(&self) -> &'static str {
            "blocking-test"
        }
    }

    fn request(meeting_id: &str) -> DiarizationRequest {
        DiarizationRequest {
            meeting_id: meeting_id.to_string(),
            system_audio: LocalAudioInput {
                samples: vec![0.0],
                sample_rate_hz: 16_000,
            },
        }
    }

    fn segments(meeting_id: &str) -> Vec<TranscriptSegment> {
        vec![
            TranscriptSegment {
                id: "mic".to_string(),
                meeting_id: meeting_id.to_string(),
                text: "hello".to_string(),
                start_ms: 0,
                end_ms: 500,
                source: AudioSource::Microphone,
            },
            TranscriptSegment {
                id: "system".to_string(),
                meeting_id: meeting_id.to_string(),
                text: "welcome".to_string(),
                start_ms: 600,
                end_ms: 1_000,
                source: AudioSource::System,
            },
        ]
    }

    #[tokio::test]
    async fn engine_error_falls_back_without_losing_transcript() {
        let manager = DiarizationJobManager::new(
            Arc::new(ResultDiarizer(Err(DiarizationError::Unavailable(
                "model missing".to_string(),
            )))),
            1,
            MappingConfig::default(),
        )
        .unwrap();

        let outcome = manager
            .process(request("meeting-a"), segments("meeting-a"))
            .await
            .unwrap();

        assert_eq!(outcome.segments.len(), 2);
        assert_eq!(outcome.segments[0].speaker.name, "You");
        assert_eq!(outcome.segments[1].speaker.name, "Remote speaker");
        assert!(matches!(
            outcome.state,
            DiarizationJobState::Fallback { .. }
        ));
        assert_eq!(manager.state("meeting-a").await, Some(outcome.state));
    }

    #[tokio::test]
    async fn successful_job_records_completion_state() {
        let manager = DiarizationJobManager::new(
            Arc::new(ResultDiarizer(Ok(DiarizationResult {
                ranges: vec![DiarizationRange {
                    start_ms: 500,
                    end_ms: 1_100,
                    cluster: "zero".to_string(),
                }],
            }))),
            1,
            MappingConfig::default(),
        )
        .unwrap();

        let outcome = manager
            .process(request("meeting-a"), segments("meeting-a"))
            .await
            .unwrap();

        assert_eq!(outcome.segments[1].speaker.name, "Speaker 1");
        assert!(matches!(
            outcome.state,
            DiarizationJobState::Completed { .. }
        ));
    }

    #[tokio::test]
    async fn capacity_is_bounded_instead_of_queueing_forever() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let manager = Arc::new(
            DiarizationJobManager::new(
                Arc::new(BlockingDiarizer {
                    started: started.clone(),
                    release: release.clone(),
                }),
                1,
                MappingConfig::default(),
            )
            .unwrap(),
        );

        let first_manager = manager.clone();
        let first = tokio::spawn(async move {
            first_manager
                .process(request("meeting-a"), Vec::new())
                .await
        });
        started.notified().await;

        let second = manager.process(request("meeting-b"), Vec::new()).await;
        assert_eq!(second.unwrap_err(), JobManagerError::Busy);

        release.notify_one();
        first.await.unwrap().unwrap();
    }

    #[test]
    fn zero_capacity_is_rejected() {
        let result = DiarizationJobManager::new(
            Arc::new(ResultDiarizer(Ok(DiarizationResult { ranges: Vec::new() }))),
            0,
            MappingConfig::default(),
        );
        assert!(matches!(result, Err(JobManagerError::InvalidCapacity)));
    }
}
