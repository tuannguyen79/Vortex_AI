-- migrations/001_initial_schema.sql
-- ════════════════════════════════════════════════════════════════════════════
-- VortexAI – Initial Database Schema (TimescaleDB required)
-- Run: sqlx migrate run
-- ════════════════════════════════════════════════════════════════════════════

-- Kích hoạt extension
CREATE EXTENSION IF NOT EXISTS timescaledb CASCADE;
CREATE EXTENSION IF NOT EXISTS "uuid-ossp";
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;

-- ── market_ticks: raw OHLCV storage ──────────────────────────────────────────
CREATE TABLE IF NOT EXISTS market_ticks (
    id          UUID         NOT NULL DEFAULT uuid_generate_v4(),
    symbol      TEXT         NOT NULL,
    market_type TEXT         NOT NULL,   -- 'VnStock' | 'Forex' | 'Commodity'
    ts          TIMESTAMPTZ  NOT NULL,
    open        NUMERIC(20,8) NOT NULL,
    high        NUMERIC(20,8) NOT NULL,
    low         NUMERIC(20,8) NOT NULL,
    close       NUMERIC(20,8) NOT NULL,
    volume      NUMERIC(30,8) NOT NULL DEFAULT 0,
    bid         NUMERIC(20,8),
    ask         NUMERIC(20,8),
    spread      NUMERIC(20,8),
    session     TEXT         NOT NULL DEFAULT 'Unknown',
    PRIMARY KEY (symbol, ts)
);

-- Hypertable (TimescaleDB) – partition theo ngày
SELECT create_hypertable(
    'market_ticks', 'ts',
    if_not_exists      => TRUE,
    chunk_time_interval => INTERVAL '1 day'
);

-- Index thường dùng
CREATE INDEX IF NOT EXISTS idx_ticks_symbol_ts ON market_ticks (symbol, ts DESC);

-- ── signals: lịch sử tín hiệu AI ─────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS signals (
    id               UUID        PRIMARY KEY DEFAULT uuid_generate_v4(),
    symbol           TEXT        NOT NULL,
    ts               TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    direction        TEXT        NOT NULL,   -- 'StrongBuy' .. 'StrongSell'
    confidence       FLOAT8      NOT NULL,
    composite_score  FLOAT8      NOT NULL,
    consensus_pct    FLOAT8      NOT NULL,
    entry_price      FLOAT8      NOT NULL,
    stop_loss        FLOAT8      NOT NULL,
    take_profit1     FLOAT8,
    take_profit2     FLOAT8,
    take_profit3     FLOAT8,
    risk_reward      FLOAT8,
    position_size_pct FLOAT8,
    alert_level      TEXT        NOT NULL DEFAULT 'Info',
    trade_mode       TEXT        NOT NULL DEFAULT 'AlertOnly',
    agent_data       JSONB,      -- raw agent signals
    llm_analysis     TEXT,
    expiry_ts        TIMESTAMPTZ,
    approved_by      TEXT,       -- NULL = auto, username = manual
    approved_at      TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_signals_symbol_ts ON signals (symbol, ts DESC);
CREATE INDEX IF NOT EXISTS idx_signals_direction  ON signals (direction);
CREATE INDEX IF NOT EXISTS idx_signals_alert      ON signals (alert_level) WHERE alert_level IN ('Critical','Urgent');

-- ── orders: lệnh giao dịch ───────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS orders (
    id              UUID        PRIMARY KEY DEFAULT uuid_generate_v4(),
    signal_id       UUID        REFERENCES signals(id) ON DELETE SET NULL,
    symbol          TEXT        NOT NULL,
    side            TEXT        NOT NULL,       -- 'Buy' | 'Sell'
    order_type      TEXT        NOT NULL,
    quantity        NUMERIC(20,8) NOT NULL,
    price           NUMERIC(20,8),
    stop_loss       NUMERIC(20,8),
    take_profit1    NUMERIC(20,8),
    take_profit2    NUMERIC(20,8),
    take_profit3    NUMERIC(20,8),
    trailing_pct    FLOAT8,
    status          TEXT        NOT NULL DEFAULT 'Pending',
    filled_qty      NUMERIC(20,8) NOT NULL DEFAULT 0,
    filled_avg      NUMERIC(20,8),
    trade_mode      TEXT        NOT NULL,
    broker_ref      TEXT,
    pnl             FLOAT8,
    commission      NUMERIC(20,8),
    notes           TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_orders_symbol  ON orders (symbol, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_orders_status  ON orders (status) WHERE status NOT IN ('Filled','Cancelled','Rejected');

-- ── trade_records: lịch sử giao dịch hoàn thành (dùng để học) ────────────────
CREATE TABLE IF NOT EXISTS trade_records (
    id                  UUID        PRIMARY KEY DEFAULT uuid_generate_v4(),
    order_id            UUID        REFERENCES orders(id),
    symbol              TEXT        NOT NULL,
    side                TEXT        NOT NULL,
    entry_price         FLOAT8      NOT NULL,
    exit_price          FLOAT8      NOT NULL,
    quantity            FLOAT8      NOT NULL,
    pnl                 FLOAT8      NOT NULL,
    pnl_pct             FLOAT8      NOT NULL,
    duration_secs       BIGINT,
    entry_ts            TIMESTAMPTZ NOT NULL,
    exit_ts             TIMESTAMPTZ NOT NULL,
    agents_used         TEXT[],
    signal_confidence   FLOAT8,
    is_success          BOOLEAN     NOT NULL,
    -- Features cho adaptive learning
    rsi_at_entry        FLOAT8,
    macd_hist_entry     FLOAT8,
    adx_at_entry        FLOAT8,
    bb_pct_b_entry      FLOAT8,
    sentiment_score     FLOAT8,
    session             TEXT,
    market_regime       TEXT        DEFAULT 'Unknown'
);

CREATE INDEX IF NOT EXISTS idx_trade_records_symbol ON trade_records (symbol, exit_ts DESC);
CREATE INDEX IF NOT EXISTS idx_trade_records_success ON trade_records (is_success, symbol);

-- ── pattern_stats: thống kê pattern học được ─────────────────────────────────
CREATE TABLE IF NOT EXISTS pattern_stats (
    id                UUID    PRIMARY KEY DEFAULT uuid_generate_v4(),
    pattern_key       TEXT    NOT NULL UNIQUE,
    occurrences       INT     NOT NULL DEFAULT 0,
    win_count         INT     NOT NULL DEFAULT 0,
    total_pnl         FLOAT8  NOT NULL DEFAULT 0,
    last_updated      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- ── agent_weights: trọng số voting adaptive ──────────────────────────────────
CREATE TABLE IF NOT EXISTS agent_weights (
    agent_name    TEXT        PRIMARY KEY,
    weight        FLOAT8      NOT NULL DEFAULT 1.0,
    accuracy_7d   FLOAT8,
    total_signals INT         NOT NULL DEFAULT 0,
    last_updated  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Seed default weights
INSERT INTO agent_weights (agent_name, weight) VALUES
    ('MomentumAgent',    1.0),
    ('ReversalAgent',    1.0),
    ('BreakoutAgent',    1.0),
    ('SentimentAgent',   0.8),
    ('FundamentalAgent', 0.6),
    ('RiskManagerAgent', 1.5)
ON CONFLICT DO NOTHING;

-- ── users: xác thực ──────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS users (
    id           UUID    PRIMARY KEY DEFAULT uuid_generate_v4(),
    username     TEXT    NOT NULL UNIQUE,
    password_hash TEXT   NOT NULL,
    role         TEXT    NOT NULL DEFAULT 'viewer',  -- 'admin' | 'trader' | 'viewer'
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- ── audit_log: log mọi hành động quan trọng ──────────────────────────────────
CREATE TABLE IF NOT EXISTS audit_log (
    id         BIGSERIAL   PRIMARY KEY,
    ts         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    user_id    UUID,
    action     TEXT        NOT NULL,
    entity     TEXT,
    entity_id  TEXT,
    detail     JSONB
);

-- Hypertable cho audit_log
SELECT create_hypertable(
    'audit_log', 'ts',
    if_not_exists      => TRUE,
    chunk_time_interval => INTERVAL '7 days'
);
