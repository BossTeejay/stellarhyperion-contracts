# Parked claim operations

The Soroban router keeps inbound funds in custody when the token rejects a payout. Settlement
always uses the recipient, token, and integer amount recorded when the authenticated rail
delivered the message. The caller of settlement cannot redirect those funds.

`get_claim_provenance(id)` returns the rail, source chain, and complete 32-byte rail message ID
for claims created with the provenance sidecar. Claims created before this addition have no
sidecar (`None`); their original claim record, including its source chain, route, and source
nonce, remains readable and settleable. No rewrite of old claim storage is required.

`claim_liability(token)` reports the tracked sum of unsettled claims alongside the router's
token balance. Counters are updated as new claims are parked and settled, so the read does not
scan an unbounded claim history. Treat `known == false` as indeterminate, never as solvent. A
failed balance read or an upgraded instance without the tracking marker makes the result
unknown. Existing claim records remain settleable; an older deployment needs an explicit
accounting migration before its liability view can be treated as complete. The view grants no
withdrawal or recovery authority and is not a substitute for monitoring the underlying token
and the router's custody.

`preview_claim_recipient(id)` is a read-only best-effort diagnostic. It can identify a missing
Stellar Asset Contract trustline and an unauthorized Stellar Asset Contract account. Other token
implementations return `Unknown` because a balance read cannot establish that a transfer will
succeed.
`Eligible` is only a point-in-time observation, not a promise that a later transfer will succeed.
Settlement remains the source of truth and never changes the saved recipient.

Pausing disables departures. It does not disable authenticated inbound arrivals or settlement,
so an already-finalized bridge message can be credited and a parked claim can be paid while the
guardian has paused outbound activity. The configured rail receiver and replay guard remain
mandatory in this path.
