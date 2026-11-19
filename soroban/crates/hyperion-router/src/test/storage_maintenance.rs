//! Storage maintenance tests: bounded settled-claim cleanup (#54), storage footprint
//! projections (#55), and upgrade rehearsal snapshot comparisons (#53).

extern crate alloc;

use alloc::format;
use hyperion_core::{AddressKind, HyperionError, RouteKind};
use soroban_sdk::{testutils::Ledger, vec, Address, BytesN, Env};

use super::doubles::{evm_destination, message_id, Delivery};
use super::setup::{World, EVM_DECIMALS, FLOW_LIMIT, HUNDRED};
use crate::storage::{MAX_CLAIM_CLEANUP_BATCH, MIN_CLAIM_RETENTION_SECONDS};
use crate::types::{
    AdminAction, Config, Destination, OutboundRequest, PendingClaim, QueuedAction, RouteConfig,
    StorageFootprint, TokenConfig, TransferRecord,
};
use crate::{HyperionRouter, HyperionRouterClient};

#[derive(Clone, Debug, Eq, PartialEq)]
struct CriticalStateSnapshot {
    config: Config,
    token_cfg: TokenConfig,
    cctp_adapter: Address,
    cctp_receiver: Address,
    cctp_enabled: bool,
    cctp_route_cfg: Option<RouteConfig>,
    cctp_flow_available: i128,
    out_nonce: u64,
    transfer_1: TransferRecord,
    claim_count: u64,
    archived_claim_count: u64,
    unsettled_claim: PendingClaim,
    settled_claim: PendingClaim,
    replay_processed: bool,
    queue_count: u64,
    queued_action: QueuedAction,
    footprint: StorageFootprint,
}

fn snapshot_via_public_methods(
    client: &HyperionRouterClient,
    token: &Address,
    unsettled_claim_id: u64,
    settled_claim_id: u64,
    processed_msg_id: &BytesN<32>,
    queued_id: u64,
) -> CriticalStateSnapshot {
    CriticalStateSnapshot {
        config: client.get_config(),
        token_cfg: client.get_token(token),
        cctp_adapter: client.get_adapter(&RouteKind::Cctp),
        cctp_receiver: client.get_rail_receiver(&RouteKind::Cctp),
        cctp_enabled: client.is_route_enabled(&RouteKind::Cctp),
        cctp_route_cfg: client.get_route_config(&RouteKind::Cctp),
        cctp_flow_available: client.flow_available(token, &RouteKind::Cctp),
        out_nonce: client.last_out_nonce(),
        transfer_1: client.get_transfer(&1),
        claim_count: client.claim_count(),
        archived_claim_count: client.archived_claim_count(),
        unsettled_claim: client.get_claim(&unsettled_claim_id),
        settled_claim: client.get_claim(&settled_claim_id),
        replay_processed: client.was_processed(&RouteKind::Cctp, processed_msg_id),
        queue_count: client.queue_count(),
        queued_action: client.get_queued(&queued_id),
        footprint: client.get_storage_footprint(),
    }
}

#[test]
fn bounded_cleanup_archives_only_aged_settled_claims_and_preserves_replay_guard() {
    let w = World::new();
    let token = w.with_failable_token();
    w.failable(&token).mint(&w.rail_id, &(HUNDRED * 3));
    w.failable(&token).set_blocked(&w.recipient, &true);

    // Claim 1: parked and settled.
    let c1 = w.rail().deliver(&Delivery {
        route: RouteKind::Cctp,
        token: token.clone(),
        amount: HUNDRED,
        recipient: w.recipient.clone(),
        recipient_kind: AddressKind::Account,
        source_chain: w.ethereum(),
        source_nonce: 101,
    });
    // Claim 2: parked and left unsettled.
    let c2 = w.rail().deliver(&Delivery {
        route: RouteKind::Cctp,
        token: token.clone(),
        amount: HUNDRED,
        recipient: w.recipient.clone(),
        recipient_kind: AddressKind::Account,
        source_chain: w.ethereum(),
        source_nonce: 102,
    });
    assert_eq!(c1, 1);
    assert_eq!(c2, 2);

    // Settle claim 1.
    w.failable(&token).set_blocked(&w.recipient, &false);
    w.router().settle_claim(&w.user, &c1);

    // Too young: nothing archived yet.
    let archived_early = w
        .router()
        .archive_settled_claims(&vec![&w.env, c1, c2], &MIN_CLAIM_RETENTION_SECONDS);
    assert_eq!(archived_early, 0);
    assert!(w.router().get_claim(&c1).settled);

    // Retention shorter than protocol floor is rejected.
    let err = w
        .router()
        .try_archive_settled_claims(&vec![&w.env, c1], &(MIN_CLAIM_RETENTION_SECONDS - 1));
    assert_eq!(err, Err(Ok(HyperionError::InvalidWindow)));

    // Oversized batch is rejected.
    let mut big_batch = vec![&w.env];
    for i in 1..=(MAX_CLAIM_CLEANUP_BATCH as u64 + 1) {
        big_batch.push_back(i);
    }
    assert_eq!(
        w.router()
            .try_archive_settled_claims(&big_batch, &MIN_CLAIM_RETENTION_SECONDS),
        Err(Ok(HyperionError::InvalidLimit))
    );

    // Advance past retention window: claim 1 is archived, claim 2 (unsettled) is untouched.
    let now = w.env.ledger().timestamp();
    w.env
        .ledger()
        .set_timestamp(now + MIN_CLAIM_RETENTION_SECONDS);
    let archived = w
        .router()
        .archive_settled_claims(&vec![&w.env, c1, c2, 999], &MIN_CLAIM_RETENTION_SECONDS);
    assert_eq!(archived, 1);
    assert_eq!(w.router().archived_claim_count(), 1);

    // Claim 1 metadata is removed; claim 2 remains intact.
    assert_eq!(
        w.router().try_get_claim(&c1),
        Err(Ok(HyperionError::ClaimNotFound))
    );
    assert!(!w.router().get_claim(&c2).settled);

    // Replay guard for claim 1's inbound message remains intact.
    let msg_id_1 = message_id(&w.env, 101);
    assert!(w.router().was_processed(&RouteKind::Cctp, &msg_id_1));
    let fp = w.router().get_storage_footprint();
    assert_eq!(fp.claim_count, 2);
    assert_eq!(fp.archived_claims, 1);
    assert_eq!(fp.active_claim_entries, 1);
}

#[test]
fn storage_footprint_tracks_transfers_claims_and_queued_actions() {
    let w = World::new();
    w.run_action(AdminAction::SetRouteConfig(
        RouteKind::Cctp,
        RouteConfig {
            max_single_transfer: HUNDRED * 2,
        },
    ));
    w.router().bridge_out(
        &w.user,
        &OutboundRequest {
            token: w.token_id.clone(),
            amount: HUNDRED,
            route: RouteKind::Cctp,
            destination: Destination {
                chain: w.ethereum(),
                address: evm_destination(&w.env, 0x22),
            },
            destination_decimals: EVM_DECIMALS,
            min_destination_amount: 0,
        },
    );
    let qid = w
        .router()
        .queue_action(&w.admin, &AdminAction::SetFeeBps(15));

    let fp = w.router().get_storage_footprint();
    assert_eq!(fp.out_nonce, 1);
    assert_eq!(fp.claim_count, 0);
    assert_eq!(fp.archived_claims, 0);
    assert_eq!(fp.active_claim_entries, 0);
    assert_eq!(fp.queue_count, qid);
    assert!(fp.bump_to > fp.bump_threshold);
    assert_eq!(fp.min_claim_retention_seconds, MIN_CLAIM_RETENTION_SECONDS);
    assert_eq!(fp.max_claim_cleanup_batch, MAX_CLAIM_CLEANUP_BATCH);
    assert_eq!(
        w.router().flow_available(&w.token_id, &RouteKind::Cctp),
        FLOW_LIMIT - w.router().get_transfer(&1).net_amount
    );
}

#[test]
fn upgrade_rehearsal_snapshots_and_compares_critical_records_via_public_methods() {
    let w = World::new();
    w.run_action(AdminAction::SetRouteConfig(
        RouteKind::Cctp,
        RouteConfig {
            max_single_transfer: HUNDRED * 5,
        },
    ));

    // 1. Outbound transfer record + flow window mutation.
    w.router().bridge_out(
        &w.user,
        &OutboundRequest {
            token: w.token_id.clone(),
            amount: HUNDRED,
            route: RouteKind::Cctp,
            destination: Destination {
                chain: w.ethereum(),
                address: evm_destination(&w.env, 0x33),
            },
            destination_decimals: EVM_DECIMALS,
            min_destination_amount: 0,
        },
    );

    // 2. Parked & settled claims + processed replay entry.
    let token = w.with_failable_token();
    w.failable(&token).mint(&w.rail_id, &(HUNDRED * 3));
    w.failable(&token).set_blocked(&w.recipient, &true);
    let c1 = w.rail().deliver(&Delivery {
        route: RouteKind::Cctp,
        token: token.clone(),
        amount: HUNDRED,
        recipient: w.recipient.clone(),
        recipient_kind: AddressKind::Account,
        source_chain: w.ethereum(),
        source_nonce: 7,
    });
    let c2 = w.rail().deliver(&Delivery {
        route: RouteKind::Cctp,
        token: token.clone(),
        amount: HUNDRED,
        recipient: w.recipient.clone(),
        recipient_kind: AddressKind::Account,
        source_chain: w.ethereum(),
        source_nonce: 8,
    });
    w.failable(&token).set_blocked(&w.recipient, &false);
    w.router().settle_claim(&w.user, &c2);

    // 3. Queued timelock action.
    let qid = w
        .router()
        .queue_action(&w.admin, &AdminAction::SetFeeBps(20));

    let msg_id = message_id(&w.env, 7);
    let pre = snapshot_via_public_methods(&w.router(), &w.token_id, c1, c2, &msg_id, qid);

    // Snapshot full ledger state and restore into a fresh Env with the router re-registered at
    // the same address (rehearsing a WASM code swap over persisted storage).
    let mut router_str_buf = [0u8; 56];
    w.router_id.to_string().copy_into_slice(&mut router_str_buf);
    let router_str = core::str::from_utf8(&router_str_buf).unwrap();

    let mut token_str_buf = [0u8; 56];
    w.token_id.to_string().copy_into_slice(&mut token_str_buf);
    let token_str = core::str::from_utf8(&token_str_buf).unwrap();

    let msg_id_bytes = msg_id.to_array();

    let ledger_snapshot = w.env.to_ledger_snapshot();
    let upgraded_env = Env::from_ledger_snapshot(ledger_snapshot);
    let upgraded_router_id = Address::from_str(&upgraded_env, router_str);
    let upgraded_token_id = Address::from_str(&upgraded_env, token_str);
    let upgraded_msg_id = BytesN::from_array(&upgraded_env, &msg_id_bytes);

    upgraded_env.register_at(&upgraded_router_id, HyperionRouter, ());
    let post_client = HyperionRouterClient::new(&upgraded_env, &upgraded_router_id);

    let post = snapshot_via_public_methods(
        &post_client,
        &upgraded_token_id,
        c1,
        c2,
        &upgraded_msg_id,
        qid,
    );
    assert_eq!(format!("{pre:?}"), format!("{post:?}"));
}
