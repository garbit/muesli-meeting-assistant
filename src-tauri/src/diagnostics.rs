//! Diagnostics: persistent logging setup, per-meeting job history, and a
//! self-contained report (also printed by `muesli --doctor`) that makes a bug
//! reproducible without asking the user for anything else.

use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use serde_json::json;
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;
use tracing_subscriber::prelude::*;

use crate::models;
use crate::paths::Paths;
use crate::sidecar::{Role, Sidecar};
use crate::AppState;

const LOG_FILE_PREFIX: &str = "muesli.log";
const LOG_RETENTION_DAYS: u64 = 14;

/// Keeps the non-blocking log writer alive for the life of the process.
static LOG_GUARD: std::sync::OnceLock<tracing_appender::non_blocking::WorkerGuard> = std::sync::OnceLock::new();

/// Log to stderr and to a daily file under `logs/` (bundles launched from Finder
/// have no visible stderr, so the file is the only record of what happened).
pub fn init_logging(paths: &Paths) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let appender = tracing_appender::rolling::daily(paths.logs(), LOG_FILE_PREFIX);
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let _ = LOG_GUARD.set(guard);
    let stderr_layer = tracing_subscriber::fmt::layer().with_target(true).with_writer(std::io::stderr);
    let file_layer = tracing_subscriber::fmt::layer().with_target(true).with_ansi(false).with_writer(writer);
    let _ = tracing_subscriber::registry().with(filter).with(stderr_layer).with(file_layer).try_init();
    sweep_old_logs(paths);
}

fn sweep_old_logs(paths: &Paths) {
    let Ok(entries) = std::fs::read_dir(paths.logs()) else { return };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(LOG_RETENTION_DAYS * 86_400);
    for e in entries.flatten() {
        if let Ok(meta) = e.metadata()
            && meta.is_file()
            && meta.modified().map(|m| m < cutoff).unwrap_or(false)
        {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Most recent log file (today's, or the newest present).
pub fn current_log_file(paths: &Paths) -> Option<PathBuf> {
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(paths.logs())
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(LOG_FILE_PREFIX))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort();
    files.pop().map(|(_, p)| p)
}

pub fn tail(path: &std::path::Path, lines: usize) -> String {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let all: Vec<&str> = s.lines().collect();
            let start = all.len().saturating_sub(lines);
            all[start..].join("\n")
        }
        Err(e) => format!("(cannot read {}: {e})", path.display()),
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRow {
    pub id: String,
    pub meeting_id: Option<String>,
    pub kind: String,
    pub stage: Option<String>,
    pub state: String,
    pub error: Option<String>,
    pub created_at: String,
    pub finished_at: Option<String>,
}

pub fn jobs_for(c: &Connection, meeting_id: Option<&str>, limit: usize) -> Result<Vec<JobRow>> {
    let sql = match meeting_id {
        Some(_) => "SELECT id, meeting_id, kind, stage, state, error, created_at, finished_at FROM jobs WHERE meeting_id = ?1 ORDER BY created_at DESC LIMIT ?2",
        None => "SELECT id, meeting_id, kind, stage, state, error, created_at, finished_at FROM jobs WHERE ?1 IS NULL ORDER BY created_at DESC LIMIT ?2",
    };
    let mut stmt = c.prepare(sql)?;
    let rows = stmt.query_map(rusqlite::params![meeting_id, limit as i64], |r| {
        Ok(JobRow {
            id: r.get(0)?,
            meeting_id: r.get(1)?,
            kind: r.get(2)?,
            stage: r.get(3)?,
            state: r.get(4)?,
            error: r.get(5)?,
            created_at: r.get(6)?,
            finished_at: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn sysctl(name: &str) -> String {
    std::process::Command::new("sysctl")
        .args(["-n", name])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn sw_vers() -> String {
    std::process::Command::new("sw_vers")
        .args(["-productVersion"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// The full report. Spawns a sidecar for CoreML model status (about a second).
pub async fn report(state: &AppState) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Muesli diagnostics ({})", chrono::Local::now().to_rfc3339());
    let _ = writeln!(out, "version: {}  build: {}", env!("CARGO_PKG_VERSION"), crate::permissions::current_code_hash().unwrap_or_default());
    let _ = writeln!(out, "macOS: {}  machine: {}  memory: {} GB", sw_vers(), sysctl("hw.model"), sysctl("hw.memsize").parse::<u64>().unwrap_or(0) / 1_073_741_824);
    let _ = writeln!(out, "data: {}  models: {}", state.paths.root.display(), state.paths.models.display());
    if let Ok(exe) = std::env::current_exe() {
        let _ = writeln!(out, "executable: {}", exe.display());
    }
    match crate::settings::load(&state.db) {
        Ok(s) => { let _ = writeln!(out, "settings: {}", serde_json::to_string(&s).unwrap_or_default()); }
        Err(e) => { let _ = writeln!(out, "settings: error {e}"); }
    }

    let _ = writeln!(out, "\n## Models");
    for m in state.manifest.models.iter().filter(|m| m.runtime == "llama.cpp") {
        let st = models::gguf_status(&state.paths, m);
        let _ = writeln!(out, "- {} installed={} bytes={}", m.id, st.installed, st.bytes_on_disk);
    }
    match Sidecar::spawn(Role::Process, &state.paths.fluid_models()).await {
        Ok(sc) => {
            match sc.call_timeout("models.status", json!({}), std::time::Duration::from_secs(15)).await {
                Ok(v) => {
                    for item in v.as_array().cloned().unwrap_or_default() {
                        let _ = writeln!(out, "- {} installed={} missing={}", item["id"].as_str().unwrap_or("?"), item["installed"], item["missing"]);
                    }
                }
                Err(e) => { let _ = writeln!(out, "- sidecar models.status failed: {e}"); }
            }
            match sc.health().await {
                Ok(v) => { let _ = writeln!(out, "sidecar: {v}"); }
                Err(e) => { let _ = writeln!(out, "sidecar health failed: {e}"); }
            }
            sc.shutdown().await;
        }
        Err(e) => { let _ = writeln!(out, "- sidecar unavailable: {e}"); }
    }

    let _ = writeln!(out, "\n## Meetings");
    if let Ok(rows) = state.db.with(|c| {
        let mut stmt = c.prepare("SELECT id, title, state, source, duration_ms, created_at FROM meetings ORDER BY created_at DESC LIMIT 10")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<i64>>(4)?, r.get::<_, String>(5)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }) {
        for (id, title, st, src, dur, created) in rows {
            let dir = state.paths.meeting_dir(&id);
            let files: Vec<String> = std::fs::read_dir(&dir).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect()).unwrap_or_default();
            let _ = writeln!(out, "- {id} \"{title}\" state={st} source={src} duration_ms={dur:?} created={created} files={files:?}");
        }
    }

    let _ = writeln!(out, "\n## Recent jobs");
    if let Ok(jobs) = state.db.with(|c| jobs_for(c, None, 10)) {
        for j in jobs {
            let _ = writeln!(out, "- {} {} {} stage={} state={} error={:?}", j.created_at, &j.id[..8], j.kind, j.stage.unwrap_or_default(), j.state, j.error);
        }
    }

    let _ = writeln!(out, "\n## Recent summaries");
    if let Ok(rows) = state.db.with(|c| {
        let mut stmt = c.prepare("SELECT substr(id,1,8), meeting_id, status, model_id, prompt_tokens, output_tokens, created_at FROM summaries ORDER BY created_at DESC LIMIT 5")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<i64>>(4)?, r.get::<_, Option<i64>>(5)?, r.get::<_, String>(6)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }) {
        for (id, mid, st, model, pt, ot, created) in rows {
            let _ = writeln!(out, "- {created} {id} meeting={mid} status={st} model={model} tokens={pt:?}/{ot:?}");
        }
    }

    let _ = writeln!(out, "\n## Log tail");
    match current_log_file(&state.paths) {
        Some(p) => { let _ = writeln!(out, "{}\n{}", p.display(), tail(&p, 150)); }
        None => { let _ = writeln!(out, "(no log file yet)"); }
    }
    out
}

/// `muesli --doctor`: print the report for the real library.
pub fn doctor() -> i32 {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result: Result<String> = rt.block_on(async {
        let paths = Paths::resolve()?;
        let db = std::sync::Arc::new(crate::db::Db::open(&paths.db())?);
        let manifest = models::Manifest::embedded()?;
        let state = AppState { paths, db, manifest };
        Ok(report(&state).await)
    });
    match result {
        Ok(r) => {
            println!("{r}");
            0
        }
        Err(e) => {
            eprintln!("doctor failed: {e:#}");
            1
        }
    }
}

// ---- commands -------------------------------------------------------------

type CmdResult<T> = Result<T, String>;

#[tauri::command]
pub async fn diagnostics_report(state: State<'_, AppState>) -> CmdResult<String> {
    Ok(report(&state).await)
}

#[tauri::command]
pub fn meeting_jobs(state: State<'_, AppState>, meeting_id: String) -> CmdResult<Vec<JobRow>> {
    state.db.with(|c| jobs_for(c, Some(&meeting_id), 20)).map_err(|e| e.to_string())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingLogs {
    pub process_log: Option<String>,
    pub capture_log: Option<String>,
    pub files: Vec<String>,
}

/// Tails of the per-meeting sidecar logs plus the files in the meeting folder.
#[tauri::command]
pub fn meeting_logs(state: State<'_, AppState>, meeting_id: String, lines: Option<usize>) -> CmdResult<MeetingLogs> {
    let dir = state.paths.meeting_dir(&meeting_id);
    let n = lines.unwrap_or(120);
    let read = |name: &str| {
        let p = dir.join(name);
        p.exists().then(|| tail(&p, n))
    };
    let files = std::fs::read_dir(&dir)
        .map(|d| {
            d.flatten()
                .map(|e| {
                    let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                    format!("{} ({} bytes)", e.file_name().to_string_lossy(), size)
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(MeetingLogs { process_log: read("process.log"), capture_log: read("capture.log"), files })
}

#[tauri::command]
pub fn open_logs_folder(app: AppHandle, state: State<'_, AppState>) -> CmdResult<()> {
    let target = current_log_file(&state.paths).unwrap_or_else(|| state.paths.logs());
    app.opener().reveal_item_in_dir(target.to_string_lossy().to_string()).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn open_data_folder(app: AppHandle, state: State<'_, AppState>, meeting_id: Option<String>) -> CmdResult<()> {
    let target = match meeting_id {
        Some(id) => state.paths.meeting_dir(&id),
        None => state.paths.root.clone(),
    };
    app.opener().open_path(target.to_string_lossy().to_string(), None::<&str>).map_err(|e| e.to_string())
}
