//! Meeting records: SQLite repository functions and the Tauri commands that
//! expose them. Transcript/summary rows are written by the job pipeline.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::db::{new_id, now_iso};
use crate::AppState;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meeting {
    pub id: String,
    pub title: String,
    pub source: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub duration_ms: Option<i64>,
    pub notes_md: String,
    pub state: String,
    /// Error of the most recent failed job, shown while the meeting is in `failed`.
    #[serde(default)]
    pub last_error: Option<String>,
}

/// Columns for `row_to_meeting`, including the derived `last_error`.
const MEETING_COLUMNS: &str = "m.id, m.title, m.source, m.created_at, m.started_at, m.ended_at, m.duration_ms, m.notes_md, m.state,     (SELECT j.error FROM jobs j WHERE j.meeting_id = m.id AND j.state = 'failed' ORDER BY j.created_at DESC LIMIT 1) AS last_error";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Segment {
    pub id: i64,
    pub idx: i64,
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
    pub diar_label: String,
    pub speaker_id: Option<String>,
    pub mic_ratio: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingSpeaker {
    pub diar_label: String,
    pub speaker_id: Option<String>,
    pub display_name: Option<String>,
    pub mic_ratio: Option<f64>,
    pub speech_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub id: String,
    pub template_id: Option<String>,
    pub model_id: String,
    pub body_md: String,
    pub status: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Recording {
    pub id: String,
    pub kind: String,
    pub path: String,
    pub duration_ms: Option<i64>,
    pub bytes: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingDetail {
    pub meeting: Meeting,
    pub recording: Option<Recording>,
    pub segments: Vec<Segment>,
    pub speakers: Vec<MeetingSpeaker>,
    pub summaries: Vec<Summary>,
}

fn row_to_meeting(r: &Row) -> rusqlite::Result<Meeting> {
    Ok(Meeting {
        id: r.get("id")?,
        title: r.get("title")?,
        source: r.get("source")?,
        created_at: r.get("created_at")?,
        started_at: r.get("started_at")?,
        ended_at: r.get("ended_at")?,
        duration_ms: r.get("duration_ms")?,
        notes_md: r.get("notes_md")?,
        state: r.get("state")?,
        last_error: r.get("last_error").ok().flatten(),
    })
}

pub fn insert_meeting(c: &Connection, title: &str, source: &str) -> Result<Meeting> {
    let m = Meeting {
        id: new_id(),
        title: title.to_string(),
        source: source.to_string(),
        created_at: now_iso(),
        started_at: None,
        ended_at: None,
        duration_ms: None,
        notes_md: String::new(),
        state: "new".into(),
        last_error: None,
    };
    c.execute(
        "INSERT INTO meetings(id, title, source, created_at, notes_md, state) VALUES (?1, ?2, ?3, ?4, '', 'new')",
        params![m.id, m.title, m.source, m.created_at],
    )?;
    Ok(m)
}

pub fn get_meeting(c: &Connection, id: &str) -> Result<Option<Meeting>> {
    Ok(c.query_row(&format!("SELECT {MEETING_COLUMNS} FROM meetings m WHERE m.id = ?1"), [id], row_to_meeting).optional()?)
}

pub fn list_meetings(c: &Connection) -> Result<Vec<Meeting>> {
    let mut stmt = c.prepare(&format!("SELECT {MEETING_COLUMNS} FROM meetings m ORDER BY m.created_at DESC"))?;
    let rows = stmt.query_map([], row_to_meeting)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn set_state(c: &Connection, id: &str, state: &str) -> Result<()> {
    c.execute("UPDATE meetings SET state = ?2 WHERE id = ?1", params![id, state])?;
    Ok(())
}

pub fn set_times(c: &Connection, id: &str, started: Option<&str>, ended: Option<&str>, duration_ms: Option<i64>) -> Result<()> {
    c.execute(
        "UPDATE meetings SET started_at = COALESCE(?2, started_at), ended_at = COALESCE(?3, ended_at), duration_ms = COALESCE(?4, duration_ms) WHERE id = ?1",
        params![id, started, ended, duration_ms],
    )?;
    Ok(())
}

pub fn insert_recording(c: &Connection, meeting_id: &str, kind: &str, path: &str, sample_rate: Option<i64>, channels: Option<i64>, duration_ms: Option<i64>) -> Result<String> {
    let id = new_id();
    let bytes = std::fs::metadata(path).map(|m| m.len() as i64).ok();
    c.execute(
        "INSERT INTO recordings(id, meeting_id, kind, path, sample_rate, channels, duration_ms, bytes) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![id, meeting_id, kind, path, sample_rate, channels, duration_ms, bytes],
    )?;
    Ok(id)
}

pub fn get_recording(c: &Connection, meeting_id: &str) -> Result<Option<Recording>> {
    Ok(c
        .query_row(
            "SELECT id, kind, path, duration_ms, bytes FROM recordings WHERE meeting_id = ?1 AND deleted_at IS NULL ORDER BY rowid DESC LIMIT 1",
            [meeting_id],
            |r| Ok(Recording { id: r.get(0)?, kind: r.get(1)?, path: r.get(2)?, duration_ms: r.get(3)?, bytes: r.get(4)? }),
        )
        .optional()?)
}

pub fn detail(c: &Connection, id: &str) -> Result<Option<MeetingDetail>> {
    let Some(meeting) = get_meeting(c, id)? else { return Ok(None) };
    let recording = get_recording(c, id)?;
    let mut segments = Vec::new();
    let mut stmt = c.prepare(
        "SELECT s.id, s.idx, s.start_ms, s.end_ms, s.text, s.diar_label, s.speaker_id, s.mic_ratio
         FROM segments s JOIN transcripts t ON t.id = s.transcript_id
         WHERE t.meeting_id = ?1 AND t.id = (SELECT id FROM transcripts WHERE meeting_id = ?1 ORDER BY created_at DESC LIMIT 1)
         ORDER BY s.idx",
    )?;
    for row in stmt.query_map([id], |r| {
        Ok(Segment {
            id: r.get(0)?,
            idx: r.get(1)?,
            start_ms: r.get(2)?,
            end_ms: r.get(3)?,
            text: r.get(4)?,
            diar_label: r.get(5)?,
            speaker_id: r.get(6)?,
            mic_ratio: r.get(7)?,
        })
    })? {
        segments.push(row?);
    }
    let mut speakers = Vec::new();
    let mut stmt = c.prepare("SELECT diar_label, speaker_id, display_name, mic_ratio, speech_ms FROM meeting_speakers WHERE meeting_id = ?1 ORDER BY speech_ms DESC")?;
    for row in stmt.query_map([id], |r| {
        Ok(MeetingSpeaker { diar_label: r.get(0)?, speaker_id: r.get(1)?, display_name: r.get(2)?, mic_ratio: r.get(3)?, speech_ms: r.get(4)? })
    })? {
        speakers.push(row?);
    }
    let mut summaries = Vec::new();
    let mut stmt = c.prepare("SELECT id, template_id, model_id, body_md, status, created_at FROM summaries WHERE meeting_id = ?1 ORDER BY created_at DESC")?;
    for row in stmt.query_map([id], |r| {
        Ok(Summary { id: r.get(0)?, template_id: r.get(1)?, model_id: r.get(2)?, body_md: r.get(3)?, status: r.get(4)?, created_at: r.get(5)? })
    })? {
        summaries.push(row?);
    }
    Ok(Some(MeetingDetail { meeting, recording, segments, speakers, summaries }))
}

/// Resolve the display name for a diarisation label: explicit display name,
/// linked speaker's name, or `Speaker N`.
pub fn speaker_names(c: &Connection, meeting_id: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = c.prepare(
        "SELECT ms.diar_label, COALESCE(ms.display_name, sp.name) FROM meeting_speakers ms
         LEFT JOIN speakers sp ON sp.id = ms.speaker_id WHERE ms.meeting_id = ?1 ORDER BY ms.diar_label",
    )?;
    let rows = stmt.query_map([meeting_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?;
    let mut out = Vec::new();
    for (i, row) in rows.enumerate() {
        let (label, name) = row?;
        out.push((label, name.unwrap_or_else(|| format!("Speaker {}", i + 1))));
    }
    Ok(out)
}

// ---- commands -------------------------------------------------------------

type CmdResult<T> = Result<T, String>;

#[tauri::command]
pub fn meetings_list(state: State<'_, AppState>) -> CmdResult<Vec<Meeting>> {
    state.db.with(list_meetings).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn meeting_get(state: State<'_, AppState>, id: String) -> CmdResult<Option<MeetingDetail>> {
    state.db.with(|c| detail(c, &id)).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn meeting_update(state: State<'_, AppState>, id: String, title: Option<String>, notes_md: Option<String>) -> CmdResult<()> {
    state
        .db
        .with(|c| {
            c.execute(
                "UPDATE meetings SET title = COALESCE(?2, title), notes_md = COALESCE(?3, notes_md) WHERE id = ?1",
                params![id, title, notes_md],
            )?;
            Ok(())
        })
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn meeting_delete(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let dir = state.paths.meeting_dir(&id);
    state
        .db
        .with(|c| {
            c.execute("DELETE FROM meetings WHERE id = ?1", [&id])?;
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}

#[tauri::command]
pub fn meeting_rename_speaker(state: State<'_, AppState>, meeting_id: String, diar_label: String, name: String) -> CmdResult<()> {
    state
        .db
        .with(|c| {
            c.execute(
                "UPDATE meeting_speakers SET display_name = ?3 WHERE meeting_id = ?1 AND diar_label = ?2",
                params![meeting_id, diar_label, name],
            )?;
            Ok(())
        })
        .map_err(|e| e.to_string())
}
