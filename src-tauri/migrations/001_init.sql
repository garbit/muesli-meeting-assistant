
CREATE TABLE meetings (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    source TEXT NOT NULL CHECK (source IN ('recorded', 'imported')),
    created_at TEXT NOT NULL,
    started_at TEXT,
    ended_at TEXT,
    duration_ms INTEGER,
    notes_md TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL DEFAULT 'new'
);

CREATE TABLE recordings (
    id TEXT PRIMARY KEY,
    meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('retained', 'import')),
    path TEXT NOT NULL,
    sample_rate INTEGER,
    channels INTEGER,
    duration_ms INTEGER,
    bytes INTEGER,
    keep_until TEXT,
    deleted_at TEXT
);

CREATE TABLE transcripts (
    id TEXT PRIMARY KEY,
    meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    engine_id TEXT NOT NULL,
    engine_version TEXT,
    diarizer_id TEXT,
    language TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE segments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    transcript_id TEXT NOT NULL REFERENCES transcripts(id) ON DELETE CASCADE,
    idx INTEGER NOT NULL,
    start_ms INTEGER NOT NULL,
    end_ms INTEGER NOT NULL,
    text TEXT NOT NULL,
    diar_label TEXT NOT NULL,
    speaker_id TEXT REFERENCES speakers(id) ON DELETE SET NULL,
    mic_ratio REAL
);
CREATE INDEX segments_transcript_idx ON segments(transcript_id, idx);

CREATE TABLE speakers (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    is_me INTEGER NOT NULL DEFAULT 0,
    embedding BLOB,
    embedding_model TEXT,
    sample_count INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    last_seen_at TEXT
);

CREATE TABLE meeting_speakers (
    meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    diar_label TEXT NOT NULL,
    speaker_id TEXT REFERENCES speakers(id) ON DELETE SET NULL,
    display_name TEXT,
    embedding BLOB,
    mic_ratio REAL,
    speech_ms INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (meeting_id, diar_label)
);

CREATE TABLE templates (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    body_md TEXT NOT NULL,
    is_builtin INTEGER NOT NULL DEFAULT 0,
    sort_order INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL
);

CREATE TABLE summaries (
    id TEXT PRIMARY KEY,
    meeting_id TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
    template_id TEXT REFERENCES templates(id) ON DELETE SET NULL,
    model_id TEXT NOT NULL,
    prompt_tokens INTEGER,
    output_tokens INTEGER,
    body_md TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'pending',
    created_at TEXT NOT NULL
);

CREATE TABLE jobs (
    id TEXT PRIMARY KEY,
    meeting_id TEXT REFERENCES meetings(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    stage TEXT,
    pct REAL NOT NULL DEFAULT 0,
    state TEXT NOT NULL DEFAULT 'queued',
    error TEXT,
    payload_json TEXT,
    created_at TEXT NOT NULL,
    finished_at TEXT
);

CREATE TABLE models (
    id TEXT PRIMARY KEY,
    state TEXT NOT NULL DEFAULT 'absent',
    path TEXT,
    bytes_done INTEGER NOT NULL DEFAULT 0,
    bytes_total INTEGER NOT NULL DEFAULT 0,
    sha_ok INTEGER NOT NULL DEFAULT 0,
    installed_at TEXT
);

CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value_json TEXT NOT NULL
);
