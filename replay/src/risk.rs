//! Оценка риска конфига DBC.
//!
//! Две группы признаков:
//!   1. Из самого аккаунта конфига — известны ДО первой покупки:
//!      кому и в каком виде достаётся ликвидность после миграции, сколько токенов
//!      уходит leftover receiver, комиссия миграции, права на токен.
//!   2. Из поведения пулов этого конфига в базе сборщика:
//!      сколько создателей им пользуются, доля стартовой покупки создателя,
//!      кошельки, продающие токены, которых они не покупали в пуле.
//!
//! Баллы — эвристика, а не вердикт. Каждый флаг печатается с объяснением,
//! чтобы читатель видел, из чего сложилась оценка.

use anyhow::Result;
use dynamic_bonding_curve::state::{LiquidityVestingInfo, PoolConfig};
use rusqlite::{params, Connection};

#[derive(Debug, Clone)]
pub struct LpSplit {
    /// % ликвидности, которую можно вывести сразу после миграции
    pub creator_unlocked: u8,
    pub partner_unlocked: u8,
    /// % навсегда заблокировано
    pub locked: u8,
    /// % с вестингом
    pub creator_vest: u8,
    pub partner_vest: u8,
    /// через сколько секунд после миграции вестинг полностью разблокирован (максимум из двух)
    pub vest_full_release_secs: u64,
}

#[derive(Debug, Clone)]
pub struct ConfigFacts {
    pub lp: LpSplit,
    pub total_supply: u64,
    pub curve_supply: u64,
    pub leftover_to_receiver: u64,
    pub update_authority: u8,
    pub migration_fee_pct: u8,
    pub has_fee_scheduler: bool,
    pub threshold: u64,
    pub token_decimal: u8,
    /// программа transfer hook (у конфигов ConfigWithTransferHook)
    pub transfer_hook: Option<String>,
    /// порог миграции ~0 при quote = SOL: кривой как рынка нет, токен сразу уходит в DAMM v2
    pub instant_graduation: bool,
    /// базовая торговая комиссия: (режим 0 линейный / 1 экспоненциальный / 2 ограничитель,
    /// начальный числитель из 1e9, число периодов, длина периода, снижение за период)
    pub base_fee: (u8, u64, u16, u64, u64),
    /// 0 — периоды в слотах, 1 — в секундах
    pub activation_type: u8,
    /// доля создателя в торговой комиссии после доли протокола (остальное — партнёру)
    pub creator_trading_fee_pct: u8,
    pub fee_claimer: Option<String>,
    pub leftover_receiver: Option<String>,
    /// во скольких конфигах базы этот адрес — получатель остатка (>= PLATFORM_MIN_CONFIGS: адрес платформы)
    pub leftover_receiver_configs: usize,
}

/// Адрес, получающий остаток в стольких конфигах, считаем служебным адресом платформы.
pub const PLATFORM_MIN_CONFIGS: usize = 10;
/// Порог миграции ниже 0,1 SOL — «мгновенная graduation».
pub const INSTANT_THRESHOLD_LAMPORTS: u64 = 100_000_000;
const WSOL: &str = "So11111111111111111111111111111111111111112";

fn vest_release(v: &LiquidityVestingInfo) -> u64 {
    if v.is_initialized == 0 || v.vesting_percentage == 0 {
        return 0;
    }
    v.cliff_duration_from_migration_time as u64 + v.number_of_periods as u64 * v.frequency as u64
}

pub fn config_facts(c: &PoolConfig) -> ConfigFacts {
    let pv = c.partner_liquidity_vesting_info.vesting_percentage;
    let cv = c.creator_liquidity_vesting_info.vesting_percentage;
    // Как в get_liquidity_distribution программы: доля создателя — остаток.
    let creator_unlocked = 100u8
        .saturating_sub(c.partner_liquidity_percentage)
        .saturating_sub(c.partner_permanent_locked_liquidity_percentage)
        .saturating_sub(pv)
        .saturating_sub(c.creator_permanent_locked_liquidity_percentage)
        .saturating_sub(cv);
    let lp = LpSplit {
        creator_unlocked,
        partner_unlocked: c.partner_liquidity_percentage,
        locked: c.partner_permanent_locked_liquidity_percentage + c.creator_permanent_locked_liquidity_percentage,
        creator_vest: cv,
        partner_vest: pv,
        vest_full_release_secs: vest_release(&c.partner_liquidity_vesting_info)
            .max(vest_release(&c.creator_liquidity_vesting_info)),
    };

    let lv = &c.locked_vesting_config;
    let locked_tokens = lv
        .cliff_unlock_amount
        .saturating_add(lv.amount_per_period.saturating_mul(lv.number_of_period));
    let used = c.swap_base_amount.saturating_add(c.migration_base_threshold).saturating_add(locked_tokens);

    // Фиксированный supply: после миграции остаток сверх post_migration_token_supply сжигается,
    // а то, что остаётся в пределах post supply, получает leftover_receiver.
    // Нефиксированный supply: весь остаток сжигается.
    let (total_supply, leftover_to_receiver) = if c.fixed_token_supply_flag == 1 {
        (c.post_migration_token_supply, c.post_migration_token_supply.saturating_sub(used))
    } else {
        (c.get_initial_base_supply().unwrap_or(used), 0)
    };

    let bf = &c.pool_fees.base_fee;
    ConfigFacts {
        lp,
        total_supply,
        curve_supply: c.swap_base_amount,
        leftover_to_receiver,
        update_authority: c.token_update_authority,
        migration_fee_pct: c.migration_fee_percentage,
        has_fee_scheduler: bf.base_fee_mode <= 1 && bf.first_factor > 0 && bf.second_factor > 0,
        base_fee: (bf.base_fee_mode, bf.cliff_fee_numerator, bf.first_factor, bf.second_factor, bf.third_factor),
        activation_type: c.activation_type,
        creator_trading_fee_pct: c.creator_trading_fee_percentage,
        threshold: c.migration_quote_threshold,
        token_decimal: c.token_decimal,
        transfer_hook: None,
        instant_graduation: c.quote_mint.to_string() == WSOL && c.migration_quote_threshold < INSTANT_THRESHOLD_LAMPORTS,
        fee_claimer: Some(c.fee_claimer.to_string()),
        leftover_receiver: Some(c.leftover_receiver.to_string()),
        leftover_receiver_configs: 1,
    }
}

#[derive(Debug, Clone, Default)]
pub struct Behavior {
    pub pools: u64,
    pub creators: u64,
    pub top_creator: Option<String>,
    pub top_creator_pools: u64,
    pub tracked: u64,
    pub graduated: u64,
    /// медиана стартовой покупки создателя в % от порога миграции (по отслеживаемым пулам)
    pub median_prebuy_pct: Option<f64>,
    /// медиана числа кошельков, продававших токены, которых не покупали в пуле
    pub median_outside_wallets: Option<f64>,
    /// медиана SOL, полученных такими кошельками
    pub median_outside_sol: Option<f64>,
    /// кошельки фермы и синтетичность активности (признаки 4, 7–11)
    pub farm: Option<crate::farm::FarmStats>,
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
}

pub fn behavior(conn: &Connection, config: &str, threshold: u64) -> Result<Behavior> {
    let mut b = Behavior::default();
    let (pools, creators): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COUNT(DISTINCT creator) FROM pools WHERE config = ?1",
        params![config],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    b.pools = pools as u64;
    b.creators = creators as u64;
    if let Ok((c, n)) = conn.query_row(
        "SELECT creator, COUNT(*) n FROM pools WHERE config = ?1 GROUP BY creator ORDER BY n DESC LIMIT 1",
        params![config],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
    ) {
        b.top_creator = Some(c);
        b.top_creator_pools = n as u64;
    }

    let mut st = conn.prepare(
        "SELECT p.pool,
           (SELECT COALESCE(SUM(s.included_fee_input_amount), 0) FROM swaps s
             WHERE s.pool = p.pool AND s.trade_direction = 1
               AND (s.fee_payer = p.creator OR s.signature = p.created_sig)
               AND s.slot <= p.created_slot + 2) AS prebuy,
           EXISTS(SELECT 1 FROM curve_complete c WHERE c.pool = p.pool) AS grad,
           (SELECT COUNT(*) FROM swaps s WHERE s.pool = p.pool) AS n_swaps
         FROM pools p WHERE p.config = ?1 AND p.tracked = 1",
    )?;
    let rows: Vec<(String, i64, bool, i64)> = st
        .query_map(params![config], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<std::result::Result<_, _>>()?;

    let mut prebuy = Vec::new();
    let mut ow = Vec::new();
    let mut osol = Vec::new();
    let mut outside = conn.prepare(
        "SELECT COUNT(DISTINCT s.fee_payer), COALESCE(SUM(s.output_amount), 0), COALESCE(SUM(s.included_fee_input_amount), 0)
         FROM swaps s JOIN pools p ON p.pool = s.pool
         WHERE s.pool = ?1 AND s.trade_direction = 0 AND s.fee_payer <> p.creator
           AND s.fee_payer NOT IN (SELECT fee_payer FROM swaps b WHERE b.pool = ?1 AND b.trade_direction = 1)",
    )?;
    // сколько токенов купил создатель: раздать на кривой он может не больше
    let mut creator_bought = conn.prepare(
        "SELECT COALESCE(SUM(s.output_amount), 0) FROM swaps s JOIN pools p ON p.pool = s.pool
         WHERE s.pool = ?1 AND s.trade_direction = 1 AND s.fee_payer = p.creator",
    )?;
    for (pool, pb, grad, n_swaps) in rows {
        if n_swaps == 0 {
            continue; // пул ещё не торговался или данные не собраны
        }
        b.tracked += 1;
        if grad {
            b.graduated += 1;
        }
        if threshold > 0 {
            prebuy.push(100.0 * pb as f64 / threshold as f64);
        }
        let (w, sol, tokens): (i64, i64, i64) = outside.query_row(params![pool], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let bought: i64 = creator_bought.query_row(params![pool], |r| r.get(0))?;
        // продавцы без покупки продали больше, чем купил создатель: раздача не от оператора
        // (например, снайпер раздал купленное своим кошелькам) — не считаем её признаком
        if (tokens as i128) * 100 > (bought as i128) * 105 {
            ow.push(0.0);
            osol.push(0.0);
        } else {
            ow.push(w as f64);
            osol.push(sol as f64 / 1e9);
        }
    }
    b.median_prebuy_pct = median(prebuy);
    b.median_outside_wallets = median(ow);
    b.median_outside_sol = median(osol);
    Ok(b)
}

#[derive(Debug, Clone)]
pub struct Flag {
    pub points: u32,
    pub text: String,
}

/// Итог по конфигу: две оси и вердикт.
#[derive(Debug, Clone)]
pub struct Report {
    /// Что конфиг ПОЗВОЛЯЕТ сделать (только аккаунт конфига), 0..100
    pub capability: u32,
    pub capability_flags: Vec<Flag>,
    /// Что РЕАЛЬНО наблюдалось в пулах этого конфига, 0..100; None — нет отслеживаемых пулов
    pub evidence: Option<u32>,
    pub evidence_flags: Vec<Flag>,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    /// Активность в пулах создают сам создатель и связанные с ним кошельки
    Synthetic,
    /// Сам конфиг чист по наблюдениям, но связан общими адресами с RED-конфигами
    RedLinked,
    /// Конфиг позволяет вывести ликвидность и сбросить крупный остаток supply
    RugCapable,
    /// Создатель сам проходит кривую целиком: покупателей на кривой нет
    SelfGraduation,
    Standard,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Synthetic => "RED",
            Verdict::RedLinked => "RED-LINK",
            Verdict::RugCapable => "AMBER",
            Verdict::SelfGraduation => "SELF-GRAD",
            Verdict::Standard => "GREEN",
        }
    }
    pub fn describe(self) -> &'static str {
        match self {
            Verdict::Synthetic => "synthetic launches: trading is dominated by the creator and wallets linked to it",
            Verdict::RedLinked => "shares operator addresses with configs where synthetic launches were observed",
            Verdict::RugCapable => "risky config: the creator or launchpad can take most of the buyers' SOL or liquidity at or after migration",
            Verdict::SelfGraduation => "instant / self-funded graduation: no real bonding-curve market; risk moves to the post-migration DAMM v2 pool",
            Verdict::Standard => "no major red flags in config or observed behaviour",
        }
    }
}

fn human_secs(s: u64) -> String {
    if s >= 86_400 {
        format!("{:.1} days", s as f64 / 86_400.0)
    } else if s >= 3_600 {
        format!("{:.1} h", s as f64 / 3_600.0)
    } else {
        format!("{s} s")
    }
}

/// Веса откалиброваны по 167 реальным конфигам (2 октября 2026):
///   * незаблокированная LP сама по себе — норма (шаблон Meteora Invent: partner 50% + creator 40% unlocked);
///   * 100% навсегда заблокированной LP — самый частый вариант (100 из 167 конфигов), продаж «извне» в нём не наблюдалось;
///   * связка «creator unlocked >= 50% + leftover >= 20% supply» — 51 конфиг, 78% наблюдаемых пулов;
///     продажи токенов «извне» видны в 13 из 32 таких конфигов с данными и ни в одном из остальных 27.
pub fn score(f: &ConfigFacts, b: &Behavior) -> Report {
    let lp = &f.lp;
    let leftover_pct = if f.total_supply > 0 { 100.0 * f.leftover_to_receiver as f64 / f.total_supply as f64 } else { 0.0 };

    // --- Возможности конфига ---
    let mut cap = Vec::new();
    let add = |v: &mut Vec<Flag>, points: u32, text: String| v.push(Flag { points, text });
    let creator_heavy = lp.creator_unlocked >= 50;
    let big_leftover = leftover_pct >= 20.0;
    let platform_custody = f.leftover_receiver_configs >= PLATFORM_MIN_CONFIGS;
    if creator_heavy && big_leftover && !platform_custody {
        add(&mut cap, 55, format!(
            "creator gets {}% of post-migration liquidity unlocked AND {:.0}% of supply goes to the leftover receiver: \
             liquidity can be pulled and leftover dumped right after migration",
            lp.creator_unlocked, leftover_pct
        ));
    } else {
        if creator_heavy {
            add(&mut cap, 20, format!("creator gets {}% of post-migration liquidity unlocked", lp.creator_unlocked));
        }
        if big_leftover && platform_custody {
            add(&mut cap, 5, format!(
                "{leftover_pct:.0}% of supply goes to platform address {} (leftover receiver in {} configs): \
                 safety depends on the platform's rules for these tokens",
                f.leftover_receiver.as_deref().unwrap_or("?"),
                f.leftover_receiver_configs
            ));
        } else if big_leftover {
            add(&mut cap, 20, format!("{leftover_pct:.0}% of supply goes to the leftover receiver after migration"));
        }
    }
    if f.instant_graduation {
        add(&mut cap, 0, format!(
            "migration threshold is ~0 ({:.4} SOL): no bonding-curve market, the token moves to DAMM v2 at once; \
             the liquidity and leftover risks above apply to buyers in that pool",
            f.threshold as f64 / 1e9
        ));
    }
    if lp.partner_unlocked >= 40 {
        add(&mut cap, 10, format!("launchpad (partner) gets {}% of post-migration liquidity unlocked", lp.partner_unlocked));
    }
    let vest = lp.creator_vest + lp.partner_vest;
    if vest > 0 && vest + lp.locked < 50 && lp.vest_full_release_secs <= 7 * 86_400 {
        add(&mut cap, 5, format!("vested {}% of liquidity fully unlocks {} after migration", vest, human_secs(lp.vest_full_release_secs)));
    }
    // Комиссия миграции забирает эту долю SOL, собранных на кривой: в пул после миграции уходит
    // только остаток (PoolConfig::get_migration_quote_amount). При 50%+ покупатели теряют
    // большую часть ликвидности в момент graduation — это само по себе рискованный конфиг.
    let fee = f.migration_fee_pct;
    if fee >= 10 {
        let points = match fee {
            50.. => 50,
            25..=49 => 25,
            _ => 10,
        };
        add(
            &mut cap,
            points,
            format!(
                "migration fee takes {fee}% of the SOL raised on the curve: only {}% goes into the post-migration pool",
                100 - fee as u32
            ),
        );
    }
    if let Some(h) = &f.transfer_hook {
        add(&mut cap, 20, format!(
            "token has a transfer hook ({h}): custom code runs on every transfer and could restrict selling; \
             legitimate for compliance/royalties, so check what the program does"
        ));
    }
    if matches!(f.update_authority, 3 | 4) {
        add(&mut cap, 40, "a creator or partner keeps MINT authority over the token".into());
    }
    let capability = cap.iter().map(|f| f.points).sum::<u32>().min(100);

    // --- Наблюдаемое поведение ---
    let mut ev = Vec::new();
    let self_grad = f.instant_graduation
        || matches!((b.median_prebuy_pct, b.median_outside_wallets), (Some(p), Some(w)) if p >= 95.0 && w < 1.0);
    if let Some(w) = b.median_outside_wallets {
        if w >= 10.0 {
            add(&mut ev, 30, format!(
                "fan-out: median {:.0} wallets per pool sold tokens they never bought in the pool ({:.2} SOL per pool)",
                w,
                b.median_outside_sol.unwrap_or(0.0)
            ));
        } else if w >= 3.0 {
            add(&mut ev, 10, format!("fan-out: median {w:.0} wallets per pool sold tokens they never bought in the pool"));
        }
    }
    if let Some(fs) = &b.farm {
        if let Some(sh) = fs.median_linked_share {
            let only_creator = fs.farm_wallets < 3 && fs.master_wallets == 0;
            if sh >= 0.8 && only_creator {
                // почти весь объём — сделки самого создателя: другие почти не торгуют
                // (брошенный запуск или продажа внешним), это не производство объёма фермой
                add(&mut ev, 20, format!(
                    "{:.0}% of trading volume (median per pool) comes from the creator itself: few other traders",
                    sh * 100.0
                ));
            } else if sh >= 0.8 {
                let masters = if fs.master_wallets > 0 {
                    format!(" and {} wallet(s) trading through proxy signers", fs.master_wallets)
                } else {
                    String::new()
                };
                add(&mut ev, 40, format!(
                    "{:.0}% of trading volume (median per pool) comes from the creator, {} recurring linked wallets{}",
                    sh * 100.0,
                    fs.farm_wallets,
                    masters
                ));
            } else if sh >= 0.5 {
                add(&mut ev, 20, format!("{:.0}% of trading volume (median per pool) comes from the creator and linked wallets", sh * 100.0));
            }
        }
        if let Some(px) = fs.median_proxied_share.filter(|x| *x >= 0.3) {
            add(
                &mut ev,
                0,
                format!(
                    "{:.0}% of curve volume (median per pool) is paid by a wallet other than the one signing the trade: one wallet trades through several proxy wallets",
                    px * 100.0
                ),
            );
        }
        if fs.volume_only_wallets >= 5 {
            add(
                &mut ev,
                0,
                format!(
                    "{} linked wallets also trade small amounts in unrelated pools (possible decoy activity); they are still counted as linked by volume",
                    fs.volume_only_wallets
                ),
            );
        }
        if fs.farm_wallets >= 5 && fs.median_linked_share.unwrap_or(0.0) < 0.8 {
            add(&mut ev, 10, format!("{} wallets trade in most of this config's pools and almost nowhere else", fs.farm_wallets));
        }
        if fs.first_buyers > 0 {
            add(&mut ev, 15, format!("{} wallet(s) buy in the first seconds of most pools of this config", fs.first_buyers));
        }
        // скриптовый старт: одинаковая первая покупка создателя или, если оператор начинает
        // с кошелька фермы, одинаковая первая покупка не от создателя; баллы — один раз
        if let Some((sol, share)) = fs.dev_buy_mode.filter(|(_, s)| *s >= 0.8) {
            add(
                &mut ev,
                15,
                format!("creator's opening buy is the same {} SOL in {:.0}% of pools", crate::farm::fmt_sol(sol), share * 100.0),
            );
        } else if let Some((sol, share)) = fs.first_buy_mode.filter(|(_, s)| *s >= 0.8) {
            add(
                &mut ev,
                15,
                format!(
                    "the first buy after launch (not by the creator) is the same {} SOL in {:.0}% of pools",
                    crate::farm::fmt_sol(sol),
                    share * 100.0
                ),
            );
        }
        if let Some(m) = fs.median_migration_secs {
            if m < 300.0 && !f.instant_graduation {
                add(
                    &mut ev,
                    10,
                    format!("the curve fills a median {m:.0} s after launch: no time for an open market, early buyers are bots or a bundle"),
                );
            }
        }
    }
    if b.creators == 1 && b.pools >= 5 {
        add(&mut ev, 20, format!("config used by a single creator ({} launches): launchpad and creator are likely the same party", b.pools));
    }
    if let Some(p) = b.median_prebuy_pct {
        if !self_grad && p >= 50.0 {
            add(&mut ev, 10, format!("creator's opening buy is {p:.0}% of the migration threshold (median)"));
        }
    }
    let evidence = if b.tracked > 0 { Some(ev.iter().map(|f| f.points).sum::<u32>().min(100)) } else { None };

    // RED требует сильного торгового доказательства: не меньше 80% объёма у связанных кошельков
    // или массовая раздача (10+ продавцов без покупок на пул). Сумма слабых признаков (около
    // половины связанного объёма, один создатель, одинаковая первая покупка) RED не даёт:
    // вердикт тогда определяется конфигом, а признаки остаются видны в отчёте.
    // 80% связанного объёма засчитывается, только если связаны не один создатель:
    // 3+ кошелька фермы или мастер-кошелёк с прокси (иначе это объём самого создателя).
    let strong_trading = b.median_outside_wallets.unwrap_or(0.0) >= 10.0
        || b.farm.as_ref().is_some_and(|fs| {
            fs.median_linked_share.unwrap_or(0.0) >= 0.8 && (fs.farm_wallets >= 3 || fs.master_wallets >= 1)
        });
    let verdict = if evidence.unwrap_or(0) >= 50 && strong_trading {
        Verdict::Synthetic
    } else if self_grad {
        Verdict::SelfGraduation
    } else if capability >= 50 {
        Verdict::RugCapable
    } else {
        Verdict::Standard
    };

    Report { capability, capability_flags: cap, evidence, evidence_flags: ev, verdict }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Конфиг, устроенный как 2toDxUnv (см. разбор xzy19…), должен получить CRITICAL,
    /// а тот же конфиг с заблокированной ликвидностью и без остатка — LOW.
    /// DBC_FIXTURE_CONFIG=/путь/к/fee_in_quote_config.bin ; RISK_DB=/tmp/risk.sqlite — сохранить тестовую базу.
    #[test]
    fn scores_rug_style_config_as_critical() {
        let Ok(path) = std::env::var("DBC_FIXTURE_CONFIG") else {
            eprintln!("DBC_FIXTURE_CONFIG not set, skipping");
            return;
        };
        let raw = std::fs::read(path).unwrap();
        let size = std::mem::size_of::<PoolConfig>();
        let mut c: PoolConfig = bytemuck::pod_read_unaligned(&raw[8..8 + size]);

        c.partner_liquidity_percentage = 0;
        c.partner_permanent_locked_liquidity_percentage = 0;
        c.creator_permanent_locked_liquidity_percentage = 0;
        c.creator_liquidity_percentage = 89;
        c.partner_liquidity_vesting_info.vesting_percentage = 0;
        c.creator_liquidity_vesting_info.is_initialized = 1;
        c.creator_liquidity_vesting_info.vesting_percentage = 11;
        c.creator_liquidity_vesting_info.cliff_duration_from_migration_time = 86_400;
        c.creator_liquidity_vesting_info.number_of_periods = 1;
        c.creator_liquidity_vesting_info.frequency = 1;
        c.fixed_token_supply_flag = 1;
        c.pre_migration_token_supply = 1_000_000_000_000_000;
        c.post_migration_token_supply = 1_000_000_000_000_000;
        c.swap_base_amount = 80_000_000_000_000;
        c.migration_base_threshold = 20_000_000_000_000;
        c.locked_vesting_config = Default::default();
        c.migration_quote_threshold = 11_510_000_000;
        c.token_update_authority = 1;

        let f = config_facts(&c);
        assert_eq!(f.lp.creator_unlocked, 89);
        assert_eq!(f.leftover_to_receiver, 900_000_000_000_000);

        // поведение: 6 пулов одного создателя, стартовая покупка 8.057 SOL, 20 продавцов-сателлитов
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE pools(pool, config, creator, base_mint, pool_type, activation_point, transfer_hook,
               created_sig, created_slot, created_time, discovered_at, tracked, last_sig, done, done_reason);
             CREATE TABLE swaps(signature, event_index, pool, config, slot, block_time, fee_payer, trade_direction,
               has_referral, swap_mode, amount_0, amount_1, included_fee_input_amount, excluded_fee_input_amount,
               amount_left, output_amount, next_sqrt_price, trading_fee, protocol_fee, referral_fee,
               quote_reserve_amount, migration_threshold, event_timestamp, transfer_hook);
             CREATE TABLE curve_complete(pool, config, signature, slot, block_time, base_reserve, quote_reserve);",
        )
        .unwrap();
        for p in 0..6 {
            let pool = format!("pool{p}");
            conn.execute(
                "INSERT INTO pools VALUES(?1,'cfg','creatorX','mint',0,100,0,?2,100,0,0,1,NULL,0,NULL)",
                params![pool, format!("create{p}")],
            )
            .unwrap();
            let ins = |sig: &str, payer: &str, dir: i64, input: i64, out: i64, slot: i64| {
                conn.execute(
                    "INSERT INTO swaps VALUES(?1,0,?2,'cfg',?3,0,?4,?5,0,0,0,0,?6,?6,0,?7,'0',0,0,0,0,0,0,0)",
                    params![sig, pool, slot, payer, dir, input, out],
                )
                .unwrap();
            };
            ins(&format!("create{p}"), "creatorX", 1, 8_057_000_000, 61_000_000_000_000, 100);
            for w in 0..20 {
                ins(&format!("s{p}-{w}"), &format!("sat{w}"), 0, 547_646_000_000, 150_000_000, 150);
            }
            ins(&format!("b{p}"), "retail", 1, 300_000_000, 700_000_000_000, 140);
            conn.execute("INSERT INTO curve_complete VALUES(?1,'cfg','x',200,0,0,0)", params![pool]).unwrap();
        }
        let b = behavior(&conn, "cfg", c.migration_quote_threshold).unwrap();
        assert_eq!((b.pools, b.creators, b.tracked, b.graduated), (6, 1, 6, 6));
        assert!((b.median_prebuy_pct.unwrap() - 70.0).abs() < 0.1);
        assert_eq!(b.median_outside_wallets, Some(20.0));

        let r = score(&f, &b);
        for fl in r.capability_flags.iter().chain(&r.evidence_flags) {
            eprintln!("+{} {}", fl.points, fl.text);
        }
        assert_eq!(r.verdict, Verdict::Synthetic);
        assert_eq!(r.capability, 60);

        // Слабые признаки (одинаковая первая покупка, один создатель) без сильного торгового
        // доказательства RED не дают, даже если баллов набралось 50+.
        let weak = Behavior {
            pools: 7,
            creators: 1,
            tracked: 3,
            median_outside_wallets: Some(0.0),
            farm: Some(crate::farm::FarmStats {
                median_linked_share: Some(0.55),
                dev_buy_mode: Some((3.0, 1.0)),
                first_buyers: 1,
                ..Default::default()
            }),
            ..Default::default()
        };
        let rw = score(&f, &weak);
        // почти весь объём — сделки создателя, ферм и мастеров нет: не RED
        let creator_only = Behavior {
            pools: 8,
            creators: 1,
            tracked: 1,
            median_outside_wallets: Some(0.0),
            farm: Some(crate::farm::FarmStats { median_linked_share: Some(0.99), ..Default::default() }),
            ..Default::default()
        };
        let rc = score(&f, &creator_only);
        assert_ne!(rc.verdict, Verdict::Synthetic, "creator's own trades in a pool nobody else trades are not synthetic volume");
        assert!(rc.evidence_flags.iter().any(|x| x.text.contains("creator itself")));
        assert!(rw.evidence.unwrap() >= 50, "weak signals add up: {:?}", rw.evidence);
        assert_eq!(rw.verdict, Verdict::RugCapable, "weak signals do not make RED; the risky config gives AMBER");

        // Тот же конфиг без наблюдений — только возможность.
        assert_eq!(score(&f, &Behavior { pools: 3, creators: 3, ..Default::default() }).verdict, Verdict::RugCapable);

        // Шаблон Meteora Invent по умолчанию: partner 50% + creator 40% unlocked, 10% locked, leftover 0.
        let mut invent = c;
        invent.partner_liquidity_percentage = 50;
        invent.creator_liquidity_percentage = 40;
        invent.partner_permanent_locked_liquidity_percentage = 5;
        invent.creator_permanent_locked_liquidity_percentage = 5;
        invent.creator_liquidity_vesting_info.vesting_percentage = 0;
        invent.post_migration_token_supply = 100_000_000_000_000;
        let inv = score(&config_facts(&invent), &Behavior { pools: 40, creators: 35, ..Default::default() });
        assert_eq!(inv.verdict, Verdict::Standard, "{:?}", inv.capability_flags);

        // Честный вариант: вся ликвидность навсегда заблокирована, остатка нет, конфиг общий.
        let mut honest = c;
        honest.creator_liquidity_percentage = 0;
        honest.creator_permanent_locked_liquidity_percentage = 50;
        honest.partner_permanent_locked_liquidity_percentage = 50;
        honest.creator_liquidity_vesting_info.vesting_percentage = 0;
        honest.post_migration_token_supply = 100_000_000_000_000;
        let hf = config_facts(&honest);
        assert_eq!(hf.lp.creator_unlocked, 0);
        let h = score(&hf, &Behavior { pools: 40, creators: 35, ..Default::default() });
        assert_eq!((h.verdict, h.capability), (Verdict::Standard, 0));

        // Мгновенная graduation распознаётся по конфигу, без наблюдений.
        let mut inst = c;
        inst.quote_mint = WSOL.parse().unwrap();
        inst.migration_quote_threshold = 1_000;
        let fi = config_facts(&inst);
        assert!(fi.instant_graduation);
        assert_eq!(score(&fi, &Behavior::default()).verdict, Verdict::SelfGraduation);

        // Остаток у адреса платформы (общий для многих конфигов): мягкий флаг вместо +55.
        let mut plat = config_facts(&honest);
        plat.leftover_to_receiver = plat.total_supply * 85 / 100;
        plat.leftover_receiver_configs = 38;
        let pr = score(&plat, &Behavior { pools: 38, creators: 1, ..Default::default() });
        assert_eq!((pr.verdict, pr.capability), (Verdict::Standard, 5), "{:?}", pr.capability_flags);
    }
}

/// Описание базовой комиссии для отчёта: начальная ставка, как и за сколько снижается, конечная.
pub fn describe_base_fee(f: &ConfigFacts) -> String {
    let (mode, cliff, n, freq, red) = f.base_fee;
    let pct = |num: f64| num / 1e7; // числитель из 1e9 -> проценты
    if !f.has_fee_scheduler {
        return format!("flat {:.2}%", pct(cliff as f64));
    }
    let end = match mode {
        0 => (cliff as f64 - n as f64 * red as f64).max(0.0),
        _ => cliff as f64 * (1.0 - red as f64 / 10_000.0).powi(n as i32),
    };
    let unit = if f.activation_type == 1 { "s" } else { "slots" };
    format!(
        "{} fee scheduler: starts at {:.2}%, falls {} over {} periods of {} {} ({} {} in total), ends at {:.2}%",
        if mode == 0 { "linear" } else { "exponential" },
        pct(cliff as f64),
        if mode == 0 { format!("by {:.3} pp per period", pct(red as f64)) } else { format!("by {:.2}% per period", red as f64 / 100.0) },
        n,
        freq,
        unit,
        n as u64 * freq,
        unit,
        pct(end)
    )
}
