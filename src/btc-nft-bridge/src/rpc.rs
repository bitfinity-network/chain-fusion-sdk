use std::cell::RefCell;
use std::str::FromStr;

use bitcoin::absolute::LockTime;
use bitcoin::transaction::Version;
use bitcoin::{
    Address, Amount, Network, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid,
    Witness,
};
use futures::TryFutureExt;
use ic_exports::ic_cdk::api::management_canister::bitcoin::{BitcoinNetwork, Utxo};
use ic_exports::ic_cdk::api::management_canister::http_request::{
    http_request, CanisterHttpRequestArgument, HttpHeader, HttpMethod,
};
use inscriber::interface::bitcoin_api;
use ord_rs::inscription::nft::id::NftId;
use ord_rs::{Nft, OrdParser};
use serde::Deserialize;

use crate::constant::{CYCLES_PER_HTTP_REQUEST, MAX_HTTP_RESPONSE_BYTES};
use crate::interface::bridge_api::BridgeError;
use crate::interface::get_deposit_address;
use crate::interface::store::NftInfo;
use crate::state::State;

/// Retrieves and validates the details of a NFT token given its ticker.
pub async fn fetch_nft_token_details(
    state: &RefCell<State>,
    id: NftId,
    holder: String,
) -> anyhow::Result<NftInfo> {
    let (network, indexer_url) = {
        let state = state.borrow();
        (state.btc_network(), state.indexer_url())
    };

    // check that BTC address is valid and/or
    // corresponds to the network.
    is_valid_btc_address(&holder, network)?;

    // <https://docs.hiro.so/ordinals/list-of-inscriptions>
    // e.g. id=66bf2c7be3b0de6916ce8d29465ca7d7c6e27bd57238c25721c101fac34f39cfi0
    let url = format!("{indexer_url}/ordinals/v1/inscriptions?address={holder}&id={id}");

    log::info!("Retrieving inscriptions for {holder} from: {url}");

    let request_params = CanisterHttpRequestArgument {
        url,
        max_response_bytes: Some(MAX_HTTP_RESPONSE_BYTES),
        method: HttpMethod::GET,
        headers: vec![HttpHeader {
            name: "Accept".to_string(),
            value: "application/json".to_string(),
        }],
        body: None,
        transform: None,
    };

    let result = http_request(request_params, CYCLES_PER_HTTP_REQUEST)
        .await
        .map_err(|err| BridgeError::FetchNftTokenDetails(format!("{err:?}")))?
        .0;

    if result.status.to_string() != "200" {
        log::error!("Failed to fetch data: HTTP status {}", result.status);
        return Err(BridgeError::FetchNftTokenDetails("Failed to fetch data".to_string()).into());
    }

    log::info!(
        "Response from indexer: Status: {} Body: {}",
        result.status,
        String::from_utf8_lossy(&result.body)
    );

    let inscriptions: ListInscriptionsResponse =
        serde_json::from_slice(&result.body).map_err(|err| {
            log::error!("Failed to retrieve inscriptions details from the indexer: {err:?}");
            BridgeError::FetchNftTokenDetails(format!("{err:?}"))
        })?;

    let inscription = inscriptions
        .results
        .into_iter()
        .find(|res| res.id == id.to_string())
        .ok_or_else(|| {
            BridgeError::FetchNftTokenDetails("No matching inscription found".to_string())
        })?;

    Ok(NftInfo::new(
        inscription.id,
        id.into(),
        inscription.address,
        inscription.output,
    )?)
}

/// Retrieves (and re-constructs) the reveal transaction by its ID.
///
/// We use the reveal transaction (as opposed to the commit transaction)
/// because it contains the actual NFT inscription that needs to be parsed.
pub(crate) async fn fetch_reveal_transaction(
    state: &RefCell<State>,
    reveal_tx_id: &str,
) -> anyhow::Result<Transaction> {
    let (ic_btc_network, btc_network, indexer_url, derivation_path) = {
        let state = state.borrow();
        (
            state.ic_btc_network(),
            state.btc_network(),
            state.indexer_url(),
            state.derivation_path(None),
        )
    };

    let bridge_addr = get_deposit_address(ic_btc_network, derivation_path).await;

    let nft_utxo = find_inscription_utxo(
        ic_btc_network,
        bridge_addr,
        reveal_tx_id.as_bytes().to_vec(),
    )
    .map_err(|e| BridgeError::FindInscriptionUtxo(e.to_string()))
    .await?;

    let txid = hex::encode(nft_utxo.outpoint.txid);

    let btc_network_str = network_as_str(btc_network);
    let url = format!("{indexer_url}{btc_network_str}/api/tx/{txid}");

    let request_params = CanisterHttpRequestArgument {
        url,
        max_response_bytes: Some(MAX_HTTP_RESPONSE_BYTES),
        method: HttpMethod::GET,
        headers: vec![HttpHeader {
            name: "Accept".to_string(),
            value: "application/json".to_string(),
        }],
        body: None,
        transform: None,
    };

    let result = http_request(request_params, CYCLES_PER_HTTP_REQUEST)
        .await
        .map_err(|err| BridgeError::GetTransactionById(format!("{err:?}")))?
        .0;

    if result.status.to_string() != "200" {
        log::error!("Failed to fetch data: HTTP status {}", result.status);
        return Err(BridgeError::FetchNftTokenDetails("Failed to fetch data".to_string()).into());
    }

    log::info!(
        "Response from indexer: Status: {} Body: {}",
        result.status,
        String::from_utf8_lossy(&result.body)
    );

    let tx: TxInfo = serde_json::from_slice(&result.body).map_err(|err| {
        log::error!("Failed to retrieve the reveal transaction from the indexer: {err:?}");
        BridgeError::GetTransactionById(format!("{err:?}"))
    })?;

    tx.try_into()
}

pub(crate) async fn parse_and_validate_inscription(
    reveal_tx: Transaction,
) -> Result<Nft, BridgeError> {
    log::info!("Parsing NFT inscription from transaction");

    let inscription = OrdParser::parse::<Nft>(&reveal_tx)
        .map_err(|e| BridgeError::InscriptionParsing(e.to_string()))?
        .ok_or_else(|| {
            BridgeError::InscriptionParsing("Failed to parse inscription".to_string())
        })?;

    Ok(inscription)
}

fn network_as_str(network: Network) -> &'static str {
    match network {
        Network::Testnet => "/testnet",
        Network::Regtest => "/regtest",
        Network::Signet => "/signet",
        _ => "",
    }
}

fn is_valid_btc_address(addr: &str, network: Network) -> Result<bool, BridgeError> {
    let network_str = network_as_str(network);

    if !Address::from_str(addr)
        .expect("Failed to convert to bitcoin address")
        .is_valid_for_network(network)
    {
        log::error!("The given bitcoin address {addr} is not valid for {network_str}");
        return Err(BridgeError::MalformedAddress(addr.to_string()));
    }

    Ok(true)
}

/// Validates a reveal transaction ID by checking if it matches
/// the transaction ID of the received UTXO.
///
/// TODO: 1. reduce latency by using derivation_path
///       2. filter out pending UTXOs
async fn find_inscription_utxo(
    network: BitcoinNetwork,
    deposit_addr: String,
    txid: Vec<u8>,
) -> Result<Utxo, BridgeError> {
    let utxos = bitcoin_api::get_utxos(network, deposit_addr)
        .await
        .map_err(|e| BridgeError::GetTransactionById(e.to_string()))?
        .utxos;

    let nft_utxo = utxos
        .iter()
        .find(|utxo| utxo.outpoint.txid == txid)
        .cloned();

    match nft_utxo {
        Some(utxo) => Ok(utxo),
        None => Err(BridgeError::GetTransactionById(
            "No matching UTXO found".to_string(),
        )),
    }
}

#[allow(unused)]
fn validate_utxos(
    _network: BitcoinNetwork,
    _addr: &str,
    _utxos: &[Utxo],
) -> Result<Vec<Utxo>, String> {
    todo!()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ListInscriptionsResponse {
    results: Vec<InscriptionResponse>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InscriptionResponse {
    id: String,
    tx_id: String,
    output: String,
    address: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct TxInfo {
    version: i32,
    locktime: u32,
    vin: Vec<Vin>,
    vout: Vec<Vout>,
}

impl TryFrom<TxInfo> for Transaction {
    type Error = anyhow::Error;

    fn try_from(info: TxInfo) -> Result<Self, Self::Error> {
        let version = Version(info.version);
        let lock_time = LockTime::from_consensus(info.locktime);

        let mut tx_in = Vec::with_capacity(info.vin.len());
        for input in info.vin {
            let txid = Txid::from_str(&input.txid)?;
            let vout = input.vout;
            let script_sig = ScriptBuf::from_hex(&input.prevout.scriptpubkey)?;

            let mut witness = Witness::new();
            for item in input.witness {
                witness.push(ScriptBuf::from_hex(&item)?);
            }

            let tx_input = TxIn {
                previous_output: OutPoint { txid, vout },
                script_sig,
                sequence: Sequence(input.sequence),
                witness,
            };

            tx_in.push(tx_input);
        }

        let mut tx_out = Vec::with_capacity(info.vout.len());
        for output in info.vout {
            let script_pubkey = ScriptBuf::from_hex(&output.scriptpubkey)?;
            let value = Amount::from_sat(output.value);

            tx_out.push(TxOut {
                script_pubkey,
                value,
            });
        }

        Ok(Transaction {
            version,
            lock_time,
            input: tx_in,
            output: tx_out,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct Vin {
    txid: String,
    vout: u32,
    sequence: u32,
    is_coinbase: bool,
    prevout: Prevout,
    witness: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct Prevout {
    scriptpubkey: String,
    scriptpubkey_asm: String,
    scriptpubkey_type: String,
    scriptpubkey_address: Option<String>,
    value: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct Vout {
    scriptpubkey: String,
    scriptpubkey_asm: String,
    scriptpubkey_type: String,
    scriptpubkey_address: Option<String>,
    value: u64,
}
