//! `muesli --import <file>`: run the processing pipeline without the UI.

use std::sync::Arc;

use crate::AppState;
use crate::db::Db;
use crate::jobs;
use crate::meetings;
use crate::models::Manifest;
use crate::paths::Paths;

/// Headless runs never touch the real library unless `MUESLI_HOME` is set explicitly.
fn use_scratch_home() {
    if std::env::var_os("MUESLI_HOME").is_none() {
        let dir = std::env::temp_dir().join("muesli-headless");
        // Keep using the real (large, already downloaded) models.
        if std::env::var_os("MUESLI_MODELS_DIR").is_none()
            && let Ok(home) = std::env::var("HOME")
        {
            let models = std::path::PathBuf::from(home).join("Library/Application Support/Muesli/models");
            // SAFETY: called at startup before any thread reads the environment.
            unsafe { std::env::set_var("MUESLI_MODELS_DIR", &models) };
        }
        // SAFETY: as above.
        unsafe { std::env::set_var("MUESLI_HOME", &dir) };
        eprintln!("MUESLI_HOME not set; using scratch library at {} (models from the real library)", dir.display());
    }
}

pub fn import_and_process(path: &str) -> i32 {
    use_scratch_home();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let code = rt.block_on(async {
        match run(path).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e:#}");
                1
            }
        }
    });
    // Release Metal buffers before exit; ggml asserts if a model is still resident.
    let _ = muesli_llm::Engine::global().unload();
    code
}

async fn run(path: &str) -> anyhow::Result<()> {
    let paths = Paths::resolve()?;
    let db = Arc::new(Db::open(&paths.db())?);
    let manifest = Manifest::embedded()?;
    db.with(crate::templates::seed_builtins)?;
    let state = AppState { paths, db, manifest };

    let src = std::path::PathBuf::from(path);
    anyhow::ensure!(src.is_file(), "not a file: {path}");
    let title = src.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "import".into());
    let meeting = state.db.with(|c| {
        let m = meetings::insert_meeting(c, &title, "imported")?;
        meetings::set_state(c, &m.id, "recorded")?;
        Ok(m)
    })?;
    let dir = state.paths.meeting_dir(&meeting.id);
    std::fs::create_dir_all(&dir)?;
    let ext = src.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_else(|| "bin".into());
    let dest = dir.join(format!("import.{ext}"));
    std::fs::copy(&src, &dest)?;
    // MUESLI_IMPORT_RETAINED=1 treats the file as a Muesli recording (L = mic, R = system)
    // so the energy timeline and "Me" detection paths are exercised.
    let kind = if std::env::var_os("MUESLI_IMPORT_RETAINED").is_some() { "retained" } else { "import" };
    state.db.with(|c| {
        meetings::insert_recording(c, &meeting.id, kind, &dest.to_string_lossy(), None, None, None)?;
        Ok(())
    })?;

    let t0 = std::time::Instant::now();
    jobs::run_processing(&jobs::stdout_emitter(), &state, &meeting.id, None).await?;
    let detail = state.db.with(|c| meetings::detail(c, &meeting.id))?.expect("meeting");
    let names: std::collections::HashMap<_, _> =
        state.db.with(|c| meetings::speaker_names(c, &meeting.id))?.into_iter().collect();
    println!(
        "meeting {} ({}): {} segments, {} speakers, {:.1}s",
        meeting.id,
        meeting.title,
        detail.segments.len(),
        detail.speakers.len(),
        t0.elapsed().as_secs_f32()
    );
    for sp in &detail.speakers {
        println!(
            "  {} → {} (speech {} ms, mic ratio {:?})",
            sp.diar_label,
            sp.display_name.clone().unwrap_or_default(),
            sp.speech_ms,
            sp.mic_ratio
        );
    }
    for s in &detail.segments {
        let name = names.get(&s.diar_label).cloned().unwrap_or(s.diar_label.clone());
        println!("[{:02}:{:02}] {}: {}", s.start_ms / 60000, (s.start_ms / 1000) % 60, name, s.text);
    }
    // MUESLI_NAME_SPEAKERS="S1=Alice,S2=Bob" names clusters so later imports can auto-link them.
    if let Ok(spec) = std::env::var("MUESLI_NAME_SPEAKERS") {
        for pair in spec.split(',') {
            if let Some((label, name)) = pair.split_once('=') {
                let id = state
                    .db
                    .with(|c| crate::speakers::link_cluster(c, &meeting.id, label.trim(), None, name.trim()))?;
                println!("named {label} → {name} (speaker {id})");
            }
        }
    }

    if std::env::var("MUESLI_NO_SUMMARY").is_err() {
        let t1 = std::time::Instant::now();
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        println!("--- summary ---");
        match crate::summarise::summarise(crate::summarise::stdout_emitter(), &state, &meeting.id, None, cancel).await {
            Ok(id) => println!("\n--- summary {id} done in {:.1}s ---", t1.elapsed().as_secs_f32()),
            Err(e) => println!("\n--- summary skipped: {e:#} ---"),
        }
    }
    Ok(())
}

/// Record for `seconds` into `dir` using the capture sidecar; writes
/// `capture-test.json` with the final status and result so a launcher that
/// cannot see stdout (LaunchServices) can still read the outcome.
pub fn capture_test(seconds: &str, dir: &str) -> i32 {
    use_scratch_home();
    let dir = std::path::PathBuf::from(dir);
    let _ = std::fs::create_dir_all(&dir);
    let log = std::fs::File::create(dir.join("capture-test.log")).ok();
    if let Some(log) = log {
        let _ = tracing_subscriber::fmt().with_writer(std::sync::Mutex::new(log)).with_ansi(false).try_init();
    }
    let secs: u64 = seconds.parse().unwrap_or(8);
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = rt.block_on(async {
        let paths = Paths::resolve()?;
        let sc = crate::sidecar::Sidecar::spawn(crate::sidecar::Role::Capture, &paths.fluid_models()).await?;
        let mut rx = sc.subscribe();
        let last = Arc::new(std::sync::Mutex::new(serde_json::Value::Null));
        let l2 = last.clone();
        let fwd = tokio::spawn(async move {
            while let Ok(n) = rx.recv().await {
                if n.method == "capture.status" {
                    *l2.lock().unwrap() = n.params;
                }
            }
        });
        let started = sc.call("capture.start", serde_json::json!({"outputDir": dir.to_string_lossy()})).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
        let status = last.lock().unwrap().clone();
        let stopped = sc.call("capture.stop", serde_json::json!({})).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        fwd.abort();
        sc.shutdown().await;
        // Plain-language verdict: room noise always gives a non-zero mic level, and a
        // working tap reports non-zero system level while anything is playing.
        let mic_rms = status["micRms"].as_f64().unwrap_or(0.0);
        let sys_rms = status["sysRms"].as_f64().unwrap_or(0.0);
        let sys_silent_ms = status["systemSilentMs"].as_i64().unwrap_or(0);
        let elapsed_ms = status["elapsedMs"].as_i64().unwrap_or(0);
        let system_heard = sys_rms > 0.0002 || (elapsed_ms > 0 && sys_silent_ms < elapsed_ms);
        let verdict = serde_json::json!({
            "microphone": if mic_rms > 0.0002 { "audio received" } else { "SILENT: check Microphone permission for Muesli" },
            "systemAudio": if system_heard { "audio received" } else { "SILENT: either nothing was playing, or System Audio Recording is denied for Muesli (System Settings → Privacy & Security → Screen & System Audio Recording; or run scripts/tcc-reset.sh and retry)" },
        });
        let out = serde_json::json!({"verdict": verdict, "started": started, "lastStatus": status, "result": stopped});
        std::fs::write(dir.join("capture-test.json"), serde_json::to_string_pretty(&out)?)?;
        anyhow::Ok(out)
    });
    match result {
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            0
        }
        Err(e) => {
            let _ = std::fs::write(
                dir.join("capture-test.json"),
                serde_json::json!({"error": format!("{e:#}")}).to_string(),
            );
            eprintln!("capture test failed: {e:#}");
            1
        }
    }
}
