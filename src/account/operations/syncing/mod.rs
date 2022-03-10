// Copyright 2021 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

pub(crate) mod addresses;
pub mod options;
pub(crate) mod outputs;
pub(crate) mod transactions;
use crate::account::{
    constants::MIN_SYNC_INTERVAL,
    handle::AccountHandle,
    operations::syncing::transactions::TransactionSyncResult,
    types::{address::AddressWithBalance, InclusionState, OutputData},
    AccountBalance,
};
#[cfg(feature = "ledger-nano")]
use crate::signing::SignerType;
pub use options::SyncOptions;

use iota_client::bee_message::output::OutputId;

use std::time::{Instant, SystemTime, UNIX_EPOCH};

impl AccountHandle {
    /// Syncs the account by fetching new information from the nodes. Will also retry pending transactions and
    /// consolidate outputs if necessary.
    pub async fn sync(&self, options: Option<SyncOptions>) -> crate::Result<AccountBalance> {
        let options = options.unwrap_or_default();
        log::debug!("[SYNC] start syncing with {:?}", options);
        let syc_start_time = Instant::now();

        // prevent syncing the account multiple times simultaneously
        let time_now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis();
        let mut last_synced = self.last_synced.lock().await;
        log::debug!("[SYNC] last time synced before {}ms", time_now - *last_synced);
        if time_now - *last_synced < MIN_SYNC_INTERVAL && !options.force_syncing {
            log::debug!(
                "[SYNC] synced within the latest {} ms, only calculating balance",
                MIN_SYNC_INTERVAL
            );
            // calculate the balance because if we created a transaction in the meantime, the amount for the inputs is
            // not available anymore
            return self.balance().await;
        }

        // sync transactions first so we maybe get confirmed outputs in the syncing process later
        // do we want a field in SyncOptions so it can be skipped?
        let transaction_sync_result = if options.sync_pending_transactions {
            Some(self.sync_pending_transactions().await?)
        } else {
            None
        };

        // one could skip addresses to sync, to sync faster (should we only add a field to the sync option to only sync
        // specific addresses?)
        let addresses_to_sync = self.get_addresses_to_sync(&options).await?;
        log::debug!("[SYNC] addresses_to_sync {}", addresses_to_sync.len());

        // get outputs for addresses and add them also the the addresses_with_balance
        let (addresses_with_output_ids, spent_outputs) = self.get_address_output_ids(&options, addresses_to_sync.clone()).await?;

        let mut all_outputs = Vec::new();
        let mut addresses_with_balance = Vec::new();
        for mut address in addresses_with_output_ids {
            let (output_responses, already_known_balance) = self.get_outputs(address.output_ids.clone()).await?;
            let outputs = self.output_response_to_output_data(output_responses, &address).await?;
            // Add balance from new outputs together with balance from already known outputs
            address.amount = outputs.iter().map(|output| output.amount).sum::<u64>()+already_known_balance;
            addresses_with_balance.push(address);
            all_outputs.extend(outputs.into_iter());
        }

        if options.automatic_output_consolidation {
            // Only consolidates outputs for non ledger accounts, because they require approval from the user
            match self.signer.signer_type {
                #[cfg(feature = "ledger-nano")]
                // don't automatically consolidate with ledger accounts, because they require approval from the user
                SignerType::LedgerNano | SignerType::LedgerNanoSimulator => {}
                _ => {
                    self.consolidate_outputs().await?;
                }
            }
        }

        // add a field to the sync options to also sync incoming transactions?

        // updates account with balances, output ids, outputs
        self.update_account(addresses_with_balance, all_outputs, transaction_sync_result, spent_outputs, &options)
            .await?;

        let account_balance = self.balance().await?;
        // update last_synced mutex
        let time_now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis();
        *last_synced = time_now;
        log::debug!("[SYNC] finished syncing in {:.2?}", syc_start_time.elapsed());
        Ok(account_balance)
    }

    /// Update account with newly synced data
    async fn update_account(
        &self,
        addresses_with_balance: Vec<AddressWithBalance>,
        outputs: Vec<OutputData>,
        transaction_sync_result: Option<TransactionSyncResult>,
        spent_outputs: Vec<OutputId>,
        options: &SyncOptions,
    ) -> crate::Result<()> {
        let mut account = self.write().await;
        // update used field of the addresses
        for address in addresses_with_balance.iter() {
            if address.internal {
                let position = account
                    .internal_addresses
                    .binary_search_by_key(&(address.key_index, address.internal), |a| (a.key_index, a.internal))
                    .map_err(|e| crate::Error::AddressNotFoundInAccount)?;
                account.internal_addresses[position].used = true;
            } else {
                let position = account
                    .public_addresses
                    .binary_search_by_key(&(address.key_index, address.internal), |a| (a.key_index, a.internal))
                    .map_err(|e| crate::Error::AddressNotFoundInAccount)?;
                account.public_addresses[position].used = true;
            }
        }

        // Update addresses_with_balance
        // get all addresses with balance that we didn't sync because their index is below the address_start_index of
        // the options
        account.addresses_with_balance = account
            .addresses_with_balance
            .iter()
            .filter(|a| a.key_index < options.address_start_index)
            .cloned()
            .collect();
        // then add all synced addresses with balance, all other addresses that had balance before will then be removed
        // from this list
        account.addresses_with_balance.extend(addresses_with_balance);

        // Update spent outputs
        for output_id in spent_outputs {
            //todo: compare the network id before removing it
            account.unspent_outputs.remove(&output_id);
            //todo: also update the output in account.outputs with the spent metadata
        }
        
        // Add new synced outputs
        for output in outputs {
            account.outputs.insert(output.output_id, output.clone());
            if !output.is_spent {
                account.unspent_outputs.insert(output.output_id, output);
            }
        }

        // Update data from synced transactions
        if let Some(transaction_sync_result) = transaction_sync_result {
            for transaction in transaction_sync_result.updated_transactions {
                match transaction.inclusion_state {
                    InclusionState::Confirmed | InclusionState::Conflicting => {
                        account.pending_transactions.remove(&transaction.payload.id());
                    }
                    _ => {}
                }
                account.transactions.insert(transaction.payload.id(), transaction);
            }

            for output_to_unlock in transaction_sync_result.spent_output_ids {
                if let Some(output) = account.outputs.get_mut(&output_to_unlock) {
                    output.is_spent = true;
                }
                account.locked_outputs.remove(&output_to_unlock);
                account.unspent_outputs.remove(&output_to_unlock);
                log::debug!("[SYNC] Unlocked spent output {}", output_to_unlock);
            }
            for output_to_unlock in transaction_sync_result.output_ids_to_unlock {
                if let Some(output) = account.outputs.get_mut(&output_to_unlock) {
                    output.is_spent = true;
                }
                account.locked_outputs.remove(&output_to_unlock);
                log::debug!(
                    "[SYNC] Unlocked unspent output {} because of a conflicting transaction",
                    output_to_unlock
                );
            }
        }
        #[cfg(feature = "storage")]
        log::debug!("[SYNC] storing account {}", account.index());
        crate::storage::manager::get()
            .await?
            .lock()
            .await
            .save_account(&account)
            .await?;
        // println!("{:#?}", account);
        Ok(())
    }
}
