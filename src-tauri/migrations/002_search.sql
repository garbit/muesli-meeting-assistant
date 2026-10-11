-- Search index: derived state, rebuilt by search::index::reconcile.

CREATE TABLE chunks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('title','notes','transcript','summary','facets')),
    source_id TEXT,                 -- transcript_id / summary_id the chunk came from
    ord INTEGER NOT NULL,           -- order within (meeting, kind)
    start_ms INTEGER, end_ms INTEGER,
    seg_first_idx INTEGER, seg_last_idx INTEGER,
    section TEXT,                   -- summary heading
    speakers_json TEXT,             -- diar labels present in the chunk
    text TEXT NOT NULL,             -- display + FTS text (no prefixes)
    embed_hash TEXT NOT NULL,       -- sha256 of the exact text that is embedded
    vec BLOB,                       -- 256 x f32 little-endian, NULL until embedded
    model_id TEXT
);
CREATE INDEX chunks_meeting ON chunks(meeting_id, kind, ord);

CREATE VIRTUAL TABLE chunks_fts USING fts5(text, content='chunks', content_rowid='id', tokenize='unicode61 remove_diacritics 2');
CREATE TRIGGER chunks_ai AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER chunks_ad AFTER DELETE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
-- chunk text is never UPDATEd (rows are replaced), so no update trigger

CREATE TABLE search_index (
    meeting_id TEXT PRIMARY KEY REFERENCES meetings(id) ON DELETE CASCADE,
    fingerprint TEXT NOT NULL,      -- see reconcile()
    model_id TEXT,                  -- NULL = chunks exist but no vectors yet
    state TEXT NOT NULL DEFAULT 'pending',   -- pending | indexed | failed
    error TEXT,
    indexed_at TEXT
);

CREATE TABLE meeting_facets (
    meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('type','topic','person','org','project','decision','action')),
    value TEXT NOT NULL,
    norm TEXT NOT NULL,                 -- lower-cased, trimmed, for grouping
    detail_json TEXT,                   -- owner/due for actions, role for people
    summary_id TEXT,
    PRIMARY KEY (meeting_id, kind, norm)
);
CREATE INDEX meeting_facets_kind_norm ON meeting_facets(kind, norm);

CREATE TABLE meeting_vectors (
    meeting_id TEXT PRIMARY KEY REFERENCES meetings(id) ON DELETE CASCADE,
    vec BLOB NOT NULL,
    model_id TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
