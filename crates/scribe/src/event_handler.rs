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

use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::rpc::types::BlockTransactionsKind;
use eyre::{bail, Result};
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

const CHALLENGE_POKE_DELAY_MS: u64 = 200;
const FLASHBOT_CHALLENGE_RETRY_COUNT: u64 = 3;
const CLASSIC_CHALLENGE_RETRY_COUNT: u64 = 3;

// Takes event logs and distributes them to the appropriate contract handler
// Each `ScribeOptimistic` instance deployed to address has its own `EventHandler` process.
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

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
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
                                log::warn!("Received event for unknown address: {:?}", event.address);
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

    pub async fn start(&mut self) -> Result<()> {
        // We have to fail if no challenge period is fetched
        self.fetch_challenge_period().await.unwrap();

        loop {
            tokio::select! {
                // main process terminates, need to finish work and exit...
                _ = self.cancellation_token.cancelled() => {
                    log::info!(
                        "[{:?}] Cancellation requested, stopping contract handler",
                        self.address
                    );
                    return Ok(());
                }
                // new [EventWithMetadata] received, process it...
                event = self.rx.recv() => {
                    log::debug!("[{:?}] Received event: {:?}", self.address, event);

                    if let Some(event) = event {
                        match &event.event {
                            // For `OpPoked` events, check if `schnorr_signature` is valid,
                            // if not - check if event is within the challenge period, send challenge.
                            // If `schnorr_signature` is valid, do nothing.
                            Event::OpPoked(op_poked) => {
                                log::debug!("[{:?}] OpPoked received", self.address);

                                if let Err(err) = self.process_op_poked(event.clone(), op_poked.clone()).await {
                                    log::error!(
                                        "[{:?}] Error processing OpPoked event: {:?}",
                                        self.address,
                                        err
                                    );
                                }
                            }
                            // If the challenge is already successful, cancel the previous challenge process
                            Event::OpPokeChallengedSuccessfully(_) => {
                                self.cancel_challenge().await;
                            }
                        }
                    }
                }
            }
        }
    }

    async fn process_op_poked(
        &mut self,
        event: EventWithMetadata,
        op_poked: OpPoked,
    ) -> Result<()> {
        // Check if the poke is within the challenge period
        let event_timestamp = match event.log.block_timestamp {
            Some(timestamp) => timestamp,
            None => self.get_timestamp_from_block(&event).await?,
        };

        let current_timestamp = chrono::Utc::now().timestamp() as u64;
        log::debug!(
            "[{:?}] OpPoked, event_timestamp: {:?}, current_timestamp: {:?}",
            self.address,
            event_timestamp,
            current_timestamp
        );
        if current_timestamp - event_timestamp > self.challenge_period.unwrap() {
            // This log is expected in tests, tests must be updated if log message is changed
            log::debug!(
                "[{:?}] OpPoked received outside of challenge period",
                self.address
            );
            return Ok(());
        }
        log::debug!("Spawning challenge...");
        self.spawn_challenge(op_poked).await;

        Ok(())
    }

    // Gets the timestamp from the block by `event.log.block_number`, if it is missing, returns an error
    async fn get_timestamp_from_block(&self, event: &EventWithMetadata) -> Result<u64> {
        // Possible block number to be missing for unconfirmed blocks
        if event.log.block_number.is_none() {
            bail!(
                "[{:?}] Block number is missing for log {:?}",
                self.address,
                event.log
            );
        }
        let block_number = event.log.block_number.unwrap();

        let block = self
            .provider
            .get_block(block_number.into(), BlockTransactionsKind::Hashes)
            .await?;

        let block = match block {
            Some(block) => block,
            None => {
                bail!(
                    "[{:?}] Block with number {:?} not found",
                    self.address,
                    block_number
                );
            }
        };

        Ok(block.header.timestamp)
    }

    async fn fetch_challenge_period(&mut self) -> Result<()> {
        // TODO: Add expiration time for the challenge period
        if self.challenge_period.is_some() {
            return Ok(());
        }
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
        let challenge_handler = Some(Arc::new(Mutex::new(ChallengeSender::new(
            op_poked,
            self.cancellation_token.clone(),
            // cancel_challenge garunteed to be Some
            self.cancel_challenge.as_ref().unwrap().clone(),
            self.address,
            self.provider.clone(),
            self.flashbot_provider.clone(),
        ))));
        // TODO: Validate challenge period first...

        // Spawn the asynchronous task
        tokio::spawn(async move {
            let handler = challenge_handler.as_ref().unwrap().lock().await;
            let _ = handler.start().await;
        });
        log::debug!("[{:?}] Spawned New challenger process", self.address);
    }

    async fn cancel_challenge(&mut self) {
        if let Some(cancel) = &self.cancel_challenge {
            log::debug!("[{:?}] Cancelling existing challenge", self.address);
            cancel.cancel();
            self.cancel_challenge = None;
        }
    }
}

// Handle the challenge process for a specific OpPoked event after a delay
// If cancelled before end of delay or inbewteen retries stop process
// First try challenge with flashbot provider, then with normal provider
struct ChallengeSender {
    pub op_poked: OpPoked,
    pub global_cancel: CancellationToken,
    pub cancel: CancellationToken,
    address: Address,
    // TODO replace with ScribeOptimisticProviderInstances
    provider: Arc<RetryProviderWithSigner>,
    flashbot_provider: Arc<RetryProviderWithSigner>,
}

impl ChallengeSender {
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
        log::debug!("[{:?}] Challenge started", self.address);
        // Perform the challenge after 200ms
        let mut challenge_attempts: u64 = 0;
        loop {
            tokio::select! {
                // Check if the challenge has been cancelled
                _ = self.cancel.cancelled() => {
                    log::debug!("[{:?}] Challenge cancelled", self.address);
                    break;
                }
                // Check if the global cancel command sent
                _ = self.global_cancel.cancelled() => {
                    log::debug!("[{:?}] Global cancel command received", self.address);
                    break;
                }
                _ = tokio::time::sleep(Duration::from_millis(CHALLENGE_POKE_DELAY_MS)) => {
                    // Verify that the OpPoked is valid
                    let is_valid = ScribeOptimisticProviderInstance::new(
                        self.address,
                        self.provider.clone()
                    ).is_schnorr_signature_valid(self.op_poked.clone()).await?;

                    if is_valid {
                        log::debug!("[{:?}] OpPoked is valid, no need to challenge", self.address);
                        break;
                    }

                    const RETRY_RANGE_END: u64 = CLASSIC_CHALLENGE_RETRY_COUNT+FLASHBOT_CHALLENGE_RETRY_COUNT;
                    match challenge_attempts {
                        0..FLASHBOT_CHALLENGE_RETRY_COUNT => {

                            let contract = ScribeOptimisticProviderInstance::new(
                                self.address, self.flashbot_provider.clone()
                            );


                            let result = contract.challenge(
                                self.op_poked.schnorrData.clone()
                            ).await;
                            match result {
                                Ok(tx_hash) => {
                                    log::debug!("[{:?}] Flashbot transaction sent: {:?}", self.address, tx_hash);
                                    break;
                                }
                                Err(e) => {
                                    log::error!("[{:?}] Failed to send flashbot transaction: {:?}", self.address, e);
                                }
                            }
                        }
                        FLASHBOT_CHALLENGE_RETRY_COUNT..RETRY_RANGE_END => {
                            let contract = ScribeOptimisticProviderInstance::new(
                                self.address, self.provider.clone()
                            );
                            match contract.challenge(self.op_poked.schnorrData.clone()).await {
                                Ok(tx_hash) => {
                                    log::debug!("[{:?}] Challenge transaction sent: {:?}", self.address, tx_hash);
                                    break;
                                }
                                Err(e) => {
                                    log::error!("[{:?}] Failed to send challenge transaction: {:?}", self.address, e);
                                }
                            }
                        }
                        _ => {
                            log::error!("[{:?}] Challenge failed, total attempts {:?}", self.address, challenge_attempts);
                            break;
                        }
                    }
                    challenge_attempts += 1;
                }
            }
        }
        Ok(())
    }
}
