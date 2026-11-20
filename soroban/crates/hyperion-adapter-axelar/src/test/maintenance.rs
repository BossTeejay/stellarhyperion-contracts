use super::setup::World;
use crate::storage::{self, DataKey};
use hyperion_core::HyperionError;
use soroban_sdk::{
    testutils::{storage::Persistent, Ledger},
    vec,
};

#[test]
fn permissionless_maintenance_keeps_expiring_peer_mapping_and_admin_unchanged() {
    let w = World::new();
    let chain = w.ethereum();
    let key = DataKey::Link(chain.clone());
    let before = w.env.as_contract(&w.adapter_id, || {
        let value: crate::types::ChainLink = w.env.storage().persistent().get(&key).unwrap();
        (value, storage::config(&w.env).unwrap().admin)
    });
    w.env
        .ledger()
        .with_mut(|ledger| ledger.sequence_number += storage::BUMP_TO - 10);
    w.env.set_auths(&[]);
    let summary = w.adapter().maintain_storage(
        &vec![&w.env],
        &vec![&w.env, chain.clone()],
        &vec![&w.env],
        &vec![&w.env],
        &vec![&w.env],
    );
    assert_eq!(summary.peer_trust_touched, 2);
    assert_eq!(summary.total_keys_touched, 2);
    w.env
        .ledger()
        .with_mut(|ledger| ledger.sequence_number += 20);
    w.env.as_contract(&w.adapter_id, || {
        let value: crate::types::ChainLink = w.env.storage().persistent().get(&key).unwrap();
        assert_eq!(value, before.0);
        assert_eq!(storage::config(&w.env).unwrap().admin, before.1);
        assert!(w.env.storage().persistent().get_ttl(&key) > storage::BUMP_TO - 30);
    });
}

#[test]
fn combined_physical_peer_key_footprint_is_bounded() {
    let w = World::new();
    let mut chains = vec![&w.env];
    for _ in 0..17 {
        chains.push_back(w.ethereum());
    }
    assert_eq!(
        w.adapter().try_maintain_storage(
            &vec![&w.env],
            &chains,
            &vec![&w.env],
            &vec![&w.env],
            &vec![&w.env]
        ),
        Err(Ok(HyperionError::InvalidLimit))
    );
}

#[test]
fn new_trust_metadata_shares_instance_lifetime_and_maintenance_keeps_it_intact() {
    let w = World::new();
    let next_peer = soroban_sdk::BytesN::from_array(&w.env, &[0x99; 20]);
    w.adapter()
        .propose_peer_rotation(&w.ethereum(), &next_peer, &3_600);
    w.adapter().set_chain_enabled(&w.ethereum(), &false);
    let pending = w.adapter().get_pending_rotation(&w.ethereum());
    w.env
        .ledger()
        .with_mut(|ledger| ledger.sequence_number += 4_097);
    w.env.set_auths(&[]);
    let summary = w.adapter().maintain_storage(
        &vec![&w.env],
        &vec![&w.env, w.ethereum()],
        &vec![&w.env],
        &vec![&w.env],
        &vec![&w.env],
    );
    assert_eq!(summary.peer_trust_touched, 2);
    assert_eq!(summary.total_keys_touched, 2);
    assert_eq!(w.adapter().get_pending_rotation(&w.ethereum()), pending);
    assert!(!w.adapter().is_chain_enabled(&w.ethereum()));
    w.env.as_contract(&w.adapter_id, || {
        assert!(w
            .env
            .storage()
            .instance()
            .has(&DataKey::PendingRotation(w.ethereum())));
        assert!(w
            .env
            .storage()
            .instance()
            .has(&DataKey::Disabled(w.ethereum())));
        assert!(!w
            .env
            .storage()
            .persistent()
            .has(&DataKey::PendingRotation(w.ethereum())));
        assert!(!w
            .env
            .storage()
            .persistent()
            .has(&DataKey::Disabled(w.ethereum())));
    });
}
