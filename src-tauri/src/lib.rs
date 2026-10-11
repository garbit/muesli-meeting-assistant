//! Muesli — local, private meeting recorder and summariser for macOS.
//!
//! Rust core: SQLite storage, model manifest + downloads, job orchestration,
//! llama.cpp summarisation, and a client for the Swift sidecar that does audio
//! capture and CoreML inference.

pub mod align;
pub mod ask;
pub mod commands;
pub mod db;
pub mod diagnostics;
pub mod facets;
pub mod headless;
pub mod jobs;
pub mod meetings;
pub mod models;
pub mod paths;
pub mod permissions;
pub mod recording;
pub mod search;
pub mod settings;
pub mod sidecar;
pub mod speakers;
pub mod summarise;
pub mod templates;
pub mod tray;

use std::sync::Arc;

use tauri::Manager;

use crate::db::Db;
use crate::jobs::JobQueue;
use crate::models::Manifest;
use crate::paths::Paths;
use crate::permissions::PermissionsCache;
use crate::recording::Recorder;
use crate::summarise::SummaryJobs;

/// Shared application state (registered with Tauri's `manage`).
pub struct AppState {
    pub paths: Paths,
    pub db: Arc<Db>,
    pub manifest: Manifest,
    /// Search index and its queue; inert until `search::start` runs.
    pub search: Arc<search::SearchState>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let paths = Paths::resolve().expect("resolve app directories");
    diagnostics::init_logging(&paths);
    let db = Arc::new(Db::open(&paths.db()).expect("open database"));
    let manifest = Manifest::embedded().expect("embedded manifest");
    db.with(templates::seed_builtins).expect("seed templates");
    if let Err(e) = jobs::mark_interrupted(&db) {
        tracing::warn!("marking interrupted jobs failed: {e:#}");
    }
    if let Err(e) = settings::retention_sweep(&db) {
        tracing::warn!("retention sweep failed: {e:#}");
    }
    tracing::info!("muesli starting; data at {}", paths.root.display());

    tauri::Builder::default()
        // A second launch (e.g. opening the app again) must never run alongside the
        // first: it would share the database and could treat a live recording as
        // abandoned. Focus the existing window instead.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(AppState { paths, db, manifest, search: Arc::new(search::SearchState::new()) })
        .manage(Arc::new(Recorder::default()))
        .manage(Arc::new(JobQueue::default()))
        .manage(Arc::new(SummaryJobs::default()))
        .manage(PermissionsCache::default())
        .invoke_handler(tauri::generate_handler![
            commands::app_info,
            commands::models_status,
            commands::models_download,
            commands::sidecar_health,
            meetings::meetings_list,
            meetings::meeting_get,
            meetings::meeting_update,
            meetings::meeting_delete,
            meetings::meeting_rename_speaker,
            recording::recording_start,
            recording::recording_stop,
            recording::recording_status,
            recording::recording_live_transcript,
            recording::meeting_live_transcript,
            recording::import_file,
            recording::meeting_process,
            jobs::meeting_cancel_processing,
            templates::templates_list,
            templates::template_upsert,
            templates::template_delete,
            templates::template_reset,
            summarise::summary_generate,
            summarise::summary_cancel,
            summarise::summary_status,
            summarise::meeting_export,
            summarise::meeting_export_to,
            speakers::speakers_list,
            speakers::speaker_assign,
            speakers::speaker_update,
            speakers::speaker_merge,
            speakers::speaker_delete,
            settings::settings_get,
            settings::settings_set,
            settings::recording_delete,
            permissions::permissions_status,
            permissions::permissions_request,
            permissions::permissions_recheck,
            permissions::permissions_open_settings,
            diagnostics::diagnostics_report,
            diagnostics::meeting_jobs,
            diagnostics::meeting_logs,
            diagnostics::open_logs_folder,
            diagnostics::open_data_folder,
            search::search_meetings,
            search::search_facets,
            search::search_status,
            search::search_reindex,
            search::search_warm,
            search::meeting_related,
            facets::meeting_facets,
            facets::facets_backfill,
            ask::ask_meetings,
            ask::ask_cancel,
        ])
        .setup(|app| {
            tray::setup(app.handle())?;
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.set_title("Muesli");
            }
            // Finish recordings a crash or quit left behind, then queue them, and
            // resume meetings that were recorded but never transcribed.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                recording::recover_interrupted(&handle).await;
                recording::resume_unprocessed(&handle);
            });
            // Establish the permission state once and push it to the UI.
            tauri::async_runtime::spawn(permissions::probe_at_startup(app.handle().clone()));
            // Background search indexer; picks up anything stale from the last run.
            search::start(app.handle());
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() != "main" {
                return;
            }
            match event {
                // Closing the main window keeps the app (and any recording) alive in the menu bar.
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    let _ = window.hide();
                }
                // Back from System Settings, most likely: re-check what is still missing.
                tauri::WindowEvent::Focused(true) => {
                    tauri::async_runtime::spawn(permissions::refresh_on_focus(window.app_handle().clone()));
                }
                _ => {}
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| match event {
            tauri::RunEvent::ExitRequested { api, .. } => {
                // Quitting mid-recording: finalise the recording first, then exit.
                // A second Cmd+Q while that runs must not cut it short, and the
                // exit we request ourselves afterwards must go through.
                use std::sync::atomic::{AtomicU8, Ordering};
                const IDLE: u8 = 0;
                const STOPPING: u8 = 1;
                const DONE: u8 = 2;
                static EXIT_STATE: AtomicU8 = AtomicU8::new(IDLE);
                match EXIT_STATE.load(Ordering::SeqCst) {
                    STOPPING => api.prevent_exit(),
                    DONE => {}
                    _ => {
                        let recorder = app.state::<Arc<Recorder>>().inner().clone();
                        if tauri::async_runtime::block_on(recorder.is_recording()) {
                            EXIT_STATE.store(STOPPING, Ordering::SeqCst);
                            api.prevent_exit();
                            let app2 = app.clone();
                            tauri::async_runtime::spawn(async move {
                                // Covers a start still in flight plus encoding a long meeting.
                                // If it does run out, the sidecar still finalises on its own
                                // and the next launch recovers the file.
                                match tokio::time::timeout(
                                    std::time::Duration::from_secs(720),
                                    recorder.stop_for_exit(&app2),
                                )
                                .await
                                {
                                    Ok(Ok(m)) => tracing::info!("recording {} finalised before exit", m.id),
                                    Ok(Err(e)) => tracing::warn!("stopping recording before exit failed: {e:#}"),
                                    Err(_) => tracing::warn!("stopping recording before exit timed out"),
                                }
                                EXIT_STATE.store(DONE, Ordering::SeqCst);
                                app2.exit(0);
                            });
                        }
                    }
                }
            }
            tauri::RunEvent::Exit => {
                // Release Metal buffers before exit; ggml asserts if a model is still resident.
                let _ = muesli_llm::Engine::global().unload();
                let _ = muesli_llm::Embedder::global().unload();
            }
            _ => {}
        });
}
