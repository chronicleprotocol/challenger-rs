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

use chrono::{DateTime, Utc};
use ethers::{
    contract::{Contract, LogMeta},
    core::types::{Address, ValueOrArray, U64},
    providers::Middleware,
};
use eyre::Result;
use log::{debug, error, info};
use std::{sync::Arc, time::Duration};
use tokio::{sync::mpsc::Sender, time};
use tokio_util::sync::CancellationToken;

use self::contract::{
    OpPokeChallengedSuccessfullyFilter, OpPokedFilter, SchnorrData, ScribeOptimistic,
};

mod contract;

// Note: this is true virtually all of the time but because of leap seconds not always.
// We take minimal time just to be sure, it's always better to check outdated blocks
// rather than miss some.
const SLOT_PERIOD_SECONDS: u16 = 12;

#[allow(dead_code)]
#[derive(Debug)]
pub struct Challenger<M> {
    address: Address,
    client: Arc<M>,
    contract: ScribeOptimistic<M>,
    last_processed_block: Option<U64>,
    challenge_period_in_sec: u16,
    challenge_period_last_updated_at: Option<DateTime<Utc>>,
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
            challenge_period_in_sec: 0,
            challenge_period_last_updated_at: None,
        }
    }

    // Reloads challenge period from contract.
    // This function have to be called every N time, because challenge period can be changed by contract owner.
    async fn reload_challenge_period(&mut self) -> Result<()> {
        let challenge_period_in_sec = self.contract.op_challenge_period().call().await.unwrap();
        debug!(
            "Address {:?}, reloaded opChallenge period for contract is {:?}",
            self.address, challenge_period_in_sec
        );
        self.challenge_period_in_sec = challenge_period_in_sec;
        self.challenge_period_last_updated_at = Some(Utc::now());

        Ok(())
    }

    // Reloads challenge period value if it was not pulled from contract or pulled more than 10 mins ago.
    async fn reload_challenge_period_if_needed(&mut self) -> Result<()> {
        let need_update = match self.challenge_period_last_updated_at {
            None => true,
            Some(utc) => {
                let diff = Utc::now() - utc;
                diff.to_std().unwrap() > Duration::from_secs(600)
            }
        };

        if need_update {
            self.reload_challenge_period().await.unwrap();
        }

        Ok(())
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
        to_block: U64,
    ) -> Result<Vec<(OpPokeChallengedSuccessfullyFilter, LogMeta)>> {
        debug!(
            "Address {:?}, searching OpPokeChallengedSuccessfully events from block {:?}",
            self.address, from_block
        );
        let event =
            Contract::event_of_type::<OpPokeChallengedSuccessfullyFilter>(self.client.clone())
                .address(ValueOrArray::Array(vec![self.address]))
                .from_block(from_block)
                .to_block(to_block);

        Ok(event.query_with_meta().await?)
    }

    // Gets list of OpPoked events for blocks gap we need.
    async fn get_op_pokes(
        &self,
        from_block: U64,
        to_block: U64,
    ) -> Result<Vec<(OpPokedFilter, LogMeta)>> {
        // Fetches `OpPoked` events
        let event = Contract::event_of_type::<OpPokedFilter>(self.client.clone())
            .address(ValueOrArray::Array(vec![self.address]))
            .from_block(from_block)
            .to_block(to_block);

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
    fn reject_challenged_pokes(
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
        // Reloads challenge period value
        self.reload_challenge_period_if_needed().await.unwrap();

        // Getting last block from chain
        let latest_block_number = self.client.get_block_number().await.unwrap();

        // Fetching block we have to start with
        let from_block = self.last_processed_block.unwrap_or(
            self.get_starting_block_number(latest_block_number, self.challenge_period_in_sec)
                .await?,
        );

        debug!(
            "Address {:?}, block we starting with {:?}",
            self.address, from_block
        );

        // Updating last processed block with latest chain block
        self.last_processed_block = Some(latest_block_number);

        // Fetch list of `OpPokeChallengedSuccessfully` events
        let challenges = self
            .get_successful_challenges(from_block, latest_block_number)
            .await?;

        // Fetches `OpPoked` events
        let op_pokes = self.get_op_pokes(from_block, latest_block_number).await?;

        // ignoring already challenged pokes
        let unchallenged_pokes = self.reject_challenged_pokes(op_pokes, challenges);

        // Check if we have unchallenged pokes
        if unchallenged_pokes.len() == 0 {
            debug!(
                "Address {:?}, no unchallenged opPokes found, skipping...",
                self.address
            );
            return Ok(());
        }

        for (log, meta) in unchallenged_pokes {
            let challengeable = self
                .is_challengeable(meta.block_number, self.challenge_period_in_sec)
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
                debug!(
                    "Address {:?}, schnorr data is not valid, trying to challenge...",
                    self.address
                );

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
    pub async fn start(
        &mut self,
        _sender: Sender<()>,
        cancellation_token: CancellationToken,
    ) -> Result<()> {
        let mut interval = time::interval(Duration::from_secs(30));

        loop {
            tokio::select! {
                _ = cancellation_token.cancelled() => {
                    info!("Address {:?}, cancellation token received, stopping...", self.address);
                    return Ok(());
                }
                _ = interval.tick() => {
                    debug!("Address {:?}, interval tick", self.address);
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
    }
}
