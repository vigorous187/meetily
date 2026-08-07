use crate::api::{TranscriptSearchResult, TranscriptSegment};
use chrono::Utc;
use sqlx::{Connection, Error as SqlxError, SqlitePool};
use std::collections::{HashMap, HashSet};
use tracing::{error, info};
use uuid::Uuid;

pub struct TranscriptsRepository;

impl TranscriptsRepository {
    /// Saves a new meeting and its associated transcript segments.
    /// This function uses a transaction to ensure that either both the meeting
    /// and all its transcripts are saved, or none of them are.
    pub async fn save_transcript(
        pool: &SqlitePool,
        meeting_title: &str,
        transcripts: &[TranscriptSegment],
        folder_path: Option<String>,
        diarization_ranges: &[crate::diarization::DiarizationRange],
    ) -> Result<String, SqlxError> {
        let meeting_id = format!("meeting-{}", Uuid::new_v4());

        let mut conn = pool.acquire().await?;
        let mut transaction = conn.begin().await?;

        let now = Utc::now();

        // 1. Create the new meeting
        let result = sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at, folder_path) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&meeting_id)
        .bind(meeting_title)
        .bind(now)
        .bind(now)
        .bind(&folder_path)
        .execute(&mut *transaction)
        .await;

        if let Err(e) = result {
            error!("Failed to create meeting record: {}", e);
            transaction.rollback().await?;
            return Err(e);
        }

        info!("Successfully created meeting with id: {}", meeting_id);

        // Apply deterministic local speaker mapping and conservative echo suppression.
        // Unknown/imported sources are preserved unchanged.
        let diarization_input = transcripts
            .iter()
            .filter_map(|segment| {
                let source = match segment.source.as_str() {
                    "mic" => crate::diarization::AudioSource::Microphone,
                    "system" => crate::diarization::AudioSource::System,
                    _ => return None,
                };
                Some(crate::diarization::TranscriptSegment {
                    id: segment.id.clone(),
                    meeting_id: meeting_id.clone(),
                    text: segment.text.clone(),
                    start_ms: (segment.audio_start_time.unwrap_or(0.0).max(0.0) * 1000.0) as u64,
                    end_ms: (segment.audio_end_time.unwrap_or(0.0).max(0.0) * 1000.0) as u64,
                    source,
                })
            })
            .collect();
        let mapped = crate::diarization::runtime::process_cached_ranges(
            &meeting_id,
            diarization_input,
            diarization_ranges.to_vec(),
        )
        .await;
        let kept_known_ids: HashSet<&str> = mapped
            .segments
            .iter()
            .map(|segment| segment.id.as_str())
            .collect();
        let mapped_speakers: HashMap<&str, &crate::diarization::Speaker> = mapped
            .segments
            .iter()
            .map(|segment| (segment.id.as_str(), &segment.speaker))
            .collect();

        for speaker in &mapped.speakers {
            let source = match speaker.kind {
                crate::diarization::SpeakerKind::You => "mic",
                _ => "system",
            };
            sqlx::query(
                "INSERT INTO meeting_speakers (meeting_id, speaker_id, display_name, source)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT(meeting_id, speaker_id) DO UPDATE SET
                   display_name = excluded.display_name,
                   source = excluded.source,
                   updated_at = CURRENT_TIMESTAMP",
            )
            .bind(&meeting_id)
            .bind(&speaker.id)
            .bind(&speaker.name)
            .bind(source)
            .execute(&mut *transaction)
            .await?;
        }

        // 2. Save each transcript segment with audio timing fields
        for segment in transcripts {
            if matches!(segment.source.as_str(), "mic" | "system")
                && !kept_known_ids.contains(segment.id.as_str())
            {
                continue;
            }
            let transcript_id = format!("transcript-{}", Uuid::new_v4());
            let mapped_speaker_id = mapped_speakers
                .get(segment.id.as_str())
                .map(|speaker| speaker.id.as_str());
            let result = sqlx::query(
                "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, audio_start_time, audio_end_time, duration, source, speaker_id)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
            )
            .bind(&transcript_id)
            .bind(&meeting_id)
            .bind(&segment.text)
            .bind(&segment.timestamp)
            .bind(segment.audio_start_time)
            .bind(segment.audio_end_time)
            .bind(segment.duration)
            .bind(&segment.source)
            .bind(mapped_speaker_id.or(segment.speaker_id.as_deref()))
            .execute(&mut *transaction)
            .await;

            if let Err(e) = result {
                error!(
                    "Failed to save transcript segment for meeting {}: {}",
                    meeting_id, e
                );
                transaction.rollback().await?;
                return Err(e);
            }
        }

        info!(
            "Successfully saved {} transcript segments for meeting {}",
            transcripts.len(),
            meeting_id
        );

        // Commit the transaction
        transaction.commit().await?;

        Ok(meeting_id)
    }

    /// Searches for a query string within the transcripts.
    /// It returns a list of matching transcripts with context.
    pub async fn search_transcripts(
        pool: &SqlitePool,
        query: &str,
    ) -> Result<Vec<TranscriptSearchResult>, SqlxError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        let search_query = format!("%{}%", query.to_lowercase());

        let rows = sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT m.id, m.title, t.transcript, t.timestamp
             FROM meetings m
             JOIN transcripts t ON m.id = t.meeting_id
             WHERE LOWER(t.transcript) LIKE ?",
        )
        .bind(&search_query)
        .fetch_all(pool)
        .await?;

        let results = rows
            .into_iter()
            .map(|(id, title, transcript, timestamp)| {
                let match_context = Self::get_match_context(&transcript, query);
                TranscriptSearchResult {
                    id,
                    title,
                    match_context,
                    timestamp,
                }
            })
            .collect();

        Ok(results)
    }

    /// Helper function to extract a snippet of text around the first match of a query.
    fn get_match_context(transcript: &str, query: &str) -> String {
        let transcript_lower = transcript.to_lowercase();
        let query_lower = query.to_lowercase();

        match transcript_lower.find(&query_lower) {
            Some(match_index) => {
                let start_index = match_index.saturating_sub(100);
                let end_index = (match_index + query.len() + 100).min(transcript.len());

                let mut context = String::new();
                if start_index > 0 {
                    context.push_str("...");
                }
                context.push_str(&transcript[start_index..end_index]);
                if end_index < transcript.len() {
                    context.push_str("...");
                }
                context
            }
            None => transcript.chars().take(200).collect(), // Fallback to the start of the transcript
        }
    }
}
