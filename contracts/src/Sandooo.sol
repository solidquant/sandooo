// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

contract Sandooo {
    address public owner;

    bytes4 internal constant TOKEN_TRANSFER_ID = 0xa9059cbb;
    bytes4 internal constant V2_SWAP_ID = 0x022c0d9f;

    constructor() {
        owner = msg.sender;
    }

    function recoverToken(address token, uint256 amount) public {
        require(msg.sender == owner, "NOT_OWNER");

        assembly {
            switch eq(token, 0)
            case 0 {
                let ptr := mload(0x40)
                mstore(ptr, TOKEN_TRANSFER_ID)
                mstore(add(ptr, 4), caller())
                mstore(add(ptr, 36), amount)
                if iszero(call(gas(), token, 0, ptr, 68, 0, 0)) {
                    revert(0, 0)
                }
            }
            case 1 {
                if iszero(call(gas(), caller(), amount, 0, 0, 0, 0)) {
                    revert(0, 0)
                }
            }
        }
    }

    receive() external payable {}

    fallback() external payable {
        require(msg.sender == owner, "NOT_OWNER");

        assembly {
            let ptr := mload(0x40)
            let end := calldatasize()

            let block_number := shr(192, calldataload(0))
            if iszero(eq(block_number, number())) {
                revert(0, 0)
            }

            for {
                let offset := 8
            } lt(offset, end) {

            } {
                let zeroForOne := shr(248, calldataload(offset))
                let pair := shr(96, calldataload(add(offset, 1)))
                let tokenIn := shr(96, calldataload(add(offset, 21)))
                let amountIn := calldataload(add(offset, 41))
                let amountOut := calldataload(add(offset, 73))
                offset := add(offset, 105)

                mstore(ptr, TOKEN_TRANSFER_ID)
                mstore(add(ptr, 4), pair)
                mstore(add(ptr, 36), amountIn)

                if iszero(call(gas(), tokenIn, 0, ptr, 68, 0, 0)) {
                    revert(0, 0)
                }

                mstore(ptr, V2_SWAP_ID)
                switch zeroForOne
                case 0 {
                    mstore(add(ptr, 4), amountOut)
                    mstore(add(ptr, 36), 0)
                }
                case 1 {
                    mstore(add(ptr, 4), 0)
                    mstore(add(ptr, 36), amountOut)
                }
                mstore(add(ptr, 68), address())
                mstore(add(ptr, 100), 0x80)

                if iszero(call(gas(), pair, 0, ptr, 164, 0, 0)) {
                    revert(0, 0)
                }
            }
        }
    }
}
