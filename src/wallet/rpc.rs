//! Manages connection with a Bitcoin Core RPC.
//!
use std::{ convert::TryFrom, thread, time::Duration };

use bitcoin::Network;
use bitcoind::bitcoincore_rpc::{ Auth, Client, RpcApi };
use serde_json::Value;

use crate::{
    utill::{ redeemscript_to_scriptpubkey, str_to_bitcoin_network },
    wallet::{ api::KeychainKind, WalletSwapCoin },
};

use serde::Deserialize;

use super::{ error::WalletError, Wallet };

/// Configuration parameters for connecting to a Bitcoin node via RPC.
#[derive(Debug, Clone)]
pub struct RPCConfig {
    /// The bitcoin node url
    pub url: String,
    /// The bitcoin node authentication mechanism
    pub auth: Auth,
    /// The network we are using (it will be checked the bitcoin node network matches this)
    pub network: Network,
    /// The wallet name in the bitcoin node, derive this from the descriptor.
    pub wallet_name: String,
}

const RPC_HOSTPORT: &str = "localhost:18443";

impl Default for RPCConfig {
    fn default() -> Self {
        Self {
            url: RPC_HOSTPORT.to_string(),
            auth: Auth::UserPass("regtestrpcuser".to_string(), "regtestrpcpass".to_string()),
            network: Network::Regtest,
            wallet_name: "random-wallet-name".to_string(),
        }
    }
}

impl TryFrom<&RPCConfig> for Client {
    type Error = WalletError;
    fn try_from(config: &RPCConfig) -> Result<Self, WalletError> {
        let rpc = Client::new(
            format!(
                "http://{}/wallet/{}",
                config.url.as_str(),
                config.wallet_name.as_str()
            ).as_str(),
            config.auth.clone()
        )?;
        if config.network != str_to_bitcoin_network(rpc.get_blockchain_info()?.chain.as_str()) {
            return Err(
                WalletError::Protocol("RPC Network not mathcing with RPCConfig".to_string())
            );
        }
        Ok(rpc)
    }
}

fn list_wallet_dir(client: &Client) -> Result<Vec<String>, WalletError> {
    #[derive(Deserialize)]
    struct Name {
        name: String,
    }
    #[derive(Deserialize)]
    struct CallResult {
        wallets: Vec<Name>,
    }

    let result: CallResult = client.call("listwalletdir", &[])?;
    Ok(
        result.wallets
            .into_iter()
            .map(|n| n.name)
            .collect()
    )
}

impl Wallet {
    /// Sync the wallet with the configured Bitcoin Core RPC. Save data to disk.
    pub fn sync(&mut self) -> Result<(), WalletError> {
        // Create or load the watch-only bitcoin core wallet
        let wallet_name = &self.store.file_name;
        if self.rpc.list_wallets()?.contains(wallet_name) {
            log::info!("wallet already loaded: {}", wallet_name);
        } else if list_wallet_dir(&self.rpc)?.contains(wallet_name) {
            self.rpc.load_wallet(wallet_name)?;
            log::info!("wallet loaded: {}", wallet_name);
        } else {
            // pre-0.21 use legacy wallets
            if self.rpc.version()? < 210_000 {
                self.rpc.create_wallet(wallet_name, Some(true), None, None, None)?;
            } else {
                // TODO: move back to api call when https://github.com/rust-bitcoin/rust-bitcoincore-rpc/issues/225 is closed
                let args = [
                    Value::String(wallet_name.clone()),
                    Value::Bool(true), // Disable Private Keys
                    Value::Bool(false), // Create a blank wallet
                    Value::Null, // Optional Passphrase
                    Value::Bool(false), // Avoid Reuse
                    Value::Bool(true), // Descriptor Wallet
                ];
                let _: Value = self.rpc.call("createwallet", &args)?;
            }

            log::info!("wallet created: {}", wallet_name);
        }

        let mut descriptors_to_import = Vec::new();

        descriptors_to_import.extend(self.get_unimported_wallet_desc()?);

        descriptors_to_import.extend(
            self.store.incoming_swapcoins
                .values()
                .map(|sc| {
                    format!("wsh(sortedmulti(2,{},{}))", sc.get_other_pubkey(), sc.get_my_pubkey())
                })
                .map(|d| self.rpc.get_descriptor_info(&d).unwrap().descriptor)
                .filter(|d| !self.is_descriptor_imported(d))
                .collect::<Vec<String>>()
        );

        descriptors_to_import.extend(
            self.store.outgoing_swapcoins
                .values()
                .map(|sc| {
                    format!("wsh(sortedmulti(2,{},{}))", sc.get_other_pubkey(), sc.get_my_pubkey())
                })
                .map(|d| self.rpc.get_descriptor_info(&d).unwrap().descriptor)
                .filter(|d| !self.is_descriptor_imported(d))
        );

        descriptors_to_import.extend(
            self.store.incoming_swapcoins
                .values()
                .map(|sc| {
                    let contract_spk = redeemscript_to_scriptpubkey(&sc.contract_redeemscript);
                    format!("raw({:x})", contract_spk)
                })
                .map(|d| self.rpc.get_descriptor_info(&d).unwrap().descriptor)
                .filter(|d| !self.is_descriptor_imported(d))
                .collect::<Vec<_>>()
        );
        descriptors_to_import.extend(
            self.store.outgoing_swapcoins
                .values()
                .map(|sc| {
                    let contract_spk = redeemscript_to_scriptpubkey(&sc.contract_redeemscript);
                    format!("raw({:x})", contract_spk)
                })
                .map(|d| self.rpc.get_descriptor_info(&d).unwrap().descriptor)
                .filter(|d| !self.is_descriptor_imported(d))
                .collect::<Vec<_>>()
        );

        let is_fidelity_addrs_imported = {
            let mut spks = self.store.fidelity_bond.iter().map(|(_, (b, _, _))| b.script_pub_key());
            let (first_addr, last_addr) = (spks.next(), spks.last());

            let is_first_imported = if let Some(spk) = first_addr {
                let descriptor_without_checksum = format!("raw({:x})", spk);
                let descriptor = self.rpc
                    .get_descriptor_info(&descriptor_without_checksum)
                    .unwrap().descriptor;
                let addr = self.rpc.derive_addresses(&descriptor, None).unwrap()[0].clone();
                self.rpc
                    .get_address_info(&addr.assume_checked())
                    .unwrap()
                    .is_watchonly.unwrap_or(false)
            } else {
                true // mark true if theres no spk to import
            };

            let is_last_imported = if let Some(spk) = last_addr {
                let descriptor_without_checksum = format!("raw({:x})", spk);
                let descriptor = self.rpc
                    .get_descriptor_info(&descriptor_without_checksum)
                    .unwrap().descriptor;
                let addr = self.rpc.derive_addresses(&descriptor, None).unwrap()[0].clone();
                self.rpc
                    .get_address_info(&addr.assume_checked())
                    .unwrap()
                    .is_watchonly.unwrap_or(false)
            } else {
                true // mark true if theres no spks to import
            };

            is_first_imported && is_last_imported
        };

        descriptors_to_import.extend(
            self.store.fidelity_bond.iter().map(|(_, (_, spk, _))| {
                let descriptor_without_checksum = format!("raw({:x})", spk);
                self.rpc.get_descriptor_info(&descriptor_without_checksum).unwrap().descriptor
            })
        );

        if descriptors_to_import.is_empty() && is_fidelity_addrs_imported {
            return Ok(());
        }

        log::debug!("Importing Wallet spks/descriptors");

        self.import_descriptors(&descriptors_to_import, None)?;

        // Now run the scan
        log::debug!("Initializing TxOut scan. This may take a while.");

        // Sometimes in test multiple wallet scans can occur at same time, resulting in error.
        // Just retry after 3 sec.
        loop {
            let last_synced_height = self.store.last_synced_height
                .unwrap_or(0)
                .max(self.store.wallet_birthday.unwrap_or(0));
            let node_synced = self.rpc.get_block_count()?;
            log::info!("rescan_blockchain from:{} to:{}", last_synced_height, node_synced);
            match
                self.rpc.rescan_blockchain(
                    Some(last_synced_height as usize),
                    Some(node_synced as usize)
                )
            {
                Ok(_) => {
                    self.store.last_synced_height = Some(node_synced);
                    break;
                }

                Err(e) => {
                    log::warn!("Sync Error, Retrying: {}", e);
                    thread::sleep(Duration::from_secs(3));
                    continue;
                }
            }
        }

        let max_external_index = self.find_hd_next_index(KeychainKind::External)?;
        self.update_external_index(max_external_index)?;
        Ok(())
    }
}
