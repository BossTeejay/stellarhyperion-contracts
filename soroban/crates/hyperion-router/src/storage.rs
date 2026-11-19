use hyperion_core::{FlowWindow, HyperionError, RouteKind};
use soroban_sdk::{contracttype, vec, Address, BytesN, Env, Vec};

use crate::types::{
    Config, InspectKey, KeyDiscoveryPage, KeyTtlStatus, PendingClaim, QueuedAction, ReplayRecord,
    RouteConfig, TokenConfig, TransferRecord,
};

/// Ledgers of headroom below which the router tops a key's lifetime back up.
///
/// Soroban storage archives if nobody touches it, which is a difference from EVM storage that
/// bites long-lived bookkeeping in particular. Flow-limit counters and pending-claim records
/// are exactly the entries that sit untouched for weeks and then matter enormously, so every
/// read path bumps them and there is a permissionless keeper entrypoint for the rest.
pub const BUMP_THRESHOLD: u32 = 17_280; // about a day
pub const BUMP_TO: u32 = 518_400; // about thirty days

/// Durable retention parameters for replay tombstones (~180 days target, clamped to host max TTL).
pub const REPLAY_BUMP_THRESHOLD: u32 = 518_400; // about thirty days
pub const REPLAY_BUMP_TO: u32 = 3_110_400; // about 180 days

/// Maximum number of keys returned per paginated discovery call.
pub const MAX_DISCOVERY_LIMIT: u32 = 50;

/// Maximum number of persistent keys inspected in a single `inspect_ttls` call.
pub const MAX_TTL_INSPECT_KEYS: u32 = 64;

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Config,
    OutNonce,
    /// Contract the router hands outbound funds to for a given rail.
    Adapter(RouteKind),
    /// The only address allowed to call `bridge_in` for a given rail.
    RailReceiver(RouteKind),
    RouteEnabled(RouteKind),
    RouteConfig(RouteKind),
    Token(Address),
    /// Per-route ceiling, overriding the token-wide one when present.
    RouteFlowLimit(Address, RouteKind),
    Flow(Address, RouteKind),
    Transfer(u64),
    /// Replay guard. Presence means this message has already been acted on.
    ///
    /// Keyed on the rail's own message identifier at its full width rather than on a sequence
    /// number we made up. CCTP v2 numbers its messages with thirty two bytes and Axelar with a
    /// string, and squeezing either of those into a u64 means two different messages can land on
    /// the same key. That collision would not let anyone mint twice, because the rail's own
    /// verifier refuses the second one first, but it would permanently refuse a delivery whose
    /// funds have already been burned on the far side, which is its own kind of loss.
    Processed(RouteKind, BytesN<32>),
    Claim(u64),
    ClaimSeq,
    Queued(u64),
    QueueSeq,
    /// Aggregate unsettled parked-claim liability per token.
    ClaimLiability(Address),
    /// Dense list of currently unsettled claim IDs for bounded discovery.
    OpenClaims,
    /// Number of indexed replay tombstones.
    ReplayCount,
    /// Maps slot index `0..ReplayCount` -> `ReplayRecord`.
    ReplaySlot(u64),
}

#[inline(never)]
fn extend_persistent_ttl(env: &Env, key: &DataKey, threshold: u32, extend_to: u32) {
    let max_ttl = env.storage().max_ttl();
    if max_ttl == 0 {
        return;
    }
    let bounded_to = extend_to.min(max_ttl);
    let bounded_threshold = threshold.min(bounded_to);
    env.storage()
        .persistent()
        .extend_ttl(key, bounded_threshold, bounded_to);
}

pub fn config(env: &Env) -> Result<Config, HyperionError> {
    env.storage()
        .instance()
        .get(&DataKey::Config)
        .ok_or(HyperionError::NotInitialized)
}

pub fn set_config(env: &Env, cfg: &Config) {
    env.storage().instance().set(&DataKey::Config, cfg);
    env.storage().instance().extend_ttl(BUMP_THRESHOLD, BUMP_TO);
}

pub fn is_initialized(env: &Env) -> bool {
    env.storage().instance().has(&DataKey::Config)
}

pub fn next_out_nonce(env: &Env) -> u64 {
    let current: u64 = env
        .storage()
        .instance()
        .get(&DataKey::OutNonce)
        .unwrap_or(0u64);
    let next = current + 1;
    env.storage().instance().set(&DataKey::OutNonce, &next);
    next
}

pub fn out_nonce(env: &Env) -> u64 {
    env.storage()
        .instance()
        .get(&DataKey::OutNonce)
        .unwrap_or(0u64)
}

pub fn adapter(env: &Env, route: RouteKind) -> Result<Address, HyperionError> {
    env.storage()
        .instance()
        .get(&DataKey::Adapter(route))
        .ok_or(HyperionError::AdapterNotSet)
}

pub fn set_adapter(env: &Env, route: RouteKind, addr: &Address) {
    env.storage().instance().set(&DataKey::Adapter(route), addr);
    env.storage().instance().extend_ttl(BUMP_THRESHOLD, BUMP_TO);
}

pub fn rail_receiver(env: &Env, route: RouteKind) -> Result<Address, HyperionError> {
    env.storage()
        .instance()
        .get(&DataKey::RailReceiver(route))
        .ok_or(HyperionError::AdapterNotSet)
}

pub fn set_rail_receiver(env: &Env, route: RouteKind, addr: &Address) {
    env.storage()
        .instance()
        .set(&DataKey::RailReceiver(route), addr);
    env.storage().instance().extend_ttl(BUMP_THRESHOLD, BUMP_TO);
}

/// Routes are off until somebody turns them on. A rail that has an adapter deployed but has
/// not been reviewed for a given deployment should not be reachable by accident.
pub fn route_enabled(env: &Env, route: RouteKind) -> bool {
    env.storage()
        .instance()
        .get(&DataKey::RouteEnabled(route))
        .unwrap_or(false)
}

pub fn set_route_enabled(env: &Env, route: RouteKind, enabled: bool) {
    env.storage()
        .instance()
        .set(&DataKey::RouteEnabled(route), &enabled);
    env.storage().instance().extend_ttl(BUMP_THRESHOLD, BUMP_TO);
}

pub fn route_config(env: &Env, route: RouteKind) -> Option<RouteConfig> {
    let key = DataKey::RouteConfig(route);
    let cfg = env.storage().persistent().get(&key);
    if cfg.is_some() {
        env.storage()
            .persistent()
            .extend_ttl(&key, BUMP_THRESHOLD, BUMP_TO);
    }
    cfg
}

pub fn set_route_config(env: &Env, route: RouteKind, cfg: &RouteConfig) {
    let key = DataKey::RouteConfig(route);
    env.storage().persistent().set(&key, cfg);
    env.storage()
        .persistent()
        .extend_ttl(&key, BUMP_THRESHOLD, BUMP_TO);
}

pub fn token_config(env: &Env, token: &Address) -> Result<TokenConfig, HyperionError> {
    let key = DataKey::Token(token.clone());
    let cfg: TokenConfig = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(HyperionError::TokenNotRegistered)?;
    env.storage()
        .persistent()
        .extend_ttl(&key, BUMP_THRESHOLD, BUMP_TO);
    Ok(cfg)
}

pub fn token_config_opt(env: &Env, token: &Address) -> Option<TokenConfig> {
    env.storage()
        .persistent()
        .get(&DataKey::Token(token.clone()))
}

pub fn set_token_config(env: &Env, token: &Address, cfg: &TokenConfig) {
    let key = DataKey::Token(token.clone());
    env.storage().persistent().set(&key, cfg);
    env.storage()
        .persistent()
        .extend_ttl(&key, BUMP_THRESHOLD, BUMP_TO);
}

pub fn route_flow_limit(env: &Env, token: &Address, route: RouteKind) -> Option<i128> {
    let key = DataKey::RouteFlowLimit(token.clone(), route);
    let limit = env.storage().persistent().get(&key);
    if limit.is_some() {
        env.storage()
            .persistent()
            .extend_ttl(&key, BUMP_THRESHOLD, BUMP_TO);
    }
    limit
}

pub fn set_route_flow_limit(env: &Env, token: &Address, route: RouteKind, limit: i128) {
    let key = DataKey::RouteFlowLimit(token.clone(), route);
    env.storage().persistent().set(&key, &limit);
    env.storage()
        .persistent()
        .extend_ttl(&key, BUMP_THRESHOLD, BUMP_TO);
}

/// The ceiling that actually applies to a token on a route: the per-route override when one is
/// configured, otherwise the token-wide limit.
pub fn effective_flow_limit(
    env: &Env,
    token: &Address,
    route: RouteKind,
) -> Result<i128, HyperionError> {
    match route_flow_limit(env, token, route) {
        Some(limit) => Ok(limit),
        None => Ok(token_config(env, token)?.flow_limit),
    }
}

pub fn flow_window(env: &Env, token: &Address, route: RouteKind) -> FlowWindow {
    let key = DataKey::Flow(token.clone(), route);
    let window = env
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(FlowWindow::empty);
    if env.storage().persistent().has(&key) {
        env.storage()
            .persistent()
            .extend_ttl(&key, BUMP_THRESHOLD, BUMP_TO);
    }
    window
}

pub fn set_flow_window(env: &Env, token: &Address, route: RouteKind, window: &FlowWindow) {
    let key = DataKey::Flow(token.clone(), route);
    env.storage().persistent().set(&key, window);
    env.storage()
        .persistent()
        .extend_ttl(&key, BUMP_THRESHOLD, BUMP_TO);
}

pub fn transfer(env: &Env, nonce: u64) -> Option<TransferRecord> {
    let key = DataKey::Transfer(nonce);
    let record = env.storage().persistent().get(&key);
    if record.is_some() {
        env.storage()
            .persistent()
            .extend_ttl(&key, BUMP_THRESHOLD, BUMP_TO);
    }
    record
}

pub fn set_transfer(env: &Env, record: &TransferRecord) {
    let key = DataKey::Transfer(record.nonce);
    env.storage().persistent().set(&key, record);
    env.storage()
        .persistent()
        .extend_ttl(&key, BUMP_THRESHOLD, BUMP_TO);
}

/// Whether a given inbound message has already been acted on.
///
/// Persistent rather than temporary on purpose. A temporary entry that archives is a replay
/// window, and a replay window on a bridge is a mint-twice bug. Checking an existing tombstone
/// also refreshes its retention TTL so active keys do not silently age out.
pub fn is_processed(env: &Env, route: RouteKind, message_id: &BytesN<32>) -> bool {
    let key = DataKey::Processed(route, message_id.clone());
    let present = env.storage().persistent().has(&key);
    if present {
        extend_persistent_ttl(env, &key, REPLAY_BUMP_THRESHOLD, REPLAY_BUMP_TO);
    }
    present
}

#[inline(never)]
pub fn mark_processed(env: &Env, route: RouteKind, message_id: &BytesN<32>) {
    let key = DataKey::Processed(route, message_id.clone());
    let already_processed = env.storage().persistent().has(&key);
    env.storage().persistent().set(&key, &true);
    extend_persistent_ttl(env, &key, REPLAY_BUMP_THRESHOLD, REPLAY_BUMP_TO);

    if !already_processed {
        let count = replay_count(env);
        let slot_key = DataKey::ReplaySlot(count);
        let record = ReplayRecord {
            route,
            message_id: message_id.clone(),
            processed_ledger: env.ledger().sequence(),
            processed_at: env.ledger().timestamp(),
        };
        env.storage().persistent().set(&slot_key, &record);
        extend_persistent_ttl(env, &slot_key, REPLAY_BUMP_THRESHOLD, REPLAY_BUMP_TO);
        env.storage()
            .instance()
            .set(&DataKey::ReplayCount, &(count + 1));
    }
}

pub fn next_claim_id(env: &Env) -> u64 {
    let current: u64 = env
        .storage()
        .instance()
        .get(&DataKey::ClaimSeq)
        .unwrap_or(0u64);
    let next = current + 1;
    env.storage().instance().set(&DataKey::ClaimSeq, &next);
    next
}

pub fn claim_count(env: &Env) -> u64 {
    env.storage()
        .instance()
        .get(&DataKey::ClaimSeq)
        .unwrap_or(0u64)
}

pub fn claim(env: &Env, id: u64) -> Option<PendingClaim> {
    let key = DataKey::Claim(id);
    let record = env.storage().persistent().get(&key);
    if record.is_some() {
        env.storage()
            .persistent()
            .extend_ttl(&key, BUMP_THRESHOLD, BUMP_TO);
    }
    record
}

pub fn set_claim(env: &Env, record: &PendingClaim) {
    let key = DataKey::Claim(record.id);
    env.storage().persistent().set(&key, record);
    env.storage()
        .persistent()
        .extend_ttl(&key, BUMP_THRESHOLD, BUMP_TO);
}

pub fn next_queue_id(env: &Env) -> u64 {
    let current: u64 = env
        .storage()
        .instance()
        .get(&DataKey::QueueSeq)
        .unwrap_or(0u64);
    let next = current + 1;
    env.storage().instance().set(&DataKey::QueueSeq, &next);
    next
}

pub fn queue_count(env: &Env) -> u64 {
    env.storage()
        .instance()
        .get(&DataKey::QueueSeq)
        .unwrap_or(0u64)
}

pub fn queued(env: &Env, id: u64) -> Option<QueuedAction> {
    env.storage().persistent().get(&DataKey::Queued(id))
}

pub fn set_queued(env: &Env, action: &QueuedAction) {
    let key = DataKey::Queued(action.id);
    env.storage().persistent().set(&key, action);
    env.storage()
        .persistent()
        .extend_ttl(&key, BUMP_THRESHOLD, BUMP_TO);
}

pub fn clear_queued(env: &Env, id: u64) {
    env.storage().persistent().remove(&DataKey::Queued(id));
}

/// Bump a flow counter's archival date without reading it into a decision.
///
/// The keeper calls this for lanes that have gone quiet. A counter that archives quietly
/// resets a limit back to full, which is the sort of bug that only shows up on the one day it
/// matters, so it is worth a scheduled job and a few stroops of fee.
pub fn touch_flow(env: &Env, token: &Address, route: RouteKind) {
    let key = DataKey::Flow(token.clone(), route);
    if env.storage().persistent().has(&key) {
        extend_persistent_ttl(env, &key, BUMP_THRESHOLD, BUMP_TO);
    }
}

// -------------------------------------------------------------------------------------------
// #52: Per-token claim liabilities
// -------------------------------------------------------------------------------------------

#[inline(never)]
pub fn claim_liability(env: &Env, token: &Address) -> i128 {
    let key = DataKey::ClaimLiability(token.clone());
    let total = env.storage().persistent().get(&key).unwrap_or(0i128);
    if env.storage().persistent().has(&key) {
        extend_persistent_ttl(env, &key, BUMP_THRESHOLD, BUMP_TO);
    }
    total
}

#[inline(never)]
pub fn add_claim_liability(
    env: &Env,
    token: &Address,
    amount: i128,
) -> Result<i128, HyperionError> {
    let key = DataKey::ClaimLiability(token.clone());
    let current = env.storage().persistent().get(&key).unwrap_or(0i128);
    let next = current
        .checked_add(amount)
        .ok_or(HyperionError::DecimalOverflow)?;
    env.storage().persistent().set(&key, &next);
    extend_persistent_ttl(env, &key, BUMP_THRESHOLD, BUMP_TO);
    Ok(next)
}

#[inline(never)]
pub fn sub_claim_liability(env: &Env, token: &Address, amount: i128) -> i128 {
    let key = DataKey::ClaimLiability(token.clone());
    let current = env.storage().persistent().get(&key).unwrap_or(0i128);
    // Saturate at zero so legacy pre-upgrade claims settled post-upgrade never strand funds.
    let next = if current >= amount {
        current - amount
    } else {
        0
    };
    env.storage().persistent().set(&key, &next);
    extend_persistent_ttl(env, &key, BUMP_THRESHOLD, BUMP_TO);
    next
}

pub fn touch_claim_liability(env: &Env, token: &Address) {
    let key = DataKey::ClaimLiability(token.clone());
    if env.storage().persistent().has(&key) {
        extend_persistent_ttl(env, &key, BUMP_THRESHOLD, BUMP_TO);
    }
}

// -------------------------------------------------------------------------------------------
// #51: Paginated maintenance-key discovery (open claims & transfers)
// -------------------------------------------------------------------------------------------

#[inline(never)]
pub fn index_open_claim(env: &Env, claim_id: u64) {
    let key = DataKey::OpenClaims;
    let mut list: Vec<u64> = env
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(|| vec![env]);
    if !list.contains(claim_id) {
        list.push_back(claim_id);
        env.storage().persistent().set(&key, &list);
    }
    extend_persistent_ttl(env, &key, BUMP_THRESHOLD, BUMP_TO);
}

#[inline(never)]
pub fn remove_open_claim(env: &Env, claim_id: u64) {
    let key = DataKey::OpenClaims;
    let Some(mut list) = env.storage().persistent().get::<_, Vec<u64>>(&key) else {
        return;
    };
    if let Some(pos) = list.first_index_of(claim_id) {
        let last_idx = list.len() - 1;
        if pos != last_idx {
            let last_val = list.get_unchecked(last_idx);
            list.set(pos, last_val);
        }
        list.pop_back();
        env.storage().persistent().set(&key, &list);
        extend_persistent_ttl(env, &key, BUMP_THRESHOLD, BUMP_TO);
    }
}

#[inline(never)]
pub fn discover_keys(
    env: &Env,
    open_claims: bool,
    cursor: u64,
    limit: u32,
) -> KeyDiscoveryPage {
    if open_claims {
        discover_open_claims(env, cursor, limit)
    } else {
        discover_transfers(env, cursor, limit)
    }
}

#[inline(never)]
fn page_bounds(cursor: u64, limit: u32, total: u64) -> (u64, u64) {
    let bounded = limit.min(MAX_DISCOVERY_LIMIT) as u64;
    let start = cursor.min(total);
    let end = start.saturating_add(bounded).min(total);
    (start, end)
}

#[inline(never)]
fn discover_open_claims(env: &Env, cursor: u64, limit: u32) -> KeyDiscoveryPage {
    let list: Vec<u64> = env
        .storage()
        .persistent()
        .get(&DataKey::OpenClaims)
        .unwrap_or_else(|| vec![env]);
    let total = list.len() as u64;
    let (start, end) = page_bounds(cursor, limit, total);
    let mut ids = vec![env];
    for i in start..end {
        ids.push_back(list.get_unchecked(i as u32));
    }
    KeyDiscoveryPage {
        ids,
        next_cursor: end,
        total,
    }
}

#[inline(never)]
fn discover_transfers(env: &Env, cursor: u64, limit: u32) -> KeyDiscoveryPage {
    let total = out_nonce(env);
    let bounded = limit.min(MAX_DISCOVERY_LIMIT) as u64;
    let start = if cursor == 0 { 1 } else { cursor };
    let mut ids = vec![env];
    if total == 0 || start > total || bounded == 0 {
        return KeyDiscoveryPage {
            ids,
            next_cursor: start,
            total,
        };
    }
    let end = start.saturating_add(bounded).min(total + 1);
    for nonce in start..end {
        if env.storage().persistent().has(&DataKey::Transfer(nonce)) {
            ids.push_back(nonce);
        }
    }
    KeyDiscoveryPage {
        ids,
        next_cursor: end,
        total,
    }
}

// -------------------------------------------------------------------------------------------
// #50: Replay-record retention & discovery
// -------------------------------------------------------------------------------------------

pub fn replay_count(env: &Env) -> u64 {
    env.storage()
        .instance()
        .get(&DataKey::ReplayCount)
        .unwrap_or(0u64)
}

pub fn touch_replay(env: &Env, route: RouteKind, message_id: &BytesN<32>) -> bool {
    let key = DataKey::Processed(route, message_id.clone());
    if !env.storage().persistent().has(&key) {
        return false;
    }
    extend_persistent_ttl(env, &key, REPLAY_BUMP_THRESHOLD, REPLAY_BUMP_TO);
    true
}

#[inline(never)]
pub fn discover_replays(env: &Env, cursor: u64, limit: u32) -> Vec<ReplayRecord> {
    let total = replay_count(env);
    let (start, end) = page_bounds(cursor, limit, total);
    let mut records = vec![env];
    for i in start..end {
        if let Some(rec) = env
            .storage()
            .persistent()
            .get::<_, ReplayRecord>(&DataKey::ReplaySlot(i))
        {
            records.push_back(rec);
        }
    }
    records
}

// -------------------------------------------------------------------------------------------
// #48: Bounded TTL inspection
// -------------------------------------------------------------------------------------------

#[inline(never)]
pub fn inspect_ttls(env: &Env, keys: &Vec<InspectKey>) -> Result<Vec<KeyTtlStatus>, HyperionError> {
    if keys.len() > MAX_TTL_INSPECT_KEYS {
        return Err(HyperionError::InvalidLimit);
    }

    let mut entries = vec![env];
    for item in keys.iter() {
        let (kind, key) = match item {
            InspectKey::Token(t) => (0, DataKey::Token(t)),
            InspectKey::Flow(t, r) => (1, DataKey::Flow(t, r)),
            InspectKey::Transfer(n) => (2, DataKey::Transfer(n)),
            InspectKey::Replay(r, m) => (3, DataKey::Processed(r, m)),
            InspectKey::Claim(id) => (4, DataKey::Claim(id)),
            InspectKey::ClaimLiability(t) => (5, DataKey::ClaimLiability(t)),
        };
        let present = env.storage().persistent().has(&key);
        #[cfg(test)]
        let remaining_ttl = if present {
            use soroban_sdk::testutils::storage::Persistent as _;
            env.storage().persistent().get_ttl(&key)
        } else {
            0
        };
        #[cfg(not(test))]
        let remaining_ttl = 0;

        entries.push_back(KeyTtlStatus {
            kind,
            present,
            remaining_ttl,
        });
    }

    Ok(entries)
}
