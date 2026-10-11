//! Speaker memory: known people with a running-mean voice embedding, matched
//! by cosine distance against each new meeting's diarised clusters.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::AppState;
use crate::db::{new_id, now_iso};
use crate::jobs::blob_to_f32s;

/// Cosine distance below which a cluster is auto-linked to a known speaker.
/// FluidAudio's guide: < 0.5 same speaker (high confidence), > 0.7 different.
pub const MATCH_THRESHOLD: f32 = 0.5;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Speaker {
    pub id: String,
    pub name: String,
    pub is_me: bool,
    pub sample_count: i64,
    pub created_at: String,
    pub last_seen_at: Option<String>,
    pub meetings: i64,
}

pub fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 1.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 1.0;
    }
    1.0 - dot / (na.sqrt() * nb.sqrt())
}

fn f32s_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Find the closest known speaker for an embedding: `(id, name, distance)`.
pub fn best_match(c: &Connection, embedding: &[f32]) -> Result<Option<(String, String, f32)>> {
    let mut stmt = c.prepare("SELECT id, name, embedding FROM speakers WHERE embedding IS NOT NULL")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Vec<u8>>(2)?)))?;
    let mut best: Option<(String, String, f32)> = None;
    for row in rows {
        let (id, name, blob) = row?;
        let d = cosine_distance(embedding, &blob_to_f32s(&blob));
        if best.as_ref().map(|b| d < b.2).unwrap_or(true) {
            best = Some((id, name, d));
        }
    }
    Ok(best)
}

/// The speaker flagged as the local user, if any: `(id, name)`.
pub fn me_speaker(c: &Connection) -> Result<Option<(String, String)>> {
    Ok(c.query_row("SELECT id, name FROM speakers WHERE is_me = 1 LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?)
}

/// Link a meeting's diarised cluster to a known speaker (creating one when
/// `speaker_id` is None), and fold the cluster embedding into the speaker's
/// running mean. Naming a cluster "Me" always resolves to the single `is_me`
/// speaker, creating it if needed.
pub fn link_cluster(
    c: &Connection,
    meeting_id: &str,
    diar_label: &str,
    speaker_id: Option<&str>,
    name: &str,
) -> Result<String> {
    let cluster_emb: Option<Vec<u8>> = c
        .query_row(
            "SELECT embedding FROM meeting_speakers WHERE meeting_id = ?1 AND diar_label = ?2",
            params![meeting_id, diar_label],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let wants_me = name.eq_ignore_ascii_case("me");
    let id = match speaker_id {
        Some(id) => id.to_string(),
        None if wants_me => match me_speaker(c)? {
            Some((id, _)) => id,
            None => {
                let id = new_id();
                c.execute(
                    "INSERT INTO speakers(id, name, is_me, sample_count, created_at) VALUES (?1, 'Me', 1, 0, ?2)",
                    params![id, now_iso()],
                )?;
                id
            }
        },
        None => {
            // Reuse an existing speaker with the same name rather than duplicating.
            let existing: Option<String> = c
                .query_row("SELECT id FROM speakers WHERE name = ?1 COLLATE NOCASE", [name], |r| r.get(0))
                .optional()?;
            match existing {
                Some(id) => id,
                None => {
                    let id = new_id();
                    c.execute(
                        "INSERT INTO speakers(id, name, is_me, sample_count, created_at) VALUES (?1, ?2, 0, 0, ?3)",
                        params![id, name, now_iso()],
                    )?;
                    id
                }
            }
        }
    };
    // The stored display name follows the speaker record, not the typed alias.
    let name: String = c.query_row("SELECT name FROM speakers WHERE id = ?1", [&id], |r| r.get(0))?;
    let name = name.as_str();
    if let Some(blob) = cluster_emb {
        let new = blob_to_f32s(&blob);
        let (old_blob, n): (Option<Vec<u8>>, i64) =
            c.query_row("SELECT embedding, sample_count FROM speakers WHERE id = ?1", [&id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        let merged = match old_blob {
            Some(ob) if n > 0 => {
                let old = blob_to_f32s(&ob);
                old.iter().zip(new.iter()).map(|(o, v)| (o * n as f32 + v) / (n as f32 + 1.0)).collect::<Vec<f32>>()
            }
            _ => new,
        };
        c.execute(
            "UPDATE speakers SET embedding = ?2, embedding_model = 'pyannote-community-1', sample_count = sample_count + 1, last_seen_at = ?3 WHERE id = ?1",
            params![id, f32s_to_blob(&merged), now_iso()],
        )?;
    }
    c.execute(
        "UPDATE meeting_speakers SET speaker_id = ?3, display_name = ?4 WHERE meeting_id = ?1 AND diar_label = ?2",
        params![meeting_id, diar_label, id, name],
    )?;
    c.execute("UPDATE segments SET speaker_id = ?3 WHERE diar_label = ?2 AND transcript_id IN (SELECT id FROM transcripts WHERE meeting_id = ?1)", params![meeting_id, diar_label, id])?;
    Ok(id)
}

pub fn list(c: &Connection) -> Result<Vec<Speaker>> {
    let mut stmt = c.prepare(
        "SELECT s.id, s.name, s.is_me, s.sample_count, s.created_at, s.last_seen_at,
                (SELECT count(DISTINCT meeting_id) FROM meeting_speakers ms WHERE ms.speaker_id = s.id)
         FROM speakers s ORDER BY s.last_seen_at DESC, s.name",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Speaker {
            id: r.get(0)?,
            name: r.get(1)?,
            is_me: r.get::<_, i64>(2)? != 0,
            sample_count: r.get(3)?,
            created_at: r.get(4)?,
            last_seen_at: r.get(5)?,
            meetings: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Meetings in which any of the given speakers appear (their chunks name them).
pub fn meetings_of(c: &Connection, speaker_ids: &[&str]) -> Result<Vec<String>> {
    let json = serde_json::to_string(speaker_ids)?;
    let mut stmt = c.prepare(
        "SELECT DISTINCT meeting_id FROM meeting_speakers WHERE speaker_id IN (SELECT value FROM json_each(?1))",
    )?;
    Ok(stmt.query_map([json], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---- commands -------------------------------------------------------------

type CmdResult<T> = Result<T, String>;

#[tauri::command]
pub fn speakers_list(state: State<'_, AppState>) -> CmdResult<Vec<Speaker>> {
    state.db.with(list).map_err(|e| e.to_string())
}

/// Name a diarised cluster. Creates or links a known speaker and remembers the voice.
#[tauri::command]
pub fn speaker_assign(
    state: State<'_, AppState>,
    meeting_id: String,
    diar_label: String,
    name: String,
    speaker_id: Option<String>,
) -> CmdResult<String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("name is empty".into());
    }
    let id = state
        .db
        .with(|c| link_cluster(c, &meeting_id, &diar_label, speaker_id.as_deref(), &name))
        .map_err(|e| e.to_string())?;
    state.search.indexer.request(&meeting_id);
    Ok(id)
}

#[tauri::command]
pub fn speaker_update(
    state: State<'_, AppState>,
    id: String,
    name: Option<String>,
    is_me: Option<bool>,
) -> CmdResult<()> {
    let renamed = name.is_some();
    let affected = state
        .db
        .with(|c| {
            if let Some(n) = name {
                c.execute("UPDATE speakers SET name = ?2 WHERE id = ?1", params![id, n.trim()])?;
                c.execute(
                    "UPDATE meeting_speakers SET display_name = ?2 WHERE speaker_id = ?1",
                    params![id, n.trim()],
                )?;
            }
            if let Some(me) = is_me {
                if me {
                    c.execute("UPDATE speakers SET is_me = 0", [])?;
                }
                c.execute("UPDATE speakers SET is_me = ?2 WHERE id = ?1", params![id, me as i64])?;
            }
            if renamed { meetings_of(c, &[id.as_str()]) } else { Ok(Vec::new()) }
        })
        .map_err(|e| e.to_string())?;
    for m in affected {
        state.search.indexer.request(&m);
    }
    Ok(())
}

/// Merge `from` into `into`: relink clusters, average embeddings, delete `from`.
#[tauri::command]
pub fn speaker_merge(state: State<'_, AppState>, from: String, into: String) -> CmdResult<()> {
    if from == into {
        return Ok(());
    }
    let affected = state
        .db
        .with_mut(|c| {
            let tx = c.transaction()?;
            let affected = meetings_of(&tx, &[from.as_str(), into.as_str()])?;
            let (ea, na): (Option<Vec<u8>>, i64) =
                tx.query_row("SELECT embedding, sample_count FROM speakers WHERE id = ?1", [&into], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
            let (eb, nb): (Option<Vec<u8>>, i64) =
                tx.query_row("SELECT embedding, sample_count FROM speakers WHERE id = ?1", [&from], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
            let merged = match (ea, eb) {
                (Some(a), Some(b)) if na + nb > 0 => {
                    let (a, b) = (blob_to_f32s(&a), blob_to_f32s(&b));
                    Some(
                        a.iter()
                            .zip(b.iter())
                            .map(|(x, y)| (x * na as f32 + y * nb as f32) / (na + nb) as f32)
                            .collect::<Vec<f32>>(),
                    )
                }
                (Some(a), None) => Some(blob_to_f32s(&a)),
                (None, Some(b)) => Some(blob_to_f32s(&b)),
                _ => None,
            };
            tx.execute(
                "UPDATE speakers SET embedding = ?2, sample_count = ?3 WHERE id = ?1",
                params![into, merged.map(|m| f32s_to_blob(&m)), na + nb],
            )?;
            let name: String = tx.query_row("SELECT name FROM speakers WHERE id = ?1", [&into], |r| r.get(0))?;
            tx.execute(
                "UPDATE meeting_speakers SET speaker_id = ?2, display_name = ?3 WHERE speaker_id = ?1",
                params![from, into, name],
            )?;
            tx.execute("UPDATE segments SET speaker_id = ?2 WHERE speaker_id = ?1", params![from, into])?;
            tx.execute("DELETE FROM speakers WHERE id = ?1", [&from])?;
            tx.commit()?;
            Ok(affected)
        })
        .map_err(|e| e.to_string())?;
    for m in affected {
        state.search.indexer.request(&m);
    }
    Ok(())
}

#[tauri::command]
pub fn speaker_delete(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    // Collect before the delete: ON DELETE SET NULL turns the names back into "Speaker N".
    let affected = state
        .db
        .with(|c| {
            let affected = meetings_of(c, &[id.as_str()])?;
            c.execute("DELETE FROM speakers WHERE id = ?1", [&id])?;
            Ok(affected)
        })
        .map_err(|e| e.to_string())?;
    for m in affected {
        state.search.indexer.request(&m);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_distance_basics() {
        assert!(cosine_distance(&[1.0, 0.0], &[1.0, 0.0]).abs() < 1e-6);
        assert!((cosine_distance(&[1.0, 0.0], &[0.0, 1.0]) - 1.0).abs() < 1e-6);
        assert_eq!(cosine_distance(&[1.0], &[1.0, 2.0]), 1.0);
    }

    #[test]
    fn linking_creates_and_remembers_a_speaker() {
        let db = crate::db::Db::open_in_memory().unwrap();
        db.with(|c| {
            let m = crate::meetings::insert_meeting(c, "t", "imported")?;
            c.execute(
                "INSERT INTO meeting_speakers(meeting_id, diar_label, display_name, embedding, speech_ms) VALUES (?1, 'S1', 'Speaker 1', ?2, 1000)",
                params![m.id, f32s_to_blob(&[1.0, 0.0, 0.0])],
            )?;
            let id = link_cluster(c, &m.id, "S1", None, "Alice")?;
            let (name, dist) = best_match(c, &[0.9, 0.1, 0.0])?.map(|(_, n, d)| (n, d)).unwrap();
            assert_eq!(name, "Alice");
            assert!(dist < MATCH_THRESHOLD);
            let again = link_cluster(c, &m.id, "S1", None, "alice")?;
            assert_eq!(id, again, "same name reuses the speaker");
            Ok(())
        })
        .unwrap();
    }
}
