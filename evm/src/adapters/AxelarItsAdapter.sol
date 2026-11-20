// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {Ownable} from "openzeppelin/access/Ownable.sol";
import {Ownable2Step} from "openzeppelin/access/Ownable2Step.sol";
import {IERC20} from "openzeppelin/token/ERC20/IERC20.sol";
import {SafeERC20} from "openzeppelin/token/ERC20/utils/SafeERC20.sol";

import {
    AlreadyConfigured,
    InvalidAmount,
    RailNotConfigured,
    RefundFailed,
    TokenNotMapped,
    Unauthorized,
    UnexpectedRailContract,
    UnknownChain,
    ZeroAddress
} from "../HyperionErrors.sol";
import {
    AddressKind,
    Destination,
    Origin,
    RouteKind,
    TokenMappingConflict,
    TokenMappingReport,
    TokenMappingRequest
} from "../HyperionTypes.sol";
import {
    IAxelarGasService,
    IInterchainTokenExecutable,
    IInterchainTokenService,
    ITokenManager
} from "../interfaces/IAxelar.sol";
import {IHyperionRouter} from "../interfaces/IHyperionRouter.sol";
import {IRailAdapter} from "../interfaces/IRailAdapter.sol";
import {HyperionNotes} from "../libraries/HyperionNotes.sol";
import {StellarAddress} from "../libraries/StellarAddress.sol";

/// @title Hyperion over Axelar's Interchain Token Service
/// @author dotmantissa
/// @notice The rail that goes both ways. Axelar carries a payload alongside the money, which is
/// what lets a delivery say who it is for, and its validator set is what decides whether a message
/// is real. Hyperion never makes that decision on either side.
///
/// Sending, this writes a note naming a Stellar destination and hands ITS the tokens. Receiving,
/// ITS transfers the tokens in and then calls `executeWithInterchainToken`, and this reads the note
/// to find an EVM address before handing the whole thing to the router.
///
/// The one check here that is load bearing is the peer comparison. ITS will happily deliver a
/// transfer with a payload from any contract on any connected chain, so the fact that a message
/// arrived through ITS says nothing about who sent it. The source address has to be Hyperion's own
/// contract on a chain Hyperion configured, and the comparison is on the raw bytes, because chains
/// disagree about how long an address is and about whether it is even binary.
contract AxelarItsAdapter is IRailAdapter, IInterchainTokenExecutable, Ownable2Step {
    using SafeERC20 for IERC20;

    /// @notice The value ITS requires a receiving contract to return from `executeWithInterchainToken`.
    /// @dev What ITS insists a receiving contract hands back. Not reverting is not enough: Axelar
    /// wants proof the contract meant to accept this, rather than having a fallback that swallowed
    /// it without looking.
    bytes32 public constant EXECUTE_SUCCESS = keccak256("its-execute-success");

    /// @dev Axelar's own envelope version for "there is a payload and it is for a contract".
    uint32 private constant METADATA_CONTRACT_CALL = 0;

    /// @notice The router. The only address allowed to send, and the only address this delivers to.
    address public immutable ROUTER;

    /// @notice Axelar's Interchain Token Service on this chain.
    IInterchainTokenService public immutable ITS;

    /// @notice Axelar's gas service, for topping up a delivery that ran short.
    IAxelarGasService public immutable GAS_SERVICE;

    /// @notice Hyperion on the far side of one connected chain.
    struct Peer {
        /// Axelar's own name for the chain, which is not always Hyperion's name for it.
        string axelarChain;
        /// Hyperion's contract over there, in whatever form that chain's ITS puts on the wire.
        /// Twenty bytes for an EVM chain, and the string form of a contract id for Stellar.
        bytes peer;
        bool configured;
        bool enabled;
    }

    mapping(string chain => Peer peer) private _peers;

    /// @dev Reverse lookup, so an inbound delivery can check the chain it names against the peer
    /// that is allowed to speak for it without the caller telling us which key to look under.
    mapping(string axelarChain => string chain) private _byAxelarName;

    /// @notice The ITS token id this chain's ERC20 is registered under, or zero if it is not
    /// linked. An unlinked token is a token this adapter will refuse to send.
    mapping(address token => bytes32 tokenId) public tokenIdOf;

    /// @dev And back again, which is what makes the inbound allowlist and the reverse half of a
    /// registration conflict answerable without an event scan. Kept in step with `tokenIdOf` by
    /// `linkToken` and by nothing else.
    mapping(bytes32 tokenId => address token) private _tokenOfId;

    /// @notice The token manager kind Axelar uses for a token id, cached when it was linked.
    /// @dev Not consulted by any transfer path: `dispatch` and `executeWithInterchainToken` read
    /// nothing from here. It exists so a dry run can compare what an operator believes against
    /// what Axelar said at registration time, and it is refreshed only by a successful link.
    mapping(bytes32 tokenId => uint256 managerType) private _managerTypeOfId;

    /// @notice A chain became reachable.
    event PeerConfigured(string chain, string axelarChain, bytes peer);

    /// @notice A chain was turned on or off.
    event PeerEnabled(string chain, bool enabled);

    /// @notice A token became routable over ITS.
    event TokenLinked(address indexed token, bytes32 tokenId);

    /// @notice A transfer was handed to ITS.
    event TransferSubmitted(
        string chain, bytes32 tokenId, uint256 amount, uint64 nonce, uint256 gasValue, bytes note
    );

    /// @notice A delivery arrived from a peer and was passed to the router.
    event DeliveryAccepted(
        string sourceChain,
        bytes32 indexed commandId,
        address indexed recipient,
        uint256 amount,
        uint64 sourceNonce
    );

    constructor(address router, address its, address gasService, address owner_) Ownable(owner_) {
        if (router == address(0) || its == address(0) || gasService == address(0)) revert ZeroAddress();
        ROUTER = router;
        ITS = IInterchainTokenService(its);
        GAS_SERVICE = IAxelarGasService(gasService);
    }

    /// @dev Axelar refunds unspent destination gas by sending it back to whoever paid, which is
    /// this contract, and it does so from its relayer in a later transaction rather than inside
    /// the call that paid. Without this the refund bounces and the money is gone, so the door has
    /// to be open even though nothing here ever wants native currency.
    ///
    /// What arrives this way is swept to the router by the next `dispatch`, where it leaves as
    /// change to that transfer's sender. That is not the person who overpaid, and pretending
    /// otherwise would mean keeping a ledger of gas refunds per transfer, off a rail that does
    /// not tell us which transfer a refund belongs to. Approximately returned beats precisely
    /// stranded.
    /// @notice Accepts the gas Axelar refunds after a delivery cost less than it was paid for.
    receive() external payable {}

    /// @inheritdoc IRailAdapter
    function route() external pure override returns (RouteKind) {
        return RouteKind.AxelarIts;
    }

    /// @inheritdoc IRailAdapter
    function supportsChain(string calldata chain) external view override returns (bool) {
        Peer storage peer = _peers[chain];
        return peer.configured && peer.enabled;
    }

    /// @inheritdoc IRailAdapter
    /// @dev Zero, because there is no honest on-chain answer. Axelar prices destination gas from
    /// the destination chain's own gas market, which this chain cannot see. The app asks Axelar's
    /// estimator and sends that much native currency; whatever is not wanted comes back in the
    /// same transaction. A made up number here would look authoritative and be wrong.
    function quoteFee(string calldata, uint256) external pure override returns (uint256) {
        return 0;
    }

    /// @inheritdoc IRailAdapter
    function dispatch(address token, uint256 amount, Destination calldata destination, uint64 nonce)
        external
        payable
        override
        returns (bytes32)
    {
        if (msg.sender != ROUTER) revert Unauthorized();
        if (amount == 0) revert InvalidAmount();

        bytes32 tokenId = tokenIdOf[token];
        if (tokenId == bytes32(0)) revert TokenNotMapped();

        Peer memory peer = _peers[destination.chain];
        if (!peer.configured) revert UnknownChain();
        if (!peer.enabled) revert RailNotConfigured();

        (AddressKind kind,,) = StellarAddress.parse(destination.strkey);
        bytes memory note = HyperionNotes.encodeInboundNote(kind, destination.strkey, nonce);

        emit TransferSubmitted(destination.chain, tokenId, amount, nonce, msg.value, note);

        IERC20(token).forceApprove(address(ITS), amount);
        ITS.interchainTransfer{value: msg.value}(
            tokenId,
            peer.axelarChain,
            peer.peer,
            amount,
            abi.encodePacked(METADATA_CONTRACT_CALL, note),
            msg.value
        );

        // ITS refunds what the destination gas market did not want, and none of it is this
        // contract's to keep.
        _refund();

        // ITS does not hand back a message id. The delivery is identified by the log Axelar's
        // gateway writes in this same transaction, which the indexer reads, so inventing a value
        // here would only give somebody a handle that resolves to nothing.
        return bytes32(0);
    }

    /// @inheritdoc IInterchainTokenExecutable
    /// @dev The tokens are already here by the time this runs. ITS transfers first and calls
    /// second, which is why this reads its own balance movement from the arguments rather than
    /// pulling anything.
    function executeWithInterchainToken(
        bytes32 commandId,
        string calldata sourceChain,
        bytes calldata sourceAddress,
        bytes calldata data,
        bytes32 tokenId,
        address token,
        uint256 amount
    ) external override returns (bytes32) {
        if (msg.sender != address(ITS)) revert Unauthorized();
        if (amount == 0) revert InvalidAmount();
        if (tokenIdOf[token] != tokenId) revert TokenNotMapped();

        string memory chain = _byAxelarName[sourceChain];
        Peer storage peer = _peers[chain];
        if (!peer.configured || !peer.enabled) revert RailNotConfigured();
        // Arriving through ITS proves a message was delivered, not who sent it. Any contract on
        // any connected chain can call ITS with a payload, so this is the check that turns "a
        // transfer arrived" into "Hyperion sent this".
        if (keccak256(sourceAddress) != keccak256(peer.peer)) revert UnexpectedRailContract();

        (address recipient, uint64 sourceNonce) = HyperionNotes.decodeOutboundNote(data);

        IERC20(token).forceApprove(ROUTER, amount);
        IHyperionRouter(ROUTER)
            .bridgeIn(
                RouteKind.AxelarIts,
                token,
                amount,
                recipient,
                Origin({
                    chain: chain,
                    nonce: sourceNonce,
                    // Axelar's command id is unique per delivery and is what its own replay protection
                    // is keyed on, so the router keys its replay guard on the same thing rather than
                    // on a nonce the far side chose.
                    messageId: commandId,
                    sender: keccak256(sourceAddress)
                })
            );
        // An approval that outlives the call it was for is an approval somebody eventually finds.
        IERC20(token).forceApprove(ROUTER, 0);

        emit DeliveryAccepted(chain, commandId, recipient, amount, sourceNonce);
        return EXECUTE_SUCCESS;
    }

    /// @notice Pay Axelar more gas for a delivery that ran short.
    /// @dev Permissionless, because a stuck transfer is somebody's money and the person most
    /// motivated to unstick it is not always the operator. Whoever pays gets nothing back except
    /// the transfer going through, which is the only incentive that needs to exist here.
    function topUpGas(bytes32 txHash, uint256 logIndex, address refundAddress) external payable {
        if (msg.value == 0) revert InvalidAmount();
        if (refundAddress == address(0)) revert ZeroAddress();
        GAS_SERVICE.addNativeGas{value: msg.value}(txHash, logIndex, refundAddress);
    }

    /// @notice Make a chain reachable, and name the contract allowed to speak for it.
    /// @dev Once only, like the CCTP adapter's lanes. Repointing a live peer would let a different
    /// contract deliver on a chain people are already using, which is not a configuration change,
    /// it is a new trust assumption, and those go through the router's timelock by way of a new
    /// adapter.
    function setPeer(string calldata chain, string calldata axelarChain, bytes calldata peerAddress)
        external
        onlyOwner
    {
        if (bytes(chain).length == 0 || bytes(axelarChain).length == 0) revert UnknownChain();
        if (peerAddress.length == 0) revert ZeroAddress();
        Peer storage peer = _peers[chain];
        if (peer.configured) revert AlreadyConfigured();
        if (bytes(_byAxelarName[axelarChain]).length != 0) revert AlreadyConfigured();

        peer.axelarChain = axelarChain;
        peer.peer = peerAddress;
        peer.configured = true;
        peer.enabled = true;
        _byAxelarName[axelarChain] = chain;

        emit PeerConfigured(chain, axelarChain, peerAddress);
        emit PeerEnabled(chain, true);
    }

    /// @notice Turn a configured chain on or off.
    function setPeerEnabled(string calldata chain, bool enabled) external onlyOwner {
        Peer storage peer = _peers[chain];
        if (!peer.configured) revert RailNotConfigured();
        peer.enabled = enabled;
        emit PeerEnabled(chain, enabled);
    }

    /// @notice Point a local token at its ITS token id.
    /// @dev Once only. An id is the same value on every chain ITS connects, so getting it right
    /// once is the whole job, and being able to change it later is only useful to somebody who
    /// wants a transfer to arrive as a different asset than it left as.
    ///
    /// A link that is already in place is never rewritten. Where the stored link disagrees with
    /// the arguments the call is refused with `AlreadyConfigured` rather than quietly overwritten,
    /// because the reverse mapping is the inbound allowlist and repointing it would change which
    /// asset an arriving delivery is paid in. Repeating the same link is accepted and writes
    /// nothing, so a deploy script that runs twice is boring. To see exactly which fields
    /// disagree, call `checkTokenMapping` first.
    function linkToken(address token, bytes32 tokenId) external onlyOwner {
        if (token == address(0)) revert ZeroAddress();
        if (tokenId == bytes32(0)) revert TokenNotMapped();

        TokenMappingReport memory report = _inspectTokenMapping(
            TokenMappingRequest({
                token: token,
                tokenId: tokenId,
                expectedDecimals: 0,
                hasExpectedDecimals: false,
                expectedManagerType: 0,
                hasExpectedManagerType: false
            })
        );
        if (report.conflicts.length != 0) {
            // Two different mistakes with two different fixes. A link that would repoint an
            // existing mapping is a configuration clash; anything else is Axelar not knowing the
            // asset, which the caller has to resolve before the link means anything.
            TokenMappingConflict first = report.conflicts[0];
            if (first == TokenMappingConflict.TokenId || first == TokenMappingConflict.LocalToken) {
                revert AlreadyConfigured();
            }
            revert TokenNotMapped();
        }

        // The link already reads back exactly as proposed. Writing it again would only republish
        // an event for a change that did not happen, so this is a successful no-op.
        if (report.mappedTokenId == tokenId && report.mappedToken == token) return;

        tokenIdOf[token] = tokenId;
        _tokenOfId[tokenId] = token;
        _managerTypeOfId[tokenId] = report.railManagerType;
        emit TokenLinked(token, tokenId);
    }

    /// @notice Report what linking `token` to `tokenId` would do, without doing any of it.
    /// @dev The EVM half of the Stellar adapter's `check_token_mapping`, and it answers the same
    /// question in the same order: every field that disagrees, named, rather than the first one
    /// that would have caused a revert. A caller passes `expectedDecimals` and
    /// `expectedManagerType` to have those judged; leaving them out reports what was observed
    /// without judging it.
    ///
    /// Read only. Nothing here writes storage or emits an event, so calling it twice leaves the
    /// adapter exactly as it found it, which is what makes it usable as a rehearsal step in a
    /// deployment script.
    /// @param token The local ERC20 the caller proposes to link.
    /// @param tokenId The ITS token id the caller proposes to link it to.
    /// @param expectedDecimals Decimals the caller expects the local asset to report.
    /// @param hasExpectedDecimals Whether `expectedDecimals` should be judged at all.
    /// @param expectedManagerType The `TokenManagerType` discriminant the caller expects.
    /// @param hasExpectedManagerType Whether `expectedManagerType` should be judged at all.
    function checkTokenMapping(
        address token,
        bytes32 tokenId,
        uint8 expectedDecimals,
        bool hasExpectedDecimals,
        uint256 expectedManagerType,
        bool hasExpectedManagerType
    ) external view returns (TokenMappingReport memory) {
        return _inspectTokenMapping(
            TokenMappingRequest({
                token: token,
                tokenId: tokenId,
                expectedDecimals: expectedDecimals,
                hasExpectedDecimals: hasExpectedDecimals,
                expectedManagerType: expectedManagerType,
                hasExpectedManagerType: hasExpectedManagerType
            })
        );
    }

    /// @notice The local asset already linked to a token id, or the zero address.
    /// @dev The reverse half of `tokenIdOf`, and the map the inbound path's allowlist check
    /// effectively uses.
    function tokenOfId(bytes32 tokenId) external view returns (address) {
        return _tokenOfId[tokenId];
    }

    /// @notice The manager kind Axelar reported for a token id when it was linked.
    /// @dev Zero is both "no manager recorded" and `NativeInterchainToken`, so this is only
    /// meaningful for an id where `tokenIdOf` is non-zero.
    function managerTypeOfId(bytes32 tokenId) external view returns (uint256) {
        return _managerTypeOfId[tokenId];
    }

    /// @dev Build the report `checkTokenMapping` returns and `linkToken` acts on.
    ///
    /// Every read is allowed to fail. ITS reverts for an id it has no manager for, and a token
    /// that is not a real ERC20 answers nothing; neither is an error here, it is a fact the
    /// report carries so the caller can tell "Axelar has never heard of this id" apart from
    /// "Axelar knows the id and it belongs to a different asset".
    function _inspectTokenMapping(TokenMappingRequest memory request)
        private
        view
        returns (TokenMappingReport memory report)
    {
        report.token = request.token;
        report.tokenId = request.tokenId;
        report.mappedTokenId = tokenIdOf[request.token];
        report.mappedToken = _tokenOfId[request.tokenId];

        // What Axelar itself says is behind this id.
        try ITS.registeredTokenAddress(request.tokenId) returns (address registered) {
            report.railToken = registered;
        } catch {}
        try ITS.tokenManagerAddress(request.tokenId) returns (address manager) {
            report.railManager = manager;
        } catch {}
        if (report.railManager != address(0)) {
            // A static call rather than a typed one: `try` catches a revert but not a decode
            // failure, and an address with no code returns nothing at all. A manager that cannot
            // be read is a fact this report carries, not a reason to fail the whole check.
            (bool okKind, bytes memory kindRet) = report.railManager
                .staticcall(abi.encodeWithSelector(ITokenManager.implementationType.selector));
            if (okKind && kindRet.length == 32) {
                report.railManagerType = abi.decode(kindRet, (uint256));
                report.managerTypeKnown = true;
            }
        }

        // A token that cannot answer `decimals()` is not an error, it is a token whose decimals
        // are unknown, which is a different finding from a token whose decimals are wrong.
        (bool ok, bytes memory ret) = request.token.staticcall(abi.encodeWithSignature("decimals()"));
        if (ok && ret.length == 32) {
            uint256 raw = abi.decode(ret, (uint256));
            if (raw <= type(uint8).max) {
                // Safe: the guard above is exactly the range check.
                // forge-lint: disable-next-line(unsafe-typecast)
                report.onchainDecimals = uint8(raw);
                report.decimalsKnown = true;
            }
        }

        report.conflicts = _classifyTokenMapping(request, report);
    }

    /// @dev Derive every disagreement from the observations, in the fixed order the Stellar side
    /// uses. Pure and separated from the reads, because these are the rules both chains have to
    /// agree on and the order is what makes two runs over the same facts print the same list.
    ///
    /// Two passes rather than a push: the length of the array has to be known before it is
    /// allocated, and computing the findings twice in a pure function is cheaper than growing an
    /// array across five branches.
    function _classifyTokenMapping(TokenMappingRequest memory request, TokenMappingReport memory report)
        private
        pure
        returns (TokenMappingConflict[] memory conflicts)
    {
        // A link that would move an asset already linked elsewhere silently changes where money
        // goes. Both directions are checked because they fail differently: the forward one breaks
        // outbound dispatch, the reverse one makes an inbound delivery pay a token nobody linked.
        bool idConflict = report.mappedTokenId != bytes32(0) && report.mappedTokenId != request.tokenId;
        bool tokenConflict = report.mappedToken != address(0) && report.mappedToken != request.token;
        bool railTokenConflict = report.railToken != address(0) && report.railToken != request.token;
        // A zero `railToken` is the rail not knowing the id, which is also the answer to "is there
        // a manager": ITS registers the two together.
        bool unregistered = report.railToken == address(0);
        // Judged on whether a kind was read, not on its value: zero is a real kind, so a
        // `NativeInterchainToken` manager reporting `0` is not the same as no manager at all.
        bool managerMissing = request.hasExpectedManagerType && !report.managerTypeKnown;
        bool decimalsConflict = request.hasExpectedDecimals && report.decimalsKnown
            && report.onchainDecimals != request.expectedDecimals;
        bool decimalsUnreadable = request.hasExpectedDecimals && !report.decimalsKnown;
        bool managerConflict = request.hasExpectedManagerType && report.managerTypeKnown
            && report.railManagerType != request.expectedManagerType;

        uint256 size;
        if (idConflict) ++size;
        if (tokenConflict) ++size;
        if (unregistered) ++size;
        if (railTokenConflict) ++size;
        if (decimalsUnreadable) ++size;
        if (decimalsConflict) ++size;
        if (managerMissing) ++size;
        if (managerConflict) ++size;

        conflicts = new TokenMappingConflict[](size);
        uint256 cursor;
        if (idConflict) conflicts[cursor++] = TokenMappingConflict.TokenId;
        if (tokenConflict) conflicts[cursor++] = TokenMappingConflict.LocalToken;
        if (unregistered) conflicts[cursor++] = TokenMappingConflict.UnregisteredTokenId;
        if (railTokenConflict) conflicts[cursor++] = TokenMappingConflict.RailTokenMismatch;
        if (decimalsUnreadable) conflicts[cursor++] = TokenMappingConflict.UnreadableMetadata;
        if (decimalsConflict) conflicts[cursor++] = TokenMappingConflict.Decimals;
        if (managerMissing) conflicts[cursor++] = TokenMappingConflict.UnregisteredTokenId;
        if (managerConflict) conflicts[cursor++] = TokenMappingConflict.ManagerType;
    }

    /// @notice Read a configured chain.
    function peerOf(string calldata chain) external view returns (Peer memory) {
        return _peers[chain];
    }

    /// @notice Hyperion's own name for whatever Axelar calls a chain.
    function chainForAxelarName(string calldata axelarChain) external view returns (string memory) {
        return _byAxelarName[axelarChain];
    }

    function _refund() private {
        uint256 balance = address(this).balance;
        if (balance == 0) return;
        (bool sent,) = payable(ROUTER).call{value: balance}("");
        if (!sent) revert RefundFailed();
    }
}
