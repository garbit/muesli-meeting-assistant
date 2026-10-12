//! llama.cpp (Metal) engine for Muesli.
//!
//! llama.cpp objects are not `Send`, so everything lives on one dedicated OS
//! thread that owns the backend, the loaded model and, per request, a context.
//! Callers talk to it through [`Engine`], a cheap cloneable handle.

pub mod template;

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use llama_cpp_2::context::params::{KvCacheType, LlamaContextParams};
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::{LogOptions, send_logs_to_tracing};

pub use template::ChatMessage;

#[derive(Debug, Clone)]
pub struct GenParams {
    pub n_ctx: u32,
    pub max_tokens: i32,
    pub temperature: f32,
    pub repeat_penalty: f32,
    pub repeat_last_n: i32,
    pub seed: u32,
}

impl Default for GenParams {
    fn default() -> Self {
        Self { n_ctx: 32_768, max_tokens: 1_800, temperature: 0.3, repeat_penalty: 1.15, repeat_last_n: 64, seed: 42 }
    }
}

#[derive(Debug, Clone, Default)]
pub struct GenResult {
    pub text: String,
    pub prompt_tokens: usize,
    pub output_tokens: usize,
    pub prompt_seconds: f32,
    pub gen_seconds: f32,
    pub cancelled: bool,
}

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub path: PathBuf,
    pub n_ctx_train: u32,
    pub n_params: u64,
    pub size_bytes: u64,
    pub has_chat_template: bool,
}

pub type TokenCallback = Box<dyn FnMut(&str) + Send>;

enum Cmd {
    Load {
        path: PathBuf,
        reply: Sender<Result<ModelInfo>>,
    },
    Unload {
        reply: Sender<()>,
    },
    Info {
        reply: Sender<Option<ModelInfo>>,
    },
    CountTokens {
        text: String,
        reply: Sender<Result<usize>>,
    },
    RenderChat {
        messages: Vec<ChatMessage>,
        reply: Sender<Result<String>>,
    },
    Generate {
        prompt: String,
        params: GenParams,
        on_token: TokenCallback,
        cancel: Arc<AtomicBool>,
        reply: Sender<Result<GenResult>>,
    },
}

/// Handle to the engine thread. Clone freely; commands are serialised.
#[derive(Clone)]
pub struct Engine {
    tx: Sender<Cmd>,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

impl Engine {
    /// The process-wide engine (one llama.cpp backend per process).
    pub fn global() -> &'static Engine {
        ENGINE.get_or_init(|| {
            let (tx, rx) = mpsc::channel::<Cmd>();
            std::thread::Builder::new()
                .name("muesli-llm".into())
                .stack_size(32 * 1024 * 1024)
                .spawn(move || worker(rx))
                .expect("spawn llm thread");
            Engine { tx }
        })
    }

    fn send<T>(&self, make: impl FnOnce(Sender<T>) -> Cmd) -> Result<T> {
        let (tx, rx) = mpsc::channel();
        self.tx.send(make(tx)).map_err(|_| anyhow!("llm thread gone"))?;
        rx.recv().map_err(|_| anyhow!("llm thread dropped reply"))
    }

    pub fn load(&self, path: &Path) -> Result<ModelInfo> {
        self.send(|reply| Cmd::Load { path: path.to_path_buf(), reply })?
    }

    pub fn unload(&self) -> Result<()> {
        self.send(|reply| Cmd::Unload { reply })
    }

    pub fn info(&self) -> Result<Option<ModelInfo>> {
        self.send(|reply| Cmd::Info { reply })
    }

    pub fn count_tokens(&self, text: &str) -> Result<usize> {
        self.send(|reply| Cmd::CountTokens { text: text.to_string(), reply })?
    }

    pub fn render_chat(&self, messages: &[ChatMessage]) -> Result<String> {
        self.send(|reply| Cmd::RenderChat { messages: messages.to_vec(), reply })?
    }

    pub fn generate(
        &self,
        prompt: &str,
        params: GenParams,
        on_token: TokenCallback,
        cancel: Arc<AtomicBool>,
    ) -> Result<GenResult> {
        self.send(|reply| Cmd::Generate { prompt: prompt.to_string(), params, on_token, cancel, reply })?
    }
}

struct Loaded {
    info: ModelInfo,
    model: LlamaModel,
    /// Parsed once per model; None when the model has no usable Jinja template.
    renderer: Option<template::Renderer>,
    bos: String,
}

fn worker(rx: mpsc::Receiver<Cmd>) {
    send_logs_to_tracing(LogOptions::default().with_logs_enabled(false));
    let backend = match LlamaBackend::init() {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("llama backend init failed: {e}");
            return;
        }
    };
    let mut loaded: Option<Loaded> = None;
    let unload_lock = Mutex::new(());

    for cmd in rx {
        match cmd {
            Cmd::Load { path, reply } => {
                if loaded.as_ref().map(|l| l.info.path == path).unwrap_or(false) {
                    let _ = reply.send(Ok(loaded.as_ref().unwrap().info.clone()));
                    continue;
                }
                loaded = None; // free the previous model before loading another
                let _ = reply.send(load_model(&backend, &path).map(|l| {
                    let info = l.info.clone();
                    loaded = Some(l);
                    info
                }));
            }
            Cmd::Unload { reply } => {
                let _g = unload_lock.lock();
                loaded = None;
                let _ = reply.send(());
            }
            Cmd::Info { reply } => {
                let _ = reply.send(loaded.as_ref().map(|l| l.info.clone()));
            }
            Cmd::CountTokens { text, reply } => {
                let _ = reply.send(match &loaded {
                    Some(l) => Ok(l.model.vocab().tokenize(text.as_bytes(), false, true).len()),
                    None => Err(anyhow!("no model loaded")),
                });
            }
            Cmd::RenderChat { messages, reply } => {
                let _ = reply.send(match &loaded {
                    Some(l) => Ok(render(l, &messages)),
                    None => Err(anyhow!("no model loaded")),
                });
            }
            Cmd::Generate { prompt, params, on_token, cancel, reply } => {
                let _ = reply.send(match &loaded {
                    Some(l) => generate(&backend, &l.model, &prompt, &params, on_token, &cancel),
                    None => Err(anyhow!("no model loaded")),
                });
            }
        }
    }
}

fn load_model(backend: &LlamaBackend, path: &Path) -> Result<Loaded> {
    let t0 = Instant::now();
    let params = LlamaModelParams::default().with_n_gpu_layers(1000);
    let model =
        LlamaModel::load_from_file(backend, path, &params).with_context(|| format!("loading {}", path.display()))?;
    let template_src =
        model.chat_template(None).ok().map(|t| String::from_utf8_lossy(t.as_c_str().to_bytes()).to_string());
    let bos = model
        .vocab()
        .tokenize(b"", true, true)
        .first()
        .map(|t| String::from_utf8_lossy(&model.vocab().token_to_piece(*t, true, None)).to_string())
        .unwrap_or_default();
    let renderer = template_src.and_then(|src| match template::Renderer::new(&src, &bos) {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::warn!("chat template does not parse, using Gemma 4 layout: {e:#}");
            None
        }
    });
    let info = ModelInfo {
        path: path.to_path_buf(),
        n_ctx_train: model.n_ctx_train(),
        n_params: model.n_params(),
        size_bytes: model.size(),
        has_chat_template: renderer.is_some(),
    };
    tracing::info!(
        "loaded {} in {:.1}s (bos={bos:?}, template={})",
        path.display(),
        t0.elapsed().as_secs_f32(),
        renderer.is_some()
    );
    Ok(Loaded { info, model, renderer, bos })
}

fn render(l: &Loaded, messages: &[ChatMessage]) -> String {
    if let Some(r) = &l.renderer {
        match r.render(messages) {
            Ok(p) => return p,
            Err(e) => tracing::warn!("chat template render failed, using Gemma 4 layout: {e:#}"),
        }
    }
    template::render_gemma4(messages, &l.bos)
}

fn generate(
    backend: &LlamaBackend,
    model: &LlamaModel,
    prompt: &str,
    params: &GenParams,
    mut on_token: TokenCallback,
    cancel: &AtomicBool,
) -> Result<GenResult> {
    // The template already inserted the BOS text, so tokenize without adding
    // specials but parsing them.
    let tokens = model.vocab().tokenize(prompt.as_bytes(), false, true);
    let n_ctx = params.n_ctx.min(model.n_ctx_train().max(2048));
    if tokens.len() as i32 + params.max_tokens >= n_ctx as i32 {
        bail!("prompt ({} tokens) + max_tokens ({}) exceeds n_ctx {n_ctx}", tokens.len(), params.max_tokens);
    }
    let n_batch = 2048usize;
    let ctx_params = LlamaContextParams::default()
        .with_n_ctx(NonZeroU32::new(n_ctx))
        .with_n_batch(n_batch as u32)
        .with_n_ubatch(512)
        .with_type_k(KvCacheType::Q8_0)
        .with_type_v(KvCacheType::Q8_0)
        .with_flash_attention_policy(llama_cpp_sys_2::LLAMA_FLASH_ATTN_TYPE_ENABLED);
    let mut ctx = model.new_context(backend, ctx_params).context("creating llama context")?;

    let t0 = Instant::now();
    let mut batch = LlamaBatch::new(n_batch, 1);
    let mut pos: i32 = 0;
    let last_index = tokens.len() - 1;
    for chunk in tokens.chunks(n_batch) {
        if cancel.load(Ordering::Relaxed) {
            return Ok(GenResult { cancelled: true, prompt_tokens: tokens.len(), ..Default::default() });
        }
        batch.clear();
        for tok in chunk {
            let is_last = pos as usize == last_index;
            batch.add(*tok, pos, &[0], is_last)?;
            pos += 1;
        }
        ctx.decode(&mut batch).context("prompt decode")?;
    }
    let prompt_seconds = t0.elapsed().as_secs_f32();

    let mut sampler = LlamaSampler::chain_simple([
        LlamaSampler::penalties(model.n_vocab(), params.repeat_last_n, params.repeat_penalty, 0.0, 0.0),
        LlamaSampler::temp(params.temperature),
        LlamaSampler::dist(params.seed),
    ]);
    let t1 = Instant::now();
    let mut text = String::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut n_gen = 0usize;
    let mut cancelled = false;
    loop {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        let token = sampler.sample(&ctx, batch.n_tokens() - 1);
        sampler.accept(token);
        if model.vocab().is_eog(token) || n_gen as i32 >= params.max_tokens {
            break;
        }
        pending.extend_from_slice(&model.vocab().token_to_piece(token, true, None));
        // Emit the longest valid UTF-8 prefix; keep split multibyte tails.
        let valid = match std::str::from_utf8(&pending) {
            Ok(_) => pending.len(),
            Err(e) => e.valid_up_to(),
        };
        if valid > 0 {
            let s = String::from_utf8_lossy(&pending[..valid]).to_string();
            text.push_str(&s);
            on_token(&s);
            pending.drain(..valid);
        }
        batch.clear();
        batch.add(token, pos, &[0], true)?;
        pos += 1;
        ctx.decode(&mut batch).context("generation decode")?;
        n_gen += 1;
    }
    if !pending.is_empty() {
        let s = String::from_utf8_lossy(&pending).to_string();
        text.push_str(&s);
        on_token(&s);
    }
    Ok(GenResult {
        text,
        prompt_tokens: tokens.len(),
        output_tokens: n_gen,
        prompt_seconds,
        gen_seconds: t1.elapsed().as_secs_f32(),
        cancelled,
    })
}
