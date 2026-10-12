//! Grouping configs: who is behind them and which template they are built from.
//!
//! Two independent signals:
//!   * TEMPLATE — a fingerprint of the config parameters: all economic fields of PoolConfig
//!     except addresses (fee_claimer, leftover_receiver). The same fingerprint means
//!     "the same rules", even if the configs were created by different addresses. A template can
//!     also be used by unrelated people (e.g. a ready-made launch tool), so
//!     on its own it does NOT merge configs.
//!   * OPERATOR — links through addresses that are hard or expensive to change:
//!     a shared fee receiver, a shared leftover receiver, a shared pool creator,
//!     shared satellite wallets (they sell tokens they never bought in the pool).
//!     Configs linked by at least one such link are merged into a cluster.

use anyhow::Result;
use dynamic_bonding_curve::state::PoolConfig;
use rusqlite::{params, Connection};
use std::collections::{BTreeMap, HashMap, HashSet};

/// How many shared satellites are needed to link two configs.
/// One or two shared addresses can be a coincidence (a common trading bot); three or more cannot.
pub const MIN_SHARED_SATELLITES: usize = 3;

/// Addresses that never link configs: burn and the system program.
const NEVER_LINK: [&str; 2] = ["1nc1nerator11111111111111111111111111111111", "11111111111111111111111111111111"];

#[derive(Debug, Clone)]
pub struct ConfigInfo {
    #[allow(dead_code)]
    pub config: String,
    pub fee_claimer: Option<String>,
    pub leftover_receiver: Option<String>,
    pub template: String,
    pub creators: Vec<String>,
    pub satellites: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LinkKind {
    FeeClaimer,
    LeftoverReceiver,
    Creator,
    Satellites,
}

impl LinkKind {
    pub fn label(self) -> &'static str {
        match self {
            LinkKind::FeeClaimer => "fee claimer",
            LinkKind::LeftoverReceiver => "leftover receiver",
            LinkKind::Creator => "pool creator",
            LinkKind::Satellites => "satellite wallets",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SharedAddress {
    pub kind: LinkKind,
    pub address: String,
    pub configs: usize,
}

#[derive(Debug, Clone)]
pub struct Cluster {
    pub id: usize,
    /// indices in the source ConfigInfo list
    pub members: Vec<usize>,
    pub links: BTreeMap<LinkKind, usize>,
    pub shared: Vec<SharedAddress>,
}

/// FNV-1a 64: stable across runs and Rust versions, unlike DefaultHasher.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Parameter fingerprint: the PoolConfig body without the fee_claimer and leftover_receiver addresses,
/// plus a flag for whether there is a transfer hook (the hook address is not part of the fingerprint).
pub fn template_of(raw: &[u8], has_hook: bool) -> String {
    let size = std::mem::size_of::<PoolConfig>();
    let mut body = raw[8..8 + size].to_vec();
    // PoolConfig: quote_mint [0..32), fee_claimer [32..64), leftover_receiver [64..96)
    for b in &mut body[32..96] {
        *b = 0;
    }
    body.push(has_hook as u8);
    format!("{:016x}", fnv1a(&body))
}

fn pubkey_at(raw: &[u8], off: usize) -> Option<String> {
    raw.get(off..off + 32)
        .and_then(|b| anchor_lang::prelude::Pubkey::try_from(b).ok())
        .map(|p| p.to_string())
}

pub fn load_info(conn: &Connection, config: &str, raw: &[u8], has_hook: bool) -> Result<ConfigInfo> {
    let mut st = conn.prepare_cached("SELECT DISTINCT creator FROM pools WHERE config = ?1")?;
    let creators = st
        .query_map(params![config], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    // satellites are sellers without a buy, but only in pools where they sold no more tokens
    // than the creator bought (otherwise the distribution is not from the operator: e.g. a sniper distributed to its own wallets)
    let mut st = conn.prepare_cached(
        "WITH fs AS (
             SELECT s.pool, s.fee_payer, s.included_fee_input_amount AS t
             FROM swaps s JOIN pools p ON p.pool = s.pool
             WHERE p.config = ?1 AND s.trade_direction = 0 AND s.fee_payer <> p.creator
               AND s.fee_payer NOT IN (SELECT fee_payer FROM swaps b WHERE b.pool = s.pool AND b.trade_direction = 1)),
         ok AS (
             SELECT fs.pool FROM fs GROUP BY fs.pool
             HAVING SUM(fs.t) * 100 <= 105 * (
                 SELECT COALESCE(SUM(c.output_amount), 0) FROM swaps c JOIN pools p2 ON p2.pool = c.pool
                 WHERE c.pool = fs.pool AND c.trade_direction = 1 AND c.fee_payer = p2.creator))
         SELECT DISTINCT fee_payer FROM fs WHERE pool IN (SELECT pool FROM ok)",
    )?;
    let satellites = st
        .query_map(params![config], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(ConfigInfo {
        config: config.to_string(),
        fee_claimer: pubkey_at(raw, 40),
        leftover_receiver: pubkey_at(raw, 72),
        template: template_of(raw, has_hook),
        creators,
        satellites,
    })
}

struct Dsu(Vec<usize>);
impl Dsu {
    fn new(n: usize) -> Self {
        Dsu((0..n).collect())
    }
    fn find(&mut self, x: usize) -> usize {
        let p = self.0[x];
        if p == x {
            return x;
        }
        let r = self.find(p);
        self.0[x] = r;
        r
    }
    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.0[b] = a;
        }
    }
}

/// Clusters of configs linked by shared addresses. All are returned, including singletons.
///
/// `red[i]` — config i has synthetic trading by its own evidence. A platform (launchpad)
/// address appears in `platform_min` or more configs, while synthetic trading is observed
/// in fewer than half of them; such an address links configs in no role:
/// otherwise all launches of the platform would be glued into one "operator cluster".
pub fn clusters(infos: &[ConfigInfo], red: &[bool], platform_min: usize) -> Vec<Cluster> {
    let n = infos.len();
    let mut dsu = Dsu::new(n);

    // address -> configs where it appears in the given role
    let mut by_addr: HashMap<(LinkKind, String), Vec<usize>> = HashMap::new();
    for (i, c) in infos.iter().enumerate() {
        if let Some(a) = &c.fee_claimer {
            by_addr.entry((LinkKind::FeeClaimer, a.clone())).or_default().push(i);
        }
        if let Some(a) = &c.leftover_receiver {
            by_addr.entry((LinkKind::LeftoverReceiver, a.clone())).or_default().push(i);
        }
        for a in &c.creators {
            by_addr.entry((LinkKind::Creator, a.clone())).or_default().push(i);
        }
    }
    let is_platform = |idx: &Vec<usize>| -> bool {
        let distinct: HashSet<usize> = idx.iter().copied().collect();
        let reds = distinct.iter().filter(|&&i| red.get(i).copied().unwrap_or(false)).count();
        distinct.len() >= platform_min && reds * 2 < distinct.len()
    };
    by_addr.retain(|(_, addr), idx| !NEVER_LINK.contains(&addr.as_str()) && !is_platform(idx));
    let mut edges: Vec<(usize, usize, LinkKind)> = Vec::new();
    for ((kind, _), idx) in &by_addr {
        for w in idx.windows(2) {
            edges.push((w[0], w[1], *kind));
        }
    }

    // satellites: link pairs of configs with >= MIN_SHARED_SATELLITES shared wallets
    let mut sat_configs: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, c) in infos.iter().enumerate() {
        for s in &c.satellites {
            sat_configs.entry(s.as_str()).or_default().push(i);
        }
    }
    let mut pair_count: HashMap<(usize, usize), usize> = HashMap::new();
    for idx in sat_configs.values() {
        if idx.len() < 2 || idx.len() > 50 {
            continue; // appears in one config, or the address is too widespread (likely a shared service)
        }
        for a in 0..idx.len() {
            for b in a + 1..idx.len() {
                *pair_count.entry((idx[a], idx[b])).or_default() += 1;
            }
        }
    }
    for ((a, b), k) in &pair_count {
        if *k >= MIN_SHARED_SATELLITES {
            edges.push((*a, *b, LinkKind::Satellites));
        }
    }

    for (a, b, _) in &edges {
        dsu.union(*a, *b);
    }

    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        let r = dsu.find(i);
        groups.entry(r).or_default().push(i);
    }
    let mut out: Vec<Cluster> = groups
        .into_values()
        .map(|members| Cluster { id: 0, members, links: BTreeMap::new(), shared: Vec::new() })
        .collect();

    let mut root_of: HashMap<usize, usize> = HashMap::new();
    for (ci, c) in out.iter().enumerate() {
        for m in &c.members {
            root_of.insert(*m, ci);
        }
    }
    for (a, _b, kind) in &edges {
        let ci = root_of[a];
        *out[ci].links.entry(*kind).or_default() += 1;
    }
    for ((kind, addr), idx) in &by_addr {
        let distinct: HashSet<usize> = idx.iter().copied().collect();
        if distinct.len() >= 2 {
            let ci = root_of[&idx[0]];
            out[ci].shared.push(SharedAddress { kind: *kind, address: addr.clone(), configs: distinct.len() });
        }
    }
    for (s, idx) in &sat_configs {
        let distinct: HashSet<usize> = idx.iter().copied().collect();
        if distinct.len() >= 2 && distinct.len() <= 50 {
            let roots: HashSet<usize> = distinct.iter().map(|i| root_of[i]).collect();
            if roots.len() == 1 {
                let ci = *roots.iter().next().unwrap();
                out[ci].shared.push(SharedAddress { kind: LinkKind::Satellites, address: s.to_string(), configs: distinct.len() });
            }
        }
    }
    for c in &mut out {
        c.shared.sort_by(|a, b| b.configs.cmp(&a.configs).then(a.kind.cmp(&b.kind)).then(a.address.cmp(&b.address)));
    }
    // stable numbers: large clusters first
    out.sort_by(|a, b| b.members.len().cmp(&a.members.len()).then(a.members[0].cmp(&b.members[0])));
    for (i, c) in out.iter_mut().enumerate() {
        c.id = i + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str, fc: &str, lr: &str, tpl: &str, creators: &[&str], sats: &[&str]) -> ConfigInfo {
        ConfigInfo {
            config: name.into(),
            fee_claimer: Some(fc.into()),
            leftover_receiver: Some(lr.into()),
            template: tpl.into(),
            creators: creators.iter().map(|s| s.to_string()).collect(),
            satellites: sats.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn links_by_addresses_not_by_template() {
        let infos = vec![
            // operator X: a new config for every launch, a shared leftover receiver
            info("x1", "fx1", "LR_X", "T_RUG", &["cx1"], &["s1", "s2", "s3", "s4"]),
            info("x2", "fx2", "LR_X", "T_RUG", &["cx2"], &["s5"]),
            // operator Y: different addresses, but the same satellites (>= 3 shared)
            info("y1", "fy1", "LR_Y", "T_RUG", &["cy1"], &["s1", "s2", "s3"]),
            // an unrelated user of the same template — must not be glued in
            info("z1", "fz1", "LR_Z", "T_RUG", &["cz1"], &["s9"]),
            // platform: a shared fee claimer for two configs
            info("p1", "FC_P", "lp1", "T_OK", &["u1"], &[]),
            info("p2", "FC_P", "lp2", "T_OK2", &["u2"], &[]),
            // one shared satellite — not enough for a link
            info("w1", "fw1", "lw1", "T_X", &["cw1"], &["s1"]),
        ];
        let cl = clusters(&infos, &vec![false; infos.len()], 10);
        let find = |name: &str| {
            let i = infos.iter().position(|c| c.config == name).unwrap();
            cl.iter().find(|c| c.members.contains(&i)).unwrap().id
        };
        assert_eq!(find("x1"), find("x2"));
        assert_eq!(find("x1"), find("y1"));
        assert_ne!(find("x1"), find("z1"));
        assert_eq!(find("p1"), find("p2"));
        assert_ne!(find("x1"), find("w1"));
        let x = cl.iter().find(|c| c.id == find("x1")).unwrap();
        assert!(x.links.contains_key(&LinkKind::LeftoverReceiver));
        assert!(x.links.contains_key(&LinkKind::Satellites));
    }

    #[test]
    fn platform_and_burn_addresses_do_not_link() {
        let mut infos = Vec::new();
        let mut red = Vec::new();
        // platform: 12 configs with a shared receiver and pool creator, synthetic trading in one
        for k in 0..12 {
            infos.push(info(&format!("p{k}"), "PLATFORM", "PLATFORM", "T", &["PLATFORM_CREATOR"], &[]));
            red.push(k == 0);
        }
        // operator: 12 configs with a shared receiver, synthetic trading in 8 — this is an operator address
        for k in 0..12 {
            infos.push(info(&format!("o{k}"), "OPERATOR", &format!("lo{k}"), "T", &[format!("co{k}").as_str()], &[]));
            red.push(k < 8);
        }
        // honest configs that burn the leftover: the burn address does not link them
        infos.push(info("b1", "fb1", "1nc1nerator11111111111111111111111111111111", "T", &["cb1"], &[]));
        infos.push(info("b2", "fb2", "1nc1nerator11111111111111111111111111111111", "T", &["cb2"], &[]));
        red.extend([false, false]);
        let cl = clusters(&infos, &red, 10);
        let id = |name: &str| {
            let i = infos.iter().position(|c| c.config == name).unwrap();
            cl.iter().find(|c| c.members.contains(&i)).unwrap().id
        };
        assert_ne!(id("p0"), id("p1"), "platform address must not link configs");
        assert_eq!(id("o0"), id("o11"), "operator address links its configs");
        assert_ne!(id("b1"), id("b2"), "burn address must not link configs");
    }

    #[test]
    fn template_ignores_addresses() {
        let size = std::mem::size_of::<PoolConfig>();
        let mut a = vec![0u8; 8 + size];
        a[200] = 7; // some economic parameter
        let mut b = a.clone();
        b[40..72].copy_from_slice(&[9u8; 32]); // a different fee_claimer
        b[72..104].copy_from_slice(&[5u8; 32]); // a different leftover_receiver
        assert_eq!(template_of(&a, false), template_of(&b, false));
        let mut c = a.clone();
        c[200] = 8;
        assert_ne!(template_of(&a, false), template_of(&c, false));
        assert_ne!(template_of(&a, false), template_of(&a, true));
    }
}
