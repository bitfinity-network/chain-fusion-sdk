use std::str::FromStr as _;

use bitcoin::Txid;
use ord_rs::MultisigConfig;

use crate::interface::{
    Brc20TransferTransactions, InscribeResult, InscribeTransactions, InscriptionFees, Multisig,
    Protocol,
};
use crate::wallet::CanisterWallet;
use crate::Inscriber;

/// Inscribes a message onto the Bitcoin blockchain using the given inscription
/// type.
pub async fn inscribe(
    inscription_type: Protocol,
    inscription: String,
    leftovers_address: String,
    dst_address: String,
    multisig_config: Option<Multisig>,
    derivation_path: Vec<Vec<u8>>,
) -> InscribeResult<InscribeTransactions> {
    let network = Inscriber::get_network_config();
    let leftovers_address = Inscriber::get_address(leftovers_address, network)?;

    let dst_address = Inscriber::get_address(dst_address, network)?;

    let multisig_config = multisig_config.map(|m| MultisigConfig {
        required: m.required,
        total: m.total,
    });

    CanisterWallet::new(derivation_path, network)
        .inscribe(
            &Inscriber::get_inscriber_state(),
            inscription_type,
            inscription,
            dst_address,
            leftovers_address,
            multisig_config,
        )
        .await
}

/// Inscribes and sends the inscribed sat from this canister to the given address.
pub async fn brc20_transfer(
    inscription: String,
    leftovers_address: String,
    dst_address: String,
    multisig_config: Option<Multisig>,
    derivation_path: Vec<Vec<u8>>,
) -> InscribeResult<Brc20TransferTransactions> {
    let network = Inscriber::get_network_config();
    let leftovers_address = Inscriber::get_address(leftovers_address, network)?;
    let transfer_dst_address = Inscriber::get_address(dst_address, network)?;

    let wallet = CanisterWallet::new(derivation_path.clone(), network);
    let inscription_dst_address = wallet.get_bitcoin_address().await;
    let inscription_leftovers_address = inscription_dst_address.clone();

    let inscribe_txs = inscribe(
        Protocol::Brc20,
        inscription,
        inscription_dst_address.to_string(),
        inscription_leftovers_address.to_string(),
        multisig_config,
        derivation_path,
    )
    .await?;

    let (transfer_tx, leftover_amount) = wallet
        .transfer_utxo(
            Txid::from_str(&inscribe_txs.commit_tx).unwrap(),
            Txid::from_str(&inscribe_txs.reveal_tx).unwrap(),
            transfer_dst_address,
            leftovers_address,
            inscribe_txs.leftover_amount,
        )
        .await?;

    Ok(Brc20TransferTransactions {
        commit_tx: inscribe_txs.commit_tx,
        reveal_tx: inscribe_txs.reveal_tx,
        transfer_tx: transfer_tx.to_string(),
        leftover_amount,
    })
}

/// Gets the Bitcoin address for the given derivation path.
pub async fn get_bitcoin_address(derivation_path: Vec<Vec<u8>>) -> String {
    let network = Inscriber::get_network_config();

    CanisterWallet::new(derivation_path, network)
        .get_bitcoin_address()
        .await
        .to_string()
}

pub async fn get_inscription_fees(
    inscription_type: Protocol,
    inscription: String,
    multisig_config: Option<Multisig>,
) -> InscribeResult<InscriptionFees> {
    let network = Inscriber::get_network_config();
    let multisig_config = multisig_config.map(|m| MultisigConfig {
        required: m.required,
        total: m.total,
    });

    CanisterWallet::new(vec![], network)
        .get_inscription_fees(inscription_type, inscription, multisig_config)
        .await
}
