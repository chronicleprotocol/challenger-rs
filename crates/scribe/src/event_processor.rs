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

use alloy::rpc::types::Log;
use std::time::Duration;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;

use crate::{
  contract::{ScribeContract, ScribeOptimistic::OpPoked},
  error::{ProcessorError, ProcessorResult},
  event::Event,
  metrics,
};

// delay challenge process waits before making a challenge,
// delay is set to give ability to cancel challenge if next event is `OpPokeChallengedSuccessfully`.
const CHALLENGE_POKE_DELAY: Duration = Duration::from_millis(200);

// Receives preparsed [crate::contract::EventWithMetadata] events for a `ScribeOptimistic` instance on address,
// validates `OpPoked` events and challenges them if they are invalid and within the challenge period.
//
// `OpPoked` validation and challenge logic is launched in a separate task and is cancellable.
// When new `OpPoked` event is received, it's challenge process is started with a delay of `CHALLENGE_POKE_DELAY_MS`.
// If next received event will be `OpPokeChallengedSuccessfully`, the challenge process is cancelled (no need to spend resources on validation).
// Otherwise, the challenge process will start procecssing.
pub struct ScribeEventsProcessor<C: ScribeContract> {
  challenge_process_cancelation_token: Option<CancellationToken>,
  cancellation_token: CancellationToken,
  challenge_period: Option<u64>,
  challenge_poke_delay: Duration,
  scribe_contract: C,
  rx: Receiver<Event>,
}

impl<C: ScribeContract + Clone + 'static> ScribeEventsProcessor<C> {
  /// Creates a new `ScribeEventsProcessor` instance and returns it along with a sender channel to send events to it.
  pub fn new(
    scribe_contract: C,
    cancellation_token: CancellationToken,
    challenge_poke_delay: Option<Duration>,
  ) -> (Self, Sender<Event>) {
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(100);

    (
      Self {
        cancellation_token,
        rx,
        scribe_contract,
        challenge_process_cancelation_token: None,
        challenge_period: None,
        challenge_poke_delay: challenge_poke_delay.unwrap_or(CHALLENGE_POKE_DELAY),
      },
      tx,
    )
  }

  // Pulls the challenge period from the contract and stores it in the struct.
  async fn refresh_challenge_period(&mut self) -> ProcessorResult<()> {
    let period = self.scribe_contract.get_challenge_period().await?;

    log::debug!(
      "ScribeEventsProcessor[{:?}] Challenge period fetched: {:?}",
      self.scribe_contract.address(),
      period
    );

    self.challenge_period = Some(period as u64);
    Ok(())
  }

  // Handles an event and processes it.
  async fn handle_event(&mut self, event: Event) -> ProcessorResult<()> {
    match event {
      Event::OpPoked(log) => {
        // For `OpPoked` events, check if `schnorr_signature` is valid,
        // if not - check if event is within the challenge period, send challenge.
        // If `schnorr_signature` is valid, do nothing.
        log::trace!(
          "ScribeEventsProcessor[{:?}] OpPoked received, start processing",
          self.scribe_contract.address()
        );

        let challenge_period =
          self
            .challenge_period
            .ok_or_else(|| ProcessorError::ChallengePeriodNotInitialized {
              address: *self.scribe_contract.address(),
            })?;

        let op_poke_challengeable = self
          .scribe_contract
          .is_op_poke_challengeable(&log, challenge_period)
          .await?;

        log::debug!(
          "ScribeEventsProcessor[{:?}] OpPoked event {:?} challengeable ?: {:?}",
          self.scribe_contract.address(),
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
          self.scribe_contract.address()
        );

        self.cancel_challenge();
      }
    }

    Ok(())
  }

  // spawns challenge process for the `OpPoked` event in separate thread.
  fn spawn_challenge(&mut self, log: &Log<OpPoked>) {
    // Ensure there is no existing challenge process existing
    self.cancel_challenge();
    // Create a new child cancellation token, so it will be cancelled when the main process is cancelled
    let child_cancellation_token = self.cancellation_token.child_token();
    self.challenge_process_cancelation_token = Some(child_cancellation_token.clone());

    let schnorr_data = log.data().schnorrData.clone();
    let contract = self.scribe_contract.clone();
    let challenge_poke_delay = self.challenge_poke_delay;

    // Spawn the asynchronous task
    tokio::spawn(async move {
      tokio::select! {
        _ = child_cancellation_token.cancelled() => {
          log::debug!(
            "ScribeEventsProcessor[{:?}] Challenge process cancelled",
            &contract.address()
          );
        }

        _ = tokio::time::sleep(challenge_poke_delay) => {
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
      self.scribe_contract.address()
    );
  }

  fn cancel_challenge(&mut self) {
    if let Some(cancel) = &self.challenge_process_cancelation_token {
      log::debug!(
        "ScribeEventsProcessor[{:?}] Cancelling existing challenge",
        self.scribe_contract.address()
      );
      cancel.cancel();
      self.challenge_process_cancelation_token = None;
    }
  }

  /// Starts the handling events.
  pub async fn start(&mut self) -> ProcessorResult<()> {
    log::debug!(
      "ScribeEventsProcessor[{:?}] Starting new contract handler",
      self.scribe_contract.address()
    );
    // We have to fail if no challenge period is fetched on start
    self.refresh_challenge_period().await?;

    loop {
      tokio::select! {
        // main process terminates, need to finish work and exit...
        _ = self.cancellation_token.cancelled() => {
            log::info!(
                "ScribeEventsProcessor[{:?}] Cancellation requested, stopping contract handler",
                self.scribe_contract.address()
            );
            return Ok(());
        }
        // new [Event] received, process it...
        event = self.rx.recv() => {
            match event {
              Some(event) => {
                if let Err(err) = self.handle_event(event).await {
                  log::error!(
                      "ScribeEventsProcessor[{:?}] Error processing event: {:?}",
                      self.scribe_contract.address(),
                      err
                  );
                }
              },
              None => {
                log::warn!(
                    "ScribeEventsProcessor[{:?}] Received None event, stopping contract handler",
                    self.scribe_contract.address()
                );
              }
            }
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    contract::{IScribe::SchnorrData, ScribeOptimistic},
    error::{ContractError, ContractResult},
  };
  use alloy::primitives::{Address, TxHash};
  use mockall::{mock, Sequence};

  // event on block number 7609469 and address 0x891e368fe81cba2ac6f6cc4b98e684c106e2ef4f
  static LOG: &str = r#"{
				"address": "0x891e368fe81cba2ac6f6cc4b98e684c106e2ef4f",
				"topics": [
					"0xb9dc937c5e394d0c8f76e0e324500b88251b4c909ddc56232df10e2ea42b3c63",
					"0x0000000000000000000000001f7acda376ef37ec371235a094113df9cb4efee1",
					"0x0000000000000000000000002b5ad5c4795c026514f8317c7a215e218dccd6cf"
				],
				"data": "0x0000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000004200000000000000000000000000000000000000000000000000000000679c9c5c5014fdeb8945691eced7992164c71f58912483580b6991637d3f37adf248e910000000000000000000000000e1fdc6d86826238f87bb29e5f7d2731bb2a83641000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000022b68000000000000000000000000000000000000000000000000000000000000",
				"blockNumber": "0x741c7d",
				"transactionHash": "0x53b5497d11e7682526cd996f33d6f20c81cfe3d24d0f459bfe07b6b3661d8a47",
				"transactionIndex": "0x96",
				"blockHash": "0xbc84d639931977426ca4448cec695558905ff5dc49aa82de39cad3c7033da5ce",
				"logIndex": "0x1ac",
				"removed": false
			}"#;

  mock! {
    // pub trait ScribeContract: Clone + Send + Sync + 'static {
    Scribe {}
    impl Clone for Scribe {
      fn clone(&self) -> Self;
    }

    impl ScribeContract for Scribe {
      /// Returns the address of the contract.
      fn address(&self) -> &Address;

      /// Returns challenge period from ScribeOptimistic smart contract deployed to `address`.
      /// NOTE: From time to time challenger might need to refresh this value, because it might be changed by the contract owner.
      async fn get_challenge_period(&self) -> ContractResult<u16>;

      /// Returns true if given `OpPoked` event is challengeable.
      /// It checks if the event is stale and if the signature is valid.
      async fn is_op_poke_challengeable(
        &self,
        op_poked: &Log<ScribeOptimistic::OpPoked>,
        challenge_period: u64,
      ) -> ContractResult<bool>;

      /// Challenges given `OpPoked` event with given `schnorr_data`.
      /// See: `IScribeOptimistic::opChallenge(SchnorrData calldata schnorrData)` for more details.
      async fn challenge(
        &self,
        schnorr_data: SchnorrData,
      ) -> ContractResult<TxHash>;
    }
  }

  #[tokio::test]
  async fn test_refresh_challenge_period() {
    let mut scribe = MockScribe::new();
    scribe.expect_get_challenge_period().returning(|| Ok(10));

    let (mut processor, _) = ScribeEventsProcessor::new(scribe, CancellationToken::new(), None);

    assert!(processor.challenge_period.is_none());
    processor.refresh_challenge_period().await.unwrap();
    assert_eq!(processor.challenge_period.unwrap(), 10);
  }

  #[tokio::test]
  async fn test_cancel_challenge() {
    let cancel = CancellationToken::new();

    let (mut processor, _) =
      ScribeEventsProcessor::new(MockScribe::new(), CancellationToken::new(), None);
    // manually set challenge process cancelation token
    processor.challenge_process_cancelation_token = Some(cancel.clone());
    processor.cancel_challenge();

    // cancels challenge process and clears cancelation token
    assert!(cancel.is_cancelled());
    assert!(processor.challenge_process_cancelation_token.is_none());
  }

  #[tokio::test]
  async fn test_spawns_challenge_and_cancels() {
    let cancel = CancellationToken::new();
    let mut scribe = MockScribe::new();
    let mut scribe_clone = MockScribe::new();
    scribe_clone.expect_challenge().never();

    scribe.expect_clone().return_once(move || scribe_clone);

    let log: Log = serde_json::from_str(LOG).unwrap();
    let poke: Log<ScribeOptimistic::OpPoked> = log.log_decode().unwrap();

    let duration = Duration::from_millis(5);

    let (mut processor, _) = ScribeEventsProcessor::new(scribe, cancel.clone(), Some(duration));

    // challenge can be cancelled
    processor.spawn_challenge(&poke);
    assert!(processor.challenge_process_cancelation_token.is_some());
    cancel.cancel();

    // wait for challenge process to be cancelled
    tokio::time::sleep(duration * 2).await;

    assert!(processor
      .challenge_process_cancelation_token
      .unwrap()
      .is_cancelled());
  }

  #[tokio::test]
  async fn test_spawns_challenge_and_challenges() {
    let cancel = CancellationToken::new();
    let mut scribe = MockScribe::new();
    let mut scribe_clone = MockScribe::new();
    scribe_clone
      .expect_address()
      .return_const(Address::random().to_owned());

    scribe_clone
      .expect_challenge()
      .times(1)
      .returning(|_| Ok(TxHash::random()));

    scribe.expect_clone().return_once(move || scribe_clone);

    let log: Log = serde_json::from_str(LOG).unwrap();
    let poke: Log<ScribeOptimistic::OpPoked> = log.log_decode().unwrap();

    let duration = Duration::from_millis(5);

    let (mut processor, _) = ScribeEventsProcessor::new(scribe, cancel.clone(), Some(duration));

    // challenge can be cancelled
    processor.spawn_challenge(&poke);
    assert!(processor.challenge_process_cancelation_token.is_some());

    tokio::time::sleep(duration * 2).await;
    assert!(!processor
      .challenge_process_cancelation_token
      .unwrap()
      .is_cancelled());
  }

  #[tokio::test]
  async fn test_handle_event_challenge_cancels_handler() {
    let challenge = r#"
      {
				"address": "0x891e368fe81cba2ac6f6cc4b98e684c106e2ef4f",
				"topics": [
					"0xac50cef58b3aef7f7c30349f5e4a342a29d2325a02eafc8dacfdba391e6d5db3",
					"0x0000000000000000000000001f7acda376ef37ec371235a094113df9cb4efee1"
				],
				"data": "0x0000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000002456d7d2e80000000000000000000000003f17f1962b36e491b30a40b2405849e597ba5fb500000000000000000000000000000000000000000000000000000000",
				"blockNumber": "0x75823d",
				"transactionHash": "0xfc99cae27a9593e3b5910ef65dfeae601de2b2cfd678a1396c7878b489f34509",
				"transactionIndex": "0x8f",
				"blockHash": "0x1ea63aa0ae6603a49fccd44d1271a7112f34b41a8512e625844a560cde066a24",
				"logIndex": "0x116",
				"removed": false
			}
      "#;
    let log: Log = serde_json::from_str(challenge).unwrap();
    let event = Event::try_from(log).unwrap();

    let cancel = CancellationToken::new();
    let child_cancel = cancel.child_token();
    let (mut processor, _) = ScribeEventsProcessor::new(MockScribe::new(), cancel.clone(), None);
    processor.challenge_process_cancelation_token = Some(child_cancel.clone());
    assert!(!child_cancel.is_cancelled());

    processor.handle_event(event).await.unwrap();
    assert!(child_cancel.is_cancelled());
  }

  #[tokio::test]
  async fn test_handle_event_op_poked_non_challengeble() {
    let mut scribe = MockScribe::new();
    scribe.expect_get_challenge_period().returning(|| Ok(10));
    scribe
      .expect_is_op_poke_challengeable()
      .returning(|_, _| Ok(false));

    let log: Log = serde_json::from_str(LOG).unwrap();
    let event = Event::try_from(log).unwrap();

    let (mut processor, _) = ScribeEventsProcessor::new(scribe, CancellationToken::new(), None);
    processor.refresh_challenge_period().await.unwrap();
    // no challenge process is spawned
    assert!(processor.challenge_process_cancelation_token.is_none());

    processor.handle_event(event).await.unwrap();

    // still no process is spawned
    assert!(processor.challenge_process_cancelation_token.is_none());
  }

  #[tokio::test]
  async fn test_handle_event_op_poked_is_challengeble() {
    let mut scribe_clone = MockScribe::new();
    scribe_clone
      .expect_address()
      .return_const(Address::random().to_owned());
    scribe_clone
      .expect_challenge()
      .times(1)
      .returning(|_| Ok(TxHash::random()));

    let mut scribe = MockScribe::new();
    scribe.expect_get_challenge_period().returning(|| Ok(10));
    scribe
      .expect_is_op_poke_challengeable()
      .returning(|_, _| Ok(true));
    scribe.expect_clone().return_once(move || scribe_clone);

    let log: Log = serde_json::from_str(LOG).unwrap();
    let event = Event::try_from(log).unwrap();

    let duration = Duration::from_millis(5);

    let (mut processor, _) =
      ScribeEventsProcessor::new(scribe, CancellationToken::new(), Some(duration));

    processor.refresh_challenge_period().await.unwrap();
    // no challenge process is spawned
    assert!(processor.challenge_process_cancelation_token.is_none());

    processor.handle_event(event).await.unwrap();

    // still no process is spawned
    assert!(processor.challenge_process_cancelation_token.is_some());
    // wait for challenge process to be called
    tokio::time::sleep(duration * 2).await;
  }

  #[tokio::test]
  async fn test_start_cancels() {
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    let mut scribe = MockScribe::new();
    scribe.expect_get_challenge_period().returning(|| Ok(10));

    let (mut processor, _) = ScribeEventsProcessor::new(scribe, cancel_clone.clone(), None);

    // cancel the process
    tokio::spawn(async move {
      tokio::time::sleep(Duration::from_millis(5)).await;
      cancel.cancel();
    });

    assert!(processor.start().await.is_ok());
    assert!(cancel_clone.is_cancelled());
  }

  #[tokio::test]
  async fn test_start_processes() {
    let mut sequence = Sequence::new();
    let mut scribe_clone = MockScribe::new();
    scribe_clone
      .expect_address()
      .return_const(Address::random().to_owned());

    let mut scribe = MockScribe::new();
    scribe
      .expect_address()
      .return_const(Address::random().to_owned());

    // Have to be called in order
    scribe
      .expect_get_challenge_period()
      .times(1)
      .in_sequence(&mut sequence)
      .returning(|| Ok(10));
    scribe
      .expect_is_op_poke_challengeable()
      .times(1)
      .in_sequence(&mut sequence)
      .returning(|_, _| Ok(true));

    scribe_clone
      .expect_challenge()
      .times(1)
      .in_sequence(&mut sequence)
      .returning(|_| Ok(TxHash::random()));

    scribe.expect_clone().return_once(move || scribe_clone);

    let cancel = CancellationToken::new();

    let log: Log = serde_json::from_str(LOG).unwrap();
    let event = Event::try_from(log).unwrap();

    let duration = Duration::from_millis(5);

    let (mut processor, tx) = ScribeEventsProcessor::new(scribe, cancel.clone(), Some(duration));

    let handle = tokio::spawn(async move {
      processor.start().await.unwrap();
    });

    tx.send(event).await.unwrap();

    tokio::time::sleep(duration * 2).await;
    // cancel the process
    cancel.cancel();
    // wait till termination
    handle.await.unwrap();
  }

  #[tokio::test]
  async fn test_handle_event_contract_error_propagates() {
    let mut scribe = MockScribe::new();
    scribe.expect_get_challenge_period().returning(|| Ok(10));
    scribe.expect_is_op_poke_challengeable().returning(|_, _| {
      Err(ContractError::AlloySolTypesError(
        alloy::sol_types::Error::Other("test error".into()),
      ))
    });

    let log: Log = serde_json::from_str(LOG).unwrap();
    let event = Event::try_from(log).unwrap();

    let (mut processor, _) = ScribeEventsProcessor::new(scribe, CancellationToken::new(), None);
    processor.refresh_challenge_period().await.unwrap();

    let result = processor.handle_event(event).await;
    assert!(
      result.is_err(),
      "Contract error should propagate from handle_event"
    );
    assert!(
      matches!(
        result.unwrap_err(),
        crate::error::ProcessorError::ContractError(_)
      ),
      "Error should be ProcessorError::ContractError"
    );
  }

  #[tokio::test]
  async fn test_spawn_challenge_error_does_not_panic() {
    let cancel = CancellationToken::new();
    let mut scribe = MockScribe::new();
    let mut scribe_clone = MockScribe::new();
    scribe_clone
      .expect_address()
      .return_const(Address::random().to_owned());

    // challenge returns an error
    scribe_clone.expect_challenge().times(1).returning(|_| {
      Err(ContractError::MissingPrivateProvider {
        address: Address::random(),
      })
    });

    scribe.expect_clone().return_once(move || scribe_clone);

    let log: Log = serde_json::from_str(LOG).unwrap();
    let poke: Log<ScribeOptimistic::OpPoked> = log.log_decode().unwrap();

    let duration = Duration::from_millis(5);

    let (mut processor, _) = ScribeEventsProcessor::new(scribe, cancel.clone(), Some(duration));

    processor.spawn_challenge(&poke);
    assert!(processor.challenge_process_cancelation_token.is_some());

    // Wait for the challenge task to complete (it should log error but not panic)
    tokio::time::sleep(duration * 3).await;
  }

  #[tokio::test]
  async fn test_refresh_challenge_period_rpc_error() {
    let mut scribe = MockScribe::new();
    scribe.expect_get_challenge_period().returning(|| {
      Err(ContractError::RpcError(
        alloy::transports::RpcError::Transport(
          alloy::transports::TransportErrorKind::PubsubUnavailable,
        ),
      ))
    });

    let (mut processor, _) = ScribeEventsProcessor::new(scribe, CancellationToken::new(), None);

    let result = processor.refresh_challenge_period().await;
    assert!(
      result.is_err(),
      "RPC error from get_challenge_period should propagate"
    );
  }

  #[tokio::test]
  async fn test_start_handles_channel_close() {
    let mut scribe = MockScribe::new();
    scribe.expect_get_challenge_period().returning(|| Ok(10));
    scribe
      .expect_address()
      .return_const(Address::random().to_owned());

    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    let (mut processor, tx) = ScribeEventsProcessor::new(scribe, cancel_clone, None);

    // Drop the sender to close the channel
    drop(tx);

    // Cancel after a short delay so start() doesn't run forever
    tokio::spawn(async move {
      tokio::time::sleep(Duration::from_millis(50)).await;
      cancel.cancel();
    });

    // start() should handle the closed channel gracefully (log warning, not crash)
    let result = processor.start().await;
    assert!(
      result.is_ok(),
      "start() should not crash when channel is closed"
    );
  }

  #[tokio::test]
  async fn test_handle_event_error_in_start_loop_logged_not_fatal() {
    let mut scribe = MockScribe::new();
    scribe
      .expect_address()
      .return_const(Address::random().to_owned());
    scribe.expect_get_challenge_period().returning(|| Ok(10));

    // First event triggers a contract error, should be logged but not fatal
    scribe.expect_is_op_poke_challengeable().returning(|_, _| {
      Err(ContractError::AlloySolTypesError(
        alloy::sol_types::Error::Other("test".into()),
      ))
    });

    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    let log: Log = serde_json::from_str(LOG).unwrap();
    let event = Event::try_from(log).unwrap();

    let (mut processor, tx) = ScribeEventsProcessor::new(scribe, cancel_clone, None);

    let handle = tokio::spawn(async move {
      processor.start().await.unwrap();
    });

    // Send event that will cause an error
    tx.send(event).await.unwrap();

    // Give time for error to be processed
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Cancel the process - if we get here, the error didn't crash start()
    cancel.cancel();
    handle.await.unwrap();
  }

  #[tokio::test]
  async fn test_multiple_op_poked_events_cancel_previous_challenge() {
    let mut scribe = MockScribe::new();
    scribe.expect_get_challenge_period().returning(|| Ok(10));
    scribe
      .expect_is_op_poke_challengeable()
      .returning(|_, _| Ok(true));

    // Need two clones for two spawn_challenge calls
    let mut clone1 = MockScribe::new();
    clone1
      .expect_address()
      .return_const(Address::random().to_owned());
    clone1.expect_challenge().never(); // Should be cancelled before executing

    let mut clone2 = MockScribe::new();
    clone2
      .expect_address()
      .return_const(Address::random().to_owned());
    clone2
      .expect_challenge()
      .times(1)
      .returning(|_| Ok(TxHash::random()));

    let mut seq = Sequence::new();
    scribe
      .expect_clone()
      .times(1)
      .in_sequence(&mut seq)
      .return_once(move || clone1);
    scribe
      .expect_clone()
      .times(1)
      .in_sequence(&mut seq)
      .return_once(move || clone2);

    let log: Log = serde_json::from_str(LOG).unwrap();
    let event1 = Event::try_from(log.clone()).unwrap();
    let event2 = Event::try_from(log).unwrap();

    // Use a long delay so we can send the second event before the first challenge fires
    let duration = Duration::from_millis(100);

    let (mut processor, _) =
      ScribeEventsProcessor::new(scribe, CancellationToken::new(), Some(duration));
    processor.refresh_challenge_period().await.unwrap();

    // First event spawns a challenge
    processor.handle_event(event1).await.unwrap();
    let first_token = processor
      .challenge_process_cancelation_token
      .clone()
      .unwrap();
    assert!(!first_token.is_cancelled(), "First token should be active");

    // Second event should cancel the first and spawn a new one
    processor.handle_event(event2).await.unwrap();
    assert!(
      first_token.is_cancelled(),
      "First challenge should be cancelled when second OpPoked arrives"
    );
    assert!(
      processor.challenge_process_cancelation_token.is_some(),
      "New challenge token should exist"
    );

    // Wait for the second challenge to execute
    tokio::time::sleep(duration * 2).await;
  }

  #[tokio::test]
  async fn test_op_poke_then_challenged_successfully_then_op_poke() {
    let challenge_log_str = r#"
      {
        "address": "0x891e368fe81cba2ac6f6cc4b98e684c106e2ef4f",
        "topics": [
          "0xac50cef58b3aef7f7c30349f5e4a342a29d2325a02eafc8dacfdba391e6d5db3",
          "0x0000000000000000000000001f7acda376ef37ec371235a094113df9cb4efee1"
        ],
        "data": "0x0000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000002456d7d2e80000000000000000000000003f17f1962b36e491b30a40b2405849e597ba5fb500000000000000000000000000000000000000000000000000000000",
        "blockNumber": "0x75823d",
        "transactionHash": "0xfc99cae27a9593e3b5910ef65dfeae601de2b2cfd678a1396c7878b489f34509",
        "transactionIndex": "0x8f",
        "blockHash": "0x1ea63aa0ae6603a49fccd44d1271a7112f34b41a8512e625844a560cde066a24",
        "logIndex": "0x116",
        "removed": false
      }
    "#;

    let mut scribe = MockScribe::new();
    scribe.expect_get_challenge_period().returning(|| Ok(10));
    scribe
      .expect_is_op_poke_challengeable()
      .returning(|_, _| Ok(true));

    // Two clones for two OpPoked events
    let mut clone1 = MockScribe::new();
    clone1
      .expect_address()
      .return_const(Address::random().to_owned());
    clone1.expect_challenge().never(); // Will be cancelled by OpPokeChallengedSuccessfully

    let mut clone2 = MockScribe::new();
    clone2
      .expect_address()
      .return_const(Address::random().to_owned());
    clone2
      .expect_challenge()
      .times(1)
      .returning(|_| Ok(TxHash::random()));

    let mut seq = Sequence::new();
    scribe
      .expect_clone()
      .times(1)
      .in_sequence(&mut seq)
      .return_once(move || clone1);
    scribe
      .expect_clone()
      .times(1)
      .in_sequence(&mut seq)
      .return_once(move || clone2);

    let log: Log = serde_json::from_str(LOG).unwrap();
    let op_poked_event1 = Event::try_from(log.clone()).unwrap();
    let op_poked_event2 = Event::try_from(log).unwrap();

    let challenge_log: Log = serde_json::from_str(challenge_log_str).unwrap();
    let challenged_event = Event::try_from(challenge_log).unwrap();

    let duration = Duration::from_millis(100);
    let (mut processor, _) =
      ScribeEventsProcessor::new(scribe, CancellationToken::new(), Some(duration));
    processor.refresh_challenge_period().await.unwrap();

    // 1. First OpPoked → spawns challenge
    processor.handle_event(op_poked_event1).await.unwrap();
    let first_token = processor
      .challenge_process_cancelation_token
      .clone()
      .unwrap();
    assert!(!first_token.is_cancelled());

    // 2. OpPokeChallengedSuccessfully → cancels first challenge
    processor.handle_event(challenged_event).await.unwrap();
    assert!(
      first_token.is_cancelled(),
      "First challenge should be cancelled"
    );
    assert!(
      processor.challenge_process_cancelation_token.is_none(),
      "Token should be cleared after cancellation"
    );

    // 3. Second OpPoked → spawns new challenge
    processor.handle_event(op_poked_event2).await.unwrap();
    assert!(
      processor.challenge_process_cancelation_token.is_some(),
      "New challenge should be spawned"
    );

    // Wait for the second challenge to execute
    tokio::time::sleep(duration * 2).await;
  }
}
