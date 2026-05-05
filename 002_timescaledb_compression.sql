-- migrations/002_timescaledb_compression.sql
-- ════════════════════════════════════════════════════════════════════════════
-- TimescaleDB Compression: giảm 70-95% dung lượng dữ liệu lịch sử.
-- Áp dụng sau 7 ngày (không ảnh hưởng real-time chunk hiện tại).
-- ════════════════════════════════════════════════════════════════════════════

-- ── market_ticks compression ─────────────────────────────────────────────────
ALTER TABLE market_ticks SET (
    timescaledb.compress,
    timescaledb.compress_segmentby = 'symbol',          -- group theo symbol
    timescaledb.compress_orderby   = 'ts ASC'           -- ordered scan hiệu quả
);

-- Tự động compress chunk > 7 ngày
SELECT add_compression_policy(
    'market_ticks',
    compress_after => INTERVAL '7 days',
    if_not_exists  => TRUE
);

-- ── audit_log compression ─────────────────────────────────────────────────────
ALTER TABLE audit_log SET (
    timescaledb.compress,
    timescaledb.compress_segmentby = 'action',
    timescaledb.compress_orderby   = 'ts ASC'
);

SELECT add_compression_policy(
    'audit_log',
    compress_after => INTERVAL '14 days',
    if_not_exists  => TRUE
);

-- ── Continuous Aggregate: OHLCV 1H (materialized view tự cập nhật) ───────────
-- Dùng để backtest nhanh mà không scan raw ticks
CREATE MATERIALIZED VIEW IF NOT EXISTS market_ohlcv_1h
WITH (timescaledb.continuous) AS
SELECT
    symbol,
    time_bucket('1 hour', ts)   AS bucket,
    FIRST(open,  ts)            AS open,
    MAX(high)                   AS high,
    MIN(low)                    AS low,
    LAST(close,  ts)            AS close,
    SUM(volume)                 AS volume
FROM market_ticks
GROUP BY symbol, bucket
WITH NO DATA;

-- Refresh policy: cập nhật mỗi 5 phút, lag 10 phút
SELECT add_continuous_aggregate_policy(
    'market_ohlcv_1h',
    start_offset  => INTERVAL '3 hours',
    end_offset    => INTERVAL '10 minutes',
    schedule_interval => INTERVAL '5 minutes',
    if_not_exists => TRUE
);

-- Index cho continuous aggregate
CREATE INDEX IF NOT EXISTS idx_ohlcv_1h_symbol_bucket
    ON market_ohlcv_1h (symbol, bucket DESC);

-- ── Continuous Aggregate: OHLCV 1D ───────────────────────────────────────────
CREATE MATERIALIZED VIEW IF NOT EXISTS market_ohlcv_1d
WITH (timescaledb.continuous) AS
SELECT
    symbol,
    time_bucket('1 day', ts)   AS bucket,
    FIRST(open,  ts)           AS open,
    MAX(high)                  AS high,
    MIN(low)                   AS low,
    LAST(close,  ts)           AS close,
    SUM(volume)                AS volume
FROM market_ticks
GROUP BY symbol, bucket
WITH NO DATA;

SELECT add_continuous_aggregate_policy(
    'market_ohlcv_1d',
    start_offset  => INTERVAL '3 days',
    end_offset    => INTERVAL '1 hour',
    schedule_interval => INTERVAL '1 hour',
    if_not_exists => TRUE
);

-- ── Retention Policy: tự xoá raw ticks > 1 năm ───────────────────────────────
-- (Aggregate vẫn giữ vĩnh viễn)
SELECT add_retention_policy(
    'market_ticks',
    drop_after    => INTERVAL '365 days',
    if_not_exists => TRUE
);
