//! Axelar's Interchain Token Service, as this adapter calls it.
//!
//! Declared here rather than pulled in as a dependency, for the same reason Circle's contracts
//! are. Axelar's Stellar packages are not on crates.io, and vendoring a git revision of somebody
//! else's workspace into a contract build is a supply chain decision rather than a convenience.
//! Soroban resolves a cross contract call by function name and argument shape at invocation time,
//! so a transcription that has drifted fails immediately and loudly on the first call rather than
//! quietly doing the wrong thing.
//!
//! # Paid Dispatch Authorization Trees and Privilege Expansion
//!
//! Axelar paid ITS dispatch authenticates the sender and native gas payment in a single atomic authorization
//! tree signed by the source wallet:
//! - **Nested authorization structure**:
//!   1. Root invocation: `router.bridge_out_with_gas(user, request, gas_amount)`
//!   2. Fee debit: `token.transfer(user, treasury, fee_amount)`
//!   3. Adapter dispatch: `adapter.dispatch_from(router, user, token, net_amount, destination_chain, destination, nonce, gas_amount)`
//!      - ITS transfer: `its.interchain_transfer(user, token_id, destination_chain, recipient, net_amount, None, gas_token)`
//!        - Principal debit: `token.burn(user, net_amount)` (for mint/burn managers) OR `token.transfer(user, manager, net_amount)` (for lock/unlock)
//!        - Gas debit: `gas_token.transfer(user, its_gas_recipient, gas_amount)`
//!
//! - **Privilege expansion protection**:
//!   Before signing, the simulated authorization tree must be verified against expected snapshot invariants:
//!   - **Spender identity**: Only the configured treasury, registered token manager, or token burn may be authorized.
//!   - **Gas recipient**: Only the trusted ITS contract or native gas service may receive the gas payment.
//!   - **Exact integer amounts**: Principal amount, router fee, and gas payment must match exact integers without inflation.
//!   - **Zero unapproved calls**: Any unexpected sub-invocation (e.g. extraneous token transfers, approvals, or admin calls)
//!     is treated as a critical privilege escalation and rejected.
//!
//! - **Offline snapshot inspection**:
//!   Snapshot generation and inspection require no private keys and no network broadcast. Simulation footprints and
//!   authorization specs are verifiable entirely offline or through simulated view calls.
//!
//! # Deployment Gating
//!
//! The active deployment is Stellar Testnet outbound ITS/CCTP. EVM contracts, GMP inbound execution, and other rail
//! development remain strictly gated until independently validated.
//!
//! Transcribed from `axelarnetwork/axelar-amplifier-stellar` on branch `main`:
//!
//! - `contracts/stellar-interchain-token-service/src/interface.rs`
//! - `contracts/stellar-interchain-token-service/src/types.rs`
//! - `packages/stellar-axelar-std/src/types.rs`

use soroban_sdk::{contractclient, contracttype, Address, Bytes, BytesN, Env, String};

/// What Axelar charges gas in, and how much of it.
///
/// Paid transfers attach this exact amount through ITS; the source account authorizes it.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Token {
    pub address: Address,
    pub amount: i128,
}

/// How a token id is backed on this chain, which decides how ITS takes funds from a sender.
///
/// A unit only enum carrying `#[repr(u32)]` under `#[contracttype]` encodes as a plain `u32`, so
/// matching Axelar's discriminants here is enough for wire compatibility without linking their
/// crate. The gap at three is real: Axelar has `LockUnlockFee = 3` commented out because Stellar
/// ITS does not support it, and leaving the hole makes that visible rather than renumbering
/// around it.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum TokenManagerType {
    NativeInterchainToken = 0,
    MintBurnFrom = 1,
    LockUnlock = 2,
    MintBurn = 4,
}

impl TokenManagerType {
    /// Whether ITS takes funds by burning them out of the sender's balance.
    ///
    /// Three of the four do. Only `LockUnlock` moves the funds somewhere, which is the one case
    /// where the authorisation this adapter grants has to name a third party.
    pub fn takes_by_burning(&self) -> bool {
        !matches!(self, TokenManagerType::LockUnlock)
    }
}

#[contractclient(name = "InterchainTokenServiceClient")]
pub trait InterchainTokenService {
    fn native_token_address(env: Env) -> Address;
    fn gas_service(env: Env) -> Address;
    /// Send an interchain token to another chain, optionally with a payload for the recipient.
    ///
    /// `caller` pays: ITS burns from or transfers out of that address, so whoever is named here
    /// has to have authorised the movement. `metadata` carries the payload that turns a plain
    /// transfer into a contract call on the far side, and `gas_token` is the relayer fee.
    #[allow(clippy::too_many_arguments)]
    fn interchain_transfer(
        env: Env,
        caller: Address,
        token_id: BytesN<32>,
        destination_chain: String,
        destination_address: Bytes,
        amount: i128,
        metadata: Option<Bytes>,
        gas_token: Option<Token>,
    );

    /// Whether Axelar's hub will route to this chain at all.
    fn is_trusted_chain(env: Env, chain: String) -> bool;

    /// The local asset a token id resolves to.
    ///
    /// Read at configuration time so a mapping that disagrees with Axelar is caught once, by an
    /// admin, rather than on every dispatch as a panic from two contracts away.
    fn registered_token_address(env: Env, token_id: BytesN<32>) -> Address;

    /// The token manager holding the locked side of a `LockUnlock` token id.
    fn deployed_token_manager(env: Env, token_id: BytesN<32>) -> Address;

    /// How ITS will take the funds, which decides what this adapter has to authorise.
    fn token_manager_type(env: Env, token_id: BytesN<32>) -> TokenManagerType;

    /// Axelar's own flow limit on a token id, if its operator set one.
    fn flow_limit(env: Env, token_id: BytesN<32>) -> Option<i128>;
}
