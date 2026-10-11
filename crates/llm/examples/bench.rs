//! M0 spike: load a GGUF with Metal, feed a ~N-token synthetic meeting
//! transcript through the chat template, and report prompt-eval and
//! generation throughput. Run with:
//!
//! cargo run --release -p muesli-llm --example bench -- <model.gguf> [--prompt-tokens 20000] [--gen 256]

use std::num::NonZeroU32;
use std::time::Instant;

use anyhow::{Context, Result};
use llama_cpp_2::context::params::{KvCacheType, LlamaContextParams};
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{LlamaChatMessage, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::{LogOptions, send_logs_to_tracing};

const SYSTEM: &str = "You are a meeting assistant. Summarise the transcript into these Markdown sections, in order: ## TL;DR, ## Main points by speaker, ## Actions and takeaways. Be concrete and brief.";

fn synthetic_transcript(model: &LlamaModel, target_tokens: usize) -> String {
    // Deterministic pseudo-random meeting so the model cannot simply loop.
    let speakers = ["Alice", "Bob", "Chen", "Dana"];
    let subjects = [
        "the launch",
        "the payments integration",
        "the onboarding flow",
        "the pricing page",
        "the migration script",
        "the incident report",
        "hiring for the platform team",
        "the Q4 roadmap",
        "the customer webinar",
        "the data retention policy",
        "the mobile beta",
        "the vendor contract",
    ];
    let verbs = [
        "is blocked on",
        "should wait for",
        "needs sign-off from",
        "was delayed by",
        "depends on",
        "can be unblocked by",
        "is at risk because of",
        "was finished ahead of",
    ];
    let objects = [
        "finance",
        "the security review",
        "the design team",
        "legal",
        "the new API keys",
        "customer feedback",
        "the load test results",
        "the board update",
        "the contractor onboarding",
        "the analytics dashboard",
    ];
    let tails = [
        "so I will follow up tomorrow.",
        "and we agreed to revisit it on Friday.",
        "which means the date moves by a week.",
        "but nobody owns it yet.",
        "and Dana will write it up.",
        "so let's decide now.",
        "and that is a hard deadline.",
        "unless QA finds something.",
    ];
    let mut out = String::new();
    let mut t = 0u32;
    let mut i = 0usize;
    let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = |n: usize| -> usize {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        (rng % n as u64) as usize
    };
    loop {
        let mm = t / 60;
        let ss = t % 60;
        out.push_str(&format!(
            "[{mm:02}:{ss:02}] {}: I think {} {} {} {}\n",
            speakers[next(speakers.len())],
            subjects[next(subjects.len())],
            verbs[next(verbs.len())],
            objects[next(objects.len())],
            tails[next(tails.len())]
        ));
        t += 7 + next(9) as u32;
        i += 1;
        if i.is_multiple_of(50) {
            let n = model.vocab().tokenize(out.as_bytes(), false, true).len();
            if n >= target_tokens {
                break;
            }
        }
    }
    out
}

/// Gemma 4 canonical format (from the model's Jinja template): `<|turn>role\n…<turn|>\n`,
/// generation prompt `<|turn>model\n`, and an empty thought channel to disable thinking.
fn gemma4_prompt(user: &str) -> String {
    format!("<|turn>system\n{SYSTEM}<turn|>\n<|turn>user\n{user}<turn|>\n<|turn>model\n<|channel>thought\n<channel|>")
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let model_path = args.next().context("usage: bench <model.gguf> [--prompt-tokens N] [--gen N] [--n-ctx N]")?;
    let mut prompt_tokens = 20_000usize;
    let mut max_gen = 256i32;
    let mut n_ctx = 32_768u32;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--prompt-tokens" => prompt_tokens = args.next().context("value")?.parse()?,
            "--gen" => max_gen = args.next().context("value")?.parse()?,
            "--n-ctx" => n_ctx = args.next().context("value")?.parse()?,
            other => anyhow::bail!("unknown arg {other}"),
        }
    }

    send_logs_to_tracing(LogOptions::default().with_logs_enabled(false));
    let backend = LlamaBackend::init()?;

    let t0 = Instant::now();
    let model_params = LlamaModelParams::default().with_n_gpu_layers(1000);
    let model = LlamaModel::load_from_file(&backend, &model_path, &model_params)
        .with_context(|| format!("loading {model_path}"))?;
    let load_s = t0.elapsed().as_secs_f32();
    println!("model loaded in {load_s:.1}s; n_ctx_train={}", model.n_ctx_train());

    let ctx_params = LlamaContextParams::default()
        .with_n_ctx(NonZeroU32::new(n_ctx))
        .with_n_batch(2048)
        .with_n_ubatch(512)
        .with_type_k(KvCacheType::Q8_0)
        .with_type_v(KvCacheType::Q8_0)
        .with_flash_attention_policy(llama_cpp_sys_2::LLAMA_FLASH_ATTN_TYPE_ENABLED);
    let mut ctx = model.new_context(&backend, ctx_params)?;

    let transcript = synthetic_transcript(&model, prompt_tokens);
    let user = format!("Meeting transcript (90 minutes, 4 participants):\n\n{transcript}\n\nWrite the summary now.");
    let msgs =
        [LlamaChatMessage::new("system".into(), SYSTEM.into())?, LlamaChatMessage::new("user".into(), user.clone())?];
    // llama.cpp's built-in template matcher does not know Gemma 4's Jinja template
    // (apply_chat_template returns -1), so print the template for reference and
    // fall back to the Gemma turn format. The real engine renders the Jinja
    // template with minijinja instead.
    let mut prompt = match model.chat_template(None) {
        Ok(tmpl) => match model.apply_chat_template(&tmpl, &msgs, true) {
            Ok(p) => p,
            Err(e) => {
                let raw = String::from_utf8_lossy(tmpl.as_c_str().to_bytes()).to_string();
                eprintln!("apply_chat_template failed ({e}); template head:\n{}\n...", &raw[..raw.len().min(1200)]);
                gemma4_prompt(&user)
            }
        },
        Err(e) => {
            eprintln!("no chat template ({e}); using Gemma 4 turn format");
            gemma4_prompt(&user)
        }
    };
    prompt.push_str("## TL;DR\n");

    let tokens = model.vocab().tokenize(prompt.as_bytes(), false, true);
    println!("prompt tokens: {}", tokens.len());
    anyhow::ensure!((tokens.len() as i32 + max_gen) < n_ctx as i32, "prompt + max_gen exceeds n_ctx");

    // prompt processing in n_batch-sized chunks
    let n_batch = 2048usize;
    let mut batch = LlamaBatch::new(n_batch, 1);
    let t1 = Instant::now();
    let mut pos: i32 = 0;
    for chunk in tokens.chunks(n_batch) {
        batch.clear();
        let last = chunk.len() - 1;
        for (i, tok) in chunk.iter().enumerate() {
            let is_last_of_prompt = pos as usize == tokens.len() - 1;
            batch.add(*tok, pos, &[0], i == last && is_last_of_prompt)?;
            pos += 1;
        }
        ctx.decode(&mut batch).context("decode prompt chunk")?;
    }
    let pp_s = t1.elapsed().as_secs_f32();
    println!("prompt eval: {} tokens in {pp_s:.2}s = {:.0} tok/s", tokens.len(), tokens.len() as f32 / pp_s);

    // generation
    let n_vocab = model.n_vocab();
    let mut sampler = LlamaSampler::chain_simple([
        LlamaSampler::penalties(n_vocab, 64, 1.15, 0.0, 0.0),
        LlamaSampler::temp(0.3),
        LlamaSampler::dist(42),
    ]);
    let _ = n_vocab;
    let mut out = String::new();
    let t2 = Instant::now();
    let mut n_gen = 0;
    let mut n_cur = pos;
    loop {
        let token = sampler.sample(&ctx, batch.n_tokens() - 1);
        sampler.accept(token);
        if model.vocab().is_eog(token) || n_gen >= max_gen {
            break;
        }
        let piece = model.vocab().token_to_piece(token, true, None);
        out.push_str(&String::from_utf8_lossy(&piece));
        batch.clear();
        batch.add(token, n_cur, &[0], true)?;
        n_cur += 1;
        ctx.decode(&mut batch).context("decode max_gen")?;
        n_gen += 1;
    }
    let tg_s = t2.elapsed().as_secs_f32();
    println!("generation: {n_gen} tokens in {tg_s:.2}s = {:.1} tok/s", n_gen as f32 / tg_s);
    println!("total wall (excluding load): {:.1}s", (pp_s + tg_s));
    println!("---- output ----\n## TL;DR\n{out}\n----------------");
    Ok(())
}
