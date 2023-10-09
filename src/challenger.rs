//  Copyright (C) 2021-2023 Chronicle Labs, Inc.
//
//  This program is free software: you can redistribute it and/or modify
//  it under the terms of the GNU Affero General Public License as
//  published by the Free Software Foundation, either version 3 of the
//  License, or (at your option) any later version.
//
//  This program is distributed in the hope that it will be useful,
//  but WITHOUT ANY WARRANTY; without even the implied warranty of
//  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
//  GNU Affero General Public License for more details.
//
//  You should have received a copy of the GNU Affero General Public License
//  along with this program.  If not, see <http://www.gnu.org/licenses/>.

use chrono::Utc;
use ethers::{
    contract::{abigen, Contract, LogMeta},
    core::types::{Address, ValueOrArray, U64},
    providers::Middleware,
};
use eyre::Result;
use log::{debug, error, info};
use scribe_optimistic::OpPokeChallengedSuccessfullyFilter;
use std::sync::Arc;
use std::time::Duration;
use tokio::{sync::mpsc::Sender, time};

abigen!(ScribeOptimistic, "./abi/ScribeOptimistic.json");

// Note: this is true virtually all of the time but because of leap seconds not always.
// We take minimal time just to be sure, it's always better to check outdated blocks
// rather than miss some.
const SLOT_PERIOD_SECONDS: u16 = 12;

#[allow(dead_code)]
pub struct Challenger<M> {
    address: Address,
    client: Arc<M>,
    contract: ScribeOptimistic<M>,
    last_processed_block: Option<U64>,
}

impl<M: Middleware> Challenger<M>
where
    M: 'static,
    M::Error: 'static,
{
    pub fn new(address: Address, client: Arc<M>) -> Self {
        let contract = ScribeOptimistic::new(address, client.clone());

        Self {
            address: address,
            client: client,
            contract: contract,
            last_processed_block: None,
        }
    }

    // Gets earliest block number we can search for non challenged `opPokes`
    async fn get_starting_block_number(
        &self,
        last_block_number: U64,
        challenge_period_in_sec: u16,
    ) -> Result<U64> {
        let blocks_per_period = challenge_period_in_sec / SLOT_PERIOD_SECONDS;

        Ok(U64::from(last_block_number - blocks_per_period))
    }

    // Gets list of OpPokeChallengedSuccessfully events for blocks gap we need.
    async fn get_successful_challenges(
        &self,
        from_block: U64,
    ) -> Result<Vec<(OpPokeChallengedSuccessfullyFilter, LogMeta)>> {
        debug!(
            "Address {:?}, searching OpPokeChallengedSuccessfully events from block {:?}",
            self.address, from_block
        );
        let event =
            Contract::event_of_type::<OpPokeChallengedSuccessfullyFilter>(self.client.clone())
                .address(ValueOrArray::Array(vec![self.address]))
                .from_block(from_block);

        Ok(event.query_with_meta().await?)
    }

    // Check if given block_number for log is already non challengeable
    async fn is_challengeable(
        &self,
        block_number: U64,
        challenge_period_in_sec: u16,
    ) -> Result<bool> {
        // Checking if log is possible to challenge ?
        let block = self.client.get_block(block_number).await?.unwrap();
        let diff = Utc::now().timestamp() as u64 - block.timestamp.as_u64();

        Ok(challenge_period_in_sec > diff as u16)
    }

    // TODO: Need tests
    fn filter_unchallenged_events(
        &self,
        pokes: Vec<(OpPokedFilter, LogMeta)>,
        challenges: Vec<(OpPokeChallengedSuccessfullyFilter, LogMeta)>,
    ) -> Vec<(OpPokedFilter, LogMeta)> {
        if challenges.len() == 0 {
            return pokes;
        }
        if pokes.len() == 0 {
            return pokes;
        }
        let mut result: Vec<(OpPokedFilter, LogMeta)> = vec![];

        if pokes.len() == 1 {
            let (_, meta) = &pokes[0];
            for (_, c_meta) in challenges.clone() {
                if c_meta.block_number > meta.block_number {
                    // empty result
                    return result;
                }
            }
            return pokes;
        }

        'pokes_loop: for i in 0..pokes.len() {
            let (poke, meta) = &pokes.get(i).unwrap();
            // If we do have next poke in list
            if let Some((_, next_meta)) = &pokes.get(i + 1) {
                for (_, c_meta) in challenges.clone() {
                    if meta.block_number < c_meta.block_number
                        && next_meta.block_number > c_meta.block_number
                    {
                        // poke already challenged
                        continue 'pokes_loop;
                    }
                }
            } else {
                for (_, c_meta) in challenges.clone() {
                    if c_meta.block_number > meta.block_number {
                        // poke already challenged
                        continue 'pokes_loop;
                    }
                }
            }
            result.push((poke.clone(), meta.clone()));
        }

        result
    }

    async fn process(&mut self) -> Result<()> {
        let challenge_period_in_sec = self.contract.op_challenge_period().call().await?;
        debug!(
            "Address {:?}, opChallenge period for contract is {:?}",
            self.address, challenge_period_in_sec
        );

        // Getting last block from chain
        let last_block_number = self.client.get_block_number().await?;

        // Fetching block we have to start with
        let from_block = self.last_processed_block.unwrap_or(
            self.get_starting_block_number(last_block_number, challenge_period_in_sec)
                .await?,
        );

        debug!(
            "Address {:?}, block we starting with {:?}",
            self.address, from_block
        );

        // Updating last processed block with latest chain block
        self.last_processed_block = Some(last_block_number);

        // Fetch list of `OpPokeChallengedSuccessfully` events
        let challenges = self.get_successful_challenges(from_block).await?;

        // Fetches `OpPoked` events
        let event = Contract::event_of_type::<OpPokedFilter>(self.client.clone())
            .address(ValueOrArray::Array(vec![self.address]))
            .from_block(from_block);

        let logs = event.query_with_meta().await?;

        let filtered = self.filter_unchallenged_events(logs, challenges);

        if filtered.len() == 0 {
            info!(
                "Address {:?}, no unchallenged opPokes found, skipping...",
                self.address
            );
            return Ok(());
        }

        for (log, meta) in filtered {
            let challengeable = self
                .is_challengeable(meta.block_number, challenge_period_in_sec)
                .await?;

            if !challengeable {
                error!(
                    "Address {:?}, block is to old for `opChallenge` block number: {:?}",
                    self.address, meta.block_number
                );
                continue;
            }

            let message = self
                .contract
                .construct_poke_message(log.poke_data)
                .call()
                .await?;

            let schnorr_data = log.schnorr_data;

            let valid = self
                .contract
                .is_acceptable_schnorr_signature_now(message, schnorr_data.clone())
                .call()
                .await?;

            debug!(
                "Address {:?}, schnorr data valid for block {:?}: {:?}",
                self.address, meta.block_number, valid
            );

            if !valid {
                // TODO: handle error gracefully, we should go further even if error happened
                match self.challenge(schnorr_data.clone()).await {
                    Ok(receipt) => {
                        info!(
                            "Address {:?}, successfully sent `opChallenge` transaction {:?}",
                            self.address, receipt
                        );
                    }
                    Err(err) => {
                        error!(
                            "Address {:?}, failed to make `opChallenge` call: {:?}",
                            self.address, err
                        );
                    }
                };
            }
        }
        Ok(())
    }

    async fn challenge(&self, schnorr_data: SchnorrData) -> Result<()> {
        info!(
            "Address {:?}, Calling opChallenge for {:?}",
            self.address, schnorr_data
        );
        let receipt = self
            .contract
            .op_challenge(schnorr_data.to_owned())
            .send()
            .await?
            .await?;

        debug!(
            "Address {:?}, opChallenge receipt: {:?}",
            self.address, receipt
        );
        Ok(())
    }

    /// Start address processing
    pub async fn start(&mut self, _sender: Sender<()>) -> Result<()> {
        let mut interval = time::interval(Duration::from_secs(30));

        loop {
            interval.tick().await;

            match self.process().await {
                Ok(_) => {
                    debug!("All ok, continue with next tick...");
                }
                Err(err) => {
                    error!(
                        "Address {:?}, failed to process opPokes: {:?}",
                        self.address, err
                    );
                }
            }
        }
    }
}
