import { RuneActor } from './ic';
import { Actor } from '@dfinity/agent';
import * as ethers from 'ethers';
import { Id256Factory } from './validation';
import WrappedTokenABI from './abi/WrappedToken';
import BftBridgeABI from './abi/BFTBridge';
import { wait } from './tests/utils';
import { encodeBtcAddress } from './utils';

type EthAddr = `0x${string}`;

export class RuneBridge {
  protected BFT_ETH_ADDRESS = process.env.BFT_BRIDGE_ETH_ADDRESS as EthAddr;

  constructor(protected provider: ethers.Signer) {}

  /**
   *
   * dfx canister call rune-bridge get_deposit_address "(\"$ETH_WALLET_ADDRESS\")"
   *
   */
  async getDepositAddress(ethAddress: EthAddr) {
    const result = await RuneActor.get_deposit_address(ethAddress);

    if (!('Ok' in result)) {
      throw new Error('Err');
    }

    return result.Ok;
  }

  private getBftBridgeContract() {
    return new ethers.Contract(
      process.env.BFT_BRIDGE_ETH_ADDRESS!,
      BftBridgeABI,
      this.provider
    );
  }

  private async getWrappedTokenContract() {
    const address = await this.getWrappedTokenEthAddress();

    return new ethers.Contract(address, WrappedTokenABI, this.provider);
  }

  /**
   *
   * TOKEN_ETH_ADDRESS=$(cargo run -q -p create_bft_bridge_tool -- create-token \
   *   --bft-bridge-address="$BFT_ETH_ADDRESS" \
   *   --token-name=RUNE \
   *   --token-id="$RUNE_BRIDGE" \
   *   --evm-canister="$EVM" \
   *   --wallet="$ETH_WALLET")
   *
   */
  async getWrappedTokenEthAddress(): Promise<string> {
    const contract = this.getBftBridgeContract();

    // TODO: is the TOKEN_ETH_ADDRESS only depends on token-id?
    return await contract.getWrappedToken(
      Id256Factory.fromPrincipal(Actor.canisterIdOf(RuneActor))
    );
  }

  async getWrappedTokenBalance(address: EthAddr) {
    const wrappedTokenContract = await this.getWrappedTokenContract();

    return await wrappedTokenContract.balanceOf(address);
  }

  async bridgeBtc(ethAddress: EthAddr) {
    for (let attempt = 0; attempt < 3; attempt++) {
      const result = await RuneActor.deposit(ethAddress);

      await wait(5000);

      if ('Ok' in result) {
        return result.Ok;
      }
    }
  }

  /**
   *
   * cargo run -q -p create_bft_bridge_tool -- burn-wrapped \
   *   --wallet="$ETH_WALLET" \
   *   --evm-canister="$EVM" \
   *   --bft-bridge="$BFT_ETH_ADDRESS" \
   *   --token-address="$TOKEN_ETH_ADDRESS" \
   *   --address="$RECEIVER" \
   *   --amount=10
   *
   */
  async bridgeEVMc(address: string, satoshis: number) {
    const wrappedTokenContract = await this.getWrappedTokenContract();

    await wrappedTokenContract.approve(this.BFT_ETH_ADDRESS, satoshis);

    await wait(15000);

    const bftBridgeContract = this.getBftBridgeContract();

    const tokenAddress = await this.getWrappedTokenEthAddress();

    await bftBridgeContract.burn(
      satoshis,
      tokenAddress,
      `0x${encodeBtcAddress(address)}`
    );
  }

  async getRunesBalance(address: string) {
    return await RuneActor.get_rune_balances(address);
  }
}