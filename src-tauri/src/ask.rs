//! "Ask your meetings": retrieve the best excerpts for a question with the
//! search pipeline, answer with the summarisation model, cite excerpts as [n].
//! Single turn; the big model's one job slot (`SummaryJobs`) serialises it with
//! summaries.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use muesli_llm::{ChatMessage, Engine, GenParams};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::AppState;
use crate::db::new_id;
use crate::search::index::IndexCtx;
use crate::search::query::{self, SearchRequest};
use crate::summarise::SummaryJobs;

const MAX_EXCERPTS: usize = 12;
const PER_MEETING: usize = 3;
/// Prompt budget in tokens; excerpts are dropped from the end until it fits.
const PROMPT_BUDGET: usize = 6_000;
const SYSTEM: &str = "You answer questions about the user's own meetings using only the numbered excerpts provided. \
Cite excerpts as [n] right after the statements they support. If the excerpts do not contain the answer, say so plainly instead of guessing. \
Answer briefly in Markdown.";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Citation {
    pub n: usize,
    pub chunk_id: i64,
    pub meeting_id: String,
    pub title: String,
    pub date: String,
    pub kind: String,
    pub start_ms: Option<i64>,
    pub section: Option<String>,
    pub speaker: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AskEvent {
    pub ask_id: String,
    pub token: Option<String>,
    /// "running" | "done" | "failed" | "cancelled"
    pub status: String,
    pub answer: Option<String>,
    pub citations: Vec<Citation>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AskRunning {
    pub ask_id: String,
}

pub type Emit = Arc<dyn Fn(&AskEvent) + Send + Sync>;

pub fn app_emitter(app: AppHandle) -> Emit {
    Arc::new(move |e: &AskEvent| {
        let _ = app.emit(if e.token.is_some() { "ask:token" } else { "ask:done" }, e);
    })
}

pub fn stdout_emitter() -> Emit {
    use std::io::Write;
    Arc::new(|e: &AskEvent| {
        if let Some(t) = &e.token {
            let mut o = std::io::stdout();
            let _ = o.write_all(t.as_bytes());
            let _ = o.flush();
        } else if e.status != "running" {
            eprintln!("\n[ask] {} {}", e.status, e.error.clone().unwrap_or_default());
        }
    })
}

pub struct Excerpt {
    pub citation: Citation,
    pub text: String,
}

fn mmss(ms: i64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// Rank with the search pipeline, then flatten to the best chunks (a few per meeting).
pub fn retrieve(ctx: &IndexCtx, question: &str, filters: &SearchRequest) -> Result<Vec<Excerpt>> {
    let req = SearchRequest { query: question.to_string(), limit: 50, offset: 0, ..filters.clone() };
    let ranked = query::rank(ctx, &req)?;
    let by_id: HashMap<&str, &query::Candidate> =
        ranked.candidates.iter().map(|c| (c.meeting.id.as_str(), c)).collect();
    let meeting_ids: Vec<String> = ranked.groups.iter().map(|g| g.meeting_id.clone()).collect();
    let speakers = ctx.db.with(|c| query::speakers_for(c, &meeting_ids))?;
    let mut picked: Vec<(f32, query::Snippet, String)> = Vec::new();
    for g in &ranked.groups {
        let names: HashMap<String, String> = speakers
            .get(&g.meeting_id)
            .map(|v| v.iter().map(|s| (s.diar_label.clone(), s.name.clone())).collect())
            .unwrap_or_default();
        for s in query::pick_snippets(g, &ranked.rows, &names, PER_MEETING) {
            picked.push((s.score, s, g.meeting_id.clone()));
        }
    }
    picked.sort_by(|a, b| b.0.total_cmp(&a.0));
    picked.truncate(MAX_EXCERPTS);
    let mut out = Vec::new();
    for (n, (_, snip, meeting_id)) in picked.into_iter().enumerate() {
        let Some(cand) = by_id.get(meeting_id.as_str()) else { continue };
        let Some(row) = ranked.rows.get(&snip.chunk_id) else { continue };
        let m = &cand.meeting;
        out.push(Excerpt {
            citation: Citation {
                n: n + 1,
                chunk_id: snip.chunk_id,
                meeting_id: meeting_id.clone(),
                title: m.title.clone(),
                date: m.started_at.clone().unwrap_or(m.created_at.clone()).chars().take(10).collect(),
                kind: row.kind.clone(),
                start_ms: row.start_ms,
                section: row.section.clone(),
                speaker: snip.speaker.clone(),
            },
            text: row.text.clone(),
        });
    }
    Ok(out)
}

pub fn build_prompt(question: &str, excerpts: &[Excerpt]) -> String {
    let mut s = format!("Question: {question}\n\nExcerpts from the user's meetings:\n\n");
    for e in excerpts {
        let c = &e.citation;
        let location = match (&c.speaker, c.start_ms, &c.section) {
            (Some(sp), Some(ms), _) => format!("{sp} at {}", mmss(ms)),
            (_, _, Some(sec)) => format!("## {sec}"),
            _ => c.kind.clone(),
        };
        s.push_str(&format!("[{}] {}, {}, {location}:\n{}\n\n", c.n, c.title, c.date, e.text));
    }
    s.push_str("Answer the question using only these excerpts, citing them as [n].");
    s
}

/// Run one question to completion (blocking: retrieval, prompt budgeting, generation).
#[allow(clippy::too_many_arguments)]
pub fn answer_blocking(
    emit: &Emit,
    ctx: &IndexCtx,
    ask_id: &str,
    question: &str,
    filters: &SearchRequest,
    model_path: &std::path::Path,
    n_ctx: u32,
    cancel: Arc<AtomicBool>,
) -> Result<(String, Vec<Citation>)> {
    let mut excerpts = retrieve(ctx, question, filters)?;
    if excerpts.is_empty() {
        let answer = "I could not find anything relevant in your meetings for that question.".to_string();
        emit(&AskEvent {
            ask_id: ask_id.into(),
            token: Some(answer.clone()),
            status: "running".into(),
            answer: None,
            citations: Vec::new(),
            error: None,
        });
        return Ok((answer, Vec::new()));
    }
    let engine = Engine::global();
    engine.load(model_path).context("loading model")?;
    let mut prompt;
    loop {
        prompt =
            engine.render_chat(&[ChatMessage::system(SYSTEM), ChatMessage::user(build_prompt(question, &excerpts))])?;
        if engine.count_tokens(&prompt)? <= PROMPT_BUDGET || excerpts.len() <= 1 {
            break;
        }
        excerpts.pop();
    }
    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    let (emit2, id2) = (emit.clone(), ask_id.to_string());
    let on_token: muesli_llm::TokenCallback = Box::new(move |t: &str| {
        emit2(&AskEvent {
            ask_id: id2.clone(),
            token: Some(t.to_string()),
            status: "running".into(),
            answer: None,
            citations: Vec::new(),
            error: None,
        });
    });
    let params = GenParams { n_ctx, temperature: 0.2, max_tokens: 900, ..GenParams::default() };
    let r = engine.generate(&prompt, params, on_token, cancel)?;
    if r.cancelled {
        bail!("cancelled");
    }
    Ok((r.text.trim().to_string(), excerpts.into_iter().map(|e| e.citation).collect()))
}

/// Resolve the model, run the question on a blocking thread, emit the final event.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    emit: Emit,
    ctx: IndexCtx,
    ask_id: String,
    question: String,
    filters: SearchRequest,
    model_path: std::path::PathBuf,
    n_ctx: u32,
    cancel: Arc<AtomicBool>,
) -> Result<String> {
    let (emit2, ctx2, id2, q2, f2, c2) = (emit.clone(), ctx, ask_id.clone(), question, filters, cancel.clone());
    let result = match tokio::task::spawn_blocking(move || {
        answer_blocking(&emit2, &ctx2, &id2, &q2, &f2, &model_path, n_ctx, c2)
    })
    .await
    {
        Ok(r) => r,
        Err(e) => Err(anyhow!("ask task panicked: {e}")),
    };
    let cancelled = cancel.load(Ordering::Relaxed);
    let event = match &result {
        Ok((answer, citations)) => AskEvent {
            ask_id: ask_id.clone(),
            token: None,
            status: "done".into(),
            answer: Some(answer.clone()),
            citations: citations.clone(),
            error: None,
        },
        Err(_) if cancelled => AskEvent {
            ask_id: ask_id.clone(),
            token: None,
            status: "cancelled".into(),
            answer: None,
            citations: Vec::new(),
            error: None,
        },
        Err(e) => AskEvent {
            ask_id: ask_id.clone(),
            token: None,
            status: "failed".into(),
            answer: None,
            citations: Vec::new(),
            error: Some(format!("{e:#}")),
        },
    };
    emit(&event);
    result.map(|(a, _)| a)
}

// ---- commands -------------------------------------------------------------

type CmdResult<T> = Result<T, String>;

/// Start answering a question. Validation errors (no model, empty question, a
/// summary already running) come back directly; the answer streams through
/// `ask:token` / `ask:done`.
#[tauri::command]
pub async fn ask_meetings(
    app: AppHandle,
    state: State<'_, AppState>,
    jobs: State<'_, Arc<SummaryJobs>>,
    question: String,
    filters: Option<SearchRequest>,
) -> CmdResult<AskRunning> {
    let question = question.trim().to_string();
    if question.is_empty() {
        return Err("ask a question first".into());
    }
    let preset: String =
        state.db.get_setting("preset").map_err(|e| e.to_string())?.unwrap_or_else(|| "balanced".into());
    let (_, model_path, n_ctx) =
        crate::summarise::resolve_llm(&state.paths, &state.manifest, &preset).map_err(|e| format!("{e:#}"))?;
    let ask_id = new_id();
    let cancel = Arc::new(AtomicBool::new(false));
    jobs.try_begin("ask", &ask_id, cancel.clone()).await.map_err(|e| format!("{e:#}"))?;
    let ctx = IndexCtx::from_state(&state);
    let (jobs2, search, id2) = (jobs.inner().clone(), state.search.clone(), ask_id.clone());
    tauri::async_runtime::spawn(async move {
        let r = run(
            app_emitter(app.clone()),
            ctx,
            id2.clone(),
            question,
            filters.unwrap_or_default(),
            model_path,
            n_ctx,
            cancel,
        )
        .await;
        if let Err(e) = r {
            tracing::warn!("ask {id2} ended: {e:#}");
        }
        jobs2.end().await;
        search.touch();
    });
    Ok(AskRunning { ask_id })
}

#[tauri::command]
pub async fn ask_cancel(jobs: State<'_, Arc<SummaryJobs>>) -> CmdResult<bool> {
    Ok(jobs.cancel_current().await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_numbers_excerpts_and_locates_them() {
        let ex = vec![
            Excerpt {
                citation: Citation {
                    n: 1,
                    chunk_id: 1,
                    meeting_id: "m".into(),
                    title: "Pricing call".into(),
                    date: "2026-10-01".into(),
                    kind: "transcript".into(),
                    start_ms: Some(65_000),
                    section: None,
                    speaker: Some("Bob".into()),
                },
                text: "Bob: forty dollars".into(),
            },
            Excerpt {
                citation: Citation {
                    n: 2,
                    chunk_id: 2,
                    meeting_id: "m".into(),
                    title: "Pricing call".into(),
                    date: "2026-10-01".into(),
                    kind: "summary".into(),
                    start_ms: None,
                    section: Some("TL;DR".into()),
                    speaker: None,
                },
                text: "Enterprise is $40.".into(),
            },
        ];
        let p = build_prompt("How much is enterprise?", &ex);
        assert!(p.contains("[1] Pricing call, 2026-10-01, Bob at 01:05:\nBob: forty dollars"));
        assert!(p.contains("[2] Pricing call, 2026-10-01, ## TL;DR:\nEnterprise is $40."));
        assert!(p.starts_with("Question: How much is enterprise?"));
        assert_eq!(mmss(3_661_000), "1:01:01");
    }
}
