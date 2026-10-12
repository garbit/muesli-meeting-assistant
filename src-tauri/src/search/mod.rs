//! Meeting search: hybrid keyword (FTS5) + semantic (EmbeddingGemma) retrieval
//! over chunks of transcripts, summaries, notes and titles, with SQL filters.
//!
//! * `chunk`   – pure chunkers
//! * `vectors` – in-memory vector index
//! * `index`   – reconcile(meeting) and the background queue
//! * `query`   – filters, ranking, snippets, facet counts

pub mod chunk;
pub mod index;
pub mod query;
pub mod vectors;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::AppState;
use crate::models::Manifest;
use crate::paths::Paths;
use index::{IndexCtx, IndexStatus, Indexer};
use query::{FacetCounts, SearchRequest, SearchResponse};
use vectors::VectorIndex;

/// Unload the embedding model after this long without a query or indexing run.
const IDLE_UNLOAD: Duration = Duration::from_secs(10 * 60);
/// Related meetings below this cosine are not shown.
const RELATED_MIN_COSINE: f32 = 0.5;

/// Search state shared through `AppState`. Constructing it starts no thread and
/// loads no model, so `--doctor` and tests stay side-effect free.
pub struct SearchState {
    pub vectors: Arc<VectorIndex>,
    pub indexer: Arc<Indexer>,
    generation: AtomicU64,
}

impl Default for SearchState {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchState {
    pub fn new() -> Self {
        Self { vectors: Arc::new(VectorIndex::new()), indexer: Arc::new(Indexer::new()), generation: AtomicU64::new(0) }
    }

    /// Note activity; the embedder is unloaded `IDLE_UNLOAD` after the last touch.
    /// Must be called from within a tokio runtime.
    pub fn touch(self: &Arc<Self>) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let me = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(IDLE_UNLOAD).await;
            if me.generation.load(Ordering::SeqCst) == generation && !me.indexer.is_running() {
                tokio::task::spawn_blocking(|| {
                    if muesli_llm::Embedder::global().info().ok().flatten().is_some()
                        && muesli_llm::Embedder::global().unload().is_ok()
                    {
                        tracing::info!("unloaded idle embedding model");
                    }
                });
            }
        });
    }
}

/// The installed default embedding model: `(model id, GGUF path)`.
pub fn resolve_embedder(paths: &Paths, manifest: &Manifest) -> Option<(String, PathBuf)> {
    let entry = manifest.default_embedding()?;
    let status = crate::models::gguf_status(paths, entry);
    if !status.installed {
        return None;
    }
    let file = entry.files.first()?;
    Some((entry.id.clone(), paths.model_file(&entry.repo, &file.path)))
}

pub fn app_emitter(app: AppHandle) -> index::Emit {
    Arc::new(move |p: &index::IndexProgress| {
        let _ = app.emit("search:index", p);
    })
}

/// Start the background indexer for the app and queue whatever is stale.
pub fn start(app: &AppHandle) {
    let state = app.state::<AppState>();
    let ctx = IndexCtx::from_state(&state);
    let search = state.search.clone();
    let jobs = app.state::<Arc<crate::summarise::SummaryJobs>>().inner().clone();
    let target = resolve_embedder(&state.paths, &state.manifest).map(|(id, _)| id);
    match search.indexer.request_stale(&state.db, target.as_deref()) {
        Ok(n) if n > 0 => tracing::info!("search index: {n} meeting(s) to (re)index"),
        Ok(_) => {}
        Err(e) => tracing::warn!("search index: stale check failed: {e:#}"),
    }
    let search2 = search.clone();
    let on_done: Arc<dyn Fn() + Send + Sync> = Arc::new(move || search2.touch());
    tauri::async_runtime::spawn(index::drain(search.indexer.clone(), ctx, jobs, app_emitter(app.clone()), on_done));
}

// ---- commands -------------------------------------------------------------

type CmdResult<T> = Result<T, String>;

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

#[tauri::command]
pub async fn search_meetings(state: State<'_, AppState>, req: SearchRequest) -> CmdResult<SearchResponse> {
    let ctx = IndexCtx::from_state(&state);
    let search = state.search.clone();
    let r = tokio::task::spawn_blocking(move || query::search(&ctx, &search.indexer, &req))
        .await
        .map_err(|e| e.to_string())?
        .map_err(err)?;
    if r.semantic {
        state.search.touch();
    }
    Ok(r)
}

#[tauri::command]
pub async fn search_facets(state: State<'_, AppState>, req: SearchRequest) -> CmdResult<FacetCounts> {
    let ctx = IndexCtx::from_state(&state);
    tokio::task::spawn_blocking(move || query::facet_counts(&ctx, &req)).await.map_err(|e| e.to_string())?.map_err(err)
}

#[tauri::command]
pub fn search_status(state: State<'_, AppState>) -> CmdResult<IndexStatus> {
    let target = resolve_embedder(&state.paths, &state.manifest).map(|(id, _)| id);
    state.search.indexer.status(&state.db, target.as_deref()).map_err(err)
}

/// Re-index: `all` forgets every fingerprint first, otherwise only stale meetings are queued.
#[tauri::command]
pub fn search_reindex(state: State<'_, AppState>, all: bool) -> CmdResult<u32> {
    if all {
        Indexer::wipe(&state.db).map_err(err)?;
    }
    let target = resolve_embedder(&state.paths, &state.manifest).map(|(id, _)| id);
    state.search.indexer.request_stale(&state.db, target.as_deref()).map(|n| n as u32).map_err(err)
}

/// Load the embedder ahead of the first query (called when the search box gains focus).
#[tauri::command]
pub async fn search_warm(state: State<'_, AppState>) -> CmdResult<bool> {
    let Some((_, path)) = resolve_embedder(&state.paths, &state.manifest) else { return Ok(false) };
    let search = state.search.clone();
    tokio::task::spawn_blocking(move || muesli_llm::Embedder::global().load(&path))
        .await
        .map_err(|e| e.to_string())?
        .map_err(err)?;
    search.touch();
    Ok(true)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelatedMeeting {
    pub meeting: crate::meetings::Meeting,
    pub score: f32,
    pub shared_speakers: Vec<String>,
    pub shared_topics: Vec<String>,
}

/// Meetings whose summary vector is closest to this one's, with what they share.
#[tauri::command]
pub fn meeting_related(state: State<'_, AppState>, id: String, limit: Option<usize>) -> CmdResult<Vec<RelatedMeeting>> {
    let limit = limit.unwrap_or(5).clamp(1, 20);
    state.search.vectors.ensure_loaded(&state.db).map_err(err)?;
    let near = state.search.vectors.related(&id, RELATED_MIN_COSINE, limit);
    if near.is_empty() {
        return Ok(Vec::new());
    }
    state
        .db
        .with(|c| {
            let my_speakers: Vec<String> = {
                let mut stmt = c.prepare("SELECT speaker_id FROM meeting_speakers WHERE meeting_id = ?1 AND speaker_id IS NOT NULL")?;
                stmt.query_map([&id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
            };
            let my_topics: Vec<String> = {
                let mut stmt = c.prepare("SELECT norm FROM meeting_facets WHERE meeting_id = ?1 AND kind = 'topic'")?;
                stmt.query_map([&id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
            };
            let mut out = Vec::new();
            for (other, score) in near {
                let Some(meeting) = crate::meetings::get_meeting(c, &other)? else { continue };
                let mut stmt = c.prepare(
                    "SELECT DISTINCT sp.name FROM meeting_speakers ms JOIN speakers sp ON sp.id = ms.speaker_id
                     WHERE ms.meeting_id = ?1 AND ms.speaker_id IN (SELECT value FROM json_each(?2)) ORDER BY sp.name",
                )?;
                let shared_speakers: Vec<String> = stmt
                    .query_map(rusqlite::params![other, serde_json::to_string(&my_speakers)?], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                let mut stmt = c.prepare(
                    "SELECT value FROM meeting_facets WHERE meeting_id = ?1 AND kind = 'topic' AND norm IN (SELECT value FROM json_each(?2)) ORDER BY value",
                )?;
                let shared_topics: Vec<String> = stmt
                    .query_map(rusqlite::params![other, serde_json::to_string(&my_topics)?], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                out.push(RelatedMeeting { meeting, score, shared_speakers, shared_topics });
            }
            Ok(out)
        })
        .map_err(err)
}
