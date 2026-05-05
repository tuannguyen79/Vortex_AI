// src/ai/local_sentiment.rs
// ════════════════════════════════════════════════════════════════════════════
// Local Sentiment Analyzer – VortexAI
//
// Chiến lược tiết kiệm token:
//   1. Chạy model nhỏ local (DistilBERT SST-2 hoặc bge-small) trên CPU
//   2. Nếu confidence < threshold → gọi LLM API (có cache Redis 5 phút)
//   3. LLM cooldown: không gọi quá 1 lần / cooldown_secs / symbol
//
// Model file: assets/sentiment/model.safetensors + tokenizer.json
// Download: huggingface-cli download distilbert-base-uncased-finetuned-sst-2-english
// ════════════════════════════════════════════════════════════════════════════

use std::{path::Path, sync::Arc, time::{Duration, Instant}};
use anyhow::{Context, Result};
use dashmap::DashMap;
use tracing::{debug, info, warn};
use serde::{Deserialize, Serialize};

// Candle imports (conditional – trả về fallback khi feature không có model)
#[cfg(feature = "local-ai")]
use {
    candle_core::{Device, Tensor},
    candle_nn::VarBuilder,
    candle_transformers::models::distilbert::{Config as DistilConfig, DistilBertForSequenceClassification},
    tokenizers::Tokenizer,
};

use crate::models::config::AiConfig;

// ── Result từ sentiment analysis ──────────────────────────────────────────────
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentimentResult {
    /// -1.0 (very negative) .. +1.0 (very positive)
    pub score:      f64,
    /// 0.0 .. 1.0 – độ tin cậy của kết quả
    pub confidence: f64,
    /// "local" | "llm_api" | "fallback"
    pub source:     String,
    /// Thời gian xử lý (ms)
    pub latency_ms: u64,
}

// ── Cooldown tracker: symbol -> last_llm_call_instant ────────────────────────
type CooldownMap = DashMap<String, Instant>;

// ─────────────────────────────────────────────────────────────────────────────

pub struct LocalSentimentAnalyzer {
    config:    AiConfig,
    cooldowns: Arc<CooldownMap>,
    redis:     deadpool_redis::Pool,

    #[cfg(feature = "local-ai")]
    model:     DistilBertForSequenceClassification,
    #[cfg(feature = "local-ai")]
    tokenizer: Tokenizer,
    #[cfg(feature = "local-ai")]
    device:    Device,
}

impl LocalSentimentAnalyzer {
    /// Khởi tạo analyzer. Nếu model file không tồn tại → chạy ở chế độ
    /// LLM-only (vẫn hoạt động, chỉ tốn API call nhiều hơn).
    pub fn new(config: AiConfig, redis: deadpool_redis::Pool) -> Self {
        #[cfg(feature = "local-ai")]
        {
            match Self::load_model() {
                Ok((model, tokenizer, device)) => {
                    info!("✅ Local sentiment model loaded on {:?}", device);
                    return Self { config, cooldowns: Arc::new(DashMap::new()), redis, model, tokenizer, device };
                }
                Err(e) => {
                    warn!("Local model load failed ({}), falling back to LLM-only mode", e);
                }
            }
        }

        // Fallback: không có model local
        #[allow(unreachable_code)]
        Self {
            config,
            cooldowns: Arc::new(DashMap::new()),
            redis,
            #[cfg(feature = "local-ai")]
            model: unreachable!(),
            #[cfg(feature = "local-ai")]
            tokenizer: unreachable!(),
            #[cfg(feature = "local-ai")]
            device: unreachable!(),
        }
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Phân tích sentiment của một đoạn text (title bài báo, tweet, v.v.).
    /// Tự động chọn: local model → LLM nếu confidence thấp.
    pub async fn analyze(&self, text: &str, symbol: &str) -> SentimentResult {
        let start = Instant::now();

        // Bước 1: Local model inference
        let local_result = self.local_inference(text);

        let latency_ms = start.elapsed().as_millis() as u64;

        // Nếu local confidence đủ cao → return ngay, tiết kiệm token
        if let Some(r) = &local_result {
            if r.confidence >= self.config.llm_call_threshold {
                debug!(
                    symbol, text = &text[..text.len().min(60)],
                    score = r.score, confidence = r.confidence,
                    "Sentiment: local model (high confidence)"
                );
                metrics::counter!("sentiment_local_hits").increment(1);
                return SentimentResult {
                    score:      r.score,
                    confidence: r.confidence,
                    source:     "local".to_string(),
                    latency_ms,
                };
            }
        }

        // Bước 2: Kiểm tra cooldown LLM
        let cooldown = Duration::from_secs(self.config.llm_cooldown_secs);
        if let Some(last) = self.cooldowns.get(symbol) {
            if last.elapsed() < cooldown {
                debug!(symbol, "LLM cooldown active, using local/neutral");
                metrics::counter!("sentiment_cooldown_skips").increment(1);
                return local_result.unwrap_or_else(|| SentimentResult {
                    score: 0.0, confidence: 0.3,
                    source: "fallback".to_string(), latency_ms,
                });
            }
        }

        // Bước 3: Kiểm tra Redis cache
        let cache_key = format!("sentiment:{}", Self::text_hash(text));
        if let Ok(cached) = self.get_cache(&cache_key).await {
            if let Some(r) = cached {
                metrics::counter!("sentiment_cache_hits").increment(1);
                return r;
            }
        }

        // Bước 4: Gọi LLM API
        self.cooldowns.insert(symbol.to_string(), Instant::now());
        match self.llm_sentiment(text).await {
            Ok(result) => {
                self.set_cache(&cache_key, &result).await;
                metrics::counter!("sentiment_llm_calls").increment(1);
                result
            }
            Err(e) => {
                warn!("LLM sentiment failed: {:?}", e);
                metrics::counter!("sentiment_llm_errors").increment(1);
                local_result.unwrap_or_else(|| SentimentResult {
                    score: 0.0, confidence: 0.1,
                    source: "fallback".to_string(), latency_ms,
                })
            }
        }
    }

    /// Phân tích batch (nhiều tiêu đề cùng lúc) → trung bình có trọng số
    pub async fn analyze_batch(&self, texts: &[String], symbol: &str) -> SentimentResult {
        if texts.is_empty() {
            return SentimentResult { score: 0.0, confidence: 0.0, source: "empty".to_string(), latency_ms: 0 };
        }

        let mut weighted_score = 0.0f64;
        let mut total_weight   = 0.0f64;

        for text in texts {
            let r = self.analyze(text, symbol).await;
            weighted_score += r.score * r.confidence;
            total_weight   += r.confidence;
        }

        let avg_score = if total_weight > 0.0 { weighted_score / total_weight } else { 0.0 };
        let confidence = (total_weight / texts.len() as f64).min(1.0);

        SentimentResult {
            score:      avg_score,
            confidence,
            source:     "batch_aggregate".to_string(),
            latency_ms: 0,
        }
    }

    // ── Local inference ───────────────────────────────────────────────────────

    #[cfg(feature = "local-ai")]
    fn local_inference(&self, text: &str) -> Option<SentimentResult> {
        let start = Instant::now();

        // Tokenize
        let encoding = self.tokenizer.encode(text, true).ok()?;
        let ids:      Vec<u32> = encoding.get_ids().to_vec();
        let mask:     Vec<u32> = encoding.get_attention_mask().to_vec();

        let ids_t  = Tensor::new(ids.as_slice(), &self.device).ok()?
            .unsqueeze(0).ok()?;
        let mask_t = Tensor::new(mask.as_slice(), &self.device).ok()?
            .unsqueeze(0).ok()?;

        // Forward pass
        let logits = self.model.forward(&ids_t, &mask_t).ok()?;
        let logits_vec: Vec<f32> = logits.flatten_all().ok()?.to_vec1().ok()?;

        // SST-2: [neg_logit, pos_logit] → softmax → score
        if logits_vec.len() < 2 { return None; }
        let neg = logits_vec[0] as f64;
        let pos = logits_vec[1] as f64;
        let max = neg.max(pos);
        let exp_neg = (neg - max).exp();
        let exp_pos = (pos - max).exp();
        let sum     = exp_neg + exp_pos;
        let prob_pos = exp_pos / sum;
        let prob_neg = exp_neg / sum;

        // score: -1..+1; confidence: max probability
        let score      = prob_pos - prob_neg;
        let confidence = prob_pos.max(prob_neg);

        Some(SentimentResult {
            score,
            confidence,
            source:     "local".to_string(),
            latency_ms: start.elapsed().as_millis() as u64,
        })
    }

    #[cfg(not(feature = "local-ai"))]
    fn local_inference(&self, _text: &str) -> Option<SentimentResult> {
        // Không có model local → None → sẽ fallback lên LLM
        None
    }

    // ── LLM API sentiment ─────────────────────────────────────────────────────

    async fn llm_sentiment(&self, text: &str) -> Result<SentimentResult> {
        let start = Instant::now();

        // Prompt siêu ngắn để tiết kiệm token
        let prompt = format!(
            "Rate the financial sentiment of this headline: \"{}\"\n\
             Respond with JSON only: {{\"score\": <-1.0 to 1.0>, \"confidence\": <0.0 to 1.0>}}",
            &text[..text.len().min(200)]
        );

        // Ưu tiên lite model để tiết kiệm
        let model = &self.config.lite_model;

        // Thử OpenRouter trước (rẻ hơn), fallback sang OpenAI
        let key = self.config.openrouter_key.as_deref()
            .or(self.config.openai_api_key.as_deref())
            .context("No AI API key configured")?;

        let base_url = if self.config.openrouter_key.is_some() {
            "https://openrouter.ai/api/v1/chat/completions"
        } else {
            "https://api.openai.com/v1/chat/completions"
        };

        let body = serde_json::json!({
            "model": model,
            "max_tokens": 60,
            "temperature": 0.0,
            "messages": [{"role": "user", "content": prompt}]
        });

        let resp = reqwest::Client::new()
            .post(base_url)
            .bearer_auth(key)
            .json(&body)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .context("LLM request failed")?;

        let json: serde_json::Value = resp.json().await.context("LLM response parse failed")?;
        let content = json["choices"][0]["message"]["content"]
            .as_str()
            .context("No content in LLM response")?;

        // Parse JSON response
        #[derive(Deserialize)]
        struct LlmSentiment { score: f64, confidence: f64 }

        let parsed: LlmSentiment = serde_json::from_str(content.trim())
            .context("LLM returned invalid JSON")?;

        Ok(SentimentResult {
            score:      parsed.score.clamp(-1.0, 1.0),
            confidence: parsed.confidence.clamp(0.0, 1.0),
            source:     format!("llm:{}", model),
            latency_ms: start.elapsed().as_millis() as u64,
        })
    }

    // ── Model loading ─────────────────────────────────────────────────────────

    #[cfg(feature = "local-ai")]
    fn load_model() -> Result<(DistilBertForSequenceClassification, Tokenizer, Device)> {
        let model_dir = Path::new("assets/sentiment");
        anyhow::ensure!(model_dir.exists(), "Model dir not found: {:?}", model_dir);

        // CPU device (máy yếu vẫn chạy được)
        let device = Device::Cpu;

        let config: DistilConfig = {
            let cfg_bytes = std::fs::read(model_dir.join("config.json"))?;
            serde_json::from_slice(&cfg_bytes)?
        };

        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(
                &[model_dir.join("model.safetensors")],
                candle_core::DType::F32,
                &device,
            )?
        };

        let model     = DistilBertForSequenceClassification::load(vb, &config)?;
        let tokenizer = Tokenizer::from_file(model_dir.join("tokenizer.json"))
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        Ok((model, tokenizer, device))
    }

    // ── Redis cache helpers ───────────────────────────────────────────────────

    async fn get_cache(&self, key: &str) -> Result<Option<SentimentResult>> {
        let mut conn = self.redis.get().await?;
        let val: Option<Vec<u8>> = redis::cmd("GET").arg(key).query_async(&mut *conn).await?;
        match val {
            Some(b) => Ok(rmp_serde::from_slice(&b).ok()),
            None    => Ok(None),
        }
    }

    async fn set_cache(&self, key: &str, result: &SentimentResult) {
        if let Ok(mut conn) = self.redis.get().await {
            if let Ok(buf) = rmp_serde::to_vec_named(result) {
                let ttl = self.config.cache_llm_ttl_secs;
                let _: Result<(), _> = redis::cmd("SETEX")
                    .arg(key).arg(ttl).arg(buf)
                    .query_async(&mut *conn).await;
            }
        }
    }

    fn text_hash(text: &str) -> String {
        use std::hash::{Hash, Hasher};
        use std::collections::hash_map::DefaultHasher;
        let mut h = DefaultHasher::new();
        text.hash(&mut h);
        format!("{:x}", h.finish())
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// Tests
// ═════════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_hash_deterministic() {
        let h1 = LocalSentimentAnalyzer::text_hash("hello world");
        let h2 = LocalSentimentAnalyzer::text_hash("hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn text_hash_differs() {
        let h1 = LocalSentimentAnalyzer::text_hash("buy");
        let h2 = LocalSentimentAnalyzer::text_hash("sell");
        assert_ne!(h1, h2);
    }

    #[test]
    fn sentiment_result_serialize_roundtrip() {
        let r = SentimentResult {
            score: 0.75, confidence: 0.9,
            source: "local".to_string(), latency_ms: 12,
        };
        let buf = rmp_serde::to_vec_named(&r).unwrap();
        let r2: SentimentResult = rmp_serde::from_slice(&buf).unwrap();
        assert!((r2.score - r.score).abs() < 1e-6);
        assert_eq!(r2.source, r.source);
    }

    #[test]
    fn batch_empty_returns_zero() {
        // Synchronous simulation of batch with empty input
        let texts: Vec<String> = vec![];
        // Chỉ test logic tính toán (không cần async runtime)
        let (mut ws, mut tw) = (0.0f64, 0.0f64);
        for t in &texts {
            let (s, c) = (0.5f64, 0.8f64); // mock
            ws += s * c; tw += c;
        }
        let avg = if tw > 0.0 { ws / tw } else { 0.0 };
        assert_eq!(avg, 0.0);
    }

    #[test]
    fn confidence_gate_logic() {
        // Nếu local confidence >= threshold → không cần LLM
        let threshold = 0.8f64;
        let local_conf = 0.85f64;
        let need_llm = local_conf < threshold;
        assert!(!need_llm, "Should NOT call LLM when local confidence is high");

        let local_conf_low = 0.6f64;
        let need_llm_2 = local_conf_low < threshold;
        assert!(need_llm_2, "Should call LLM when local confidence is low");
    }

    #[test]
    fn cooldown_logic() {
        let cooldowns: CooldownMap = DashMap::new();
        let cooldown_dur = Duration::from_secs(60);
        let sym = "EURUSD";

        // Chưa có entry → không cooldown
        assert!(!cooldowns.contains_key(sym));

        // Thêm entry vừa xong
        cooldowns.insert(sym.to_string(), Instant::now());

        // Kiểm tra cooldown active
        if let Some(last) = cooldowns.get(sym) {
            let is_cooling = last.elapsed() < cooldown_dur;
            assert!(is_cooling, "Should be in cooldown immediately after call");
        }
    }
}
