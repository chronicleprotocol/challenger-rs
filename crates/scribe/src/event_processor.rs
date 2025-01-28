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

use alloy::{
  primitives::{Address, FixedBytes},
  providers::Provider,
  rpc::types::{BlockTransactionsKind, Log},
};
use eyre::{bail, Context, Result};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;

use crate::{
  contract::{ScribeContract, ScribeContractInstance, ScribeOptimistic::OpPoked},
  event::Event,
  metrics, RetryProviderWithSigner,
};

const GAS_LIMIT: u64 = 200000;
const CHALLENGE_POKE_DELAY_MS: u64 = 200;
const FLASHBOT_CHALLENGE_RETRY_COUNT: u64 = 3;
const CLASSIC_CHALLENGE_RETRY_COUNT: u64 = 3;

// Receives preparsed [crate::contract::EventWithMetadata] events for a `ScribeOptimistic` instance on address,
// validates `OpPoked` events and challenges them if they are invalid and within the challenge period.
//
// `OpPoked` validation and challenge logic is launched in a separate task and is cancellable.
// When new `OpPoked` event is received, it's challenge process is started with a delay of `CHALLENGE_POKE_DELAY_MS`.
// If next received event will be `OpPokeChallengedSuccessfully`, the challenge process is cancelled (no need to spend resources on validation).
// Otherwise, the challenge process will start procecssing.
pub struct ScribeEventsProcessor {
  address: Address,
  cancel_challenge: Option<CancellationToken>,
  cancellation_token: CancellationToken,
  challenge_period: Option<u64>,
  flashbot_provider: Arc<RetryProviderWithSigner>,
  provider: Arc<RetryProviderWithSigner>,
  rx: Receiver<Event>,
}

impl ScribeEventsProcessor {
  /// Creates a new `ScribeEventsProcessor` instance and returns it along with a sender channel to send events to it.
  pub fn new(
    address: Address,
    provider: Arc<RetryProviderWithSigner>,
    flashbot_provider: Arc<RetryProviderWithSigner>,
    cancellation_token: CancellationToken,
  ) -> (Self, Sender<Event>) {
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(100);

    (
      Self {
        address,
        provider,
        flashbot_provider,
        cancellation_token,
        rx,
        cancel_challenge: None,
        challenge_period: None,
      },
      tx,
    )
  }

  // Pulls the challenge period from the contract and stores it in the struct.
  async fn refresh_challenge_period(&mut self) -> Result<()> {
    let period = ScribeContractInstance::new(self.address, self.provider.clone())
      .get_challenge_period()
      .await?;

    self.challenge_period = Some(period as u64);
    Ok(())
  }

  pub async fn start(&mut self) -> Result<()> {
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
            match event {
              Some(event) => {
                if let Err(err) = self.process_event(event).await {
                  log::error!(
                      "ScribeEventsProcessor[{:?}] Error processing event: {:?}",
                      self.address,
                      err
                  );
                }
              },
              None => {
                log::warn!(
                    "ScribeEventsProcessor[{:?}] Received None event, stopping contract handler",
                    self.address
                );
              }
            }
        }
      }
    }
  }

  async fn process_event(&mut self, event: Event) -> Result<()> {
    match event {
      Event::OpPoked(log) => {
        // For `OpPoked` events, check if `schnorr_signature` is valid,
        // if not - check if event is within the challenge period, send challenge.
        // If `schnorr_signature` is valid, do nothing.
        log::trace!(
          "ScribeEventsProcessor[{:?}] OpPoked received, start processing",
          self.address
        );

        if self.is_log_stale(&log).await? {
          // This log is expected in tests, tests must be updated if log message is changed
          log::debug!(
            "ScribeEventsProcessor[{:?}] OpPoked received outside of challenge period",
            self.address
          );

          return Ok(());
        }

        log::trace!(
          "ScribeEventsProcessor[{:?}] Spawning validation and challenge process...",
          self.address
        );
        self.spawn_challenge(&log).await;
      }

      // If the challenge is already successful, cancel the previous challenge process
      Event::OpPokeChallengedSuccessfully { .. } => {
        log::debug!(
                "ScribeEventsProcessor[{:?}] OpPokeChallengedSuccessfully received, cancelling challenge process",
                self.address
            );
        self.cancel_challenge();
      }
    }

    Ok(())
  }

  // Checks if the log is stale, i.e. if the event is outside of the challenge period.
  // If the block timestamp is missing, it is fetched from the block number.
  // If the block number is also missing, an error is returned.
  async fn is_log_stale(&self, log: &Log<OpPoked>) -> Result<bool> {
    // Check if the poke is within the challenge period
    let event_timestamp = match log.block_timestamp {
      Some(timestamp) => timestamp,
      None => {
        if log.block_number.is_none() {
          bail!("Block number and timestamp are missing in the log");
        }

        self
          .get_timestamp_from_block(log.block_number.unwrap())
          .await
          .wrap_err("Failed to get timestamp from block via RPC call")?
      }
    };

    let current_timestamp = chrono::Utc::now().timestamp() as u64;
    log::debug!(
      "ScribeEventsProcessor[{:?}] OpPoked, event_timestamp: {:?}, current_timestamp: {:?}",
      self.address,
      event_timestamp,
      current_timestamp
    );

    Ok(current_timestamp - event_timestamp > self.challenge_period.unwrap())
  }

  // Gets the timestamp from the block by `event.log.block_number`, if it is missing, returns an error
  async fn get_timestamp_from_block(&self, block_number: u64) -> Result<u64> {
    let Some(block) = self
      .provider
      .get_block(block_number.into(), BlockTransactionsKind::Hashes)
      .await?
    else {
      bail!("Block with number {:?} not found", block_number);
    };

    Ok(block.header.timestamp)
  }

  // Spawn a new challenge process for the given `OpPoked` event.
  //
  async fn spawn_challenge(&mut self, log: &Log<OpPoked>) {
    // Ensure there is no existing challenge existing
    self.cancel_challenge();
    // Create a new child cancellation token, so it will be cancelled when the main process is cancelled
    let child_cancellation_token = self.cancellation_token.child_token();
    self.cancel_challenge = Some(child_cancellation_token.clone());
    // Create a new challenger instance
    let challenge_handler = OpPokedChallengerProcess::new(
      log.data().clone(),
      child_cancellation_token,
      self.address,
      self.provider.clone(),
      self.flashbot_provider.clone(),
    );

    // Spawn the asynchronous task
    tokio::spawn(async move {
      if let Err(e) = challenge_handler.start().await {
        log::error!(
          "ScribeEventsProcessor[{:?}] Error in challenge process: {:?}",
          challenge_handler.address,
          e
        );
      }
    });

    log::debug!(
      "ScribeEventsProcessor[{:?}] Spawned New challenger process",
      self.address
    );
  }

  fn cancel_challenge(&mut self) {
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
struct OpPokedChallengerProcess {
  address: Address,
  cancellation_token: CancellationToken,
  flashbot_provider: Arc<RetryProviderWithSigner>,
  op_poked_event: OpPoked,
  provider: Arc<RetryProviderWithSigner>,
}

impl OpPokedChallengerProcess {
  pub fn new(
    op_poked_event: OpPoked,
    cancellation_token: CancellationToken,
    address: Address,
    provider: Arc<RetryProviderWithSigner>,
    flashbot_provider: Arc<RetryProviderWithSigner>,
  ) -> Self {
    Self {
      op_poked_event,
      cancellation_token,
      address,
      provider,
      flashbot_provider,
    }
  }

  // TODO: refactor
  pub async fn start(&self) -> Result<()> {
    // This checked for in tests, tests must be updated if log is changed
    log::debug!(
      "OpPokedValidator[{:?}] OpPoked validation started",
      self.address
    );

    tokio::select! {
        // Check if the challenge has been cancelled
        _ = self.cancellation_token.cancelled() => {
          log::debug!("OpPokedValidator[{:?}] Challenge cancelled", self.address);
          Ok(())
        }
        _ = tokio::time::sleep(Duration::from_millis(CHALLENGE_POKE_DELAY_MS)) => {
          // TODO: move to upper level, all validation have to be done in one place ?
          // Verify that the OpPoked is valid
          let is_valid = ScribeContractInstance::new(
              self.address,
              self.provider.clone()
          ).is_signature_valid(self.op_poked_event.clone()).await?;

          if is_valid {
              log::debug!("OpPokedValidator[{:?}] OpPoked is valid, no need to challenge", self.address);
              return Ok(());
          }

          // Perform the challenge process
          self.do_challenge().await?;
          Ok(())
        }
    }
  }

  // TODO: need refactoring.
  // Perform the challenge process, first with flashbot provider, then with normal provider.
  async fn do_challenge(&self) -> Result<FixedBytes<32>> {
    // Perform the challenge after 200ms
    let mut challenge_attempts: u64 = 0;
    const RETRY_RANGE_END: u64 = CLASSIC_CHALLENGE_RETRY_COUNT + FLASHBOT_CHALLENGE_RETRY_COUNT;

    log::info!(
      "OpPokedValidator[{:?}] Challending data: {:?}",
      self.address,
      self.op_poked_event
    );

    loop {
      match challenge_attempts {
        0..FLASHBOT_CHALLENGE_RETRY_COUNT => {
          log::debug!(
            "OpPokedValidator[{:?}] Attempting flashbot challenge",
            self.address
          );
          let contract = ScribeContractInstance::new(self.address, self.flashbot_provider.clone());

          let result = contract
            .challenge(self.op_poked_event.schnorrData.clone(), GAS_LIMIT)
            .await;

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
          let contract = ScribeContractInstance::new(self.address, self.provider.clone());
          match contract
            .challenge(self.op_poked_event.schnorrData.clone(), GAS_LIMIT)
            .await
          {
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

    bail!(
      "OpPokedValidator[{:?}] Challenge failed, total attempts {:?}",
      self.address,
      challenge_attempts
    )
  }
}
