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
use eyre::{bail, Result};
use log::{debug, error, info, warn};
use std::time::Duration;
use tokio::time;

pub mod contract;
pub mod metrics;

use contract::{OpPokeChallengedSuccessfullyFilter, OpPokedFilter, ScribeOptimisticProvider};

// Note: this is true virtually all of the time but because of leap seconds not always.
// We take minimal time just to be sure, it's always better to check outdated blocks
// rather than miss some.
const SLOT_PERIOD_SECONDS: u16 = 12;

// Time interval in seconds to reload challenge period from contract.
const DEFAULT_CHALLENGE_PERIOD_RELOAD_INTERVAL: Duration = Duration::from_secs(600);

// Time interval for checking new pokes in milliseconds.
const DEFAULT_CHECK_INTERVAL_IN_MS: u64 = 30_000;

// Max number of failures before we stop processing address.
const MAX_FAILURE_COUNT: u8 = 3;

#[derive(Debug)]
pub struct Challenger<P: ScribeOptimisticProvider + 'static> {
    address: Address,
    contract_provider: P,
    last_processed_block: Option<U64>,
    challenge_period_in_sec: u16,
    challenge_period_last_updated_at: Option<DateTime<Utc>>,
    max_failure_count: u8,
    failure_count: u8,
    tick_interval: Duration,
}

impl<P> Challenger<P>
where
    P: ScribeOptimisticProvider + 'static,
{
    pub fn new(
        address: Address,
        contract_provider: P,
        tick_interval: Option<u64>,
        max_failure_count: Option<u8>,
    ) -> Self {
        Self {
            address,
            contract_provider,
            last_processed_block: None,
            challenge_period_in_sec: 0,
            challenge_period_last_updated_at: None,
            failure_count: 0,
            max_failure_count: max_failure_count.unwrap_or(MAX_FAILURE_COUNT),
            tick_interval: Duration::from_millis(
                tick_interval.unwrap_or(DEFAULT_CHECK_INTERVAL_IN_MS),
            ),
        }
    }

    // Sets last processed block number in challenger
    fn set_last_processed_block(&mut self, block: U64) {
        self.last_processed_block = Some(block);

        // Updating last scanned block metric
        metrics::set_last_scanned_block(
            self.address,
            self.contract_provider.get_from().unwrap_or_default(),
            block.as_u64() as i64,
        );
    }

    // Reloads challenge period from contract.
    // This function have to be called every N time, because challenge period can be changed by contract owner.
    async fn reload_challenge_period(&mut self) -> Result<()> {
        let challenge_period_in_sec = self.contract_provider.get_challenge_period().await?;

        debug!(
            "[{:?}] Reloaded opChallenge period for contract is {:?}",
            self.address, challenge_period_in_sec
        );
        self.challenge_period_in_sec = challenge_period_in_sec;
        self.challenge_period_last_updated_at = Some(Utc::now());

        Ok(())
    }

    // Reloads the challenge period from the contract if it has not been updated within the default challenge period reload interval.
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
        debug!(
            "[{:?}] Calculating starting block number, latest from chain {:?}, period {:?}",
            self.address, last_block_number, challenge_period_in_sec
        );

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

    // This function is called every tick.
    // Deciedes blocks range we have to process, processing them and if no error happened sets new `last_processed_block` for next tick.
    // If error happened on next tick it will again try to process same blocks.
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

        // In some cases (block drop) our latest processed block can be bigger than latest block from chain,
        // in this case we have to skip processing and reset last processed block, so on next tick we will retry.
        // Also we returning error to increase failure count.
        if from_block > latest_block_number {
            // Resetting last processed block with latest chain block
            self.set_last_processed_block(latest_block_number);

            bail!(
                "Invalid block range {:?} - {:?}, from block is bigger than to block, resetting last processed block to latest block from chain.",
                from_block, latest_block_number
            );
        }

        // Processing blocks range
        self.process_blocks_range(from_block, latest_block_number)
            .await?;

        // Updating last processed block with latest chain block
        self.set_last_processed_block(latest_block_number);

        Ok(())
    }

    // Validates all `OpPoked` events and challenges them if needed.
    async fn process_blocks_range(&mut self, from_block: U64, to_block: U64) -> Result<()> {
        debug!(
            "[{:?}] Processing blocks range {:?} - {:?}",
            self.address, from_block, to_block
        );

        // Fetch list of `OpPokeChallengedSuccessfully` events
        let challenges = self
            .contract_provider
            .get_successful_challenges(from_block, to_block)
            .await?;

        // Fetches `OpPoked` events
        let op_pokes = self
            .contract_provider
            .get_op_pokes(from_block, to_block)
            .await?;

        // ignoring already challenged pokes
        let unchallenged_pokes = reject_challenged_pokes(op_pokes, challenges);

        // Check if we have unchallenged pokes
        if unchallenged_pokes.is_empty() {
            debug!(
                "[{:?}] No unchallenged opPokes found in block range {:?} - {:?}, skipping...",
                self.address, from_block, to_block
            );
            return Ok(());
        }

        for (poke, meta) in unchallenged_pokes {
            let challengeable = self
                .is_challengeable(meta.block_number, self.challenge_period_in_sec)
                .await?;

            if !challengeable {
                error!(
                    "[{:?}] Block is to old for `opChallenge` block number: {:?}",
                    self.address, meta.block_number
                );
                continue;
            }

            let valid = self
                .contract_provider
                .is_schnorr_signature_valid(poke.clone())
                .await?;

            // If schnorr data is valid, we should not challenge it...
            if valid {
                debug!(
                    "[{:?}] Schnorr data for block {:?} is valid, nothing to do...",
                    self.address, meta.block_number
                );

                continue;
            }

            info!(
                "[{:?}] Schnorr data for block {:?} is not valid, trying to challenge...",
                self.address, meta.block_number
            );

            // TODO: handle error gracefully, we should go further even if error happened
            match self.contract_provider.challenge(poke.schnorr_data).await {
                Ok(receipt) => {
                    if let Some(receipt) = receipt {
                        info!(
                            "[{:?}] Successfully sent `opChallenge` transaction for OpPoke on block {:?}: {:?}",
                            self.address, meta.block_number, receipt
                        );
                        // Add challenge to metrics
                        metrics::inc_challenge_counter(
                            self.address,
                            self.contract_provider.get_from().unwrap_or_default(),
                            receipt.transaction_hash,
                        );
                    } else {
                        warn!(
                                "[{:?}] Successfully sent `opChallenge` for block {:?} transaction but no receipt returned",
                                self.address, meta.block_number
                            );
                    }
                }
                Err(err) => {
                    error!(
                        "[{:?}] Failed to make `opChallenge` call for block {:?}: {:?}",
                        self.address, meta.block_number, err
                    );
                }
            };
        }

        Ok(())
    }

    /// Starts processing pokes for the given contract address using the specified provider and tick interval.
    ///
    /// The function uses a tokio::time::interval to run the process method at regular intervals specified by the tick_interval field.
    ///
    /// # Used arguments
    ///
    /// * `contract_address` - The address of the contract to process pokes for.
    /// * `provider` - The provider to use for interacting with the Ethereum network.
    /// * `tick_interval` - The interval at which to check for new pokes.
    ///
    /// # Examples
    ///
    /// ```
    /// use eyre::Result;
    /// use ethers::providers::{Http, Provider};
    /// use challenger::{Challenger, HttpScribeOptimisticProvider};
    /// use std::time::Duration;
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<()> {
    ///     let rpc_provider = Provider::<Http>::connect("https://mainnet.infura.io/v3/your-project-id").await?;
    ///     let contract_address = "0x1234567890123456789012345678901234567890".parse()?;
    ///     let provider = HttpScribeOptimisticProvider::new(contract_address, rpc_provider);
    ///     let mut challenger = Challenger::new(contract_address, provider, Duration::from_secs(30), None);
    ///
    ///     challenger.start().await?
    /// }
    /// ```
    pub async fn start(&mut self) -> Result<()> {
        let mut interval = time::interval(self.tick_interval);

        loop {
            match self.process().await {
                Ok(_) => {
                    // Reset error counter
                    self.failure_count = 0;
                }
                Err(err) => {
                    error!("[{:?}] Failed to process opPokes: {:?}", self.address, err);

                    // Increment error counter
                    metrics::inc_errors_counter(
                        self.address,
                        self.contract_provider.get_from().unwrap_or_default(),
                        &err.to_string(),
                    );

                    // Increment and check error counter
                    self.failure_count += 1;
                    if self.failure_count >= self.max_failure_count {
                        error!(
                            "[{:?}] Reached max failure count, stopping processing...",
                            self.address
                        );
                        return Err(err);
                    }
                }
            }

            interval.tick().await;
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
    use super::*;

    use async_trait::async_trait;
    use contract::SchnorrData;
    use ethers::{
        contract::LogMeta,
        types::{Block, TransactionReceipt, H160, H256, U256, U64},
    };
    use eyre::Result;
    use mockall::{mock, predicate::*};

    mock! {
        pub TestScribe{}

        #[async_trait]
        impl ScribeOptimisticProvider for TestScribe {
            async fn get_latest_block_number(&self) -> Result<U64>;

            async fn get_block(&self, block_number: U64) -> Result<Option<Block<H256>>>;

            async fn get_challenge_period(&self) -> Result<u16>;

            async fn get_successful_challenges(
                &self,
                from_block: U64,
                to_block: U64,
            ) -> Result<Vec<(OpPokeChallengedSuccessfullyFilter, LogMeta)>>;

            async fn get_op_pokes(
                &self,
                from_block: U64,
                to_block: U64,
            ) -> Result<Vec<(OpPokedFilter, LogMeta)>>;

            async fn is_schnorr_signature_valid(&self, op_poked: OpPokedFilter) -> Result<bool>;

            async fn challenge(&self, schnorr_data: SchnorrData) -> Result<Option<TransactionReceipt>>;
        }
    }

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
    fn test_reject_challenged_pokes() {
        {
            // Does nothing if no pokes or challenges
            let pokes: Vec<(OpPokedFilter, LogMeta)> = vec![];
            let challenges: Vec<(OpPokeChallengedSuccessfullyFilter, LogMeta)> = vec![];

            let result = reject_challenged_pokes(pokes.clone(), challenges);

            assert_eq!(result, pokes);
        }

        {
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

        {
            // One poke one challenge after it
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

        {
            // One poke one challenge before it
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

        {
            // Multi pokes - one challenge after first poke
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

            let (_, meta) = result.first().unwrap();
            assert_eq!(meta.block_number, U64::from(3));
        }

        {
            // Multi pokes & multi challenges in random order
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

            let (_, meta) = result.first().unwrap();
            assert_eq!(meta.block_number, U64::from(3));

            let (_, meta2) = result.get(1).unwrap();
            assert_eq!(meta2.block_number, U64::from(7));
        }
    }

    #[tokio::test]
    async fn test_challenger_returns_error_on_max_failures() {
        // Just random address
        let address = "0x3D4c07Bd3cf5FB80ACB6Ec31531DBB338329b5F5"
            .parse::<Address>()
            .unwrap();
        let max_failures: u8 = 3;

        // Setting up mock
        let mut mock = MockTestScribe::new();
        mock.expect_get_latest_block_number()
            .returning(|| Ok(U64::from(100)));

        mock.expect_get_block().returning(|_| Ok(None));

        mock.expect_get_challenge_period().returning(|| Ok(600));

        // Let's fail on this function.
        mock.expect_get_successful_challenges()
            .times(usize::from(max_failures))
            .returning(|_, _| eyre::bail!("Error challenges"));

        mock.expect_get_op_pokes()
            .returning(|_, _| eyre::bail!("Error op_pokes"));

        // Setting up challenger
        let mut challenger = Challenger::new(address, mock, Some(10), Some(max_failures));

        let res = challenger.start().await;
        assert!(res.is_err());
    }

    #[tokio::test]
    #[should_panic]
    async fn test_challenge_period_requires_to_be_fetched() {
        // Just random address
        let address = "0x3D4c07Bd3cf5FB80ACB6Ec31531DBB338329b5F5"
            .parse::<Address>()
            .unwrap();

        // Setting up mock
        let mut mock = MockTestScribe::new();
        mock.expect_get_latest_block_number()
            .returning(|| Ok(U64::from(100)));

        mock.expect_get_block().returning(|_| Ok(None));

        mock.expect_get_challenge_period()
            .returning(|| eyre::bail!("Error get challenge period"));

        // Let's fail on this function.
        mock.expect_get_successful_challenges()
            .returning(|_, _| Ok(vec![]));

        mock.expect_get_op_pokes().returning(|_, _| Ok(vec![]));

        // Setting up challenger
        let mut challenger = Challenger::new(address, mock, Some(10), None);

        challenger.process().await.unwrap();
    }

    #[tokio::test]
    async fn test_last_processed_block_bigger_than_last_chain_block() {
        // Just random address
        let address = "0x3D4c07Bd3cf5FB80ACB6Ec31531DBB338329b5F5"
            .parse::<Address>()
            .unwrap();
        let max_failures: u8 = 3;

        // Setting up mock
        let mut mock = MockTestScribe::new();
        mock.expect_get_latest_block_number()
            .returning(|| Ok(U64::from(1000)));

        mock.expect_get_challenge_period().returning(|| Ok(600));
        mock.expect_get_successful_challenges().never();
        mock.expect_get_op_pokes().never();
        mock.expect_challenge().never();
        mock.expect_get_block().never();

        // Setting up challenger
        let mut challenger = Challenger::new(address, mock, Some(10), Some(max_failures));
        challenger.set_last_processed_block(U64::from(1001));

        let res = challenger.process().await;
        // Should be error in result.
        assert!(res.is_err());
        // Check we reseted the last processed block
        assert!(challenger.last_processed_block == Some(U64::from(1000)));
    }

    #[tokio::test]
    async fn test_no_pokes_no_execution() {
        // Just random address
        let address = "0x3D4c07Bd3cf5FB80ACB6Ec31531DBB338329b5F5"
            .parse::<Address>()
            .unwrap();
        let max_failures: u8 = 3;

        // Setting up mock
        let mut mock = MockTestScribe::new();
        mock.expect_get_latest_block_number()
            .returning(|| Ok(U64::from(1000)));

        mock.expect_get_challenge_period().returning(|| Ok(600));
        // Let's fail on this function.
        mock.expect_get_successful_challenges()
            .returning(|_, _| Ok(vec![]));
        mock.expect_get_op_pokes().returning(|_, _| Ok(vec![]));
        mock.expect_challenge().never();

        mock.expect_get_block().never();

        // Setting up challenger
        let mut challenger = Challenger::new(address, mock, Some(10), Some(max_failures));

        let res = challenger.process().await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_calls_challenge_on_invalid_signature() {
        // Just random address
        let address = "0x3D4c07Bd3cf5FB80ACB6Ec31531DBB338329b5F5"
            .parse::<Address>()
            .unwrap();
        let max_failures: u8 = 3;

        // Setting up mock
        let mut mock = MockTestScribe::new();
        mock.expect_get_latest_block_number()
            .returning(|| Ok(U64::from(1000)));

        mock.expect_get_challenge_period().returning(|| Ok(600));
        // Let's fail on this function.
        mock.expect_get_successful_challenges()
            .returning(|_, _| Ok(vec![]));

        mock.expect_get_op_pokes().returning(|_, _| {
            Ok(vec![(
                OpPokedFilter {
                    ..Default::default()
                },
                new_log_meta(U64::from(999)),
            )])
        });
        mock.expect_is_schnorr_signature_valid()
            .once()
            .returning(|_| Ok(false));
        // Called only once !
        mock.expect_challenge().once().returning(|_| Ok(None));

        mock.expect_get_block().returning(|_| {
            Ok(Some(Block {
                timestamp: U256::from(Utc::now().timestamp()),
                ..Default::default()
            }))
        });

        // Setting up challenger
        let mut challenger = Challenger::new(address, mock, Some(10), Some(max_failures));

        let res = challenger.process().await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_ignores_challenge_on_valid_signature() {
        // Just random address
        let address = "0x3D4c07Bd3cf5FB80ACB6Ec31531DBB338329b5F5"
            .parse::<Address>()
            .unwrap();
        let max_failures: u8 = 3;

        // Setting up mock
        let mut mock = MockTestScribe::new();
        mock.expect_get_latest_block_number()
            .returning(|| Ok(U64::from(1000)));

        mock.expect_get_challenge_period().returning(|| Ok(600));
        // Let's fail on this function.
        mock.expect_get_successful_challenges()
            .returning(|_, _| Ok(vec![]));

        mock.expect_get_op_pokes().returning(|_, _| {
            Ok(vec![(
                OpPokedFilter {
                    ..Default::default()
                },
                new_log_meta(U64::from(999)),
            )])
        });
        mock.expect_is_schnorr_signature_valid()
            .once()
            .returning(|_| Ok(true));
        // Never called
        mock.expect_challenge().never();

        mock.expect_get_block().returning(|_| {
            Ok(Some(Block {
                timestamp: U256::from(Utc::now().timestamp()),
                ..Default::default()
            }))
        });

        // Setting up challenger
        let mut challenger = Challenger::new(address, mock, Some(10), Some(max_failures));

        let res = challenger.process().await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_blocks_older_than_challenge_period_are_ignored() {
        // Just random address
        let address = "0x3D4c07Bd3cf5FB80ACB6Ec31531DBB338329b5F5"
            .parse::<Address>()
            .unwrap();
        let max_failures: u8 = 3;

        // Setting up mock
        let mut mock = MockTestScribe::new();
        mock.expect_get_latest_block_number()
            .returning(|| Ok(U64::from(1000)));

        mock.expect_get_challenge_period().returning(|| Ok(600));
        // Let's fail on this function.
        mock.expect_get_successful_challenges()
            .returning(|_, _| Ok(vec![]));

        mock.expect_get_op_pokes().returning(|_, _| {
            Ok(vec![(
                OpPokedFilter {
                    ..Default::default()
                },
                new_log_meta(U64::from(999)),
            )])
        });
        // Never called, due to expired timestamp for poke
        mock.expect_is_schnorr_signature_valid().never();
        // Never called
        mock.expect_challenge().never();

        // Returning block older than `get_challenge_period`
        mock.expect_get_block().once().returning(|_| {
            Ok(Some(Block {
                timestamp: U256::from(Utc::now().timestamp() - 601),
                ..Default::default()
            }))
        });

        // Setting up challenger
        let mut challenger = Challenger::new(address, mock, Some(10), Some(max_failures));

        let res = challenger.process().await;
        assert!(res.is_ok());
    }
}
