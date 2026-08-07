//! Local semantic meeting search.
//!
//! This module deliberately has no dependency on a concrete embedding model.
//! Production can provide an ONNX-backed [`EmbeddingProvider`], while tests use
//! [`HashEmbeddingProvider`] without downloading models or contacting a network.
//! Production never substitutes hash vectors for semantic embeddings: when the
//! verified MiniLM model is unavailable, indexing and search fall back to FTS.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use ndarray::Array2;
use once_cell::sync::OnceCell;
use ort::execution_providers::CPUExecutionProvider;
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::TensorRef;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::Write;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokenizers::{PaddingParams, Tokenizer, TruncationParams};

const INDEX_FORMAT_VERSION: &str = "semantic-search-v1";
const DEFAULT_RRF_K: f32 = 60.0;

/// The model is pinned to an immutable Hugging Face commit. Every file is
/// verified before ONNX Runtime or the tokenizer is allowed to parse it.
pub const MINILM_MODEL_DIRECTORY: &str =
    "semantic-search/all-MiniLM-L6-v2-751bff37182d3f1213fa05d7196b954e230abad9";
const MINILM_MODEL_ID: &str =
    "all-MiniLM-L6-v2-int8-751bff37182d3f1213fa05d7196b954e230abad9-mean-pool-v1";
const MINILM_MAX_TOKENS: usize = 256;

const MINILM_FILES: [(&str, u64, &str); 5] = [
    (
        "model_quantized.onnx",
        22_972_370,
        "afdb6f1a0e45b715d0bb9b11772f032c399babd23bfc31fed1c170afc848bdb1",
    ),
    (
        "tokenizer.json",
        711_661,
        "da0e79933b9ed51798a3ae27893d3c5fa4a201126cef75586296df9b4d2c62a0",
    ),
    (
        "config.json",
        650,
        "7135149f7cffa1a573466c6e4d8423ed73b62fd2332c575bf738a0d033f70df7",
    ),
    (
        "special_tokens_map.json",
        125,
        "b6d346be366a7d1d48332dbc9fdf3bf8960b5d879522b7799ddba59e76237ee3",
    ),
    (
        "tokenizer_config.json",
        366,
        "9261e7d79b44c8195c1cada2b453e55b00aeb81e907a6664974b4d7776172ab3",
    ),
];

static MINILM_PROVIDER: OnceCell<(PathBuf, Arc<MiniLmEmbeddingProvider>)> = OnceCell::new();

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Stable identifier that changes when the model or embedding format changes.
    fn model_id(&self) -> &str;

    /// Produce one embedding for every input, in the same order.
    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// A deterministic, local embedding provider intended for tests and development.
/// It hashes normalized tokens into a fixed-size signed bag-of-words vector.
#[derive(Debug, Clone)]
pub struct HashEmbeddingProvider {
    dimensions: usize,
    model_id: String,
}

impl HashEmbeddingProvider {
    pub fn new(dimensions: usize) -> Result<Self> {
        if dimensions == 0 {
            bail!("embedding dimensions must be greater than zero");
        }
        Ok(Self {
            dimensions,
            model_id: format!("hash-v1-{dimensions}"),
        })
    }
}

impl Default for HashEmbeddingProvider {
    fn default() -> Self {
        Self::new(128).expect("the default hash embedding dimensions are valid")
    }
}

#[async_trait]
impl EmbeddingProvider for HashEmbeddingProvider {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(inputs
            .iter()
            .map(|input| {
                let mut vector = vec![0.0_f32; self.dimensions];
                for token in lexical_tokens(input) {
                    let digest = Sha256::digest(token.as_bytes());
                    let bucket = u64::from_le_bytes(digest[0..8].try_into().unwrap()) as usize
                        % self.dimensions;
                    let sign = if digest[8] & 1 == 0 { 1.0 } else { -1.0 };
                    vector[bucket] += sign;
                }
                normalize(&mut vector);
                vector
            })
            .collect())
    }
}

/// Production, fully local sentence embeddings backed by the pinned quantized
/// all-MiniLM-L6-v2 ONNX model. No model download or network call occurs here.
pub struct MiniLmEmbeddingProvider {
    tokenizer: Tokenizer,
    session: Mutex<Session>,
}

impl MiniLmEmbeddingProvider {
    pub fn from_verified_directory(model_directory: &Path) -> Result<Self> {
        verify_minilm_model(model_directory)?;

        let mut tokenizer = Tokenizer::from_file(model_directory.join("tokenizer.json"))
            .map_err(|error| anyhow::anyhow!("loading MiniLM tokenizer: {error}"))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: MINILM_MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(|error| anyhow::anyhow!("configuring MiniLM truncation: {error}"))?;
        tokenizer.with_padding(Some(PaddingParams::default()));

        let session = Session::builder()
            .context("creating MiniLM ONNX session")?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .context("configuring MiniLM graph optimization")?
            .with_execution_providers(vec![CPUExecutionProvider::default().build()])
            .context("configuring MiniLM CPU execution")?
            .with_parallel_execution(true)
            .context("configuring MiniLM parallel execution")?
            .commit_from_file(model_directory.join("model_quantized.onnx"))
            .context("loading verified MiniLM ONNX model")?;

        Ok(Self {
            tokenizer,
            session: Mutex::new(session),
        })
    }
}

#[async_trait]
impl EmbeddingProvider for MiniLmEmbeddingProvider {
    fn model_id(&self) -> &str {
        MINILM_MODEL_ID
    }

    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }

        let encodings = self
            .tokenizer
            .encode_batch(inputs.iter().map(String::as_str).collect(), true)
            .map_err(|error| anyhow::anyhow!("tokenizing MiniLM input: {error}"))?;
        let sequence_length = encodings
            .first()
            .map(|encoding| encoding.get_ids().len())
            .context("MiniLM tokenizer returned no encodings")?;
        if sequence_length == 0
            || encodings
                .iter()
                .any(|encoding| encoding.get_ids().len() != sequence_length)
        {
            bail!("MiniLM tokenizer returned an invalid padded batch");
        }

        let batch_size = encodings.len();
        let input_ids = Array2::from_shape_vec(
            (batch_size, sequence_length),
            encodings
                .iter()
                .flat_map(|encoding| encoding.get_ids().iter().map(|value| i64::from(*value)))
                .collect(),
        )?;
        let attention_mask = Array2::from_shape_vec(
            (batch_size, sequence_length),
            encodings
                .iter()
                .flat_map(|encoding| {
                    encoding
                        .get_attention_mask()
                        .iter()
                        .map(|value| i64::from(*value))
                })
                .collect(),
        )?;
        let token_type_ids = Array2::from_shape_vec(
            (batch_size, sequence_length),
            encodings
                .iter()
                .flat_map(|encoding| {
                    encoding
                        .get_type_ids()
                        .iter()
                        .map(|value| i64::from(*value))
                })
                .collect(),
        )?;

        let mut session = self
            .session
            .lock()
            .map_err(|_| anyhow::anyhow!("MiniLM ONNX session lock was poisoned"))?;
        let outputs = session
            .run(ort::inputs![
                "input_ids" => TensorRef::from_array_view(input_ids.view())?,
                "attention_mask" => TensorRef::from_array_view(attention_mask.view())?,
                "token_type_ids" => TensorRef::from_array_view(token_type_ids.view())?,
            ])
            .context("running local MiniLM inference")?;
        let hidden = outputs
            .get("last_hidden_state")
            .context("MiniLM output did not contain last_hidden_state")?
            .try_extract_array::<f32>()
            .context("reading MiniLM last_hidden_state tensor")?;
        let shape = hidden.shape();
        if shape.len() != 3 || shape[0] != batch_size || shape[1] != sequence_length {
            bail!("MiniLM returned an unexpected output shape: {shape:?}");
        }

        let dimensions = shape[2];
        let mut embeddings = Vec::with_capacity(batch_size);
        for batch in 0..batch_size {
            let mut embedding = vec![0.0_f32; dimensions];
            let mut token_count = 0.0_f32;
            for token in 0..sequence_length {
                if attention_mask[[batch, token]] == 0 {
                    continue;
                }
                token_count += 1.0;
                for dimension in 0..dimensions {
                    embedding[dimension] += hidden[[batch, token, dimension]];
                }
            }
            if token_count == 0.0 {
                bail!("MiniLM attention mask contained no input tokens");
            }
            for value in &mut embedding {
                *value /= token_count;
            }
            normalize(&mut embedding);
            embeddings.push(embedding);
        }
        Ok(embeddings)
    }
}

#[derive(Clone)]
pub enum LocalEmbeddingProvider {
    MiniLm(Arc<MiniLmEmbeddingProvider>),
    KeywordOnly,
}

impl LocalEmbeddingProvider {
    /// Load and cache the verified production model. If it is absent or altered,
    /// return a keyword-only provider so FTS remains available without presenting
    /// deterministic token hashes as semantic similarity.
    pub fn verified_or_keyword(model_directory: &Path) -> (Self, Option<anyhow::Error>) {
        if let Some((cached_path, provider)) = MINILM_PROVIDER.get() {
            if cached_path == model_directory {
                return (Self::MiniLm(Arc::clone(provider)), None);
            }
        }

        match MiniLmEmbeddingProvider::from_verified_directory(model_directory) {
            Ok(provider) => {
                let provider = Arc::new(provider);
                let _ = MINILM_PROVIDER.set((model_directory.to_path_buf(), Arc::clone(&provider)));
                (Self::MiniLm(provider), None)
            }
            Err(error) => (Self::KeywordOnly, Some(error)),
        }
    }
}

#[async_trait]
impl EmbeddingProvider for LocalEmbeddingProvider {
    fn model_id(&self) -> &str {
        match self {
            Self::MiniLm(provider) => provider.model_id(),
            Self::KeywordOnly => "keyword-only-v1",
        }
    }

    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        match self {
            Self::MiniLm(provider) => provider.embed(inputs).await,
            Self::KeywordOnly => bail!("semantic embeddings are unavailable; use FTS"),
        }
    }
}

pub fn verify_minilm_model(model_directory: &Path) -> Result<()> {
    for (name, expected_size, expected_sha256) in MINILM_FILES {
        let path = model_directory.join(name);
        let metadata = path
            .metadata()
            .with_context(|| format!("reading MiniLM model metadata for {}", path.display()))?;
        if !metadata.is_file() || metadata.len() != expected_size {
            bail!(
                "MiniLM model file {} has an unexpected size (expected {}, got {})",
                path.display(),
                expected_size,
                metadata.len()
            );
        }

        let mut file = File::open(&path)
            .with_context(|| format!("opening MiniLM model file {}", path.display()))?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .with_context(|| format!("hashing MiniLM model file {}", path.display()))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        let actual_sha256 = format!("{:x}", hasher.finalize());
        if actual_sha256 != expected_sha256 {
            bail!(
                "MiniLM model file {} failed SHA-256 verification",
                path.display()
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptSourceSegment {
    pub text: String,
    pub audio_start_time: Option<f64>,
    pub audio_end_time: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SearchDocument {
    pub meeting_id: String,
    pub title: String,
    pub segments: Vec<TranscriptSourceSegment>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChunkingOptions {
    pub max_tokens: usize,
    pub overlap_tokens: usize,
}

impl Default for ChunkingOptions {
    fn default() -> Self {
        Self {
            max_tokens: 224,
            overlap_tokens: 32,
        }
    }
}

impl ChunkingOptions {
    fn validate(self) -> Result<Self> {
        if self.max_tokens == 0 {
            bail!("max_tokens must be greater than zero");
        }
        if self.overlap_tokens >= self.max_tokens {
            bail!("overlap_tokens must be smaller than max_tokens");
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SearchChunk {
    pub text: String,
    pub token_count: usize,
    pub audio_start_time: Option<f64>,
    pub audio_end_time: Option<f64>,
}

#[derive(Debug, Clone)]
struct TimedToken {
    text: String,
    sentence_end: bool,
    audio_start_time: Option<f64>,
    audio_end_time: Option<f64>,
}

/// Split transcript segments into token-bounded chunks. When possible, chunks
/// end at sentence punctuation in the latter half of the token window. Oversized
/// sentences are safely split at the hard token limit.
pub fn chunk_segments(
    segments: &[TranscriptSourceSegment],
    options: ChunkingOptions,
) -> Result<Vec<SearchChunk>> {
    let options = options.validate()?;
    let mut tokens = Vec::new();

    for segment in segments {
        let words: Vec<&str> = segment.text.split_whitespace().collect();
        let word_count = words.len();
        for (index, word) in words.into_iter().enumerate() {
            let (audio_start_time, audio_end_time) = interpolate_audio_time(
                segment.audio_start_time,
                segment.audio_end_time,
                index,
                word_count,
            );
            tokens.push(TimedToken {
                text: word.to_owned(),
                sentence_end: is_sentence_end(word),
                audio_start_time,
                audio_end_time,
            });
        }
    }

    let mut chunks = Vec::new();
    let mut start = 0;
    while start < tokens.len() {
        let hard_end = (start + options.max_tokens).min(tokens.len());
        let end = if hard_end == tokens.len() {
            hard_end
        } else {
            let preferred_start = start + options.max_tokens / 2;
            (preferred_start..hard_end)
                .rev()
                .find(|index| tokens[*index].sentence_end)
                .map(|index| index + 1)
                .unwrap_or(hard_end)
        };

        let window = &tokens[start..end];
        chunks.push(SearchChunk {
            text: window
                .iter()
                .map(|token| token.text.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            token_count: window.len(),
            audio_start_time: window.iter().find_map(|token| token.audio_start_time),
            audio_end_time: window.iter().rev().find_map(|token| token.audio_end_time),
        });

        if end == tokens.len() {
            break;
        }
        let next_start = end.saturating_sub(options.overlap_tokens);
        start = next_start.max(start + 1);
    }

    Ok(chunks)
}

fn interpolate_audio_time(
    segment_start: Option<f64>,
    segment_end: Option<f64>,
    index: usize,
    count: usize,
) -> (Option<f64>, Option<f64>) {
    match (segment_start, segment_end) {
        (Some(start), Some(end)) if count > 0 && end >= start => {
            let duration = (end - start) / count as f64;
            (
                Some(start + duration * index as f64),
                Some(start + duration * (index + 1) as f64),
            )
        }
        (start, end) => (start, end),
    }
}

fn is_sentence_end(token: &str) -> bool {
    token
        .trim_end_matches(['"', '\'', ')', ']', '}', '”', '’'])
        .ends_with(['.', '?', '!', '。', '？', '！'])
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SearchOptions {
    pub limit: usize,
    pub candidate_limit: usize,
    pub minimum_semantic_score: f32,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            limit: 20,
            candidate_limit: 100,
            minimum_semantic_score: 0.15,
        }
    }
}

impl SearchOptions {
    fn validate(self) -> Result<Self> {
        if self.limit == 0 {
            bail!("search result limit must be greater than zero");
        }
        if self.candidate_limit < self.limit {
            bail!("candidate_limit must be at least limit");
        }
        if !self.minimum_semantic_score.is_finite()
            || !(-1.0..=1.0).contains(&self.minimum_semantic_score)
        {
            bail!("minimum_semantic_score must be a finite value between -1 and 1");
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchMatchType {
    Keyword,
    Semantic,
    Hybrid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SemanticSearchResult {
    pub meeting_id: String,
    pub title: String,
    pub snippet: String,
    pub audio_timestamp: Option<f64>,
    pub score: f32,
    pub match_type: SearchMatchType,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReindexOutcome {
    Unchanged,
    Indexed { chunk_count: usize },
}

pub struct SemanticSearchService<'a, P: EmbeddingProvider> {
    pool: &'a SqlitePool,
    provider: &'a P,
    chunking: ChunkingOptions,
}

impl<'a, P: EmbeddingProvider> SemanticSearchService<'a, P> {
    pub fn new(pool: &'a SqlitePool, provider: &'a P, chunking: ChunkingOptions) -> Result<Self> {
        Ok(Self {
            pool,
            provider,
            chunking: chunking.validate()?,
        })
    }

    /// Replaces a meeting's index atomically. An unchanged source hash skips
    /// both embedding work and database writes.
    pub async fn reindex(&self, document: &SearchDocument) -> Result<ReindexOutcome> {
        if document.meeting_id.trim().is_empty() {
            bail!("meeting_id cannot be empty");
        }
        if document.title.trim().is_empty() {
            bail!("meeting title cannot be empty");
        }

        let source_hash = source_hash(document, self.chunking, self.provider.model_id());
        let existing_hash = sqlx::query_scalar::<_, String>(
            "SELECT source_hash FROM semantic_search_documents WHERE meeting_id = ?",
        )
        .bind(&document.meeting_id)
        .fetch_optional(self.pool)
        .await
        .context("checking the semantic-search source hash")?;

        if existing_hash.as_deref() == Some(source_hash.as_str()) {
            return Ok(ReindexOutcome::Unchanged);
        }

        let chunks = chunk_segments(&document.segments, self.chunking)?;
        let texts: Vec<String> = chunks.iter().map(|chunk| chunk.text.clone()).collect();
        let embeddings = if texts.is_empty() {
            Some(Vec::new())
        } else {
            match self.provider.embed(&texts).await {
                Ok(embeddings) => match validate_embeddings(&embeddings, chunks.len()) {
                    Ok(()) => Some(embeddings),
                    Err(error) => {
                        log::warn!(
                            "Semantic embedding output was invalid; indexing FTS only: {error}"
                        );
                        None
                    }
                },
                Err(error) => {
                    log::warn!(
                        "Semantic embeddings unavailable; indexing FTS only: {error}"
                    );
                    None
                }
            }
        };

        let mut transaction = self.pool.begin().await?;
        sqlx::query("DELETE FROM semantic_search_chunks WHERE meeting_id = ?")
            .bind(&document.meeting_id)
            .execute(&mut *transaction)
            .await?;

        for (index, chunk) in chunks.iter().enumerate() {
            let embedding = embeddings
                .as_ref()
                .and_then(|embeddings| embeddings.get(index));
            sqlx::query(
                "INSERT INTO semantic_search_chunks (
                    meeting_id, meeting_title, chunk_index, text,
                    audio_start_time, audio_end_time, embedding,
                    embedding_dimensions, source_hash
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&document.meeting_id)
            .bind(&document.title)
            .bind(index as i64)
            .bind(&chunk.text)
            .bind(chunk.audio_start_time)
            .bind(chunk.audio_end_time)
            .bind(embedding.map_or_else(Vec::new, |vector| encode_embedding(vector)))
            .bind(embedding.map_or(0_i64, |vector| vector.len() as i64))
            .bind(&source_hash)
            .execute(&mut *transaction)
            .await?;
        }

        sqlx::query(
            "INSERT INTO semantic_search_documents (
                meeting_id, source_hash, embedding_model, chunk_count, indexed_at
             ) VALUES (?, ?, ?, ?, CURRENT_TIMESTAMP)
             ON CONFLICT(meeting_id) DO UPDATE SET
                source_hash = excluded.source_hash,
                embedding_model = excluded.embedding_model,
                chunk_count = excluded.chunk_count,
                indexed_at = excluded.indexed_at",
        )
        .bind(&document.meeting_id)
        .bind(&source_hash)
        .bind(self.provider.model_id())
        .bind(chunks.len() as i64)
        .execute(&mut *transaction)
        .await?;

        transaction.commit().await?;
        Ok(ReindexOutcome::Indexed {
            chunk_count: chunks.len(),
        })
    }

    pub async fn delete_meeting(&self, meeting_id: &str) -> Result<()> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("DELETE FROM semantic_search_chunks WHERE meeting_id = ?")
            .bind(meeting_id)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("DELETE FROM semantic_search_documents WHERE meeting_id = ?")
            .bind(meeting_id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Hybrid local search. If query embedding fails, valid FTS results are
    /// still returned instead of making search unavailable.
    pub async fn search(
        &self,
        query: &str,
        options: SearchOptions,
    ) -> Result<Vec<SemanticSearchResult>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let options = options.validate()?;

        let keyword_candidates = self
            .keyword_candidates(query, options.candidate_limit)
            .await?;
        let semantic_candidates = match self.provider.embed(&[query.to_owned()]).await {
            Ok(mut vectors) if vectors.len() == 1 => {
                let query_vector = vectors.remove(0);
                if validate_vector(&query_vector).is_ok() {
                    self.semantic_candidates(
                        &query_vector,
                        options.candidate_limit,
                        options.minimum_semantic_score,
                    )
                    .await?
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        };

        Ok(fuse_candidates(
            keyword_candidates,
            semantic_candidates,
            options.limit,
        ))
    }

    async fn keyword_candidates(&self, query: &str, limit: usize) -> Result<Vec<Candidate>> {
        let Some(fts_query) = make_fts_query(query) else {
            return Ok(Vec::new());
        };
        let rows = sqlx::query(
            "SELECT c.id, c.meeting_id, c.meeting_title, c.text,
                    c.audio_start_time,
                    snippet(semantic_search_chunks_fts, 0, '', '', ' … ', 32) AS snippet
             FROM semantic_search_chunks_fts
             JOIN semantic_search_chunks c
               ON c.id = semantic_search_chunks_fts.rowid
             WHERE semantic_search_chunks_fts MATCH ?
             ORDER BY bm25(semantic_search_chunks_fts)
             LIMIT ?",
        )
        .bind(fts_query)
        .bind(limit as i64)
        .fetch_all(self.pool)
        .await
        .context("running local full-text search")?;

        rows.into_iter()
            .map(|row| {
                Ok(Candidate {
                    chunk_id: row.try_get("id")?,
                    meeting_id: row.try_get("meeting_id")?,
                    title: row.try_get("meeting_title")?,
                    snippet: row.try_get("snippet")?,
                    audio_timestamp: row.try_get("audio_start_time")?,
                    semantic_score: None,
                })
            })
            .collect()
    }

    async fn semantic_candidates(
        &self,
        query_vector: &[f32],
        limit: usize,
        minimum_score: f32,
    ) -> Result<Vec<Candidate>> {
        let rows = sqlx::query(
            "SELECT c.id, c.meeting_id, c.meeting_title, c.text,
                    c.audio_start_time, c.embedding, c.embedding_dimensions
             FROM semantic_search_chunks c
             JOIN semantic_search_documents d ON d.meeting_id = c.meeting_id
             WHERE d.embedding_model = ?",
        )
        .bind(self.provider.model_id())
        .fetch_all(self.pool)
        .await
        .context("loading local semantic-search vectors")?;

        let mut candidates = Vec::new();
        for row in rows {
            let dimensions: i64 = row.try_get("embedding_dimensions")?;
            if dimensions < 0 || dimensions as usize != query_vector.len() {
                continue;
            }
            let bytes: Vec<u8> = row.try_get("embedding")?;
            let Some(vector) = decode_embedding(&bytes, dimensions as usize) else {
                continue;
            };
            let score = cosine_similarity(query_vector, &vector).unwrap_or(0.0);
            if score < minimum_score {
                continue;
            }
            let text: String = row.try_get("text")?;
            candidates.push(Candidate {
                chunk_id: row.try_get("id")?,
                meeting_id: row.try_get("meeting_id")?,
                title: row.try_get("meeting_title")?,
                snippet: make_snippet(&text, 240),
                audio_timestamp: row.try_get("audio_start_time")?,
                semantic_score: Some(score),
            });
        }

        candidates.sort_by(|left, right| {
            right
                .semantic_score
                .partial_cmp(&left.semantic_score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left.chunk_id.cmp(&right.chunk_id))
        });
        candidates.truncate(limit);
        Ok(candidates)
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    chunk_id: i64,
    meeting_id: String,
    title: String,
    snippet: String,
    audio_timestamp: Option<f64>,
    semantic_score: Option<f32>,
}

#[derive(Debug)]
struct FusedCandidate {
    candidate: Candidate,
    score: f32,
    keyword: bool,
    semantic: bool,
}

fn fuse_candidates(
    keyword: Vec<Candidate>,
    semantic: Vec<Candidate>,
    limit: usize,
) -> Vec<SemanticSearchResult> {
    let mut fused: HashMap<i64, FusedCandidate> = HashMap::new();

    for (rank, candidate) in keyword.into_iter().enumerate() {
        let score = 1.0 / (DEFAULT_RRF_K + rank as f32 + 1.0);
        fused
            .entry(candidate.chunk_id)
            .and_modify(|entry| {
                entry.score += score;
                entry.keyword = true;
                entry.candidate.snippet = candidate.snippet.clone();
            })
            .or_insert(FusedCandidate {
                candidate,
                score,
                keyword: true,
                semantic: false,
            });
    }

    for (rank, candidate) in semantic.into_iter().enumerate() {
        let score = 1.0 / (DEFAULT_RRF_K + rank as f32 + 1.0);
        fused
            .entry(candidate.chunk_id)
            .and_modify(|entry| {
                entry.score += score;
                entry.semantic = true;
            })
            .or_insert(FusedCandidate {
                candidate,
                score,
                keyword: false,
                semantic: true,
            });
    }

    let mut results: Vec<_> = fused.into_values().collect();
    results.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.candidate.meeting_id.cmp(&right.candidate.meeting_id))
            .then_with(|| left.candidate.chunk_id.cmp(&right.candidate.chunk_id))
    });
    results.truncate(limit);

    results
        .into_iter()
        .map(|entry| SemanticSearchResult {
            meeting_id: entry.candidate.meeting_id,
            title: entry.candidate.title,
            snippet: entry.candidate.snippet,
            audio_timestamp: entry.candidate.audio_timestamp,
            score: entry.score,
            match_type: match (entry.keyword, entry.semantic) {
                (true, true) => SearchMatchType::Hybrid,
                (true, false) => SearchMatchType::Keyword,
                (false, true) => SearchMatchType::Semantic,
                (false, false) => unreachable!("a fused candidate has at least one rank"),
            },
        })
        .collect()
}

fn source_hash(document: &SearchDocument, chunking: ChunkingOptions, model_id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(INDEX_FORMAT_VERSION.as_bytes());
    hash.update([0]);
    hash.update(model_id.as_bytes());
    hash.update([0]);
    hash.update(chunking.max_tokens.to_le_bytes());
    hash.update(chunking.overlap_tokens.to_le_bytes());
    hash.update(document.title.as_bytes());
    hash.update([0]);
    for segment in &document.segments {
        hash.update(segment.text.as_bytes());
        hash.update([0]);
        hash.update(segment.audio_start_time.unwrap_or(f64::NAN).to_le_bytes());
        hash.update(segment.audio_end_time.unwrap_or(f64::NAN).to_le_bytes());
    }
    let mut encoded = String::with_capacity(64);
    for byte in hash.finalize() {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn validate_embeddings(embeddings: &[Vec<f32>], expected_count: usize) -> Result<()> {
    if embeddings.len() != expected_count {
        bail!(
            "embedding provider returned {} vectors for {expected_count} inputs",
            embeddings.len()
        );
    }
    let mut expected_dimensions = None;
    for embedding in embeddings {
        validate_vector(embedding)?;
        match expected_dimensions {
            Some(dimensions) if dimensions != embedding.len() => {
                bail!("embedding provider returned inconsistent vector dimensions")
            }
            None => expected_dimensions = Some(embedding.len()),
            _ => {}
        }
    }
    Ok(())
}

fn validate_vector(vector: &[f32]) -> Result<()> {
    if vector.is_empty() {
        bail!("embedding vectors cannot be empty");
    }
    if vector.iter().any(|value| !value.is_finite()) {
        bail!("embedding vectors must contain only finite values");
    }
    Ok(())
}

fn encode_embedding(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * std::mem::size_of::<f32>());
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn decode_embedding(bytes: &[u8], dimensions: usize) -> Option<Vec<f32>> {
    if bytes.len() != dimensions.checked_mul(std::mem::size_of::<f32>())? {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
            .collect(),
    )
}

pub fn cosine_similarity(left: &[f32], right: &[f32]) -> Option<f32> {
    if left.is_empty() || left.len() != right.len() {
        return None;
    }
    let dot: f32 = left.iter().zip(right).map(|(a, b)| a * b).sum();
    let left_norm: f32 = left.iter().map(|value| value * value).sum::<f32>().sqrt();
    let right_norm: f32 = right.iter().map(|value| value * value).sum::<f32>().sqrt();
    if left_norm == 0.0 || right_norm == 0.0 {
        return None;
    }
    Some((dot / (left_norm * right_norm)).clamp(-1.0, 1.0))
}

fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in vector {
            *value /= norm;
        }
    }
}

fn lexical_tokens(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| token.to_lowercase())
        .collect()
}

fn make_fts_query(query: &str) -> Option<String> {
    let terms = lexical_tokens(query);
    if terms.is_empty() {
        None
    } else {
        Some(
            terms
                .iter()
                .map(|term| format!("\"{term}\"*"))
                .collect::<Vec<_>>()
                .join(" OR "),
        )
    }
}

fn make_snippet(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let snippet: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{snippet}…")
    } else {
        snippet
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    const MIGRATION: &str = include_str!("../../migrations/20260729093000_add_semantic_search.sql");

    struct CountingProvider {
        calls: AtomicUsize,
        fail: bool,
    }

    #[async_trait]
    impl EmbeddingProvider for CountingProvider {
        fn model_id(&self) -> &str {
            "counting-test-v1"
        }

        async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            if self.fail {
                bail!("deliberate embedding failure");
            }
            Ok(inputs
                .iter()
                .map(|text| {
                    let lower = text.to_lowercase();
                    if lower.contains("budget") || lower.contains("finance") {
                        vec![1.0, 0.0, 0.0]
                    } else if lower.contains("design") || lower.contains("interface") {
                        vec![0.0, 1.0, 0.0]
                    } else {
                        vec![0.0, 0.0, 1.0]
                    }
                })
                .collect())
        }
    }

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE meetings (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(MIGRATION).execute(&pool).await.unwrap();
        pool
    }

    async fn insert_meeting(pool: &SqlitePool, id: &str, title: &str) {
        sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at)
             VALUES (?, ?, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
        )
        .bind(id)
        .bind(title)
        .execute(pool)
        .await
        .unwrap();
    }

    fn document(id: &str, title: &str, text: &str) -> SearchDocument {
        SearchDocument {
            meeting_id: id.to_owned(),
            title: title.to_owned(),
            segments: vec![TranscriptSourceSegment {
                text: text.to_owned(),
                audio_start_time: Some(10.0),
                audio_end_time: Some(20.0),
            }],
        }
    }

    #[test]
    fn chunking_prefers_sentence_boundaries_and_bounds_overlap() {
        let segments = vec![TranscriptSourceSegment {
            text: "one two three. four five six seven eight. nine ten eleven twelve".into(),
            audio_start_time: Some(0.0),
            audio_end_time: Some(12.0),
        }];
        let chunks = chunk_segments(
            &segments,
            ChunkingOptions {
                max_tokens: 8,
                overlap_tokens: 2,
            },
        )
        .unwrap();

        assert_eq!(chunks[0].text, "one two three. four five six seven eight.");
        assert!(chunks.iter().all(|chunk| chunk.token_count <= 8));
        assert_eq!(chunks[1].text.split_whitespace().next(), Some("seven"));
        assert_eq!(chunks[0].audio_start_time, Some(0.0));
        assert!(chunks[0].audio_end_time.unwrap() <= 8.0);
    }

    #[test]
    fn chunking_splits_an_oversized_sentence_at_the_hard_limit() {
        let segments = vec![TranscriptSourceSegment {
            text: "one two three four five six seven eight nine ten".into(),
            audio_start_time: None,
            audio_end_time: None,
        }];
        let chunks = chunk_segments(
            &segments,
            ChunkingOptions {
                max_tokens: 4,
                overlap_tokens: 1,
            },
        )
        .unwrap();
        assert_eq!(chunks[0].token_count, 4);
        assert_eq!(chunks[1].text, "four five six seven");
    }

    #[tokio::test]
    async fn hash_embeddings_are_deterministic_and_normalized() {
        let provider = HashEmbeddingProvider::new(32).unwrap();
        let inputs = vec!["Local private meeting search".to_owned()];
        let first = provider.embed(&inputs).await.unwrap();
        let second = provider.embed(&inputs).await.unwrap();
        assert_eq!(first, second);
        assert!((cosine_similarity(&first[0], &first[0]).unwrap() - 1.0).abs() < 1e-5);
    }

    #[tokio::test]
    async fn verified_minilm_model_produces_semantic_embeddings_when_configured() {
        let Ok(model_directory) = std::env::var("MEETILY_MINILM_MODEL_DIR") else {
            return;
        };
        let provider =
            MiniLmEmbeddingProvider::from_verified_directory(Path::new(&model_directory))
                .expect("the configured MiniLM model must pass integrity checks and load");
        let embeddings = provider
            .embed(&[
                "annual budget and financial forecast".to_owned(),
                "yearly finance planning".to_owned(),
                "chocolate cake recipe".to_owned(),
            ])
            .await
            .expect("MiniLM inference must succeed");

        assert_eq!(embeddings.len(), 3);
        assert_eq!(embeddings[0].len(), 384);
        assert!(embeddings.iter().all(|embedding| {
            (cosine_similarity(embedding, embedding).unwrap() - 1.0).abs() < 1e-4
        }));
        assert!(
            cosine_similarity(&embeddings[0], &embeddings[1])
                > cosine_similarity(&embeddings[0], &embeddings[2])
        );
    }

    #[test]
    fn cosine_rejects_zero_or_mismatched_vectors() {
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[1.0]), None);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[0.0, 0.0]), None);
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]), Some(0.0));
    }

    #[tokio::test]
    async fn unchanged_source_hash_skips_reembedding() {
        let pool = test_pool().await;
        insert_meeting(&pool, "meeting-1", "Budget").await;
        let provider = CountingProvider {
            calls: AtomicUsize::new(0),
            fail: false,
        };
        let service =
            SemanticSearchService::new(&pool, &provider, ChunkingOptions::default()).unwrap();
        let document = document("meeting-1", "Budget", "The annual budget is approved.");

        assert_eq!(
            service.reindex(&document).await.unwrap(),
            ReindexOutcome::Indexed { chunk_count: 1 }
        );
        assert_eq!(
            service.reindex(&document).await.unwrap(),
            ReindexOutcome::Unchanged
        );
        assert_eq!(provider.calls.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    async fn changed_document_atomically_replaces_old_fts_content() {
        let pool = test_pool().await;
        insert_meeting(&pool, "meeting-1", "Planning").await;
        let provider = HashEmbeddingProvider::default();
        let service =
            SemanticSearchService::new(&pool, &provider, ChunkingOptions::default()).unwrap();

        service
            .reindex(&document(
                "meeting-1",
                "Planning",
                "obsolete roadmap wording",
            ))
            .await
            .unwrap();
        service
            .reindex(&document(
                "meeting-1",
                "Planning",
                "current launch schedule",
            ))
            .await
            .unwrap();

        let old = service
            .search("obsolete", SearchOptions::default())
            .await
            .unwrap();
        let current = service
            .search("launch", SearchOptions::default())
            .await
            .unwrap();
        assert!(old.is_empty());
        assert_eq!(current[0].meeting_id, "meeting-1");
    }

    #[tokio::test]
    async fn search_fuses_keyword_and_semantic_ranks() {
        let pool = test_pool().await;
        insert_meeting(&pool, "meeting-1", "Finance review").await;
        insert_meeting(&pool, "meeting-2", "Product review").await;
        let provider = CountingProvider {
            calls: AtomicUsize::new(0),
            fail: false,
        };
        let service =
            SemanticSearchService::new(&pool, &provider, ChunkingOptions::default()).unwrap();
        service
            .reindex(&document(
                "meeting-1",
                "Finance review",
                "The budget forecast was accepted.",
            ))
            .await
            .unwrap();
        service
            .reindex(&document(
                "meeting-2",
                "Product review",
                "The interface design needs another pass.",
            ))
            .await
            .unwrap();

        let results = service
            .search("budget", SearchOptions::default())
            .await
            .unwrap();
        assert_eq!(results[0].meeting_id, "meeting-1");
        assert_eq!(results[0].match_type, SearchMatchType::Hybrid);
        assert_eq!(results[0].audio_timestamp, Some(10.0));
    }

    #[tokio::test]
    async fn embedding_failure_falls_back_to_fts() {
        let pool = test_pool().await;
        insert_meeting(&pool, "meeting-1", "Launch").await;
        let good_provider = CountingProvider {
            calls: AtomicUsize::new(0),
            fail: false,
        };
        let good_service =
            SemanticSearchService::new(&pool, &good_provider, ChunkingOptions::default()).unwrap();
        good_service
            .reindex(&document(
                "meeting-1",
                "Launch",
                "The launch checklist is ready.",
            ))
            .await
            .unwrap();

        let failing_provider = CountingProvider {
            calls: AtomicUsize::new(0),
            fail: true,
        };
        let service =
            SemanticSearchService::new(&pool, &failing_provider, ChunkingOptions::default())
                .unwrap();
        let results = service
            .search("launch", SearchOptions::default())
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].match_type, SearchMatchType::Keyword);
    }

    #[tokio::test]
    async fn embedding_failure_still_builds_a_searchable_fts_index() {
        let pool = test_pool().await;
        insert_meeting(&pool, "meeting-1", "Launch").await;
        let provider = CountingProvider {
            calls: AtomicUsize::new(0),
            fail: true,
        };
        let service =
            SemanticSearchService::new(&pool, &provider, ChunkingOptions::default()).unwrap();

        let outcome = service
            .reindex(&document(
                "meeting-1",
                "Launch",
                "The launch checklist is ready.",
            ))
            .await
            .unwrap();
        assert_eq!(outcome, ReindexOutcome::Indexed { chunk_count: 1 });

        let results = service
            .search("launch", SearchOptions::default())
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].match_type, SearchMatchType::Keyword);
        let dimensions: i64 = sqlx::query_scalar(
            "SELECT embedding_dimensions FROM semantic_search_chunks WHERE meeting_id = ?",
        )
        .bind("meeting-1")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(dimensions, 0);
    }

    #[tokio::test]
    async fn deleting_a_meeting_removes_vector_and_fts_results() {
        let pool = test_pool().await;
        insert_meeting(&pool, "meeting-1", "Confidential").await;
        let provider = HashEmbeddingProvider::default();
        let service =
            SemanticSearchService::new(&pool, &provider, ChunkingOptions::default()).unwrap();
        service
            .reindex(&document(
                "meeting-1",
                "Confidential",
                "unique deletion sentinel",
            ))
            .await
            .unwrap();

        service.delete_meeting("meeting-1").await.unwrap();
        assert!(service
            .search("sentinel", SearchOptions::default())
            .await
            .unwrap()
            .is_empty());
        let document_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM semantic_search_documents WHERE meeting_id = ?",
        )
        .bind("meeting-1")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(document_count, 0);
    }

    #[test]
    fn result_contract_serializes_with_frontend_friendly_fields() {
        let result = SemanticSearchResult {
            meeting_id: "meeting-1".into(),
            title: "Review".into(),
            snippet: "A matching passage".into(),
            audio_timestamp: Some(42.5),
            score: 0.25,
            match_type: SearchMatchType::Hybrid,
        };
        let json = serde_json::to_value(result).unwrap();
        assert_eq!(json["meetingId"], "meeting-1");
        assert_eq!(json["audioTimestamp"], 42.5);
        assert_eq!(json["matchType"], "hybrid");
    }

    #[test]
    fn rejects_invalid_limits_and_overlap() {
        assert!(chunk_segments(
            &[],
            ChunkingOptions {
                max_tokens: 4,
                overlap_tokens: 4,
            }
        )
        .is_err());
        assert!(SearchOptions {
            limit: 10,
            candidate_limit: 5,
            minimum_semantic_score: 0.0,
        }
        .validate()
        .is_err());
    }
}
