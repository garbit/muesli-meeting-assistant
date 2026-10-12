//! Tauri commands exposed to the React UI. Every command returns a plain
//! `Result<T, String>` so errors render as strings in the UI.

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, State};

use crate::models::{self, ModelStatus};
use crate::sidecar::{Role, Sidecar};
use crate::AppState;

type CmdResult<T> = Result<T, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub version: String,
    pub data_dir: String,
    pub manifest_version: u32,
    pub presets: Value,
    pub models: Value,
}

#[tauri::command]
pub fn app_info(state: State<'_, AppState>) -> CmdResult<AppInfo> {
    Ok(AppInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        data_dir: state.paths.root.display().to_string(),
        manifest_version: state.manifest.version,
        presets: serde_json::to_value(&state.manifest.presets).map_err(err)?,
        models: serde_json::to_value(&state.manifest.models).map_err(err)?,
    })
}

/// Install status of every manifest model: GGUFs from disk, CoreML models via
/// the sidecar's `models.status`.
#[tauri::command]
pub async fn models_status(state: State<'_, AppState>) -> CmdResult<Vec<ModelStatus>> {
    let mut out: Vec<ModelStatus> = state
        .manifest
        .models
        .iter()
        .filter(|m| m.runtime == "llama.cpp")
        .map(|m| models::gguf_status(&state.paths, m))
        .collect();

    match Sidecar::spawn(Role::Process, &state.paths.fluid_models()).await {
        Ok(sc) => {
            if let Ok(v) = sc.call("models.status", json!({})).await
                && let Some(arr) = v.as_array() {
                    for item in arr {
                        out.push(ModelStatus {
                            id: item["id"].as_str().unwrap_or("").to_string(),
                            installed: item["installed"].as_bool().unwrap_or(false),
                            bytes_on_disk: item["bytes"].as_u64().unwrap_or(0),
                            path: item["path"].as_str().map(str::to_string),
                        });
                    }
                }
            sc.shutdown().await;
        }
        Err(e) => tracing::warn!("sidecar unavailable for models.status: {e}"),
    }
    Ok(out)
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DownloadProgress {
    pub id: String,
    pub fraction: f64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub phase: String,
}

/// Download one approved model. Emits `models:progress` events to the UI.
#[tauri::command]
pub async fn models_download(app: AppHandle, state: State<'_, AppState>, id: String) -> CmdResult<ModelStatus> {
    let entry = state.manifest.get(&id).map_err(err)?.clone();
    match entry.runtime.as_str() {
        "llama.cpp" => {
            for file in &entry.files {
                let app2 = app.clone();
                let id2 = id.clone();
                models::download_gguf(&state.paths, &entry, file, move |done, total| {
                    let _ = app2.emit(
                        "models:progress",
                        DownloadProgress {
                            id: id2.clone(),
                            fraction: if total > 0 { done as f64 / total as f64 } else { 0.0 },
                            bytes_done: done,
                            bytes_total: total,
                            phase: "downloading".into(),
                        },
                    );
                })
                .await
                .map_err(err)?;
            }
            Ok(models::gguf_status(&state.paths, &entry))
        }
        "sidecar" => {
            let sc = Sidecar::spawn(Role::Process, &state.paths.fluid_models()).await.map_err(err)?;
            let mut rx = sc.subscribe();
            let app2 = app.clone();
            let id2 = id.clone();
            let forward = tokio::spawn(async move {
                while let Ok(n) = rx.recv().await {
                    if n.method == "job.progress" {
                        let _ = app2.emit(
                            "models:progress",
                            DownloadProgress {
                                id: id2.clone(),
                                fraction: n.params["fraction"].as_f64().unwrap_or(0.0),
                                bytes_done: 0,
                                bytes_total: 0,
                                phase: n.params["phase"].as_str().unwrap_or("").to_string(),
                            },
                        );
                    }
                }
            });
            let result = sc.call("models.download", json!({"ids": [id], "jobId": format!("dl-{id}")})).await;
            sc.shutdown().await;
            forward.abort();
            let v = result.map_err(err)?;
            let item = v.as_array().and_then(|a| a.first()).cloned().unwrap_or(Value::Null);
            Ok(ModelStatus {
                id: item["id"].as_str().unwrap_or(&id).to_string(),
                installed: item["installed"].as_bool().unwrap_or(false),
                bytes_on_disk: item["bytes"].as_u64().unwrap_or(0),
                path: item["path"].as_str().map(str::to_string),
            })
        }
        other => Err(format!("unknown runtime {other}")),
    }
}

#[tauri::command]
pub async fn sidecar_health(state: State<'_, AppState>) -> CmdResult<Value> {
    let sc = Sidecar::spawn(Role::Process, &state.paths.fluid_models()).await.map_err(err)?;
    let v = sc.health().await.map_err(err)?;
    sc.shutdown().await;
    Ok(v)
}
