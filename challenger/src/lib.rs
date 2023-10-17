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
use tokio::time;

pub mod contract;

use contract::{OpPokeChallengedSuccessfullyFilter, OpPokedFilter, ScribeOptimisticProvider};

// Note: this is true virtually all of the time but because of leap seconds not always.
// We take minimal time just to be sure, it's always better to check outdated blocks
// rather than miss some.
const SLOT_PERIOD_SECONDS: u16 = 12;

// Time interval in seconds to reload challenge period from contract.
const DEFAULT_CHALLENGE_PERIOD_RELOAD_INTERVAL: Duration = Duration::from_secs(600);

// Time interval for checking new pokes in seconds.
const DEFAULT_CHECK_INTERVAL: u64 = 30;

// Max number of failures before we stop processing address.
const MAX_FAILURE_COUNT: u8 = 3;

pub struct Challenger<P: ScribeOptimisticProvider + 'static> {
    address: Address,
    contract_provider: P,
    last_processed_block: Option<U64>,
    challenge_period_in_sec: u16,
    challenge_period_last_updated_at: Option<DateTime<Utc>>,
    failure_count: u8,
}

impl<P> Challenger<P>
where
    P: ScribeOptimisticProvider + 'static,
{
    pub fn new(address: Address, contract_provider: P) -> Self {
        Self {
            address,
            contract_provider,
            last_processed_block: None,
            challenge_period_in_sec: 0,
            challenge_period_last_updated_at: None,
            failure_count: 0,
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
        let unchallenged_pokes = reject_challenged_pokes(op_pokes, challenges);

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
    pub async fn start(&mut self) -> Result<()> {
        let mut interval = time::interval(Duration::from_secs(DEFAULT_CHECK_INTERVAL));

        loop {
            interval.tick().await;

            debug!("Address {:?}, interval tick", self.address);
            match self.process().await {
                Ok(_) => {
                    debug!("All ok, continue with next tick...");
                    // Reset error counter
                    self.failure_count = 0;
                }
                Err(err) => {
                    error!(
                        "Address {:?}, failed to process opPokes: {:?}",
                        self.address, err
                    );

                    // Increment and check error counter
                    self.failure_count += 1;
                    if self.failure_count >= MAX_FAILURE_COUNT {
                        error!(
                            "Address {:?}, reached max failure count, stopping processing...",
                            self.address
                        );
                        return Err(err);
                    }
                }
            }
        }
    }
}

// Removes challenged pokes from list of loaded pokes.
// Logic is very simple, if `OpPokeChallengedSuccessfully` event is after `OpPoked` event, then we can safely
// say that `OpPoked` event is already challenged. So we need to validate sequence of events and remove all
// `OpPoked` events that has `OpPokeChallengedSuccessfully` event after it.
fn reject_challenged_pokes(
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

#[cfg(test)]
mod tests {
    use ethers::types::{H160, H256, U256};

    use super::*;

    // Builds new LogMeta with default values, only `block_number` is useful for us.
    fn new_log_meta(block_number: U64) -> LogMeta {
        LogMeta {
            block_number,
            address: H160::from_low_u64_be(0),
            block_hash: H256::from_low_u64_be(0),
            transaction_hash: H256::from_low_u64_be(0),
            transaction_index: U64::from(0),
            log_index: U256::from(0),
        }
    }

    #[test]
    fn filtering_does_nothing_on_empty_lists() {
        let pokes: Vec<(OpPokedFilter, LogMeta)> = vec![];
        let challenges: Vec<(OpPokeChallengedSuccessfullyFilter, LogMeta)> = vec![];

        let result = reject_challenged_pokes(pokes.clone(), challenges);

        assert_eq!(result, pokes);
    }

    #[test]
    fn one_poke_no_challenges() {
        // Only 1 poke - returns it back
        let pokes: Vec<(OpPokedFilter, LogMeta)> = vec![(
            OpPokedFilter {
                ..Default::default()
            },
            new_log_meta(U64::from(1)),
        )];
        let challenges: Vec<(OpPokeChallengedSuccessfullyFilter, LogMeta)> = vec![];

        let result = reject_challenged_pokes(pokes.clone(), challenges);
        assert_eq!(result, pokes);
    }
    #[test]
    fn one_poke_one_challenge_after_it() {
        let pokes: Vec<(OpPokedFilter, LogMeta)> = vec![(
            OpPokedFilter {
                ..Default::default()
            },
            new_log_meta(U64::from(1)),
        )];
        let challenges: Vec<(OpPokeChallengedSuccessfullyFilter, LogMeta)> = vec![(
            OpPokeChallengedSuccessfullyFilter {
                ..Default::default()
            },
            new_log_meta(U64::from(2)),
        )];

        let result = reject_challenged_pokes(pokes, challenges);
        assert!(result.is_empty());
    }

    #[test]
    fn one_poke_one_challenge_before_poke() {
        let pokes: Vec<(OpPokedFilter, LogMeta)> = vec![(
            OpPokedFilter {
                ..Default::default()
            },
            new_log_meta(U64::from(2)),
        )];
        let challenges: Vec<(OpPokeChallengedSuccessfullyFilter, LogMeta)> = vec![(
            OpPokeChallengedSuccessfullyFilter {
                ..Default::default()
            },
            new_log_meta(U64::from(1)),
        )];

        let result = reject_challenged_pokes(pokes.clone(), challenges);
        assert_eq!(result, pokes);
    }

    #[test]
    fn multiple_pokes_with_one_challenge() {
        let pokes: Vec<(OpPokedFilter, LogMeta)> = vec![
            (
                OpPokedFilter {
                    ..Default::default()
                },
                new_log_meta(U64::from(1)),
            ),
            (
                OpPokedFilter {
                    ..Default::default()
                },
                new_log_meta(U64::from(3)),
            ),
        ];
        let challenges: Vec<(OpPokeChallengedSuccessfullyFilter, LogMeta)> = vec![(
            OpPokeChallengedSuccessfullyFilter {
                ..Default::default()
            },
            new_log_meta(U64::from(2)),
        )];

        let result = reject_challenged_pokes(pokes.clone(), challenges);
        assert_eq!(result.len(), 1);

        let (_, meta) = result.get(0).unwrap();
        assert_eq!(meta.block_number, U64::from(3));
    }

    #[test]
    fn set_of_pokes_and_challenges() {
        let pokes: Vec<(OpPokedFilter, LogMeta)> = vec![
            (
                OpPokedFilter {
                    ..Default::default()
                },
                new_log_meta(U64::from(1)),
            ),
            (
                OpPokedFilter {
                    ..Default::default()
                },
                new_log_meta(U64::from(3)),
            ),
            (
                OpPokedFilter {
                    ..Default::default()
                },
                new_log_meta(U64::from(4)),
            ),
            (
                OpPokedFilter {
                    ..Default::default()
                },
                new_log_meta(U64::from(7)),
            ),
        ];
        let challenges: Vec<(OpPokeChallengedSuccessfullyFilter, LogMeta)> = vec![
            (
                OpPokeChallengedSuccessfullyFilter {
                    ..Default::default()
                },
                new_log_meta(U64::from(2)),
            ),
            (
                OpPokeChallengedSuccessfullyFilter {
                    ..Default::default()
                },
                new_log_meta(U64::from(5)),
            ),
            (
                OpPokeChallengedSuccessfullyFilter {
                    ..Default::default()
                },
                new_log_meta(U64::from(6)),
            ),
        ];

        let result = reject_challenged_pokes(pokes.clone(), challenges);
        assert_eq!(result.len(), 2);

        let (_, meta) = result.get(0).unwrap();
        assert_eq!(meta.block_number, U64::from(3));

        let (_, meta2) = result.get(1).unwrap();
        assert_eq!(meta2.block_number, U64::from(7));
    }
}
