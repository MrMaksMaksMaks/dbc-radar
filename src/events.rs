//! Декодирование событий DBC, которые программа эмитит через Anchor event CPI.
//!
//! Данные внутренней инструкции event CPI устроены так:
//!   [8 байт EVENT_IX_TAG] [8 байт дискриминатора события] [borsh-payload]
//!
//! Раскладки структур и дискриминаторы взяты из IDL dynamic_bonding_curve v0.2.1
//! (MeteoraAg/dynamic-bonding-curve-sdk, src/idl/dynamic-bonding-curve/idl.json).
//! При обновлении программы сверяйте их с актуальным IDL.

use anyhow::{bail, Result};

pub const DBC_PROGRAM_ID: &str = "dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN";

/// Anchor EVENT_IX_TAG (0x1d9acb512ea545e4) в little-endian.
pub const EVENT_IX_TAG: [u8; 8] = [0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d];

const D_INIT_POOL: [u8; 8] = [228, 50, 246, 85, 203, 66, 134, 37];
const D_INIT_POOL_TH: [u8; 8] = [213, 137, 164, 53, 193, 74, 15, 110];
const D_SWAP2: [u8; 8] = [189, 66, 51, 168, 38, 80, 117, 153];
const D_SWAP2_TH: [u8; 8] = [134, 59, 168, 120, 94, 51, 114, 231];
const D_CURVE_COMPLETE: [u8; 8] = [229, 231, 86, 84, 156, 134, 75, 24];
const D_CURVE_COMPLETE_TH: [u8; 8] = [59, 47, 109, 205, 13, 31, 44, 159];

#[derive(Debug, Clone)]
pub struct InitPool {
    pub pool: String,
    pub config: String,
    pub creator: String,
    pub base_mint: String,
    pub pool_type: u8,
    pub activation_point: u64,
    pub transfer_hook: bool,
}

#[derive(Debug, Clone)]
pub struct Swap2 {
    pub pool: String,
    pub config: String,
    /// 0 = base -> quote (продажа), 1 = quote -> base (покупка)
    pub trade_direction: u8,
    pub has_referral: bool,
    pub amount_0: u64,
    pub amount_1: u64,
    /// 0 = exact in, 1 = partial fill, 2 = exact out
    pub swap_mode: u8,
    pub included_fee_input_amount: u64,
    pub excluded_fee_input_amount: u64,
    pub amount_left: u64,
    pub output_amount: u64,
    pub next_sqrt_price: u128,
    pub trading_fee: u64,
    pub protocol_fee: u64,
    pub referral_fee: u64,
    pub quote_reserve_amount: u64,
    pub migration_threshold: u64,
    pub current_timestamp: u64,
    pub transfer_hook: bool,
}

#[derive(Debug, Clone)]
pub struct CurveComplete {
    pub pool: String,
    pub config: String,
    pub base_reserve: u64,
    pub quote_reserve: u64,
}

#[derive(Debug, Clone)]
pub enum DbcEvent {
    InitializePool(InitPool),
    Swap2(Swap2),
    CurveComplete(CurveComplete),
}

/// Пробует декодировать данные внутренней инструкции как событие DBC.
/// Возвращает Ok(None) для событий, которые нам не нужны (claims, metadata и т.д.).
pub fn decode_event_ix(data: &[u8]) -> Result<Option<DbcEvent>> {
    if data.len() < 16 || data[..8] != EVENT_IX_TAG {
        return Ok(None);
    }
    let disc: [u8; 8] = data[8..16].try_into().unwrap();
    let mut r = Reader::new(&data[16..]);

    let ev = match disc {
        D_INIT_POOL | D_INIT_POOL_TH => DbcEvent::InitializePool(InitPool {
            pool: r.pubkey()?,
            config: r.pubkey()?,
            creator: r.pubkey()?,
            base_mint: r.pubkey()?,
            pool_type: r.u8()?,
            activation_point: r.u64()?,
            transfer_hook: disc == D_INIT_POOL_TH,
        }),
        D_SWAP2 | D_SWAP2_TH => DbcEvent::Swap2(Swap2 {
            pool: r.pubkey()?,
            config: r.pubkey()?,
            trade_direction: r.u8()?,
            has_referral: r.bool()?,
            // SwapParameters2
            amount_0: r.u64()?,
            amount_1: r.u64()?,
            swap_mode: r.u8()?,
            // SwapResult2
            included_fee_input_amount: r.u64()?,
            excluded_fee_input_amount: r.u64()?,
            amount_left: r.u64()?,
            output_amount: r.u64()?,
            next_sqrt_price: r.u128()?,
            trading_fee: r.u64()?,
            protocol_fee: r.u64()?,
            referral_fee: r.u64()?,
            // хвост EvtSwap2
            quote_reserve_amount: r.u64()?,
            migration_threshold: r.u64()?,
            current_timestamp: r.u64()?,
            transfer_hook: disc == D_SWAP2_TH,
        }),
        D_CURVE_COMPLETE | D_CURVE_COMPLETE_TH => DbcEvent::CurveComplete(CurveComplete {
            pool: r.pubkey()?,
            config: r.pubkey()?,
            base_reserve: r.u64()?,
            quote_reserve: r.u64()?,
        }),
        _ => return Ok(None),
    };
    Ok(Some(ev))
}

/// Минимальный borsh-ридер (little-endian, без выравнивания).
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            bail!("event payload too short: need {} at {}, len {}", n, self.pos, self.buf.len());
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn bool(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn u128(&mut self) -> Result<u128> {
        Ok(u128::from_le_bytes(self.take(16)?.try_into().unwrap()))
    }
    fn pubkey(&mut self) -> Result<String> {
        Ok(bs58::encode(self.take(32)?).into_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(b: u8) -> [u8; 32] {
        [b; 32]
    }

    #[test]
    fn decodes_swap2() {
        let mut d = Vec::new();
        d.extend_from_slice(&EVENT_IX_TAG);
        d.extend_from_slice(&D_SWAP2);
        d.extend_from_slice(&pk(1)); // pool
        d.extend_from_slice(&pk(2)); // config
        d.push(1); // trade_direction = buy
        d.push(0); // has_referral
        d.extend_from_slice(&1_000u64.to_le_bytes()); // amount_0
        d.extend_from_slice(&900u64.to_le_bytes()); // amount_1
        d.push(0); // swap_mode
        d.extend_from_slice(&1_000u64.to_le_bytes()); // included_fee_input
        d.extend_from_slice(&990u64.to_le_bytes()); // excluded_fee_input
        d.extend_from_slice(&0u64.to_le_bytes()); // amount_left
        d.extend_from_slice(&5_000u64.to_le_bytes()); // output
        d.extend_from_slice(&(1u128 << 64).to_le_bytes()); // next_sqrt_price
        d.extend_from_slice(&8u64.to_le_bytes()); // trading_fee
        d.extend_from_slice(&2u64.to_le_bytes()); // protocol_fee
        d.extend_from_slice(&0u64.to_le_bytes()); // referral_fee
        d.extend_from_slice(&123u64.to_le_bytes()); // quote_reserve_amount
        d.extend_from_slice(&456u64.to_le_bytes()); // migration_threshold
        d.extend_from_slice(&1_700_000_000u64.to_le_bytes()); // ts

        match decode_event_ix(&d).unwrap().unwrap() {
            DbcEvent::Swap2(s) => {
                assert_eq!(s.trade_direction, 1);
                assert_eq!(s.output_amount, 5_000);
                assert_eq!(s.next_sqrt_price, 1u128 << 64);
                assert_eq!(s.migration_threshold, 456);
                assert_eq!(s.current_timestamp, 1_700_000_000);
                assert_eq!(s.pool, bs58::encode(pk(1)).into_string());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn ignores_non_event_data() {
        assert!(decode_event_ix(&[0u8; 40]).unwrap().is_none());
    }

    #[test]
    fn short_payload_is_error() {
        let mut d = Vec::new();
        d.extend_from_slice(&EVENT_IX_TAG);
        d.extend_from_slice(&D_CURVE_COMPLETE);
        d.extend_from_slice(&[0u8; 10]);
        assert!(decode_event_ix(&d).is_err());
    }
}
