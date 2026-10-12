//! Indexer: `reconcile(meeting)` rebuilds one meeting's chunks, FTS rows and
//! vectors when its inputs changed. The index is derived state: a fingerprint
//! of the inputs decides whether anything needs doing, and vectors whose text
//! did not change are reused by hash, so a rename re-embeds only what moved.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use super::chunk::{self, ChunkInput, SegmentIn};
use super::vectors::{DIM, VectorIndex, normalise, to_blob};
use crate::AppState;
use crate::db::{Db, now_iso};
use crate::jobs::blob_to_f32s;
use crate::meetings;
use crate::models::Manifest;
use crate::paths::Paths;

/// Documents embedded per embedder call; each batch is written in its own short transaction.
const EMBED_BATCH: usize = 32;
/// Bursts of requests (three renames in a row) are coalesced for this long.
const COALESCE: Duration = Duration::from_millis(1_500);
/// Indexing waits for a running summary so the summariser keeps the GPU, up to this long.
const SUMMARY_WAIT_MAX: Duration = Duration::from_secs(600);
/// Bump when the chunker output changes so every meeting is rebuilt.
const CHUNKER_VERSION: &str = "chunks-v1";

/// Everything `reconcile` needs, detached from Tauri state so it can run on a blocking thread.
#[derive(Clone)]
pub struct IndexCtx {
    pub db: Arc<Db>,
    pub paths: Paths,
    pub manifest: Manifest,
    pub vectors: Arc<VectorIndex>,
}

impl IndexCtx {
    pub fn from_state(state: &AppState) -> Self {
        Self {
            db: state.db.clone(),
            paths: state.paths.clone(),
            manifest: state.manifest.clone(),
            vectors: state.search.vectors.clone(),
        }
    }
}

/// `(title, text)` documents in, unit vectors out. Production uses the embedder; tests inject a fake.
pub type EmbedFn<'a> = &'a (dyn Fn(&[(Option<String>, String)]) -> Result<Vec<Vec<f32>>> + Sync);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexStatus {
    pub indexed: u32,
    pub pending: u32,
    pub failed: u32,
    pub total: u32,
    pub queued: u32,
    pub running: bool,
    pub model_id: Option<String>,
    /// The embedding model is installed, so queries can use vectors.
    pub semantic_available: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexProgress {
    pub meeting_id: String,
    /// "indexed" | "failed"
    pub state: String,
    pub pending: u32,
    pub error: Option<String>,
}

pub type Emit = Arc<dyn Fn(&IndexProgress) + Send + Sync>;

#[derive(Debug, Clone, PartialEq)]
pub enum ReconcileOutcome {
    /// The meeting no longer exists; its rows cascaded away.
    Missing,
    Unchanged,
    Rebuilt {
        chunks: usize,
        embedded: usize,
        vectors: bool,
    },
}

struct Inputs {
    title: String,
    notes: String,
    transcript_id: Option<String>,
    segments: Vec<SegmentIn>,
    name_list: Vec<(String, String)>,
    summary: Option<(String, String)>,
    facets_text: Option<String>,
}

fn load_inputs(c: &Connection, meeting_id: &str) -> Result<Option<Inputs>> {
    let Some((title, notes)) = c
        .query_row("SELECT title, notes_md FROM meetings WHERE id = ?1", [meeting_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .optional()?
    else {
        return Ok(None);
    };
    let transcript_id: Option<String> = c
        .query_row(
            "SELECT id FROM transcripts WHERE meeting_id = ?1 ORDER BY created_at DESC LIMIT 1",
            [meeting_id],
            |r| r.get(0),
        )
        .optional()?;
    let mut segments = Vec::new();
    if let Some(tid) = &transcript_id {
        let mut stmt = c.prepare(
            "SELECT idx, start_ms, end_ms, text, diar_label FROM segments WHERE transcript_id = ?1 ORDER BY idx",
        )?;
        for row in stmt.query_map([tid], |r| {
            Ok(SegmentIn { idx: r.get(0)?, start_ms: r.get(1)?, end_ms: r.get(2)?, text: r.get(3)?, label: r.get(4)? })
        })? {
            segments.push(row?);
        }
    }
    let name_list = meetings::speaker_names(c, meeting_id)?;
    let summary = c
        .query_row(
            "SELECT id, body_md FROM summaries WHERE meeting_id = ?1 AND status = 'done' ORDER BY created_at DESC LIMIT 1",
            [meeting_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let facets_text = facets_text(c, meeting_id)?;
    Ok(Some(Inputs { title, notes, transcript_id, segments, name_list, summary, facets_text }))
}

/// The keyword-searchable rendering of a meeting's extracted facets.
pub fn facets_text(c: &Connection, meeting_id: &str) -> Result<Option<String>> {
    let mut stmt = c.prepare("SELECT kind, value FROM meeting_facets WHERE meeting_id = ?1 ORDER BY kind, norm")?;
    let rows: Vec<(String, String)> =
        stmt.query_map([meeting_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    if rows.is_empty() {
        return Ok(None);
    }
    const LABELS: [(&str, &str); 7] = [
        ("type", "Type"),
        ("topic", "Topics"),
        ("project", "Projects"),
        ("org", "Organisations"),
        ("person", "People"),
        ("decision", "Decisions"),
        ("action", "Actions"),
    ];
    let mut out = String::new();
    for (kind, label) in LABELS {
        let vals: Vec<&str> = rows.iter().filter(|(k, _)| k == kind).map(|(_, v)| v.as_str()).collect();
        if !vals.is_empty() {
            out.push_str(&format!("{label}: {}\n", vals.join(", ")));
        }
    }
    Ok(Some(out.trim_end().to_string()))
}

fn fingerprint(i: &Inputs) -> String {
    let mut h = Sha256::new();
    h.update(CHUNKER_VERSION.as_bytes());
    for part in [
        i.transcript_id.as_deref().unwrap_or(""),
        i.summary.as_ref().map(|s| s.0.as_str()).unwrap_or(""),
        i.notes.as_str(),
        i.title.as_str(),
        i.facets_text.as_deref().unwrap_or(""),
    ] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    for (label, name) in &i.name_list {
        h.update(label.as_bytes());
        h.update([1u8]);
        h.update(name.as_bytes());
        h.update([0u8]);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Rebuild a meeting's index with the installed embedding model (if any).
pub fn reconcile(ctx: &IndexCtx, meeting_id: &str) -> Result<ReconcileOutcome> {
    match super::resolve_embedder(&ctx.paths, &ctx.manifest) {
        Some((model_id, path)) => {
            let f = move |docs: &[(Option<String>, String)]| -> Result<Vec<Vec<f32>>> {
                let e = muesli_llm::Embedder::global();
                e.load(&path)?;
                e.embed_documents(docs)
            };
            reconcile_with(ctx, meeting_id, Some(&model_id), Some(&f))
        }
        None => reconcile_with(ctx, meeting_id, None, None),
    }
}

/// `reconcile` with the embedding model and function supplied by the caller.
pub fn reconcile_with(
    ctx: &IndexCtx,
    meeting_id: &str,
    target_model: Option<&str>,
    embed: Option<EmbedFn>,
) -> Result<ReconcileOutcome> {
    let Some(inputs) = ctx.db.with(|c| load_inputs(c, meeting_id))? else {
        ctx.vectors.remove_meeting(meeting_id);
        return Ok(ReconcileOutcome::Missing);
    };
    let fp = fingerprint(&inputs);
    let existing: Option<(String, Option<String>, String)> = ctx.db.with(|c| {
        Ok(c.query_row(
            "SELECT fingerprint, model_id, state FROM search_index WHERE meeting_id = ?1",
            [meeting_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?)
    })?;
    if let Some((old_fp, old_model, state)) = &existing
        && *old_fp == fp
        && state == "indexed"
        && (target_model.is_none() || old_model.as_deref() == target_model)
    {
        return Ok(ReconcileOutcome::Unchanged);
    }

    let names: HashMap<String, String> = inputs.name_list.iter().cloned().collect();
    let chunks = chunk::build_chunks(&ChunkInput {
        title: &inputs.title,
        transcript_id: inputs.transcript_id.as_deref(),
        segments: &inputs.segments,
        names: &names,
        summary: inputs.summary.as_ref().map(|(id, body)| (id.as_str(), body.as_str())),
        notes: &inputs.notes,
        facets_text: inputs.facets_text.as_deref(),
    });

    struct Pending {
        id: i64,
        title: Option<String>,
        text: String,
    }
    /// `(chunk id, vector)` rows that already have a vector.
    type Rows = Vec<(i64, Vec<f32>)>;
    /// chunk id -> kind, for the whole-meeting vector.
    type Kinds = HashMap<i64, &'static str>;
    // 1. Replace the chunk rows, reusing vectors whose embedded text is unchanged.
    let (mut rows, kinds, pending): (Rows, Kinds, Vec<Pending>) =
        ctx.db.with_mut(|c| {
            let tx = c.transaction()?;
            let mut reuse: HashMap<String, Vec<u8>> = HashMap::new();
            if let Some(model) = target_model {
                let mut stmt = tx.prepare(
                    "SELECT embed_hash, vec FROM chunks WHERE meeting_id = ?1 AND vec IS NOT NULL AND model_id = ?2",
                )?;
                for row in stmt.query_map(params![meeting_id, model], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
                })? {
                    let (h, v) = row?;
                    reuse.insert(h, v);
                }
            }
            tx.execute("DELETE FROM chunks WHERE meeting_id = ?1", [meeting_id])?;
            let mut rows = Vec::new();
            let mut kinds = HashMap::new();
            let mut pending = Vec::new();
            for ch in &chunks {
                let hash = chunk::embed_hash(ch);
                let reused: Option<&Vec<u8>> = if ch.embed { reuse.get(&hash) } else { None };
                let row_model: Option<&str> = if reused.is_some() { target_model } else { None };
                tx.execute(
                    "INSERT INTO chunks(meeting_id, kind, source_id, ord, start_ms, end_ms, seg_first_idx, seg_last_idx, section, speakers_json, text, embed_hash, vec, model_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    params![
                        meeting_id,
                        ch.kind,
                        ch.source_id,
                        ch.ord,
                        ch.start_ms,
                        ch.end_ms,
                        ch.seg_first_idx,
                        ch.seg_last_idx,
                        ch.section,
                        serde_json::to_string(&ch.speakers)?,
                        ch.text,
                        hash,
                        reused,
                        row_model
                    ],
                )?;
                let id = tx.last_insert_rowid();
                kinds.insert(id, ch.kind);
                match reused {
                    Some(v) => rows.push((id, blob_to_f32s(v))),
                    None if ch.embed && target_model.is_some() => {
                        pending.push(Pending { id, title: ch.embed_title.clone(), text: ch.text.clone() })
                    }
                    None => {}
                }
            }
            tx.execute(
                "INSERT INTO search_index(meeting_id, fingerprint, model_id, state, error, indexed_at) VALUES (?1, ?2, NULL, 'pending', NULL, NULL)
                 ON CONFLICT(meeting_id) DO UPDATE SET fingerprint = excluded.fingerprint, state = 'pending', error = NULL",
                params![meeting_id, fp],
            )?;
            tx.commit()?;
            Ok((rows, kinds, pending))
        })?;

    // 2. Embed what is missing, in batches, outside the DB lock.
    let mut embedded = 0usize;
    if let (Some(model), Some(embed)) = (target_model, embed)
        && !pending.is_empty()
    {
        for batch in pending.chunks(EMBED_BATCH) {
            let docs: Vec<(Option<String>, String)> = batch.iter().map(|p| (p.title.clone(), p.text.clone())).collect();
            let vecs = match embed(&docs) {
                Ok(v) if v.len() == batch.len() && v.iter().all(|x| x.len() == DIM) => v,
                Ok(v) => {
                    let msg = format!("embedder returned {} vectors for {} documents", v.len(), batch.len());
                    mark_failed(&ctx.db, meeting_id, &msg)?;
                    bail!("{msg}");
                }
                Err(e) => {
                    mark_failed(&ctx.db, meeting_id, &format!("{e:#}"))?;
                    return Err(e.context("embedding chunks"));
                }
            };
            ctx.db.with_mut(|c| {
                let tx = c.transaction()?;
                for (p, v) in batch.iter().zip(&vecs) {
                    tx.execute(
                        "UPDATE chunks SET vec = ?2, model_id = ?3 WHERE id = ?1",
                        params![p.id, to_blob(v), model],
                    )?;
                }
                tx.commit()?;
                Ok(())
            })?;
            for (p, v) in batch.iter().zip(vecs) {
                rows.push((p.id, v));
            }
            embedded += batch.len();
        }
    }

    // 3. In-memory index plus the whole-meeting vector used for "related meetings".
    ctx.vectors.replace_meeting(meeting_id, &rows);
    let have_vectors = !rows.is_empty();
    let mv = meeting_vector(&rows, &kinds);
    ctx.db.with(|c| {
        match &mv {
            Some(v) => c.execute(
                "INSERT INTO meeting_vectors(meeting_id, vec, model_id, updated_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(meeting_id) DO UPDATE SET vec = excluded.vec, model_id = excluded.model_id, updated_at = excluded.updated_at",
                params![meeting_id, to_blob(v), target_model.unwrap_or(""), now_iso()],
            )?,
            None => c.execute("DELETE FROM meeting_vectors WHERE meeting_id = ?1", [meeting_id])?,
        };
        Ok(())
    })?;
    ctx.vectors.set_meeting_vector(meeting_id, mv);

    // 4. Record the outcome. `model_id` stays NULL while any chunk lacks its vector.
    let indexed_model = if target_model.is_some() && pending.len() == embedded { target_model } else { None };
    ctx.db.with(|c| {
        c.execute(
            "UPDATE search_index SET model_id = ?2, state = 'indexed', error = NULL, indexed_at = ?3 WHERE meeting_id = ?1",
            params![meeting_id, indexed_model, now_iso()],
        )?;
        Ok(())
    })?;
    Ok(ReconcileOutcome::Rebuilt { chunks: chunks.len(), embedded, vectors: have_vectors })
}

fn mark_failed(db: &Db, meeting_id: &str, error: &str) -> Result<()> {
    db.with(|c| {
        c.execute(
            "INSERT INTO search_index(meeting_id, fingerprint, state, error) VALUES (?1, '', 'failed', ?2)
             ON CONFLICT(meeting_id) DO UPDATE SET state = 'failed', error = excluded.error",
            params![meeting_id, error],
        )?;
        Ok(())
    })
}

/// Normalised mean of the summary chunk vectors (transcript chunks when there is no summary).
fn meeting_vector(rows: &[(i64, Vec<f32>)], kinds: &HashMap<i64, &'static str>) -> Option<Vec<f32>> {
    let pick = |kind: &str| -> Vec<&Vec<f32>> {
        rows.iter().filter(|(id, _)| kinds.get(id).copied() == Some(kind)).map(|(_, v)| v).collect()
    };
    let mut chosen = pick("summary");
    if chosen.is_empty() {
        chosen = pick("transcript");
    }
    if chosen.is_empty() {
        return None;
    }
    let mut mean = vec![0f32; DIM];
    for v in &chosen {
        for (m, x) in mean.iter_mut().zip(v.iter()) {
            *m += x;
        }
    }
    normalise(&mut mean);
    Some(mean)
}

// ---- queue ------------------------------------------------------------------

/// Meetings waiting to be reconciled, drained one at a time by [`drain`].
#[derive(Default)]
pub struct Indexer {
    queue: Mutex<VecDeque<String>>,
    notify: Notify,
    running: AtomicBool,
}

impl Indexer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueue a meeting (deduplicated) and wake the drain task.
    pub fn request(&self, meeting_id: &str) {
        if let Ok(mut q) = self.queue.lock()
            && !q.iter().any(|m| m == meeting_id)
        {
            q.push_back(meeting_id.to_string());
        }
        self.notify.notify_one();
    }

    /// Enqueue every meeting whose index is missing, failed, or built with another model.
    pub fn request_stale(&self, db: &Db, target_model: Option<&str>) -> Result<usize> {
        let ids: Vec<String> = db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT m.id FROM meetings m LEFT JOIN search_index si ON si.meeting_id = m.id
                 WHERE m.state NOT IN ('new', 'recording')
                   AND (si.meeting_id IS NULL OR si.state != 'indexed' OR (?1 IS NOT NULL AND si.model_id IS NOT ?1))
                 ORDER BY COALESCE(m.started_at, m.created_at) DESC",
            )?;
            let rows = stmt.query_map([target_model], |r| r.get(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        let n = ids.len();
        for id in ids {
            self.request(&id);
        }
        Ok(n)
    }

    /// Forget every meeting's fingerprint so the next `request_stale` rebuilds all of them.
    pub fn wipe(db: &Db) -> Result<()> {
        db.with(|c| {
            c.execute("DELETE FROM search_index", [])?;
            Ok(())
        })
    }

    pub fn pop(&self) -> Option<String> {
        self.queue.lock().ok().and_then(|mut q| q.pop_front())
    }

    pub fn queued(&self) -> usize {
        self.queue.lock().map(|q| q.len()).unwrap_or(0)
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn status(&self, db: &Db, target_model: Option<&str>) -> Result<IndexStatus> {
        let (total, indexed, failed): (u32, u32, u32) = db.with(|c| {
            Ok(c.query_row(
                "SELECT (SELECT COUNT(*) FROM meetings WHERE state NOT IN ('new', 'recording')),
                        (SELECT COUNT(*) FROM search_index si JOIN meetings m ON m.id = si.meeting_id
                           WHERE si.state = 'indexed' AND (?1 IS NULL OR si.model_id IS ?1)),
                        (SELECT COUNT(*) FROM search_index WHERE state = 'failed')",
                [target_model],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?)
        })?;
        Ok(IndexStatus {
            indexed,
            pending: total.saturating_sub(indexed).saturating_sub(failed),
            failed,
            total,
            queued: self.queued() as u32,
            running: self.is_running(),
            model_id: target_model.map(str::to_string),
            semantic_available: target_model.is_some(),
        })
    }
}

/// Background drain: coalesce bursts, wait out a running summary, then reconcile
/// one meeting at a time on a blocking thread. Never returns.
pub async fn drain(
    indexer: Arc<Indexer>,
    ctx: IndexCtx,
    jobs: Arc<crate::summarise::SummaryJobs>,
    emit: Emit,
    on_done: Arc<dyn Fn() + Send + Sync>,
) {
    loop {
        if indexer.queued() == 0 {
            indexer.notify.notified().await;
        }
        tokio::time::sleep(COALESCE).await;
        let t0 = Instant::now();
        while jobs.is_running().await && t0.elapsed() < SUMMARY_WAIT_MAX {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        while let Some(id) = indexer.pop() {
            indexer.running.store(true, Ordering::Relaxed);
            let (ctx2, id2) = (ctx.clone(), id.clone());
            let result = tokio::task::spawn_blocking(move || reconcile(&ctx2, &id2)).await;
            indexer.running.store(false, Ordering::Relaxed);
            let pending = indexer.queued() as u32;
            match result {
                Ok(Ok(outcome)) => {
                    if let ReconcileOutcome::Rebuilt { chunks, embedded, .. } = &outcome {
                        tracing::info!("indexed {id}: {chunks} chunks, {embedded} embedded, {pending} queued");
                    }
                    emit(&IndexProgress { meeting_id: id, state: "indexed".into(), pending, error: None });
                }
                Ok(Err(e)) => {
                    tracing::warn!("indexing {id} failed: {e:#}");
                    emit(&IndexProgress {
                        meeting_id: id,
                        state: "failed".into(),
                        pending,
                        error: Some(format!("{e:#}")),
                    });
                }
                Err(e) => {
                    tracing::error!("indexing {id} panicked: {e}");
                    emit(&IndexProgress {
                        meeting_id: id,
                        state: "failed".into(),
                        pending,
                        error: Some(e.to_string()),
                    });
                }
            }
            on_done();
            // A summary that started meanwhile gets the GPU back; we resume afterwards.
            if jobs.is_running().await {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn ctx() -> IndexCtx {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let vectors = Arc::new(VectorIndex::new());
        vectors.ensure_loaded(&db).unwrap();
        IndexCtx {
            db,
            paths: Paths { root: std::env::temp_dir(), models: std::env::temp_dir() },
            manifest: Manifest::embedded().unwrap(),
            vectors,
        }
    }

    /// A deterministic fake embedder: a unit vector derived from the text's hash.
    fn fake_embed(docs: &[(Option<String>, String)]) -> Result<Vec<Vec<f32>>> {
        Ok(docs
            .iter()
            .map(|(t, d)| {
                let mut v = vec![0f32; DIM];
                let mut h = Sha256::new();
                h.update(t.as_deref().unwrap_or("").as_bytes());
                h.update(d.as_bytes());
                for (i, b) in h.finalize().iter().enumerate() {
                    v[i % DIM] += *b as f32 / 255.0;
                }
                normalise(&mut v);
                v
            })
            .collect())
    }

    fn fixture(c: &Connection) -> String {
        let m = meetings::insert_meeting(c, "Pricing call", "imported").unwrap();
        meetings::set_state(c, &m.id, "transcribed").unwrap();
        c.execute(
            "INSERT INTO transcripts(id, meeting_id, engine_id, created_at) VALUES ('t1', ?1, 'parakeet', ?2)",
            params![m.id, now_iso()],
        )
        .unwrap();
        for (i, (label, text)) in [
            ("S1", "Let us talk about the pricing tiers."),
            ("S2", "The enterprise tier needs SSO."),
            ("S1", "Agreed, SSO it is."),
        ]
        .iter()
        .enumerate()
        {
            c.execute(
                "INSERT INTO segments(transcript_id, idx, start_ms, end_ms, text, diar_label) VALUES ('t1', ?1, ?2, ?3, ?4, ?5)",
                params![i as i64, (i as i64) * 5000, (i as i64) * 5000 + 4000, text, label],
            )
            .unwrap();
        }
        c.execute(
            "INSERT INTO meeting_speakers(meeting_id, diar_label, display_name, speech_ms) VALUES (?1, 'S1', 'Alice', 20000), (?1, 'S2', 'Bob', 16000)",
            [&m.id],
        )
        .unwrap();
        c.execute(
            "INSERT INTO summaries(id, meeting_id, model_id, body_md, status, created_at) VALUES ('sum1', ?1, 'llm', '## TL;DR\nSSO for enterprise.\n\n## Actions and takeaways\nNone noted.\n', 'done', ?2)",
            params![m.id, now_iso()],
        )
        .unwrap();
        m.id
    }

    fn chunk_ids(db: &Db, meeting: &str) -> Vec<(i64, String, bool)> {
        db.with(|c| {
            let mut stmt =
                c.prepare("SELECT id, kind, vec IS NOT NULL FROM chunks WHERE meeting_id = ?1 ORDER BY kind, ord")?;
            Ok(stmt
                .query_map([meeting], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .unwrap()
    }

    #[test]
    fn reconcile_builds_fts_and_is_idempotent_without_a_model() {
        let ctx = ctx();
        let id = ctx.db.with(|c| Ok(fixture(c))).unwrap();
        let out = reconcile_with(&ctx, &id, None, None).unwrap();
        assert!(matches!(out, ReconcileOutcome::Rebuilt { chunks: 3, embedded: 0, vectors: false }), "{out:?}");
        let first = chunk_ids(&ctx.db, &id);
        assert_eq!(
            first.iter().map(|(_, k, _)| k.as_str()).collect::<Vec<_>>(),
            vec!["summary", "title", "transcript"]
        );
        // FTS finds the transcript by a word, with speaker names rendered in.
        let hits: Vec<String> = ctx
            .db
            .with(|c| {
                let mut stmt = c.prepare("SELECT text FROM chunks_fts WHERE chunks_fts MATCH '\"enterprise\"'")?;
                Ok(stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
            })
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|t| t.contains("Bob: The enterprise tier")));
        assert_eq!(reconcile_with(&ctx, &id, None, None).unwrap(), ReconcileOutcome::Unchanged);
        assert_eq!(chunk_ids(&ctx.db, &id), first, "an unchanged meeting keeps its rows");
        let st = Indexer::new().status(&ctx.db, None).unwrap();
        assert_eq!((st.total, st.indexed, st.pending, st.failed), (1, 1, 0, 0));
        assert!(!st.semantic_available);
    }

    #[test]
    fn vectors_are_reused_by_hash_and_renames_rewrite_chunks() {
        let ctx = ctx();
        let id = ctx.db.with(|c| Ok(fixture(c))).unwrap();
        let out = reconcile_with(&ctx, &id, Some("fake"), Some(&fake_embed)).unwrap();
        assert!(matches!(out, ReconcileOutcome::Rebuilt { chunks: 3, embedded: 3, vectors: true }), "{out:?}");
        assert!(ctx.vectors.has_meeting_vector(&id));
        assert_eq!(ctx.vectors.len(), 3);
        let before = chunk_ids(&ctx.db, &id);
        assert!(before.iter().all(|(_, _, has_vec)| *has_vec));

        // A notes edit changes the fingerprint; only the new notes chunk needs embedding.
        ctx.db
            .with(|c| {
                c.execute("UPDATE meetings SET notes_md = 'remember the discount' WHERE id = ?1", [&id])?;
                Ok(())
            })
            .unwrap();
        let out = reconcile_with(&ctx, &id, Some("fake"), Some(&fake_embed)).unwrap();
        assert!(matches!(out, ReconcileOutcome::Rebuilt { chunks: 4, embedded: 1, .. }), "{out:?}");

        // A speaker rename rewrites the transcript text (and its vector), FTS sees the new name.
        ctx.db
            .with(|c| {
                c.execute(
                    "UPDATE meeting_speakers SET display_name = 'Carol' WHERE meeting_id = ?1 AND diar_label = 'S2'",
                    [&id],
                )?;
                Ok(())
            })
            .unwrap();
        let out = reconcile_with(&ctx, &id, Some("fake"), Some(&fake_embed)).unwrap();
        assert!(matches!(out, ReconcileOutcome::Rebuilt { embedded: 1, .. }), "{out:?}");
        let n: i64 = ctx
            .db
            .with(|c| {
                Ok(c.query_row("SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH '\"Carol\"'", [], |r| r.get(0))?)
            })
            .unwrap();
        assert_eq!(n, 1);

        // Rename back: the old vector is still in the table under its hash? No: rows were
        // replaced, so the hash reuse only spans one generation. Verify no re-embedding
        // happens when nothing changed at all.
        assert_eq!(reconcile_with(&ctx, &id, Some("fake"), Some(&fake_embed)).unwrap(), ReconcileOutcome::Unchanged);

        // Model mismatch is picked up by request_stale, and indexing without the
        // embedder leaves model_id NULL.
        let idx = Indexer::new();
        assert_eq!(idx.request_stale(&ctx.db, Some("other-model")).unwrap(), 1);
        assert_eq!(idx.pop().as_deref(), Some(id.as_str()));
        assert_eq!(idx.request_stale(&ctx.db, Some("fake")).unwrap(), 0);
        let st = idx.status(&ctx.db, Some("fake")).unwrap();
        assert_eq!((st.indexed, st.pending), (1, 0));
        assert!(st.semantic_available);
    }

    #[test]
    fn missing_embedder_leaves_model_null_and_deletion_cascades() {
        let ctx = ctx();
        let id = ctx.db.with(|c| Ok(fixture(c))).unwrap();
        let out = reconcile_with(&ctx, &id, Some("fake"), None).unwrap();
        assert!(matches!(out, ReconcileOutcome::Rebuilt { embedded: 0, vectors: false, .. }));
        let model: Option<String> = ctx
            .db
            .with(|c| Ok(c.query_row("SELECT model_id FROM search_index WHERE meeting_id = ?1", [&id], |r| r.get(0))?))
            .unwrap();
        assert_eq!(model, None);
        let st = Indexer::new().status(&ctx.db, Some("fake")).unwrap();
        assert_eq!((st.indexed, st.pending), (0, 1), "no vectors yet counts as pending");

        ctx.db
            .with(|c| {
                c.execute("DELETE FROM meetings WHERE id = ?1", [&id])?;
                Ok(())
            })
            .unwrap();
        let (chunks, fts): (i64, i64) = ctx
            .db
            .with(|c| {
                Ok((
                    c.query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))?,
                    c.query_row("SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH '\"pricing\"'", [], |r| {
                        r.get(0)
                    })?,
                ))
            })
            .unwrap();
        assert_eq!((chunks, fts), (0, 0), "cascade and trigger empty the index");
        assert_eq!(reconcile_with(&ctx, &id, None, None).unwrap(), ReconcileOutcome::Missing);
    }

    #[test]
    fn facets_text_groups_by_kind() {
        let ctx = ctx();
        let id = ctx.db.with(|c| Ok(fixture(c))).unwrap();
        ctx.db
            .with(|c| {
                c.execute(
                    "INSERT INTO meeting_facets(meeting_id, kind, value, norm) VALUES (?1, 'topic', 'Pricing', 'pricing'), (?1, 'topic', 'SSO', 'sso'), (?1, 'type', 'planning', 'planning')",
                    [&id],
                )?;
                Ok(())
            })
            .unwrap();
        let t = ctx.db.with(|c| facets_text(c, &id)).unwrap().unwrap();
        assert_eq!(t, "Type: planning\nTopics: Pricing, SSO");
        let out = reconcile_with(&ctx, &id, None, None).unwrap();
        assert!(matches!(out, ReconcileOutcome::Rebuilt { chunks: 4, .. }), "{out:?}");
        let n: i64 = ctx
            .db
            .with(|c| {
                Ok(c.query_row("SELECT count(*) FROM chunks WHERE kind = 'facets' AND vec IS NULL", [], |r| r.get(0))?)
            })
            .unwrap();
        assert_eq!(n, 1);
    }
}
