//! Query path: filters narrow the meetings (plain SQL), FTS5 and the vector
//! index each rank chunks within them, reciprocal rank fusion merges the two,
//! and meetings are presented with their best snippets.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use rusqlite::{Connection, ToSql, params};
use serde::{Deserialize, Serialize};

use super::index::{IndexCtx, IndexStatus, Indexer};
use super::vectors::VecHit;
use crate::meetings::{self, Meeting};

/// Diarisation clusters with less speech than this are not counted as participants.
pub const MIN_SPEECH_MS: i64 = 15_000;
/// Semantic hits below this cosine are noise.
pub const MIN_COSINE: f32 = 0.35;
const RRF_K: f32 = 60.0;
const TOP_CHUNKS: usize = 200;
const SNIPPETS_PER_MEETING: usize = 3;
const SNIPPET_CHARS: usize = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SpeakerMode {
    #[default]
    All,
    Any,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FacetRef {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SearchRequest {
    pub query: String,
    /// Speaker ids, or names for clusters that were named but not linked.
    pub speaker_ids: Vec<String>,
    pub speaker_mode: SpeakerMode,
    /// ISO dates (YYYY-MM-DD), inclusive, on the meeting's start (or creation) date.
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    pub min_speakers: Option<u32>,
    pub max_speakers: Option<u32>,
    pub min_duration_ms: Option<i64>,
    pub max_duration_ms: Option<i64>,
    pub source: Option<String>,
    pub has_summary: Option<bool>,
    pub facets: Vec<FacetRef>,
    /// Chunk kinds to search; empty = all.
    pub kinds: Vec<String>,
    pub limit: usize,
    pub offset: usize,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            speaker_ids: Vec::new(),
            speaker_mode: SpeakerMode::All,
            date_from: None,
            date_to: None,
            min_speakers: None,
            max_speakers: None,
            min_duration_ms: None,
            max_duration_ms: None,
            source: None,
            has_summary: None,
            facets: Vec::new(),
            kinds: Vec::new(),
            limit: 20,
            offset: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HitSpeaker {
    pub diar_label: String,
    pub name: String,
    pub speaker_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snippet {
    pub chunk_id: i64,
    pub kind: String,
    pub section: Option<String>,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub speaker: Option<String>,
    /// Keyword hits carry `«»` around matches; semantic snippets are plain text.
    pub text: String,
    pub score: f32,
    /// "semantic" | "keyword" | "both"
    pub matched_by: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub meeting: Meeting,
    pub score: f32,
    pub speakers: Vec<HitSpeaker>,
    pub speaker_count: u32,
    pub snippets: Vec<Snippet>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    /// Meetings that passed the filters (before ranking and paging).
    pub candidates: usize,
    /// Vectors took part in the ranking.
    pub semantic: bool,
    pub total_hits: usize,
    pub index: IndexStatus,
}

// ---- candidates -------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Candidate {
    pub meeting: Meeting,
    pub speaker_count: u32,
}

/// Meetings that satisfy the filters, newest first.
pub fn candidates(c: &Connection, req: &SearchRequest) -> Result<Vec<Candidate>> {
    let mut clauses: Vec<String> = vec!["m.state NOT IN ('new', 'recording')".into()];
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    let bind = |args: &mut Vec<Box<dyn ToSql>>, v: Box<dyn ToSql>| -> String {
        args.push(v);
        format!("?{}", args.len())
    };
    if let Some(d) = req.date_from.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        let p = bind(&mut args, Box::new(d.to_string()));
        clauses.push(format!("substr(COALESCE(m.started_at, m.created_at), 1, 10) >= {p}"));
    }
    if let Some(d) = req.date_to.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        let p = bind(&mut args, Box::new(d.to_string()));
        clauses.push(format!("substr(COALESCE(m.started_at, m.created_at), 1, 10) <= {p}"));
    }
    if let Some(ms) = req.min_duration_ms {
        let p = bind(&mut args, Box::new(ms));
        clauses.push(format!("m.duration_ms >= {p}"));
    }
    if let Some(ms) = req.max_duration_ms {
        let p = bind(&mut args, Box::new(ms));
        clauses.push(format!("m.duration_ms <= {p}"));
    }
    if let Some(s) = req.source.as_deref().filter(|s| !s.is_empty()) {
        let p = bind(&mut args, Box::new(s.to_string()));
        clauses.push(format!("m.source = {p}"));
    }
    if let Some(has) = req.has_summary {
        let e = "EXISTS (SELECT 1 FROM summaries s WHERE s.meeting_id = m.id AND s.status = 'done')";
        clauses.push(if has { e.to_string() } else { format!("NOT {e}") });
    }
    let speakers: Vec<&str> = req.speaker_ids.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    if !speakers.is_empty() {
        let mut parts = Vec::new();
        for s in speakers {
            let p1 = bind(&mut args, Box::new(s.to_string()));
            let p2 = bind(&mut args, Box::new(s.to_string()));
            let p3 = bind(&mut args, Box::new(s.to_string()));
            parts.push(format!(
                "EXISTS (SELECT 1 FROM meeting_speakers ms LEFT JOIN speakers sp ON sp.id = ms.speaker_id
                         WHERE ms.meeting_id = m.id AND (ms.speaker_id = {p1} OR lower(ms.display_name) = lower({p2}) OR lower(sp.name) = lower({p3})))"
            ));
        }
        let joiner = if req.speaker_mode == SpeakerMode::Any { " OR " } else { " AND " };
        clauses.push(format!("({})", parts.join(joiner)));
    }
    for f in &req.facets {
        let norm = f.value.trim().to_lowercase();
        if norm.is_empty() {
            continue;
        }
        let pk = bind(&mut args, Box::new(f.kind.clone()));
        let pv = bind(&mut args, Box::new(norm));
        clauses.push(format!(
            "EXISTS (SELECT 1 FROM meeting_facets f WHERE f.meeting_id = m.id AND f.kind = {pk} AND f.norm = {pv})"
        ));
    }
    let mut outer: Vec<String> = Vec::new();
    if let Some(n) = req.min_speakers {
        let p = bind(&mut args, Box::new(n as i64));
        outer.push(format!("speaker_count >= {p}"));
    }
    if let Some(n) = req.max_speakers {
        let p = bind(&mut args, Box::new(n as i64));
        outer.push(format!("speaker_count <= {p}"));
    }
    let sql = format!(
        "SELECT * FROM (
            SELECT {cols}, (SELECT COUNT(*) FROM meeting_speakers ms WHERE ms.meeting_id = m.id AND ms.speech_ms >= {min_speech}) AS speaker_count
            FROM meetings m WHERE {inner}
         ) WHERE {outer}
         ORDER BY COALESCE(started_at, created_at) DESC",
        cols = meetings::MEETING_COLUMNS,
        min_speech = MIN_SPEECH_MS,
        inner = clauses.join(" AND "),
        outer = if outer.is_empty() { "1 = 1".to_string() } else { outer.join(" AND ") },
    );
    let mut stmt = c.prepare(&sql).context("candidate query")?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| {
        Ok(Candidate { meeting: meetings::row_to_meeting(r)?, speaker_count: r.get::<_, i64>("speaker_count")? as u32 })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---- keyword ----------------------------------------------------------------

/// Quoted FTS5 terms from free text: `"phrases"` stay intact, other words are
/// split on non-alphanumerics, and the last loose word gets a prefix star.
pub fn fts_terms(q: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut last_loose: Option<usize> = None;
    let mut rest = q;
    while !rest.is_empty() {
        match rest.find('"') {
            Some(start) => {
                let (before, after) = rest.split_at(start);
                push_words(before, &mut terms, &mut last_loose);
                let after = &after[1..];
                match after.find('"') {
                    Some(end) => {
                        let phrase: Vec<&str> =
                            after[..end].split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
                        if !phrase.is_empty() {
                            terms.push(format!("\"{}\"", phrase.join(" ")));
                            last_loose = None;
                        }
                        rest = &after[end + 1..];
                    }
                    None => {
                        push_words(after, &mut terms, &mut last_loose);
                        rest = "";
                    }
                }
            }
            None => {
                push_words(rest, &mut terms, &mut last_loose);
                rest = "";
            }
        }
    }
    if let Some(i) = last_loose
        && terms[i].len() >= 4
    {
        terms[i].push('*');
    }
    terms
}

fn push_words(s: &str, terms: &mut Vec<String>, last_loose: &mut Option<usize>) {
    for w in s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
        terms.push(format!("\"{w}\""));
        *last_loose = Some(terms.len() - 1);
    }
}

#[derive(Debug, Clone)]
pub struct KwHit {
    pub chunk_id: i64,
    pub meeting_id: String,
    pub rank: f64,
    pub snippet: String,
}

fn keyword_search(
    c: &Connection,
    match_expr: &str,
    candidate_ids: Option<&str>,
    kinds_json: Option<&str>,
) -> Result<Vec<KwHit>> {
    let mut stmt = c.prepare(
        "SELECT c.id, c.meeting_id, bm25(chunks_fts) AS rank, snippet(chunks_fts, 0, '«', '»', '…', 24)
         FROM chunks_fts JOIN chunks c ON c.id = chunks_fts.rowid
         WHERE chunks_fts MATCH ?1
           AND (?2 IS NULL OR c.meeting_id IN (SELECT value FROM json_each(?2)))
           AND (?3 IS NULL OR c.kind IN (SELECT value FROM json_each(?3)))
         ORDER BY rank LIMIT ?4",
    )?;
    let rows = stmt.query_map(params![match_expr, candidate_ids, kinds_json, TOP_CHUNKS as i64], |r| {
        Ok(KwHit { chunk_id: r.get(0)?, meeting_id: r.get(1)?, rank: r.get(2)?, snippet: r.get(3)? })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// AND the terms first; fall back to OR when nothing matches.
pub fn keyword_hits(
    c: &Connection,
    query: &str,
    candidate_ids: Option<&str>,
    kinds_json: Option<&str>,
) -> Result<Vec<KwHit>> {
    let terms = fts_terms(query);
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let hits = keyword_search(c, &terms.join(" "), candidate_ids, kinds_json)?;
    if !hits.is_empty() || terms.len() == 1 {
        return Ok(hits);
    }
    keyword_search(c, &terms.join(" OR "), candidate_ids, kinds_json)
}

// ---- fusion -----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Fused {
    pub chunk_id: i64,
    pub meeting_id: String,
    pub score: f32,
    pub keyword: bool,
    pub semantic: bool,
    pub snippet: Option<String>,
}

impl Fused {
    pub fn matched_by(&self) -> &'static str {
        match (self.keyword, self.semantic) {
            (true, true) => "both",
            (true, false) => "keyword",
            _ => "semantic",
        }
    }
}

/// Reciprocal rank fusion of the two ranked lists, best first.
pub fn fuse(keyword: &[KwHit], semantic: &[VecHit]) -> Vec<Fused> {
    let mut map: HashMap<i64, Fused> = HashMap::new();
    for (rank, h) in keyword.iter().enumerate() {
        let e = map.entry(h.chunk_id).or_insert_with(|| Fused {
            chunk_id: h.chunk_id,
            meeting_id: h.meeting_id.clone(),
            score: 0.0,
            keyword: false,
            semantic: false,
            snippet: None,
        });
        e.score += 1.0 / (RRF_K + rank as f32 + 1.0);
        e.keyword = true;
        e.snippet = Some(h.snippet.clone());
    }
    for (rank, h) in semantic.iter().enumerate() {
        let e = map.entry(h.chunk_id).or_insert_with(|| Fused {
            chunk_id: h.chunk_id,
            meeting_id: h.meeting_id.clone(),
            score: 0.0,
            keyword: false,
            semantic: false,
            snippet: None,
        });
        e.score += 1.0 / (RRF_K + rank as f32 + 1.0);
        e.semantic = true;
    }
    let mut out: Vec<Fused> = map.into_values().collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.chunk_id.cmp(&b.chunk_id)));
    out
}

#[derive(Debug, Clone)]
pub struct MeetingGroup {
    pub meeting_id: String,
    pub score: f32,
    /// Best first.
    pub chunks: Vec<Fused>,
}

/// Group fused chunks by meeting: best chunk plus a small bonus for breadth.
pub fn group_by_meeting(fused: Vec<Fused>) -> Vec<MeetingGroup> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<Fused>> = HashMap::new();
    for f in fused {
        if !groups.contains_key(&f.meeting_id) {
            order.push(f.meeting_id.clone());
        }
        groups.entry(f.meeting_id.clone()).or_default().push(f);
    }
    let mut out: Vec<MeetingGroup> = order
        .into_iter()
        .map(|id| {
            let chunks = groups.remove(&id).unwrap_or_default();
            let best = chunks.first().map(|c| c.score).unwrap_or(0.0);
            MeetingGroup { meeting_id: id, score: best + 0.05 * (1.0 + chunks.len() as f32).ln(), chunks }
        })
        .collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.meeting_id.cmp(&b.meeting_id)));
    out
}

// ---- assembly ---------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ChunkRow {
    pub id: i64,
    pub meeting_id: String,
    pub kind: String,
    pub section: Option<String>,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub seg_first_idx: Option<i64>,
    pub seg_last_idx: Option<i64>,
    pub speakers: Vec<String>,
    pub text: String,
}

pub fn chunk_rows(c: &Connection, ids: &[i64]) -> Result<HashMap<i64, ChunkRow>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let json = serde_json::to_string(ids)?;
    let mut stmt = c.prepare(
        "SELECT id, meeting_id, kind, section, start_ms, end_ms, seg_first_idx, seg_last_idx, speakers_json, text
         FROM chunks WHERE id IN (SELECT value FROM json_each(?1))",
    )?;
    let rows = stmt.query_map([json], |r| {
        let speakers_json: Option<String> = r.get(8)?;
        Ok(ChunkRow {
            id: r.get(0)?,
            meeting_id: r.get(1)?,
            kind: r.get(2)?,
            section: r.get(3)?,
            start_ms: r.get(4)?,
            end_ms: r.get(5)?,
            seg_first_idx: r.get(6)?,
            seg_last_idx: r.get(7)?,
            speakers: speakers_json.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default(),
            text: r.get(9)?,
        })
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let row = row?;
        out.insert(row.id, row);
    }
    Ok(out)
}

/// Speakers of the given meetings, loudest first: `meeting_id -> speakers`.
pub fn speakers_for(c: &Connection, meeting_ids: &[String]) -> Result<HashMap<String, Vec<HitSpeaker>>> {
    let mut out: HashMap<String, Vec<HitSpeaker>> = HashMap::new();
    if meeting_ids.is_empty() {
        return Ok(out);
    }
    let json = serde_json::to_string(meeting_ids)?;
    let mut stmt = c.prepare(
        "SELECT ms.meeting_id, ms.diar_label, COALESCE(ms.display_name, sp.name), ms.speaker_id
         FROM meeting_speakers ms LEFT JOIN speakers sp ON sp.id = ms.speaker_id
         WHERE ms.meeting_id IN (SELECT value FROM json_each(?1)) ORDER BY ms.meeting_id, ms.speech_ms DESC",
    )?;
    let rows = stmt.query_map([json], |r| {
        Ok((
            r.get::<_, String>(0)?,
            HitSpeaker {
                diar_label: r.get(1)?,
                name: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                speaker_id: r.get(3)?,
            },
        ))
    })?;
    for row in rows {
        let (mid, mut sp) = row?;
        let list = out.entry(mid).or_default();
        if sp.name.is_empty() {
            sp.name = format!("Speaker {}", list.len() + 1);
        }
        list.push(sp);
    }
    Ok(out)
}

fn plain_snippet(text: &str) -> String {
    let mut s: String = text.chars().take(SNIPPET_CHARS).collect();
    if text.chars().count() > SNIPPET_CHARS {
        s.push('…');
    }
    s
}

fn overlaps(a: &ChunkRow, b: &ChunkRow) -> bool {
    match (a.seg_first_idx, a.seg_last_idx, b.seg_first_idx, b.seg_last_idx) {
        (Some(a0), Some(a1), Some(b0), Some(b1)) => a0 <= b1 && b0 <= a1,
        _ => false,
    }
}

/// Pick up to `n` snippets for one meeting, skipping transcript chunks that
/// overlap one already chosen (adjacent windows share a turn).
pub fn pick_snippets(
    group: &MeetingGroup,
    rows: &HashMap<i64, ChunkRow>,
    names: &HashMap<String, String>,
    n: usize,
) -> Vec<Snippet> {
    let mut chosen: Vec<&ChunkRow> = Vec::new();
    let mut out = Vec::new();
    for f in &group.chunks {
        let Some(row) = rows.get(&f.chunk_id) else { continue };
        if row.kind == "transcript" && chosen.iter().any(|c| c.kind == "transcript" && overlaps(c, row)) {
            continue;
        }
        chosen.push(row);
        let speaker = if row.kind == "transcript" {
            row.speakers.first().map(|l| names.get(l).cloned().unwrap_or_else(|| l.clone()))
        } else {
            None
        };
        out.push(Snippet {
            chunk_id: row.id,
            kind: row.kind.clone(),
            section: row.section.clone(),
            start_ms: row.start_ms,
            end_ms: row.end_ms,
            speaker,
            text: f.snippet.clone().unwrap_or_else(|| plain_snippet(&row.text)),
            score: f.score,
            matched_by: f.matched_by().to_string(),
        });
        if out.len() >= n {
            break;
        }
    }
    out
}

/// Everything the ranking produced, before paging: reused by "Ask".
pub struct Ranked {
    pub candidates: Vec<Candidate>,
    pub groups: Vec<MeetingGroup>,
    pub rows: HashMap<i64, ChunkRow>,
    pub semantic: bool,
}

pub fn rank(ctx: &IndexCtx, req: &SearchRequest) -> Result<Ranked> {
    let cands = ctx.db.with(|c| candidates(c, req))?;
    let query = req.query.trim();
    if query.is_empty() {
        return Ok(Ranked { candidates: cands, groups: Vec::new(), rows: HashMap::new(), semantic: false });
    }
    let ids: Vec<String> = cands.iter().map(|c| c.meeting.id.clone()).collect();
    let id_set: HashSet<String> = ids.iter().cloned().collect();
    let ids_json = serde_json::to_string(&ids)?;
    let kinds_json = if req.kinds.is_empty() { None } else { Some(serde_json::to_string(&req.kinds)?) };

    let keyword = ctx.db.with(|c| keyword_hits(c, query, Some(&ids_json), kinds_json.as_deref()))?;

    let mut semantic_hits: Vec<VecHit> = Vec::new();
    let mut semantic = false;
    if let Some((_, path)) = super::resolve_embedder(&ctx.paths, &ctx.manifest) {
        let embedder = muesli_llm::Embedder::global();
        match embedder.load(&path).and_then(|_| embedder.embed_query(query)) {
            Ok(q) => {
                ctx.vectors.ensure_loaded(&ctx.db)?;
                semantic_hits = ctx
                    .vectors
                    .search(&q, Some(&id_set), TOP_CHUNKS)
                    .into_iter()
                    .filter(|h| h.score >= MIN_COSINE)
                    .collect();
                semantic = true;
            }
            Err(e) => tracing::warn!("semantic search unavailable: {e:#}"),
        }
    }

    let mut fused = fuse(&keyword, &semantic_hits);
    let all_ids: Vec<i64> = fused.iter().map(|f| f.chunk_id).collect();
    let rows = ctx.db.with(|c| chunk_rows(c, &all_ids))?;
    if !req.kinds.is_empty() {
        fused.retain(|f| rows.get(&f.chunk_id).map(|r| req.kinds.contains(&r.kind)).unwrap_or(false));
    }
    Ok(Ranked { candidates: cands, groups: group_by_meeting(fused), rows, semantic })
}

/// The full search: rank, page, attach speakers and snippets.
pub fn search(ctx: &IndexCtx, indexer: &Indexer, req: &SearchRequest) -> Result<SearchResponse> {
    let limit = req.limit.clamp(1, 100);
    let ranked = rank(ctx, req)?;
    let target = super::resolve_embedder(&ctx.paths, &ctx.manifest).map(|(id, _)| id);
    let index = indexer.status(&ctx.db, target.as_deref())?;
    let by_id: HashMap<&str, &Candidate> = ranked.candidates.iter().map(|c| (c.meeting.id.as_str(), c)).collect();

    /// One page of results: `(meeting id, score, ranked group if any)`.
    type Page<'a> = Vec<(String, f32, Option<&'a MeetingGroup>)>;
    let (page_meetings, total_hits): (Page<'_>, usize) = if req.query.trim().is_empty() {
        (
            ranked.candidates.iter().skip(req.offset).take(limit).map(|c| (c.meeting.id.clone(), 0.0, None)).collect(),
            ranked.candidates.len(),
        )
    } else {
        (
            ranked
                .groups
                .iter()
                .skip(req.offset)
                .take(limit)
                .map(|g| (g.meeting_id.clone(), g.score, Some(g)))
                .collect(),
            ranked.groups.len(),
        )
    };
    let page_ids: Vec<String> = page_meetings.iter().map(|(id, _, _)| id.clone()).collect();
    let speakers = ctx.db.with(|c| speakers_for(c, &page_ids))?;
    let mut hits = Vec::new();
    for (id, score, group) in page_meetings {
        let Some(cand) = by_id.get(id.as_str()) else { continue };
        let sp = speakers.get(&id).cloned().unwrap_or_default();
        let names: HashMap<String, String> = sp.iter().map(|s| (s.diar_label.clone(), s.name.clone())).collect();
        let snippets = group.map(|g| pick_snippets(g, &ranked.rows, &names, SNIPPETS_PER_MEETING)).unwrap_or_default();
        hits.push(SearchHit {
            meeting: cand.meeting.clone(),
            score,
            speakers: sp,
            speaker_count: cand.speaker_count,
            snippets,
        });
    }
    Ok(SearchResponse { hits, candidates: ranked.candidates.len(), semantic: ranked.semantic, total_hits, index })
}

// ---- facet counts -----------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeakerCount {
    /// Value to put in `speakerIds`: the speaker id, or the name for unlinked clusters.
    pub id: String,
    pub name: String,
    pub count: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValueCount {
    pub value: String,
    pub count: u32,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Range<T> {
    pub min: Option<T>,
    pub max: Option<T>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FacetCounts {
    pub candidates: usize,
    pub speakers: Vec<SpeakerCount>,
    pub speaker_count: Range<u32>,
    pub dates: Range<String>,
    pub facets: HashMap<String, Vec<ValueCount>>,
}

/// Chip values and counts over the meetings that pass the filters (the query text is ignored).
pub fn facet_counts(ctx: &IndexCtx, req: &SearchRequest) -> Result<FacetCounts> {
    let cands = ctx.db.with(|c| candidates(c, req))?;
    let ids: Vec<String> = cands.iter().map(|c| c.meeting.id.clone()).collect();
    let json = serde_json::to_string(&ids)?;
    let (speakers, facets) = ctx.db.with(|c| {
        let mut stmt = c.prepare(
            "SELECT COALESCE(ms.speaker_id, COALESCE(ms.display_name, sp.name)) AS key, COALESCE(ms.display_name, sp.name) AS name, COUNT(DISTINCT ms.meeting_id) AS n
             FROM meeting_speakers ms LEFT JOIN speakers sp ON sp.id = ms.speaker_id
             WHERE ms.meeting_id IN (SELECT value FROM json_each(?1)) AND ms.speech_ms >= ?2 AND COALESCE(ms.display_name, sp.name) IS NOT NULL AND COALESCE(ms.display_name, sp.name) NOT LIKE 'Speaker %'
             GROUP BY key ORDER BY n DESC, 2 LIMIT 50",
        )?;
        let speakers: Vec<SpeakerCount> = stmt
            .query_map(params![json, MIN_SPEECH_MS], |r| {
                Ok(SpeakerCount { id: r.get(0)?, name: r.get(1)?, count: r.get::<_, i64>(2)? as u32 })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut stmt = c.prepare(
            "SELECT kind, MIN(value), COUNT(*) AS n FROM meeting_facets
             WHERE meeting_id IN (SELECT value FROM json_each(?1)) AND kind IN ('topic', 'type', 'person', 'org', 'project')
             GROUP BY kind, norm ORDER BY kind, n DESC, 2",
        )?;
        let mut facets: HashMap<String, Vec<ValueCount>> = HashMap::new();
        for row in stmt.query_map([&json], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? as u32)))? {
            let (kind, value, count) = row?;
            let list = facets.entry(kind).or_default();
            if list.len() < 30 {
                list.push(ValueCount { value, count });
            }
        }
        Ok((speakers, facets))
    })?;
    let counts: Vec<u32> = cands.iter().map(|c| c.speaker_count).collect();
    let dates: Vec<String> = cands
        .iter()
        .map(|c| {
            c.meeting.started_at.clone().unwrap_or_else(|| c.meeting.created_at.clone()).chars().take(10).collect()
        })
        .collect();
    Ok(FacetCounts {
        candidates: cands.len(),
        speakers,
        speaker_count: Range { min: counts.iter().min().copied(), max: counts.iter().max().copied() },
        dates: Range { min: dates.iter().min().cloned(), max: dates.iter().max().cloned() },
        facets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, now_iso};
    use std::sync::Arc;

    #[test]
    fn fts_terms_quote_phrases_and_prefix_the_last_word() {
        assert_eq!(fts_terms("pricing tiers"), vec!["\"pricing\"", "\"tiers\"*"]);
        assert_eq!(fts_terms("\"pricing tiers\" sso"), vec!["\"pricing tiers\"", "\"sso\"*"]);
        assert_eq!(fts_terms("say \"hello there"), vec!["\"say\"", "\"hello\"", "\"there\"*"]);
        assert_eq!(fts_terms("a b"), vec!["\"a\"", "\"b\""], "short words get no prefix star");
        assert_eq!(fts_terms("  --  "), Vec::<String>::new());
        assert_eq!(fts_terms("O'Brien"), vec!["\"O\"", "\"Brien\"*"]);
    }

    #[test]
    fn rrf_and_grouping() {
        let kw = vec![
            KwHit { chunk_id: 1, meeting_id: "m1".into(), rank: -5.0, snippet: "«a»".into() },
            KwHit { chunk_id: 2, meeting_id: "m2".into(), rank: -4.0, snippet: "«b»".into() },
        ];
        let sem = vec![
            VecHit { chunk_id: 2, meeting_id: "m2".into(), score: 0.9 },
            VecHit { chunk_id: 3, meeting_id: "m1".into(), score: 0.8 },
        ];
        let fused = fuse(&kw, &sem);
        assert_eq!(fused[0].chunk_id, 2, "a chunk in both lists ranks first");
        assert_eq!(fused[0].matched_by(), "both");
        assert!((fused[0].score - (1.0 / 61.0 + 1.0 / 62.0)).abs() < 1e-6);
        assert_eq!(fused[1].chunk_id, 1);
        assert_eq!(fused[1].matched_by(), "keyword");
        assert_eq!(fused[1].snippet.as_deref(), Some("«a»"));
        assert_eq!(fused[2].matched_by(), "semantic");
        let groups = group_by_meeting(fused);
        assert_eq!(groups.len(), 2);
        // m1 has two chunks (1 and 3): its breadth bonus lifts it above m2's single best chunk.
        assert_eq!(groups[0].meeting_id, "m1");
        assert_eq!(groups[0].chunks.len(), 2);
    }

    fn db_with_meetings() -> (Arc<Db>, Vec<String>) {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let ids = db
            .with(|c| {
                let mut ids = Vec::new();
                for (title, source, day, dur, speakers, summary) in [
                    ("Kickoff", "recorded", "2026-10-01", 1_800_000i64, vec![("S1", "Alice", Some("sp-alice")), ("S2", "Bob", None)], true),
                    ("Standup", "recorded", "2026-10-05", 600_000, vec![("S1", "Alice", Some("sp-alice"))], false),
                    ("Imported", "imported", "2026-10-09", 5_400_000, vec![("S1", "Carol", None), ("S2", "Dan", None), ("S3", "Eve", None)], true),
                ] {
                    let m = meetings::insert_meeting(c, title, source)?;
                    meetings::set_state(c, &m.id, "transcribed")?;
                    meetings::set_times(c, &m.id, Some(&format!("{day}T10:00:00.000Z")), None, Some(dur))?;
                    for (label, name, sid) in speakers {
                        if let Some(sid) = sid {
                            c.execute(
                                "INSERT OR IGNORE INTO speakers(id, name, created_at) VALUES (?1, ?2, ?3)",
                                params![sid, name, now_iso()],
                            )?;
                        }
                        c.execute(
                            "INSERT INTO meeting_speakers(meeting_id, diar_label, speaker_id, display_name, speech_ms) VALUES (?1, ?2, ?3, ?4, 30000)",
                            params![m.id, label, sid, name],
                        )?;
                    }
                    if summary {
                        c.execute(
                            "INSERT INTO summaries(id, meeting_id, model_id, body_md, status, created_at) VALUES (?1, ?2, 'llm', 'x', 'done', ?3)",
                            params![format!("sum-{}", m.id), m.id, now_iso()],
                        )?;
                    }
                    ids.push(m.id);
                }
                Ok(ids)
            })
            .unwrap();
        (db, ids)
    }

    fn titles(db: &Db, req: &SearchRequest) -> Vec<String> {
        db.with(|c| candidates(c, req)).unwrap().into_iter().map(|c| c.meeting.title).collect()
    }

    #[test]
    fn candidate_filters() {
        let (db, _) = db_with_meetings();
        let all = titles(&db, &SearchRequest::default());
        assert_eq!(all, vec!["Imported", "Standup", "Kickoff"], "newest first");
        let req = SearchRequest { speaker_ids: vec!["Alice".into()], ..Default::default() };
        assert_eq!(titles(&db, &req), vec!["Standup", "Kickoff"]);
        let req = SearchRequest {
            speaker_ids: vec!["sp-alice".into(), "bob".into()],
            speaker_mode: SpeakerMode::All,
            ..Default::default()
        };
        assert_eq!(titles(&db, &req), vec!["Kickoff"]);
        let req = SearchRequest {
            speaker_ids: vec!["bob".into(), "eve".into()],
            speaker_mode: SpeakerMode::Any,
            ..Default::default()
        };
        assert_eq!(titles(&db, &req), vec!["Imported", "Kickoff"]);
        let req = SearchRequest {
            date_from: Some("2026-10-02".into()),
            date_to: Some("2026-10-08".into()),
            ..Default::default()
        };
        assert_eq!(titles(&db, &req), vec!["Standup"]);
        let req = SearchRequest { min_speakers: Some(2), ..Default::default() };
        assert_eq!(titles(&db, &req), vec!["Imported", "Kickoff"]);
        let req = SearchRequest { max_speakers: Some(1), ..Default::default() };
        assert_eq!(titles(&db, &req), vec!["Standup"]);
        let req = SearchRequest { min_duration_ms: Some(3_600_000), ..Default::default() };
        assert_eq!(titles(&db, &req), vec!["Imported"]);
        let req = SearchRequest { source: Some("imported".into()), ..Default::default() };
        assert_eq!(titles(&db, &req), vec!["Imported"]);
        let req = SearchRequest { has_summary: Some(false), ..Default::default() };
        assert_eq!(titles(&db, &req), vec!["Standup"]);
        let req = SearchRequest {
            facets: vec![FacetRef { kind: "topic".into(), value: "Launch".into() }],
            ..Default::default()
        };
        assert!(titles(&db, &req).is_empty());
    }

    #[test]
    fn facet_counts_over_candidates() {
        let (db, ids) = db_with_meetings();
        db.with(|c| {
            c.execute(
                "INSERT INTO meeting_facets(meeting_id, kind, value, norm) VALUES (?1, 'topic', 'Launch', 'launch'), (?2, 'topic', 'launch', 'launch'), (?1, 'type', 'planning', 'planning')",
                params![ids[0], ids[2]],
            )?;
            Ok(())
        })
        .unwrap();
        let ctx = IndexCtx {
            db: db.clone(),
            paths: crate::paths::Paths { root: std::env::temp_dir(), models: std::env::temp_dir() },
            manifest: crate::models::Manifest::embedded().unwrap(),
            vectors: Arc::new(super::super::vectors::VectorIndex::new()),
        };
        let fc = facet_counts(&ctx, &SearchRequest::default()).unwrap();
        assert_eq!(fc.candidates, 3);
        let alice = fc.speakers.iter().find(|s| s.name == "Alice").unwrap();
        assert_eq!((alice.id.as_str(), alice.count), ("sp-alice", 2));
        assert!(fc.speakers.iter().any(|s| s.name == "Eve" && s.id == "Eve"));
        assert_eq!((fc.speaker_count.min, fc.speaker_count.max), (Some(1), Some(3)));
        assert_eq!((fc.dates.min.as_deref(), fc.dates.max.as_deref()), (Some("2026-10-01"), Some("2026-10-09")));
        assert_eq!(fc.facets["topic"][0].count, 2);
        assert_eq!(fc.facets["type"].len(), 1);
        let req = SearchRequest {
            facets: vec![FacetRef { kind: "topic".into(), value: " LAUNCH ".into() }],
            ..Default::default()
        };
        assert_eq!(titles(&db, &req), vec!["Imported", "Kickoff"]);
    }
}
