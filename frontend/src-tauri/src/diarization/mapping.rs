use std::collections::{BTreeMap, BTreeSet, HashMap};

use sha2::{Digest, Sha256};

use super::{
    AudioSource, DiarizationRange, LabeledTranscriptSegment, Speaker, SpeakerKind,
    TranscriptSegment,
};

#[derive(Debug, Clone, Copy)]
pub struct MappingConfig {
    /// Required overlap as a fraction of the shorter segment.
    pub echo_min_overlap_ratio: f64,
    /// Maximum normalized character edit distance for non-identical text.
    pub echo_max_edit_ratio: f64,
}

impl Default for MappingConfig {
    fn default() -> Self {
        Self {
            echo_min_overlap_ratio: 0.80,
            echo_max_edit_ratio: 0.05,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedTranscript {
    pub segments: Vec<LabeledTranscriptSegment>,
    pub speakers: Vec<Speaker>,
    pub suppressed_echo_segments: usize,
}

/// Maps raw dual-channel transcript segments to local meeting speakers.
///
/// Invalid diarization ranges are ignored. System segments without a usable range retain
/// the safe `Remote speaker` fallback instead of being discarded.
pub fn map_speakers(
    meeting_id: &str,
    segments: Vec<TranscriptSegment>,
    ranges: &[DiarizationRange],
    config: MappingConfig,
) -> MappedTranscript {
    let (segments, suppressed_echo_segments) = suppress_echo_duplicates(segments, config);
    let valid_ranges: Vec<&DiarizationRange> =
        ranges.iter().filter(|range| range.is_valid()).collect();

    let selected_clusters: Vec<Option<String>> = segments
        .iter()
        .map(|segment| match segment.source {
            AudioSource::Microphone => None,
            AudioSource::System => best_cluster(segment, &valid_ranges).map(str::to_string),
        })
        .collect();

    let cluster_numbers = number_selected_clusters(&selected_clusters, &valid_ranges);
    let you = speaker(meeting_id, "you", "You", SpeakerKind::You, None);
    let remote = speaker(
        meeting_id,
        "remote-fallback",
        "Remote speaker",
        SpeakerKind::RemoteFallback,
        None,
    );

    let mut identified = BTreeMap::new();
    for (cluster, number) in &cluster_numbers {
        identified.insert(
            cluster.clone(),
            speaker(
                meeting_id,
                &format!("cluster:{cluster}"),
                &format!("Speaker {number}"),
                SpeakerKind::Identified,
                Some(cluster.clone()),
            ),
        );
    }

    let has_you = segments
        .iter()
        .any(|segment| segment.source == AudioSource::Microphone);
    let mut has_remote = false;
    let labeled = segments
        .into_iter()
        .zip(selected_clusters)
        .map(|(segment, cluster)| {
            let assigned = match segment.source {
                AudioSource::Microphone => you.clone(),
                AudioSource::System => {
                    match cluster.and_then(|name| identified.get(&name).cloned()) {
                        Some(speaker) => speaker,
                        None => {
                            has_remote = true;
                            remote.clone()
                        }
                    }
                }
            };
            LabeledTranscriptSegment::from_raw(segment, assigned)
        })
        .collect();

    let mut speakers = Vec::new();
    if has_you {
        speakers.push(you);
    }
    speakers.extend(identified.into_values());
    if has_remote {
        speakers.push(remote);
    }

    MappedTranscript {
        segments: labeled,
        speakers,
        suppressed_echo_segments,
    }
}

fn best_cluster<'a>(
    segment: &TranscriptSegment,
    ranges: &'a [&DiarizationRange],
) -> Option<&'a str> {
    ranges
        .iter()
        .copied()
        // A nearby diarization turn is not evidence that it produced this
        // transcript segment.  Requiring a real overlap avoids fabricating a
        // speaker when the engine returned only disjoint ranges.
        .filter(|range| {
            overlap_ms(
                segment.start_ms,
                segment.end_ms,
                range.start_ms,
                range.end_ms,
            ) > 0
        })
        .max_by(|left, right| {
            let left_overlap =
                overlap_ms(segment.start_ms, segment.end_ms, left.start_ms, left.end_ms);
            let right_overlap = overlap_ms(
                segment.start_ms,
                segment.end_ms,
                right.start_ms,
                right.end_ms,
            );
            left_overlap
                .cmp(&right_overlap)
                // If positive overlap is tied, prefer the nearest midpoint.
                .then_with(|| {
                    let left_distance = segment.midpoint_ms().abs_diff(left.midpoint_ms());
                    let right_distance = segment.midpoint_ms().abs_diff(right.midpoint_ms());
                    right_distance.cmp(&left_distance)
                })
                // `max_by` chooses Greater; reverse lexical order so the lowest label wins.
                .then_with(|| right.cluster.cmp(&left.cluster))
        })
        .map(|range| range.cluster.as_str())
}

fn number_selected_clusters(
    selected: &[Option<String>],
    ranges: &[&DiarizationRange],
) -> HashMap<String, usize> {
    let selected: BTreeSet<&str> = selected.iter().filter_map(Option::as_deref).collect();
    let mut first_appearance: HashMap<&str, u64> = HashMap::new();
    for range in ranges {
        if selected.contains(range.cluster.as_str()) {
            first_appearance
                .entry(range.cluster.as_str())
                .and_modify(|start| *start = (*start).min(range.start_ms))
                .or_insert(range.start_ms);
        }
    }

    let mut ordered: Vec<(&str, u64)> = first_appearance.into_iter().collect();
    ordered.sort_by(|left, right| left.1.cmp(&right.1).then(left.0.cmp(right.0)));
    ordered
        .into_iter()
        .enumerate()
        .map(|(index, (cluster, _))| (cluster.to_string(), index + 1))
        .collect()
}

fn speaker(
    meeting_id: &str,
    discriminator: &str,
    name: &str,
    kind: SpeakerKind,
    cluster: Option<String>,
) -> Speaker {
    let mut digest = Sha256::new();
    digest.update(meeting_id.as_bytes());
    digest.update([0]);
    digest.update(discriminator.as_bytes());
    let digest = digest.finalize();
    let id = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Speaker {
        id: format!("speaker_{id}"),
        name: name.to_string(),
        kind,
        cluster,
    }
}

fn suppress_echo_duplicates(
    segments: Vec<TranscriptSegment>,
    config: MappingConfig,
) -> (Vec<TranscriptSegment>, usize) {
    let suppress: Vec<bool> = segments
        .iter()
        .map(|segment| {
            segment.source == AudioSource::System
                && segments.iter().any(|mic| {
                    mic.source == AudioSource::Microphone
                        && sufficiently_overlapping(mic, segment, config.echo_min_overlap_ratio)
                        && near_identical_text(&mic.text, &segment.text, config.echo_max_edit_ratio)
                })
        })
        .collect();
    let suppressed = suppress.iter().filter(|suppress| **suppress).count();
    let kept = segments
        .into_iter()
        .zip(suppress)
        .filter_map(|(segment, suppress)| (!suppress).then_some(segment))
        .collect();
    (kept, suppressed)
}

fn sufficiently_overlapping(
    left: &TranscriptSegment,
    right: &TranscriptSegment,
    minimum_ratio: f64,
) -> bool {
    let left_duration = left.end_ms.saturating_sub(left.start_ms);
    let right_duration = right.end_ms.saturating_sub(right.start_ms);
    let shorter = left_duration.min(right_duration);
    if shorter == 0 {
        return false;
    }
    overlap_ms(left.start_ms, left.end_ms, right.start_ms, right.end_ms) as f64 / shorter as f64
        >= minimum_ratio.clamp(0.0, 1.0)
}

fn overlap_ms(left_start: u64, left_end: u64, right_start: u64, right_end: u64) -> u64 {
    left_end
        .min(right_end)
        .saturating_sub(left_start.max(right_start))
}

fn near_identical_text(left: &str, right: &str, maximum_ratio: f64) -> bool {
    let left = normalize_text(left);
    let right = normalize_text(right);
    if left.is_empty() || right.is_empty() {
        return false;
    }
    if left == right {
        return true;
    }

    let left_chars: Vec<char> = left.chars().collect();
    let right_chars: Vec<char> = right.chars().collect();
    let longest = left_chars.len().max(right_chars.len());
    // Short utterances are too collision-prone for fuzzy suppression.
    if longest < 12 {
        return false;
    }
    let allowed = ((longest as f64) * maximum_ratio.clamp(0.0, 0.20)).floor() as usize;
    allowed > 0 && levenshtein_with_limit(&left_chars, &right_chars, allowed) <= allowed
}

fn normalize_text(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .map(|character| {
            if character.is_alphanumeric() {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn levenshtein_with_limit(left: &[char], right: &[char], limit: usize) -> usize {
    if left.len().abs_diff(right.len()) > limit {
        return limit + 1;
    }
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];
    for (left_index, left_char) in left.iter().enumerate() {
        current[0] = left_index + 1;
        let mut row_minimum = current[0];
        for (right_index, right_char) in right.iter().enumerate() {
            current[right_index + 1] = (previous[right_index + 1] + 1)
                .min(current[right_index] + 1)
                .min(previous[right_index] + usize::from(left_char != right_char));
            row_minimum = row_minimum.min(current[right_index + 1]);
        }
        if row_minimum > limit {
            return limit + 1;
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(
        id: &str,
        text: &str,
        start_ms: u64,
        end_ms: u64,
        source: AudioSource,
    ) -> TranscriptSegment {
        TranscriptSegment {
            id: id.to_string(),
            meeting_id: "meeting-a".to_string(),
            text: text.to_string(),
            start_ms,
            end_ms,
            source,
        }
    }

    #[test]
    fn microphone_is_always_you_and_fallback_is_remote() {
        let mapped = map_speakers(
            "meeting-a",
            vec![
                segment("mic", "hello", 0, 500, AudioSource::Microphone),
                segment("system", "welcome", 600, 1_000, AudioSource::System),
            ],
            &[],
            MappingConfig::default(),
        );

        assert_eq!(mapped.segments[0].speaker.name, "You");
        assert_eq!(mapped.segments[1].speaker.name, "Remote speaker");
        assert_eq!(mapped.speakers.len(), 2);
    }

    #[test]
    fn maximum_overlap_wins_even_if_another_midpoint_is_closer() {
        let ranges = vec![
            DiarizationRange {
                start_ms: 0,
                end_ms: 800,
                cluster: "large-overlap".to_string(),
            },
            DiarizationRange {
                start_ms: 550,
                end_ms: 650,
                cluster: "close-midpoint".to_string(),
            },
        ];
        let mapped = map_speakers(
            "meeting-a",
            vec![segment("s", "hello", 400, 900, AudioSource::System)],
            &ranges,
            MappingConfig::default(),
        );

        assert_eq!(
            mapped.segments[0].speaker.cluster.as_deref(),
            Some("large-overlap")
        );
    }

    #[test]
    fn disjoint_ranges_fall_back_without_fabricating_a_speaker() {
        let ranges = vec![
            DiarizationRange {
                start_ms: 0,
                end_ms: 100,
                cluster: "far".to_string(),
            },
            DiarizationRange {
                start_ms: 800,
                end_ms: 900,
                cluster: "near".to_string(),
            },
        ];
        let mapped = map_speakers(
            "meeting-a",
            vec![segment("s", "hello", 1_000, 1_100, AudioSource::System)],
            &ranges,
            MappingConfig::default(),
        );

        assert_eq!(mapped.segments[0].speaker.cluster, None);
        assert_eq!(mapped.segments[0].speaker.name, "Remote speaker");
    }

    #[test]
    fn speaker_ids_and_names_are_deterministic_per_meeting() {
        let ranges = vec![
            DiarizationRange {
                start_ms: 900,
                end_ms: 1_200,
                cluster: "z".to_string(),
            },
            DiarizationRange {
                start_ms: 0,
                end_ms: 500,
                cluster: "a".to_string(),
            },
        ];
        let segments = vec![
            segment("later", "later", 950, 1_100, AudioSource::System),
            segment("first", "first", 100, 300, AudioSource::System),
        ];
        let first = map_speakers(
            "meeting-a",
            segments.clone(),
            &ranges,
            MappingConfig::default(),
        );
        let second = map_speakers("meeting-a", segments, &ranges, MappingConfig::default());

        assert_eq!(first, second);
        assert_eq!(first.segments[0].speaker.name, "Speaker 2");
        assert_eq!(first.segments[1].speaker.name, "Speaker 1");
        let other_meeting = speaker(
            "meeting-b",
            "cluster:z",
            "Speaker 2",
            SpeakerKind::Identified,
            Some("z".to_string()),
        );
        assert_ne!(first.segments[0].speaker.id, other_meeting.id);
    }

    #[test]
    fn suppresses_only_conservative_cross_channel_echoes() {
        let mapped = map_speakers(
            "meeting-a",
            vec![
                segment(
                    "mic",
                    "The quarterly report is ready.",
                    100,
                    1_000,
                    AudioSource::Microphone,
                ),
                segment(
                    "echo",
                    "The quarterly report is ready!",
                    150,
                    950,
                    AudioSource::System,
                ),
                segment(
                    "real-remote",
                    "The quarterly report needs changes",
                    150,
                    950,
                    AudioSource::System,
                ),
            ],
            &[],
            MappingConfig::default(),
        );

        assert_eq!(mapped.suppressed_echo_segments, 1);
        assert_eq!(mapped.segments.len(), 2);
        assert!(mapped
            .segments
            .iter()
            .any(|segment| segment.id == "real-remote"));
    }

    #[test]
    fn does_not_suppress_similar_text_without_strong_time_overlap() {
        let mapped = map_speakers(
            "meeting-a",
            vec![
                segment(
                    "mic",
                    "exact same sentence",
                    0,
                    1_000,
                    AudioSource::Microphone,
                ),
                segment(
                    "system",
                    "exact same sentence",
                    1_100,
                    2_000,
                    AudioSource::System,
                ),
            ],
            &[],
            MappingConfig::default(),
        );
        assert_eq!(mapped.suppressed_echo_segments, 0);
        assert_eq!(mapped.segments.len(), 2);
    }
}
