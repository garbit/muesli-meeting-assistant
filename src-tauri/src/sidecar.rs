//! Client for the Swift sidecar (`muesli-sidecar`), which speaks newline-delimited
//! JSON-RPC 2.0 over stdio. Two roles exist: `process` (one-shot ML worker) and
//! `capture` (lives for one meeting). Both are spawned from the directory of the
//! current executable, where Tauri places `externalBin` binaries.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{broadcast, oneshot, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Process,
    Capture,
}

impl Role {
    fn arg(self) -> &'static str {
        match self {
            Role::Process => "process",
            Role::Capture => "capture",
        }
    }
}

/// A server → client notification (e.g. `job.progress`, `capture.status`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("sidecar error {code}: {message}")]
    Remote { code: i64, message: String, data: Option<Value> },
    #[error("sidecar exited before replying")]
    Closed,
    #[error("sidecar did not answer {method} within {after:?}")]
    Timeout { method: String, after: Duration },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

pub struct Sidecar {
    role: Role,
    child: Mutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    pending: Pending,
    notifications: broadcast::Sender<Notification>,
    next_id: AtomicU64,
}

/// Resolve the sidecar binary: next to the current executable (dev and bundle),
/// or `MUESLI_SIDECAR` for ad-hoc testing.
pub fn sidecar_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("MUESLI_SIDECAR") {
        return Ok(PathBuf::from(p));
    }
    let exe = std::env::current_exe().context("current_exe")?;
    let dir = exe.parent().ok_or_else(|| anyhow!("no parent dir for {exe:?}"))?;
    let candidate = dir.join("muesli-sidecar");
    if candidate.exists() {
        return Ok(candidate);
    }
    Err(anyhow!("sidecar binary not found at {candidate:?}; run scripts/build-sidecar.sh"))
}

impl Sidecar {
    pub async fn spawn(role: Role, models_root: &std::path::Path) -> Result<Arc<Self>> {
        Self::spawn_logged(role, models_root, None).await
    }

    /// Spawn and additionally append every stderr line of the sidecar to `log_to`
    /// (e.g. `<meeting>/process.log`), so a failure can be diagnosed after the fact.
    pub async fn spawn_logged(role: Role, models_root: &std::path::Path, log_to: Option<PathBuf>) -> Result<Arc<Self>> {
        let path = sidecar_path()?;
        let mut child = Command::new(&path)
            .arg(role.arg())
            .env("MUESLI_MODELS_ROOT", models_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The capture role finalises the recording when its stdin closes, so it
            // must not be SIGKILLed when the app exits; a process worker exits on EOF too.
            .kill_on_drop(role == Role::Process)
            .spawn()
            .with_context(|| format!("spawning {path:?}"))?;
        let stdin = child.stdin.take().context("sidecar stdin")?;
        let stdout = child.stdout.take().context("sidecar stdout")?;
        let stderr = child.stderr.take().context("sidecar stderr")?;
        let (tx, _rx) = broadcast::channel(256);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        let this = Arc::new(Self {
            role,
            child: Mutex::new(Some(child)),
            stdin: Mutex::new(Some(stdin)),
            pending: pending.clone(),
            notifications: tx.clone(),
            next_id: AtomicU64::new(1),
        });

        // stdout reader: route responses to pending calls, fan out notifications.
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    tracing::warn!(target: "sidecar", "unparseable line: {line}");
                    continue;
                };
                if let Some(id) = v.get("id").and_then(Value::as_u64) {
                    if let Some(tx) = pending.lock().await.remove(&id) {
                        let result = if let Some(err) = v.get("error") {
                            Err(RpcError::Remote {
                                code: err.get("code").and_then(Value::as_i64).unwrap_or(-32000),
                                message: err.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
                                data: err.get("data").cloned(),
                            })
                        } else {
                            Ok(v.get("result").cloned().unwrap_or(Value::Null))
                        };
                        let _ = tx.send(result);
                    }
                } else if let Some(method) = v.get("method").and_then(Value::as_str) {
                    let _ = tx.send(Notification {
                        method: method.to_string(),
                        params: v.get("params").cloned().unwrap_or(Value::Null),
                    });
                }
            }
            // stdout closed: fail every pending call.
            for (_, tx) in pending.lock().await.drain() {
                let _ = tx.send(Err(RpcError::Closed));
            }
        });

        // stderr → tracing (and the per-meeting log file when requested)
        let role_name = role.arg();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let mut file = match &log_to {
                Some(p) => tokio::fs::OpenOptions::new().create(true).append(true).open(p).await.ok(),
                None => None,
            };
            if let Some(f) = file.as_mut() {
                let _ = f.write_all(format!("\n=== {} {role_name} sidecar started ===\n", chrono::Local::now().to_rfc3339()).as_bytes()).await;
            }
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(target: "sidecar", "[{role_name}] {line}");
                if let Some(f) = file.as_mut() {
                    let _ = f.write_all(line.as_bytes()).await;
                    let _ = f.write_all(b"\n").await;
                }
            }
            if let Some(f) = file.as_mut() {
                let _ = f.flush().await;
            }
        });

        Ok(this)
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.notifications.subscribe()
    }

    /// Send a request and wait for its response, failing after `timeout`.
    pub async fn call_timeout(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        match tokio::time::timeout(timeout, self.call_inner(id, method, params)).await {
            Ok(r) => r,
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(RpcError::Timeout { method: method.to_string(), after: timeout })
            }
        }
    }

    /// Send a request and wait for its response (5 minute default timeout).
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.call_timeout(method, params, Duration::from_secs(300)).await
    }

    async fn call_inner(&self, id: u64, method: &str, params: Value) -> Result<Value, RpcError> {
        let mut line = serde_json::to_string(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .map_err(|e| RpcError::Other(e.into()))?;
        line.push('\n');
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let written: Result<(), RpcError> = async {
            let mut guard = self.stdin.lock().await;
            let stdin = guard.as_mut().ok_or(RpcError::Closed)?;
            stdin.write_all(line.as_bytes()).await.map_err(|e| RpcError::Other(anyhow!("writing to sidecar stdin: {e}")))?;
            stdin.flush().await.map_err(|e| RpcError::Other(anyhow!("flush: {e}")))
        }
        .await;
        if let Err(e) = written {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }
        rx.await.unwrap_or(Err(RpcError::Closed))
    }

    /// Close the sidecar's stdin, which ends its read loop, and wait briefly for
    /// it to exit. A `process` worker that lingers is killed; a `capture` sidecar
    /// that lingers is finalising a recording and is left to finish on its own
    /// (tokio reaps it when it exits).
    pub async fn shutdown(&self) {
        drop(self.stdin.lock().await.take());
        if let Some(mut child) = self.child.lock().await.take()
            && tokio::time::timeout(Duration::from_secs(3), child.wait()).await.is_err()
        {
            match self.role {
                Role::Process => {
                    tracing::warn!(target: "sidecar", "process sidecar did not exit; killing");
                    let _ = child.kill().await;
                }
                Role::Capture => tracing::info!(target: "sidecar", "capture sidecar still finalising; leaving it to finish"),
            }
        }
    }

    pub async fn health(&self) -> Result<Value, RpcError> {
        self.call("health", json!({})).await
    }
}
