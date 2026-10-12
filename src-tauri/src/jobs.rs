//! The post-meeting processing pipeline. One job at a time; each job spawns a
//! fresh `process` sidecar (decode → ASR → diarise), then aligns and stores the
//! transcript in Rust and, if enabled, summarises it.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use rusqlite::params;
use serde::Serialize;
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Mutex;

use crate::AppState;
use crate::align::{self, AsrOutput, DecodedAudio, DiarOutput, Turn};
use crate::db::{Db, new_id, now_iso};
use crate::meetings;
use crate::sidecar::{Role, Sidecar};

/// Serialises processing so only one sidecar + LLM run at a time, and remembers
/// the running job so the UI can cancel it.
#[derive(Default)]
pub struct JobQueue {
    lock: Mutex<()>,
    current: Mutex<Option<RunningJob>>,
    /// Meetings queued or running, so a double click (or startup recovery plus
    /// resume) cannot process the same meeting twice in a row.
    pending: std::sync::Mutex<std::collections::HashSet<String>>,
}

pub struct RunningJob {
    pub meeting_id: String,
    pub job_id: String,
    sidecar: Arc<Sidecar>,
    cancel: Arc<AtomicBool>,
}

impl JobQueue {
    /// Ask the running job for `meeting_id` (or any job when None) to stop.
    pub async fn cancel(&self, meeting_id: Option<&str>) -> bool {
        let guard = self.current.lock().await;
        if let Some(job) = guard.as_ref()
            && meeting_id.map(|m| m == job.meeting_id).unwrap_or(true)
        {
            job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = job.sidecar.call_timeout("job.cancel", json!({"jobId": job.job_id}), Duration::from_secs(5)).await;
            return true;
        }
        false
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobProgress {
    pub job_id: String,
    pub meeting_id: String,
    pub stage: String,
    pub fraction: f64,
    pub state: String,
    pub error: Option<String>,
}

/// Where progress goes: the Tauri event bus in the app, stderr in headless mode.
pub type Emit = Arc<dyn Fn(&JobProgress) + Send + Sync>;

pub fn app_emitter(app: AppHandle) -> Emit {
    Arc::new(move |p: &JobProgress| {
        let _ = app.emit("job:progress", p);
    })
}

pub fn stdout_emitter() -> Emit {
    Arc::new(|p: &JobProgress| {
        eprintln!(
            "[job {}] {} {:>3}% {}",
            &p.job_id[..8],
            p.stage,
            (p.fraction * 100.0) as u32,
            p.error.clone().unwrap_or_default()
        );
    })
}

/// Spawn the processing pipeline for a meeting in the background. A meeting that
/// is already queued or running is not queued again.
pub fn enqueue_processing(app: AppHandle, meeting_id: String) {
    let queue = app.state::<Arc<JobQueue>>().inner().clone();
    if !queue.pending.lock().map(|mut p| p.insert(meeting_id.clone())).unwrap_or(true) {
        tracing::info!("processing of {meeting_id} already queued");
        return;
    }
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let _guard = queue.lock.lock().await;
        let result = run_processing(&app_emitter(app.clone()), &state, &meeting_id, Some(&queue)).await;
        if let Ok(mut p) = queue.pending.lock() {
            p.remove(&meeting_id);
        }
        match result {
            Err(e) => tracing::error!("processing {meeting_id} failed: {e:#}"),
            Ok(()) => {
                // Summarise with the default template; a missing LLM is not a pipeline failure.
                let settings = crate::settings::load(&state.db).unwrap_or_default();
                if settings.auto_summarise {
                    let jobs = app.state::<Arc<crate::summarise::SummaryJobs>>().inner().clone();
                    let emit = crate::summarise::app_emitter(app.clone());
                    if let Err(e) = crate::summarise::summarise_tracked(
                        emit,
                        &state,
                        &jobs,
                        &meeting_id,
                        Some(&settings.default_template_id),
                    )
                    .await
                    {
                        tracing::warn!("auto-summary skipped for {meeting_id}: {e:#}");
                    }
                }
            }
        }
    });
}

/// Mark jobs and summaries that were running when the app last exited.
pub fn mark_interrupted(db: &Db) -> Result<()> {
    db.with(|c| {
        let now = now_iso();
        let jobs = c.execute(
            "UPDATE jobs SET state = 'failed', error = 'interrupted by app exit', finished_at = ?1 WHERE state IN ('running', 'queued')",
            [&now],
        )?;
        let sums = c.execute("UPDATE summaries SET status = 'failed' WHERE status = 'running'", [])?;
        // Meetings stuck mid-processing go back to 'recorded' so they can be re-run.
        c.execute("UPDATE meetings SET state = 'recorded' WHERE state = 'processing'", [])?;
        if jobs + sums > 0 {
            tracing::info!("marked {jobs} job(s) and {sums} summary(ies) as interrupted");
        }
        Ok(())
    })
}

pub async fn run_processing(
    emit: &Emit,
    state: &AppState,
    meeting_id: &str,
    queue: Option<&Arc<JobQueue>>,
) -> Result<()> {
    let db = state.db.clone();
    let (recording_path, is_retained) = db.with(|c| {
        let rec = meetings::get_recording(c, meeting_id)?.ok_or_else(|| anyhow!("meeting has no recording"))?;
        Ok((rec.path, rec.kind == "retained"))
    })?;
    let job_id = new_id();
    db.with(|c| {
        c.execute(
            "INSERT INTO jobs(id, meeting_id, kind, stage, pct, state, created_at) VALUES (?1, ?2, 'process', 'decode', 0, 'running', ?3)",
            params![job_id, meeting_id, now_iso()],
        )?;
        meetings::set_state(c, meeting_id, "processing")?;
        Ok(())
    })?;

    let mut progress = JobProgress {
        job_id: job_id.clone(),
        meeting_id: meeting_id.to_string(),
        stage: "decode".into(),
        fraction: 0.0,
        state: "running".into(),
        error: None,
    };
    emit(&progress);

    let cancel = Arc::new(AtomicBool::new(false));
    let result = run_stages(
        emit,
        state,
        meeting_id,
        &recording_path,
        is_retained,
        &job_id,
        &mut progress,
        queue,
        cancel.clone(),
    )
    .await;
    if let Some(q) = queue {
        *q.current.lock().await = None;
    }

    match &result {
        Ok(()) => {
            progress.state = "done".into();
            progress.fraction = 1.0;
            progress.stage = "done".into();
            db.with(|c| {
                c.execute(
                    "UPDATE jobs SET state = 'done', pct = 1, stage = 'done', finished_at = ?2 WHERE id = ?1",
                    params![job_id, now_iso()],
                )?;
                meetings::set_state(c, meeting_id, "transcribed")?;
                Ok(())
            })?;
        }
        Err(e) => {
            let cancelled = cancel.load(std::sync::atomic::Ordering::Relaxed);
            progress.state = if cancelled { "cancelled".into() } else { "failed".into() };
            progress.error = Some(if cancelled { "cancelled".into() } else { format!("{e:#}") });
            db.with(|c| {
                c.execute(
                    "UPDATE jobs SET state = ?2, error = ?3, finished_at = ?4 WHERE id = ?1",
                    params![job_id, progress.state, progress.error, now_iso()],
                )?;
                meetings::set_state(c, meeting_id, if cancelled { "recorded" } else { "failed" })?;
                Ok(())
            })?;
        }
    }
    emit(&progress);
    result
}

#[allow(clippy::too_many_arguments)]
async fn run_stages(
    emit: &Emit,
    state: &AppState,
    meeting_id: &str,
    recording_path: &str,
    is_retained: bool,
    job_id: &str,
    progress: &mut JobProgress,
    queue: Option<&Arc<JobQueue>>,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    let dir = state.paths.meeting_dir(meeting_id);
    std::fs::create_dir_all(&dir)?;
    let mix = dir.join("mix16k.wav");
    let asr_json = dir.join("asr.json");
    let diar_json = dir.join("diar.json");

    let sc = Sidecar::spawn_logged(Role::Process, &state.paths.fluid_models(), Some(dir.join("process.log"))).await?;
    if let Some(q) = queue {
        *q.current.lock().await = Some(RunningJob {
            meeting_id: meeting_id.to_string(),
            job_id: job_id.to_string(),
            sidecar: sc.clone(),
            cancel: cancel.clone(),
        });
    }
    // Forward sidecar progress to the UI, mapped onto the overall job.
    let mut rx = sc.subscribe();
    let emit2 = emit.clone();
    let base = JobProgress { ..progress.clone() };
    let forward = tokio::spawn(async move {
        while let Ok(n) = rx.recv().await {
            if n.method != "job.progress" {
                continue;
            }
            let phase = n.params["phase"].as_str().unwrap_or("").to_string();
            let f = n.params["fraction"].as_f64().unwrap_or(0.0);
            let (stage, offset, span) = match phase.as_str() {
                "decode" => ("decode", 0.0, 0.05),
                "transcribe" => ("transcribe", 0.05, 0.55),
                "diarize" => ("diarize", 0.60, 0.30),
                _ => ("process", 0.0, 0.0),
            };
            let p = JobProgress { stage: stage.into(), fraction: offset + span * f, ..base.clone() };
            emit2(&p);
        }
    });

    let long = Duration::from_secs(60 * 60);
    let stage_result: Result<()> = async {
        // 1. decode
        let v = sc
            .call_timeout(
                "decode",
                json!({"jobId": job_id, "input": recording_path, "output": mix.to_string_lossy(), "micSystemStereo": is_retained}),
                Duration::from_secs(20 * 60),
            )
            .await
            .context("decode")?;
        let decoded: DecodedAudio = serde_json::from_value(v).context("decode result")?;
        state.db.with(|c| meetings::set_times(c, meeting_id, None, None, Some(decoded.duration_ms)))?;

        // 2. ASR
        sc.call_timeout("asr.run", json!({"jobId": job_id, "audio": mix.to_string_lossy(), "output": asr_json.to_string_lossy()}), long)
            .await
            .context("asr.run")?;
        let asr: AsrOutput = serde_json::from_slice(&std::fs::read(&asr_json)?).context("asr.json")?;

        // 3. diarisation
        sc.call_timeout("diarize.run", json!({"jobId": job_id, "audio": mix.to_string_lossy(), "output": diar_json.to_string_lossy()}), long)
            .await
            .context("diarize.run")?;
        let diar: DiarOutput = serde_json::from_slice(&std::fs::read(&diar_json)?).context("diar.json")?;

        // 4. align + store
        progress.stage = "align".into();
        progress.fraction = 0.92;
        emit(progress);
        store_transcript(state, meeting_id, &asr, &diar, &decoded)?;
        Ok(())
    }
    .await;

    forward.abort();
    sc.shutdown().await;
    // The 16 kHz mix is only needed during processing (~170 MB per 90 min); it is
    // regenerated from the retained recording if the meeting is re-processed.
    let _ = std::fs::remove_file(&mix);
    stage_result
}

fn f32s_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

pub fn blob_to_f32s(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

/// Build turns from word timings, or, when the ASR produced text without
/// timings, spread the text over the diarised segments proportionally to their
/// duration so nothing is lost.
fn turns_for(asr: &AsrOutput, diar: &DiarOutput) -> Vec<Turn> {
    if !asr.words.is_empty() {
        return align::build_turns(&asr.words, &diar.segments, 1.5, 200);
    }
    let text = asr.text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    if diar.segments.is_empty() {
        return vec![Turn {
            start_ms: 0,
            end_ms: (asr.duration_s * 1000.0) as i64,
            text: text.to_string(),
            label: "S1".into(),
            mic_ratio: None,
        }];
    }
    let total: f64 = diar.segments.iter().map(|s| (s.e - s.s).max(0.0)).sum::<f64>().max(1e-3);
    let mut turns = Vec::new();
    let mut idx = 0usize;
    for (i, seg) in diar.segments.iter().enumerate() {
        let share = ((seg.e - seg.s).max(0.0) / total * words.len() as f64).round() as usize;
        let end = if i == diar.segments.len() - 1 { words.len() } else { (idx + share).min(words.len()) };
        if end > idx {
            turns.push(Turn {
                start_ms: (seg.s * 1000.0) as i64,
                end_ms: (seg.e * 1000.0) as i64,
                text: words[idx..end].join(" "),
                label: seg.speaker.clone(),
                mic_ratio: None,
            });
            idx = end;
        }
    }
    turns
}

fn store_transcript(
    state: &AppState,
    meeting_id: &str,
    asr: &AsrOutput,
    diar: &DiarOutput,
    decoded: &DecodedAudio,
) -> Result<()> {
    let mut turns = turns_for(asr, diar);
    for t in &mut turns {
        t.mic_ratio = align::mic_ratio(decoded.energy.as_ref(), t.start_ms, t.end_ms);
    }
    let (stats, me) = align::speaker_stats(&turns);
    let order = align::label_order(&turns);

    state.db.with_mut(|c| {
        let tx = c.transaction()?;
        let transcript_id = new_id();
        tx.execute(
            "INSERT INTO transcripts(id, meeting_id, engine_id, diarizer_id, language, created_at) VALUES (?1, ?2, ?3, ?4, 'en', ?5)",
            params![transcript_id, meeting_id, asr.engine, diar.engine, now_iso()],
        )?;
        for (idx, t) in turns.iter().enumerate() {
            tx.execute(
                "INSERT INTO segments(transcript_id, idx, start_ms, end_ms, text, diar_label, mic_ratio) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![transcript_id, idx as i64, t.start_ms, t.end_ms, t.text, t.label, t.mic_ratio],
            )?;
        }
        tx.execute("DELETE FROM meeting_speakers WHERE meeting_id = ?1", [meeting_id])?;
        let me_speaker = crate::speakers::me_speaker(&tx)?;
        let mut linked = 0;
        for (i, label) in order.iter().enumerate() {
            let (speech_ms, ratio) = stats.get(label).cloned().unwrap_or((0, None));
            let emb_vec = diar.speakers.get(label).cloned();
            let emb = emb_vec.as_ref().map(|v| f32s_to_blob(v));
            // Known voice? Auto-link when close enough. The mic-dominant cluster is the
            // local user: link it to the "me" speaker when one exists.
            let known = match &emb_vec {
                Some(v) => crate::speakers::best_match(&tx, v)?.filter(|(_, _, d)| *d < crate::speakers::MATCH_THRESHOLD),
                None => None,
            };
            let is_me_cluster = me.as_deref() == Some(label);
            let (speaker_id, display) = match (known, is_me_cluster, &me_speaker) {
                (Some((id, name, _)), _, _) => {
                    linked += 1;
                    (Some(id), name)
                }
                (None, true, Some((id, name))) => {
                    linked += 1;
                    (Some(id.clone()), name.clone())
                }
                (None, true, None) => (None, "Me".to_string()),
                (None, false, _) => (None, format!("Speaker {}", i + 1)),
            };
            tx.execute(
                "INSERT INTO meeting_speakers(meeting_id, diar_label, speaker_id, display_name, embedding, mic_ratio, speech_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![meeting_id, label, speaker_id, display, emb, ratio, speech_ms],
            )?;
            if let Some(id) = &speaker_id {
                tx.execute("UPDATE segments SET speaker_id = ?3 WHERE transcript_id = ?1 AND diar_label = ?2", params![transcript_id, label, id])?;
                tx.execute("UPDATE speakers SET last_seen_at = ?2 WHERE id = ?1", params![id, now_iso()])?;
            }
        }
        tx.commit()?;
        if linked > 0 {
            tracing::info!("auto-linked {linked} cluster(s) to known speakers");
        }
        Ok(())
    })?;
    tracing::info!("stored transcript for {meeting_id}: {} turns, {} speakers", turns.len(), order.len());
    Ok(())
}

// ---- commands -------------------------------------------------------------

#[tauri::command]
pub async fn meeting_cancel_processing(
    queue: tauri::State<'_, Arc<JobQueue>>,
    meeting_id: String,
) -> Result<bool, String> {
    Ok(queue.cancel(Some(&meeting_id)).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::align::DiarSegment;

    #[test]
    fn text_without_timings_is_spread_over_segments() {
        let asr = AsrOutput {
            engine: "x".into(),
            text: "one two three four five six".into(),
            words: vec![],
            duration_s: 6.0,
        };
        let diar = DiarOutput {
            engine: "d".into(),
            segments: vec![
                DiarSegment { speaker: "S1".into(), s: 0.0, e: 2.0, q: 1.0 },
                DiarSegment { speaker: "S2".into(), s: 2.0, e: 6.0, q: 1.0 },
            ],
            speakers: Default::default(),
        };
        let turns = turns_for(&asr, &diar);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].text, "one two");
        assert_eq!(turns[1].text, "three four five six");
        assert_eq!(turns[1].label, "S2");
    }
}
