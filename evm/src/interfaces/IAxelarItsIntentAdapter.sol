// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {Destination} from "../HyperionTypes.sol";

/// @notice Optional router-to-Axelar entry point that carries the original user's address.
interface IAxelarItsIntentAdapter {
    function dispatchWithSender(
        address token,
        uint256 amount,
        Destination calldata destination,
        uint64 nonce,
        address sender
    ) external payable returns (bytes32 railRef);
}
