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

use alloy::{primitives::Address, rpc::types::Log};
use std::time::Duration;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;

use crate::{
  contract::{ScribeContract, ScribeOptimistic::OpPoked},
  error::ProcessorResult,
  event::Event,
  metrics,
};

const CHALLENGE_POKE_DELAY_MS: u64 = 200;

// Receives preparsed [crate::contract::EventWithMetadata] events for a `ScribeOptimistic` instance on address,
// validates `OpPoked` events and challenges them if they are invalid and within the challenge period.
//
// `OpPoked` validation and challenge logic is launched in a separate task and is cancellable.
// When new `OpPoked` event is received, it's challenge process is started with a delay of `CHALLENGE_POKE_DELAY_MS`.
// If next received event will be `OpPokeChallengedSuccessfully`, the challenge process is cancelled (no need to spend resources on validation).
// Otherwise, the challenge process will start procecssing.
pub struct ScribeEventsProcessor<C: ScribeContract> {
  address: Address,
  cancel_challenge: Option<CancellationToken>,
  cancellation_token: CancellationToken,
  challenge_period: Option<u64>,
  scribe_contract: C,
  rx: Receiver<Event>,
}

impl<C: ScribeContract> ScribeEventsProcessor<C> {
  /// Creates a new `ScribeEventsProcessor` instance and returns it along with a sender channel to send events to it.
  pub fn new(
    address: Address,
    scribe_contract: C,
    cancellation_token: CancellationToken,
  ) -> (Self, Sender<Event>) {
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(100);

    (
      Self {
        address,
        cancellation_token,
        rx,
        scribe_contract,
        cancel_challenge: None,
        challenge_period: None,
      },
      tx,
    )
  }

  // Pulls the challenge period from the contract and stores it in the struct.
  async fn refresh_challenge_period(&mut self) -> ProcessorResult<()> {
    let period = self.scribe_contract.get_challenge_period().await?;

    log::debug!(
      "ScribeEventsProcessor[{:?}] Challenge period fetched: {:?}",
      self.address,
      period
    );

    self.challenge_period = Some(period as u64);
    Ok(())
  }

  /// Starts the handling events.
  pub async fn start(&mut self) {
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
            return;
        }
        // new [Event] received, process it...
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

  async fn process_event(&mut self, event: Event) -> ProcessorResult<()> {
    match event {
      Event::OpPoked(log) => {
        // For `OpPoked` events, check if `schnorr_signature` is valid,
        // if not - check if event is within the challenge period, send challenge.
        // If `schnorr_signature` is valid, do nothing.
        log::trace!(
          "ScribeEventsProcessor[{:?}] OpPoked received, start processing",
          self.address
        );

        let op_poke_challengeable = self
          .scribe_contract
          .is_op_poke_challangeble(&log, self.challenge_period.unwrap())
          .await?;

        log::debug!(
          "ScribeEventsProcessor[{:?}] OpPoked event {:?} challengeable ?: {:?}",
          self.address,
          log.transaction_hash,
          op_poke_challengeable
        );

        if op_poke_challengeable {
          self.spawn_challenge(&log);
        }
      }

      // If the challenge is already successful, cancel the previous challenge process
      Event::OpPokeChallengedSuccessfully { .. } => {
        log::trace!(
          "ScribeEventsProcessor[{:?}] OpPokeChallengedSuccessfully received, cancelling challenge",
          self.address
        );

        self.cancel_challenge();
      }
    }

    Ok(())
  }

  //
  fn spawn_challenge(&mut self, log: &Log<OpPoked>) {
    // Ensure there is no existing challenge existing
    self.cancel_challenge();
    // Create a new child cancellation token, so it will be cancelled when the main process is cancelled
    let child_cancellation_token = self.cancellation_token.child_token();
    self.cancel_challenge = Some(child_cancellation_token.clone());

    let schnorr_data = log.data().schnorrData.clone();
    let contract = self.scribe_contract.clone();

    // Spawn the asynchronous task
    tokio::spawn(async move {
      tokio::select! {
        _ = child_cancellation_token.cancelled() => {
          log::debug!(
            "ScribeEventsProcessor[{:?}] Challenge process cancelled",
            &contract.address()
          );
        }

        _ = tokio::time::sleep(Duration::from_millis(CHALLENGE_POKE_DELAY_MS)) => {
          log::debug!(
            "ScribeEventsProcessor[{:?}] Trying to challenge OpPoked event",
            &contract.address()
          );

          match contract.challenge(schnorr_data).await {
            Ok(tx_hash) => {
              // Increment the challenge counter
              metrics::inc_challenge_counter(*contract.address());

              log::debug!(
                "ScribeEventsProcessor[{:?}] OpPoked event challenged successfully, tx_hash: {:?}",
                &contract.address(),
                tx_hash
              );
            }
            Err(e) => {
              log::error!(
                "ScribeEventsProcessor[{:?}] Failed to challenge OpPoked event: {:?}",
                &contract.address(),
                e
              );
            }
          };
        }
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
