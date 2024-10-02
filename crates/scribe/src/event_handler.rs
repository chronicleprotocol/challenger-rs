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

use alloy::network::TransactionBuilder;
use alloy::primitives::Address;
use alloy::providers::ProviderBuilder;
use eyre::Result;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::contract::{Event, ScribeOptimistic};
use crate::contract::IScribe::SchnorrData;
use crate::contract::{
    EventWithMetadata, ScribeOptimisticProvider, ScribeOptimisticProviderInstance,
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
            self.txs.insert(address.clone(), tx);

            let mut contract_handler = ContractEventHandler::new(
                address.clone(),
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
                    match event {
                        Some(event) => {
                            event.address;
                            // Send the event to the appropriate contract handler
                            match self.txs.get(&event.address) {
                                Some(tx) => {
                                    let _ = tx.send(event).await;
                                }
                                _ => {}
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

struct ContractEventHandler
{
    scribe_address: Address,
    provider: Arc<RetryProviderWithSigner>,
    flashbot_provider: Arc<RetryProviderWithSigner>,
    cancel: CancellationToken,
    rx: Receiver<EventWithMetadata>,
    cancel_challenge: Option<CancellationToken>,
}

impl ContractEventHandler {
    pub fn new(
        scribe_address: Address,
        provider: Arc<RetryProviderWithSigner>,
        flashbot_provider: Arc<RetryProviderWithSigner>,
        cancel: CancellationToken,
        rx: Receiver<EventWithMetadata>,
    ) -> Self {
        Self {
            scribe_address,
            provider,
            flashbot_provider,
            cancel,
            rx,
            cancel_challenge: None,
        }
    }

    pub async fn start(&mut self) -> Result<()> {
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    log::info!("[{:?}] Cancellation requested, stopping contract handler", self.scribe_address);
                    break;
                }
                event = self.rx.recv() => {
                    log::debug!("[{:?}] Received event: {:?}", self.scribe_address, event);

                    match event {
                        Some(event) => {
                            match event.event {
                                Event::OpPoked(op_poked) => {
                                    log::debug!("[{:?}] OpPoked received", self.scribe_address);

                                    // Cancel challenge if any
                                    self.cancel_challenge().await;
                                    log::debug!("Challenge cancelled existing");

                                    // Check if the schnorr signature is valid
                                    let is_valid = ScribeOptimisticProviderInstance::new(self.scribe_address, self.flashbot_provider.clone()).is_schnorr_signature_valid(op_poked.clone()).await?;
                                    // let is_valid = self.contract.is_schnorr_signature_valid(op_poked.clone()).await?;
                                    if !is_valid {
                                        // Create a new challenge handler instance
                                        self.cancel_challenge = Some(CancellationToken::new());
                                        let challenge_handler = Some(Arc::new(Mutex::new(ChallengeHandler::new(
                                            op_poked.schnorrData,
                                            self.cancel.clone(),
                                            self.cancel_challenge.as_ref().unwrap().clone(),
                                            self.scribe_address,
                                            self.provider.clone(),
                                            self.flashbot_provider.clone(),
                                        ))));

                                        // Spawn the asynchronous task
                                        tokio::spawn(async move {
                                            let handler = challenge_handler.as_ref().unwrap().lock().await;
                                            let _ = handler.start().await;
                                        });
                                        log::debug!("Spawned New challneger");
                                    }
                                }
                                Event::OpPokeChallengedSuccessfully(_) => {
                                    self.cancel_challenge().await;
                                }
                                _ => {
                                    // if event is None
                                }
                            }
                        }
                        _ => {
                            // if event is None
                        }
                    }

                    // TODO: Handle the event
                    // if opPoked
                    // start new process (process need to wait 200ms before start OpPoked validation) (need to wait beacuse next event can be ChallengeSuccessful and we wouldn't need to do anything)
                    // if next received event is ChallengeSuccessful ->  terminate just started OpPoked process
                    // if no ChallengeSuccessful received in 200ms -> validate OpPoked
                    // if OpPoked is valid -> do nothing, all ok
                    // if OpPoked is invalid -> need to challenge it
                    //
                    // Challenge process have to be:
                    // 1. Send private challenge transaction
                    // 2. Wait for ChallengeSuccessful event for next 3-4 blocks
                    // 3. If ChallengeSuccessful received -> all ok, do nothing
                    // 4. If no ChallengeSuccessful received -> send public challenge transaction
                }
            }
        }
        todo!()
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

// Challenge a single OpPoked event after a delay
// If the challenge is not cancelled within the delay time period, the challenge is performed
// <C>
// where
// C: ScribeOptimisticProvider,
struct ChallengeHandler {
    pub schnorr_data: SchnorrData,
    pub global_cancel: CancellationToken,
    pub cancel: CancellationToken,
    address: Address,
    provider: Arc<RetryProviderWithSigner>,
    flashbot_provider: Arc<RetryProviderWithSigner>,
}

impl ChallengeHandler {
    pub fn new(
        schnorr_data: SchnorrData,
        global_cancel: CancellationToken,
        cancel: CancellationToken,
        address: Address,
        provider: Arc<RetryProviderWithSigner>,
        flashbot_provider: Arc<RetryProviderWithSigner>,
    ) -> Self {
        Self {
            schnorr_data,
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
                    break;
                }
                _ = tokio::time::sleep(Duration::from_millis(CHALLENGE_POKE_DELAY_MS)) => {
                    const RETRY_RANGE_END: u64 = CLASSIC_CHALLENGE_RETRY_COUNT+FLASHBOT_CHALLENGE_RETRY_COUNT;
                    match challenge_attempts {
                        0..FLASHBOT_CHALLENGE_RETRY_COUNT => {
                            let contract = ScribeOptimisticProviderInstance::new(self.address, self.flashbot_provider.clone());
                            let result = contract.challenge(self.schnorr_data.clone()).await;
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
                            let contract = ScribeOptimisticProviderInstance::new(self.address, self.provider.clone());
                            let result = contract.challenge(self.schnorr_data.clone()).await;
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


                    // // TODO count the number of challenges using flashbot
                    // // TODO count the number of public challenges
                    // // TODO print error if no callenge successful event is received
                    // log::debug!("Performing challenge");
                    // // perform challenge
                    // let result = ScribeOptimisticProviderInstance::new(self.address, self.provider.clone())
                    //     .challenge(self.schnorr_data.clone())
                    //     .await;
                    // // TODO maybe move the received eth to a different address
                    // log::debug!("Challenge result: {:?}", result);
                    // break;
                }
            }
        }
        Ok(())
    }
}
