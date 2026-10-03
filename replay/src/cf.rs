//! Контрфактический повтор: те же сделки, другой конфиг комиссий.
//!
//! Модель поведения трейдеров (статический повтор):
//!   * покупки повторяются с теми же входными суммами (exact in — та же сумма quote,
//!     exact out — то же желаемое количество base);
//!   * продажа моделируется как ДОЛЯ от позиции кошелька: если в реальности кошелёк
//!     продал 40% своих токенов, в симуляции он продаёт 40% от того, что у него есть
//!     в симуляции. Иначе при высокой комиссии на входе кошелёк "продал бы" токены,
//!     которых у него нет;
//!   * если кошелёк продаёт больше, чем купил в этом пуле (токены пришли извне),
//!     продажа повторяется как есть и кошелёк помечается как external;
//!   * после завершения кривой сделки пропускаются.
//! Реакцию трейдеров на другие комиссии модель не учитывает — это оценка, а не прогноз.

use crate::engine::{simulate_swap, RecordedSwap, SwapInput};
use anyhow::{anyhow, bail, Result};
use dynamic_bonding_curve::{
    activation_handler::ActivationType,
    base_fee::get_base_fee_handler,
    params::swap::TradeDirection,
    state::{fee::FeeMode, PoolConfig, PoolState, SwapResult2},
};
use std::collections::BTreeMap;

const FEE_DENOMINATOR: f64 = 1_000_000_000.0;

/// Расписание базовой комиссии (планировщик DBC).
#[derive(Debug, Clone)]
pub struct FeeSchedule {
    /// 0 = линейный, 1 = экспоненциальный (как base_fee_mode в DBC)
    pub mode: u8,
    pub start_pct: f64,
    pub end_pct: f64,
    pub periods: u16,
    /// длина периода в слотах или секундах — в единицах activation_type конфига
    pub period_len: u64,
}

impl FeeSchedule {
    pub fn describe(&self, activation_type: u8) -> String {
        let unit = if activation_type == 0 { "slot" } else { "s" };
        format!(
            "{} {:.2}% -> {:.2}% over {} periods x {} {}",
            if self.mode == 0 { "linear" } else { "exponential" },
            self.start_pct,
            self.end_pct,
            self.periods,
            self.period_len,
            unit
        )
    }
}

/// Копия конфига с другим планировщиком комиссии. Проверяется теми же правилами,
/// что и при создании конфига onchain (минимум 0,25%, максимум 99% и т.д.).
pub fn with_schedule(config: &PoolConfig, s: &FeeSchedule) -> Result<PoolConfig> {
    if s.end_pct > s.start_pct {
        bail!("end fee must not exceed start fee");
    }
    if s.periods == 0 || s.period_len == 0 {
        bail!("periods and period length must be > 0");
    }
    let cliff = (s.start_pct / 100.0 * FEE_DENOMINATOR).round() as u64;
    let end = (s.end_pct / 100.0 * FEE_DENOMINATOR).round() as u64;
    let reduction = if s.mode == 0 {
        // fee = cliff - period * r
        (cliff - end) / s.periods as u64
    } else {
        // fee = cliff * (1 - r/10000)^period ; округляем вниз, чтобы не уйти ниже end
        let r = 10_000.0 * (1.0 - (end as f64 / cliff as f64).powf(1.0 / s.periods as f64));
        r.floor() as u64
    };
    let mut c = *config;
    let bf = &mut c.pool_fees.base_fee;
    bf.cliff_fee_numerator = cliff;
    bf.first_factor = s.periods;
    bf.second_factor = s.period_len;
    bf.third_factor = reduction.max(1);
    bf.base_fee_mode = s.mode;

    let activation = ActivationType::try_from(c.activation_type)
        .map_err(|_| anyhow!("bad activation_type"))?;
    get_base_fee_handler(bf.cliff_fee_numerator, bf.first_factor, bf.second_factor, bf.third_factor, bf.base_fee_mode)
        .and_then(|h| h.validate(c.collect_fee_mode, activation))
        .map_err(|e| anyhow!("counterfactual config rejected by program rules: {e:?}"))?;
    Ok(c)
}

#[derive(Debug, Default, Clone)]
pub struct WalletStats {
    pub trades: u32,
    pub first_point: Option<u64>,
    pub quote_in: u128,
    pub quote_out: u128,
    pub base: i128,
    pub external: bool,
}

impl WalletStats {
    pub fn realized(&self) -> i128 {
        self.quote_out as i128 - self.quote_in as i128
    }
}

#[derive(Debug, Clone)]
pub enum TradeStatus {
    Done(SwapResult2),
    Skipped(&'static str),
    Error(String),
}

#[derive(Debug)]
pub struct RunResult {
    pub trades: Vec<TradeStatus>,
    pub wallets: BTreeMap<String, WalletStats>,
    /// комиссии в quote и в base (в зависимости от collect_fee_mode часть берётся в base)
    pub trading_fee_quote: u128,
    pub trading_fee_base: u128,
    pub protocol_fee_quote: u128,
    pub protocol_fee_base: u128,
    /// комиссии, пересчитанные в quote по цене после каждой сделки
    pub trading_fee_equiv: f64,
    pub protocol_fee_equiv: f64,
    /// номер сделки, на которой кривая завершилась
    pub completed_at: Option<usize>,
    pub final_pool: PoolState,
}

/// Цена base в единицах quote (сырые единицы) по sqrt price в формате Q64.64.
pub fn raw_price(sqrt_price: u128) -> f64 {
    let s = sqrt_price as f64 / 18_446_744_073_709_551_616.0;
    s * s
}

fn point_of(config: &PoolConfig, s: &RecordedSwap) -> u64 {
    if config.activation_type == 0 { s.slot } else { s.event_timestamp }
}

/// Прогон упорядоченных реальных сделок через `config` по модели выше.
pub fn run(
    config: &PoolConfig,
    mut pool: PoolState,
    ordered: &[RecordedSwap],
    created_sig: &str,
) -> Result<RunResult> {
    // Реальные позиции кошельков — чтобы вычислить долю каждой продажи.
    let mut real_base: BTreeMap<&str, u128> = BTreeMap::new();

    let mut res = RunResult {
        trades: Vec::with_capacity(ordered.len()),
        wallets: BTreeMap::new(),
        trading_fee_quote: 0,
        trading_fee_base: 0,
        protocol_fee_quote: 0,
        protocol_fee_base: 0,
        trading_fee_equiv: 0.0,
        protocol_fee_equiv: 0.0,
        completed_at: None,
        final_pool: pool,
    };

    for (i, s) in ordered.iter().enumerate() {
        let is_buy = s.trade_direction == 1;
        let rb = real_base.entry(s.fee_payer.as_str()).or_insert(0);
        let real_before = *rb;
        // обновляем реальную позицию по записанному результату
        if is_buy {
            *rb += s.result.output_amount as u128;
        } else {
            *rb = rb.saturating_sub(s.result.included_fee_input_amount as u128);
        }

        let w = res.wallets.entry(s.fee_payer.clone()).or_default();
        if w.first_point.is_none() {
            w.first_point = Some(point_of(config, s));
        }

        if res.completed_at.is_some() {
            res.trades.push(TradeStatus::Skipped("curve complete"));
            continue;
        }

        // Сумма сделки в симуляции.
        let mut amount = s.amount_0;
        let base_input_sell = !is_buy && s.swap_mode != 2; // продажа с заданным количеством base
        if base_input_sell {
            let sold_real = s.result.included_fee_input_amount as u128;
            if real_before >= sold_real && real_before > 0 {
                let have = w.base.max(0) as u128;
                amount = if sold_real == real_before {
                    have as u64
                } else {
                    (have * s.amount_0 as u128 / real_before) as u64
                };
            } else {
                w.external = true;
            }
        }
        if amount == 0 {
            res.trades.push(TradeStatus::Skipped("nothing to sell"));
            continue;
        }

        let input = SwapInput {
            trade_direction: s.trade_direction,
            swap_mode: s.swap_mode,
            amount_0: amount,
            has_referral: s.has_referral,
            slot: s.slot,
            timestamp: s.event_timestamp,
            in_creation_tx: s.signature == created_sig,
        };
        let mut trial = pool;
        match simulate_swap(&mut trial, config, &input) {
            Ok(r) => {
                pool = trial;
                if is_buy {
                    w.quote_in += r.included_fee_input_amount as u128;
                    w.base += r.output_amount as i128;
                } else {
                    w.base -= r.included_fee_input_amount as i128;
                    w.quote_out += r.output_amount as u128;
                }
                w.trades += 1;

                let dir = if is_buy { TradeDirection::QuoteToBase } else { TradeDirection::BaseToQuote };
                let fee_mode = FeeMode::get_fee_mode(config.collect_fee_mode, dir, s.has_referral)
                    .map_err(|e| anyhow!("{e:?}"))?;
                let k = if fee_mode.fees_on_base_token { raw_price(r.next_sqrt_price) } else { 1.0 };
                if fee_mode.fees_on_base_token {
                    res.trading_fee_base += r.trading_fee as u128;
                    res.protocol_fee_base += r.protocol_fee as u128;
                } else {
                    res.trading_fee_quote += r.trading_fee as u128;
                    res.protocol_fee_quote += r.protocol_fee as u128;
                }
                res.trading_fee_equiv += r.trading_fee as f64 * k;
                res.protocol_fee_equiv += r.protocol_fee as f64 * k;
                if pool.is_curve_complete(config.migration_quote_threshold) {
                    res.completed_at = Some(i);
                }
                res.trades.push(TradeStatus::Done(r));
            }
            Err(e) => res.trades.push(TradeStatus::Error(format!("{e:#}"))),
        }
    }
    res.final_pool = pool;
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{initial_pool, PoolInit};
    use anchor_lang::prelude::Pubkey;

    /// Снайперы покупают в первые периоды и продают позже. При высокой стартовой
    /// комиссии их прибыль должна упасть, а базовый прогон — совпасть с "реальностью".
    /// DBC_FIXTURE_CONFIG=/путь/к/fee_in_quote_config.bin
    #[test]
    fn schedule_hurts_early_buyers() {
        let Ok(path) = std::env::var("DBC_FIXTURE_CONFIG") else {
            eprintln!("DBC_FIXTURE_CONFIG not set, skipping");
            return;
        };
        let raw = std::fs::read(path).unwrap();
        let size = std::mem::size_of::<PoolConfig>();
        let mut config: PoolConfig = bytemuck::pod_read_unaligned(&raw[8..8 + size]);
        // исходный конфиг делаем плоским 0.25%, как у реального пула 28VR
        config.pool_fees.base_fee.cliff_fee_numerator = 2_500_000;
        config.pool_fees.base_fee.first_factor = 0;
        config.pool_fees.base_fee.second_factor = 0;
        config.pool_fees.base_fee.third_factor = 0;
        config.pool_fees.base_fee.base_fee_mode = 0;

        let init = PoolInit {
            pool: Pubkey::new_unique().to_string(),
            config: Pubkey::new_unique().to_string(),
            creator: Pubkey::new_unique().to_string(),
            base_mint: Pubkey::new_unique().to_string(),
            pool_type: 0,
            activation_point: 1_000,
            created_sig: "create".into(),
        };
        let slot_mode = config.activation_type == 0;
        let at = |p: u64| if slot_mode { (p, 7u64) } else { (7u64, p) };

        // (point, wallet, buy?, amount: quote для покупки / доля в % для продажи)
        let plan: Vec<(u64, &str, bool, u64)> = vec![
            (1_000, "sniperA", true, 200_000_000),
            (1_001, "sniperB", true, 150_000_000),
            (1_002, "sniperC", true, 100_000_000),
            (1_050, "retail1", true, 300_000_000),
            (1_050, "sniperA", false, 100),
            (1_051, "sniperB", false, 100),
            (1_052, "sniperC", false, 50),
            (1_080, "retail2", true, 120_000_000),
        ];
        let mut pool = initial_pool(&config, &init).unwrap();
        let mut held: BTreeMap<&str, u64> = BTreeMap::new();
        let mut rec = Vec::new();
        for (n, (p, w, buy, amt)) in plan.iter().enumerate() {
            let (slot, ts) = at(*p);
            let amount = if *buy { *amt } else { held[w] * *amt / 100 };
            let input = SwapInput {
                trade_direction: if *buy { 1 } else { 0 },
                swap_mode: 0, amount_0: amount, has_referral: false,
                slot, timestamp: ts, in_creation_tx: false,
            };
            let r = simulate_swap(&mut pool, &config, &input).unwrap();
            let h = held.entry(w).or_insert(0);
            if *buy { *h += r.output_amount } else { *h -= r.included_fee_input_amount }
            rec.push(RecordedSwap {
                signature: format!("s{n}"), event_index: 0, slot, event_timestamp: ts,
                fee_payer: w.to_string(), trade_direction: input.trade_direction,
                has_referral: false, swap_mode: 0, amount_0: amount, result: r,
                quote_reserve_amount: pool.quote_reserve,
            });
        }

        let start = initial_pool(&config, &init).unwrap();
        let real = run(&config, start, &rec, "create").unwrap();
        for (t, s) in real.trades.iter().zip(&rec) {
            assert!(matches!(t, TradeStatus::Done(r) if *r == s.result), "baseline must match");
        }

        let sched = FeeSchedule { mode: 1, start_pct: 50.0, end_pct: 0.25, periods: 20, period_len: 1 };
        let alt_cfg = with_schedule(&config, &sched).unwrap();
        let alt = run(&alt_cfg, start, &rec, "create").unwrap();

        let pnl = |r: &RunResult, w: &str| r.wallets[w].realized();
        for w in ["sniperA", "sniperB"] {
            assert!(pnl(&alt, w) < pnl(&real, w), "{w}: cf {} !< real {}", pnl(&alt, w), pnl(&real, w));
            // продали всё и в симуляции
            assert_eq!(alt.wallets[w].base, 0);
        }
        assert!(alt.trading_fee_equiv > real.trading_fee_equiv);
        eprintln!(
            "sniperA real {} cf {} | fees real {:.0} cf {:.0}",
            pnl(&real, "sniperA"), pnl(&alt, "sniperA"), real.trading_fee_equiv, alt.trading_fee_equiv
        );
    }
}
