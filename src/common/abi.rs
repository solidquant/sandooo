use ethers::abi::parse_abi;
use ethers::prelude::BaseContract;

#[derive(Clone, Debug)]
pub struct Abi {
    pub factory: BaseContract,
    pub pair: BaseContract,
    pub token: BaseContract,
    pub sando_bot: BaseContract,
}

impl Abi {
    pub fn new() -> Self {
        let factory = BaseContract::from(
            parse_abi(&["function getPair(address,address) external view returns (address)"])
                .unwrap(),
        );

        let pair = BaseContract::from(
            parse_abi(&[
                "function token0() external view returns (address)",
                "function token1() external view returns (address)",
                "function getReserves() external view returns (uint112,uint112,uint32)",
            ])
            .unwrap(),
        );

        let token = BaseContract::from(
            parse_abi(&[
                "function owner() external view returns (address)",
                "function name() external view returns (string)",
                "function symbol() external view returns (string)",
                "function decimals() external view returns (uint8)",
                "function totalSupply() external view returns (uint256)",
                "function balanceOf(address) external view returns (uint256)",
                "function approve(address,uint256) external view returns (bool)",
                "function transfer(address,uint256) external returns (bool)",
                "function allowance(address,address) external view returns (uint256)",
            ])
            .unwrap(),
        );

        let sando_bot = BaseContract::from(
            parse_abi(&["function recoverToken(address,uint256) public"]).unwrap(),
        );

        Self {
            factory,
            pair,
            token,
            sando_bot,
        }
    }
}
