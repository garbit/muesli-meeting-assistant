//! On-disk layout under `~/Library/Application Support/Muesli/`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
    /// Model store; defaults to `<root>/models` but can live elsewhere
    /// (`MUESLI_MODELS_DIR`) so scratch libraries can share downloaded models.
    pub models: PathBuf,
}

impl Paths {
    pub fn resolve() -> Result<Self> {
        let root = if let Ok(p) = std::env::var("MUESLI_HOME") {
            PathBuf::from(p)
        } else {
            let home = std::env::var("HOME").context("HOME not set")?;
            PathBuf::from(home).join("Library/Application Support/Muesli")
        };
        let models = match std::env::var("MUESLI_MODELS_DIR") {
            Ok(p) if !p.is_empty() => PathBuf::from(p),
            _ => root.join("models"),
        };
        for d in ["meetings", "logs"] {
            std::fs::create_dir_all(root.join(d))?;
        }
        for d in ["FluidInference", "ggml-org"] {
            std::fs::create_dir_all(models.join(d))?;
        }
        Ok(Self { root, models })
    }

    pub fn db(&self) -> PathBuf {
        self.root.join("muesli.db")
    }

    /// Parent directory FluidAudio appends repo folder names to.
    pub fn fluid_models(&self) -> PathBuf {
        self.models.join("FluidInference")
    }

    /// GGUF files live at models/<org>/<repo>/<file>, mirroring Hugging Face.
    pub fn model_file(&self, repo: &str, file: &str) -> PathBuf {
        self.models.join(repo).join(file)
    }

    pub fn meeting_dir(&self, meeting_id: &str) -> PathBuf {
        self.root.join("meetings").join(meeting_id)
    }

    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }
}

pub fn file_size(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}
