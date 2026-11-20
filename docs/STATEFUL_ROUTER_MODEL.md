# Stateful router reference

Issue #144 is exercised against the real native Soroban router and the deployed EVM router in
Foundry. `hyperion-core/src/model.rs` is an allocation-free, bounded off-chain specification.
It does not call the production fee, window, quote, storage or authorization implementation.
The Foundry model likewise computes expected state independently, and a common Rust-generated
corpus checks that its specification agrees with the Rust model after every operation.

The model tracks one admitted token, four live outbound rails and one authenticated CCTP
inbound receiver. The six accounts are sender, treasury, combined adapter/receiver balance,
router and two fixed recipients. Both sender and receiver are funded once, before the sequence;
no operation mints tokens. Each step checks all balances, conservation, per-rail flow headroom,
fee configuration, nonce and action/claim counters. Every claim's amount, original recipient
and settled flag are compared, and the sum of unsettled liabilities must equal router collateral.
Failed operations must leave the observed state unchanged.

## Clocks and differences

Soroban flow usage advances with ledger sequence; its timelock and claim timestamps use Unix
seconds. EVM flow and timelock use block timestamps. Advancing only one clock does not advance
the other. The fixtures use the deployed policy defaults:720 ledgers/3600 seconds for Soroban,
3600 seconds/2 days for EVM. Fee expiry is7 days after maturity. Soroban departure floors7→6 decimal
net amounts; EVM expands6→7 decimals. Duplicate pause/unpause is idempotent on Soroban and an
OpenZeppelin refusal on EVM. The model names these differences instead of hiding them.

## Portable operations and reproduction

Both drivers decode a 64-bit decimal word. Kind is `(word & 255) % 10`; `small = word >> 8`;
`arg = word >> 16`. These fields deliberately overlap; the word itself is the reproduction.

| Kind | Operation                 | Parameters                                                                |
| ---- | ------------------------- | ------------------------------------------------------------------------- |
| 0    | Dispatch                  | rail=`small % 4`, amount=`arg % 1500000000`                               |
| 1    | Inbound                   | amount as above, recipient=`small % 2`, message nonce=`(word >> 10) % 16` |
| 2    | Recipient freeze          | recipient=`small % 2`, blocked=`arg % 2 != 0`                             |
| 3    | Permissionless settlement | claim ID=`small % 20 + 1`                                                 |
| 4/5  | Pause/unpause             | actor=`small % 3`                                                         |
| 6    | Queue fee change          | actor as above, bps=`arg % 121`                                           |
| 7/8  | Execute/cancel            | actor as above, relative action ID=`arg % 20 + 1`                         |
| 9    | Advance clocks            | seconds=`arg % 800000`, ledgers=`small % 1500`                            |

Actor 0 is admin, 1 guardian, 2 unrelated caller. Relative action IDs exclude fixture setup.
The nonce range intentionally generates replays, while the two recipients generate independent
liabilities. The bounds keep native-host campaigns short and keep claims/actions within fixed
arrays; they are test limits, not new contract limits.

Run from `soroban`:

```sh
cargo test --locked -p hyperion-router stateful_model -- --nocapture
cargo run --locked -p hyperion-core --example model_vectors -- ../testdata --check
```

Remove `--check` only when intentionally regenerating the checked-in corpus after reviewing a
specification change. Generation is deterministic and checks conservation at every step.
The 64-step corpus contains a deliberate prefix for frozen claims, inbound while paused,
replay, early/mature/expired/cancelled actions, precision and flow boundaries, plus seeded words.

Run from `evm` with the repository-pinned Foundry version:

```sh
forge test --match-path test/StatefulModel.t.sol -vv
FOUNDRY_PROFILE=ci forge test --match-path test/StatefulModel.t.sol -vv
```

Proptest generates variable-length sequences and shrinks both words and length; it prints the
minimal failing input and retains a regression seed. Set `PROPTEST_RNG_SEED` to replay a seed.
Foundry fuzzes arbitrary words and its invariant campaign shrinks call sequences. Replay with
the printed fuzz seed or persisted failure. Invariant `fail-on-revert=true` makes a handler
assertion fatal; ordinary expected router refusals are caught and compared inside the handler.
`Trace::minimize` also supports 1-minimal deletion traces by rerunning a caller-provided failure
predicate against freshly initialized worlds; a synthetic missing-liability fault verifies it.

This intentionally small model does not claim rail-network behavior, adapter-specific fees,
upgrade/migration, TTL expiry or every governance action. Their dedicated suites remain required.
No production ABI, storage representation, optimizer policy or lint/security exemption is added.
