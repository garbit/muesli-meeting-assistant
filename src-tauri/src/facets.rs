//! LLM-extracted facets: meeting type, topics, people, organisations, projects,
//! decisions and actions, pulled from a finished summary while the model is
//! still loaded. Advisory only: they feed filters and chips, never retrieval.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use muesli_llm::{ChatMessage, Engine, GenParams};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::AppState;
use crate::db::Db;
use crate::meetings;
use crate::search::SearchState;
use crate::summarise::SummaryJobs;

pub const TYPES: [&str; 7] = ["standup", "one-on-one", "customer-call", "interview", "planning", "review", "other"];

const SYSTEM: &str =
    "You extract structured metadata from meeting summaries. Reply with one JSON object and nothing else.";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Person {
    pub name: String,
    pub role: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Action {
    pub text: String,
    pub owner: Option<String>,
    pub due: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ExtractedFacets {
    pub meeting_type: Option<String>,
    pub topics: Vec<String>,
    pub people: Vec<Person>,
    pub organisations: Vec<String>,
    pub projects: Vec<String>,
    pub decisions: Vec<String>,
    pub actions: Vec<Action>,
}

fn clean_list(v: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    v.retain_mut(|s| {
        *s = s.trim().trim_matches(|c: char| c == '.' || c == ',').trim().to_string();
        !s.is_empty() && s.chars().count() <= 120 && seen.insert(s.to_lowercase())
    });
}

impl ExtractedFacets {
    fn normalise(&mut self) {
        clean_list(&mut self.topics);
        clean_list(&mut self.organisations);
        clean_list(&mut self.projects);
        clean_list(&mut self.decisions);
        let mut seen = std::collections::HashSet::new();
        self.people.retain_mut(|p| {
            p.name = p.name.trim().to_string();
            p.role = p.role.as_deref().map(str::trim).filter(|r| !r.is_empty()).map(str::to_string);
            !p.name.is_empty() && seen.insert(p.name.to_lowercase())
        });
        self.actions.retain_mut(|a| {
            a.text = a.text.trim().to_string();
            a.owner = a.owner.as_deref().map(str::trim).filter(|r| !r.is_empty()).map(str::to_string);
            a.due = a.due.as_deref().map(str::trim).filter(|r| !r.is_empty()).map(str::to_string);
            !a.text.is_empty()
        });
        self.meeting_type = self
            .meeting_type
            .as_deref()
            .map(|t| t.trim().to_lowercase())
            .map(|t| if TYPES.contains(&t.as_str()) { t } else { "other".to_string() });
    }

    /// `(kind, value, detail_json)` rows for `meeting_facets`.
    pub fn rows(&self) -> Vec<(&'static str, String, Option<String>)> {
        let mut out = Vec::new();
        if let Some(t) = &self.meeting_type {
            out.push(("type", t.clone(), None));
        }
        out.extend(self.topics.iter().map(|v| ("topic", v.clone(), None)));
        out.extend(self.people.iter().map(|p| {
            ("person", p.name.clone(), p.role.as_ref().map(|r| serde_json::json!({ "role": r }).to_string()))
        }));
        out.extend(self.organisations.iter().map(|v| ("org", v.clone(), None)));
        out.extend(self.projects.iter().map(|v| ("project", v.clone(), None)));
        out.extend(self.decisions.iter().map(|v| ("decision", v.clone(), None)));
        out.extend(self.actions.iter().map(|a| {
            let detail = (a.owner.is_some() || a.due.is_some())
                .then(|| serde_json::json!({ "owner": a.owner, "due": a.due }).to_string());
            ("action", a.text.clone(), detail)
        }));
        out
    }
}

pub fn user_prompt(title: &str, date: &str, speakers: &[String], summary: &str, notes: &str) -> String {
    let notes = if notes.trim().is_empty() { "(none)" } else { notes };
    format!(
        "Meeting: {title}\nDate: {date}\nParticipants: {}\n\nSummary:\n{summary}\n\nUser notes:\n{notes}\n\n\
         Return one JSON object with exactly these keys:\n\
         {{\"meetingType\": one of {types}, \"topics\": [3 to 8 short noun phrases], \
         \"people\": [{{\"name\": \"...\", \"role\": \"...\" or null}}] (people mentioned, including non-participants), \
         \"organisations\": [\"...\"], \"projects\": [\"...\"], \"decisions\": [\"...\"], \
         \"actions\": [{{\"text\": \"...\", \"owner\": \"...\" or null, \"due\": \"...\" or null}}]}}\n\
         Use only information from the summary and notes. Empty lists are fine.",
        speakers.join(", "),
        types = TYPES.iter().map(|t| format!("\"{t}\"")).collect::<Vec<_>>().join(" | "),
    )
}

/// Parse the model's reply: tolerate code fences and text around the object.
pub fn parse(text: &str) -> Result<ExtractedFacets> {
    let start = text.find('{').ok_or_else(|| anyhow!("no JSON object in the reply"))?;
    let end = text.rfind('}').ok_or_else(|| anyhow!("no JSON object in the reply"))?;
    if end <= start {
        bail!("malformed JSON object in the reply");
    }
    let mut f: ExtractedFacets = serde_json::from_str(&text[start..=end]).context("parsing facets JSON")?;
    f.normalise();
    Ok(f)
}

/// Replace a meeting's facets in one transaction.
pub fn store(c: &mut Connection, meeting_id: &str, summary_id: &str, f: &ExtractedFacets) -> Result<()> {
    let tx = c.transaction()?;
    tx.execute("DELETE FROM meeting_facets WHERE meeting_id = ?1", [meeting_id])?;
    for (kind, value, detail) in f.rows() {
        tx.execute(
            "INSERT OR IGNORE INTO meeting_facets(meeting_id, kind, value, norm, detail_json, summary_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![meeting_id, kind, value, value.trim().to_lowercase(), detail, summary_id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Facet {
    pub kind: String,
    pub value: String,
    pub detail: Option<serde_json::Value>,
}

pub fn list(c: &Connection, meeting_id: &str) -> Result<Vec<Facet>> {
    let mut stmt =
        c.prepare("SELECT kind, value, detail_json FROM meeting_facets WHERE meeting_id = ?1 ORDER BY kind, rowid")?;
    let rows = stmt.query_map([meeting_id], |r| {
        let detail: Option<String> = r.get(2)?;
        Ok(Facet { kind: r.get(0)?, value: r.get(1)?, detail: detail.and_then(|d| serde_json::from_str(&d).ok()) })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Build the prompt from the database and run the model (blocking). The engine
/// is normally still loaded from the summary that just finished.
pub fn extract_blocking(
    db: &Db,
    meeting_id: &str,
    summary_id: &str,
    model_path: &Path,
    n_ctx: u32,
    cancel: &AtomicBool,
) -> Result<ExtractedFacets> {
    let (title, date, speakers, body, notes) = db.with(|c| {
        let m = meetings::get_meeting(c, meeting_id)?.ok_or_else(|| anyhow!("meeting not found"))?;
        let body: String = c.query_row("SELECT body_md FROM summaries WHERE id = ?1", [summary_id], |r| r.get(0))?;
        let names = meetings::speaker_names(c, meeting_id)?;
        let date: String = m.started_at.clone().unwrap_or(m.created_at.clone()).chars().take(10).collect();
        Ok((m.title, date, names.into_iter().map(|(_, n)| n).collect::<Vec<_>>(), body, m.notes_md))
    })?;
    let engine = Engine::global();
    engine.load(model_path).context("loading model")?;
    let params = GenParams { n_ctx, temperature: 0.1, max_tokens: 700, ..GenParams::default() };
    let mut user = user_prompt(&title, &date, &speakers, &body, &notes);
    for attempt in 0..2 {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let prompt = engine.render_chat(&[ChatMessage::system(SYSTEM), ChatMessage::user(user.clone())])?;
        let r = engine.generate(&prompt, params.clone(), Box::new(|_| {}), Arc::new(AtomicBool::new(false)))?;
        match parse(&r.text) {
            Ok(f) => return Ok(f),
            Err(e) if attempt == 0 => {
                tracing::warn!("facets reply did not parse ({e:#}); retrying once");
                user.push_str("\n\nReturn valid JSON only: one object with the keys above, no prose, no code fences.");
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("the retry loop returns")
}

/// After a summary finished: extract and store facets (when enabled), then re-index.
/// Errors are logged; a summary is never failed by its facets.
pub fn after_summary(
    db: Arc<Db>,
    search: Arc<SearchState>,
    meeting_id: String,
    summary_id: String,
    model_path: PathBuf,
    n_ctx: u32,
) {
    let enabled = crate::settings::load(&db).map(|s| s.extract_facets).unwrap_or(true);
    if enabled {
        let never = AtomicBool::new(false);
        match extract_blocking(&db, &meeting_id, &summary_id, &model_path, n_ctx, &never) {
            Ok(f) => match db.with_mut(|c| store(c, &meeting_id, &summary_id, &f)) {
                Ok(()) => {
                    tracing::info!("facets for {meeting_id}: {} topics, {} people", f.topics.len(), f.people.len())
                }
                Err(e) => tracing::warn!("storing facets for {meeting_id} failed: {e:#}"),
            },
            Err(e) => tracing::warn!("facet extraction for {meeting_id} failed: {e:#}"),
        }
    }
    search.indexer.request(&meeting_id);
}

// ---- commands -------------------------------------------------------------

type CmdResult<T> = Result<T, String>;

#[tauri::command]
pub fn meeting_facets(state: State<'_, AppState>, meeting_id: String) -> CmdResult<Vec<Facet>> {
    state.db.with(|c| list(c, &meeting_id)).map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FacetsProgress {
    pub done: u32,
    pub total: u32,
    pub meeting_id: Option<String>,
    pub finished: bool,
}

/// Extract facets for every summarised meeting that has none. Loads the big
/// model, so it is explicit (Settings) and takes the single summary slot;
/// `summary_cancel` stops it.
#[tauri::command]
pub async fn facets_backfill(
    app: AppHandle,
    state: State<'_, AppState>,
    jobs: State<'_, Arc<SummaryJobs>>,
) -> CmdResult<u32> {
    let targets: Vec<(String, String)> = state
        .db
        .with(|c| {
            let mut stmt = c.prepare(
                "SELECT m.id, s.id FROM meetings m JOIN summaries s ON s.meeting_id = m.id AND s.status = 'done'
                 WHERE NOT EXISTS (SELECT 1 FROM meeting_facets f WHERE f.meeting_id = m.id)
                   AND s.created_at = (SELECT MAX(created_at) FROM summaries WHERE meeting_id = m.id AND status = 'done')
                 ORDER BY COALESCE(m.started_at, m.created_at) DESC",
            )?;
            Ok(stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .map_err(|e| e.to_string())?;
    if targets.is_empty() {
        return Ok(0);
    }
    let preset: String =
        state.db.get_setting("preset").map_err(|e| e.to_string())?.unwrap_or_else(|| "balanced".into());
    let (_, model_path, n_ctx) =
        crate::summarise::resolve_llm(&state.paths, &state.manifest, &preset).map_err(|e| format!("{e:#}"))?;
    let cancel = Arc::new(AtomicBool::new(false));
    jobs.try_begin("facets", "facets", cancel.clone()).await.map_err(|e| format!("{e:#}"))?;
    let total = targets.len() as u32;
    let jobs2 = jobs.inner().clone();
    let (db, search) = (state.db.clone(), state.search.clone());
    tauri::async_runtime::spawn(async move {
        let mut done = 0u32;
        for (meeting_id, summary_id) in targets {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let (db2, mp, c2, mid, sid) =
                (db.clone(), model_path.clone(), cancel.clone(), meeting_id.clone(), summary_id.clone());
            let r = tokio::task::spawn_blocking(move || {
                let f = extract_blocking(&db2, &mid, &sid, &mp, n_ctx, &c2)?;
                db2.with_mut(|c| store(c, &mid, &sid, &f))
            })
            .await;
            match r {
                Ok(Ok(())) => search.indexer.request(&meeting_id),
                Ok(Err(e)) => tracing::warn!("facets backfill for {meeting_id}: {e:#}"),
                Err(e) => tracing::error!("facets backfill panicked: {e}"),
            }
            done += 1;
            let _ = app
                .emit("facets:progress", FacetsProgress { done, total, meeting_id: Some(meeting_id), finished: false });
        }
        jobs2.end().await;
        let _ = app.emit("facets:progress", FacetsProgress { done, total, meeting_id: None, finished: true });
    });
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_json_with_junk_around_it() {
        let reply = "Sure! Here is the JSON:\n```json\n{\"meetingType\": \"Planning\", \"topics\": [\" Pricing tiers\", \"pricing tiers\", \"SSO\"], \"people\": [{\"name\": \"Alice\", \"role\": \"\"}, {\"name\": \" \"}], \"organisations\": [], \"projects\": [\"Launch\"], \"decisions\": [\"Enterprise gets SSO.\"], \"actions\": [{\"text\": \"Write the proposal\", \"owner\": \"Bob\", \"due\": null}]}\n```\nLet me know if you need more.";
        let f = parse(reply).unwrap();
        assert_eq!(f.meeting_type.as_deref(), Some("planning"));
        assert_eq!(f.topics, vec!["Pricing tiers", "SSO"], "trimmed and de-duplicated case-insensitively");
        assert_eq!(f.people.len(), 1);
        assert_eq!(f.people[0].role, None);
        let rows = f.rows();
        assert!(rows.iter().any(|(k, v, _)| *k == "type" && v == "planning"));
        let action = rows.iter().find(|(k, _, _)| *k == "action").unwrap();
        assert_eq!(action.2.as_deref(), Some("{\"due\":null,\"owner\":\"Bob\"}"));
        let decision = rows.iter().find(|(k, _, _)| *k == "decision").unwrap();
        assert_eq!(decision.1, "Enterprise gets SSO");
    }

    #[test]
    fn unknown_type_becomes_other_and_garbage_fails() {
        let f = parse("{\"meetingType\": \"brainstorm\"}").unwrap();
        assert_eq!(f.meeting_type.as_deref(), Some("other"));
        assert!(parse("no json here").is_err());
        assert!(parse("{\"topics\": \"not a list\"}").is_err());
    }

    #[test]
    fn store_replaces_rows() {
        let db = Db::open_in_memory().unwrap();
        let id = db.with(|c| Ok(meetings::insert_meeting(c, "t", "imported")?.id)).unwrap();
        let f = ExtractedFacets { topics: vec!["A".into(), "B".into()], ..Default::default() };
        db.with_mut(|c| store(c, &id, "s1", &f)).unwrap();
        let f2 =
            ExtractedFacets { topics: vec!["C".into()], meeting_type: Some("review".into()), ..Default::default() };
        db.with_mut(|c| store(c, &id, "s2", &f2)).unwrap();
        let rows = db.with(|c| list(c, &id)).unwrap();
        assert_eq!(
            rows.iter().map(|r| (r.kind.as_str(), r.value.as_str())).collect::<Vec<_>>(),
            vec![("topic", "C"), ("type", "review")]
        );
    }

    #[test]
    fn prompt_mentions_every_key() {
        let p = user_prompt("T", "2026-10-11", &["Alice".into()], "## TL;DR\nx", "");
        for key in ["meetingType", "topics", "people", "organisations", "projects", "decisions", "actions", "(none)"] {
            assert!(p.contains(key), "missing {key}");
        }
    }
}
