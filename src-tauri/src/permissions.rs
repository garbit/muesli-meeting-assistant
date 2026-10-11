//! Permission bookkeeping for an ad-hoc signed app. macOS keys TCC grants to the
//! code signature, so every rebuild or update looks like a new app and the
//! Microphone / System Audio Recording grants are lost. We hash the main
//! executable, remember it, and tell the UI when it changed so it can show the
//! re-grant screen.

use std::sync::Mutex;

use anyhow::Result;
use serde::Serialize;
use serde_json::json;
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;

use crate::AppState;
use crate::sidecar::{Role, Sidecar};

const SETTING_KEY: &str = "acknowledged_code_hash";

/// Cached probe result; probing spawns a sidecar, so do it once per session and
/// again only after an explicit request.
#[derive(Default)]
pub struct PermissionsCache {
    status: Mutex<Option<PermissionsStatus>>,
}

pub fn current_code_hash() -> Result<String> {
    let exe = std::env::current_exe()?;
    let bytes = std::fs::read(&exe)?;
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(&bytes);
    Ok(digest.iter().take(8).map(|b| format!("{b:02x}")).collect())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsStatus {
    pub microphone: String,
    pub system_audio: String,
    pub code_hash: String,
    pub code_hash_changed: bool,
    pub first_run: bool,
}

type CmdResult<T> = Result<T, String>;

async fn probe(state: &AppState) -> CmdResult<PermissionsStatus> {
    let code_hash = current_code_hash().map_err(|e| e.to_string())?;
    let acknowledged: Option<String> = state.db.get_setting(SETTING_KEY).map_err(|e| e.to_string())?;
    let mut microphone = "unknown".to_string();
    let mut system_audio = "unknown".to_string();
    if let Ok(sc) = Sidecar::spawn(Role::Capture, &state.paths.fluid_models()).await {
        if let Ok(v) = sc.call_timeout("permissions.probe", json!({}), std::time::Duration::from_secs(10)).await {
            microphone = v["microphone"].as_str().unwrap_or("unknown").to_string();
            system_audio = v["systemAudio"].as_str().unwrap_or("unknown").to_string();
        }
        sc.shutdown().await;
    }
    Ok(PermissionsStatus {
        microphone,
        system_audio,
        code_hash_changed: acknowledged.as_deref().map(|a| a != code_hash).unwrap_or(false),
        first_run: acknowledged.is_none(),
        code_hash,
    })
}

#[tauri::command]
pub async fn permissions_status(
    state: State<'_, AppState>,
    cache: State<'_, PermissionsCache>,
    refresh: Option<bool>,
) -> CmdResult<PermissionsStatus> {
    if refresh != Some(true)
        && let Some(s) = cache.status.lock().map_err(|e| e.to_string())?.clone()
    {
        return Ok(s);
    }
    let s = probe(&state).await?;
    *cache.status.lock().map_err(|e| e.to_string())? = Some(s.clone());
    Ok(s)
}

/// Trigger the microphone prompt (attributed to Muesli) and remember this build.
#[tauri::command]
pub async fn permissions_request(
    state: State<'_, AppState>,
    cache: State<'_, PermissionsCache>,
) -> CmdResult<PermissionsStatus> {
    if let Ok(sc) = Sidecar::spawn(Role::Capture, &state.paths.fluid_models()).await {
        let _ = sc.call_timeout("permissions.request", json!({}), std::time::Duration::from_secs(120)).await;
        sc.shutdown().await;
    }
    acknowledge(&state)?;
    let s = probe(&state).await?;
    *cache.status.lock().map_err(|e| e.to_string())? = Some(s.clone());
    Ok(s)
}

fn acknowledge(state: &AppState) -> CmdResult<()> {
    let hash = current_code_hash().map_err(|e| e.to_string())?;
    state.db.set_setting(SETTING_KEY, &hash).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn permissions_acknowledge(
    state: State<'_, AppState>,
    cache: State<'_, PermissionsCache>,
) -> CmdResult<PermissionsStatus> {
    acknowledge(&state)?;
    let s = probe(&state).await?;
    *cache.status.lock().map_err(|e| e.to_string())? = Some(s.clone());
    Ok(s)
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
