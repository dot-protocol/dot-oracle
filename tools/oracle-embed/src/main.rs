//! oracle-embed: Oracle's meaning-search model, held loaded in one process, behind the `/api/embed` Ollama speaks.
//!
//! Why (2026-10-10, measured on core): through Ollama a five-word query took 0.6-5.5 s, of which ~0.5-1 s was the
//! model being set up again on every request; the model itself (snowflake-arctic-embed-l-v2.0, XLM-RoBERTa, 568M)
//! answers the same query in ~175 ms on 8 threads of this CPU. The vectors are the same model's (cosine 0.99999 with
//! Ollama's), so the existing index stays valid and Oracle only changes `ORACLE_EMBED_URL`.
//!
//! `oracle-embed --model <dir with config.json, tokenizer.json, model.safetensors> [--bind 127.0.0.1:11436]`.
//! Pooling is the first token (the model's `1_Pooling`: cls), then unit length. Threads: `RAYON_NUM_THREADS`.
use anyhow::{Context, Result};
use axum::{extract::State, http::StatusCode, routing::{get, post}, Json, Router};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::xlm_roberta::{Config, XLMRobertaModel};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tokenizers::Tokenizer;

/// The longest input in tokens (the model's positions: 8194, two reserved).
const MAX_TOKENS: usize = 8192;

struct Embedder {
    model: XLMRobertaModel,
    tokenizer: Tokenizer,
    name: String,
}

/// Inputs of at most this many tokens are embedded together in one padded pass; a longer one goes alone (padding
/// short queries out to a long document's length would cost more than it saves).
const BATCH_TOKENS: usize = 512;
/// The most inputs one pass takes.
const BATCH_MAX: usize = 16;

/// One request waiting for the model: its texts and where the vectors go.
type Job = (Vec<String>, tokio::sync::oneshot::Sender<std::result::Result<Vec<Vec<f32>>, String>>);

impl Embedder {
    fn load(dir: &std::path::Path) -> Result<Embedder> {
        let cfg: Config = serde_json::from_slice(&std::fs::read(dir.join("config.json"))?).context("config.json")?;
        let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|e| anyhow::anyhow!("tokenizer.json: {e}"))?;
        // Safety: the weights file is read-only and not changed while the process runs.
        let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[dir.join("model.safetensors")], DType::F32, &Device::Cpu)? };
        let model = XLMRobertaModel::new(&cfg, vb)?;
        let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(Embedder { model, tokenizer, name })
    }

    /// A text's token ids, cut at MAX_TOKENS (keeping the closing token).
    fn ids(&self, text: &str) -> Result<Vec<u32>> {
        let enc = self.tokenizer.encode(text, true).map_err(|e| anyhow::anyhow!("tokenize: {e}"))?;
        let mut ids: Vec<u32> = enc.get_ids().to_vec();
        if ids.len() > MAX_TOKENS {
            let last = *ids.last().unwrap_or(&2);
            ids.truncate(MAX_TOKENS - 1);
            ids.push(last);
        }
        Ok(ids)
    }

    /// One text to one unit vector.
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        Ok(self.embed_ids(&[self.ids(text)?])?.remove(0))
    }

    /// Several token lists in one padded pass: pad id 1 and mask 0 past each list's end; each row's first token's
    /// state, normalised. Padding is masked, so a row's vector is the same as when embedded alone.
    fn embed_ids(&self, rows: &[Vec<u32>]) -> Result<Vec<Vec<f32>>> {
        let len = rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
        let mut ids = Vec::with_capacity(rows.len() * len);
        let mut mask = Vec::with_capacity(rows.len() * len);
        for r in rows {
            ids.extend(r.iter().copied().chain(std::iter::repeat_n(1, len - r.len())));
            mask.extend(std::iter::repeat_n(1u8, r.len()).chain(std::iter::repeat_n(0, len - r.len())));
        }
        let b = rows.len();
        let input = Tensor::from_vec(ids, (b, len), &Device::Cpu)?;
        let mask = Tensor::from_vec(mask, (b, len), &Device::Cpu)?;
        let types = Tensor::zeros((b, len), DType::U32, &Device::Cpu)?;
        let out = self.model.forward(&input, &mask, &types, None, None, None)?;
        (0..b)
            .map(|i| {
                let cls: Vec<f32> = out.get(i)?.get(0)?.to_vec1()?;
                let norm = cls.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
                Ok(cls.into_iter().map(|x| x / norm).collect())
            })
            .collect()
    }
}

/// The model's one worker: takes every request waiting, embeds their short texts together in passes of up to
/// BATCH_MAX and each long one alone, and answers each request (2026-10-10: Grok searched in bursts of three a second
/// and requests served one at a time queued past Oracle's 2.5 s budget; 34 of 96 searches fell back to keyword).
fn worker(e: Arc<Embedder>, rx: std::sync::mpsc::Receiver<Job>) {
    while let Ok(first) = rx.recv() {
        let mut jobs = vec![first];
        while let Ok(more) = rx.try_recv() {
            jobs.push(more);
        }
        // Every text of every job, tokenised, with where its vector goes.
        let mut short: Vec<(usize, usize, Vec<u32>)> = vec![];
        let mut out: Vec<Vec<Option<Vec<f32>>>> = jobs.iter().map(|(t, _)| vec![None; t.len()]).collect();
        let mut failed: Vec<Option<String>> = vec![None; jobs.len()];
        for (j, (texts, _)) in jobs.iter().enumerate() {
            for (k, t) in texts.iter().enumerate() {
                match e.ids(t) {
                    Ok(ids) if ids.len() <= BATCH_TOKENS => short.push((j, k, ids)),
                    Ok(ids) => match e.embed_ids(&[ids]) {
                        Ok(mut v) => out[j][k] = Some(v.remove(0)),
                        Err(err) => failed[j] = Some(err.to_string()),
                    },
                    Err(err) => failed[j] = Some(err.to_string()),
                }
            }
        }
        for chunk in short.chunks(BATCH_MAX) {
            let rows: Vec<Vec<u32>> = chunk.iter().map(|(_, _, ids)| ids.clone()).collect();
            match e.embed_ids(&rows) {
                Ok(vs) => {
                    for ((j, k, _), v) in chunk.iter().zip(vs) {
                        out[*j][*k] = Some(v);
                    }
                }
                Err(err) => {
                    for (j, _, _) in chunk {
                        failed[*j] = Some(err.to_string());
                    }
                }
            }
        }
        for (j, (_, reply)) in jobs.into_iter().enumerate() {
            let r = match failed[j].take() {
                Some(err) => Err(err),
                None => Ok(out[j].drain(..).map(|v| v.unwrap_or_default()).collect()),
            };
            let _ = reply.send(r);
        }
    }
}

struct Served {
    name: String,
    jobs: std::sync::Mutex<std::sync::mpsc::Sender<Job>>,
}

#[derive(Deserialize)]
struct EmbedReq {
    #[serde(default)]
    model: String,
    input: Value,
}

async fn embed(State(e): State<Arc<Served>>, Json(req): Json<EmbedReq>) -> Result<Json<Value>, (StatusCode, String)> {
    let texts: Vec<String> = match req.input {
        Value::String(s) => vec![s],
        Value::Array(a) => a.into_iter().map(|v| v.as_str().map(str::to_owned).ok_or((StatusCode::BAD_REQUEST, "input must be strings".into()))).collect::<Result<_, _>>()?,
        _ => return Err((StatusCode::BAD_REQUEST, "input must be a string or a list of strings".into())),
    };
    let started = std::time::Instant::now();
    let (tx, rx) = tokio::sync::oneshot::channel();
    e.jobs.lock().map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "worker lock".to_string()))?
        .send((texts, tx)).map_err(|_| (StatusCode::SERVICE_UNAVAILABLE, "the model worker stopped".to_string()))?;
    let vecs = rx.await.map_err(|_| (StatusCode::SERVICE_UNAVAILABLE, "the model worker stopped".to_string()))?
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err))?;
    let model = if req.model.is_empty() { e.name.clone() } else { req.model };
    Ok(Json(json!({"model": model, "embeddings": vecs, "total_duration": started.elapsed().as_nanos() as u64, "load_duration": 0})))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let dir = arg("--model").context("--model <dir> is required")?;
    let bind = arg("--bind").unwrap_or_else(|| "127.0.0.1:11436".into());
    let t = std::time::Instant::now();
    let e = Arc::new(Embedder::load(std::path::Path::new(&dir))?);
    e.embed("warm")?;
    eprintln!("oracle-embed {} loaded in {:.1}s, listening on {bind}", e.name, t.elapsed().as_secs_f32());
    let (tx, rx) = std::sync::mpsc::channel::<Job>();
    let served = Arc::new(Served { name: e.name.clone(), jobs: std::sync::Mutex::new(tx) });
    std::thread::spawn(move || worker(e, rx));
    let app = Router::new()
        .route("/api/embed", post(embed))
        .route("/health", get(|| async { "ok" }))
        .with_state(served);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).with_graceful_shutdown(async { let _ = tokio::signal::ctrl_c().await; }).await?;
    Ok(())
}
