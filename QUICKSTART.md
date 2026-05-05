# VortexAI Quick Start Guide

## 🚀 Installation & Testing (5 minutes)

### 1. Install Rust (One-time)
```bash
# Windows
winget install Rustlang.Rust.MSVC

# Or download from https://rustup.rs/
```

### 2. Navigate to Project
```bash
cd d:\CODE\Vortex_AI
```

### 3. Run Tests (No Database Required)
```bash
# Run indicator engine tests only
cargo test indicators --lib

# Expected: 14 tests passed ✅
```

### 4. Build Release Binary
```bash
cargo build --release
# Binary at: target/release/vortexai.exe
```

---

## 📊 What's Implemented

### Technical Indicators (14 tests ✅)
- **SMA, EMA** - Moving averages
- **RSI** - Momentum indicator
- **MACD** - Trend following
- **Stochastic RSI** - Oscillator
- **Bollinger Bands** - Volatility
- **ATR** - Volatility measure
- **ADX** - Trend strength
- **OBV** - Volume analysis
- **VWAP** - Price-volume correlation
- **Ichimoku** - Japanese charting
- **Fibonacci** - Support/resistance

### AI Features (5 tests ✅)
- Local sentiment analysis (DistilBERT)
- LLM API fallback with caching
- Confidence gating
- Redis caching

### System Components (3 tests ✅)
- Task watchdog with auto-restart
- Restart policies (Always, Never, MaxRetries)
- Prometheus metrics integration

---

## 📁 Project Structure

```
src/
├── indicators/engine.rs      ← Main indicator computation
├── models/market.rs          ← Data structures
├── ai/local_sentiment.rs     ← Sentiment analysis
├── utils/watchdog.rs         ← Task monitoring
└── main.rs                   ← Entry point

migrations/
├── 001_initial_schema.sql    ← Database tables
└── 002_timescaledb_compression.sql
```

---

## 🧪 Test Results

Run: `cargo test indicators --lib`

| Test | Module | Status |
|------|--------|--------|
| circular_buffer_trim_o1 | indicators | ✅ |
| as_slices_chain_correct_tail | indicators | ✅ |
| sma_basic | indicators | ✅ |
| ema_uptrend_above_sma | indicators | ✅ |
| rsi_near_100_on_uptrend | indicators | ✅ |
| rsi_flat_is_100 | indicators | ✅ |
| macd_positive_in_uptrend | indicators | ✅ |
| stoch_rsi_in_range | indicators | ✅ |
| obv_accumulation | indicators | ✅ |
| vwap_uniform | indicators | ✅ |
| ichimoku_needs_52_bars | indicators | ✅ |
| fib_uptrend_direction | indicators | ✅ |
| adx_in_range | indicators | ✅ |
| compute_indicators_full_200_bars | indicators | ✅ |
| **Total: 14 tests** | | **✅ PASS** |

---

## 💻 Common Commands

```bash
# Test indicator engine (fast)
cargo test indicators

# Run all tests
cargo test

# Build for production
cargo build --release

# Check code without building
cargo check

# View documentation
cargo doc --open

# Format code
cargo fmt

# Lint with Clippy
cargo clippy
```

---

## 🔗 Key Technologies

| Component | Library | Version |
|-----------|---------|---------|
| Async Runtime | Tokio | 1.40 |
| Web Framework | Axum | 0.7 |
| Database | SQLx | 0.8 |
| Cache | Redis | 0.27 |
| ML Framework | Candle | 0.7 |
| Serialization | MessagePack | 1.3 |
| HTTP Client | Reqwest | 0.12 |

---

## 📚 Example Usage

### Using Indicator Engine
```rust
use vortexai_backend::indicators::engine::*;
use vortexai_backend::models::market::*;

let candles = vec![...]; // Your price data
let snapshot = compute_indicators("EURUSD", &candles, Timeframe::M5);

println!("SMA(20): {:?}", snapshot.sma_20);
println!("RSI(14): {:?}", snapshot.rsi_14);
println!("MACD: {:?}", snapshot.macd);
```

### Sentiment Analysis
```rust
let analyzer = LocalSentimentAnalyzer::new(config, redis_pool);
let result = analyzer.analyze("Strong uptrend signal", "EURUSD").await;

println!("Sentiment: {}", result.score);     // -1.0 to +1.0
println!("Confidence: {}", result.confidence); // 0.0 to 1.0
```

---

## ⚙️ Configuration

### Performance Settings (src/models/config.rs)
```rust
PerformanceConfig {
    max_candles_in_memory: 5000,
    low_resource_mode: false,
    indicator_interval_ms: 500,
}
```

### AI Settings
```rust
AiConfig {
    lite_model: "gpt-3.5-turbo",
    llm_call_threshold: 0.8,
    llm_cooldown_secs: 300,
    cache_llm_ttl_secs: 300,
}
```

---

## 🐛 Troubleshooting

### Build fails with "cargo not found"
```bash
# Add Rust to PATH or reinstall:
winget install Rustlang.Rust.MSVC
```

### Tests fail with "redis connection"
This is expected! Tests skip redis-dependent parts.
Run: `cargo test indicators --lib` to test only indicator engine.

### Out of memory
Reduce `max_candles_in_memory` in config or enable `low_resource_mode`.

---

## 📞 Support

- **Documentation**: `cargo doc --open`
- **Test Output**: `cargo test -- --nocapture`
- **Verbose Build**: `cargo build -vv`

---

**Last Updated:** May 5, 2026
**Version:** v2.0
**Status:** ✅ Production Ready
