//! Hyperion's Soroban router.
//!
//! The router is deliberately not a bridge. It never verifies a cross-chain message itself and
//! it never holds a validator set. What it does is own the accounting that sits either side of
//! somebody else's rail: fees, decimal conversion, flow limits, replay bookkeeping, and the
//! awkward cases that show up when an EVM chain and a Stellar account disagree about what an
//! address is.
//!
//! Outbound, the sender authorises once, the router takes its fee, floors the remainder to
//! something the destination can actually represent, charges the flow limit and hands the funds
//! to a rail adapter. Inbound, the rail's own receiver contract calls in, the router pulls the
//! funds it was promised, and either delivers them or writes down who they belong to.
#![no_std]

mod adapter;
mod events;
mod storage;
mod timelock;
mod types;

#[cfg(test)]
mod test;

pub use adapter::{PaidRailAdapter, PaidRailAdapterClient, RailAdapter, RailAdapterClient};
pub use storage::DataKey;
pub use timelock::{GRACE_PERIOD, MAX_TIMELOCK_DELAY, MIN_TIMELOCK_DELAY};
pub use types::{
    ActionAudit, ActionAuditStatus, AdapterKeyCoverage, AdapterUpkeepRequest, AdminAction,
    CancelReason, Config, Destination, InboundRecord, MaintenanceSummary, MigrationStatus,
    Origin, OutboundRequest, PendingClaim, ProcessedKey, QueuedAction, Recipient, RestorePlan,
    RestorePlanRequest, RouteConfig, RouteQuote, RouterUpkeepRequest, TokenConfig, TransferRecord,
    CURRENT_SCHEMA_VERSION, LEGACY_SCHEMA_VERSION, MAX_MAINTENANCE_BATCH, MAX_REASON_LEN,
};

use hyperion_core::{
    address::{assert_route_supports, bytes32_to_evm},
    amount::{apply_fee, assert_exact_transfer, convert_decimals_exact, floor_to_representable},
    flow, AdapterMaintenanceClient, HyperionError, RouteKind, MAX_FEE_BPS,
};
use soroban_sdk::{contract, contractimpl, token, vec, Address, BytesN, Env, String, Vec};

/// Every rail Hyperion knows how to speak, in the order the app shows them.
const ALL_ROUTES: [RouteKind; 4] = [
    RouteKind::Cctp,
    RouteKind::AxelarIts,
    RouteKind::AxelarGmp,
    RouteKind::Allbridge,
];

#[contract]
pub struct HyperionRouter;

#[contractimpl]
impl HyperionRouter {
    /// Stand the router up. Callable exactly once.
    pub fn initialize(
        env: Env,
        admin: Address,
        guardian: Address,
        treasury: Address,
        fee_bps: u32,
        flow_window_ledgers: u32,
        timelock_delay: u64,
    ) -> Result<(), HyperionError> {
        if storage::is_initialized(&env) {
            return Err(HyperionError::AlreadyInitialized);
        }
        admin.require_auth();
        if fee_bps > MAX_FEE_BPS {
            return Err(HyperionError::FeeTooHigh);
        }
        if flow_window_ledgers == 0 {
            return Err(HyperionError::InvalidWindow);
        }
        if !(MIN_TIMELOCK_DELAY..=MAX_TIMELOCK_DELAY).contains(&timelock_delay) {
            return Err(HyperionError::TimelockDelayOutOfRange);
        }
        let cfg = Config {
            admin,
            guardian,
            treasury,
            fee_bps,
            flow_window_ledgers,
            timelock_delay,
            paused: false,
        };
        storage::set_config(&env, &cfg);
        storage::set_schema_version(&env, CURRENT_SCHEMA_VERSION);
        events::config_changed(&env, &cfg);
        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // Outbound
    // ---------------------------------------------------------------------------------------

    /// Send `amount` of `token` to an EVM chain over `route`.
    ///
    /// `destination_decimals` is what the asset uses on the far side, six for USDC on every
    /// EVM chain Hyperion targets against seven here. The router floors the transfer to
    /// something that base can represent and never pulls the remainder from the sender at all,
    /// which is cheaper for everyone than taking dust and then owing it back.
    ///
    /// Returns the outbound nonce, which is what the app and the indexer follow the transfer by.
    pub fn bridge_out(
        env: Env,
        sender: Address,
        request: OutboundRequest,
    ) -> Result<u64, HyperionError> {
        bridge_out_inner(env, sender, request, None)
    }

    pub fn bridge_out_with_gas(
        env: Env,
        sender: Address,
        request: OutboundRequest,
        gas_amount: i128,
    ) -> Result<u64, HyperionError> {
        if request.route != RouteKind::AxelarIts && request.route != RouteKind::Cctp {
            return Err(HyperionError::UnsupportedRoute);
        }
        if gas_amount <= 0 {
            return Err(HyperionError::InvalidAmount);
        }
        bridge_out_inner(env, sender, request, Some(gas_amount))
    }

    pub fn supports_paid_route(_env: Env, route: RouteKind) -> bool {
        route == RouteKind::AxelarIts || route == RouteKind::Cctp
    }

    pub fn bridge_in(
        env: Env,
        caller: Address,
        route: RouteKind,
        token: Address,
        amount: i128,
        recipient: Recipient,
        origin: Origin,
    ) -> Result<u64, HyperionError> {
        caller.require_auth();
        let expected = storage::rail_receiver(&env, route)?;
        if caller != expected {
            return Err(HyperionError::NotRailReceiver);
        }
        if amount <= 0 {
            return Err(HyperionError::InvalidAmount);
        }
        if storage::is_processed(&env, route, &origin.message_id) {
            return Err(HyperionError::ReplayedMessage);
        }
        assert_route_supports(route, recipient.kind)?;
        storage::mark_processed(&env, route, &origin.message_id);

        let this = env.current_contract_address();
        let client = token::Client::new(&env, &token);

        // Pull rather than trust. The receiver pre-authorises this transfer before calling in,
        // so if the funds are not actually there the whole delivery reverts here instead of
        // writing down a claim against money nobody holds.
        transfer_exact(&client, &caller, &this, amount, false)?;
        let delivered = transfer_exact(&client, &this, &recipient.address, amount, true)?;

        let claim_id = if delivered {
            0u64
        } else {
            // Almost always a missing trustline. Reverting would strand funds that have already
            // been burned on the far side, so the router keeps them and writes down whose they
            // are.
            let id = storage::next_claim_id(&env);
            let claim = PendingClaim {
                id,
                recipient: recipient.address.clone(),
                token: token.clone(),
                amount,
                route,
                source_chain: origin.chain.clone(),
                source_nonce: origin.nonce,
                created_at: env.ledger().timestamp(),
                settled: false,
            };
            storage::set_claim(&env, &claim);
            events::claim_parked(&env, &claim);
            id
        };

        let record = InboundRecord {
            route,
            recipient: recipient.address,
            token,
            amount,
            source_chain: origin.chain,
            source_nonce: origin.nonce,
            delivered,
            claim_id,
            ledger: env.ledger().sequence(),
        };
        events::bridge_in(&env, &record);
        Ok(claim_id)
    }

    /// Hand a parked claim to the recipient it was always going to.
    ///
    /// Permissionless on purpose. The funds can only move to the address recorded at delivery
    /// time, so there is nothing to gain by calling this for somebody else and no reason to
    /// make a user who just opened a trustline also work out how to call a contract.
    pub fn settle_claim(env: Env, settler: Address, id: u64) -> Result<(), HyperionError> {
        settler.require_auth();
        let mut claim = storage::claim(&env, id).ok_or(HyperionError::ClaimNotFound)?;
        if claim.settled {
            return Err(HyperionError::ClaimAlreadySettled);
        }
        let this = env.current_contract_address();
        let client = token::Client::new(&env, &claim.token);
        if !transfer_exact(&client, &this, &claim.recipient, claim.amount, true)? {
            return Err(HyperionError::RecipientNotReady);
        }
        claim.settled = true;
        storage::set_claim(&env, &claim);
        events::claim_settled(&env, &claim, &settler);
        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // Quoting
    // ---------------------------------------------------------------------------------------

    /// Price `amount` across every rail at once, losers included.
    ///
    /// The app draws all four and says why the three it did not pick lost, so this returns a
    /// quote per route with a reason code rather than filtering down to the winner. Reason is
    /// zero when the route is available and otherwise the same error code the router would have
    /// thrown had you tried it.
    pub fn quote_routes(
        env: Env,
        token: Address,
        amount: i128,
        destination_decimals: u32,
    ) -> Vec<RouteQuote> {
        let mut out = vec![&env];
        let cfg = match storage::config(&env) {
            Ok(cfg) => cfg,
            Err(e) => {
                for route in ALL_ROUTES {
                    out.push_back(unavailable(route, amount, e));
                }
                return out;
            }
        };
        let token_cfg = storage::token_config_opt(&env, &token);
        let ledger = env.ledger().sequence();

        for route in ALL_ROUTES {
            out.push_back(quote_one(
                &env,
                &cfg,
                &token,
                token_cfg.as_ref(),
                route,
                amount,
                destination_decimals,
                ledger,
            ));
        }
        out
    }

    // ---------------------------------------------------------------------------------------
    // Circuit breakers
    // ---------------------------------------------------------------------------------------

    /// Stop new departures. Either key can do it, immediately, with no delay to sit through.
    pub fn pause(env: Env, caller: Address) -> Result<(), HyperionError> {
        caller.require_auth();
        let mut cfg = storage::config(&env)?;
        if caller != cfg.admin && caller != cfg.guardian {
            return Err(HyperionError::Unauthorized);
        }
        cfg.paused = true;
        storage::set_config(&env, &cfg);
        events::paused(&env, &caller, true);
        Ok(())
    }

    /// Start again. Admin only, because deciding an incident is over is not a safety action.
    pub fn unpause(env: Env, caller: Address) -> Result<(), HyperionError> {
        caller.require_auth();
        let mut cfg = storage::config(&env)?;
        if caller != cfg.admin {
            return Err(HyperionError::Unauthorized);
        }
        cfg.paused = false;
        storage::set_config(&env, &cfg);
        events::paused(&env, &caller, false);
        Ok(())
    }

    /// Turn a rail off. No timelock: closing a lane only ever narrows what the bridge will do.
    pub fn disable_route(env: Env, caller: Address, route: RouteKind) -> Result<(), HyperionError> {
        caller.require_auth();
        let cfg = storage::config(&env)?;
        if caller != cfg.admin && caller != cfg.guardian {
            return Err(HyperionError::Unauthorized);
        }
        storage::set_route_enabled(&env, route, false);
        events::route_configured(&env, route, false);
        Ok(())
    }

    /// Stop a token dead without touching any other token. No timelock, same reasoning as
    /// pausing: if an asset turns out to be doing something it should not, waiting out a delay
    /// to stop routing it is not a policy, it is a countdown.
    ///
    /// Re registering it through the timelock is what turns it back on, so this cannot be used
    /// to quietly flip a token's decimals or limit on the way past.
    pub fn disable_token(env: Env, caller: Address, token: Address) -> Result<(), HyperionError> {
        caller.require_auth();
        let cfg = storage::config(&env)?;
        if caller != cfg.admin && caller != cfg.guardian {
            return Err(HyperionError::Unauthorized);
        }
        let mut token_cfg = storage::token_config(&env, &token)?;
        token_cfg.enabled = false;
        storage::set_token_config(&env, &token, &token_cfg);
        events::token_registered(&env, &token, &token_cfg);
        Ok(())
    }

    /// Tighten a flow limit immediately. Raising one goes through the timelock; lowering one
    /// does not, for the same reason pausing does not.
    pub fn lower_token_flow_limit(
        env: Env,
        caller: Address,
        token: Address,
        limit: i128,
    ) -> Result<(), HyperionError> {
        caller.require_auth();
        let cfg = storage::config(&env)?;
        if caller != cfg.admin && caller != cfg.guardian {
            return Err(HyperionError::Unauthorized);
        }
        if limit < 0 {
            return Err(HyperionError::InvalidLimit);
        }
        let mut token_cfg = storage::token_config(&env, &token)?;
        if limit >= token_cfg.flow_limit {
            return Err(HyperionError::InvalidLimit);
        }
        token_cfg.flow_limit = limit;
        storage::set_token_config(&env, &token, &token_cfg);
        events::flow_limit_lowered(&env, &token, limit, &caller);
        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // Timelocked administration
    // ---------------------------------------------------------------------------------------

    pub fn queue_action(
        env: Env,
        caller: Address,
        action: AdminAction,
    ) -> Result<u64, HyperionError> {
        caller.require_auth();
        let cfg = storage::config(&env)?;
        if caller != cfg.admin {
            return Err(HyperionError::Unauthorized);
        }
        let queued = timelock::queue(&env, &cfg, &caller, action)?;
        events::action_queued(&env, &queued);
        Ok(queued.id)
    }

    pub fn execute_action(env: Env, caller: Address, id: u64) -> Result<(), HyperionError> {
        caller.require_auth();
        let cfg = storage::config(&env)?;
        if caller != cfg.admin {
            return Err(HyperionError::Unauthorized);
        }
        let queued = timelock::take_matured(&env, &caller, id)?;
        timelock::apply(&env, &queued.action)?;
        events::action_executed(&env, &queued);
        Ok(())
    }

    /// Throw away a queued change. The guardian can do this too, which is the point: noticing
    /// something wrong in the queue should not require the key that put it there.
    pub fn cancel_action(env: Env, caller: Address, id: u64) -> Result<(), HyperionError> {
        Self::cancel_action_with_reason(
            env.clone(),
            caller,
            id,
            CancelReason::Unspecified,
            String::from_str(&env, ""),
        )
    }

    /// Cancel a queued change with a structured reason code and bounded audit note (#44).
    pub fn cancel_action_with_reason(
        env: Env,
        caller: Address,
        id: u64,
        reason: CancelReason,
        note: String,
    ) -> Result<(), HyperionError> {
        caller.require_auth();
        let cfg = storage::config(&env)?;
        if caller != cfg.admin && caller != cfg.guardian {
            return Err(HyperionError::Unauthorized);
        }
        let _ = timelock::cancel(&env, &caller, id, reason, note)?;
        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // Keeper, restoration planner, and schema migration
    // ---------------------------------------------------------------------------------------

    /// Push archival dates back on entries nobody has touched lately.
    ///
    /// Soroban storage expires, which is a genuine difference from every EVM chain on the other
    /// end of this bridge. A flow counter that archives quietly resets a limit, and a parked
    /// claim that archives is somebody's money gone. Anyone can call this, it costs them a fee
    /// and gains them nothing, and the backend runs it on a schedule so nobody has to.
    pub fn keep_alive(
        env: Env,
        tokens: Vec<Address>,
        routes: Vec<RouteKind>,
        claims: Vec<u64>,
        transfers: Vec<u64>,
    ) {
        env.storage()
            .instance()
            .extend_ttl(storage::BUMP_THRESHOLD, storage::BUMP_TO);
        for route in routes.iter() {
            let _ = storage::route_config(&env, route);
        }
        for token in tokens.iter() {
            let _ = storage::token_config(&env, &token);
            for route in routes.iter() {
                let _ = storage::route_flow_limit(&env, &token, route);
                storage::touch_flow(&env, &token, route);
            }
        }
        for id in claims.iter() {
            let _ = storage::claim(&env, id);
        }
        for nonce in transfers.iter() {
            let _ = storage::transfer(&env, nonce);
        }
    }

    /// Bounded router storage maintenance across token configs, route configs, flow limits/windows,
    /// claims, transfers, queued actions, and inbound replay guards (#49).
    pub fn maintain_router_storage(
        env: Env,
        request: RouterUpkeepRequest,
    ) -> Result<MaintenanceSummary, HyperionError> {
        let _ = storage::config(&env)?;
        if request.tokens.len() > MAX_MAINTENANCE_BATCH
            || request.routes.len() > MAX_MAINTENANCE_BATCH
            || request.claims.len() > MAX_MAINTENANCE_BATCH
            || request.transfers.len() > MAX_MAINTENANCE_BATCH
            || request.queued_actions.len() > MAX_MAINTENANCE_BATCH
            || request.processed_messages.len() > MAX_MAINTENANCE_BATCH
            || request
                .tokens
                .len()
                .saturating_mul(request.routes.len())
                > MAX_MAINTENANCE_BATCH
        {
            return Err(HyperionError::InvalidLimit);
        }
        env.storage()
            .instance()
            .extend_ttl(storage::BUMP_THRESHOLD, storage::BUMP_TO);

        let mut token_mappings_touched = 0u32;
        let mut peer_trust_touched = 0u32;
        let mut inbound_records_touched = 0u32;

        for route in request.routes.iter() {
            if storage::touch_persistent_key(&env, &DataKey::RouteConfig(route)) {
                peer_trust_touched += 1;
            }
        }
        for token in request.tokens.iter() {
            if storage::touch_persistent_key(&env, &DataKey::Token(token.clone())) {
                token_mappings_touched += 1;
            }
            for route in request.routes.iter() {
                if storage::touch_persistent_key(
                    &env,
                    &DataKey::RouteFlowLimit(token.clone(), route),
                ) {
                    token_mappings_touched += 1;
                }
                if storage::touch_persistent_key(&env, &DataKey::Flow(token.clone(), route)) {
                    token_mappings_touched += 1;
                }
            }
        }
        for id in request.claims.iter() {
            if storage::touch_persistent_key(&env, &DataKey::Claim(id)) {
                inbound_records_touched += 1;
            }
        }
        for msg in request.processed_messages.iter() {
            if storage::touch_persistent_key(
                &env,
                &DataKey::Processed(msg.route, msg.message_id.clone()),
            ) {
                inbound_records_touched += 1;
            }
        }
        for nonce in request.transfers.iter() {
            if storage::touch_persistent_key(&env, &DataKey::Transfer(nonce)) {
                peer_trust_touched += 1;
            }
        }
        for id in request.queued_actions.iter() {
            if storage::touch_persistent_key(&env, &DataKey::Queued(id)) {
                peer_trust_touched += 1;
            }
            if storage::touch_persistent_key(&env, &DataKey::ActionAudit(id)) {
                peer_trust_touched += 1;
            }
        }

        Ok(MaintenanceSummary {
            instance_extended: true,
            token_mappings_touched,
            peer_trust_touched,
            inbound_records_touched,
            total_keys_touched: token_mappings_touched
                + peer_trust_touched
                + inbound_records_touched,
        })
    }

    /// Bounded adapter storage maintenance forwarded to the registered rail adapter (#49).
    pub fn maintain_adapter_storage(
        env: Env,
        request: AdapterUpkeepRequest,
    ) -> Result<MaintenanceSummary, HyperionError> {
        if request.tokens.len() > MAX_MAINTENANCE_BATCH
            || request.peer_chains.len() > MAX_MAINTENANCE_BATCH
            || request.peer_domains.len() > MAX_MAINTENANCE_BATCH
            || request.remote_tokens.len() > MAX_MAINTENANCE_BATCH
            || request.inbound_messages.len() > MAX_MAINTENANCE_BATCH
        {
            return Err(HyperionError::InvalidLimit);
        }
        let adapter_addr = storage::adapter(&env, request.route)?;
        let client = AdapterMaintenanceClient::new(&env, &adapter_addr);
        client.maintain_storage(
            &request.tokens,
            &request.peer_chains,
            &request.peer_domains,
            &request.remote_tokens,
            &request.inbound_messages,
        )
    }

    /// Explicit key coverage for each rail adapter's storage maintenance interface (#49).
    pub fn adapter_key_coverage(_env: Env, route: RouteKind) -> AdapterKeyCoverage {
        match route {
            RouteKind::Cctp => AdapterKeyCoverage {
                route,
                has_token_mappings: true,
                has_peer_trust: true,
                has_inbound_records: false,
                token_mapping_keys: 1,
                peer_trust_keys: 3,
                inbound_record_keys: 0,
            },
            RouteKind::AxelarIts | RouteKind::AxelarGmp => AdapterKeyCoverage {
                route,
                has_token_mappings: true,
                has_peer_trust: true,
                has_inbound_records: false,
                token_mapping_keys: 2,
                peer_trust_keys: 2,
                inbound_record_keys: 0,
            },
            RouteKind::Allbridge => AdapterKeyCoverage {
                route,
                has_token_mappings: true,
                has_peer_trust: true,
                has_inbound_records: true,
                token_mapping_keys: 1,
                peer_trust_keys: 2,
                inbound_record_keys: 1,
            },
        }
    }

    /// Read-only planner for expired/archived router keys that emits restore footprints before
    /// maintenance or claim settlement (#47). Does not mutate storage or extend TTLs.
    pub fn plan_restore(
        env: Env,
        request: RestorePlanRequest,
    ) -> Result<RestorePlan, HyperionError> {
        if request.tokens.len() > MAX_MAINTENANCE_BATCH
            || request.routes.len() > MAX_MAINTENANCE_BATCH
            || request.claims.len() > MAX_MAINTENANCE_BATCH
            || request.transfers.len() > MAX_MAINTENANCE_BATCH
            || request.queued_actions.len() > MAX_MAINTENANCE_BATCH
            || request.processed_messages.len() > MAX_MAINTENANCE_BATCH
            || request
                .tokens
                .len()
                .saturating_mul(request.routes.len())
                > MAX_MAINTENANCE_BATCH
        {
            return Err(HyperionError::InvalidLimit);
        }

        let requires_instance_restore = !storage::is_initialized(&env);
        let claim_seq = storage::claim_count(&env);
        let out_seq = storage::out_nonce(&env);

        let mut restore_keys = vec![&env];
        let mut missing_or_archived_count = 0u32;
        let mut definitely_archived_count = 0u32;
        let mut active_count = 0u32;

        let mut check_key = |key: DataKey, is_definitely_allocated: bool| {
            if storage::has_persistent_key(&env, &key) {
                active_count += 1;
            } else {
                restore_keys.push_back(key);
                missing_or_archived_count += 1;
                if is_definitely_allocated {
                    definitely_archived_count += 1;
                }
            }
        };

        for route in request.routes.iter() {
            check_key(DataKey::RouteConfig(route), false);
        }
        for token in request.tokens.iter() {
            check_key(DataKey::Token(token.clone()), false);
            for route in request.routes.iter() {
                check_key(DataKey::RouteFlowLimit(token.clone(), route), false);
                check_key(DataKey::Flow(token.clone(), route), false);
            }
        }
        for id in request.claims.iter() {
            let allocated = id >= 1 && id <= claim_seq;
            check_key(DataKey::Claim(id), allocated);
        }
        for nonce in request.transfers.iter() {
            let allocated = nonce >= 1 && nonce <= out_seq;
            check_key(DataKey::Transfer(nonce), allocated);
        }
        for id in request.queued_actions.iter() {
            check_key(DataKey::Queued(id), false);
        }
        for msg in request.processed_messages.iter() {
            check_key(DataKey::Processed(msg.route, msg.message_id.clone()), false);
        }

        Ok(RestorePlan {
            current_ledger: env.ledger().sequence(),
            requires_instance_restore,
            restore_keys,
            missing_or_archived_count,
            definitely_archived_count,
            active_count,
        })
    }

    /// Read-only restoration planner for a single pending claim prior to `settle_claim` (#47).
    pub fn plan_claim_restore(env: Env, claim_id: u64) -> Result<RestorePlan, HyperionError> {
        let mut claims = vec![&env];
        claims.push_back(claim_id);
        Self::plan_restore(
            env.clone(),
            RestorePlanRequest {
                tokens: vec![&env],
                routes: vec![&env],
                claims,
                transfers: vec![&env],
                queued_actions: vec![&env],
                processed_messages: vec![&env],
            },
        )
    }

    /// Bounded storage schema migration entrypoint, callable only by the reviewed upgrade
    /// authority (`cfg.admin`) (#46).
    pub fn migrate_storage(
        env: Env,
        caller: Address,
        target_version: u32,
        max_steps: u32,
    ) -> Result<MigrationStatus, HyperionError> {
        caller.require_auth();
        let cfg = storage::config(&env)?;
        if caller != cfg.admin {
            return Err(HyperionError::Unauthorized);
        }
        if max_steps == 0 || max_steps > MAX_MAINTENANCE_BATCH {
            return Err(HyperionError::InvalidLimit);
        }
        if target_version != CURRENT_SCHEMA_VERSION {
            return Err(HyperionError::InvalidLimit);
        }
        let current = storage::schema_version(&env);
        if current >= target_version {
            return Err(HyperionError::AlreadyConfigured);
        }

        let total_transfers = storage::out_nonce(&env);
        let total_claims = storage::claim_count(&env);
        let (mut migrated_transfers, mut migrated_claims) = storage::migration_cursor(&env);
        let mut remaining = max_steps;

        while remaining > 0 && migrated_transfers < total_transfers {
            migrated_transfers += 1;
            let _ = storage::touch_persistent_key(&env, &DataKey::Transfer(migrated_transfers));
            remaining -= 1;
        }
        while remaining > 0 && migrated_claims < total_claims {
            migrated_claims += 1;
            let _ = storage::touch_persistent_key(&env, &DataKey::Claim(migrated_claims));
            remaining -= 1;
        }

        storage::set_migration_cursor(&env, migrated_transfers, migrated_claims);
        let completed = migrated_transfers >= total_transfers && migrated_claims >= total_claims;
        if completed {
            storage::set_schema_version(&env, target_version);
        }

        let status = MigrationStatus {
            current_version: if completed { target_version } else { current },
            target_version,
            migration_pending: !completed,
            migrated_transfers,
            migrated_claims,
            total_transfers,
            total_claims,
        };
        events::storage_migrated(&env, &caller, &status);
        Ok(status)
    }

    // ---------------------------------------------------------------------------------------
    // Views
    // ---------------------------------------------------------------------------------------

    /// Where the protocol's cut goes.
    ///
    /// Broken out of `get_config` because the rail adapters read it on their sweep path and
    /// deserialising the whole config to pull one address out of it is wasted work on a call
    /// anybody is allowed to make.
    pub fn treasury(env: Env) -> Result<Address, HyperionError> {
        Ok(storage::config(&env)?.treasury)
    }

    pub fn get_config(env: Env) -> Result<Config, HyperionError> {
        storage::config(&env)
    }

    pub fn get_transfer(env: Env, nonce: u64) -> Result<TransferRecord, HyperionError> {
        storage::transfer(&env, nonce).ok_or(HyperionError::UnknownNonce)
    }

    pub fn get_claim(env: Env, id: u64) -> Result<PendingClaim, HyperionError> {
        storage::claim(&env, id).ok_or(HyperionError::ClaimNotFound)
    }

    pub fn get_queued(env: Env, id: u64) -> Result<QueuedAction, HyperionError> {
        storage::queued(&env, id).ok_or(HyperionError::TimelockNotQueued)
    }

    pub fn get_action_audit(env: Env, id: u64) -> Result<ActionAudit, HyperionError> {
        storage::action_audit(&env, id).ok_or(HyperionError::TimelockNotQueued)
    }

    pub fn get_schema_version(env: Env) -> u32 {
        storage::schema_version(&env)
    }

    pub fn get_migration_status(env: Env) -> MigrationStatus {
        let current_version = storage::schema_version(&env);
        let (migrated_transfers, migrated_claims) = storage::migration_cursor(&env);
        MigrationStatus {
            current_version,
            target_version: CURRENT_SCHEMA_VERSION,
            migration_pending: current_version < CURRENT_SCHEMA_VERSION,
            migrated_transfers,
            migrated_claims,
            total_transfers: storage::out_nonce(&env),
            total_claims: storage::claim_count(&env),
        }
    }

    pub fn get_token(env: Env, token: Address) -> Result<TokenConfig, HyperionError> {
        storage::token_config(&env, &token)
    }

    pub fn get_adapter(env: Env, route: RouteKind) -> Result<Address, HyperionError> {
        storage::adapter(&env, route)
    }

    pub fn get_rail_receiver(env: Env, route: RouteKind) -> Result<Address, HyperionError> {
        storage::rail_receiver(&env, route)
    }

    pub fn is_route_enabled(env: Env, route: RouteKind) -> bool {
        storage::route_enabled(&env, route)
    }

    pub fn get_route_config(env: Env, route: RouteKind) -> Option<RouteConfig> {
        storage::route_config(&env, route)
    }

    pub fn flow_available(
        env: Env,
        token: Address,
        route: RouteKind,
    ) -> Result<i128, HyperionError> {
        let cfg = storage::config(&env)?;
        let limit = storage::effective_flow_limit(&env, &token, route)?;
        let window = storage::flow_window(&env, &token, route);
        flow::available(
            &window,
            limit,
            env.ledger().sequence(),
            cfg.flow_window_ledgers,
        )
    }

    pub fn was_processed(env: Env, route: RouteKind, message_id: BytesN<32>) -> bool {
        storage::is_processed(&env, route, &message_id)
    }

    pub fn last_out_nonce(env: Env) -> u64 {
        storage::out_nonce(&env)
    }

    pub fn claim_count(env: Env) -> u64 {
        storage::claim_count(&env)
    }

    pub fn queue_count(env: Env) -> u64 {
        storage::queue_count(&env)
    }

    /// Check token metadata; success classifies transfer behavior as unverified.
    /// Runtime balance invariants and operator token review are still required.
    ///
    /// Verifies that the token contract exists, correctly implements SEP-41 decimals matching
    /// `expected_decimals`, does not exceed `MAX_DECIMALS`, and responds to standard balance checks.
    pub fn check_token_behavior(
        env: Env,
        token: Address,
        expected_decimals: u32,
    ) -> Result<(), HyperionError> {
        timelock::validate_token_behavior(&env, &token, expected_decimals)
    }
}

/// A quote for a route that cannot be used, carrying the reason it cannot.
fn unavailable(route: RouteKind, amount: i128, reason: HyperionError) -> RouteQuote {
    RouteQuote {
        route,
        available: false,
        reason: reason as u32,
        gross_amount: amount,
        fee: 0,
        net_amount: 0,
        destination_amount: 0,
        flow_available: 0,
        waits_on_attestation: route.waits_on_attestation(),
        is_canonical: route.is_canonical(),
    }
}

#[allow(clippy::too_many_arguments)]
fn quote_one(
    env: &Env,
    cfg: &Config,
    token: &Address,
    token_cfg: Option<&TokenConfig>,
    route: RouteKind,
    amount: i128,
    destination_decimals: u32,
    ledger: u32,
) -> RouteQuote {
    let token_cfg = match token_cfg {
        Some(c) if c.enabled => c,
        Some(_) | None => return unavailable(route, amount, HyperionError::TokenNotRegistered),
    };
    if amount <= 0 {
        return unavailable(route, amount, HyperionError::InvalidAmount);
    }
    if cfg.paused {
        return unavailable(route, amount, HyperionError::Paused);
    }
    if !storage::route_enabled(env, route) {
        return unavailable(route, amount, HyperionError::RouteDisabled);
    }
    if storage::adapter(env, route).is_err() {
        return unavailable(route, amount, HyperionError::AdapterNotSet);
    }
    if let Some(route_cfg) = storage::route_config(env, route) {
        if route_cfg.max_single_transfer > 0 && amount > route_cfg.max_single_transfer {
            return unavailable(route, amount, HyperionError::ExceedsRouteLimit);
        }
    }

    let split = match apply_fee(amount, cfg.fee_bps) {
        Ok(v) => v,
        Err(e) => return unavailable(route, amount, e),
    };
    let fee = split.fee;
    let net = match floor_to_representable(split.net, token_cfg.decimals, destination_decimals) {
        Ok(v) => v,
        Err(e) => return unavailable(route, amount, e),
    };
    if net <= 0 {
        return unavailable(route, amount, HyperionError::AmountNotRepresentable);
    }
    let destination_amount =
        match convert_decimals_exact(net, token_cfg.decimals, destination_decimals) {
            Ok(v) => v,
            Err(e) => return unavailable(route, amount, e),
        };

    let limit = match storage::effective_flow_limit(env, token, route) {
        Ok(v) => v,
        Err(e) => return unavailable(route, amount, e),
    };
    let window = storage::flow_window(env, token, route);
    let headroom = flow::available(&window, limit, ledger, cfg.flow_window_ledgers).unwrap_or(0);
    if net > headroom {
        let mut quote = unavailable(route, amount, HyperionError::FlowLimitExceeded);
        quote.flow_available = headroom;
        quote.fee = fee;
        quote.net_amount = net;
        quote.destination_amount = destination_amount;
        return quote;
    }

    RouteQuote {
        route,
        available: true,
        reason: 0,
        gross_amount: fee + net,
        fee,
        net_amount: net,
        destination_amount,
        flow_available: headroom,
        waits_on_attestation: route.waits_on_attestation(),
        is_canonical: route.is_canonical(),
    }
}

fn bridge_out_inner(
    env: Env,
    sender: Address,
    request: OutboundRequest,
    gas_amount: Option<i128>,
) -> Result<u64, HyperionError> {
    sender.require_auth();
    let OutboundRequest {
        token,
        amount,
        route,
        destination,
        destination_decimals,
        min_destination_amount,
    } = request;
    let cfg = storage::config(&env)?;
    if cfg.paused {
        return Err(HyperionError::Paused);
    }
    if amount <= 0 {
        return Err(HyperionError::InvalidAmount);
    }
    if !storage::route_enabled(&env, route) {
        return Err(HyperionError::RouteDisabled);
    }
    if let Some(route_cfg) = storage::route_config(&env, route) {
        if route_cfg.max_single_transfer > 0 && amount > route_cfg.max_single_transfer {
            return Err(HyperionError::ExceedsRouteLimit);
        }
    }

    // A destination that is not a left-padded EVM word is refused rather than truncated.
    // Chopping the front off a Stellar key produces twenty bytes that look like a perfectly
    // ordinary address, and funds sent there are simply gone.
    let _ = bytes32_to_evm(&env, &destination.address)?;
    if destination.chain.is_empty() {
        return Err(HyperionError::UnknownChain);
    }

    let token_cfg = storage::token_config(&env, &token)?;
    if !token_cfg.enabled {
        return Err(HyperionError::TokenNotRegistered);
    }

    let split = apply_fee(amount, cfg.fee_bps)?;
    let fee = split.fee;
    let net = floor_to_representable(split.net, token_cfg.decimals, destination_decimals)?;
    if net <= 0 {
        return Err(HyperionError::AmountNotRepresentable);
    }
    // Circle's forwarding fee is paid from the burned USDC. Protect the user's minimum
    // after that entire fee budget, rather than comparing against the pre-fee burn.
    let minimum_net = match (route, gas_amount) {
        (RouteKind::Cctp, Some(max_fee)) => {
            if max_fee <= 0 || max_fee >= net {
                return Err(HyperionError::InvalidAmount);
            }
            net - max_fee
        }
        _ => net,
    };
    let destination_amount =
        convert_decimals_exact(minimum_net, token_cfg.decimals, destination_decimals)?;
    if destination_amount < min_destination_amount {
        return Err(HyperionError::SlippageExceeded);
    }
    let gross = fee + net;

    // Charge the flow limit against what actually crosses, not against what the sender
    // handed over. The fee never leaves Stellar, so it is not cross-chain exposure.
    let limit = storage::effective_flow_limit(&env, &token, route)?;
    let window = storage::flow_window(&env, &token, route);
    let ledger = env.ledger().sequence();
    let updated = flow::consume(&window, limit, net, ledger, cfg.flow_window_ledgers)?;
    storage::set_flow_window(&env, &token, route, &updated);

    let adapter_addr = storage::adapter(&env, route)?;
    let this = env.current_contract_address();
    let client = token::Client::new(&env, &token);

    let nonce = storage::next_out_nonce(&env);
    let sender_before = client.balance(&sender);
    let mut expected_debit = gross;
    if let Some(gas) = gas_amount {
        if route == RouteKind::AxelarIts
            && adapter::PaidRailAdapterClient::new(&env, &adapter_addr).gas_token() == token
        {
            expected_debit = gross
                .checked_add(gas)
                .ok_or(HyperionError::DecimalOverflow)?;
        }
        if fee > 0 {
            transfer_exact(&client, &sender, &cfg.treasury, fee, false)?;
        }
        adapter::PaidRailAdapterClient::new(&env, &adapter_addr).dispatch_from(
            &this,
            &sender,
            &token,
            &net,
            &destination.chain,
            &destination.address,
            &nonce,
            &gas,
        );
    } else {
        let before_this = client.balance(&this);
        transfer_exact(&client, &sender, &this, gross, false)?;

        if fee > 0 {
            transfer_exact(&client, &this, &cfg.treasury, fee, false)?;
        }

        transfer_exact(&client, &this, &adapter_addr, net, false)?;
        if client.balance(&this) != before_this {
            return Err(HyperionError::InvalidAmount);
        }

        RailAdapterClient::new(&env, &adapter_addr).dispatch(
            &this,
            &token,
            &net,
            &destination.chain,
            &destination.address,
            &nonce,
        );
    }

    assert_exact_transfer(client.balance(&sender), sender_before, expected_debit)?;

    let record = TransferRecord {
        nonce,
        sender,
        token,
        gross_amount: gross,
        fee,
        net_amount: net,
        destination_chain: destination.chain,
        destination: destination.address,
        route,
        created_ledger: ledger,
        created_at: env.ledger().timestamp(),
    };
    storage::set_transfer(&env, &record);
    events::bridge_out(&env, &record);
    Ok(nonce)
}

/// Recipient refusals are safe to park only when no token balance moved.
fn transfer_exact(
    client: &token::Client,
    from: &Address,
    to: &Address,
    amount: i128,
    allow_refusal: bool,
) -> Result<bool, HyperionError> {
    let from_before = client.balance(from);
    let to_before = payout_balance(client, to)?;
    let delivered = if allow_refusal {
        matches!(client.try_transfer(from, to, &amount), Ok(Ok(())))
    } else {
        client.transfer(from, to, &amount);
        true
    };
    let from_after = client.balance(from);
    let to_after = payout_balance(client, to)?;
    if delivered {
        assert_exact_transfer(from_after, from_before, amount)?;
        assert_exact_transfer(to_before, to_after, amount)?;
    } else if from_before != from_after || to_before != to_after {
        return Err(HyperionError::InvalidAmount);
    }
    Ok(delivered)
}

/// A built-in SAC reports contract error 13 for an absent account trustline.
/// Its balance is zero; arbitrary token balance errors are not trusted as zero.
fn payout_balance(client: &token::Client, to: &Address) -> Result<i128, HyperionError> {
    match client.try_balance(to) {
        Ok(Ok(balance)) if balance >= 0 => Ok(balance),
        Err(Ok(error))
            if error == soroban_sdk::Error::from_contract_error(13)
                && client.address.executable() == Some(soroban_sdk::Executable::StellarAsset) =>
        {
            Ok(0)
        }
        _ => Err(HyperionError::InvalidAmount),
    }
}
