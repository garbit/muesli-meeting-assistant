//! Permission bookkeeping for an ad-hoc signed app.
//!
//! macOS keys TCC grants to the code signature, so every rebuild or update looks
//! like a new app and the Microphone / System Audio Recording grants are lost.
//!
//! * Microphone status is a public API (`AVCaptureDevice`), read through the
//!   capture sidecar; it never prompts.
//! * System Audio Recording has no public query at all, and a denied tap simply
//!   delivers zeros, so that status is learnt by exercising a tap once per build
//!   (`permissions.systemAudioProbe`, see `SystemAudioProbe.swift`) and remembered
//!   in the database together with the build it was measured for.
//!
//! The UI reads one shared status, pushed as `permissions:changed` whenever it is
//! re-derived: at startup, after the setup flow, after a recheck, when the window
//! regains focus while something is missing, and when a recording reveals the
//! truth about system audio.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_opener::OpenerExt;

use crate::AppState;
use crate::db::{Db, now_iso};
use crate::recording::Recorder;
use crate::sidecar::{Role, Sidecar};

/// Event carrying a fresh [`PermissionsStatus`].
pub const EVENT: &str = "permissions:changed";

const ACKNOWLEDGED_KEY: &str = "acknowledged_code_hash";
const SYSTEM_AUDIO_KEY: &str = "system_audio_probe";

pub const AUTHORIZED: &str = "authorized";
pub const DENIED: &str = "denied";
pub const RESTRICTED: &str = "restricted";
pub const NOT_DETERMINED: &str = "notDetermined";
/// The capture sidecar could not be started or did not answer.
pub const UNAVAILABLE: &str = "unavailable";

pub const MIC_OFF_MESSAGE: &str = "Microphone access is off for Muesli. Turn it on in System Settings → Privacy & Security → Microphone, then try again.";
const DETAIL_NO_STREAM: &str = "the system audio tap produced no stream";
const DETAIL_SILENT: &str =
    "the system audio tap delivered only silence; if the macOS prompt is still open, allow it and Muesli will re-check";

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// `AVCaptureDevice.requestAccess` blocks until the user answers the prompt.
const MIC_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Tap budget when the System Audio prompt may be pending: the tap can stay
/// silent until the user answers, so the sidecar keeps trying for this long.
const TAP_PROBE_PROMPT_MS: u64 = 10_000;
/// Tap budget once macOS has decided (rechecks never prompt).
const TAP_PROBE_RECHECK_MS: u64 = 3_000;
/// Added to the sidecar's own budget: a synchronous prompt can hold the call.
const TAP_PROBE_GRACE: Duration = Duration::from_secs(60);
/// Focus re-checks while something is missing (the user is probably fixing it)…
const FOCUS_DEBOUNCE: Duration = Duration::from_secs(5);
/// …and while everything is fine (catching a grant revoked in System Settings).
const FOCUS_DEBOUNCE_READY: Duration = Duration::from_secs(10);

/// Cached status plus the lock that serialises everything which spawns a
/// sidecar or shows a prompt (startup probe, the UI's first fetch, the setup
/// flow, rechecks, focus refreshes), so two callers never double-prompt.
#[derive(Default)]
pub struct PermissionsCache {
    status: Mutex<Option<PermissionsStatus>>,
    last_probe: Mutex<Option<Instant>>,
    flow: tokio::sync::Mutex<()>,
}

impl PermissionsCache {
    pub fn get(&self) -> Option<PermissionsStatus> {
        self.status.lock().ok().and_then(|g| g.clone())
    }

    fn set(&self, status: &PermissionsStatus) {
        if let Ok(mut g) = self.status.lock() {
            *g = Some(status.clone());
        }
        self.touch();
    }

    fn touch(&self) {
        if let Ok(mut g) = self.last_probe.lock() {
            *g = Some(Instant::now());
        }
    }

    fn probed_within(&self, d: Duration) -> bool {
        self.last_probe.lock().ok().and_then(|g| *g).is_some_and(|t| t.elapsed() < d)
    }
}

/// First 8 bytes of the SHA-256 over the running executable and the sidecar
/// next to it: changes with every build, which is exactly when TCC forgets us
/// (the ad-hoc signature covers the whole bundle, so a sidecar-only rebuild is
/// a new app to macOS too). Computed once per process; the updater replaces
/// the bundle before relaunching.
pub fn current_code_hash() -> Result<String> {
    static HASH: OnceLock<String> = OnceLock::new();
    if let Some(h) = HASH.get() {
        return Ok(h.clone());
    }
    use sha2::{Digest, Sha256};
    let exe = std::env::current_exe()?;
    let mut hasher = Sha256::new();
    hasher.update(std::fs::read(&exe)?);
    if let Ok(sidecar) = crate::sidecar::sidecar_path()
        && let Ok(bytes) = std::fs::read(&sidecar)
    {
        hasher.update(bytes);
    }
    let digest = hasher.finalize();
    let hash: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    Ok(HASH.get_or_init(|| hash).clone())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsStatus {
    /// `authorized` | `denied` | `restricted` | `notDetermined` | `unavailable`.
    pub microphone: String,
    /// `authorized` | `denied` | `notDetermined` (never exercised for this build).
    pub system_audio: String,
    /// Why system audio is `denied`, when known.
    pub system_audio_detail: Option<String>,
    pub code_hash: String,
    /// A different build was acknowledged before: the user has seen an update.
    pub code_hash_changed: bool,
    /// No build was ever acknowledged: a fresh install (or a wiped database).
    pub first_run: bool,
    /// Both grants confirmed; recording will capture both sides.
    pub ready: bool,
    /// The setup flow (prompts) has not run for this build and something is
    /// missing or undecided. The UI runs it automatically.
    pub needs_setup: bool,
}

/// What the last tap exercise found, and for which build.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemAudioProbe {
    pub code_hash: String,
    pub status: String,
    pub detail: Option<String>,
    pub at: String,
}

fn derive_status(
    microphone: &str,
    stored: Option<&SystemAudioProbe>,
    code_hash: &str,
    acknowledged: Option<&str>,
) -> PermissionsStatus {
    let (system_audio, system_audio_detail) = match stored {
        Some(p) if p.code_hash == code_hash => (p.status.clone(), p.detail.clone()),
        _ => (NOT_DETERMINED.to_string(), None),
    };
    let ready = microphone == AUTHORIZED && system_audio == AUTHORIZED;
    let acknowledged_current = acknowledged == Some(code_hash);
    let undecided = microphone == NOT_DETERMINED || system_audio == NOT_DETERMINED;
    PermissionsStatus {
        microphone: microphone.to_string(),
        system_audio,
        system_audio_detail,
        code_hash: code_hash.to_string(),
        code_hash_changed: acknowledged.is_some_and(|a| a != code_hash),
        first_run: acknowledged.is_none(),
        ready,
        // A stale TCC entry from the previous build can report `denied` without
        // ever prompting, so an unacknowledged build that is not ready needs the
        // flow too, not only an undecided one.
        needs_setup: !acknowledged_current && microphone != UNAVAILABLE && (undecided || !ready),
    }
}

/// Turn the sidecar's raw facts into a verdict.
fn classify(facts: &Value) -> (String, Option<String>) {
    let code = |k: &str| facts[k].as_i64().unwrap_or(0);
    let denied = |why: String| (DENIED.to_string(), Some(why));
    if let Some(e) = facts["error"].as_str() {
        return denied(e.to_string());
    }
    if code("tapCreate") != 0 {
        return denied(format!("creating the system audio tap failed (OSStatus {})", code("tapCreate")));
    }
    if code("aggregateCreate") != 0 {
        return denied(format!("creating the capture device failed (OSStatus {})", code("aggregateCreate")));
    }
    if !facts["tapPresent"].as_bool().unwrap_or(false) {
        return denied(DETAIL_NO_STREAM.to_string());
    }
    if code("start") != 0 {
        return denied(format!("starting the capture device failed (OSStatus {})", code("start")));
    }
    if facts["nonZero"].as_bool().unwrap_or(false) {
        return (AUTHORIZED.to_string(), None);
    }
    denied(DETAIL_SILENT.to_string())
}

fn status_from_db(db: &Db, microphone: &str) -> Result<PermissionsStatus> {
    let code_hash = current_code_hash()?;
    let acknowledged: Option<String> = db.get_setting(ACKNOWLEDGED_KEY)?;
    // An unreadable record (older format) counts as "never exercised".
    let stored: Option<SystemAudioProbe> = db.get_setting(SYSTEM_AUDIO_KEY).ok().flatten();
    Ok(derive_status(microphone, stored.as_ref(), &code_hash, acknowledged.as_deref()))
}

/// Remember that the setup flow ran for this build.
fn acknowledge(db: &Db) -> Result<()> {
    db.set_setting(ACKNOWLEDGED_KEY, &current_code_hash()?)
}

/// A build whose grants are all in place needs no flow: acknowledge it quietly.
fn settle(db: &Db, status: PermissionsStatus) -> Result<PermissionsStatus> {
    if status.ready && (status.first_run || status.code_hash_changed) {
        acknowledge(db)?;
        return status_from_db(db, &status.microphone);
    }
    Ok(status)
}

async fn spawn_capture(state: &AppState) -> Option<Arc<Sidecar>> {
    match Sidecar::spawn(Role::Capture, &state.paths.fluid_models()).await {
        Ok(sc) => Some(sc),
        Err(e) => {
            tracing::warn!("permissions: cannot spawn the capture sidecar: {e:#}");
            None
        }
    }
}

async fn mic_status(sc: &Sidecar) -> String {
    match sc.call_timeout("permissions.probe", json!({}), PROBE_TIMEOUT).await {
        Ok(v) => v["microphone"].as_str().unwrap_or(UNAVAILABLE).to_string(),
        Err(e) => {
            tracing::warn!("permissions.probe failed: {e}");
            UNAVAILABLE.to_string()
        }
    }
}

/// Exercise the tap through `sc`, classify and persist the outcome for this build.
async fn tap_probe(db: &Db, sc: &Sidecar, budget_ms: u64) -> SystemAudioProbe {
    let timeout = Duration::from_millis(budget_ms) + TAP_PROBE_GRACE;
    let facts = match sc.call_timeout("permissions.systemAudioProbe", json!({"timeoutMs": budget_ms}), timeout).await {
        Ok(v) => v,
        Err(e) => json!({"error": format!("the system audio check did not complete: {e}")}),
    };
    let (status, detail) = classify(&facts);
    tracing::info!("system audio probe: {status} ({}) facts={facts}", detail.as_deref().unwrap_or("audio heard"));
    let record = SystemAudioProbe { code_hash: current_code_hash().unwrap_or_default(), status, detail, at: now_iso() };
    if let Err(e) = db.set_setting(SYSTEM_AUDIO_KEY, &record) {
        tracing::warn!("saving the system audio probe: {e:#}");
    }
    record
}

/// One sidecar session: read the microphone and, when asked, exercise the tap
/// (which updates the stored verdict). Returns the microphone status.
async fn observe(state: &AppState, exercise_tap: bool, budget_ms: u64) -> String {
    let Some(sc) = spawn_capture(state).await else { return UNAVAILABLE.to_string() };
    let microphone = mic_status(&sc).await;
    if exercise_tap {
        tap_probe(&state.db, &sc, budget_ms).await;
    }
    sc.shutdown().await;
    microphone
}

/// Side-effect free status: microphone from the sidecar, system audio from the
/// record kept for this build. Never creates a tap, never prompts.
pub async fn probe(state: &AppState) -> Result<PermissionsStatus> {
    let microphone = observe(state, false, 0).await;
    status_from_db(&state.db, &microphone)
}

fn publish(app: &AppHandle, status: &PermissionsStatus) {
    app.state::<PermissionsCache>().set(status);
    if let Err(e) = app.emit(EVENT, status) {
        tracing::warn!("emitting {EVENT}: {e}");
    }
}

async fn is_recording(app: &AppHandle) -> bool {
    app.state::<Arc<Recorder>>().is_recording().await
}

/// Startup: establish the status once and push it to the UI. The UI's own first
/// fetch takes the same lock, so whichever comes second reuses the result.
///
/// Once a build has a system-audio verdict, the tap is exercised again here:
/// macOS applies a revoked System Audio Recording grant only after the app
/// quits, so the next launch is the first chance to notice. It never prompts
/// (macOS has decided) and costs about a second. A build with no verdict yet is
/// left to the setup flow, which shows the card before any prompt.
pub async fn probe_at_startup(app: AppHandle) {
    let cache = app.state::<PermissionsCache>();
    let _flow = cache.flow.lock().await;
    if cache.get().is_some() {
        return;
    }
    let state = app.state::<AppState>();
    let decided = status_from_db(&state.db, AUTHORIZED).map(|s| s.system_audio != NOT_DETERMINED).unwrap_or(false);
    let exercise = decided && !is_recording(&app).await;
    let microphone = observe(&state, exercise, TAP_PROBE_RECHECK_MS).await;
    match status_from_db(&state.db, &microphone).and_then(|s| settle(&state.db, s)) {
        Ok(status) => publish(&app, &status),
        Err(e) => tracing::warn!("startup permission probe failed: {e:#}"),
    }
}

/// The window regained focus, typically after a trip to System Settings: look
/// again. While something is missing this is how the fix shows up without a
/// click; while everything is fine it is how a grant revoked in System Settings
/// shows up. The tap is exercised only when macOS has decided (a denied or
/// authorised verdict), so this never prompts; it also repairs a `denied`
/// recorded while the system prompt was still open (the prompt closing is what
/// gives the window its focus back). A build that still needs the setup flow
/// is left alone.
pub async fn refresh_on_focus(app: AppHandle) {
    let cache = app.state::<PermissionsCache>();
    let Some(current) = cache.get() else { return };
    let debounce = if current.ready { FOCUS_DEBOUNCE_READY } else { FOCUS_DEBOUNCE };
    if current.needs_setup || current.microphone == UNAVAILABLE || cache.probed_within(debounce) {
        return;
    }
    let Ok(_flow) = cache.flow.try_lock() else { return };
    let state = app.state::<AppState>();
    let exercise = current.system_audio != NOT_DETERMINED && !is_recording(&app).await;
    let microphone = observe(&state, exercise, TAP_PROBE_RECHECK_MS).await;
    match status_from_db(&state.db, &microphone).and_then(|s| settle(&state.db, s)) {
        Ok(next) if next != current => publish(&app, &next),
        Ok(_) => cache.touch(),
        Err(e) => tracing::warn!("focus permission refresh failed: {e:#}"),
    }
}

/// Called before a recording starts (sidebar button and tray alike). A denied
/// microphone would record silence, so refuse with a message the user can act
/// on. Refusing wrongly is the costly mistake, so a cached denial is confirmed
/// live first; with no cached status at all the recording goes ahead.
pub async fn ensure_microphone(app: &AppHandle) -> Result<()> {
    let cache = app.state::<PermissionsCache>();
    let Some(current) = cache.get() else { return Ok(()) };
    if !matches!(current.microphone.as_str(), DENIED | RESTRICTED) {
        return Ok(());
    }
    let state = app.state::<AppState>();
    let Some(sc) = spawn_capture(&state).await else { return Ok(()) };
    let microphone = mic_status(&sc).await;
    sc.shutdown().await;
    if let Ok(next) = status_from_db(&state.db, &microphone)
        && next != current
    {
        publish(app, &next);
    }
    if matches!(microphone.as_str(), DENIED | RESTRICTED) {
        return Err(anyhow!(MIC_OFF_MESSAGE));
    }
    Ok(())
}

/// A live recording is the one place where system audio shows its true state:
/// a missing or silent tap means the grant is not in effect, audio actually
/// heard means it is. Record either for this build and tell the UI. Cheap when
/// nothing changed, so it can run on every status tick.
pub fn note_recording_system_audio(app: &AppHandle, system_audio: &str, sys_rms: f64) {
    let (status, detail) = match system_audio {
        "missing" => (DENIED, Some("the last recording had no system audio stream")),
        "silent" => (DENIED, Some("the last recording heard no system audio while other apps were playing")),
        "ok" if sys_rms > 0.0 => (AUTHORIZED, None),
        _ => return,
    };
    let cache = app.state::<PermissionsCache>();
    let Some(current) = cache.get() else { return };
    if current.system_audio == status {
        return;
    }
    let Ok(code_hash) = current_code_hash() else { return };
    let state = app.state::<AppState>();
    let record =
        SystemAudioProbe { code_hash, status: status.to_string(), detail: detail.map(str::to_string), at: now_iso() };
    if let Err(e) = state.db.set_setting(SYSTEM_AUDIO_KEY, &record) {
        tracing::warn!("saving the system audio state from a recording: {e:#}");
        return;
    }
    match status_from_db(&state.db, &current.microphone) {
        Ok(next) => publish(app, &next),
        Err(e) => tracing::warn!("deriving permissions after a recording: {e:#}"),
    }
}

/// One line for the diagnostics report and `muesli --doctor`.
pub async fn diagnostics_summary(state: &AppState) -> String {
    match probe(state).await {
        Ok(s) => {
            let stored: Option<SystemAudioProbe> = state.db.get_setting(SYSTEM_AUDIO_KEY).ok().flatten();
            let acknowledged = if s.first_run {
                "never"
            } else if s.code_hash_changed {
                "an older build"
            } else {
                "this build"
            };
            format!(
                "microphone={} systemAudio={}{} ready={} build={} acknowledged={} lastSystemAudioProbe={}",
                s.microphone,
                s.system_audio,
                s.system_audio_detail.map(|d| format!(" ({d})")).unwrap_or_default(),
                s.ready,
                s.code_hash,
                acknowledged,
                stored
                    .map(|p| format!("{} {} for build {}", p.at, p.status, p.code_hash))
                    .unwrap_or_else(|| "none".to_string()),
            )
        }
        Err(e) => format!("error: {e:#}"),
    }
}

// ---- commands -------------------------------------------------------------

type CmdResult<T> = Result<T, String>;

#[tauri::command]
pub async fn permissions_status(app: AppHandle, refresh: Option<bool>) -> CmdResult<PermissionsStatus> {
    let refresh = refresh == Some(true);
    let cache = app.state::<PermissionsCache>();
    if !refresh && let Some(s) = cache.get() {
        return Ok(s);
    }
    let _flow = cache.flow.lock().await;
    // Someone else (the startup probe, usually) may have filled the cache meanwhile.
    if !refresh && let Some(s) = cache.get() {
        return Ok(s);
    }
    let state = app.state::<AppState>();
    let status = probe(&state).await.and_then(|s| settle(&state.db, s)).map_err(|e| e.to_string())?;
    publish(&app, &status);
    Ok(status)
}

/// The setup flow: prompt for the microphone if macOS has not decided, then
/// exercise the tap (which prompts for System Audio Recording if needed), and
/// remember that this build has been through it. Runs the whole flow in one
/// sidecar process. Interim statuses are published so the UI can show the
/// microphone result while the second prompt is up.
#[tauri::command]
pub async fn permissions_request(app: AppHandle) -> CmdResult<PermissionsStatus> {
    let cache = app.state::<PermissionsCache>();
    let state = app.state::<AppState>();
    let _flow = cache.flow.lock().await;
    let db = &state.db;
    let Some(sc) = spawn_capture(&state).await else {
        let status = status_from_db(db, UNAVAILABLE).map_err(|e| e.to_string())?;
        publish(&app, &status);
        return Ok(status);
    };
    let mut microphone = mic_status(&sc).await;
    if microphone == NOT_DETERMINED {
        if let Err(e) = sc.call_timeout("permissions.request", json!({}), MIC_REQUEST_TIMEOUT).await {
            tracing::warn!("permissions.request failed: {e}");
        }
        microphone = mic_status(&sc).await;
        if let Ok(interim) = status_from_db(db, &microphone) {
            publish(&app, &interim);
        }
    }
    let before = status_from_db(db, &microphone).map_err(|e| e.to_string())?;
    if before.system_audio != AUTHORIZED && !is_recording(&app).await {
        let budget = if before.system_audio == NOT_DETERMINED { TAP_PROBE_PROMPT_MS } else { TAP_PROBE_RECHECK_MS };
        tap_probe(db, &sc, budget).await;
    }
    sc.shutdown().await;
    acknowledge(db).map_err(|e| e.to_string())?;
    let status = status_from_db(db, &microphone).map_err(|e| e.to_string())?;
    publish(&app, &status);
    Ok(status)
}

/// Re-read the microphone and exercise the tap again. Once macOS has decided,
/// neither step prompts, so this is safe to offer as a button.
#[tauri::command]
pub async fn permissions_recheck(app: AppHandle) -> CmdResult<PermissionsStatus> {
    let cache = app.state::<PermissionsCache>();
    let state = app.state::<AppState>();
    let _flow = cache.flow.lock().await;
    let db = &state.db;
    let exercise = !is_recording(&app).await;
    let microphone = observe(&state, exercise, TAP_PROBE_RECHECK_MS).await;
    if microphone != UNAVAILABLE {
        acknowledge(db).map_err(|e| e.to_string())?;
    }
    let status = status_from_db(db, &microphone).map_err(|e| e.to_string())?;
    publish(&app, &status);
    Ok(status)
}

/// Open System Settings → Privacy & Security at the right pane. Done from Rust
/// because the opener plugin's default scope only allows http(s)/mailto/tel.
#[tauri::command]
pub fn permissions_open_settings(app: AppHandle, pane: String) -> CmdResult<()> {
    let url = match pane.as_str() {
        "microphone" => "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone",
        "system-audio" => "x-apple.systempreferences:com.apple.preference.security?Privacy_AudioCapture",
        _ => "x-apple.systempreferences:com.apple.preference.security",
    };
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(code_hash: &str, status: &str) -> SystemAudioProbe {
        SystemAudioProbe { code_hash: code_hash.into(), status: status.into(), detail: None, at: now_iso() }
    }

    #[test]
    fn classify_reads_the_tap_facts() {
        let ok = json!({"tapCreate": 0, "aggregateCreate": 0, "tapPresent": true, "start": 0, "nonZero": true});
        assert_eq!(classify(&ok), (AUTHORIZED.to_string(), None));

        let silent = json!({"tapCreate": 0, "aggregateCreate": 0, "tapPresent": true, "start": 0, "nonZero": false});
        assert_eq!(classify(&silent), (DENIED.to_string(), Some(DETAIL_SILENT.to_string())));

        let no_stream = json!({"tapCreate": 0, "aggregateCreate": 0, "tapPresent": false});
        assert_eq!(classify(&no_stream), (DENIED.to_string(), Some(DETAIL_NO_STREAM.to_string())));

        let (status, detail) = classify(&json!({"tapCreate": 1852797029}));
        assert_eq!(status, DENIED);
        assert!(detail.unwrap().contains("1852797029"));

        let (status, detail) = classify(&json!({"error": "no default audio device"}));
        assert_eq!(status, DENIED);
        assert_eq!(detail.as_deref(), Some("no default audio device"));

        // Audio heard wins even when the first IO took a while.
        let late = json!({"tapCreate": 0, "aggregateCreate": 0, "tapPresent": true, "start": 0, "firstIoMs": 4200, "nonZero": true});
        assert_eq!(classify(&late).0, AUTHORIZED);
    }

    #[test]
    fn derive_status_matrix() {
        // Fresh install: nothing decided, nothing acknowledged.
        let s = derive_status(NOT_DETERMINED, None, "h1", None);
        assert!(s.first_run && !s.code_hash_changed && !s.ready && s.needs_setup);
        assert_eq!(s.system_audio, NOT_DETERMINED);

        // Fresh install but macOS already holds the grants (database wiped): no flow.
        let s = derive_status(AUTHORIZED, Some(&record("h1", AUTHORIZED)), "h1", None);
        assert!(s.ready && !s.needs_setup && s.first_run);

        // Same build acknowledged and everything granted.
        let s = derive_status(AUTHORIZED, Some(&record("h1", AUTHORIZED)), "h1", Some("h1"));
        assert!(s.ready && !s.needs_setup && !s.first_run && !s.code_hash_changed);

        // Rebuild: the tap record belongs to the old build, so system audio is undecided again.
        let s = derive_status(NOT_DETERMINED, Some(&record("h1", AUTHORIZED)), "h2", Some("h1"));
        assert!(s.code_hash_changed && !s.first_run && s.needs_setup && !s.ready);
        assert_eq!(s.system_audio, NOT_DETERMINED);

        // Rebuild with a stale TCC entry: denied without a prompt still needs the flow.
        let s = derive_status(DENIED, Some(&record("h2", DENIED)), "h2", Some("h1"));
        assert!(s.needs_setup && !s.ready);

        // Flow ran for this build, microphone still off: banner territory, no card.
        let s = derive_status(DENIED, Some(&record("h2", AUTHORIZED)), "h2", Some("h2"));
        assert!(!s.needs_setup && !s.ready);

        // Sidecar missing: never start a flow that cannot run.
        let s = derive_status(UNAVAILABLE, None, "h2", None);
        assert!(!s.needs_setup && !s.ready);

        // Detail travels with a denial for the current build only.
        let mut r = record("h2", DENIED);
        r.detail = Some("x".into());
        assert_eq!(derive_status(AUTHORIZED, Some(&r), "h2", Some("h2")).system_audio_detail.as_deref(), Some("x"));
        assert_eq!(derive_status(AUTHORIZED, Some(&r), "h3", Some("h2")).system_audio_detail, None);
    }

    #[test]
    fn stored_probe_round_trips_and_settles() {
        let db = Db::open_in_memory().unwrap();
        let hash = current_code_hash().unwrap();

        // Nothing stored: undecided for this build.
        let s = status_from_db(&db, AUTHORIZED).unwrap();
        assert_eq!(s.system_audio, NOT_DETERMINED);
        assert!(s.first_run && s.needs_setup);

        // A record for another build does not count.
        db.set_setting(SYSTEM_AUDIO_KEY, &record("other", AUTHORIZED)).unwrap();
        assert_eq!(status_from_db(&db, AUTHORIZED).unwrap().system_audio, NOT_DETERMINED);

        // A record for this build does, and settling acknowledges a ready build.
        db.set_setting(SYSTEM_AUDIO_KEY, &record(&hash, AUTHORIZED)).unwrap();
        let s = status_from_db(&db, AUTHORIZED).unwrap();
        assert!(s.ready && s.first_run);
        let s = settle(&db, s).unwrap();
        assert!(s.ready && !s.first_run && !s.code_hash_changed && !s.needs_setup);
        let acknowledged: Option<String> = db.get_setting(ACKNOWLEDGED_KEY).unwrap();
        assert_eq!(acknowledged.as_deref(), Some(hash.as_str()));

        // Settling never acknowledges a build that is not ready.
        db.set_setting(ACKNOWLEDGED_KEY, &"stale").unwrap();
        let s = settle(&db, status_from_db(&db, DENIED).unwrap()).unwrap();
        assert!(s.code_hash_changed && s.needs_setup);

        // An unreadable record is treated as absent rather than failing.
        db.set_setting(SYSTEM_AUDIO_KEY, &"not a record").unwrap();
        assert_eq!(status_from_db(&db, AUTHORIZED).unwrap().system_audio, NOT_DETERMINED);
    }
}
