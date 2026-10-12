//! Live recording session: owns the `capture` sidecar for the duration of a
//! meeting, forwards its status and live transcript to the UI and the tray,
//! and hands the finished recording to the processing pipeline.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Mutex;

use crate::AppState;
use crate::db::now_iso;
use crate::jobs;
use crate::meetings::{self, Meeting};
use crate::sidecar::{Role, RpcError, Sidecar};
use crate::tray;

/// `capture.start` can block while macOS shows a permission prompt.
const START_TIMEOUT: Duration = Duration::from_secs(90);
/// `capture.stop` and `capture.recover` encode the whole meeting to AAC.
const STOP_TIMEOUT: Duration = Duration::from_secs(600);
/// How long recovery waits for an orphaned sidecar that is still finalising.
const RECOVER_BUSY_WAIT: Duration = Duration::from_secs(600);
/// Sidecar error code: a live recorder still holds the directory.
const RPC_BUSY: i64 = -32002;

/// A near-real-time transcript segment from the capture sidecar.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSegment {
    pub text: String,
    pub confirmed: bool,
    pub start_ms: i64,
    pub end_ms: i64,
    #[serde(default)]
    pub who: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveTranscript {
    pub confirmed: Vec<LiveSegment>,
    pub volatile: Option<LiveSegment>,
}

pub struct Session {
    pub meeting_id: String,
    sidecar: Arc<Sidecar>,
    /// When `capture.start` returned, i.e. when audio actually began.
    started: Instant,
    forwarder: tokio::task::JoinHandle<()>,
    last_status: Arc<std::sync::Mutex<Value>>,
    live: Arc<std::sync::Mutex<LiveTranscript>>,
    /// Set once Stop was pressed: status notifications stop being forwarded so
    /// the UI's "Finalising…" is not overwritten by a late one.
    finalising: Arc<AtomicBool>,
}

enum Phase {
    Idle,
    /// `capture.start` in flight; it can block for seconds while macOS shows a
    /// permission prompt, so the mutex is not held during that time.
    Starting {
        meeting_id: String,
    },
    Recording(Session),
}

pub struct Recorder {
    phase: Mutex<Phase>,
}

impl Default for Recorder {
    fn default() -> Self {
        Self { phase: Mutex::new(Phase::Idle) }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingStatus {
    pub meeting_id: String,
    pub elapsed_ms: u64,
    pub starting: bool,
    pub status: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveTranscriptEvent {
    pub meeting_id: String,
    pub segment: LiveSegment,
}

impl Recorder {
    pub async fn is_recording(&self) -> bool {
        !matches!(*self.phase.lock().await, Phase::Idle)
    }

    pub async fn status(&self) -> Option<RecordingStatus> {
        match &*self.phase.lock().await {
            Phase::Idle => None,
            Phase::Starting { meeting_id } => Some(RecordingStatus {
                meeting_id: meeting_id.clone(),
                elapsed_ms: 0,
                starting: true,
                status: json!({}),
            }),
            Phase::Recording(s) => {
                let status = s.last_status.lock().map(|v| v.clone()).unwrap_or_else(|_| json!({}));
                // The sidecar's clock is authoritative; ours only covers the gap before its first status.
                let elapsed_ms = status["elapsedMs"].as_u64().unwrap_or_else(|| s.started.elapsed().as_millis() as u64);
                Some(RecordingStatus { meeting_id: s.meeting_id.clone(), elapsed_ms, starting: false, status })
            }
        }
    }

    pub async fn live_transcript(&self) -> Option<LiveTranscript> {
        match &*self.phase.lock().await {
            Phase::Recording(s) => s.live.lock().ok().map(|l| l.clone()),
            _ => None,
        }
    }

    pub async fn start(&self, app: &AppHandle, title: Option<String>) -> Result<Meeting> {
        let state = app.state::<AppState>();
        // A denied microphone records silence; refuse with an actionable message instead.
        crate::permissions::ensure_microphone(app).await?;
        let title = title.unwrap_or_else(|| format!("Meeting {}", chrono::Local::now().format("%a %d %b %H:%M")));
        let meeting = {
            let mut guard = self.phase.lock().await;
            if !matches!(*guard, Phase::Idle) {
                return Err(anyhow!("already recording"));
            }
            let meeting = state.db.with(|c| {
                let m = meetings::insert_meeting(c, &title, "recorded")?;
                meetings::set_times(c, &m.id, Some(&now_iso()), None, None)?;
                meetings::set_state(c, &m.id, "recording")?;
                Ok(m)
            })?;
            *guard = Phase::Starting { meeting_id: meeting.id.clone() };
            meeting
        };

        match self.start_session(app, &meeting.id).await {
            Ok(session) => {
                *self.phase.lock().await = Phase::Recording(session);
                tray::set_recording(app, Some(0));
                let _ = app.emit("recording:changed", json!({"recording": true, "meetingId": meeting.id}));
                Ok(meeting)
            }
            Err(e) => {
                *self.phase.lock().await = Phase::Idle;
                discard_failed_start(&state, &meeting.id);
                Err(e)
            }
        }
    }

    async fn start_session(&self, app: &AppHandle, meeting_id: &str) -> Result<Session> {
        let state = app.state::<AppState>();
        let settings = crate::settings::load(&state.db).unwrap_or_default();
        let dir = state.paths.meeting_dir(meeting_id);
        std::fs::create_dir_all(&dir)?;
        let sidecar = Sidecar::spawn_logged(Role::Capture, &state.paths.fluid_models(), Some(dir.join("capture.log")))
            .await
            .context("spawning capture sidecar")?;
        let last_status = Arc::new(std::sync::Mutex::new(json!({})));
        let live = Arc::new(std::sync::Mutex::new(LiveTranscript::default()));
        let mut rx = sidecar.subscribe();
        let app2 = app.clone();
        let mid = meeting_id.to_string();
        let ls = last_status.clone();
        let live2 = live.clone();
        let finalising = Arc::new(AtomicBool::new(false));
        let finalising2 = finalising.clone();
        let spawned = Instant::now();
        let forwarder = tokio::spawn(async move {
            let mut last_tray = 0u64;
            while let Ok(n) = rx.recv().await {
                match n.method.as_str() {
                    "capture.status" => {
                        if finalising2.load(Ordering::Relaxed) {
                            continue;
                        }
                        if let Ok(mut v) = ls.lock() {
                            *v = n.params.clone();
                        }
                        // The tap's behaviour during a real recording is the truth about
                        // the System Audio Recording grant; keep the permission state honest.
                        crate::permissions::note_recording_system_audio(
                            &app2,
                            n.params["systemAudio"].as_str().unwrap_or(""),
                            n.params["sysRms"].as_f64().unwrap_or(0.0),
                        );
                        let elapsed =
                            n.params["elapsedMs"].as_u64().unwrap_or_else(|| spawned.elapsed().as_millis() as u64);
                        let _ = app2.emit(
                            "recording:status",
                            RecordingStatus {
                                meeting_id: mid.clone(),
                                elapsed_ms: elapsed,
                                starting: false,
                                status: n.params,
                            },
                        );
                        if elapsed / 1000 != last_tray {
                            last_tray = elapsed / 1000;
                            tray::set_recording(&app2, Some(elapsed));
                        }
                    }
                    "capture.transcript" => {
                        if let Ok(seg) = serde_json::from_value::<LiveSegment>(n.params) {
                            if let Ok(mut l) = live2.lock() {
                                if seg.confirmed {
                                    l.confirmed.push(seg.clone());
                                    l.volatile = None;
                                } else {
                                    l.volatile = Some(seg.clone());
                                }
                            }
                            let _ = app2.emit(
                                "recording:transcript",
                                LiveTranscriptEvent { meeting_id: mid.clone(), segment: seg },
                            );
                        }
                    }
                    _ => {}
                }
            }
        });
        let params = json!({"outputDir": dir.to_string_lossy(), "liveTranscript": settings.live_transcript});
        if let Err(e) = sidecar.call_timeout("capture.start", params, START_TIMEOUT).await {
            forwarder.abort();
            sidecar.shutdown().await;
            return Err(anyhow!("capture.start failed: {e}"));
        }
        Ok(Session {
            meeting_id: meeting_id.to_string(),
            sidecar,
            started: Instant::now(),
            forwarder,
            last_status,
            live,
            finalising,
        })
    }

    pub async fn stop(&self, app: &AppHandle) -> Result<Meeting> {
        self.stop_inner(app, true, Duration::from_secs(10)).await
    }

    /// Finalise the recording without starting processing (used while quitting;
    /// the next launch resumes unprocessed meetings). Waits out a start that is
    /// still in flight so a quit right after "Record" loses nothing.
    pub async fn stop_for_exit(&self, app: &AppHandle) -> Result<Meeting> {
        self.stop_inner(app, false, START_TIMEOUT + Duration::from_secs(5)).await
    }

    /// Take the live session out of the recorder, waiting up to `wait_for_start`
    /// for a `capture.start` still in flight.
    async fn take_session(&self, wait_for_start: Duration) -> Result<Session> {
        let deadline = Instant::now() + wait_for_start;
        loop {
            {
                let mut guard = self.phase.lock().await;
                match std::mem::replace(&mut *guard, Phase::Idle) {
                    Phase::Recording(s) => return Ok(s),
                    Phase::Idle => return Err(anyhow!("not recording")),
                    Phase::Starting { meeting_id } => {
                        *guard = Phase::Starting { meeting_id };
                        if Instant::now() >= deadline {
                            return Err(anyhow!("recording is still starting; try again in a moment"));
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn stop_inner(&self, app: &AppHandle, process: bool, wait_for_start: Duration) -> Result<Meeting> {
        let session = self.take_session(wait_for_start).await?;
        let state = app.state::<AppState>();
        // Encoding a long meeting takes a while; tell the UI the recording is over
        // but the file is not ready yet. Transcript notifications keep flowing.
        session.finalising.store(true, Ordering::Relaxed);
        let _ = app.emit(
            "recording:status",
            RecordingStatus {
                meeting_id: session.meeting_id.clone(),
                elapsed_ms: session.last_status.lock().ok().and_then(|v| v["elapsedMs"].as_u64()).unwrap_or(0),
                starting: false,
                status: json!({"state": "finalising"}),
            },
        );
        let result = session.sidecar.call_timeout("capture.stop", json!({}), STOP_TIMEOUT).await;
        session.forwarder.abort();
        session.sidecar.shutdown().await;
        tray::set_recording(app, None);
        let _ = app.emit("recording:changed", json!({"recording": false, "meetingId": session.meeting_id}));

        let v = match result {
            Ok(v) => v,
            Err(e) => {
                // The sidecar finalises on its own once its stdin closes. Pick up
                // whatever it leaves on disk (chunks or recording.m4a) as soon as it
                // lets go of the directory, instead of waiting for the next launch.
                state.db.with(|c| meetings::set_state(c, &session.meeting_id, "failed"))?;
                let app2 = app.clone();
                let id = session.meeting_id.clone();
                tauri::async_runtime::spawn(async move {
                    recover_one(&app2, &id, true).await;
                });
                return Err(anyhow!("capture.stop failed: {e}"));
            }
        };
        save_live_transcript(&state, &session, &v);
        let meeting = match finish_recording(&state, &session.meeting_id, &v) {
            Ok(m) => m,
            Err(e) => {
                let _ = state.db.with(|c| meetings::set_state(c, &session.meeting_id, "failed"));
                return Err(e);
            }
        };
        let settings = crate::settings::load(&state.db).unwrap_or_default();
        if process && settings.auto_transcribe {
            jobs::enqueue_processing(app.clone(), meeting.id.clone());
        }
        Ok(meeting)
    }
}

/// Keep the live transcript so the meeting is readable before (or without) the
/// full transcription pass. The sidecar's list in the stop result is
/// authoritative (its last notifications can arrive after the response); the
/// segments forwarded during the meeting are the fallback.
fn save_live_transcript(state: &AppState, session: &Session, stop_result: &Value) {
    let mut segments: Vec<LiveSegment> =
        serde_json::from_value(stop_result["liveSegments"].clone()).unwrap_or_default();
    if segments.is_empty() {
        let Ok(live) = session.live.lock() else { return };
        segments = live.confirmed.clone();
        if let Some(v) = &live.volatile {
            segments.push(LiveSegment { confirmed: true, ..v.clone() });
        }
    }
    if segments.is_empty() {
        return;
    }
    let path = state.paths.meeting_dir(&session.meeting_id).join("live.json");
    let written = serde_json::to_vec_pretty(&segments)
        .map_err(anyhow::Error::from)
        .and_then(|json| std::fs::write(&path, json).map_err(Into::into));
    if let Err(e) = written {
        tracing::warn!("saving live transcript for {}: {e:#}", session.meeting_id);
    }
}

/// A recording that never started leaves nothing worth keeping, so drop the
/// meeting rather than fill the library with empty "Failed" rows. If the sidecar
/// did get as far as writing audio (a permission prompt answered after our
/// timeout), keep the meeting as `failed`; recovery finishes it from disk.
fn discard_failed_start(state: &AppState, meeting_id: &str) {
    let dir = state.paths.meeting_dir(meeting_id);
    if has_audio_on_disk(&dir) {
        let _ = state.db.with(|c| meetings::set_state(c, meeting_id, "failed"));
        return;
    }
    let deleted = state.db.with(|c| {
        c.execute("DELETE FROM meetings WHERE id = ?1", [meeting_id])?;
        Ok(())
    });
    match deleted {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&dir);
        }
        Err(e) => tracing::warn!("removing meeting {meeting_id} after a failed start: {e:#}"),
    }
}

/// Chunk files, a finished recording, or a live recorder's lock file.
fn has_audio_on_disk(dir: &Path) -> bool {
    if dir.join("recording.m4a").exists() || dir.join("capture.lock").exists() {
        return true;
    }
    std::fs::read_dir(dir)
        .map(|entries| {
            entries.flatten().any(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with("chunk-") && name.ends_with(".pcm")
            })
        })
        .unwrap_or(false)
}

/// Startup pass: meetings that have a recording but were never transcribed
/// (e.g. the app quit during processing) are queued again when auto-transcribe is on.
pub fn resume_unprocessed(app: &AppHandle) {
    let state = app.state::<AppState>();
    let settings = crate::settings::load(&state.db).unwrap_or_default();
    if !settings.auto_transcribe {
        return;
    }
    let ids: Vec<String> = state
        .db
        .with(|c| {
            let mut stmt = c.prepare(
                "SELECT m.id FROM meetings m WHERE m.state = 'recorded'
                 AND EXISTS (SELECT 1 FROM recordings r WHERE r.meeting_id = m.id AND r.deleted_at IS NULL)
                 AND NOT EXISTS (SELECT 1 FROM transcripts t WHERE t.meeting_id = m.id)
                 ORDER BY m.created_at",
            )?;
            Ok(stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .unwrap_or_default();
    for id in ids {
        tracing::info!("resuming processing of meeting {id}");
        jobs::enqueue_processing(app.clone(), id);
    }
}

/// Store a finished recording (from `capture.stop` or `capture.recover`).
pub fn finish_recording(state: &AppState, meeting_id: &str, v: &Value) -> Result<Meeting> {
    let path = v["recordingPath"].as_str().ok_or_else(|| anyhow!("no recordingPath in capture result"))?.to_string();
    let duration = v["durationMs"].as_i64();
    let rate = v["sampleRate"].as_f64().map(|r| r as i64);
    state.db.with(|c| {
        meetings::insert_recording(c, meeting_id, "retained", &path, rate, Some(2), duration)?;
        meetings::set_times(c, meeting_id, None, Some(&now_iso()), duration)?;
        meetings::set_state(c, meeting_id, "recorded")?;
        meetings::get_meeting(c, meeting_id)?.ok_or_else(|| anyhow!("meeting vanished"))
    })
}

/// Live transcript saved at stop time, if any.
pub fn read_live_transcript(state: &AppState, meeting_id: &str) -> Vec<LiveSegment> {
    let path = state.paths.meeting_dir(meeting_id).join("live.json");
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Startup pass: meetings whose recording never reached the database — left in
/// `recording` by a crash or forced quit, or `failed` because `capture.stop` did
/// not answer — still have raw chunks or a finished `recording.m4a` on disk.
/// Rebuild their recording row; `resume_unprocessed` then queues them.
pub async fn recover_interrupted(app: &AppHandle) {
    let state = app.state::<AppState>();
    let candidates: Vec<(String, String)> = match state.db.with(|c| {
        let mut stmt = c.prepare(
            "SELECT m.id, m.state FROM meetings m WHERE m.state = 'recording'
             OR (m.state = 'failed' AND NOT EXISTS (SELECT 1 FROM recordings r WHERE r.meeting_id = m.id AND r.deleted_at IS NULL))
             ORDER BY m.created_at",
        )?;
        Ok(stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)
    }) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("recovery query failed: {e}");
            return;
        }
    };
    let mut stuck = Vec::new();
    for (id, meeting_state) in candidates {
        if has_audio_on_disk(&state.paths.meeting_dir(&id)) {
            stuck.push(id);
        } else if meeting_state == "recording" {
            // Nothing was ever written: a crash before the first chunk.
            tracing::warn!("meeting {id} was recording but left no audio; marking failed");
            let _ = state.db.with(|c| meetings::set_state(c, &id, "failed"));
        }
    }
    if stuck.is_empty() {
        return;
    }
    let sc = match Sidecar::spawn(Role::Capture, &state.paths.fluid_models()).await {
        Ok(sc) => sc,
        Err(e) => {
            tracing::warn!("recovery: cannot spawn sidecar: {e}");
            return;
        }
    };
    for id in stuck {
        recover_meeting(app, &sc, &id, false).await;
    }
    sc.shutdown().await;
}

/// Recover a single meeting with its own sidecar (after a failed stop).
async fn recover_one(app: &AppHandle, meeting_id: &str, enqueue: bool) {
    let state = app.state::<AppState>();
    match Sidecar::spawn(Role::Capture, &state.paths.fluid_models()).await {
        Ok(sc) => {
            recover_meeting(app, &sc, meeting_id, enqueue).await;
            sc.shutdown().await;
        }
        Err(e) => tracing::warn!("recovery of {meeting_id}: cannot spawn sidecar: {e}"),
    }
}

/// Rebuild one meeting's recording from its directory. An orphaned sidecar that
/// is still finalising holds `capture.lock`; wait for it rather than fight it.
async fn recover_meeting(app: &AppHandle, sc: &Sidecar, meeting_id: &str, enqueue: bool) {
    let state = app.state::<AppState>();
    let dir = state.paths.meeting_dir(meeting_id);
    let deadline = Instant::now() + RECOVER_BUSY_WAIT;
    let result = loop {
        match sc.call_timeout("capture.recover", json!({"outputDir": dir.to_string_lossy()}), STOP_TIMEOUT).await {
            Err(RpcError::Remote { code: RPC_BUSY, .. }) if Instant::now() < deadline => {
                tracing::info!("recovery of {meeting_id}: a recorder is still finalising; waiting");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            other => break other,
        }
    };
    match result {
        Ok(v) => match finish_recording(&state, meeting_id, &v) {
            Ok(_) => {
                tracing::info!("recovered interrupted recording for meeting {meeting_id}");
                if enqueue && crate::settings::load(&state.db).map(|s| s.auto_transcribe).unwrap_or(true) {
                    jobs::enqueue_processing(app.clone(), meeting_id.to_string());
                }
            }
            Err(e) => tracing::warn!("recovery of {meeting_id} failed to store: {e:#}"),
        },
        Err(e) => {
            tracing::warn!("recovery of {meeting_id} failed: {e}");
            let _ = state.db.with(|c| meetings::set_state(c, meeting_id, "failed"));
        }
    }
}

// ---- commands -------------------------------------------------------------

type CmdResult<T> = Result<T, String>;

#[tauri::command]
pub async fn recording_start(
    app: AppHandle,
    recorder: State<'_, Arc<Recorder>>,
    title: Option<String>,
) -> CmdResult<Meeting> {
    recorder.start(&app, title).await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn recording_stop(app: AppHandle, recorder: State<'_, Arc<Recorder>>) -> CmdResult<Meeting> {
    recorder.stop(&app).await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn recording_status(recorder: State<'_, Arc<Recorder>>) -> CmdResult<Option<RecordingStatus>> {
    Ok(recorder.status().await)
}

/// The live transcript of the recording in progress (for pages mounted late).
#[tauri::command]
pub async fn recording_live_transcript(recorder: State<'_, Arc<Recorder>>) -> CmdResult<Option<LiveTranscript>> {
    Ok(recorder.live_transcript().await)
}

/// The live transcript saved when a meeting stopped, used until the full
/// transcription replaces it.
#[tauri::command]
pub fn meeting_live_transcript(state: State<'_, AppState>, meeting_id: String) -> CmdResult<Vec<LiveSegment>> {
    Ok(read_live_transcript(&state, &meeting_id))
}

/// Import an existing audio/video file: copy it into the meeting folder and
/// run the processing pipeline.
#[tauri::command]
pub async fn import_file(
    app: AppHandle,
    state: State<'_, AppState>,
    path: String,
    title: Option<String>,
) -> CmdResult<Meeting> {
    let src = std::path::PathBuf::from(&path);
    if !src.is_file() {
        return Err(format!("not a file: {path}"));
    }
    let name = src.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "import".into());
    let title = title.unwrap_or_else(|| name.rsplit_once('.').map(|(a, _)| a.to_string()).unwrap_or(name.clone()));
    let meeting = state
        .db
        .with(|c| {
            let m = meetings::insert_meeting(c, &title, "imported")?;
            meetings::set_state(c, &m.id, "recorded")?;
            Ok(m)
        })
        .map_err(|e| e.to_string())?;
    let dir = state.paths.meeting_dir(&meeting.id);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let ext = src.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_else(|| "bin".into());
    let dest = dir.join(format!("import.{ext}"));
    tokio::fs::copy(&src, &dest).await.map_err(|e| format!("copying import: {e}"))?;
    state
        .db
        .with(|c| {
            meetings::insert_recording(c, &meeting.id, "import", &dest.to_string_lossy(), None, None, None)?;
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    jobs::enqueue_processing(app.clone(), meeting.id.clone());
    Ok(meeting)
}

#[tauri::command]
pub fn meeting_process(app: AppHandle, id: String) -> CmdResult<()> {
    jobs::enqueue_processing(app, id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_on_disk_detects_chunks_recordings_and_locks() {
        let dir = std::env::temp_dir().join(format!("muesli-rec-test-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!has_audio_on_disk(&dir));
        std::fs::write(dir.join("capture.log"), b"x").unwrap();
        assert!(!has_audio_on_disk(&dir));
        std::fs::write(dir.join("chunk-00000-48000.pcm"), b"").unwrap();
        assert!(has_audio_on_disk(&dir));
        std::fs::remove_file(dir.join("chunk-00000-48000.pcm")).unwrap();
        std::fs::write(dir.join("recording.m4a"), b"").unwrap();
        assert!(has_audio_on_disk(&dir));
        std::fs::remove_file(dir.join("recording.m4a")).unwrap();
        std::fs::write(dir.join("capture.lock"), b"1").unwrap();
        assert!(has_audio_on_disk(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_directory_has_no_audio() {
        assert!(!has_audio_on_disk(Path::new("/nonexistent/muesli-recording-dir")));
    }
}
