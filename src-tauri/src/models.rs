//! Curated model manifest (the allow-list), install status, and the GGUF
//! downloader. CoreML models (ASR, diarisation) are fetched by the sidecar via
//! `models.download`; this module only downloads llama.cpp GGUF files itself.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::paths::{Paths, file_size};

pub const MANIFEST_JSON: &str = include_str!("../models.manifest.json");

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestFile {
    pub path: String,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelEntry {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub organisation: String,
    pub licence: String,
    pub repo: String,
    #[serde(default)]
    pub files: Vec<ManifestFile>,
    pub runtime: String,
    #[serde(default)]
    pub approx_bytes: u64,
    #[serde(default)]
    pub min_ram_gb: u32,
    #[serde(default)]
    pub min_macos: Option<String>,
    #[serde(default)]
    pub context_tokens: Option<u32>,
    #[serde(default)]
    pub preset: Option<String>,
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub provenance: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preset {
    pub asr: String,
    pub diarisation: String,
    pub llm: String,
    pub min_ram_gb: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub version: u32,
    pub models: Vec<ModelEntry>,
    pub presets: HashMap<String, Preset>,
}

impl Manifest {
    pub fn embedded() -> Result<Self> {
        serde_json::from_str(MANIFEST_JSON).context("parsing embedded manifest")
    }

    /// The policy gate: anything not in the manifest does not exist.
    pub fn get(&self, id: &str) -> Result<&ModelEntry> {
        self.models.iter().find(|m| m.id == id).ok_or_else(|| anyhow!("model '{id}' is not in the approved manifest"))
    }

    pub fn default_llm(&self) -> Option<&ModelEntry> {
        self.models.iter().find(|m| m.kind == "llm" && m.default)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelStatus {
    pub id: String,
    pub installed: bool,
    pub bytes_on_disk: u64,
    pub path: Option<String>,
}

/// Status of GGUF (llama.cpp) models from the file system. Sidecar models are
/// reported by the sidecar's `models.status`.
pub fn gguf_status(paths: &Paths, entry: &ModelEntry) -> ModelStatus {
    let mut total = 0u64;
    let mut installed = !entry.files.is_empty();
    let mut first: Option<PathBuf> = None;
    for f in &entry.files {
        let p = paths.model_file(&entry.repo, &f.path);
        let size = file_size(&p);
        if size == 0 || p.with_extension("partial").exists() {
            installed = false;
        }
        total += size;
        first.get_or_insert(p);
    }
    ModelStatus { id: entry.id.clone(), installed, bytes_on_disk: total, path: first.map(|p| p.display().to_string()) }
}

/// Download one GGUF file from Hugging Face with HTTP range resume, then verify
/// its SHA-256 when the manifest pins one. `progress(done, total)` is called as
/// bytes arrive.
pub async fn download_gguf(
    paths: &Paths,
    entry: &ModelEntry,
    file: &ManifestFile,
    progress: impl Fn(u64, u64) + Send + Sync,
) -> Result<PathBuf> {
    let dest = paths.model_file(&entry.repo, &file.path);
    let part = dest.with_extension("partial");
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let url = format!("https://huggingface.co/{}/resolve/main/{}", entry.repo, file.path);
    let client = reqwest::Client::builder().user_agent(concat!("muesli/", env!("CARGO_PKG_VERSION"))).build()?;

    let existing = file_size(&part);
    let mut req = client.get(&url);
    if existing > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={existing}-"));
    }
    let resp = req.send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !(status.is_success() || status == reqwest::StatusCode::PARTIAL_CONTENT) {
        bail!("{url}: HTTP {status}");
    }
    let resumed = status == reqwest::StatusCode::PARTIAL_CONTENT;
    let content_len = resp.content_length().unwrap_or(0);
    let total = if resumed { existing + content_len } else { content_len };

    let mut out =
        tokio::fs::OpenOptions::new().create(true).write(true).append(resumed).truncate(!resumed).open(&part).await?;
    let mut done = if resumed { existing } else { 0 };
    let mut stream = resp.bytes_stream();
    let mut last_report = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading body")?;
        out.write_all(&chunk).await?;
        done += chunk.len() as u64;
        if done - last_report > 4 * 1024 * 1024 {
            progress(done, total);
            last_report = done;
        }
    }
    out.flush().await?;
    drop(out);
    progress(done, total);

    if let Some(expected) = &file.sha256 {
        let actual = sha256_file(&part).await?;
        if !actual.eq_ignore_ascii_case(expected) {
            tokio::fs::remove_file(&part).await.ok();
            bail!("sha256 mismatch for {}: expected {expected}, got {actual}", file.path);
        }
    }
    tokio::fs::rename(&part, &dest).await?;
    Ok(dest)
}

pub async fn sha256_file(path: &std::path::Path) -> Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<String> {
        use std::io::Read;
        let mut f = std::fs::File::open(&path)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 8 * 1024 * 1024];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>())
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_and_has_default_llm() {
        let m = Manifest::embedded().unwrap();
        assert!(m.models.len() >= 4);
        assert_eq!(m.default_llm().unwrap().id, "gemma-4-26b-a4b-it-q4_0");
        assert!(m.get("qwen-anything").is_err(), "policy gate must reject unknown ids");
        assert!(m.presets.contains_key("balanced"));
    }
}
