-- Local semantic-search index. Embeddings remain on-device and are deleted
-- automatically when their parent meeting is removed.
CREATE TABLE IF NOT EXISTS semantic_search_documents (
    meeting_id TEXT PRIMARY KEY,
    source_hash TEXT NOT NULL,
    embedding_model TEXT NOT NULL,
    chunk_count INTEGER NOT NULL DEFAULT 0,
    indexed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS semantic_search_chunks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    meeting_id TEXT NOT NULL,
    meeting_title TEXT NOT NULL,
    chunk_index INTEGER NOT NULL,
    text TEXT NOT NULL,
    audio_start_time REAL,
    audio_end_time REAL,
    embedding BLOB NOT NULL,
    embedding_dimensions INTEGER NOT NULL,
    source_hash TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE,
    UNIQUE (meeting_id, chunk_index)
);

CREATE INDEX IF NOT EXISTS idx_semantic_search_chunks_meeting
    ON semantic_search_chunks(meeting_id);

CREATE VIRTUAL TABLE IF NOT EXISTS semantic_search_chunks_fts USING fts5(
    text,
    meeting_title UNINDEXED,
    meeting_id UNINDEXED,
    content='semantic_search_chunks',
    content_rowid='id',
    tokenize='unicode61 remove_diacritics 2'
);

CREATE TRIGGER IF NOT EXISTS semantic_search_chunks_ai
AFTER INSERT ON semantic_search_chunks BEGIN
    INSERT INTO semantic_search_chunks_fts(rowid, text, meeting_title, meeting_id)
    VALUES (new.id, new.text, new.meeting_title, new.meeting_id);
END;

CREATE TRIGGER IF NOT EXISTS semantic_search_chunks_ad
AFTER DELETE ON semantic_search_chunks BEGIN
    INSERT INTO semantic_search_chunks_fts(
        semantic_search_chunks_fts, rowid, text, meeting_title, meeting_id
    ) VALUES ('delete', old.id, old.text, old.meeting_title, old.meeting_id);
END;

CREATE TRIGGER IF NOT EXISTS semantic_search_chunks_au
AFTER UPDATE ON semantic_search_chunks BEGIN
    INSERT INTO semantic_search_chunks_fts(
        semantic_search_chunks_fts, rowid, text, meeting_title, meeting_id
    ) VALUES ('delete', old.id, old.text, old.meeting_title, old.meeting_id);
    INSERT INTO semantic_search_chunks_fts(rowid, text, meeting_title, meeting_id)
    VALUES (new.id, new.text, new.meeting_title, new.meeting_id);
END;

-- Populate the FTS index if this migration is applied to a database where a
-- pre-release version of semantic_search_chunks already contained rows.
INSERT INTO semantic_search_chunks_fts(semantic_search_chunks_fts) VALUES ('rebuild');
