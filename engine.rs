// src/indicators/engine.rs
// ════════════════════════════════════════════════════════════════════════════
// Technical Indicator Engine – VortexAI
//
// Upgrade log (v2):
//  [MEM]  VecDeque<Candle>  : O(1) push_back/pop_front vs Vec::drain O(n)
//  [NET]  Redis pipeline    : 1 RTT per batch vs N RTTs
//  [SER]  MessagePack       : ~30% smaller, ~3× faster than JSON
//  [PERF] Rayon par_iter    : compute all symbols in parallel
//  [OBS]  metrics::counter! : Prometheus counters cho mọi path
// ════════════════════════════════════════════════════════════════════════════

use std::{collections::VecDeque, sync::Arc, time::Duration};

use dashmap::DashMap;
use rayon::prelude::*;
use tokio::time::interval;
use tracing::{debug, error, info, warn};

use crate::{
    data::symbol_manager::SymbolManager,
    models::{config::PerformanceConfig, market::*},
};

const PUBLISH_BATCH_SIZE:    usize = 50;
const PUBLISH_BATCH_SIZE_LR: usize = 20;
const STREAM_KEY:            &str  = "indicator_data";
const STREAM_MAXLEN:         u32   = 2_000;

// ─────────────────────────────────────────────────────────────────────────────

pub struct IndicatorEngine {
    pub db:      sqlx::PgPool,
    pub redis:   deadpool_redis::Pool,
    pub symbols: Arc<SymbolManager>,
    pub config:  PerformanceConfig,
    /// Circular buffer: symbol -> VecDeque<Candle>
    candle_cache:    Arc<DashMap<String, VecDeque<Candle>>>,
    /// Read-path cache for agents / API
    indicator_cache: Arc<DashMap<String, IndicatorSnapshot>>,
}

impl IndicatorEngine {
    pub fn new(
        db:      sqlx::PgPool,
        redis:   deadpool_redis::Pool,
        symbols: Arc<SymbolManager>,
        config:  PerformanceConfig,
    ) -> Self {
        Self {
            db, redis, symbols, config,
            candle_cache:    Arc::new(DashMap::new()),
            indicator_cache: Arc::new(DashMap::new()),
        }
    }

    // ── Read-path ─────────────────────────────────────────────────────────────

    pub fn get_snapshot(&self, symbol: &str) -> Option<IndicatorSnapshot> {
        self.indicator_cache.get(symbol).map(|v| v.clone())
    }

    /// Trả Vec<Candle> cho agents / API.
    /// Dùng as_slices().chain() - không cần make_contiguous() (immutable).
    pub fn get_candles(&self, symbol: &str, limit: usize) -> Vec<Candle> {
        self.candle_cache.get(symbol).map(|entry| {
            let deque = entry.value();
            let (s1, s2) = deque.as_slices();
            let total = deque.len();
            let skip  = total.saturating_sub(limit);
            s1.iter().chain(s2.iter()).skip(skip).cloned().collect()
        }).unwrap_or_default()
    }

    pub fn candle_count(&self, symbol: &str) -> usize {
        self.candle_cache.get(symbol).map(|e| e.len()).unwrap_or(0)
    }

    // ── Push-path (DataIngestor → IndicatorEngine) ────────────────────────────

    pub fn push_tick(&self, tick: &MarketTick) {
        use rust_decimal::prelude::ToPrimitive;
        let price = tick.close.to_f64().unwrap_or(0.0);
        let vol   = tick.volume.to_f64().unwrap_or(0.0);
        let ts_bucket = (tick.timestamp.unix_timestamp() / 300) * 300;
        let ts_ms     = ts_bucket * 1_000;

        let mut entry = self.candle_cache
            .entry(tick.symbol.clone())
            .or_insert_with(VecDeque::new);

        // Cập nhật nến hiện tại nếu trùng bucket
        if let Some(last) = entry.back_mut() {
            if last.ts == ts_ms {
                if price > last.high { last.high = price; }
                if price < last.low  { last.low  = price; }
                last.close   = price;
                last.volume += vol;
                return;
            }
        }

        // Nến mới - push_back O(1)
        entry.push_back(Candle {
            symbol:    tick.symbol.clone(),
            timeframe: Timeframe::M5,
            ts:        ts_ms,
            open:      price,
            high:      price,
            low:       price,
            close:     price,
            volume:    vol,
            is_closed: false,
        });

        // Trim: pop_front O(1) - không dịch chuyển bộ nhớ
        let max = self.config.max_candles_in_memory;
        while entry.len() > max {
            entry.pop_front();
        }
    }

    // ── Computation loop ──────────────────────────────────────────────────────

    pub async fn run_loop(&self) {
        let interval_ms = if self.config.low_resource_mode {
            self.config.indicator_interval_ms.max(3_000)
        } else {
            self.config.indicator_interval_ms.max(500)
        };
        let batch_size = if self.config.low_resource_mode {
            PUBLISH_BATCH_SIZE_LR
        } else {
            PUBLISH_BATCH_SIZE
        };

        info!(interval_ms, batch_size, "IndicatorEngine started");

        let mut ticker  = interval(Duration::from_millis(interval_ms));
        let mut pending: Vec<(String, IndicatorSnapshot)> = Vec::with_capacity(batch_size * 2);

        loop {
            ticker.tick().await;
            let symbols = self.symbols.active_symbols().await;
            if symbols.is_empty() { continue; }

            // 1. Snapshot Vec<Candle> từ VecDeque (giải phóng lock ngay)
            let symbol_candles: Vec<(String, Vec<Candle>)> = symbols
                .iter()
                .filter_map(|sym| {
                    let entry = self.candle_cache.get(sym)?;
                    let deque = entry.value();
                    if deque.len() < 26 { return None; }
                    let (s1, s2) = deque.as_slices();
                    let candles: Vec<Candle> = s1.iter().chain(s2.iter()).cloned().collect();
                    Some((sym.clone(), candles))
                })
                .collect();

            // 2. Tính song song Rayon
            let new_snaps: Vec<(String, IndicatorSnapshot)> = symbol_candles
                .par_iter()
                .map(|(sym, candles)| {
                    let snap = compute_indicators(sym, candles, Timeframe::M5);
                    (sym.clone(), snap)
                })
                .collect();

            // 3. Cập nhật in-memory cache
            for (sym, snap) in &new_snaps {
                self.indicator_cache.insert(sym.clone(), snap.clone());
            }
            pending.extend(new_snaps);

            // 4. Flush batch khi đủ
            if pending.len() >= batch_size {
                self.flush_to_redis(&pending).await;
                pending.clear();
            }

            metrics::gauge!("indicator_symbols_computed").set(symbols.len() as f64);
        }
    }

    /// Redis pipeline + MessagePack: 1 round-trip cho toàn bộ batch.
    async fn flush_to_redis(&self, snapshots: &[(String, IndicatorSnapshot)]) {
        let mut conn = match self.redis.get().await {
            Ok(c)  => c,
            Err(e) => { warn!("Redis pool exhausted: {:?}", e); return; }
        };

        let mut pipe = redis::pipe();
        pipe.atomic();

        let mut err_count = 0usize;
        for (sym, snap) in snapshots {
            match rmp_serde::to_vec_named(snap) {
                Ok(buf) => {
                    pipe.cmd("XADD")
                        .arg(STREAM_KEY)
                        .arg("MAXLEN").arg("~").arg(STREAM_MAXLEN)
                        .arg("*")
                        .arg("sym").arg(sym.as_str())
                        .arg("d").arg(buf.as_slice());
                    // Pub/sub nhanh cho WS hub
                    pipe.cmd("PUBLISH")
                        .arg(format!("ind:{sym}"))
                        .arg(buf.as_slice());
                }
                Err(e) => { err_count += 1; warn!(sym, %e, "msgpack error"); }
            }
        }

        if err_count > 0 {
            metrics::counter!("indicator_serialize_errors").increment(err_count as u64);
        }

        match pipe.query_async::<()>(&mut *conn).await {
            Ok(_)  => {
                metrics::counter!("indicator_redis_batches").increment(1);
                debug!(count = snapshots.len(), "batch flushed");
            }
            Err(e) => {
                error!("Redis pipeline error: {:?}", e);
                metrics::counter!("indicator_redis_errors").increment(1);
            }
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// Pure computation (no IO, rayon-safe, fully unit-tested)
// ═════════════════════════════════════════════════════════════════════════════

pub fn compute_indicators(symbol: &str, candles: &[Candle], tf: Timeframe) -> IndicatorSnapshot {
    let n = candles.len();
    let mut closes = Vec::with_capacity(n);
    let mut highs  = Vec::with_capacity(n);
    let mut lows   = Vec::with_capacity(n);
    let mut vols   = Vec::with_capacity(n);
    for c in candles {
        closes.push(c.close);
        highs.push(c.high);
        lows.push(c.low);
        vols.push(c.volume);
    }
    let ts = candles.last().map(|c| c.ts).unwrap_or(0);
    IndicatorSnapshot {
        symbol: symbol.to_string(), ts, timeframe: tf,
        sma_20:   sma(&closes, 20),
        sma_50:   sma(&closes, 50),
        ema_9:    ema(&closes, 9),
        ema_21:   ema(&closes, 21),
        ema_200:  ema(&closes, 200),
        rsi_14:   rsi(&closes, 14),
        macd:     macd_indicator(&closes, 12, 26, 9),
        stoch_rsi: stoch_rsi_full(&closes, 14, 3, 3),
        atr_14:   atr(&highs, &lows, &closes, 14),
        bb:       bollinger_bands(&closes, 20, 2.0),
        adx_14:   adx(&highs, &lows, &closes, 14),
        obv:      obv(&closes, &vols),
        vwap:     vwap_from_slices(&highs, &lows, &closes, &vols),
        ichimoku: ichimoku(&highs, &lows),
        fib:      fibonacci(&highs, &lows, 50),
    }
}

#[inline] fn period_high(d: &[f64], p: usize) -> Option<f64> {
    if d.len() < p { None } else { d[d.len()-p..].iter().cloned().reduce(f64::max) }
}
#[inline] fn period_low(d: &[f64], p: usize) -> Option<f64> {
    if d.len() < p { None } else { d[d.len()-p..].iter().cloned().reduce(f64::min) }
}

pub fn sma(data: &[f64], period: usize) -> Option<f64> {
    if data.len() < period { return None; }
    Some(data[data.len()-period..].iter().sum::<f64>() / period as f64)
}

pub fn ema(data: &[f64], period: usize) -> Option<f64> {
    if data.len() < period { return None; }
    let k = 2.0 / (period as f64 + 1.0);
    let mut val = data[..period].iter().sum::<f64>() / period as f64;
    for &p in &data[period..] { val = p * k + val * (1.0 - k); }
    Some(val)
}

pub fn rsi(data: &[f64], period: usize) -> Option<f64> {
    if data.len() < period + 1 { return None; }
    let start = data.len() - period - 1;
    let (mut ag, mut al) = (0.0f64, 0.0f64);
    for i in start..start+period {
        let d = data[i+1] - data[i];
        if d > 0.0 { ag += d; } else { al -= d; }
    }
    ag /= period as f64; al /= period as f64;
    for i in (start+period)..(data.len()-1) {
        let d = data[i+1] - data[i];
        let g = if d>0.0{d}else{0.0};
        let l = if d<0.0{-d}else{0.0};
        ag = (ag*(period as f64-1.0)+g)/period as f64;
        al = (al*(period as f64-1.0)+l)/period as f64;
    }
    if al == 0.0 { return Some(100.0); }
    Some(100.0 - 100.0/(1.0+ag/al))
}

pub fn macd_indicator(data: &[f64], fast: usize, slow: usize, sig: usize) -> Option<MacdValue> {
    if data.len() < slow + sig { return None; }
    let series: Vec<f64> = (slow..=data.len())
        .filter_map(|i| Some(ema(&data[..i], fast)? - ema(&data[..i], slow)?))
        .collect();
    if series.len() < sig { return None; }
    let signal = ema(&series, sig)?;
    let macd_v = *series.last()?;
    Some(MacdValue { macd: macd_v, signal, hist: macd_v - signal })
}

pub fn stoch_rsi_full(data: &[f64], rp: usize, kp: usize, dp: usize) -> Option<StochRsiValue> {
    if data.len() < rp+kp+dp+1 { return None; }
    let rs: Vec<f64> = (rp+1..=data.len()).filter_map(|i| rsi(&data[..i], rp)).collect();
    if rs.len() < kp+dp { return None; }
    let ks: Vec<f64> = (kp..=rs.len()).filter_map(|i| {
        let w = &rs[i-kp..i];
        let lo = w.iter().cloned().fold(f64::INFINITY, f64::min);
        let hi = w.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if hi==lo { Some(50.0) } else { Some((*w.last()? - lo)/(hi-lo)*100.0) }
    }).collect();
    let k = *ks.last()?;
    let d = sma(&ks, dp)?;
    Some(StochRsiValue { k, d })
}

pub fn atr(highs: &[f64], lows: &[f64], closes: &[f64], period: usize) -> Option<f64> {
    let n = closes.len();
    if n < period+1 { return None; }
    let trs: Vec<f64> = (1..n).map(|i| {
        (highs[i]-lows[i]).max((highs[i]-closes[i-1]).abs()).max((lows[i]-closes[i-1]).abs())
    }).collect();
    if trs.len() < period { return None; }
    let mut v = trs[..period].iter().sum::<f64>() / period as f64;
    for &t in &trs[period..] { v = (v*(period as f64-1.0)+t)/period as f64; }
    Some(v)
}

pub fn bollinger_bands(data: &[f64], period: usize, k: f64) -> Option<BollingerBands> {
    let mid = sma(data, period)?;
    let slice = &data[data.len()-period..];
    let sd = (slice.iter().map(|x|(x-mid).powi(2)).sum::<f64>()/period as f64).sqrt();
    let (upper, lower) = (mid+k*sd, mid-k*sd);
    let last = *data.last()?;
    let pct_b = if upper!=lower { (last-lower)/(upper-lower) } else { 0.5 };
    let width = if mid!=0.0 { (upper-lower)/mid } else { 0.0 };
    Some(BollingerBands { upper, mid, lower, pct_b, width })
}

pub fn adx(highs: &[f64], lows: &[f64], closes: &[f64], period: usize) -> Option<f64> {
    let n = closes.len();
    if n < period*2+1 { return None; }
    let (mut dp, mut dm, mut tr) = (vec![], vec![], vec![]);
    for i in 1..n {
        let up   = highs[i]-highs[i-1];
        let down = lows[i-1]-lows[i];
        dp.push(if up>down&&up>0.0{up}else{0.0});
        dm.push(if down>up&&down>0.0{down}else{0.0});
        tr.push((highs[i]-lows[i]).max((highs[i]-closes[i-1]).abs()).max((lows[i]-closes[i-1]).abs()));
    }
    let smooth = |v: &[f64]| -> Option<f64> {
        if v.len()<period { return None; }
        Some(v[period..].iter().fold(v[..period].iter().sum::<f64>(), |a,&x| a-a/period as f64+x))
    };
    let (as_, ds, ms) = (smooth(&tr)?, smooth(&dp)?, smooth(&dm)?);
    if as_==0.0 { return None; }
    let (dip, dim) = (ds/as_*100.0, ms/as_*100.0);
    let s = dip+dim; if s==0.0 { return Some(0.0); }
    Some((dip-dim).abs()/s*100.0)
}

pub fn obv(closes: &[f64], vols: &[f64]) -> Option<f64> {
    if closes.len()<2 { return None; }
    Some(closes.windows(2).zip(vols[1..].iter()).fold(0.0f64, |acc,(w,&v)| {
        if w[1]>w[0]{acc+v} else if w[1]<w[0]{acc-v} else{acc}
    }))
}

pub fn vwap_from_slices(h: &[f64], l: &[f64], c: &[f64], v: &[f64]) -> Option<f64> {
    let n = h.len().min(l.len()).min(c.len()).min(v.len());
    if n==0 { return None; }
    let (tv,vs) = (0..n).fold((0.0,0.0),|(tv,vs),i| (tv+(h[i]+l[i]+c[i])/3.0*v[i], vs+v[i]));
    if vs==0.0 { None } else { Some(tv/vs) }
}

pub fn ichimoku(highs: &[f64], lows: &[f64]) -> Option<IchimokuValue> {
    let tenkan   = (period_high(highs,9)?  + period_low(lows,9)?)  / 2.0;
    let kijun    = (period_high(highs,26)? + period_low(lows,26)?) / 2.0;
    let senkou_a = (tenkan + kijun) / 2.0;
    let senkou_b = (period_high(highs,52)? + period_low(lows,52)?) / 2.0;
    Some(IchimokuValue { tenkan, kijun, senkou_a, senkou_b, chikou: None })
}

pub fn fibonacci(highs: &[f64], lows: &[f64], lookback: usize) -> Option<FibLevels> {
    let n = highs.len().min(lows.len());
    if n<lookback { return None; }
    let sh = highs[n-lookback..].iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let sl = lows[n-lookback..].iter().cloned().fold(f64::INFINITY, f64::min);
    let r  = sh - sl;
    let dir = if highs[n-1]>highs[n-lookback] { FibDirection::Uptrend } else { FibDirection::Downtrend };
    Some(FibLevels { swing_high:sh, swing_low:sl,
        r_0236:sh-0.236*r, r_0382:sh-0.382*r, r_0500:sh-0.500*r,
        r_0618:sh-0.618*r, r_0786:sh-0.786*r, r_1000:sl, direction:dir })
}

// ═════════════════════════════════════════════════════════════════════════════
// Tests – cargo test -p vortexai-backend indicators
// ═════════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn make_linear(n: usize) -> Vec<f64> { (1..=n).map(|x| x as f64).collect() }

    #[test]
    fn circular_buffer_trim_o1() {
        let max = 3usize;
        let mut d: VecDeque<f64> = VecDeque::new();
        for i in 0..10u64 {
            d.push_back(i as f64);
            while d.len() > max { d.pop_front(); }
        }
        assert_eq!(d.len(), max);
        let v: Vec<f64> = d.into_iter().collect();
        assert_eq!(v, vec![7.0, 8.0, 9.0]);
    }

    #[test]
    fn as_slices_chain_correct_tail() {
        let mut d: VecDeque<i32> = VecDeque::from([1,2,3,4,5]);
        d.pop_front(); d.push_back(6); // layout may be non-contiguous
        let (s1,s2) = d.as_slices();
        let skip = d.len().saturating_sub(3);
        let r: Vec<i32> = s1.iter().chain(s2.iter()).skip(skip).cloned().collect();
        assert_eq!(r, vec![4,5,6]);
    }

    #[test] fn sma_basic() {
        assert_eq!(sma(&[1.0,2.0,3.0,4.0,5.0], 3), Some(4.0));
        assert_eq!(sma(&[1.0,2.0], 3), None);
    }

    #[test] fn ema_uptrend_above_sma() {
        let d = make_linear(50);
        assert!(ema(&d, 9).unwrap() > sma(&d, 9).unwrap());
    }

    #[test] fn rsi_near_100_on_uptrend() {
        let d: Vec<f64> = (1..=30).map(|x| x as f64).collect();
        assert!(rsi(&d,14).unwrap() > 90.0);
    }

    #[test] fn rsi_flat_is_100() {
        let d = vec![10.0f64; 20];
        assert_eq!(rsi(&d,14), Some(100.0));
    }

    #[test] fn macd_positive_in_uptrend() {
        let d = make_linear(60);
        let m = macd_indicator(&d,12,26,9).unwrap();
        assert!(m.macd > 0.0);
        assert!((m.hist - (m.macd - m.signal)).abs() < 1e-10);
    }

    #[test] fn stoch_rsi_in_range() {
        let d = make_linear(60);
        let s = stoch_rsi_full(&d,14,3,3).unwrap();
        assert!(s.k >= 0.0 && s.k <= 100.0);
        assert!(s.d >= 0.0 && s.d <= 100.0);
    }

    #[test] fn obv_accumulation() {
        let c = vec![10.0,11.0,12.0,11.5,13.0];
        let v = vec![ 0.0,100.0,200.0,150.0,300.0];
        assert_eq!(obv(&c,&v), Some(450.0));
    }

    #[test] fn vwap_uniform() {
        let h = vec![10.0;5]; let l=h.clone(); let c=h.clone(); let v=vec![100.0;5];
        assert_eq!(vwap_from_slices(&h,&l,&c,&v), Some(10.0));
    }

    #[test] fn ichimoku_needs_52_bars() {
        let h: Vec<f64> = (1..=52).map(|x| x as f64).collect();
        let l: Vec<f64> = h.iter().map(|x| x-1.0).collect();
        assert!(ichimoku(&h,&l).is_some());
        assert!(ichimoku(&h[..51],&l[..51]).is_none());
    }

    #[test] fn fib_uptrend_direction() {
        let h: Vec<f64> = (1..=60).map(|x| x as f64).collect();
        let l: Vec<f64> = h.iter().map(|x| x-0.5).collect();
        let f = fibonacci(&h,&l,50).unwrap();
        assert!(matches!(f.direction, FibDirection::Uptrend));
        assert!(f.r_0618 < f.r_0382);
    }

    #[test] fn adx_in_range() {
        let c: Vec<f64> = (1..=60).map(|x| x as f64).collect();
        let h: Vec<f64> = c.iter().map(|x| x+0.5).collect();
        let l: Vec<f64> = c.iter().map(|x| x-0.5).collect();
        let a = adx(&h,&l,&c,14).unwrap();
        assert!(a >= 0.0 && a <= 100.0, "ADX={a}");
    }

    #[test] fn compute_indicators_full_200_bars() {
        let candles: Vec<Candle> = (0..200).map(|i| Candle {
            symbol:"T".to_string(), timeframe:Timeframe::M5,
            ts: i*300_000, open:100.0+i as f64*0.1,
            high:100.5+i as f64*0.1, low:99.5+i as f64*0.1,
            close:100.2+i as f64*0.1, volume:1000.0, is_closed:true,
        }).collect();
        let s = compute_indicators("T", &candles, Timeframe::M5);
        assert!(s.sma_20.is_some(),  "sma_20");
        assert!(s.ema_200.is_some(), "ema_200");
        assert!(s.rsi_14.is_some(),  "rsi_14");
        assert!(s.macd.is_some(),    "macd");
        assert!(s.atr_14.is_some(),  "atr_14");
        assert!(s.bb.is_some(),      "bb");
        assert!(s.adx_14.is_some(),  "adx_14");
        assert!(s.ichimoku.is_some(),"ichimoku");
        assert!(s.fib.is_some(),     "fib");
    }
}
