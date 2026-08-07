ALTER TABLE transcripts ADD COLUMN source TEXT NOT NULL DEFAULT 'unknown';
ALTER TABLE transcripts ADD COLUMN speaker_id TEXT;

UPDATE transcripts
SET source = CASE
    WHEN speaker = 'microphone' THEN 'mic'
    WHEN speaker = 'system' THEN 'system'
    ELSE 'unknown'
END,
speaker_id = CASE
    WHEN speaker = 'microphone' THEN 'you'
    WHEN speaker = 'system' THEN 'remote'
    ELSE NULL
END;

CREATE TABLE IF NOT EXISTS meeting_speakers (
    meeting_id TEXT NOT NULL,
    speaker_id TEXT NOT NULL,
    display_name TEXT NOT NULL,
    source TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (meeting_id, speaker_id),
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_transcripts_meeting_source
    ON transcripts(meeting_id, source);
