use core::sync::atomic::Ordering;
use std::cell::RefCell;
use std::str::FromStr;

use bitcoin::{Address, Network, PublicKey};
use candid::Principal;
use did::{H160, H256};
use eth_signer::sign_strategy::TransactionSigner;
use ic_canister::virtual_canister_call;
use ic_exports::ic_cdk::api::management_canister::ecdsa::{
    self as IcEcdsa, EcdsaKeyId, EcdsaPublicKeyArgument, EcdsaPublicKeyResponse,
};
use ic_stable_structures::CellStructure;
use minter_did::id256::Id256;
use minter_did::order::{MintOrder, SignedMintOrder};
use ord_rs::{Brc20, Inscription, OrdParser};

use crate::api::{
    Brc20InscribeError, Brc20InscribeStatus, Brc20TokenDetails, BridgeError, Erc20MintError,
    Erc20MintStatus, InscribeBrc20Args,
};
use crate::constant::{BRC20_TICKER_LEN, NONCE};
use crate::inscriber_api::{InscribeResult, InscribeTransactions, Protocol};
use crate::state::State;
use crate::store::Brc20TokenInfo;

/// Swap a BRC20 for an ERC20.
///
/// This burns a BRC20 and mints an equivalent ERC20.
pub async fn brc20_to_erc20(
    state: &RefCell<State>,
    eth_address: H160,
    brc20_ticker: String,
    holder_btc_addr: String,
) -> Result<Erc20MintStatus, Erc20MintError> {
    let Brc20TokenDetails {
        ticker,
        holder,
        tx_id,
    } = crate::rpc::fetch_brc20_token_details(state, brc20_ticker, holder_btc_addr)
        .await
        .map_err(|e| Erc20MintError::InvalidBrc20(e.to_string()))?;

    log::trace!("Parsing and validating BRC20 inscription from transaction ID: {tx_id}");
    let brc20 = parse_and_validate_inscription(state, &tx_id)
        .await
        .map_err(|e| Erc20MintError::InvalidBrc20(e.to_string()))?;

    state
        .borrow_mut()
        .inscriptions_mut()
        .insert(Brc20TokenInfo {
            tx_id,
            ticker,
            holder,
        });

    let (amount, tick) = get_brc20_data(&brc20);
    // Set the token symbol using the tick (symbol) from the BRC20
    state
        .borrow_mut()
        .set_token_symbol(tick)
        .map_err(|e| Erc20MintError::Brc20Bridge(e.to_string()))?;

    let nonce = NONCE.fetch_add(1, Ordering::Relaxed);

    log::info!("Minting an ERC20 token with symbol: {tick}");
    mint_erc20(state, eth_address, amount, nonce).await
}

async fn parse_and_validate_inscription(
    state: &RefCell<State>,
    reveal_txid: &str,
) -> Result<Brc20, BridgeError> {
    log::trace!("Fetching the reveal transaction by its ID: {reveal_txid}");
    let reveal_tx = crate::rpc::fetch_reveal_transaction(state, reveal_txid)
        .await
        .map_err(|e| BridgeError::GetTransactionById(e.to_string()))?;

    let inscription = OrdParser::parse::<Brc20>(&reveal_tx)
        .map_err(|e| BridgeError::InscriptionParsing(e.to_string()))?;

    match inscription {
        Some(brc20) => {
            let (_amount, ticker) = get_brc20_data(&brc20);
            if ticker.len() != BRC20_TICKER_LEN {
                return Err(BridgeError::InscriptionParsing(
                    "BRC20 ticker (symbol) should be only 4 letters".to_string(),
                ));
            }
            log::info!("BRC20 inscription validated");
            Ok(brc20)
        }
        None => Err(BridgeError::InscriptionParsing(
            "No BRC20 inscription associated with this transaction".to_string(),
        )),
    }
}

fn get_brc20_data(inscription: &Brc20) -> (u64, &str) {
    match inscription {
        Brc20::Deploy(deploy_func) => (deploy_func.max, &deploy_func.tick),
        Brc20::Mint(mint_func) => (mint_func.amt, &mint_func.tick),
        Brc20::Transfer(transfer_func) => (transfer_func.amt, &transfer_func.tick),
    }
}

pub async fn mint_erc20(
    state: &RefCell<State>,
    eth_address: H160,
    amount: u64,
    nonce: u32,
) -> Result<Erc20MintStatus, Erc20MintError> {
    let fee = state.borrow().erc20_minter_fee();
    let amount_minus_fee = amount
        .checked_sub(fee)
        .ok_or(Erc20MintError::ValueTooSmall(amount.to_string()))?;

    let mint_order =
        prepare_mint_order(state, eth_address.clone(), amount_minus_fee, nonce).await?;
    store_mint_order(state, mint_order, &eth_address, nonce);

    Ok(match send_mint_order(state, mint_order).await {
        Ok(tx_id) => Erc20MintStatus::Minted {
            amount: amount_minus_fee,
            tx_id,
        },
        Err(err) => {
            log::warn!("Failed to send mint order: {err:?}");
            Erc20MintStatus::Signed(Box::new(mint_order))
        }
    })
}

async fn prepare_mint_order(
    state: &RefCell<State>,
    eth_address: H160,
    amount: u64,
    nonce: u32,
) -> Result<SignedMintOrder, Erc20MintError> {
    log::trace!("preparing mint order");

    let (signer, mint_order) = {
        let state_ref = state.borrow();

        let sender_chain_id = state_ref.btc_chain_id();
        let sender = Id256::from_evm_address(&eth_address, sender_chain_id);
        let src_token = Id256::from(&ic_exports::ic_kit::ic::id());

        let recipient_chain_id = state_ref.erc20_chain_id();

        let mint_order = MintOrder {
            amount: amount.into(),
            sender,
            src_token,
            recipient: eth_address,
            dst_token: H160::default(),
            nonce,
            sender_chain_id,
            recipient_chain_id,
            name: state_ref.token_name(),
            symbol: state_ref.token_symbol(),
            decimals: state_ref.decimals(),
            approve_spender: Default::default(),
            approve_amount: Default::default(),
        };

        let signer = state_ref.signer().get().clone();

        (signer, mint_order)
    };

    let signed_mint_order = mint_order
        .encode_and_sign(&signer)
        .await
        .map_err(|err| Erc20MintError::Sign(format!("{err:?}")))?;

    Ok(signed_mint_order)
}

fn store_mint_order(
    state: &RefCell<State>,
    signed_mint_order: SignedMintOrder,
    eth_address: &H160,
    nonce: u32,
) {
    let mut state = state.borrow_mut();
    let sender_chain_id = state.btc_chain_id();
    let sender = Id256::from_evm_address(eth_address, sender_chain_id);
    state
        .mint_orders_mut()
        .push(sender, nonce, signed_mint_order);

    log::trace!("Mint order added");
}

async fn send_mint_order(
    state: &RefCell<State>,
    mint_order: SignedMintOrder,
) -> Result<H256, Erc20MintError> {
    log::trace!("Sending mint transaction");

    let signer = state.borrow().signer().get().clone();
    let sender = signer
        .get_address()
        .await
        .map_err(|err| Erc20MintError::Sign(format!("{err:?}")))?;

    let (evm_info, evm_params) = {
        let state = state.borrow();

        let evm_info = state.get_evm_info();
        let evm_params = state
            .get_evm_params()
            .clone()
            .ok_or(Erc20MintError::NotInitialized(
                "Bridge must be initialized first".to_string(),
            ))?;

        (evm_info, evm_params)
    };

    let mut tx = minter_contract_utils::bft_bridge_api::mint_transaction(
        sender.0,
        evm_info.bridge_contract.0,
        evm_params.nonce.into(),
        evm_params.gas_price.into(),
        mint_order.to_vec(),
        evm_params.chain_id as _,
    );

    let signature = signer
        .sign_transaction(&(&tx).into())
        .await
        .map_err(|err| Erc20MintError::Sign(format!("{err:?}")))?;

    tx.r = signature.r.0;
    tx.s = signature.s.0;
    tx.v = signature.v.0;
    tx.hash = tx.hash();

    let client = evm_info.link.get_client();
    let id = client
        .send_raw_transaction(tx)
        .await
        .map_err(|err| Erc20MintError::Evm(format!("{err:?}")))?;

    state.borrow_mut().update_evm_params(|p| {
        if let Some(params) = p.as_mut() {
            params.nonce += 1;
        }
    });

    log::trace!("Mint transaction sent");

    Ok(id.into())
}

/// Swap an ERC20 for a BRC20.
///
/// This burns an ERC20 and inscribes an equivalent BRC20.
pub async fn erc20_to_brc20(
    _state: &RefCell<State>,
    _request_id: u32,
    _address: &str,
    _amount: u64,
) -> Result<Brc20InscribeStatus, Brc20InscribeError> {
    todo!()
}

// WIP
pub async fn erc20_to_brc20_v2(
    state: &RefCell<State>,
    _request_id: u32,
    _eth_address: &str,
    _amount: u64,
    brc20_args: InscribeBrc20Args,
    _reveal_txid: &str,
) -> Result<Brc20InscribeStatus, Brc20InscribeError> {
    let inscriber = state.borrow().inscriber();

    let InscribeBrc20Args {
        inscription_type,
        inscription,
        leftovers_address,
        dst_address,
        multisig_config,
    } = brc20_args;

    let brc20: Brc20 =
        serde_json::from_str(&inscription).expect("Failed to deserialize BRC20 from string");

    // state
    //     .borrow_mut()
    //     .inscriptions_mut()
    //     .insert(&brc20, reveal_txid.to_string());
    let (_amount, _tick) = get_brc20_data(&brc20);

    log::info!("Creating a BRC20 inscription");
    let tx_ids = virtual_canister_call!(
        inscriber,
        "inscribe",
        (
            inscription_type,
            inscription,
            leftovers_address,
            dst_address.clone(),
            multisig_config,
        ),
        InscribeResult<InscribeTransactions>
    )
    .await
    .map_err(|e| Brc20InscribeError::TemporarilyUnavailable(e.1))?
    .map_err(|e| Brc20InscribeError::Inscribe(e.to_string()))?;

    log::trace!("Created a BRC20 inscription with IDs: {tx_ids:?}");

    todo!()
}

/// Returns the BRC20 deposit address
pub async fn get_deposit_address(
    state: &RefCell<State>,
    eth_address: H160,
) -> Result<Address, BridgeError> {
    let (network, key_id, derivation_path) = {
        let state = state.borrow();
        (
            state.btc_network(),
            state.ecdsa_key_id(),
            state.derivation_path(Some(eth_address)),
        )
    };

    let public_key = ecdsa_public_key(key_id, derivation_path).await?;
    let public_key = PublicKey::from_str(&public_key)
        .map_err(|e| BridgeError::PublicKeyFromStr(e.to_string()))?;

    btc_address_from_public_key(network, &public_key)
}

/// Retrieves the ECDSA public key of this canister at the given derivation path
/// from IC's ECDSA API.
async fn ecdsa_public_key(
    key_id: EcdsaKeyId,
    derivation_path: Vec<Vec<u8>>,
) -> Result<String, BridgeError> {
    let arg = EcdsaPublicKeyArgument {
        canister_id: None,
        derivation_path,
        key_id,
    };

    let (res,): (EcdsaPublicKeyResponse,) = IcEcdsa::ecdsa_public_key(arg)
        .await
        .map_err(|e| BridgeError::EcdsaPublicKey(e.1))?;

    Ok(hex::encode(res.public_key))
}

fn btc_address_from_public_key(
    network: Network,
    public_key: &PublicKey,
) -> Result<Address, BridgeError> {
    Address::p2wpkh(public_key, network)
        .map_err(|e| BridgeError::AddressFromPublicKey(e.to_string()))
}

pub async fn burn_brc20(
    state: &RefCell<State>,
    address: &str,
    request_id: u32,
    reveal_txid: &str,
) -> Result<InscribeTransactions, BridgeError> {
    if !state.borrow().has_brc20(reveal_txid) {
        return Err(BridgeError::Brc20Burn(format!(
            "Specified tx ID ({}) not associated with any BRC20 inscription",
            reveal_txid
        )));
    }

    log::trace!("Parsing and validating a BRC20 inscription from transaction ID: {reveal_txid}");
    let brc20 = parse_and_validate_inscription(state, reveal_txid)
        .await
        .map_err(|e| Erc20MintError::InvalidBrc20(e.to_string()))?
        .encode()
        .map_err(|e| BridgeError::Brc20Burn(e.to_string()))?;

    let inscriber_principal = {
        let mut state = state.borrow_mut();

        state
            .burn_requests_mut()
            .insert(request_id, address.to_string(), reveal_txid.to_string());

        state.inscriber()
    };

    let inscriber_btc_address = get_inscriber_account(inscriber_principal).await?;

    log::trace!(
        "Transferring BRC20 with reveal_txid ({}) to {} with request id {}",
        reveal_txid,
        inscriber_btc_address,
        request_id
    );

    let transfer_args = InscribeBrc20Args {
        inscription_type: Protocol::Brc20,
        inscription: brc20,
        leftovers_address: inscriber_btc_address.clone(),
        dst_address: inscriber_btc_address,
        multisig_config: None,
    };

    let result = transfer_brc20(inscriber_principal, transfer_args)
        .await
        .map_err(|e| BridgeError::Brc20Burn(e.to_string()));

    let mut state = state.borrow_mut();
    if result.is_ok() {
        state
            .inscriptions_mut()
            .remove(reveal_txid.to_string())
            .map_err(|e| BridgeError::Brc20Burn(e.to_string()))?;

        state.burn_requests_mut().set_transferred(request_id);
        state.burn_requests_mut().remove(request_id);
    } else {
        log::error!("Failed to transfer BRC20 for request {}", request_id);
    }

    result
}

async fn get_inscriber_account(inscriber: Principal) -> Result<String, BridgeError> {
    log::trace!("Requesting the Inscriber canister's account");

    let account = virtual_canister_call!(inscriber, "get_bitcoin_address", (), String)
        .await
        .map_err(|err| {
            log::error!("Failed to retrieve Inscriber's BRC20 account: {err:?}");
            BridgeError::GetDepositAddress("get bitcoin address".to_string())
        })?;

    log::trace!("Inscriber's BRC20 account: {account:?}");

    Ok(account)
}

async fn transfer_brc20(
    inscriber: Principal,
    transfer_args: InscribeBrc20Args,
) -> Result<InscribeTransactions, Brc20InscribeError> {
    log::trace!("Transferring BRC20 to the Inscriber");

    let InscribeBrc20Args {
        inscription_type,
        inscription,
        leftovers_address,
        dst_address,
        multisig_config,
    } = transfer_args;

    let tx_ids = virtual_canister_call!(
        inscriber,
        "brc20_transfer",
        (
            inscription_type,
            inscription,
            leftovers_address,
            dst_address.clone(),
            multisig_config,
        ),
        InscribeResult<InscribeTransactions>
    )
    .await
    .map_err(|e| Brc20InscribeError::TemporarilyUnavailable(e.1))?
    .map_err(|e| Brc20InscribeError::Brc20Transfer(e.to_string()))?;

    log::trace!("Transferred BRC20 with IDs {tx_ids:?} to {dst_address:?}");

    Ok(tx_ids)
}
