//! Движок повтора: прогоняет записанные свопы через математику самой программы DBC.
//!
//! Повторяет порядок действий из `process_swap` программы:
//!   update_pre_swap -> get_swap_result_* -> apply_swap_result
//! Поэтому при правильном конфиге и порядке сделок результат должен совпадать
//! с событиями EvtSwap2 до последнего лампорта.

use anchor_lang::prelude::Pubkey;
use anyhow::{anyhow, bail, Result};
use dynamic_bonding_curve::{
    activation_handler::ActivationType,
    constants::fee::PROTOCOL_LIQUIDITY_MIGRATION_FEE_BPS,
    params::swap::TradeDirection,
    state::{
        fee::{FeeMode, VolatilityTracker},
        PoolConfig, PoolState, SwapResult2,
    },
};
use std::str::FromStr;

/// Параметры создания пула (из EvtInitializePool).
#[derive(Debug, Clone)]
pub struct PoolInit {
    pub pool: String,
    pub config: String,
    pub creator: String,
    pub base_mint: String,
    pub pool_type: u8,
    pub activation_point: u64,
    pub created_sig: String,
}

/// Своп, как он записан сборщиком (поля EvtSwap2 + контекст транзакции).
#[derive(Debug, Clone)]
pub struct RecordedSwap {
    pub signature: String,
    pub event_index: u32,
    pub slot: u64,
    pub event_timestamp: u64,
    pub fee_payer: String,
    pub trade_direction: u8,
    pub has_referral: bool,
    pub swap_mode: u8,
    pub amount_0: u64,
    pub result: SwapResult2,
    pub quote_reserve_amount: u64,
}

fn anchor_err(e: anchor_lang::error::Error) -> anyhow::Error {
    anyhow!("{e:?}")
}

fn pk(s: &str) -> Result<Pubkey> {
    Pubkey::from_str(s).map_err(|e| anyhow!("bad pubkey {s}: {e}"))
}

/// Состояние пула сразу после создания — так же, как его заполняет инструкция
/// initialize_virtual_pool_with_spl_token.
pub fn initial_pool(config: &PoolConfig, init: &PoolInit) -> Result<PoolState> {
    let mut pool = PoolState::default();
    pool.initialize(
        VolatilityTracker::default(),
        pk(&init.config)?,
        pk(&init.creator)?,
        pk(&init.base_mint)?,
        Pubkey::default(), // vault-адреса на математику не влияют
        Pubkey::default(),
        config.sqrt_start_price,
        init.pool_type,
        init.activation_point,
        config.get_initial_base_supply().map_err(anchor_err)?,
        PROTOCOL_LIQUIDITY_MIGRATION_FEE_BPS,
    );
    Ok(pool)
}

/// Входные параметры одной сделки — то, что задаёт трейдер.
#[derive(Debug, Clone, Copy)]
pub struct SwapInput {
    pub trade_direction: u8, // 0 = base->quote (продажа), 1 = quote->base (покупка)
    pub swap_mode: u8,       // 0 exact in, 1 partial fill, 2 exact out
    pub amount_0: u64,
    pub has_referral: bool,
    pub slot: u64,
    pub timestamp: u64,
    pub in_creation_tx: bool,
}

impl From<&RecordedSwap> for SwapInput {
    fn from(s: &RecordedSwap) -> Self {
        Self {
            trade_direction: s.trade_direction,
            swap_mode: s.swap_mode,
            amount_0: s.amount_0,
            has_referral: s.has_referral,
            slot: s.slot,
            timestamp: s.event_timestamp,
            in_creation_tx: false,
        }
    }
}

fn direction(d: u8) -> Result<TradeDirection> {
    TradeDirection::try_from(d).map_err(|_| anyhow!("bad trade_direction {d}"))
}

/// Считает результат сделки на `pool` и применяет его (мутирует `pool`).
pub fn simulate_swap(pool: &mut PoolState, config: &PoolConfig, input: &SwapInput) -> Result<SwapResult2> {
    if pool.is_curve_complete(config.migration_quote_threshold) {
        bail!("curve already complete");
    }
    let dir = direction(input.trade_direction)?;
    let activation = ActivationType::try_from(config.activation_type)
        .map_err(|_| anyhow!("bad activation_type"))?;
    let current_point = match activation {
        ActivationType::Slot => input.slot,
        ActivationType::Timestamp => input.timestamp,
    };
    let eligible_first =
        config.is_first_swap_with_min_fee_enabled() && pool.is_first_swap() && input.in_creation_tx;

    pool.update_pre_swap(config, input.timestamp).map_err(anchor_err)?;
    let fee_mode = FeeMode::get_fee_mode(config.collect_fee_mode, dir, input.has_referral)
        .map_err(anchor_err)?;

    let r = match input.swap_mode {
        0 => pool.get_swap_result_from_exact_input(config, input.amount_0, &fee_mode, dir, current_point, eligible_first),
        1 => pool.get_swap_result_from_partial_input(config, input.amount_0, &fee_mode, dir, current_point, eligible_first),
        2 => pool.get_swap_result_from_exact_output(config, input.amount_0, &fee_mode, dir, current_point, eligible_first),
        m => bail!("unknown swap_mode {m}"),
    }
    .map_err(anchor_err)?;

    pool.apply_swap_result(config, &r.get_swap_result(), &fee_mode, dir, input.timestamp)
        .map_err(anchor_err)?;
    Ok(r)
}

/// Применяет к пулу *записанный* результат (для синхронизации состояния с onchain).
fn apply_recorded(pool: &mut PoolState, config: &PoolConfig, s: &RecordedSwap) -> Result<()> {
    let dir = direction(s.trade_direction)?;
    pool.update_pre_swap(config, s.event_timestamp).map_err(anchor_err)?;
    let fee_mode = FeeMode::get_fee_mode(config.collect_fee_mode, dir, s.has_referral)
        .map_err(anchor_err)?;
    pool.apply_swap_result(config, &s.result.get_swap_result(), &fee_mode, dir, s.event_timestamp)
        .map_err(anchor_err)
}

/// Список полей, в которых симуляция разошлась с записью.
pub fn diff(sim: &SwapResult2, rec: &SwapResult2) -> Vec<String> {
    let mut d = Vec::new();
    macro_rules! cmp {
        ($f:ident) => {
            if sim.$f != rec.$f {
                d.push(format!("{}: sim {} != rec {}", stringify!($f), sim.$f, rec.$f));
            }
        };
    }
    cmp!(included_fee_input_amount);
    cmp!(excluded_fee_input_amount);
    cmp!(amount_left);
    cmp!(output_amount);
    cmp!(next_sqrt_price);
    cmp!(trading_fee);
    cmp!(protocol_fee);
    cmp!(referral_fee);
    d
}

#[derive(Debug)]
pub struct Outcome {
    pub swap: RecordedSwap,
    pub sim: Result<SwapResult2, String>,
    pub diffs: Vec<String>,
    pub reserve_ok: bool,
}

impl Outcome {
    pub fn matched(&self) -> bool {
        self.sim.is_ok() && self.diffs.is_empty() && self.reserve_ok
    }
}

/// Проверочный повтор с исходным конфигом.
///
/// Порядок сделок внутри одного слота RPC не сообщает, поэтому внутри слота
/// жадно выбираем ту сделку, симуляция которой совпадает с записью.
/// После каждой сделки состояние синхронизируется по записанному результату,
/// так что одна ошибка не тянет за собой все следующие.
pub fn validate(config: &PoolConfig, init: &PoolInit, mut swaps: Vec<RecordedSwap>) -> Result<Vec<Outcome>> {
    let mut pool = initial_pool(config, init)?;
    swaps.sort_by(|a, b| (a.slot, &a.signature, a.event_index).cmp(&(b.slot, &b.signature, b.event_index)));

    let mut out = Vec::with_capacity(swaps.len());
    let mut i = 0;
    while i < swaps.len() {
        let slot = swaps[i].slot;
        let mut group: Vec<RecordedSwap> = swaps[i..].iter().take_while(|s| s.slot == slot).cloned().collect();
        i += group.len();

        while !group.is_empty() {
            // Ищем первую сделку группы, которая точно совпадает при текущем состоянии.
            let mut pick = None;
            for (k, s) in group.iter().enumerate() {
                let mut trial = pool;
                let mut input = SwapInput::from(s);
                input.in_creation_tx = s.signature == init.created_sig;
                if let Ok(r) = simulate_swap(&mut trial, config, &input) {
                    if diff(&r, &s.result).is_empty() && trial.quote_reserve == s.quote_reserve_amount {
                        pick = Some(k);
                        break;
                    }
                }
            }
            let k = pick.unwrap_or(0);
            let s = group.remove(k);

            let mut trial = pool;
            let mut input = SwapInput::from(&s);
            input.in_creation_tx = s.signature == init.created_sig;
            let sim = simulate_swap(&mut trial, config, &input);
            let (diffs, reserve_ok) = match &sim {
                Ok(r) => (diff(r, &s.result), trial.quote_reserve == s.quote_reserve_amount),
                Err(_) => (vec![], false),
            };

            apply_recorded(&mut pool, config, &s)?;
            out.push(Outcome {
                sim: sim.map_err(|e| format!("{e:#}")),
                diffs,
                reserve_ok,
                swap: s,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Самосогласованность: генерируем сделки симуляцией, перемешиваем их внутри
    /// одного слота и проверяем, что validate восстанавливает порядок и всё совпадает.
    /// Нужен файл конфига: DBC_FIXTURE_CONFIG=/путь/к/fee_in_quote_config.bin
    /// (лежит в репозитории dynamic-bonding-curve: dynamic-bonding-curve-sdk/fixtures).
    #[test]
    fn replay_is_self_consistent() {
        let Ok(path) = std::env::var("DBC_FIXTURE_CONFIG") else {
            eprintln!("DBC_FIXTURE_CONFIG not set, skipping");
            return;
        };
        let raw = std::fs::read(path).unwrap();
        let size = std::mem::size_of::<PoolConfig>();
        let config: PoolConfig = bytemuck::pod_read_unaligned(&raw[8..8 + size]);

        let init = PoolInit {
            pool: Pubkey::new_unique().to_string(),
            config: Pubkey::new_unique().to_string(),
            creator: Pubkey::new_unique().to_string(),
            base_mint: Pubkey::new_unique().to_string(),
            pool_type: 0,
            activation_point: 1_000,
            created_sig: "create".into(),
        };
        let activation_is_slot = config.activation_type == 0;
        let mut pool = initial_pool(&config, &init).unwrap();

        // 6 покупок разного размера и 2 продажи, все в одном слоте
        let plan: [(u8, u64); 8] = [
            (1, 50_000_000), (1, 120_000_000), (1, 10_000_000), (0, 0),
            (1, 300_000_000), (1, 7_000_000), (0, 0), (1, 90_000_000),
        ];
        let mut recorded = Vec::new();
        let mut bought: u64 = 0;
        for (n, (dir, amt)) in plan.iter().enumerate() {
            let amount = if *dir == 0 { bought / 3 } else { *amt };
            let input = SwapInput {
                trade_direction: *dir,
                swap_mode: 0,
                amount_0: amount,
                has_referral: false,
                slot: if activation_is_slot { 1_000 } else { 5 },
                timestamp: if activation_is_slot { 5 } else { 1_000 },
                in_creation_tx: false,
            };
            let r = simulate_swap(&mut pool, &config, &input).unwrap();
            if *dir == 1 { bought += r.output_amount } else { bought -= amount }
            recorded.push(RecordedSwap {
                signature: format!("sig{}", 9 - n), // имена не совпадают с реальным порядком
                event_index: 0,
                slot: input.slot,
                event_timestamp: input.timestamp,
                fee_payer: "w".into(),
                trade_direction: *dir,
                has_referral: false,
                swap_mode: 0,
                amount_0: amount,
                result: r,
                quote_reserve_amount: pool.quote_reserve,
            });
        }
        recorded.reverse();
        let outcomes = validate(&config, &init, recorded).unwrap();
        for o in &outcomes {
            assert!(o.matched(), "{:?} {:?}", o.sim, o.diffs);
        }
    }
}
