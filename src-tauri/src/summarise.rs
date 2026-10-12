//! Summarisation: prompt building, token budgeting with map-reduce fallback,
//! streaming generation through the llama.cpp engine, validation and storage.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use muesli_llm::{ChatMessage, Engine, GenParams};
use rusqlite::params;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Mutex;

use crate::AppState;
use crate::db::{new_id, now_iso};
use crate::meetings;
use crate::models::Manifest;
use crate::paths::Paths;
use crate::templates;

pub const SYSTEM_PROMPT: &str = "You are Muesli, a meeting assistant that writes accurate, concrete summaries of meeting transcripts. \
Use only information from the transcript and the user's notes; never invent names, numbers or decisions. \
Refer to participants by the names given in the transcript. Write Markdown with exactly the section headings the user asks for, in that order. \
Be brief: prefer short bullets over prose, keep the TL;DR to a few sentences, and include owners and dates for actions when they were stated. \
If a section has nothing to report, write 'None noted.' under it. Do not add commentary before or after the sections.";

const CHUNK_PROMPT: &str = "Below is one part of a longer meeting transcript. Write dense bullet notes capturing everything substantive in this part: who said what, decisions, actions (with owners and dates), numbers, disagreements and open questions. Keep speaker names. Do not summarise beyond this part.\n\nTranscript part {{part}} of {{parts}}:\n{{transcript}}";

/// Tokens reserved for the answer plus a safety margin.
const OUTPUT_RESERVE: usize = 2_300;
/// Target size of a map-reduce chunk, in tokens.
const CHUNK_TOKENS: usize = 6_000;
/// Unload the model after this long without a summary request.
const IDLE_UNLOAD: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryEvent {
    pub meeting_id: String,
    pub summary_id: String,
    pub token: Option<String>,
    /// "running" | "done" | "failed" | "cancelled"
    pub status: String,
    pub error: Option<String>,
}

/// The one summary that may run at a time (UI-triggered or pipeline-triggered),
/// plus the idle-unload generation counter.
#[derive(Default)]
pub struct SummaryJobs {
    running: Mutex<Option<Running>>,
    generation: AtomicU64,
}

#[derive(Clone)]
struct Running {
    meeting_id: String,
    summary_id: String,
    cancel: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryRunning {
    pub meeting_id: String,
    pub summary_id: String,
}

pub type Emit = Arc<dyn Fn(&SummaryEvent) + Send + Sync>;

pub fn app_emitter(app: AppHandle) -> Emit {
    Arc::new(move |e: &SummaryEvent| {
        let _ = app.emit(if e.token.is_some() { "summary:token" } else { "summary:done" }, e);
    })
}

pub fn stdout_emitter() -> Emit {
    use std::io::Write;
    Arc::new(|e: &SummaryEvent| {
        if let Some(t) = &e.token {
            let mut o = std::io::stdout();
            let _ = o.write_all(t.as_bytes());
            let _ = o.flush();
        } else {
            eprintln!("\n[summary {}] {} {}", &e.summary_id[..8], e.status, e.error.clone().unwrap_or_default());
        }
    })
}

#[derive(Debug, Clone)]
pub struct Turn {
    pub start_ms: i64,
    pub name: String,
    pub text: String,
}

/// `[mm:ss] Name: text`, merging consecutive turns of the same speaker.
pub fn render_transcript(turns: &[Turn]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < turns.len() {
        let start = turns[i].start_ms;
        let name = &turns[i].name;
        let mut text = turns[i].text.clone();
        let mut j = i + 1;
        while j < turns.len() && &turns[j].name == name && turns[j].start_ms - turns[j - 1].start_ms < 60_000 {
            text.push(' ');
            text.push_str(&turns[j].text);
            j += 1;
        }
        let s = start / 1000;
        if s >= 3600 {
            out.push_str(&format!("[{}:{:02}:{:02}] {name}: {text}\n", s / 3600, (s % 3600) / 60, s % 60));
        } else {
            out.push_str(&format!("[{:02}:{:02}] {name}: {text}\n", s / 60, s % 60));
        }
        i = j;
    }
    out
}

/// Split rendered transcript lines into chunks of roughly `max_chars`.
pub fn chunk_lines(transcript: &str, max_chars: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for line in transcript.lines() {
        if !cur.is_empty() && cur.len() + line.len() + 1 > max_chars {
            chunks.push(std::mem::take(&mut cur));
        }
        cur.push_str(line);
        cur.push('\n');
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// Headings required by the built-in templates; validation looks for the
/// first two at least.
pub fn has_required_headings(body: &str, output: &str) -> bool {
    let required: Vec<&str> = body.lines().filter(|l| l.starts_with("## ")).take(2).collect();
    required.iter().all(|h| output.contains(h.trim()))
}

pub struct Prepared {
    pub template_id: Option<String>,
    pub body: String,
    pub vars: Vec<(String, String)>,
    pub transcript: String,
    pub prefill: bool,
}

pub fn prepare(state: &AppState, meeting_id: &str, template_id: Option<&str>) -> Result<Prepared> {
    let (detail, names, template) = state.db.with(|c| {
        let d = meetings::detail(c, meeting_id)?.ok_or_else(|| anyhow!("meeting not found"))?;
        let names = meetings::speaker_names(c, meeting_id)?;
        let t = match template_id {
            Some(id) => templates::get(c, id)?.ok_or_else(|| anyhow!("template {id} not found"))?,
            None => templates::get(c, "general")?.ok_or_else(|| anyhow!("no general template"))?,
        };
        Ok((d, names, t))
    })?;
    if detail.segments.is_empty() {
        bail!("meeting has no transcript yet");
    }
    let name_of = |label: &str| {
        names.iter().find(|(l, _)| l == label).map(|(_, n)| n.clone()).unwrap_or_else(|| label.to_string())
    };
    let turns: Vec<Turn> = detail
        .segments
        .iter()
        .map(|s| Turn { start_ms: s.start_ms, name: name_of(&s.diar_label), text: s.text.clone() })
        .collect();
    let transcript = render_transcript(&turns);
    let m = &detail.meeting;
    let duration =
        m.duration_ms.map(|ms| format!("{} min", (ms / 60_000).max(1))).unwrap_or_else(|| "unknown length".into());
    let date = m
        .started_at
        .clone()
        .or(Some(m.created_at.clone()))
        .map(|d| d.chars().take(10).collect::<String>())
        .unwrap_or_default();
    let speakers = names.iter().map(|(_, n)| n.clone()).collect::<Vec<_>>().join(", ");
    let notes = if m.notes_md.trim().is_empty() { "(none)".to_string() } else { m.notes_md.clone() };
    let vars = vec![
        ("title".to_string(), m.title.clone()),
        ("date".to_string(), date),
        ("duration".to_string(), duration),
        ("speakers".to_string(), speakers),
        ("notes".to_string(), notes),
    ];
    let mut body = template.body_md.clone();
    if !body.contains("{{transcript}}") {
        body.push_str("\n\nTranscript:\n{{transcript}}");
    }
    let prefill = body.contains("## TL;DR");
    Ok(Prepared { template_id: Some(template.id), body, vars, transcript, prefill })
}

fn fill(body: &str, vars: &[(String, String)], transcript: &str) -> String {
    let mut pairs: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    pairs.push(("transcript", transcript));
    templates::substitute(body, &pairs)
}

/// Resolve the LLM to use: the preset's model, which must be installed.
pub fn resolve_llm(paths: &Paths, manifest: &Manifest, preset: &str) -> Result<(String, std::path::PathBuf, u32)> {
    let p = manifest.presets.get(preset).ok_or_else(|| anyhow!("unknown preset {preset}"))?;
    let entry = manifest.get(&p.llm)?;
    let file = entry.files.first().ok_or_else(|| anyhow!("model {} has no files", entry.id))?;
    let path = paths.model_file(&entry.repo, &file.path);
    if !path.exists() {
        bail!("model {} is not installed; download it from the Models screen", entry.name);
    }
    Ok((entry.id.clone(), path, entry.context_tokens.unwrap_or(16_384)))
}

/// Everything that can fail *before* generation starts; run this first so the
/// caller gets the error directly instead of a summary that never finishes.
pub struct Validated {
    model_id: String,
    model_path: std::path::PathBuf,
    n_ctx: u32,
    prepared: Prepared,
}

pub fn validate(state: &AppState, meeting_id: &str, template_id: Option<&str>) -> Result<Validated> {
    let preset: String = state.db.get_setting("preset")?.unwrap_or_else(|| "balanced".to_string());
    let (model_id, model_path, n_ctx) = resolve_llm(&state.paths, &state.manifest, &preset)?;
    let prepared = prepare(state, meeting_id, template_id)?;
    Ok(Validated { model_id, model_path, n_ctx, prepared })
}

/// Run a summary while registering it in `jobs` so the UI can see and cancel it
/// whether it was started from the pipeline or from a button.
pub async fn summarise_tracked(
    emit: Emit,
    state: &AppState,
    jobs: &Arc<SummaryJobs>,
    meeting_id: &str,
    template_id: Option<&str>,
) -> Result<String> {
    let validated = validate(state, meeting_id, template_id)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let summary_id = new_id();
    {
        let mut guard = jobs.running.lock().await;
        if guard.is_some() {
            bail!("a summary is already being generated");
        }
        *guard = Some(Running {
            meeting_id: meeting_id.to_string(),
            summary_id: summary_id.clone(),
            cancel: cancel.clone(),
        });
    }
    let result = summarise_validated(emit, state, meeting_id, &summary_id, validated, cancel).await;
    *jobs.running.lock().await = None;
    schedule_idle_unload(jobs);
    result
}

/// Drop the model after `IDLE_UNLOAD` unless another summary ran in between.
fn schedule_idle_unload(jobs: &Arc<SummaryJobs>) {
    let generation = jobs.generation.fetch_add(1, Ordering::SeqCst) + 1;
    let jobs = jobs.clone();
    tokio::spawn(async move {
        tokio::time::sleep(IDLE_UNLOAD).await;
        if jobs.generation.load(Ordering::SeqCst) == generation && jobs.running.lock().await.is_none() {
            tokio::task::spawn_blocking(|| {
                if Engine::global().unload().is_ok() {
                    tracing::info!("unloaded idle LLM");
                }
            });
        }
    });
}

/// Headless entry point: validate and run without the job registry.
pub async fn summarise(
    emit: Emit,
    state: &AppState,
    meeting_id: &str,
    template_id: Option<&str>,
    cancel: Arc<AtomicBool>,
) -> Result<String> {
    let validated = validate(state, meeting_id, template_id)?;
    let summary_id = new_id();
    summarise_validated(emit, state, meeting_id, &summary_id, validated, cancel).await
}

async fn summarise_validated(
    emit: Emit,
    state: &AppState,
    meeting_id: &str,
    summary_id: &str,
    v: Validated,
    cancel: Arc<AtomicBool>,
) -> Result<String> {
    state.db.with(|c| {
        c.execute(
            "INSERT INTO summaries(id, meeting_id, template_id, model_id, body_md, status, created_at) VALUES (?1, ?2, ?3, ?4, '', 'running', ?5)",
            params![summary_id, meeting_id, v.prepared.template_id, v.model_id, now_iso()],
        )?;
        Ok(())
    })?;
    emit(&SummaryEvent {
        meeting_id: meeting_id.to_string(),
        summary_id: summary_id.to_string(),
        token: None,
        status: "running".into(),
        error: None,
    });

    let mid = meeting_id.to_string();
    let sid = summary_id.to_string();
    let emit2 = emit.clone();
    let cancel2 = cancel.clone();
    let result: Result<(String, usize, usize)> = match tokio::task::spawn_blocking(move || {
        generate_blocking(emit2, &mid, &sid, &v.model_path, v.n_ctx, v.prepared, cancel2)
    })
    .await
    {
        Ok(r) => r,
        Err(e) => Err(anyhow!("summary task panicked: {e}")),
    };

    let cancelled = cancel.load(Ordering::Relaxed);
    let (status, error, body, tokens) = match &result {
        Ok((text, pt, ot)) => ("done", None, text.clone(), (Some(*pt as i64), Some(*ot as i64))),
        Err(_) if cancelled => ("cancelled", None, String::new(), (None, None)),
        Err(e) => ("failed", Some(format!("{e:#}")), String::new(), (None, None)),
    };
    let store = state.db.with(|c| {
        c.execute(
            "UPDATE summaries SET body_md = ?2, status = ?3, prompt_tokens = ?4, output_tokens = ?5 WHERE id = ?1",
            params![summary_id, body, status, tokens.0, tokens.1],
        )?;
        if status == "done" {
            meetings::set_state(c, meeting_id, "summarised")?;
        }
        Ok(())
    });
    let error = match store {
        Ok(()) => error,
        Err(e) => Some(format!("{e:#}")),
    };
    emit(&SummaryEvent {
        meeting_id: meeting_id.to_string(),
        summary_id: summary_id.to_string(),
        token: None,
        status: if error.is_some() && status == "done" { "failed".into() } else { status.into() },
        error,
    });
    result.map(|_| summary_id.to_string())
}

fn generate_blocking(
    emit: Emit,
    meeting_id: &str,
    summary_id: &str,
    model_path: &std::path::Path,
    n_ctx: u32,
    prepared: Prepared,
    cancel: Arc<AtomicBool>,
) -> Result<(String, usize, usize)> {
    let engine = Engine::global();
    engine.load(model_path).context("loading model")?;
    let params = GenParams { n_ctx, ..GenParams::default() };
    let budget = n_ctx as usize - OUTPUT_RESERVE;

    // Single pass if the whole prompt fits, otherwise map-reduce over chunks.
    let full_user = fill(&prepared.body, &prepared.vars, &prepared.transcript);
    let full_prompt = engine.render_chat(&[ChatMessage::system(SYSTEM_PROMPT), ChatMessage::user(full_user)])?;
    let full_tokens = engine.count_tokens(&full_prompt)?;
    let (mut prompt, reduced) = if full_tokens <= budget {
        (full_prompt, false)
    } else {
        tracing::info!("prompt is {full_tokens} tokens > budget {budget}; map-reduce");
        let chunks = chunk_lines(&prepared.transcript, CHUNK_TOKENS * 4);
        let mut notes = String::new();
        for (i, chunk) in chunks.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                bail!("cancelled");
            }
            let user = CHUNK_PROMPT
                .replace("{{part}}", &(i + 1).to_string())
                .replace("{{parts}}", &chunks.len().to_string())
                .replace("{{transcript}}", chunk);
            let p = engine.render_chat(&[ChatMessage::system(SYSTEM_PROMPT), ChatMessage::user(user)])?;
            let part = engine.generate(
                &p,
                GenParams { max_tokens: 700, ..params.clone() },
                Box::new(|_| {}),
                cancel.clone(),
            )?;
            if part.cancelled {
                bail!("cancelled");
            }
            notes.push_str(&format!("\n### Part {} of {}\n{}\n", i + 1, chunks.len(), part.text.trim()));
        }
        let condensed = format!("(Condensed notes of the full transcript, {} parts)\n{notes}", chunks.len());
        (
            engine.render_chat(&[
                ChatMessage::system(SYSTEM_PROMPT),
                ChatMessage::user(fill(&prepared.body, &prepared.vars, &condensed)),
            ])?,
            true,
        )
    };

    let prefill = if prepared.prefill { "## TL;DR\n" } else { "" };
    prompt.push_str(prefill);

    let mut attempt_params = params.clone();
    let mut last = None;
    for attempt in 0..2 {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let e2 = emit.clone();
        let (mid, sid) = (meeting_id.to_string(), summary_id.to_string());
        if !prefill.is_empty() {
            e2(&SummaryEvent {
                meeting_id: mid.clone(),
                summary_id: sid.clone(),
                token: Some(prefill.to_string()),
                status: "running".into(),
                error: None,
            });
        }
        let on_token: muesli_llm::TokenCallback = Box::new(move |t: &str| {
            e2(&SummaryEvent {
                meeting_id: mid.clone(),
                summary_id: sid.clone(),
                token: Some(t.to_string()),
                status: "running".into(),
                error: None,
            });
        });
        let r = engine.generate(&prompt, attempt_params.clone(), on_token, cancel.clone())?;
        if r.cancelled {
            bail!("cancelled");
        }
        let text = format!("{prefill}{}", r.text.trim());
        let ok = has_required_headings(&prepared.body, &text);
        tracing::info!(
            "summary attempt {attempt}: {} prompt tok ({:.1}s), {} out tok ({:.1}s), headings ok={ok}, reduced={reduced}",
            r.prompt_tokens,
            r.prompt_seconds,
            r.output_tokens,
            r.gen_seconds
        );
        last = Some((text, r.prompt_tokens, r.output_tokens));
        if ok {
            break;
        }
        attempt_params.temperature = 0.1;
        attempt_params.seed = attempt_params.seed.wrapping_add(1);
        emit(&SummaryEvent {
            meeting_id: meeting_id.into(),
            summary_id: summary_id.into(),
            token: Some("\n\n---\n(retrying for the required sections)\n\n".into()),
            status: "running".into(),
            error: None,
        });
    }
    last.ok_or_else(|| anyhow!("no output"))
}

// ---- commands -------------------------------------------------------------

type CmdResult<T> = Result<T, String>;

/// Start a summary. Validation errors (no model, no transcript, bad template)
/// are returned directly; generation itself runs in the background and reports
/// through `summary:token` / `summary:done`.
#[tauri::command]
pub async fn summary_generate(
    app: AppHandle,
    state: State<'_, AppState>,
    jobs: State<'_, Arc<SummaryJobs>>,
    meeting_id: String,
    template_id: Option<String>,
) -> CmdResult<SummaryRunning> {
    if jobs.running.lock().await.is_some() {
        return Err("a summary is already being generated".into());
    }
    let validated = validate(&state, &meeting_id, template_id.as_deref()).map_err(|e| format!("{e:#}"))?;
    let cancel = Arc::new(AtomicBool::new(false));
    let summary_id = new_id();
    {
        let mut guard = jobs.running.lock().await;
        if guard.is_some() {
            return Err("a summary is already being generated".into());
        }
        *guard =
            Some(Running { meeting_id: meeting_id.clone(), summary_id: summary_id.clone(), cancel: cancel.clone() });
    }
    let app2 = app.clone();
    let jobs2 = jobs.inner().clone();
    let (mid, sid) = (meeting_id.clone(), summary_id.clone());
    tauri::async_runtime::spawn(async move {
        let st = app2.state::<AppState>();
        let r = summarise_validated(app_emitter(app2.clone()), &st, &mid, &sid, validated, cancel).await;
        if let Err(e) = r {
            tracing::warn!("summary {sid} ended: {e:#}");
        }
        *jobs2.running.lock().await = None;
        schedule_idle_unload(&jobs2);
    });
    Ok(SummaryRunning { meeting_id, summary_id })
}

#[tauri::command]
pub async fn summary_status(jobs: State<'_, Arc<SummaryJobs>>) -> CmdResult<Option<SummaryRunning>> {
    Ok(jobs
        .running
        .lock()
        .await
        .as_ref()
        .map(|r| SummaryRunning { meeting_id: r.meeting_id.clone(), summary_id: r.summary_id.clone() }))
}

#[tauri::command]
pub async fn summary_cancel(jobs: State<'_, Arc<SummaryJobs>>) -> CmdResult<bool> {
    let guard = jobs.running.lock().await;
    if let Some(r) = guard.as_ref() {
        r.cancel.store(true, Ordering::Relaxed);
        return Ok(true);
    }
    Ok(false)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportBundle {
    pub summary_md: String,
    pub transcript_txt: String,
}

fn export_bundle(state: &AppState, meeting_id: &str, summary_id: Option<&str>) -> Result<ExportBundle> {
    state.db.with(|c| {
        let d = meetings::detail(c, meeting_id)?.ok_or_else(|| anyhow!("meeting not found"))?;
        let names = meetings::speaker_names(c, meeting_id)?;
        let name_of = |label: &str| names.iter().find(|(l, _)| l == label).map(|(_, n)| n.clone()).unwrap_or_else(|| label.to_string());
        let turns: Vec<Turn> = d.segments.iter().map(|s| Turn { start_ms: s.start_ms, name: name_of(&s.diar_label), text: s.text.clone() }).collect();
        let transcript_txt = render_transcript(&turns);
        let summary = match summary_id {
            Some(id) => d.summaries.iter().find(|s| s.id == id).cloned(),
            None => d.summaries.iter().find(|s| s.status == "done").cloned(),
        };
        let m = &d.meeting;
        let date = m.started_at.clone().unwrap_or(m.created_at.clone());
        let participants = names.iter().map(|(_, n)| format!("\"{n}\"")).collect::<Vec<_>>().join(", ");
        let body = summary.map(|s| s.body_md).unwrap_or_else(|| "_No summary yet._".into());
        let summary_md = format!(
            "---\ntitle: \"{}\"\ndate: {date}\nduration_minutes: {}\nparticipants: [{participants}]\nsource: muesli\n---\n\n# {}\n\n{body}\n",
            m.title.replace('"', "\\\""),
            m.duration_ms.map(|ms| ms / 60_000).unwrap_or(0),
            m.title
        );
        Ok(ExportBundle { summary_md, transcript_txt })
    })
}

/// Markdown summary with front-matter and the plain-text transcript, for copying.
#[tauri::command]
pub fn meeting_export(
    state: State<'_, AppState>,
    meeting_id: String,
    summary_id: Option<String>,
) -> CmdResult<ExportBundle> {
    export_bundle(&state, &meeting_id, summary_id.as_deref()).map_err(|e| e.to_string())
}

/// Write the summary to `path` (chosen by the user in a save dialog) and the
/// transcript next to it as `<name>.transcript.txt`.
#[tauri::command]
pub async fn meeting_export_to(
    state: State<'_, AppState>,
    meeting_id: String,
    summary_id: Option<String>,
    path: String,
) -> CmdResult<Vec<String>> {
    let bundle = export_bundle(&state, &meeting_id, summary_id.as_deref()).map_err(|e| e.to_string())?;
    let summary_path = std::path::PathBuf::from(&path);
    if summary_path.extension().and_then(|e| e.to_str()) != Some("md") {
        return Err("export path must end in .md".into());
    }
    let transcript_path = summary_path.with_extension("transcript.txt");
    tokio::fs::write(&summary_path, bundle.summary_md).await.map_err(|e| format!("writing {path}: {e}"))?;
    tokio::fs::write(&transcript_path, bundle.transcript_txt).await.map_err(|e| format!("writing transcript: {e}"))?;
    Ok(vec![summary_path.display().to_string(), transcript_path.display().to_string()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_merges_consecutive_turns() {
        let turns = vec![
            Turn { start_ms: 0, name: "A".into(), text: "one".into() },
            Turn { start_ms: 5_000, name: "A".into(), text: "two".into() },
            Turn { start_ms: 65_000, name: "B".into(), text: "three".into() },
        ];
        assert_eq!(render_transcript(&turns), "[00:00] A: one two\n[01:05] B: three\n");
    }

    #[test]
    fn chunking_respects_line_boundaries() {
        let t = "a\nbb\nccc\ndddd\n";
        let chunks = chunk_lines(t, 6);
        assert_eq!(chunks, vec!["a\nbb\n".to_string(), "ccc\n".to_string(), "dddd\n".to_string()]);
    }

    #[test]
    fn heading_validation() {
        let body = "x\n## TL;DR\n## Main points by speaker\n## Actions";
        assert!(has_required_headings(body, "## TL;DR\nhi\n## Main points by speaker\n- a"));
        assert!(!has_required_headings(body, "## TL;DR only"));
    }
}
