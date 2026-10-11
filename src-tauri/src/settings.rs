//! User settings (stored as JSON in the `settings` table) and the retention sweep.

use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::AppState;
use crate::db::{Db, now_iso};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// "fast" | "balanced"
    pub preset: String,
    /// Days to keep audio recordings; 0 keeps them forever.
    pub retention_days: u32,
    pub default_template_id: String,
    /// Run the full transcription + diarisation as soon as a recording stops.
    #[serde(default = "default_true")]
    pub auto_transcribe: bool,
    pub auto_summarise: bool,
    /// Show a near-real-time transcript while recording.
    #[serde(default = "default_true")]
    pub live_transcript: bool,
    /// Extract topics, people and decisions from each new summary (runs the LLM once more).
    #[serde(default = "default_true")]
    pub extract_facets: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            preset: "balanced".into(),
            retention_days: 0,
            default_template_id: "general".into(),
            auto_transcribe: true,
            auto_summarise: true,
            live_transcript: true,
            extract_facets: true,
        }
    }
}

pub fn load(db: &Db) -> Result<Settings> {
    let mut s = Settings::default();
    if let Some(p) = db.get_setting::<String>("preset")? {
        s.preset = p;
    }
    if let Some(d) = db.get_setting::<u32>("retention_days")? {
        s.retention_days = d;
    }
    if let Some(t) = db.get_setting::<String>("default_template_id")? {
        s.default_template_id = t;
    }
    if let Some(a) = db.get_setting::<bool>("auto_summarise")? {
        s.auto_summarise = a;
    }
    if let Some(a) = db.get_setting::<bool>("auto_transcribe")? {
        s.auto_transcribe = a;
    }
    if let Some(l) = db.get_setting::<bool>("live_transcript")? {
        s.live_transcript = l;
    }
    if let Some(f) = db.get_setting::<bool>("extract_facets")? {
        s.extract_facets = f;
    }
    Ok(s)
}

pub fn save(db: &Db, s: &Settings) -> Result<()> {
    db.set_setting("preset", &s.preset)?;
    db.set_setting("retention_days", &s.retention_days)?;
    db.set_setting("default_template_id", &s.default_template_id)?;
    db.set_setting("auto_summarise", &s.auto_summarise)?;
    db.set_setting("auto_transcribe", &s.auto_transcribe)?;
    db.set_setting("live_transcript", &s.live_transcript)?;
    db.set_setting("extract_facets", &s.extract_facets)?;
    Ok(())
}

/// Delete retained recordings older than the configured number of days.
/// Transcripts and summaries are kept. Returns the number of files removed.
pub fn retention_sweep(db: &Db) -> Result<usize> {
    let s = load(db)?;
    if s.retention_days == 0 {
        return Ok(0);
    }
    let cutoff = chrono::Utc::now() - chrono::Duration::days(s.retention_days as i64);
    let cutoff = cutoff.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let victims: Vec<(String, String)> = db.with(|c| {
        let mut stmt = c.prepare(
            "SELECT r.id, r.path FROM recordings r JOIN meetings m ON m.id = r.meeting_id
             WHERE r.deleted_at IS NULL AND m.state IN ('transcribed', 'summarised') AND m.created_at < ?1",
        )?;
        let rows = stmt.query_map([&cutoff], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })?;
    let mut removed = 0;
    for (id, path) in victims {
        // Derived audio next to the recording is never needed once transcribed.
        if let Some(dir) = std::path::Path::new(&path).parent() {
            let _ = std::fs::remove_file(dir.join("mix16k.wav"));
        }
        if std::fs::remove_file(&path).is_ok() || !std::path::Path::new(&path).exists() {
            db.with(|c| {
                c.execute("UPDATE recordings SET deleted_at = ?2 WHERE id = ?1", params![id, now_iso()])?;
                Ok(())
            })?;
            removed += 1;
        }
    }
    if removed > 0 {
        tracing::info!("retention sweep removed {removed} recording(s) older than {} days", s.retention_days);
    }
    Ok(removed)
}

type CmdResult<T> = Result<T, String>;

#[tauri::command]
pub fn settings_get(state: State<'_, AppState>) -> CmdResult<Settings> {
    load(&state.db).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn settings_set(state: State<'_, AppState>, settings: Settings) -> CmdResult<Settings> {
    if !["fast", "balanced"].contains(&settings.preset.as_str()) {
        return Err(format!("unknown preset {}", settings.preset));
    }
    save(&state.db, &settings).map_err(|e| e.to_string())?;
    let _ = retention_sweep(&state.db);
    load(&state.db).map_err(|e| e.to_string())
}

/// Delete one meeting's audio now, keeping transcript and summary.
#[tauri::command]
pub fn recording_delete(state: State<'_, AppState>, meeting_id: String) -> CmdResult<()> {
    state
        .db
        .with(|c| {
            let mut stmt = c.prepare("SELECT id, path FROM recordings WHERE meeting_id = ?1 AND deleted_at IS NULL")?;
            let rows: Vec<(String, String)> =
                stmt.query_map([&meeting_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            for (id, path) in rows {
                if let Some(dir) = std::path::Path::new(&path).parent() {
                    let _ = std::fs::remove_file(dir.join("mix16k.wav"));
                }
                let _ = std::fs::remove_file(&path);
                c.execute("UPDATE recordings SET deleted_at = ?2 WHERE id = ?1", params![id, now_iso()])?;
            }
            Ok(())
        })
        .map_err(|e| e.to_string())
}
