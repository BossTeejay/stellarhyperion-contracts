# Axelar trusted-peer rotation

Live peer changes use `propose_peer_rotation` / `apply_peer_rotation` on Soroban or `proposePeerRotation` / `applyPeerRotation` on EVM. The minimum delay is 3,600 seconds. Proposals do not change active trust. Application at or after the recorded deadline changes the peer atomically and clears the proposal. A proposal must be cancelled before it can be replaced; timestamp overflow refuses the proposal without changing trust.

| Lane state                | Active peer deliveries      | Proposed peer deliveries             | Dispatch           |
| ------------------------- | --------------------------- | ------------------------------------ | ------------------ |
| Enabled, rotation pending | Accepted with normal checks | Refused                              | Uses existing lane |
| Enabled, rotation applied | Retired peer refused        | New peer accepted with normal checks | Uses rotated lane  |
| Disabled                  | Refused                     | Refused                              | Refused            |
| Unlinked / removed        | Refused                     | Refused                              | Refused            |

Emergency disable and re-enable require the current administrator and affect an already configured lane. Applying or cancelling a rotation never re-enables a disabled lane. Disable can hold previously dispatched messages while a fault is investigated; once re-enabled, the active peer and ordinary replay guard still determine which deliveries can execute.

Soroban `link_chain` remains available for initial setup and identical deployment reruns. It rejects peer replacement, chain renaming and reverse-name collisions. These changes require a reviewed migration instead of changing a lane through the original setup function. EVM `setPeer` remains once only. Empty or all-zero EVM wire addresses are refused without imposing a new chain-specific address length.

Unlink/removal is permanent for both the Hyperion chain name and its Axelar name in that adapter instance. It deletes the active forward/reverse mapping and any pending rotation; retired-name records prevent remove-and-relink or aliasing from bypassing the delay. Use emergency disable for reversible maintenance. A retired lane that must return requires a separately governed replacement adapter and the existing router activation process.

Existing Soroban `Config`, `Link`, `Source`, `TokenId` and `Token` formats and storage classes remain unchanged. New pending rotation, disable and retired-name metadata lives in instance storage, alongside configuration, so it is renewed and restored with the adapter. Permissionless maintenance still counts the two physical persistent peer mapping keys and renews the instance. It cannot apply a proposal, change trust, enable a lane or erase a retirement. The released adapter has no earlier pending/disable metadata; development snapshots from before this repair that placed it in persistent storage need an explicit state migration or fresh adapter before use.

GMP payload sender trust and ITS token-manager trust are distinct. GMP checks the authenticated ITS caller and exact active peer address before crediting the note's recipient. Bare ITS dispatch does not use a peer payload; token mappings and manager authorization still govern exact token movement. This change preserves note encodings, integer amounts, gas authorization, replay identity and recipient-bound claims. Accepted claims remain payable to their recorded recipient after peer rotation, disable or removal. Old-peer messages still in flight at cutover are refused and require operational recovery; there is no old-peer grace period after application.

The active deployment remains Stellar Testnet outbound ITS/CCTP. EVM contracts, inbound GMP and peer migrations remain development paths until independently validated under the existing testnet and deployment gates. No deployment or activation is authorized by these APIs or this document. Local regressions cover delay boundaries, setup/removal bypasses, alias collisions, proposal replacement/overflow, authorization, disable preservation, storage maintenance, replay and claim continuity. Test execution and generated ABI verification are recorded separately from source preparation.
