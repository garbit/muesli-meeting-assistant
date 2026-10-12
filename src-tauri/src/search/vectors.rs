//! In-memory brute-force index over chunk embeddings (unit vectors, dot
//! product). A personal library is tens of thousands of chunks at most, so a
//! linear scan (about 2 ms per 20 k rows) beats any ANN structure plus the
//! filtering headaches it brings. Loaded lazily from `chunks.vec`.

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use anyhow::Result;

use crate::db::Db;
use crate::jobs::blob_to_f32s;

pub const DIM: usize = muesli_llm::EMBED_DIM;

pub fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

#[derive(Default)]
struct Inner {
    loaded: bool,
    data: Vec<f32>,
    chunk_ids: Vec<i64>,
    meeting_of: Vec<u32>,
    alive: Vec<bool>,
    dead: usize,
    meetings: Vec<String>,
    meeting_idx: HashMap<String, u32>,
    rows_by_meeting: HashMap<u32, Vec<usize>>,
    /// Whole-meeting vectors (mean of summary chunks), for "related meetings".
    meeting_vecs: HashMap<String, Vec<f32>>,
}

impl Inner {
    fn meeting_index(&mut self, meeting_id: &str) -> u32 {
        if let Some(i) = self.meeting_idx.get(meeting_id) {
            return *i;
        }
        let i = self.meetings.len() as u32;
        self.meetings.push(meeting_id.to_string());
        self.meeting_idx.insert(meeting_id.to_string(), i);
        i
    }

    fn push(&mut self, chunk_id: i64, meeting: u32, vec: &[f32]) {
        if vec.len() != DIM {
            return;
        }
        let row = self.chunk_ids.len();
        self.data.extend_from_slice(vec);
        self.chunk_ids.push(chunk_id);
        self.meeting_of.push(meeting);
        self.alive.push(true);
        self.rows_by_meeting.entry(meeting).or_default().push(row);
    }

    fn remove(&mut self, meeting_id: &str) {
        let Some(&m) = self.meeting_idx.get(meeting_id) else { return };
        if let Some(rows) = self.rows_by_meeting.remove(&m) {
            for r in rows {
                if self.alive[r] {
                    self.alive[r] = false;
                    self.dead += 1;
                }
            }
        }
        self.meeting_vecs.remove(meeting_id);
    }

    fn compact(&mut self) {
        let n = self.chunk_ids.len();
        if n < 1_000 || self.dead * 5 < n {
            return;
        }
        let mut fresh =
            Inner { loaded: true, meeting_vecs: std::mem::take(&mut self.meeting_vecs), ..Default::default() };
        for r in 0..n {
            if self.alive[r] {
                let mid = self.meetings[self.meeting_of[r] as usize].clone();
                let m = fresh.meeting_index(&mid);
                fresh.push(self.chunk_ids[r], m, &self.data[r * DIM..(r + 1) * DIM]);
            }
        }
        *self = fresh;
    }
}

#[derive(Default)]
pub struct VectorIndex {
    inner: RwLock<Inner>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VecHit {
    pub chunk_id: i64,
    pub meeting_id: String,
    pub score: f32,
}

impl VectorIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_loaded(&self) -> bool {
        self.inner.read().map(|i| i.loaded).unwrap_or(false)
    }

    /// Alive rows.
    pub fn len(&self) -> usize {
        self.inner.read().map(|i| i.chunk_ids.len() - i.dead).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Load every embedded chunk and meeting vector from the database once.
    pub fn ensure_loaded(&self, db: &Db) -> Result<()> {
        if self.is_loaded() {
            return Ok(());
        }
        let rows: Vec<(i64, String, Vec<u8>)> = db.with(|c| {
            let mut stmt =
                c.prepare("SELECT id, meeting_id, vec FROM chunks WHERE vec IS NOT NULL ORDER BY meeting_id, id")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        let meeting_rows: Vec<(String, Vec<u8>)> = db.with(|c| {
            let mut stmt = c.prepare("SELECT meeting_id, vec FROM meeting_vectors")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        let mut inner = self.inner.write().map_err(|_| anyhow::anyhow!("vector index poisoned"))?;
        if inner.loaded {
            return Ok(());
        }
        let mut fresh = Inner { loaded: true, ..Default::default() };
        for (chunk_id, meeting_id, blob) in rows {
            let m = fresh.meeting_index(&meeting_id);
            fresh.push(chunk_id, m, &blob_to_f32s(&blob));
        }
        for (meeting_id, blob) in meeting_rows {
            let v = blob_to_f32s(&blob);
            if v.len() == DIM {
                fresh.meeting_vecs.insert(meeting_id, v);
            }
        }
        tracing::info!("vector index loaded: {} chunks, {} meetings", fresh.chunk_ids.len(), fresh.meeting_vecs.len());
        *inner = fresh;
        Ok(())
    }

    /// Replace a meeting's rows. A no-op before the first load (the load reads the database).
    pub fn replace_meeting(&self, meeting_id: &str, rows: &[(i64, Vec<f32>)]) {
        let Ok(mut inner) = self.inner.write() else { return };
        if !inner.loaded {
            return;
        }
        inner.remove(meeting_id);
        let m = inner.meeting_index(meeting_id);
        for (id, v) in rows {
            inner.push(*id, m, v);
        }
        inner.compact();
    }

    pub fn remove_meeting(&self, meeting_id: &str) {
        let Ok(mut inner) = self.inner.write() else { return };
        if !inner.loaded {
            return;
        }
        inner.remove(meeting_id);
        inner.compact();
    }

    pub fn set_meeting_vector(&self, meeting_id: &str, vec: Option<Vec<f32>>) {
        let Ok(mut inner) = self.inner.write() else { return };
        if !inner.loaded {
            return;
        }
        match vec {
            Some(v) if v.len() == DIM => {
                inner.meeting_vecs.insert(meeting_id.to_string(), v);
            }
            _ => {
                inner.meeting_vecs.remove(meeting_id);
            }
        }
    }

    /// Top `top_k` chunks by dot product (cosine on unit vectors), optionally
    /// restricted to a set of meetings.
    pub fn search(&self, q: &[f32], candidates: Option<&HashSet<String>>, top_k: usize) -> Vec<VecHit> {
        let Ok(inner) = self.inner.read() else { return Vec::new() };
        if q.len() != DIM || top_k == 0 {
            return Vec::new();
        }
        let mut scored: Vec<(f32, usize)> = Vec::new();
        let mut allowed: Option<HashSet<u32>> = None;
        if let Some(c) = candidates {
            allowed = Some(c.iter().filter_map(|m| inner.meeting_idx.get(m).copied()).collect());
        }
        for r in 0..inner.chunk_ids.len() {
            if !inner.alive[r] {
                continue;
            }
            if let Some(a) = &allowed
                && !a.contains(&inner.meeting_of[r])
            {
                continue;
            }
            let row = &inner.data[r * DIM..(r + 1) * DIM];
            let dot: f32 = row.iter().zip(q).map(|(a, b)| a * b).sum();
            scored.push((dot, r));
        }
        if scored.len() > top_k {
            scored.select_nth_unstable_by(top_k, |a, b| b.0.total_cmp(&a.0));
            scored.truncate(top_k);
        }
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        scored
            .into_iter()
            .map(|(score, r)| VecHit {
                chunk_id: inner.chunk_ids[r],
                meeting_id: inner.meetings[inner.meeting_of[r] as usize].clone(),
                score,
            })
            .collect()
    }

    /// Meetings whose whole-meeting vector is closest to `meeting_id`'s.
    pub fn related(&self, meeting_id: &str, min_cos: f32, limit: usize) -> Vec<(String, f32)> {
        let Ok(inner) = self.inner.read() else { return Vec::new() };
        let Some(q) = inner.meeting_vecs.get(meeting_id) else { return Vec::new() };
        let mut out: Vec<(String, f32)> = inner
            .meeting_vecs
            .iter()
            .filter(|(id, _)| id.as_str() != meeting_id)
            .map(|(id, v)| (id.clone(), v.iter().zip(q).map(|(a, b)| a * b).sum::<f32>()))
            .filter(|(_, s)| *s >= min_cos)
            .collect();
        out.sort_by(|a, b| b.1.total_cmp(&a.1));
        out.truncate(limit);
        out
    }

    pub fn has_meeting_vector(&self, meeting_id: &str) -> bool {
        self.inner.read().map(|i| i.meeting_vecs.contains_key(meeting_id)).unwrap_or(false)
    }
}

/// L2-normalise in place; a zero vector is left untouched.
pub fn normalise(v: &mut [f32]) {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(i: usize, w: f32) -> Vec<f32> {
        let mut v = vec![0.0f32; DIM];
        v[i % DIM] = 1.0;
        v[(i + 1) % DIM] = w;
        normalise(&mut v);
        v
    }

    fn loaded() -> VectorIndex {
        let idx = VectorIndex::new();
        idx.inner.write().unwrap().loaded = true;
        idx
    }

    #[test]
    fn replace_remove_and_search_with_candidates() {
        let idx = loaded();
        idx.replace_meeting("m1", &[(1, unit(0, 0.0)), (2, unit(1, 0.0))]);
        idx.replace_meeting("m2", &[(3, unit(0, 0.5))]);
        assert_eq!(idx.len(), 3);
        let hits = idx.search(&unit(0, 0.0), None, 10);
        assert_eq!(hits[0].chunk_id, 1);
        assert_eq!(hits[1].chunk_id, 3);
        assert!(hits[0].score > hits[1].score);
        let only_m2: HashSet<String> = HashSet::from(["m2".to_string()]);
        let hits = idx.search(&unit(0, 0.0), Some(&only_m2), 10);
        assert_eq!(hits.iter().map(|h| h.chunk_id).collect::<Vec<_>>(), vec![3]);
        idx.replace_meeting("m1", &[(9, unit(5, 0.0))]);
        let hits = idx.search(&unit(0, 0.0), None, 10);
        assert!(hits.iter().all(|h| h.chunk_id != 1 && h.chunk_id != 2));
        idx.remove_meeting("m2");
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.search(&unit(5, 0.0), None, 1)[0].chunk_id, 9);
        // top_k is honoured
        assert_eq!(idx.search(&unit(5, 0.0), None, 0).len(), 0);
    }

    #[test]
    fn compaction_keeps_alive_rows() {
        let idx = loaded();
        for m in 0..40 {
            let rows: Vec<(i64, Vec<f32>)> = (0..50).map(|i| ((m * 50 + i) as i64, unit(i, 0.1))).collect();
            idx.replace_meeting(&format!("m{m}"), &rows);
        }
        for m in 0..30 {
            idx.remove_meeting(&format!("m{m}"));
        }
        assert_eq!(idx.len(), 500);
        let inner = idx.inner.read().unwrap();
        assert!(inner.chunk_ids.len() < 2000, "compaction should have dropped dead rows");
        assert_eq!(inner.chunk_ids.len() - inner.dead, 500, "alive rows survive lazy compaction");
        drop(inner);
        let hits = idx.search(&unit(7, 0.1), None, 5);
        assert_eq!(hits.len(), 5);
        assert!(hits.iter().all(|h| h.meeting_id.trim_start_matches('m').parse::<usize>().unwrap() >= 30));
    }

    #[test]
    fn related_meetings_use_meeting_vectors() {
        let idx = loaded();
        idx.set_meeting_vector("a", Some(unit(0, 0.0)));
        idx.set_meeting_vector("b", Some(unit(0, 0.3)));
        idx.set_meeting_vector("c", Some(unit(9, 0.0)));
        let r = idx.related("a", 0.5, 5);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "b");
        idx.set_meeting_vector("b", None);
        assert!(idx.related("a", 0.5, 5).is_empty());
        assert!(!idx.has_meeting_vector("b"));
    }

    #[test]
    fn mutations_before_load_are_ignored() {
        let idx = VectorIndex::new();
        idx.replace_meeting("m", &[(1, unit(0, 0.0))]);
        assert_eq!(idx.len(), 0);
        assert!(idx.search(&unit(0, 0.0), None, 5).is_empty());
    }
}
