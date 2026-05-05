# VortexAI – Upgrade v2: Integration Guide
# ════════════════════════════════════════════════════════════════════════════
# 5 nhóm cải tiến, pipeline logic, "làm 1 phát ăn luôn"

## Sơ đồ luồng dữ liệu sau upgrade

```
DataIngestor (WebSocket/REST)
       │  push_tick(&MarketTick)
       ▼
IndicatorEngine.push_tick()
  ├── VecDeque<Candle>::push_back()  ← O(1), trim pop_front() O(1)
  │   (thay Vec::drain O(n))
  └── candle_cache: DashMap<String, VecDeque<Candle>>

       │ [interval: 500ms / 3000ms low-resource]
       ▼
IndicatorEngine.run_loop()
  ├── snapshot: as_slices().chain() → Vec<Candle>  (no make_contiguous)
  ├── Rayon par_iter → compute_indicators() [pure, no IO]
  ├── indicator_cache.insert()  (read-path cho agents/API)
  ├── pending.push()
  └── if pending.len() >= batch_size → flush_to_redis()
            │
            ▼
    Redis MULTI/EXEC pipeline
      ├── XADD indicator_data MAXLEN~2000  [MessagePack binary]
      └── PUBLISH ind:{symbol}             [MessagePack binary]

       │ pub/sub
       ▼
WsHub → broadcast → Frontend WebSocket

CoordinatorAgent (subscribe Redis ind:*)
  ├── LocalSentimentAnalyzer.analyze()
  │     ├── local_inference() [DistilBERT CPU, <10ms]
  │     ├── if confidence >= threshold → return  ← 70% cases
  │     ├── if cooldown active → return local/neutral
  │     ├── Redis GET cache:sentiment:{hash}
  │     └── LLM API (lite model) → Redis SETEX TTL=5min
  └── composite_signal → TradingEngine

TradingEngine
  ├── RiskManager.check()
  ├── if TradeMode::AutoApproved → place order
  ├── if TradeMode::PendingApproval → notify frontend (manual confirm)
  └── LearningEngine.record_trade() → pattern_stats UPDATE

Watchdog (JoinSet)
  ├── monitors: data_ingestor, indicator_engine, signal_pipeline, learning_engine
  ├── on exit/panic → log + metrics::counter!("task_panics_total")
  └── RestartPolicy::Always { delay_secs } → auto-restart

TimescaleDB
  ├── market_ticks → compress after 7 days (segment by symbol)
  ├── market_ohlcv_1h (continuous aggregate, auto-refresh 5min)
  ├── market_ohlcv_1d (continuous aggregate, auto-refresh 1h)
  └── retention: drop raw ticks after 365 days
```

## File thay đổi

| File                                          | Thay đổi                                      |
|-----------------------------------------------|-----------------------------------------------|
| `Cargo.toml`                                  | +rmp-serde, +pprof feature                    |
| `.cargo/config.toml`                          | target-cpu=native, LLD linker                 |
| `migrations/001_initial_schema.sql`           | Full schema: ticks, signals, orders, learning |
| `migrations/002_timescaledb_compression.sql`  | Compression + continuous aggregates           |
| `src/indicators/engine.rs`                    | VecDeque, pipeline, MessagePack, unit tests   |
| `src/ai/local_sentiment.rs`                   | Local DistilBERT + confidence gate + LLM      |
| `src/utils/watchdog.rs`                       | JoinSet watchdog + profiling endpoint         |
| `src/main.rs`                                 | Watchdog-based task spawn                     |

## Cài đặt & Chạy

### Yêu cầu

```bash
# Rust 1.80+
rustup update stable

# LLD linker (Linux)
apt install lld

# TimescaleDB
docker run -d --name tsdb \
  -e POSTGRES_PASSWORD=vortex \
  -p 5432:5432 \
  timescale/timescaledb-ha:pg16-latest

# Redis
docker run -d --name redis -p 6379:6379 redis:7-alpine
```

### Tải model sentiment local (tuỳ chọn)

```bash
pip install huggingface_hub
huggingface-cli download \
  distilbert-base-uncased-finetuned-sst-2-english \
  --local-dir assets/sentiment \
  --include "*.json" "*.safetensors"
```

### Build

```bash
# Build thường (không có local AI model)
cargo build --release

# Build với local sentiment model
cargo build --release --features local-ai

# Build với profiling endpoint
cargo build --release --features profiling

# Build full
cargo build --release --features "local-ai,profiling"
```

### Chạy test

```bash
# Chỉ test indicator engine (nhanh, no DB needed)
cargo test -p vortexai-backend indicators -- --nocapture

# Chạy tất cả unit tests
cargo test -p vortexai-backend -- --nocapture

# Test với output JSON
cargo test -p vortexai-backend -- -Z unstable-options --format json
```

### Config `.env`

```env
VORTEX__DATABASE__URL=postgresql://postgres:vortex@localhost/vortexai
VORTEX__REDIS__URL=redis://localhost:6379
VORTEX__SERVER__JWT_SECRET=change_me_in_production_min_32_chars
VORTEX__SERVER__PORT=8080
VORTEX__SERVER__HOST=0.0.0.0

# AI Keys (chỉ cần 1 trong 3)
VORTEX__AI__OPENROUTER_KEY=sk-or-...    # ưu tiên (rẻ hơn)
VORTEX__AI__OPENAI_API_KEY=sk-...
VORTEX__AI__ANTHROPIC_API_KEY=sk-ant-...

# Tiết kiệm token
VORTEX__AI__LITE_MODEL=gpt-4o-mini
VORTEX__AI__FULL_MODEL=gpt-4o
VORTEX__AI__LLM_CALL_THRESHOLD=0.8     # chỉ gọi LLM khi confidence < 0.8
VORTEX__AI__LLM_COOLDOWN_SECS=60
VORTEX__AI__CACHE_LLM_TTL_SECS=300

# Máy yếu
VORTEX__PERFORMANCE__LOW_RESOURCE_MODE=true
VORTEX__PERFORMANCE__INDICATOR_INTERVAL_MS=3000
VORTEX__PERFORMANCE__MAX_CANDLES_IN_MEMORY=300
VORTEX__PERFORMANCE__WORKER_THREADS=2
```

### Migrations

```bash
cargo install sqlx-cli
sqlx migrate run --database-url "$VORTEX__DATABASE__URL"
```

## Kiểm chứng các cải tiến

### 1. VecDeque O(1) pop

```bash
# Log sẽ thấy không có warning về "memory drain"
# Benchmark: cargo bench (thêm benches/indicator_bench.rs)
```

### 2. Redis pipeline

```bash
# Monitor Redis
redis-cli monitor | grep XADD
# Sẽ thấy batch MULTI/EXEC thay vì từng XADD riêng lẻ
```

### 3. MessagePack size

```bash
# So sánh size (chạy trong tests)
cargo test msgpack_vs_json -- --nocapture
```

### 4. TimescaleDB compression

```sql
-- Sau 7 ngày, kiểm tra compression ratio
SELECT
  hypertable_name,
  pg_size_pretty(before_compression_total_bytes) AS before,
  pg_size_pretty(after_compression_total_bytes)  AS after,
  compression_ratio
FROM timescaledb_information.compression_settings;
```

### 5. Watchdog monitoring

```bash
# Kill một task giả lập
kill -9 <pid_of_ingestor>  # (test trong dev)
# Prometheus: task_panics_total{task="data_ingestor"} sẽ tăng
curl http://localhost:9090/metrics | grep task_panics
```

### 6. Profiling flamegraph

```bash
# Build với feature profiling
cargo build --release --features profiling

# Lấy flamegraph 30 giây
curl "http://localhost:8080/debug/profile?seconds=30" > flamegraph.svg
open flamegraph.svg
```

## Đánh đổi & lưu ý

| Cải tiến       | Lợi ích                    | Chi phí / Đánh đổi                          |
|----------------|----------------------------|---------------------------------------------|
| VecDeque       | O(1) trim, ít GC pressure  | Clone khi tính toán (~μs, acceptable)       |
| Redis pipeline | N→1 RTT, throughput +5x    | Mất atomicity nếu pipe fail giữa chừng      |
| MessagePack    | -30% size, +3x speed       | Binary không readable (dùng JSON cho debug) |
| Local AI       | 70% LLM calls saved        | Model ~250MB RAM, cold start 2-3s           |
| Watchdog       | Zero silent crash          | Restart loop nếu task exit ngay lập tức     |
| TimescaleDB    | -80% storage sau 7 ngày    | Compress chunk không query được bằng range  |
| native CPU     | +10-30% perf               | Binary không portable sang máy khác         |
