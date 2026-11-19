//! Maintenance, liability reconciliation, key discovery, replay retention, and TTL inspection.

use hyperion_core::{AddressKind, HyperionError, RouteKind};
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    vec, Address, BytesN, Vec,
};

use super::doubles::{evm_destination, message_id, Delivery};
use super::setup::{World, EVM_DECIMALS, HUNDRED};
use crate::storage::{self, MAX_DISCOVERY_LIMIT, MAX_TTL_INSPECT_KEYS, REPLAY_BUMP_TO};
use crate::types::{Destination, InspectKey, OutboundRequest};

fn outbound_req(w: &World, route: RouteKind, amount: i128) -> OutboundRequest {
    OutboundRequest {
        token: w.token_id.clone(),
        amount,
        route,
        destination: Destination {
            chain: w.ethereum(),
            address: evm_destination(&w.env, 0x11),
        },
        destination_decimals: EVM_DECIMALS,
        min_destination_amount: 0,
    }
}

#[test]
fn claim_liabilities_track_parked_and_settled_claims_independently_of_balance() {
    let w = World::new();
    let token = w.with_failable_token();
    let stranger = Address::generate(&w.env);
    w.failable(&token).mint(&w.rail_id, &(HUNDRED * 3));
    w.failable(&token).set_blocked(&stranger, &true);

    // Park two claims for recipients without trustlines.
    let c1 = w.rail().deliver(&Delivery {
        route: RouteKind::Cctp,
        token: token.clone(),
        amount: HUNDRED,
        recipient: stranger.clone(),
        recipient_kind: AddressKind::Account,
        source_chain: w.ethereum(),
        source_nonce: 1,
    });
    let c2 = w.rail().deliver(&Delivery {
        route: RouteKind::AxelarIts,
        token: token.clone(),
        amount: HUNDRED * 2,
        recipient: stranger.clone(),
        recipient_kind: AddressKind::Account,
        source_chain: w.ethereum(),
        source_nonce: 2,
    });

    assert_eq!(w.router().reconcile_claim_liability(&token).tracked_liability, HUNDRED * 3);
    let report = w.router().reconcile_claim_liability(&token);
    assert_eq!(report.tracked_liability, HUNDRED * 3);
    assert_eq!(report.contract_balance, HUNDRED * 3);
    assert!(report.balanced);

    // Unrelated direct transfer to the router creates surplus without altering liability.
    w.failable(&token).mint(&w.router_id, &50);
    let report = w.router().reconcile_claim_liability(&token);
    assert_eq!(report.tracked_liability, HUNDRED * 3);
    assert_eq!(report.contract_balance, HUNDRED * 3 + 50);
    assert!(report.balanced);

    // Open trustline and settle both claims.
    w.failable(&token).set_blocked(&stranger, &false);
    w.router().settle_claim(&w.user, &c1);
    assert_eq!(w.router().reconcile_claim_liability(&token).tracked_liability, HUNDRED * 2);

    w.router().settle_claim(&w.user, &c2);
    assert_eq!(w.router().reconcile_claim_liability(&token).tracked_liability, 0);

    let report = w.router().reconcile_claim_liability(&token);
    assert_eq!(report.tracked_liability, 0);
    assert_eq!(report.contract_balance, 50);
    assert!(report.balanced);
}

#[test]
fn legacy_pre_upgrade_claim_settles_even_if_liability_counter_is_zero() {
    let w = World::new();
    let token = w.with_failable_token();
    let stranger = Address::generate(&w.env);
    w.failable(&token).mint(&w.rail_id, &HUNDRED);
    w.failable(&token).set_blocked(&stranger, &true);

    let c1 = w.rail().deliver(&Delivery {
        route: RouteKind::Cctp,
        token: token.clone(),
        amount: HUNDRED,
        recipient: stranger.clone(),
        recipient_kind: AddressKind::Account,
        source_chain: w.ethereum(),
        source_nonce: 11,
    });

    // Simulate pre-upgrade state where ClaimLiability(token) was not yet tracked.
    w.env.as_contract(&w.router_id, || {
        w.env
            .storage()
            .persistent()
            .remove(&storage::DataKey::ClaimLiability(token.clone()));
    });
    assert_eq!(w.router().reconcile_claim_liability(&token).tracked_liability, 0);

    w.failable(&token).set_blocked(&stranger, &false);
    w.router().settle_claim(&w.user, &c1);
    assert_eq!(w.failable(&token).balance(&stranger), HUNDRED);
    assert_eq!(w.router().reconcile_claim_liability(&token).tracked_liability, 0);
}

#[test]
fn paginated_discovery_indexes_open_claims_and_transfers_without_full_scans() {
    let w = World::new();
    let token = w.with_failable_token();
    let stranger = Address::generate(&w.env);
    w.failable(&token).mint(&w.rail_id, &(HUNDRED * 4));
    w.failable(&token).set_blocked(&stranger, &true);

    // Create 4 parked claims.
    let c1 = w.rail().deliver(&Delivery {
        route: RouteKind::Cctp,
        token: token.clone(),
        amount: HUNDRED,
        recipient: stranger.clone(),
        recipient_kind: AddressKind::Account,
        source_chain: w.ethereum(),
        source_nonce: 1,
    });
    let c2 = w.rail().deliver(&Delivery {
        route: RouteKind::Cctp,
        token: token.clone(),
        amount: HUNDRED,
        recipient: stranger.clone(),
        recipient_kind: AddressKind::Account,
        source_chain: w.ethereum(),
        source_nonce: 2,
    });
    let c3 = w.rail().deliver(&Delivery {
        route: RouteKind::Cctp,
        token: token.clone(),
        amount: HUNDRED,
        recipient: stranger.clone(),
        recipient_kind: AddressKind::Account,
        source_chain: w.ethereum(),
        source_nonce: 3,
    });
    let c4 = w.rail().deliver(&Delivery {
        route: RouteKind::Cctp,
        token: token.clone(),
        amount: HUNDRED,
        recipient: stranger.clone(),
        recipient_kind: AddressKind::Account,
        source_chain: w.ethereum(),
        source_nonce: 4,
    });

    assert_eq!(w.router().discover_keys(&true, &0, &0).total, 4);
    let page1 = w.router().discover_keys(&true, &0, &2);
    assert_eq!(page1.ids, vec![&w.env, c1, c2]);
    assert_eq!(page1.next_cursor, 2);
    assert_eq!(page1.total, 4);

    let page2 = w.router().discover_keys(&true, &page1.next_cursor, &2);
    assert_eq!(page2.ids, vec![&w.env, c3, c4]);
    assert_eq!(page2.next_cursor, 4);

    // Settle c2 (middle element) -> swap-remove keeps index dense with 3 open claims.
    w.failable(&token).set_blocked(&stranger, &false);
    w.router().settle_claim(&w.user, &c2);
    assert_eq!(w.router().discover_keys(&true, &0, &0).total, 3);
    let remaining = w.router().discover_keys(&true, &0, &10);
    assert_eq!(remaining.ids.len(), 3);
    assert!(!remaining.ids.contains(c2));

    // Create 3 outbound transfers and paginate from cursor 2.
    let req = outbound_req(&w, RouteKind::Cctp, HUNDRED);
    let n1 = w.router().bridge_out(&w.user, &req);
    let n2 = w.router().bridge_out(&w.user, &req);
    let n3 = w.router().bridge_out(&w.user, &req);
    assert_eq!((n1, n2, n3), (1, 2, 3));

    let tx_page = w.router().discover_keys(&false, &2, &2);
    assert_eq!(tx_page.ids, vec![&w.env, 2, 3]);
    assert_eq!(tx_page.next_cursor, 4);
    assert_eq!(tx_page.total, 3);

    // Oversized limit is safely clamped.
    let clamped = w
        .router()
        .discover_keys(&false, &1, &(MAX_DISCOVERY_LIMIT + 500));
    assert_eq!(clamped.ids.len(), 3);
}

#[test]
fn replay_retention_policy_preserves_identity_and_supports_bounded_upkeep() {
    let w = World::new();
    w.fund_rail(HUNDRED * 2);

    w.rail().deliver(&Delivery {
        route: RouteKind::Cctp,
        token: w.token_id.clone(),
        amount: HUNDRED,
        recipient: w.recipient.clone(),
        recipient_kind: AddressKind::Account,
        source_chain: w.ethereum(),
        source_nonce: 77,
    });
    w.rail().deliver(&Delivery {
        route: RouteKind::AxelarIts,
        token: w.token_id.clone(),
        amount: HUNDRED,
        recipient: w.recipient.clone(),
        recipient_kind: AddressKind::Account,
        source_chain: w.ethereum(),
        source_nonce: 78,
    });

    let records = w.router().discover_replays(&0, &1);
    assert_eq!(records.len(), 1);
    let rec = records.get(0).unwrap();
    assert_eq!(rec.route, RouteKind::Cctp);
    assert_eq!(rec.message_id, message_id(&w.env, 77));
    assert_eq!(rec.processed_ledger, 100_000);

    // Advance ledger and verify keep_alive_replays refreshes TTL back to REPLAY_BUMP_TO.
    w.env.ledger().with_mut(|li| {
        li.sequence_number += 2_600_000;
    });
    w.router()
        .keep_alive_replays(&vec![&w.env, (rec.route, rec.message_id.clone())]);

    let entries = w.router().inspect_ttls(&vec![
        &w.env,
        InspectKey::Replay(rec.route, rec.message_id),
    ]);
    let entry = entries.get(0).unwrap();
    assert_eq!(entry.kind, 3);
    assert!(entry.present);
    assert!(entry.remaining_ttl >= REPLAY_BUMP_TO - 10);
}

#[test]
fn legacy_processed_tombstone_without_metadata_still_blocks_replay() {
    let w = World::new();
    w.fund_rail(HUNDRED);
    let mid = message_id(&w.env, 555);

    w.env.as_contract(&w.router_id, || {
        w.env
            .storage()
            .persistent()
            .set(&storage::DataKey::Processed(RouteKind::Cctp, mid.clone()), &true);
    });

    assert!(w.router().was_processed(&RouteKind::Cctp, &mid));
    assert_eq!(
        w.rail().try_deliver(&Delivery {
            route: RouteKind::Cctp,
            token: w.token_id.clone(),
            amount: HUNDRED,
            recipient: w.recipient.clone(),
            recipient_kind: AddressKind::Account,
            source_chain: w.ethereum(),
            source_nonce: 555,
        }),
        Err(Ok(HyperionError::ReplayedMessage))
    );
}

#[test]
fn inspect_ttls_reports_present_and_missing_keys_and_enforces_bound() {
    let w = World::new();
    let failable_token = w.with_failable_token();
    let stranger = Address::generate(&w.env);
    w.failable(&failable_token).mint(&w.rail_id, &HUNDRED);
    w.failable(&failable_token).set_blocked(&stranger, &true);

    let req = outbound_req(&w, RouteKind::Cctp, HUNDRED);
    let nonce = w.router().bridge_out(&w.user, &req);
    let claim_id = w.rail().deliver(&Delivery {
        route: RouteKind::Cctp,
        token: failable_token.clone(),
        amount: HUNDRED,
        recipient: stranger,
        recipient_kind: AddressKind::Account,
        source_chain: w.ethereum(),
        source_nonce: 90,
    });

    let entries = w.router().inspect_ttls(&vec![
        &w.env,
        InspectKey::Token(failable_token.clone()),
        InspectKey::ClaimLiability(failable_token.clone()),
        InspectKey::Flow(failable_token, RouteKind::Cctp),
        InspectKey::Transfer(nonce),
        InspectKey::Transfer(999),
        InspectKey::Replay(RouteKind::Cctp, message_id(&w.env, 90)),
        InspectKey::Claim(claim_id),
    ]);

    // Token + ClaimLiability + Flow(missing for failable_token) + 2 transfers + 1 replay + 1 claim = 7 entries
    assert_eq!(entries.len(), 7);
    assert!(entries.get(0).unwrap().present); // Token
    assert!(entries.get(1).unwrap().present); // ClaimLiability
    assert!(!entries.get(2).unwrap().present); // Flow (failable_token has no outbound flow yet)
    assert!(entries.get(3).unwrap().present); // Transfer(1)
    assert!(!entries.get(4).unwrap().present); // Transfer(999)
    assert_eq!(entries.get(4).unwrap().remaining_ttl, 0);
    assert!(entries.get(5).unwrap().present); // Replay
    assert!(entries.get(6).unwrap().present); // Claim(1)

    // Exceeding MAX_TTL_INSPECT_KEYS is rejected.
    let mut too_many: Vec<InspectKey> = vec![&w.env];
    for i in 0..=MAX_TTL_INSPECT_KEYS {
        too_many.push_back(InspectKey::Transfer(i as u64));
    }
    assert_eq!(
        w.router().try_inspect_ttls(&too_many),
        Err(Ok(HyperionError::InvalidLimit))
    );
    let _ = BytesN::from_array(&w.env, &[0u8; 32]);
}
