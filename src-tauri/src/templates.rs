//! Prompt templates: built-ins seeded into SQLite on first run, user templates
//! edited in-app. A template body is Markdown with `{{variables}}`.

use anyhow::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::db::{new_id, now_iso};
use crate::AppState;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Template {
    pub id: String,
    pub name: String,
    pub body_md: String,
    pub is_builtin: bool,
    pub sort_order: i64,
    pub updated_at: String,
}

pub const VARIABLES: &[&str] = &["title", "date", "duration", "speakers", "notes", "transcript"];

const CONTEXT_BLOCK: &str = "Meeting: {{title}} ({{date}}, {{duration}})\nParticipants: {{speakers}}\n\nMy notes (may be empty):\n{{notes}}\n\nTranscript:\n{{transcript}}";

pub fn builtin_templates() -> Vec<(&'static str, &'static str, String)> {
    let general = format!(
        "Summarise this meeting.\n\nRequired sections, in this order:\n## TL;DR\n2–4 sentences an executive could read in 20 seconds.\n## Main points by speaker\nFor each participant, 2–5 bullets of what they said, asked for, or argued.\n## Actions and takeaways\nBulleted actions with owner and due date when stated, then the key takeaways.\n## Decisions\n## Open questions\n\n{CONTEXT_BLOCK}"
    );
    let one_on_one = format!(
        "Summarise this 1:1 conversation.\n\nRequired sections, in this order:\n## TL;DR\n## What we discussed\n## Feedback and concerns raised\n## Actions and takeaways\nActions with owner and due date when stated.\n## Follow up next time\n\n{CONTEXT_BLOCK}"
    );
    let standup = format!(
        "Summarise this stand-up.\n\nRequired sections, in this order:\n## TL;DR\n## Main points by speaker\nFor each person: done, doing, blocked.\n## Blockers\n## Actions and takeaways\n\n{CONTEXT_BLOCK}"
    );
    let customer = format!(
        "Summarise this customer call for the account team.\n\nRequired sections, in this order:\n## TL;DR\n## Customer context and goals\n## Pain points and requests\n## Objections and risks\n## Actions and takeaways\nActions with owner and due date when stated.\n## Next steps agreed\n\n{CONTEXT_BLOCK}"
    );
    let interview = format!(
        "Summarise this interview for the hiring panel. Be factual and quote the candidate where useful.\n\nRequired sections, in this order:\n## TL;DR\n## Main points by speaker\n## Strengths observed\n## Concerns and gaps\n## Actions and takeaways\n## Questions for the next round\n\n{CONTEXT_BLOCK}"
    );
    let decisions = format!(
        "Produce a decision log for this meeting.\n\nRequired sections, in this order:\n## TL;DR\n## Decisions\nFor each decision: what was decided, who decided, rationale, and alternatives considered.\n## Actions and takeaways\nActions with owner and due date.\n## Open questions\n\n{CONTEXT_BLOCK}"
    );
    vec![
        ("general", "General meeting", general),
        ("one-on-one", "1:1", one_on_one),
        ("standup", "Stand-up", standup),
        ("customer-call", "Customer call", customer),
        ("interview", "Interview", interview),
        ("decision-log", "Decision log", decisions),
    ]
}

/// Insert built-ins that are missing (never overwrites user edits).
pub fn seed_builtins(c: &Connection) -> Result<()> {
    for (i, (id, name, body)) in builtin_templates().into_iter().enumerate() {
        c.execute(
            "INSERT OR IGNORE INTO templates(id, name, body_md, is_builtin, sort_order, updated_at) VALUES (?1, ?2, ?3, 1, ?4, ?5)",
            params![id, name, body, i as i64, now_iso()],
        )?;
    }
    Ok(())
}

pub fn list(c: &Connection) -> Result<Vec<Template>> {
    let mut stmt = c.prepare("SELECT id, name, body_md, is_builtin, sort_order, updated_at FROM templates ORDER BY is_builtin DESC, sort_order, name")?;
    let rows = stmt.query_map([], |r| {
        Ok(Template { id: r.get(0)?, name: r.get(1)?, body_md: r.get(2)?, is_builtin: r.get::<_, i64>(3)? != 0, sort_order: r.get(4)?, updated_at: r.get(5)? })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn get(c: &Connection, id: &str) -> Result<Option<Template>> {
    Ok(list(c)?.into_iter().find(|t| t.id == id))
}

/// Replace `{{var}}` occurrences. Unknown variables are left as-is.
pub fn substitute(body: &str, vars: &[(&str, &str)]) -> String {
    let mut out = body.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{{{k}}}}}"), v);
    }
    out
}

// ---- commands -------------------------------------------------------------

type CmdResult<T> = Result<T, String>;

#[tauri::command]
pub fn templates_list(state: State<'_, AppState>) -> CmdResult<Vec<Template>> {
    state.db.with(list).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn template_upsert(state: State<'_, AppState>, id: Option<String>, name: String, body_md: String) -> CmdResult<Template> {
    let id = id.unwrap_or_else(new_id);
    state
        .db
        .with(|c| {
            c.execute(
                "INSERT INTO templates(id, name, body_md, is_builtin, sort_order, updated_at) VALUES (?1, ?2, ?3, 0, 100, ?4)
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name, body_md = excluded.body_md, updated_at = excluded.updated_at",
                params![id, name, body_md, now_iso()],
            )?;
            get(c, &id)?.ok_or_else(|| anyhow::anyhow!("template vanished"))
        })
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn template_delete(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state
        .db
        .with(|c| {
            c.execute("DELETE FROM templates WHERE id = ?1 AND is_builtin = 0", [&id])?;
            Ok(())
        })
        .map_err(|e| e.to_string())
}

/// Restore a built-in template's body to the shipped version.
#[tauri::command]
pub fn template_reset(state: State<'_, AppState>, id: String) -> CmdResult<Template> {
    let Some((_, name, body)) = builtin_templates().into_iter().find(|(bid, _, _)| *bid == id) else {
        return Err(format!("{id} is not a built-in template"));
    };
    state
        .db
        .with(|c| {
            c.execute("UPDATE templates SET name = ?2, body_md = ?3, updated_at = ?4 WHERE id = ?1", params![id, name, body, now_iso()])?;
            get(c, &id)?.ok_or_else(|| anyhow::anyhow!("template vanished"))
        })
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitute_replaces_known_variables_only() {
        let s = substitute("A {{title}} B {{unknown}} {{notes}}", &[("title", "T"), ("notes", "")]);
        assert_eq!(s, "A T B {{unknown}} ");
    }

    #[test]
    fn builtins_reference_the_transcript() {
        for (_, _, body) in builtin_templates() {
            assert!(body.contains("{{transcript}}"));
            assert!(body.contains("## TL;DR"));
        }
    }
}
