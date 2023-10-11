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
    contract::LogMeta,
    core::types::{Address, U64},
};
use eyre::Result;
use log::{debug, error, info};
use std::time::Duration;
use tokio::{sync::mpsc::Sender, time};
use tokio_util::sync::CancellationToken;

use self::contract::{OpPokeChallengedSuccessfullyFilter, OpPokedFilter, ScribeOptimisticProvider};

pub mod contract;

// Note: this is true virtually all of the time but because of leap seconds not always.
// We take minimal time just to be sure, it's always better to check outdated blocks
// rather than miss some.
const SLOT_PERIOD_SECONDS: u16 = 12;

// Time interval in seconds to reload challenge period from contract.
const DEFAULT_CHALLENGE_PERIOD_RELOAD_INTERVAL: Duration = Duration::from_secs(600);

pub struct Challenger {
    address: Address,
    contract_provider: Box<dyn ScribeOptimisticProvider + 'static>,
    last_processed_block: Option<U64>,
    challenge_period_in_sec: u16,
    challenge_period_last_updated_at: Option<DateTime<Utc>>,
}

impl Challenger {
    pub fn new(
        address: Address,
        contract_provider: Box<dyn ScribeOptimisticProvider + 'static>,
    ) -> Self {
        Self {
            address,
            contract_provider,
            last_processed_block: None,
            challenge_period_in_sec: 0,
            challenge_period_last_updated_at: None,
        }
    }

    // Reloads challenge period from contract.
    // This function have to be called every N time, because challenge period can be changed by contract owner.
    async fn reload_challenge_period(&mut self) -> Result<()> {
        let challenge_period_in_sec = self
            .contract_provider
            .get_challenge_period(self.address)
            .await?;

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
                diff.to_std().unwrap() > DEFAULT_CHALLENGE_PERIOD_RELOAD_INTERVAL
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

        Ok(last_block_number - blocks_per_period)
    }

    // Check if given block_number for log is already non challengeable
    async fn is_challengeable(
        &self,
        block_number: U64,
        challenge_period_in_sec: u16,
    ) -> Result<bool> {
        // Checking if log is possible to challenge ?
        let block = self
            .contract_provider
            .get_block(block_number)
            .await?
            .unwrap();

        let diff = Utc::now().timestamp() as u64 - block.timestamp.as_u64();

        Ok(challenge_period_in_sec > diff as u16)
    }

    // TODO: Need tests
    fn reject_challenged_pokes(
        &self,
        pokes: Vec<(OpPokedFilter, LogMeta)>,
        challenges: Vec<(OpPokeChallengedSuccessfullyFilter, LogMeta)>,
    ) -> Vec<(OpPokedFilter, LogMeta)> {
        if challenges.is_empty() || pokes.is_empty() {
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
        let latest_block_number = self
            .contract_provider
            .get_latest_block_number()
            .await
            .unwrap();

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
            .contract_provider
            .get_successful_challenges(self.address, from_block, latest_block_number)
            .await?;

        // Fetches `OpPoked` events
        let op_pokes = self
            .contract_provider
            .get_op_pokes(self.address, from_block, latest_block_number)
            .await?;

        // ignoring already challenged pokes
        let unchallenged_pokes = self.reject_challenged_pokes(op_pokes, challenges);

        // Check if we have unchallenged pokes
        if unchallenged_pokes.is_empty() {
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

            let valid = self
                .contract_provider
                .is_schnorr_signature_valid(log.clone())
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
                match self.contract_provider.challenge(log.schnorr_data).await {
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
