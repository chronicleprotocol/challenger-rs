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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, FixedBytes};
use alloy::providers::Provider;
use alloy::rpc::types::BlockTransactionsKind;
use eyre::{bail, Context, Result};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::contract::Event;
use crate::contract::ScribeOptimistic::OpPoked;
use crate::contract::{
    EventWithMetadata, ScribeOptimisticProvider, ScribeOptimisticProviderInstance,
};
use crate::events_listener::RetryProviderWithSigner;
use crate::metrics;

const CHALLENGE_POKE_DELAY_MS: u64 = 200;
const FLASHBOT_CHALLENGE_RETRY_COUNT: u64 = 3;
const CLASSIC_CHALLENGE_RETRY_COUNT: u64 = 3;

/// You can think about `EventDistributor` as a router for events.
/// It listens for contract events from logs and distributes them to the appropriate `ScribeEventsProcessor` instance via channels.
/// Each `ScribeOptimistic` instance deployed to address should have its own `ScribeEventsProcessor` processor.
///
/// `EventDistributor` is responsible for creating and managing `ScribeEventsProcessor` instances.
/// You don't need to spawn anything manually, just call `start` and it will take care of the rest.
pub struct EventDistributor {
    addresses: Vec<Address>,
    cancel: CancellationToken,
    provider: Arc<RetryProviderWithSigner>,
    flashbot_provider: Arc<RetryProviderWithSigner>,
    rx: Receiver<EventWithMetadata>,
    txs: HashMap<Address, Sender<EventWithMetadata>>,
}

impl EventDistributor {
    pub fn new(
        addresses: Vec<Address>,
        cancel: CancellationToken,
        provider: Arc<RetryProviderWithSigner>,
        flashbot_provider: Arc<RetryProviderWithSigner>,
        rx: Receiver<EventWithMetadata>,
    ) -> Self {
        Self {
            addresses,
            cancel,
            provider,
            flashbot_provider,
            rx,
            txs: HashMap::new(),
        }
    }

    /// Spawns list of `ScribeEventsProcessor` instances for each address in `addresses`,
    /// creates a channel for each address and starts listening for events.
    pub async fn start(&mut self) -> Result<()> {
        let mut contract_handler_set = JoinSet::new();
        // Create a contract handler for each scribe address
        for address in &self.addresses {
            let (tx, rx) = tokio::sync::mpsc::channel::<EventWithMetadata>(100);
            self.txs.insert(*address, tx);

            let mut contract_handler = ScribeEventsProcessor::new(
                *address,
                self.provider.clone(),
                self.flashbot_provider.clone(),
                self.cancel.clone(),
                rx,
            );
            contract_handler_set.spawn(async move {
                let _ = contract_handler.start().await;
            });
        }
        log::debug!("EventDistributor: All processors started, listening for events...");

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    log::info!("EventDistributor: received cancel command, stopping...");
                    break;
                }
                event = self.rx.recv() => {
                    if let Some(event) = event {
                        // Send the event to the appropriate contract handler
                        match self.txs.get(&event.address) {
                            Some(tx) => {
                                let _ = tx.send(event).await;
                            }
                            _ => {
                                // Should never happen
                                log::warn!(
                                    "EventDistributor: Received event from unknown address or receiver is dead: {:?}",
                                    event
                                );
                            }
                        }
                    }
                }
            }
        }
        // Wait for all tasks to finish
        contract_handler_set.join_all().await;
        Ok(())
    }
}

// Receives preparsed [crate::contract::EventWithMetadata] events for a `ScribeOptimistic` instance on address,
// validates `OpPoked` events and challenges them if they are invalid and within the challenge period.
//
// `OpPoked` validation and challenge logic is launched in a separate task and is cancellable.
// When new `OpPoked` event is received, it's challenge process is started with a delay of `CHALLENGE_POKE_DELAY_MS`.
// If next received event will be `OpPokeChallengedSuccessfully`, the challenge process is cancelled (no need to spend resources on validation).
// Otherwise, the challenge process will start procecssing.
struct ScribeEventsProcessor {
    address: Address,
    provider: Arc<RetryProviderWithSigner>,
    flashbot_provider: Arc<RetryProviderWithSigner>,
    cancellation_token: CancellationToken,
    rx: Receiver<EventWithMetadata>,
    cancel_challenge: Option<CancellationToken>,
    challenge_period: Option<u64>,
}

impl ScribeEventsProcessor {
    pub fn new(
        scribe_address: Address,
        provider: Arc<RetryProviderWithSigner>,
        flashbot_provider: Arc<RetryProviderWithSigner>,
        cancel: CancellationToken,
        rx: Receiver<EventWithMetadata>,
    ) -> Self {
        Self {
            address: scribe_address,
            provider,
            flashbot_provider,
            cancellation_token: cancel,
            rx,
            cancel_challenge: None,
            challenge_period: None,
        }
    }

    async fn start(&mut self) -> Result<()> {
        log::debug!(
            "ScribeEventsProcessor[{:?}] Starting new contract handler",
            self.address
        );
        // We have to fail if no challenge period is fetched on start
        self.refresh_challenge_period().await.unwrap();

        loop {
            tokio::select! {
                // main process terminates, need to finish work and exit...
                _ = self.cancellation_token.cancelled() => {
                    log::info!(
                        "ScribeEventsProcessor[{:?}] Cancellation requested, stopping contract handler",
                        self.address
                    );
                    return Ok(());
                }
                // new [EventWithMetadata] received, process it...
                event = self.rx.recv() => {
                    if let Some(event) = event {
                        match &event.event {
                            // For `OpPoked` events, check if `schnorr_signature` is valid,
                            // if not - check if event is within the challenge period, send challenge.
                            // If `schnorr_signature` is valid, do nothing.
                            Event::OpPoked(op_poked) => {
                                log::trace!(
                                    "ScribeEventsProcessor[{:?}] OpPoked received, start processing",
                                    self.address
                                );

                                if let Err(err) = self.process_op_poked(event.clone(), op_poked.clone()).await {
                                    log::error!(
                                        "ScribeEventsProcessor[{:?}] Error processing OpPoked event: {:?}",
                                        self.address,
                                        err
                                    );
                                }
                            }
                            // If the challenge is already successful, cancel the previous challenge process
                            Event::OpPokeChallengedSuccessfully(_) => {
                                log::debug!(
                                    "ScribeEventsProcessor[{:?}] OpPokeChallengedSuccessfully received, cancelling challenge process",
                                    self.address
                                );
                                self.cancel_challenge().await;
                            }
                        }
                    } else {
                        log::warn!(
                            "ScribeEventsProcessor[{:?}] Received None event in contract handler",
                            self.address
                        );
                    }
                }
            }
        }
    }

    // Starts the validation and challenge process for the `OpPoked` event.
    async fn process_op_poked(
        &mut self,
        event: EventWithMetadata,
        op_poked: OpPoked,
    ) -> Result<()> {
        // Check if the poke is within the challenge period
        let event_timestamp = match event.log.block_timestamp {
            Some(timestamp) => timestamp,
            None => self
                .get_timestamp_from_block(&event)
                .await
                .wrap_err("Failed to get timestamp from block via RPC call")?,
        };

        let current_timestamp = chrono::Utc::now().timestamp() as u64;
        log::debug!(
            "ScribeEventsProcessor[{:?}] OpPoked, event_timestamp: {:?}, current_timestamp: {:?}",
            self.address,
            event_timestamp,
            current_timestamp
        );
        if current_timestamp - event_timestamp > self.challenge_period.unwrap() {
            // This log is expected in tests, tests must be updated if log message is changed
            log::debug!(
                "ScribeEventsProcessor[{:?}] OpPoked received outside of challenge period",
                self.address
            );
            return Ok(());
        }

        log::debug!(
            "ScribeEventsProcessor[{:?}] Spawning validation and challenge process...",
            self.address
        );
        self.spawn_challenge(op_poked).await;

        Ok(())
    }

    // Gets the timestamp from the block by `event.log.block_number`, if it is missing, returns an error
    async fn get_timestamp_from_block(&self, event: &EventWithMetadata) -> Result<u64> {
        // Possible block number to be missing for unconfirmed blocks
        if event.log.block_number.is_none() {
            bail!("Block number is missing for log {:?}", event.log);
        }
        let block_number = event.log.block_number.unwrap();

        let block = self
            .provider
            .get_block(block_number.into(), BlockTransactionsKind::Hashes)
            .await?;

        let block = match block {
            Some(block) => block,
            None => {
                bail!("Block with number {:?} not found", block_number);
            }
        };

        Ok(block.header.timestamp)
    }

    async fn refresh_challenge_period(&mut self) -> Result<()> {
        let period = ScribeOptimisticProviderInstance::new(self.address, self.provider.clone())
            .get_challenge_period()
            .await?;

        self.challenge_period = Some(period as u64);
        Ok(())
    }

    async fn spawn_challenge(&mut self, op_poked: OpPoked) {
        // Ensure there is no existing challenge
        self.cancel_challenge().await;
        // Create a new cancellation token
        self.cancel_challenge = Some(CancellationToken::new());
        // Create a new challenger instance
        let challenge_handler = Some(Arc::new(Mutex::new(OpPokedValidator::new(
            op_poked,
            self.cancellation_token.clone(),
            // cancel_challenge guaranteed to be Some
            self.cancel_challenge.as_ref().unwrap().clone(),
            self.address,
            self.provider.clone(),
            self.flashbot_provider.clone(),
        ))));

        // Spawn the asynchronous task
        tokio::spawn(async move {
            let handler = challenge_handler.as_ref().unwrap().lock().await;
            let _ = handler.start().await;
        });
        log::debug!(
            "ScribeEventsProcessor[{:?}] Spawned New challenger process",
            self.address
        );
    }

    async fn cancel_challenge(&mut self) {
        if let Some(cancel) = &self.cancel_challenge {
            log::debug!(
                "ScribeEventsProcessor[{:?}] Cancelling existing challenge",
                self.address
            );
            cancel.cancel();
            self.cancel_challenge = None;
        }
    }
}

// Handle the challenge process for a specific OpPoked event after a delay
// If cancelled before end of delay or inbetween retries stop process
// First try challenge with flashbot provider, then with normal provider
struct OpPokedValidator {
    op_poked: OpPoked,
    global_cancel: CancellationToken,
    cancel: CancellationToken,
    address: Address,
    // TODO replace with ScribeOptimisticProviderInstances
    provider: Arc<RetryProviderWithSigner>,
    flashbot_provider: Arc<RetryProviderWithSigner>,
}

impl OpPokedValidator {
    pub fn new(
        op_poked: OpPoked,
        global_cancel: CancellationToken,
        cancel: CancellationToken,
        address: Address,
        provider: Arc<RetryProviderWithSigner>,
        flashbot_provider: Arc<RetryProviderWithSigner>,
    ) -> Self {
        Self {
            op_poked,
            global_cancel,
            cancel,
            address,
            provider,
            flashbot_provider,
        }
    }

    pub async fn start(&self) -> Result<()> {
        // This checked for in tests, tests must be updated if log is changed
        log::debug!(
            "OpPokedValidator[{:?}] OpPoked validation started",
            self.address
        );
        tokio::select! {
            // Check if the challenge has been cancelled
            _ = self.cancel.cancelled() => {
                log::debug!("OpPokedValidator[{:?}] Challenge cancelled", self.address);
                Ok(())
            }
            // Check if the global cancel command sent
            _ = self.global_cancel.cancelled() => {
                log::debug!("OpPokedValidator[{:?}] Global cancel command received", self.address);
                Ok(())
            }
            _ = tokio::time::sleep(Duration::from_millis(CHALLENGE_POKE_DELAY_MS)) => {
                // Verify that the OpPoked is valid
                let is_valid = ScribeOptimisticProviderInstance::new(
                    self.address,
                    self.provider.clone()
                ).is_schnorr_signature_valid(self.op_poked.clone()).await?;

                if is_valid {
                    log::debug!("OpPokedValidator[{:?}] OpPoked is valid, no need to challenge", self.address);
                    return Ok(());
                }

                // Perform the challenge process
                self.challenge().await?;
                Ok(())
            }
        }
    }

    // Perform the challenge process, first with flashbot provider, then with normal provider.
    async fn challenge(&self) -> Result<FixedBytes<32>> {
        // Perform the challenge after 200ms
        let mut challenge_attempts: u64 = 0;
        const RETRY_RANGE_END: u64 = CLASSIC_CHALLENGE_RETRY_COUNT + FLASHBOT_CHALLENGE_RETRY_COUNT;

        log::info!(
            "OpPokedValidator[{:?}] Challending data: {:?}",
            self.address,
            self.op_poked
        );

        loop {
            match challenge_attempts {
                0..FLASHBOT_CHALLENGE_RETRY_COUNT => {
                    log::debug!(
                        "OpPokedValidator[{:?}] Attempting flashbot challenge",
                        self.address
                    );
                    let contract = ScribeOptimisticProviderInstance::new(
                        self.address,
                        self.flashbot_provider.clone(),
                    );

                    let result = contract.challenge(self.op_poked.schnorrData.clone()).await;

                    match result {
                        Ok(tx_hash) => {
                            log::info!(
                                "OpPokedValidator[{:?}] Flashbot transaction sent via flashbots RPC: {:?}",
                                self.address,
                                tx_hash
                            );

                            // Increment the challenge counter
                            metrics::inc_challenge_counter(self.address, true);
                            return Ok(tx_hash);
                        }
                        Err(e) => {
                            log::error!(
                                "OpPokedValidator[{:?}] Failed to send challenge transaction via flashbots: {:?}",
                                self.address,
                                e
                            );
                        }
                    }
                }
                FLASHBOT_CHALLENGE_RETRY_COUNT..RETRY_RANGE_END => {
                    log::debug!(
                        "OpPokedValidator[{:?}] Attempting public challenge",
                        self.address
                    );
                    let contract =
                        ScribeOptimisticProviderInstance::new(self.address, self.provider.clone());
                    match contract.challenge(self.op_poked.schnorrData.clone()).await {
                        Ok(tx_hash) => {
                            log::info!(
                                "OpPokedValidator[{:?}] Challenge transaction sent via public RPC: {:?}",
                                self.address,
                                tx_hash
                            );
                            // Increment the challenge counter
                            metrics::inc_challenge_counter(self.address, false);
                            return Ok(tx_hash);
                        }
                        Err(e) => {
                            log::error!(
                                "OpPokedValidator[{:?}] Failed to send challenge transaction via public RPC: {:?}",
                                self.address,
                                e
                            );
                        }
                    }
                }
                _ => {
                    log::error!(
                        "OpPokedValidator[{:?}] Challenge failed, total attempts {:?}",
                        self.address,
                        challenge_attempts
                    );
                    break;
                }
            }
            challenge_attempts += 1;
        }

        bail!(format!(
            "OpPokedValidator[{:?}] Challenge failed, total attempts {:?}",
            self.address, challenge_attempts
        ))
    }
}
