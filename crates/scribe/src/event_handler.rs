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
use eyre::Result;
use tokio::sync::mpsc::{ Receiver, Sender };
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::contract::Event;
use crate::contract::ScribeOptimistic::OpPoked;
use crate::contract::{
    EventWithMetadata,
    ScribeOptimisticProvider,
    ScribeOptimisticProviderInstance,
};
use crate::events_listener::RetryProviderWithSigner;

const CHALLENGE_POKE_DELAY_MS: u64 = 200;
const FLASHBOT_CHALLENGE_RETRY_COUNT: u64 = 3;
const CLASSIC_CHALLENGE_RETRY_COUNT: u64 = 3;

// Take event logs and distribute them to the appropriate contract handler
// Each scribe address has its own contract handler instance
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
        rx: Receiver<EventWithMetadata>
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
            self.txs.insert(address.clone(), tx);

            let mut contract_handler = EventHandler::new(
                address.clone(),
                self.provider.clone(),
                self.flashbot_provider.clone(),
                self.cancel.clone(),
                rx
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
                    match event {
                        Some(event) => {
                            event.address;
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
                        _ => {}
                    }
                }
            }
        }
        // Wait for all tasks to finish
        contract_handler_set.join_all().await;
        Ok(())
    }
}

// Receive events for a specific scribe address and create or cancel challenge processes
struct EventHandler {
    scribe_address: Address,
    provider: Arc<RetryProviderWithSigner>,
    flashbot_provider: Arc<RetryProviderWithSigner>,
    cancel: CancellationToken,
    rx: Receiver<EventWithMetadata>,
    cancel_challenge: Option<CancellationToken>,
    challenge_period: Option<u64>,
}

impl EventHandler {
    pub fn new(
        scribe_address: Address,
        provider: Arc<RetryProviderWithSigner>,
        flashbot_provider: Arc<RetryProviderWithSigner>,
        cancel: CancellationToken,
        rx: Receiver<EventWithMetadata>
    ) -> Self {
        Self {
            scribe_address,
            provider,
            flashbot_provider,
            cancel,
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
                _ = self.cancel.cancelled() => {
                    log::info!(
                        "[{:?}] Cancellation requested, stopping contract handler",
                        self.scribe_address
                    );
                    break;
                }
                event = self.rx.recv() => {
                    log::debug!("[{:?}] Received event: {:?}", self.scribe_address, event);
                    match event {
                        Some(event) => {
                            match event.event {
                                Event::OpPoked(op_poked) => {
                                    log::debug!("[{:?}] OpPoked received", self.scribe_address);
                                    // Check if the poke is within the challenge period
                                    let event_timestamp = match event.log.block_timestamp {
                                        Some(timestamp) => timestamp,
                                        None => {
                                            let block = self.provider.get_block(
                                                event.log.block_number.unwrap().into(),
                                                BlockTransactionsKind::Hashes
                                            ).await?;
                                            let block = match block {
                                                Some(block) => block,
                                                None => {
                                                    log::error!("Block not found");
                                                    continue;
                                                }
                                            };
                                            let timestamp = block.header.timestamp;
                                            timestamp
                                        }
                                    };
                                    let current_timestamp = chrono::Utc::now().timestamp() as u64;
                                    log::debug!("[{:?}] OpPoked, event_timestamp: {:?}, current_timestamp: {:?}",
                                        self.scribe_address, event_timestamp, current_timestamp);
                                    if current_timestamp - event_timestamp >
                                        self.challenge_period.unwrap() {
                                            log::debug!(
                                                "[{:?}] OpPoked received outside of challenge period",
                                                self.scribe_address
                                            );
                                            continue;
                                    }
                                    log::debug!("Spawning challenge...");
                                    self.spawn_challenge(op_poked).await;
                                }
                                Event::OpPokeChallengedSuccessfully(_) => {
                                    self.cancel_challenge().await;
                                }
                            }
                        }
                        _ => {
                            // if event is None
                        }
                    }
                }
            }
        }
        todo!()
    }

    async fn fetch_challenge_period(&mut self) -> Result<()> {
        // TODO: Add expiration time for the challenge period
        if self.challenge_period.is_some() {
            return Ok(());
        }

        let period = ScribeOptimisticProviderInstance::new(
            self.scribe_address,
            self.provider.clone()
        ).get_challenge_period().await?;

        self.challenge_period = Some(period as u64);
        Ok(())
    }

    async fn spawn_challenge(&mut self, op_poked: OpPoked) {
        // Ensure there is no existing challenge
        self.cancel_challenge().await;
        // Create a new cancellation token
        self.cancel_challenge = Some(CancellationToken::new());
        // Create a new challenger instance
        let challenge_handler = Some(
            Arc::new(
                Mutex::new(
                    ChallengeHandler::new(
                        op_poked,
                        self.cancel.clone(),
                        // cancel_challenge garunteed to be Some
                        self.cancel_challenge.as_ref().unwrap().clone(),
                        self.scribe_address,
                        self.provider.clone(),
                        self.flashbot_provider.clone()
                    )
                )
            )
        );
        // TODO: Validate challenge period first...

        // Spawn the asynchronous task
        tokio::spawn(async move {
            let handler = challenge_handler.as_ref().unwrap().lock().await;
            let _ = handler.start().await;
        });
        log::debug!("Spawned New challenger");
    }

    async fn cancel_challenge(&mut self) {
        match &self.cancel_challenge {
            Some(cancel) => {
                cancel.cancel();
                self.cancel_challenge = None;
            }
            _ => {}
        }
    }
}

// Handle the challenge process for a specific OpPoked event after a delay
// If cancelled before end of delay or inbewteen retries stop process
// First try challenge with flashbot provider, then with normal provider
struct ChallengeHandler {
    pub op_poked: OpPoked,
    pub global_cancel: CancellationToken,
    pub cancel: CancellationToken,
    address: Address,
    // TODO replace with ScribeOptimisticProviderInstances
    provider: Arc<RetryProviderWithSigner>,
    flashbot_provider: Arc<RetryProviderWithSigner>,
}

impl ChallengeHandler {
    pub fn new(
        op_poked: OpPoked,
        global_cancel: CancellationToken,
        cancel: CancellationToken,
        address: Address,
        provider: Arc<RetryProviderWithSigner>,
        flashbot_provider: Arc<RetryProviderWithSigner>
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
        log::debug!("Challenge started");
        // Perform the challenge after 200ms

        // TODO: better way to retry !!!!!!!!!!!!!!
        let mut challenge_attempts: u64 = 0;
        loop {
            tokio::select! {
                // Check if the challenge has been cancelled
                _ = self.cancel.cancelled() => {
                    log::debug!("Challenge cancelled");
                    break;
                }
                // Check if the global cancel command sent
                _ = self.global_cancel.cancelled() => {
                    log::debug!("Global cancel command received");
                    break;
                }
                _ = tokio::time::sleep(Duration::from_millis(CHALLENGE_POKE_DELAY_MS)) => {
                    // Verify that the OpPoked is valid
                    let is_valid = ScribeOptimisticProviderInstance::new(
                        self.address,
                        self.provider.clone()
                    ).is_schnorr_signature_valid(self.op_poked.clone()).await?;
                    if is_valid {
                        log::debug!("OpPoked is valid, no need to challenge");
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
                            log::debug!("Challenge result: {:?}", result);
                            match result {
                                Ok(tx_hash) => {
                                    log::debug!("Flashbot transaction sent: {:?}", tx_hash);
                                    break;
                                }
                                Err(e) => {
                                    log::error!("Failed to send flashbot transaction: {:?}", e);
                                }
                            }
                        }
                        FLASHBOT_CHALLENGE_RETRY_COUNT..RETRY_RANGE_END => {
                            let contract = ScribeOptimisticProviderInstance::new(
                                self.address, self.provider.clone()
                            );
                            let result = contract.challenge(self.op_poked.schnorrData.clone()).await;
                            log::debug!("Challenge result: {:?}", result);
                            match result {
                                Ok(tx_hash) => {
                                    log::debug!("Challenge transaction sent: {:?}", tx_hash);
                                    break;
                                }
                                Err(e) => {
                                    log::error!("Failed to send challenge transaction: {:?}", e);
                                }
                            }
                        }
                        _ => {
                            log::debug!("Challenge failed");
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
