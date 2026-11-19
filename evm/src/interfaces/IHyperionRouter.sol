// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {
    ActionAuditStatus,
    ActionKind,
    AdminAction,
    CancelReason,
    Destination,
    Origin,
    OutboundRequest,
    PendingClaim,
    QueuedAction,
    RouteKind,
    RouteQuote,
    TokenConfig
} from "../HyperionTypes.sol";

/// @title The router, as everything outside it sees it
/// @notice Two entry points carry every transfer. `bridgeOut` is what a person calls, and
/// `bridgeIn` is what a rail's own receiving contract calls once its verifier has already decided
/// the message is real. Hyperion never decides that itself, on either chain, which is the single
/// design decision the rest of this repository is downstream of.
///
/// Everything else here is either a question you can ask before committing money, or an admin
/// action that has to sit in a timelock first.
interface IHYperionRouter {
    /// @notice A transfer left for another chain.
    /// @dev The Soroban router writes a `TransferRecord` to storage as well as emitting, because
    /// Soroban events age out after about a week and an indexer that fell behind has nowhere else
    /// to look. A log on this chain is permanent, so this is the only record and that is enough.
    /// @param route Which rail carried it.
    /// @param sender Who paid.
    /// @param nonce This router's counter for the transfer, and what the far side reads back.
    /// @param token The ERC20 that left.
    /// @param grossAmount What the sender was charged, which is the fee plus the net and can be
    /// less than the amount they asked to send when flooring gave them dust back.
    /// @param fee What Hyperion kept.
    /// @param netAmount What went onto the rail.
    /// @param destinationChain Hyperion's name for where it is headed.
    /// @param destinationAddress The recipient's strkey, as a string, so an indexer never has to
    /// re-encode one to show it.
    /// @param railRef Whatever the rail calls this transfer, or zero if it names it nothing.
    event BridgeOut(
        RouteKind indexed route,
        address indexed sender,
        uint64 indexed nonce,
        address token,
        uint256 grossAmount,
        uint256 fee,
        uint256 netAmount,
        string destinationChain,
        string destinationAddress,
        bytes32 railRef
    );

    /// @notice A transfer arrived and was paid straight through.
    /// @param route Which rail delivered it.
    /// @param recipient Who was paid.
    /// @param token The ERC20 they were paid in.
    /// @param amount How much landed.
    /// @param sourceChain Hyperion's name for where it came from.
    /// @param sourceNonce The far side's counter, which is what joins this to its departure.
    /// @param messageId The rail's own identifier, and the replay key.
    event BridgeIn(
        RouteKind indexed route,
        address indexed recipient,
        address indexed token,
        uint256 amount,
        string sourceChain,
        uint64 sourceNonce,
        bytes32 messageId
    );

    /// @notice A transfer arrived and could not be paid out yet, so the router is holding it.
    /// @dev The money is here and it is theirs. Emitted rather than reverted, because reverting
    /// would send the message back to a rail that has already burned the tokens on the far side.
    /// @param id The claim's id, which is what `settleClaim` takes.
    /// @param recipient Who the money belongs to.
    /// @param token The ERC20 being held.
    /// @param amount How much is being held.
    /// @param route Which rail delivered it.
    /// @param sourceChain Hyperion's name for where it came from.
    /// @param sourceNonce The far side's counter for the departure.
    event ClaimParked(
        uint64 indexed id,
        address indexed recipient,
        address indexed token,
        uint256 amount,
        RouteKind route,
        string sourceChain,
        uint64 sourceNonce
    );

    /// @notice A parked transfer was paid out.
    /// @dev `settledBy` is whoever pressed the button, which does not have to be the recipient.
    /// Anybody can settle somebody else's claim; the money only ever goes to the recipient, so
    /// there is nothing to gain by it and someone watching for stuck transfers can just clear
    /// them.
    /// @param id The claim that was cleared.
    /// @param recipient Who was paid.
    /// @param token The ERC20 they were paid in.
    /// @param amount How much they were paid.
    /// @param settledBy Whoever sent the transaction.
    event ClaimSettled(
        uint64 indexed id, address indexed recipient, address token, uint256 amount, address settledBy
    );

    /// @notice A privileged change was proposed, and the clock started.
    /// @param id The action's id.
    /// @param kind What sort of change it is.
    /// @param eta The earliest timestamp it can be executed at.
    /// @param expiresAt When it stops being executable and has to be queued again.
    event ActionQueued(uint64 indexed id, ActionKind indexed kind, uint64 eta, uint64 expiresAt);

    /// @notice A privileged change waited out its delay and took effect.
    /// @param id The action that was applied.
    /// @param kind What sort of change it was.
    event ActionExecuted(uint64 indexed id, ActionKind indexed kind);

    /// @notice A queued change was dropped before it took effect.
    /// @param id The action that was dropped.
    /// @param by Who dropped it.
    event ActionCancelled(uint64 indexed id, address indexed by);

    /// @notice Emitted on queue, execute, and cancel with bounded audit metadata.
    /// @param version Audit encoding version, currently one.
    /// @param id The original action identifier.
    /// @param status The lifecycle transition.
    /// @param actor The caller responsible for this transition.
    /// @param cancelReason Structured cancellation reason, or Unspecified.
    /// @param reasonNote At most 128 printable ASCII bytes.
    event ActionAuditUpdated(
        uint32 version,
        uint64 indexed id,
        ActionAuditStatus indexed status,
        address indexed actor,
        CancelReason cancelReason,
        string reasonNote
    );

    /// @notice A token's flow limit was tightened without waiting.
    /// @dev Allowed to skip the timelock precisely because it only ever narrows what the bridge
    /// will do. Raising one is a queued action; lowering one is something you want available at
    /// three in the morning.
    /// @param token The token whose ceiling moved.
    /// @param by Who moved it.
    /// @param limit The new ceiling, which is always lower than the old one.
    event FlowLimitLowered(address indexed token, address indexed by, uint256 limit);

    /// @notice Audit metadata for a two-step adapter ownership handover.
    /// @dev Emitted on proposal, acceptance, and cancellation so an indexer can reconstruct
    /// which address was authoritative at every point in the handover.
    /// @param route The rail whose adapter is being handed over.
    /// @param adapter The adapter contract whose ownership is moving.
    /// @param currentOwner The owner at the time of the transition.
    /// @param pendingOwner The proposed owner, or zero once accepted or cancelled.
    /// @param accepted Whether this transition completed the handover.
    event AdapterOwnershipUpdated(
        RouteKind indexed route,
        address indexed adapter,
        address currentOwner,
        address pendingOwner,
        bool accepted
    );

    /// @notice An adapter's ownership handover was proposed.
    /// @dev The pending owner is not authoritative until it accepts. Until then the current
    /// owner retains full control of the adapter and the router continues to recognise it.
    /// @param route The rail whose adapter is being handed over.
    /// @param adapter The adapter contract.
    /// @param currentOwner The owner still in charge.
    /// @param pendingOwner The proposed new owner.
    function proposeAdapterOwner(RouteKind route, address adapter, address pendingOwner)
        external;

    /// @notice The pending owner accepts the adapter handover.
    /// @dev Only the pending owner may accept. Acceptance is one-way: once complete, the
    /// pending owner clears and the former owner loses authority. Repeated acceptance must
    /// revert rather than transfer authority again.
    /// @param route The rail whose adapter is being handed over.
    /// @param adapter The adapter contract.
    function acceptAdapterOwnership(RouteKind route, address adapter) external;

    /// @notice The current owner cancels a pending handover.
    /// @dev Only the current owner may cancel. The pending owner is cleared and the
    /// current owner remains authoritative.
    /// @param route The rail whose adapter handover is being cancelled.
    /// @param adapter The adapter contract.
    function cancelAdapterOwnership(RouteKind route, address adapter) external;

    /// @notice Reports an adapter's actual current and pending owner as the adapter sees them.
    /// @dev This is a view of the adapter's own state, not the router's cache. If the two
    /// disagree, the adapter is the source of truth and the router must be rewired.
    /// @param route The rail to report on.
    /// @param adapter The adapter contract.
    /// @return currentOwner The owner that may configure the adapter now.
    /// @return pendingOwner The owner that may accept next, or zero.
    function adapterOwnership(RouteKind route, address adapter)
        external
        view
        returns (address currentOwner, address pendingOwner);

    /// @notice A token became routable, or its parameters changed.
    /// @param token The ERC20.
    /// @param config Its decimals, its ceiling, and whether it is currently enabled.
    event TokenRegistered(address indexed token, TokenConfig config);

    /// @notice A rail was turned on or off for everybody.
    /// @param route The rail.
    /// @param enabled Whether it now accepts departures. Arrivals are never affected.
    event RouteConfigured(RouteKind indexed route, bool enabled);

    /// @notice A rail was paused individually.
    /// @param route Which rail was paused.
    /// @param caller Who paused it.
    event RoutePaused(RouteKind indexed route, address indexed caller);

    /// @notice A rail was unpaused individually.
    /// @param route Which rail was unpaused.
    /// @param caller Who unpaused it.
    event RouteUnpaused(RouteKind indexed route, address indexed caller);

    /// @notice The adapter that speaks for a rail was pointed somewhere else.
    /// @param route The rail.
    /// @param adapter Its new adapter, or zero when the rail was unwired.
    event AdapterSet(RouteKind indexed route, address indexed adapter);

    /// @notice The contract allowed to call `bridgeIn` for a rail was changed.
    /// @param route The rail.
    /// @param receiver The only address that rail's arrivals may come from.
    event RailReceiverSet(RouteKind indexed route, address indexed receiver);

    /// @notice Where the fee goes, or how much of it there is, changed.
    /// @param treasury Where the fee lands.
    /// @param feeBps The fee in basis xoints, capped at one percent by a constant.
    /// @param timelockDelay How long a queued change now waits, in seconds.
    /// @param flowWindow How long a flow window now runs, in seconds.
    event ConfigChanged(address treasury, uint16 feeBps, uint64 timelockDelay, uint64 flowWindow);

    /// @notice Send `request.amount` of a token to another chain.
    /// @dev Pull, then fee, then floor, then hand off. The fee comes off the raw amount and the
    /// remainder is floored to what the destination can actually represent, in that order, so the
    /// number somebody is quoted is the number that arrives.
    ///
    /// Payable because some rails bill for destination gas at send time. Anything left over is
    /// refunded in the same transaction.
    /// @param request The token, the amount, the rail, the destination, and the least the sender
    /// is willing to see land on the far side.
    /// @return nonce This router's own counter for the transfer, which is also what appears in the
    /// note the far side reads.
    function bridgeOut(OutboundRequest caldata request) external payable returns (uint64 nonce);

    /// @notice Pay out a transfer that arrived over a rail.
    /// @dev Callable only by the contract registered as that rail's receiver, and only after the
    /// rail's own verifier has accepted the message. Hyperion adds a replay guard on top of that
    /// and nothing else: the question of whether the message is genuine was already answered by
    /// somebody with an audit.
    ///
    /// No flow ceiling on this side, deliberately. A limit works by refusing, and refusing an
    /// arrival cannot undo the burn that paid for it, so all a ceiling here would do is strand
    /// somebody's money until an admin raised it. Ceilings belong on departures, where the funds
    /// are still in the sender's wallet and a refusal costs nobody anything.
    /// @param route Which rail this arrival came over.
    /// @param token The ERC20 to pay out.
    /// @param amount How much to pay out, in this chain's decimals for the token.
    /// @param recipient Who to pay.
    /// @param origin The source chain, the far side's nonce, and the rail's message id.
    /// @return claimId Zero when the recipient was paid, or the id of a parked claim when they
    /// could not be.
    function bridgeIn(
        RouteKind route,
        address token,
        uint256 amount,
        address recipient,
        Origin caldata origin
    ) external returns (uint64 claimId);

    /// @notice Pay out a claim the router has been holding.
    /// @dev Permissionless. The funds go to the claim's recipient no matter who calls.
    /// @param claimId The claim to clear.
    function settleClaim(uint64 claimId) external;

    /// @notice What a transfer would cost and whether it would go through, without sending it.
    /// @dev Never reverts for a blocked route. It returns a reason instead, because the app has
    /// to explain the blockage to somebody, and a bare revert gives it nothing to say.
    /// @param route Which rail to price.
    /// @param token The ERC20 being sent.
    /// @param amount The gross amount the sender has in mind.
    /// @param destination Chain name plus the recipient's strkey.
    /// @param destinationDecimals How many decimals the token has on the far side, which the
    /// caller supplies because this chain has no way to read another chain's token.
    /// @return The fee, the net, what lands, the current headroom, and a reason if it is blocked.
    function quote(
        RouteKind route,
        address token,
        uint256 amount,
        Destination callData destination,
        uint8 destinationDecimals
    ) external view returns (RouteQuote memory);

    /// @notice Read the router's current configuration and counters.
    /// @return treasury Where fees go.
    /// @return feeBps The fee in basis points.
    /// @return timelockDelay Seconds a queued action waits.
    /// @return flowWindow Seconds a flow window runs.
    /// @return nextNonce The next outbound nonce.
    /// @return nextActionId The next admin action id.
    /// @return nextClaimId The next claim id.
    function configuration()
        external
        view
        returns (
            address treasury,
            uint16 feeBps,
            uint64 timelockDelay,
            uint64 flowWindow,
            uint64 nextNonce,
            uint64 nextActionId,
            uint64 nextClaimId
        );

    /// @notice Read a token's configuration and current flow window usage.
    /// @param token The ERC20.
    /// @return config The token's decimals, ceiling, and enabled flag.
    /// @return windowStart When the current flow window opened.
    /// @return windowUsed How much has gone out in this window.
    function tokenState(address token)
        external
        view
        returns (TokenConfig memory config, uint64 windowStart, uint256 windowUsed);

    /// @notice Read a rail's current wiring and pause state.
    /// @param route The rail.
    /// @return enabled Whether departures are allowed.
    /// @return paused Whether the rail is paused individually.
    /// @return adapter The adapter contract, or zero.
    /// @return receiver The only address that may call `bridgeIn`, or zero.
    function routeState(RouteKind route)
        external
        view
        returns (bool enabled, bool paused, address adapter, address receiver);

    /// @notice Read a queued admin action.
    /// @param id The action id.
    /// @return The queued action and its current audit status.
    function queuedAction(uint64 id)
        external
        view
        returns (QueuedAction memory action, ActionAuditStatus status);

    /// @notice Read a parked claim.
    /// @param id The claim id.
    /// @return The claim as the router is holding it.
    function claim(uint64 id) external view returns (PendingClaim memory);

    /// @notice Whether a rail's message has already been processed.
    /// @param route The rail.
    /// @param messageId The rail's own identifier.
    /// @return Whether this message is consumed.
    function isConsumed(RouteKind route, bytes32 messageId) external view returns (bool);

    /// @notice Queue a privileged change for execution after the timelock.
    /// @param action The action to queue.
    /// @return id The action id.
    function queueAction(AdminAction caldata action) external returns (uint64 id);

    /// @notice Execute a queued action once its delay has elapsed.
    /// @param id The action id.
    function executeAction(uint64 id) external;

    /// @notice Cancel a queued action before it executes.
    /// @param id The action id.
    /// @param reason Structured cancellation reason.
    /// @param note At most 128 printable ASCII bytes.
    function cancelAction(uint64 id, CancelReason reason, string calldata note) external;

    /// @notice Lower a token's flow ceiling immediately.
    /// @param token The ERC20.
    /// @param limit The new ceiling, which must be lower than the current one.
    function lowerFlowLimit(address token, uint256 limit) external;

    /// @notice Pause a single rail's departures immediately.
    /// @param route The rail to pause.
    function pauseRoute(RouteKind route) external;

    /// @notice Unpause a single rail's departures.
    /// @param route The rail to unpause.
    function unpauseRoute(RouteKind route) external;
}
