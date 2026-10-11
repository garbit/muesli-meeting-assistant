//! Embedding engine: EmbeddingGemma (or nomic-embed) through llama.cpp on its
//! own thread, so a query embedding never waits behind a two-minute summary on
//! the `muesli-llm` thread. The model stays loaded with one persistent context.

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::context::params::{LlamaContextParams, LlamaPoolingType};
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::token::LlamaToken;

/// Matryoshka truncation: the first 256 dimensions of the model's output.
pub const EMBED_DIM: usize = 256;
/// Tokens per decode call and the context size; non-causal models need `n_ubatch >= n_tokens`.
const N_BATCH: u32 = 2048;
/// Texts packed into one decode, each as its own sequence.
const N_SEQ_MAX: u32 = 8;

#[derive(Debug, Clone)]
pub struct EmbedInfo {
    pub path: PathBuf,
    pub arch: String,
    pub n_embd_out: usize,
    pub dim: usize,
    pub n_ctx: u32,
    pub add_bos: bool,
    pub add_eos: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arch {
    GemmaEmbedding,
    NomicBert,
}

impl Arch {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "gemma-embedding" => Some(Self::GemmaEmbedding),
            "nomic-bert" | "nomic-bert-moe" => Some(Self::NomicBert),
            _ => None,
        }
    }

    fn query_prompt(self, q: &str) -> String {
        match self {
            Self::GemmaEmbedding => format!("task: search result | query: {q}"),
            Self::NomicBert => format!("search_query: {q}"),
        }
    }

    fn document_prompt(self, title: Option<&str>, text: &str) -> String {
        match self {
            Self::GemmaEmbedding => format!("title: {} | text: {text}", title.unwrap_or("none")),
            Self::NomicBert => format!("search_document: {text}"),
        }
    }
}

pub enum EmbedItem {
    Query(String),
    Document { title: Option<String>, text: String },
}

enum Cmd {
    Load { path: PathBuf, reply: Sender<Result<EmbedInfo>> },
    Unload { reply: Sender<()> },
    Info { reply: Sender<Option<EmbedInfo>> },
    CountTokens { text: String, reply: Sender<Result<usize>> },
    Embed { items: Vec<EmbedItem>, reply: Sender<Result<Vec<Vec<f32>>>> },
}

/// Handle to the embedding thread. Clone freely; commands are serialised.
#[derive(Clone)]
pub struct Embedder {
    tx: Sender<Cmd>,
}

static EMBEDDER: OnceLock<Embedder> = OnceLock::new();

impl Embedder {
    pub fn global() -> &'static Embedder {
        EMBEDDER.get_or_init(|| {
            let (tx, rx) = mpsc::channel::<Cmd>();
            std::thread::Builder::new()
                .name("muesli-embed".into())
                .stack_size(16 * 1024 * 1024)
                .spawn(move || worker(rx))
                .expect("spawn embed thread");
            Embedder { tx }
        })
    }

    fn send<T>(&self, make: impl FnOnce(Sender<T>) -> Cmd) -> Result<T> {
        let (tx, rx) = mpsc::channel();
        self.tx.send(make(tx)).map_err(|_| anyhow!("embed thread gone"))?;
        rx.recv().map_err(|_| anyhow!("embed thread dropped reply"))
    }

    /// Load (or keep) the model at `path`; a different path replaces the current one.
    pub fn load(&self, path: &Path) -> Result<EmbedInfo> {
        self.send(|reply| Cmd::Load { path: path.to_path_buf(), reply })?
    }

    pub fn unload(&self) -> Result<()> {
        self.send(|reply| Cmd::Unload { reply })
    }

    pub fn info(&self) -> Result<Option<EmbedInfo>> {
        self.send(|reply| Cmd::Info { reply })
    }

    pub fn count_tokens(&self, text: &str) -> Result<usize> {
        self.send(|reply| Cmd::CountTokens { text: text.to_string(), reply })?
    }

    /// Embed `(title, text)` documents with the model's document prompt; unit vectors of `EMBED_DIM`.
    pub fn embed_documents(&self, docs: &[(Option<String>, String)]) -> Result<Vec<Vec<f32>>> {
        let items = docs.iter().map(|(t, d)| EmbedItem::Document { title: t.clone(), text: d.clone() }).collect();
        self.send(|reply| Cmd::Embed { items, reply })?
    }

    /// Embed a query with the model's query prompt.
    pub fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = self.send(|reply| Cmd::Embed { items: vec![EmbedItem::Query(text.to_string())], reply })??;
        v.pop().ok_or_else(|| anyhow!("no embedding returned"))
    }
}

struct Loaded {
    info: EmbedInfo,
    model: LlamaModel,
    arch: Arch,
}

fn worker(rx: Receiver<Cmd>) {
    let backend = match crate::backend() {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("embedder: {e:#}");
            return;
        }
    };
    let mut next: Option<Cmd> = None;
    loop {
        let cmd = match next.take() {
            Some(c) => c,
            None => match rx.recv() {
                Ok(c) => c,
                Err(_) => return,
            },
        };
        match cmd {
            Cmd::Load { path, reply } => match load_model(backend, &path) {
                Ok(loaded) => {
                    // `serve` owns the context, which borrows the model; both end before the
                    // next load, by ordinary scoping.
                    let after = serve(backend, &loaded, reply, &rx);
                    drop(loaded);
                    match after {
                        Some(Cmd::Unload { reply }) => {
                            let _ = reply.send(());
                        }
                        other => next = other,
                    }
                }
                Err(e) => {
                    let _ = reply.send(Err(e));
                }
            },
            Cmd::Unload { reply } => {
                let _ = reply.send(());
            }
            Cmd::Info { reply } => {
                let _ = reply.send(None);
            }
            Cmd::CountTokens { reply, .. } => {
                let _ = reply.send(Err(anyhow!("no embedding model loaded")));
            }
            Cmd::Embed { reply, .. } => {
                let _ = reply.send(Err(anyhow!("no embedding model loaded")));
            }
        }
    }
}

/// Command loop for one loaded model. Returns the command that ends the session
/// (a load of another path, or an unload) so the caller can act after dropping it.
fn serve(backend: &LlamaBackend, l: &Loaded, reply: Sender<Result<EmbedInfo>>, rx: &Receiver<Cmd>) -> Option<Cmd> {
    let params = LlamaContextParams::default()
        .with_n_ctx(NonZeroU32::new(l.info.n_ctx))
        .with_n_batch(N_BATCH)
        .with_n_ubatch(N_BATCH)
        .with_n_seq_max(N_SEQ_MAX)
        .with_kv_unified(true)
        .with_embeddings(true)
        .with_pooling_type(LlamaPoolingType::Mean);
    let mut ctx = match l.model.new_context(backend, params).context("creating embedding context") {
        Ok(c) => c,
        Err(e) => {
            let _ = reply.send(Err(e));
            return None;
        }
    };
    let _ = reply.send(Ok(l.info.clone()));
    for cmd in rx.iter() {
        match cmd {
            Cmd::Load { path, reply } if path == l.info.path => {
                let _ = reply.send(Ok(l.info.clone()));
            }
            Cmd::Load { path, reply } => return Some(Cmd::Load { path, reply }),
            Cmd::Unload { reply } => return Some(Cmd::Unload { reply }),
            Cmd::Info { reply } => {
                let _ = reply.send(Some(l.info.clone()));
            }
            Cmd::CountTokens { text, reply } => {
                let _ = reply.send(Ok(l.model.vocab().tokenize(text.as_bytes(), true, false).len()));
            }
            Cmd::Embed { items, reply } => {
                let _ = reply.send(embed(&mut ctx, l, &items));
            }
        }
    }
    None
}

fn load_model(backend: &LlamaBackend, path: &Path) -> Result<Loaded> {
    let t0 = Instant::now();
    let params = LlamaModelParams::default().with_n_gpu_layers(1000);
    let model =
        LlamaModel::load_from_file(backend, path, &params).with_context(|| format!("loading {}", path.display()))?;
    let arch_s = model.meta_val_str("general.architecture").unwrap_or_default();
    let arch = Arch::parse(&arch_s).ok_or_else(|| {
        anyhow!(
            "{} is a {arch_s:?} model; only gemma-embedding and nomic-bert embedding models are supported",
            path.display()
        )
    })?;
    let n_embd_out = model.n_embd_out().max(0) as usize;
    if n_embd_out < EMBED_DIM {
        bail!("{} produces {n_embd_out}-d embeddings, fewer than the {EMBED_DIM} required", path.display());
    }
    if let Ok(p) = model.meta_val_str(&format!("{arch_s}.pooling_type"))
        && p != "1"
    {
        tracing::warn!("{arch_s} declares pooling type {p}; using mean pooling regardless");
    }
    let (add_bos, add_eos) = {
        let vocab = model.vocab();
        (vocab.should_add_bos(), vocab.should_add_eos())
    };
    if arch == Arch::GemmaEmbedding && !add_eos {
        tracing::warn!("gemma-embedding model reports no EOS token; vectors may not match the reference pipeline");
    }
    let n_ctx = N_BATCH.min(model.n_ctx_train().max(1));
    let info = EmbedInfo {
        path: path.to_path_buf(),
        arch: arch_s.clone(),
        n_embd_out,
        dim: EMBED_DIM,
        n_ctx,
        add_bos,
        add_eos,
    };
    tracing::info!(
        "loaded embedding model {} in {:.1}s (arch={arch_s}, out={n_embd_out}, ctx={n_ctx}, bos={add_bos}, eos={add_eos})",
        path.display(),
        t0.elapsed().as_secs_f32()
    );
    Ok(Loaded { info, model, arch })
}

fn normalise(v: &mut [f32]) {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

/// Tokenize with the model's prompt, pack several texts per decode (one
/// sequence each), read the pooled embedding per sequence, truncate, normalise.
fn embed(ctx: &mut LlamaContext<'_>, l: &Loaded, items: &[EmbedItem]) -> Result<Vec<Vec<f32>>> {
    let vocab = l.model.vocab();
    let n_ctx = l.info.n_ctx as usize;
    let mut tokenized: Vec<Vec<LlamaToken>> = Vec::with_capacity(items.len());
    for item in items {
        let text = match item {
            EmbedItem::Query(q) => l.arch.query_prompt(q),
            EmbedItem::Document { title, text } => l.arch.document_prompt(title.as_deref(), text),
        };
        let mut toks = vocab.tokenize(text.as_bytes(), true, false);
        if toks.len() > n_ctx {
            tracing::warn!("embedding input of {} tokens truncated to {n_ctx}", toks.len());
            toks.truncate(n_ctx - 1);
            if l.info.add_eos {
                toks.push(vocab.eos());
            }
        }
        tokenized.push(toks);
    }

    let mut results = vec![vec![0.0f32; EMBED_DIM]; items.len()];
    let todo: Vec<usize> = tokenized.iter().enumerate().filter(|(_, t)| !t.is_empty()).map(|(i, _)| i).collect();
    if todo.len() < items.len() {
        tracing::warn!("{} empty text(s) embedded as zero vectors", items.len() - todo.len());
    }
    let mut batch = LlamaBatch::new(N_BATCH as usize, N_SEQ_MAX as i32);
    let mut cursor = 0usize;
    while cursor < todo.len() {
        batch.clear();
        let mut group: Vec<usize> = Vec::new();
        let mut total = 0usize;
        while cursor < todo.len() && group.len() < N_SEQ_MAX as usize {
            let idx = todo[cursor];
            let len = tokenized[idx].len();
            if !group.is_empty() && total + len > N_BATCH as usize {
                break;
            }
            let seq = group.len() as i32;
            for (p, tok) in tokenized[idx].iter().enumerate() {
                // Pooled embeddings need every token flagged as output.
                batch.add(*tok, p as i32, &[seq], true)?;
            }
            total += len;
            group.push(idx);
            cursor += 1;
        }
        ctx.decode(&mut batch).context("embedding decode")?;
        for (seq, idx) in group.iter().enumerate() {
            let emb = ctx
                .embeddings_seq_ith(seq as i32)
                .map_err(|e| anyhow!("no pooled embedding for sequence {seq}: {e:?}"))?;
            if emb.len() < EMBED_DIM {
                bail!("embedding has {} dims, expected at least {EMBED_DIM}", emb.len());
            }
            let mut v: Vec<f32> = emb[..EMBED_DIM].to_vec();
            normalise(&mut v);
            results[*idx] = v;
        }
        // No-op for cache-less embedding models; keeps a cached fallback model correct.
        ctx.clear_kv_cache();
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_path() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("MUESLI_EMBED_GGUF") {
            return Some(PathBuf::from(p));
        }
        let home = std::env::var("HOME").ok()?;
        let p = PathBuf::from(home).join(
            "Library/Application Support/Muesli/models/ggml-org/embeddinggemma-300M-GGUF/embeddinggemma-300M-Q8_0.gguf",
        );
        p.exists().then_some(p)
    }

    fn cos(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn prompts_follow_the_model_card() {
        assert_eq!(Arch::GemmaEmbedding.query_prompt("x"), "task: search result | query: x");
        assert_eq!(Arch::GemmaEmbedding.document_prompt(None, "d"), "title: none | text: d");
        assert_eq!(Arch::GemmaEmbedding.document_prompt(Some("T"), "d"), "title: T | text: d");
        assert_eq!(Arch::NomicBert.query_prompt("x"), "search_query: x");
        assert_eq!(Arch::NomicBert.document_prompt(Some("T"), "d"), "search_document: d");
        assert!(Arch::parse("llama").is_none());
    }

    /// Needs the EmbeddingGemma GGUF (`MUESLI_EMBED_GGUF` or the default models dir).
    #[test]
    #[ignore]
    fn embeddinggemma_end_to_end() {
        let Some(path) = model_path() else {
            eprintln!("skipped: EmbeddingGemma GGUF not installed");
            return;
        };
        let e = Embedder::global();
        let info = e.load(&path).unwrap();
        assert_eq!(info.arch, "gemma-embedding");
        assert!(info.add_eos, "the reference pipeline appends <eos>");
        assert_eq!((info.dim, info.n_ctx), (256, 2048));
        assert!(e.info().unwrap().is_some());

        let q = e.embed_query("How much does the enterprise plan cost?").unwrap();
        assert_eq!(q.len(), EMBED_DIM);
        assert!((cos(&q, &q) - 1.0).abs() < 1e-3, "unit norm");
        let docs = vec![
            (
                Some("Pricing call".to_string()),
                "Bob: The enterprise tier is 40 dollars per seat per month.".to_string(),
            ),
            (Some("Standup".to_string()), "Alice: I fixed the flaky test in the audio recorder.".to_string()),
        ];
        let d = e.embed_documents(&docs).unwrap();
        assert!(
            cos(&q, &d[0]) > cos(&q, &d[1]),
            "paraphrase {} should beat unrelated {}",
            cos(&q, &d[0]),
            cos(&q, &d[1])
        );
        let d2 = e.embed_documents(&docs).unwrap();
        assert!(cos(&d[0], &d2[0]) > 0.9999, "deterministic");

        // Packing several sequences into one decode must match one-by-one embedding.
        let many: Vec<(Option<String>, String)> = (0..10)
            .map(|i| (Some(format!("Doc {i}")), format!("Sentence number {i} is about topic {}.", i % 3)))
            .collect();
        let batched = e.embed_documents(&many).unwrap();
        for (i, doc) in many.iter().enumerate() {
            let single = e.embed_documents(std::slice::from_ref(doc)).unwrap();
            let c = cos(&batched[i], &single[0]);
            assert!(c > 0.999, "doc {i}: batched vs single cosine {c}");
        }

        // Over-long input truncates without error; empty input is a zero vector.
        let long = "word ".repeat(3_000);
        let l = e.embed_documents(&[(None, long), (None, String::new())]).unwrap();
        assert_eq!(l[0].len(), EMBED_DIM);
        assert!(l[1].iter().all(|x| *x == 0.0));
        assert!(e.count_tokens("hello world").unwrap() >= 2);

        e.unload().unwrap();
        assert!(e.info().unwrap().is_none());
    }
}
