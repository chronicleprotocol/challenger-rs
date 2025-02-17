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

use std::{collections::HashMap, time::Duration};

use alloy::{
  primitives::Address,
  rpc::types::{Filter, Log},
  sol_types::SolEvent,
};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::{
  contract::ScribeOptimistic::{OpPokeChallengedSuccessfully, OpPoked},
  error::{PollerError, PollerResult},
  metrics,
  provider::PollProvider,
  Event,
};

const MAX_ADDRESS_PER_REQUEST: usize = 10;
const MAX_RETRY_COUNT: u16 = 5;
const MAX_HISTORY_BLOCKS: u64 = 5;

#[derive(Debug, Default)]
pub struct PollerBuilder {
  signer_address: Address,
  handler_channels: HashMap<Address, Sender<Event>>,
  cancellation_token: CancellationToken,
  poll_interval: Duration,
  from_block: Option<u64>,
}

impl PollerBuilder {
  /// Creates a new `PollerBuilder` with default values.
  pub fn builder() -> Self {
    Self::default()
  }

  /// Sets the signer address for the `PollerBuilder`.
  ///
  /// # Arguments
  ///
  /// * `signer_address` - The address of the signer.
  pub fn with_signer_address(self, signer_address: Address) -> Self {
    Self {
      signer_address,
      ..self
    }
  }

  /// Sets the handler channels for the `PollerBuilder`.
  ///
  /// # Arguments
  ///
  /// * `handler_channels` - A map of addresses to event sender channels.
  pub fn with_handler_channels(self, handler_channels: HashMap<Address, Sender<Event>>) -> Self {
    Self {
      handler_channels,
      ..self
    }
  }

  /// Sets the cancellation token for the `PollerBuilder`.
  ///
  /// # Arguments
  ///
  /// * `cancellation_token` - The cancellation token to use for stopping the poller.
  pub fn with_cancellation_token(self, cancellation_token: CancellationToken) -> Self {
    Self {
      cancellation_token,
      ..self
    }
  }

  /// Sets the poll interval in seconds for the `PollerBuilder`.
  ///
  /// # Arguments
  ///
  /// * `poll_interval_seconds` - The interval in seconds between each poll.
  pub fn with_poll_interval(self, poll_interval: Duration) -> Self {
    Self {
      poll_interval,
      ..self
    }
  }

  /// Sets the starting block number for the `PollerBuilder`.
  ///
  /// # Arguments
  ///
  /// * `from_block` - The block number to start polling from.
  pub fn with_from_block(self, from_block: Option<u64>) -> Self {
    Self { from_block, ..self }
  }

  /// Builds the `Poller` with the provided `PollProvider`.
  ///
  /// # Arguments
  ///
  /// * `provider` - The provider to use for polling.
  ///
  /// # Returns
  ///
  /// A new instance of `Poller`.
  pub fn build<P: PollProvider>(self, provider: P) -> Poller<P> {
    Poller::new(
      self.signer_address,
      self.handler_channels,
      self.cancellation_token,
      provider,
      self.poll_interval,
      self.from_block,
    )
  }
}

/// Poller is responsible for polling for new events in the Ethereum network.
/// It will poll for new events in the range `self.last_processes_block..latest_block`
/// every `self.poll_interval_seconds` seconds.
/// It will send the new events to the `tx` channel.
/// For optimization reasons, it will query for events in chunks of `MAX_ADDRESS_PER_REQUEST` addresses.
#[derive(Debug, Clone)]
pub struct Poller<P: PollProvider> {
  from: Address,
  addresses: Vec<Address>,
  cancellation_token: CancellationToken,
  handler_channels: HashMap<Address, Sender<Event>>,
  last_processes_block: u64,
  poll_interval: Duration,
  provider: P,
  retry_count: u16,
}

impl<P: PollProvider> Poller<P> {
  fn new(
    from: Address,
    handler_channels: HashMap<Address, Sender<Event>>,
    cancellation_token: CancellationToken,
    provider: P,
    poll_interval: Duration,
    from_block: Option<u64>,
  ) -> Self {
    Self {
      from,
      addresses: handler_channels.keys().cloned().collect(),
      cancellation_token,
      provider,
      handler_channels,
      poll_interval,
      last_processes_block: from_block.unwrap_or(0),
      retry_count: 0,
    }
  }

  // Loads list of logs with filters:
  // - address (set of addresses to get events from)
  // - from_block, to_block
  // - events_signature (checks by topics, need only `OpPoked` and `OpPokeChallengedSuccessfully`
  async fn query_logs(
    &self,
    chunk: Vec<Address>,
    from_block: u64,
    to_block: u64,
  ) -> PollerResult<Vec<Log>> {
    let filter = Filter::new()
      .address(chunk.to_vec())
      .from_block(from_block)
      .to_block(to_block)
      .event_signature(vec![
        OpPoked::SIGNATURE_HASH,
        OpPokeChallengedSuccessfully::SIGNATURE_HASH,
      ]);

    log::trace!(
      "Poller: Polling for new events from {} to {} for addresses [{:?}]",
      &from_block,
      &to_block,
      &chunk
    );

    Ok(self.provider.get_logs(&filter).await?)
  }

  // Sends event to the channel by address, if no channel found, panics.
  async fn send_log_for_processing(&self, log: Log) -> PollerResult<()> {
    let event = Event::try_from(log)?;

    log::debug!(
      "Poller: {} received for address {:?} processing",
      event.title(),
      &event.address()
    );

    // Send event to the channel
    let Some(tx) = self.handler_channels.get(&event.address()) else {
      // should never happen !
      panic!(
        "Poller: No channel found for address {:?}, skipping",
        &event.address()
      );
    };

    tx.send(event).await?;

    Ok(())
  }

  // Split addresses into chunks and poll for logs for every chunk
  // from `self.last_processes_block..latest_block`.
  // If any log is received, send it for processing using `send_log_for_processing`.
  async fn chunk_and_poll_logs(&self, latest_block: u64) -> PollerResult<()> {
    // Split addresses into chunks of MAX_ADDRESS_PER_REQUEST to optimize amount of requests
    for chunk in self.addresses.chunks(MAX_ADDRESS_PER_REQUEST) {
      let logs = self
        .query_logs(chunk.to_vec(), self.last_processes_block, latest_block)
        .await?;

      log::debug!(
        "Poller: Received {} logs for chunk [{:?}]",
        logs.len(),
        chunk
      );

      for log in logs {
        if let Err(e) = self.send_log_for_processing(log).await {
          log::error!("Poller: Failed to parse log: {:?}", e);
        };
      }
    }

    Ok(())
  }

  // Polls for new events in the Ethereum network.
  // It will poll for new events in the range `self.last_processes_block..latest_block`.
  // It fetches latest block number before polling.
  async fn poll_for_new_events(&mut self) -> PollerResult<()> {
    log::trace!("Poller: Polling for new events");
    // Get latest block number
    let latest_block = self.provider.get_block_number().await?;
    if self.last_processes_block == 0 {
      log::info!("Poller: First run, setting last processed block to latest block");
      // safe to store without checking `< 0` because of unsigned
      self.last_processes_block = latest_block - MAX_HISTORY_BLOCKS;
    }

    if latest_block <= self.last_processes_block {
      log::warn!(
        "Poller: Latest block {:?} is not greater than last processed block {:?}",
        latest_block,
        self.last_processes_block
      );
      return Ok(());
    }

    self.chunk_and_poll_logs(latest_block).await?;

    // +1 because we don't need to get data from same block twice
    self.last_processes_block = latest_block + 1;
    // Updating last scanned block metric
    metrics::set_last_scanned_block(self.from, latest_block as i64);

    Ok(())
  }

  /// Start the event listener
  pub async fn start(&mut self) -> PollerResult<()> {
    log::info!("Poller: Starting polling events from RPC...");

    loop {
      tokio::select! {
        _ = self.cancellation_token.cancelled() => {
          log::info!("Poller: got cancel signal, terminating...");
          return Ok(());
        }
        _ = tokio::time::sleep(self.poll_interval) => {
          log::trace!("Poller: Executing tick for events listener...");
          if let Err(err) = self.poll_for_new_events().await {
            if self.retry_count >= MAX_RETRY_COUNT {
              log::error!(
                "Poller: Max {} reties reached, will not retry anymore: {:?}",
                MAX_RETRY_COUNT,
                err
              );
              return Err(PollerError::MaxRetryAttemptsExceeded(MAX_RETRY_COUNT));
            }

            self.retry_count += 1;

            log::error!("Poller: Failed to poll for events, will retry: {:?}", err);
            continue;
          }

          // Reset retry count on success
          self.retry_count = 0;
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{error::PollProviderError, provider::MockPollProvider};
  use alloy::{
    primitives::address,
    rpc::types::Log,
    transports::{RpcError, TransportErrorKind},
  };

  // Predefined details for tests
  static ADDRESS: Address = address!("0x891e368fe81cba2ac6f6cc4b98e684c106e2ef4f");
  static BLOCK_NUMBER: u64 = 7609469;
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

  #[tokio::test]
  async fn test_poller_poll_with_proper_filters() {
    let mut provider = MockPollProvider::new();
    // Have to be called only once.
    provider
      .expect_get_logs()
      .times(1)
      .withf(move |f| {
        // Check that filter is correct
        assert_eq!(f.get_from_block(), Some(5));
        assert_eq!(f.get_to_block(), Some(10));
        assert!(!f.address.is_empty());
        f.address.matches(&ADDRESS.clone())
      })
      .returning(|_| Ok(vec![]));

    // First call to get_block_number
    provider
      .expect_get_block_number()
      .times(1)
      .returning(|| Ok(10));

    let mut addresses: HashMap<Address, Sender<Event>> = HashMap::new();
    addresses.insert(ADDRESS, tokio::sync::mpsc::channel(100).0);

    let mut poller = PollerBuilder::builder()
      .with_handler_channels(addresses)
      .with_poll_interval(Duration::from_millis(100))
      .build(provider);

    poller.poll_for_new_events().await.unwrap();
  }

  #[tokio::test]
  async fn test_proper_logs_sends_to_channels() {
    let deserialized: Log = serde_json::from_str(LOG).unwrap();

    let mut provider = MockPollProvider::new();
    // Have to be called only once.
    provider
      .expect_get_logs()
      .returning(move |_| Ok(vec![deserialized.clone()]));

    provider
      .expect_get_block_number()
      .returning(move || Ok(BLOCK_NUMBER));

    let mut addresses: HashMap<Address, Sender<Event>> = HashMap::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    addresses.insert(ADDRESS, tx);

    let mut poller = PollerBuilder::builder()
      .with_handler_channels(addresses)
      .with_poll_interval(Duration::from_millis(100))
      .build(provider);

    poller.poll_for_new_events().await.unwrap();

    // Check event was sent to the channel
    let event = rx.recv().await.unwrap();
    assert_eq!(event.title(), "OpPoked");
    assert_eq!(event.address(), ADDRESS);

    // poll() updated last processed block + 1
    assert_eq!(poller.last_processes_block, BLOCK_NUMBER + 1);
  }

  #[tokio::test]
  async fn test_send_event_to_process() {
    let deserialized: Log = serde_json::from_str(LOG).unwrap();

    // New channel for address
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);

    let mut addresses: HashMap<Address, Sender<Event>> = HashMap::new();
    addresses.insert(ADDRESS, tx);

    let poller = PollerBuilder::builder()
      .with_handler_channels(addresses)
      .with_poll_interval(Duration::from_millis(100))
      .build(MockPollProvider::new());

    poller.send_log_for_processing(deserialized).await.unwrap();

    let event = rx.recv().await.unwrap();
    assert_eq!(event.title(), "OpPoked");
    assert_eq!(event.address(), ADDRESS);
  }

  #[tokio::test]
  #[should_panic]
  async fn test_send_event_to_process_panics_on_unknown_address() {
    let deserialized: Log = serde_json::from_str(LOG).unwrap();

    // should panic with no channel found for event address
    PollerBuilder::builder()
      .with_poll_interval(Duration::from_millis(100))
      .build(MockPollProvider::new())
      .send_log_for_processing(deserialized)
      .await
      .unwrap();
  }

  // query_logs returns error

  #[tokio::test]
  async fn query_logs_returns_error() {
    let mut provider = MockPollProvider::new();
    // Have to be called only once.
    provider.expect_get_logs().times(1).returning(|_| {
      // Just some random error
      Err(PollProviderError::RpcError(RpcError::Transport(
        TransportErrorKind::PubsubUnavailable,
      )))
    });

    // First call to get_block_number
    provider
      .expect_get_block_number()
      .times(1)
      .returning(|| Ok(10));
    // Last block processed...
    let last_block = 5;

    let mut addresses: HashMap<Address, Sender<Event>> = HashMap::new();
    addresses.insert(ADDRESS, tokio::sync::mpsc::channel(100).0);

    let mut poller = PollerBuilder::builder()
      .with_handler_channels(addresses)
      .with_poll_interval(Duration::from_millis(100))
      .with_from_block(Some(last_block))
      .build(provider);

    // First call will update last processed block
    assert!(poller.poll_for_new_events().await.is_err());
    // last processed block wasn't updated
    assert_eq!(poller.last_processes_block, last_block);
  }

  #[tokio::test]
  async fn query_logs_not_failing_on_parsing_error() {
    let invalid_log = r#"{
				"address": "0x891e368fe81cba2ac6f6cc4b98e684c106e2ef4f",
				"topics": [
					"0x02652e7fe7a64eb217d52e6eec6ef55e181ceac19d9f5deb15aa62ea2b1bf9aa",
					"0x0000000000000000000000001f7acda376ef37ec371235a094113df9cb4efee1",
					"0x0000000000000000000000002b5ad5c4795c026514f8317c7a215e218dccd6cf",
					"0x0000000000000000000000000000000000000000000000000000000000000038"
				],
				"data": "0x",
				"blockNumber": "0x741c7a",
				"transactionHash": "0x790900909f67b7fc19de3630d1189663371b5c069aebdb9c1f0958e5223fe48d",
				"transactionIndex": "0x56",
				"blockHash": "0xd70c77284af3e450312fc85d5581a9f78ad738e8339073e0b33adf3d5aa5e721",
				"logIndex": "0x86",
				"removed": false
			}"#;
    let deserialized: Log = serde_json::from_str(invalid_log).unwrap();
    let log = deserialized.clone();

    let mut provider = MockPollProvider::new();
    // Have to be called only once.
    provider
      .expect_get_logs()
      .returning(move |_| Ok(vec![log.clone()]));

    // New channel for address
    let (tx, rx) = tokio::sync::mpsc::channel(1);

    let mut addresses: HashMap<Address, Sender<Event>> = HashMap::new();
    addresses.insert(ADDRESS, tx);

    let poller = PollerBuilder::builder()
      .with_handler_channels(addresses)
      .with_poll_interval(Duration::from_millis(100))
      .build(provider);
    // returns error on parsing log
    assert!(poller.send_log_for_processing(deserialized).await.is_err());
    // doesn't fail if parsing fails
    assert!(poller.chunk_and_poll_logs(1).await.is_ok());
    // nothing was sent to the channel
    assert!(rx.is_empty());
  }

  // poll splits addresses into chunks
  #[tokio::test]
  async fn test_poller_poll_splits_addresses_into_chunks() {
    let mut provider = MockPollProvider::new();
    // Have to be called 2 times with chunks of `MAX_ADDRESS_PER_REQUEST` addresses
    provider.expect_get_logs().times(2).returning(move |f| {
      // Check that filter is correct
      assert_eq!(f.address.clone().into_iter().len(), MAX_ADDRESS_PER_REQUEST);

      Ok(vec![])
    });

    // First call to get_block_number
    provider
      .expect_get_block_number()
      .times(1)
      .returning(|| Ok(10));

    let mut addresses: HashMap<Address, Sender<Event>> = HashMap::new();
    for _i in 0..MAX_ADDRESS_PER_REQUEST * 2 {
      addresses.insert(Address::random(), tokio::sync::mpsc::channel(100).0);
    }

    let mut poller = PollerBuilder::builder()
      .with_handler_channels(addresses)
      .with_poll_interval(Duration::from_millis(100))
      .build(provider);

    poller.poll_for_new_events().await.unwrap();
  }

  #[tokio::test]
  async fn test_start_can_be_cancelled() {
    let mut provider = MockPollProvider::new();
    // Have to be called only once.
    provider.expect_get_logs().returning(|_| Ok(vec![]));

    // First call to get_block_number
    provider.expect_get_block_number().returning(|| Ok(10));

    let mut addresses: HashMap<Address, Sender<Event>> = HashMap::new();
    addresses.insert(ADDRESS, tokio::sync::mpsc::channel(100).0);

    // Create a cancellation token
    let cancel = CancellationToken::new();

    let mut poller = PollerBuilder::builder()
      .with_handler_channels(addresses)
      .with_poll_interval(Duration::from_millis(100))
      .with_cancellation_token(cancel.clone())
      .build(provider);

    tokio::spawn(async move {
      tokio::time::sleep(Duration::from_millis(101)).await;
      cancel.cancel();
    });

    poller.start().await.unwrap();
  }

  #[tokio::test]
  async fn test_start_exist_after_max_retries() {
    let mut provider = MockPollProvider::new();
    // Have to be called only once.
    provider.expect_get_logs().returning(|_| {
      Err(PollProviderError::RpcError(RpcError::Transport(
        TransportErrorKind::PubsubUnavailable,
      )))
    });

    // Always fail on get_block_number
    provider
      .expect_get_block_number()
      .returning(|| Err(PollProviderError::RpcError(RpcError::NullResp)));

    let mut addresses: HashMap<Address, Sender<Event>> = HashMap::new();
    addresses.insert(ADDRESS, tokio::sync::mpsc::channel(100).0);

    // Create a cancellation token
    let cancel = CancellationToken::new();

    let mut poller = PollerBuilder::builder()
      .with_handler_channels(addresses)
      .with_poll_interval(Duration::from_millis(10))
      .with_cancellation_token(cancel.clone())
      .build(provider);

    // Cancel if not finished on max retries
    tokio::spawn(async move {
      tokio::time::sleep(Duration::from_secs(1)).await;
      cancel.cancel();
    });

    // If assertion fails, it means that poller didn't exit after max retries and was cancelled
    assert!(poller.start().await.is_err());
  }
}
