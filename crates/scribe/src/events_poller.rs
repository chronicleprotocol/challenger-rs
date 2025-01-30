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

use std::{collections::HashMap, sync::Arc, time::Duration};

use alloy::{
  network::{Ethereum, EthereumWallet},
  primitives::Address,
  providers::{
    fillers::{
      BlobGasFiller, ChainIdFiller, FillProvider, GasFiller, JoinFill, NonceFiller, WalletFiller,
    },
    Identity, Provider, RootProvider, WalletProvider,
  },
  rpc::types::{Filter, Log},
  sol_types::SolEvent,
  transports::{
    http::{Client, Http},
    layers::RetryBackoffService,
  },
};
use tokio::{select, sync::mpsc::Sender};
use tokio_util::sync::CancellationToken;

use crate::{
  contract::ScribeOptimistic::{OpPokeChallengedSuccessfully, OpPoked},
  error::{PollerError, PollerResult},
  metrics, Event,
};

const MAX_ADDRESS_PER_REQUEST: usize = 10;
const MAX_RETRY_COUNT: u16 = 5;

/// The provider type used to interact with the Ethereum network with a signer.
pub type RetryProviderWithSigner = FillProvider<
  JoinFill<
    JoinFill<
      JoinFill<
        Identity,
        JoinFill<GasFiller, JoinFill<BlobGasFiller, JoinFill<NonceFiller, ChainIdFiller>>>,
      >,
      ChainIdFiller,
    >,
    WalletFiller<EthereumWallet>,
  >,
  RootProvider<RetryBackoffService<Http<Client>>>,
  RetryBackoffService<Http<Client>>,
  Ethereum,
>;

/// Poller is responsible for polling for new events in the Ethereum network.
/// It will poll for new events in the range `self.last_processes_block..latest_block`
/// every `self.poll_interval_seconds` seconds.
/// It will send the new events to the `tx` channel.
/// For optimization reasons, it will query for events in chunks of `MAX_ADDRESS_PER_REQUEST` addresses.
#[derive(Debug, Clone)]
pub struct Poller {
  addresses: Vec<Address>,
  cancellation_token: CancellationToken,
  handler_channels: HashMap<Address, Sender<Event>>,
  last_processes_block: u64,
  poll_interval_seconds: u64,
  provider: Arc<RetryProviderWithSigner>,
  retry_count: u16,
}

impl Poller {
  pub fn new(
    handler_channels: HashMap<Address, Sender<Event>>,
    cancellation_token: CancellationToken,
    provider: Arc<RetryProviderWithSigner>,
    poll_interval_seconds: u64,
    from_block: Option<u64>,
  ) -> Self {
    Self {
      addresses: handler_channels.keys().cloned().collect(),
      cancellation_token,
      provider,
      handler_channels,
      poll_interval_seconds,
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

  // Poll for new events in block range `self.last_processes_block..latest_block`
  async fn poll(&mut self) -> PollerResult<()> {
    log::trace!("Poller: Polling for new events");
    // Get latest block number
    let latest_block = self.provider.get_block_number().await?;
    if self.last_processes_block == 0 {
      log::info!("Poller: First run, setting last processed block to latest block");
      self.last_processes_block = latest_block;
    }

    if latest_block <= self.last_processes_block {
      log::warn!(
        "Poller: Latest block {:?} is not greater than last processed block {:?}",
        latest_block,
        self.last_processes_block
      );
      return Ok(());
    }
    // Split addresses into chunks of MAX_ADDRESS_PER_REQUEST to optimize amount of requests
    for chunk in self.addresses.chunks(MAX_ADDRESS_PER_REQUEST) {
      let logs = self
        .query_logs(
          chunk.to_vec(),
          self.last_processes_block, // unwrap is safe because we checked it in the beginning
          latest_block,
        )
        .await?;

      log::debug!(
        "Poller: Received {} logs for chunk [{:?}]",
        logs.len(),
        chunk
      );

      for log in logs {
        match Event::try_from(log) {
          Ok(event) => {
            log::debug!(
              "Poller: {} received for address {:?} processing",
              event.title(),
              &event.address()
            );
            // Send event to the channel
            let Some(tx) = self.handler_channels.get(&event.address()) else {
              // TODO: should never happen
              panic!(
                "Poller: No channel found for address {:?}, skipping",
                &event.address()
              );
            };

            tx.send(event).await?;
          }
          Err(e) => {
            log::error!("Poller: Failed to parse log: {:?}", e);
            continue;
          }
        };
      }
    }

    // Reset retry count, or might be issues
    self.retry_count = 0;
    // +1 because we don't need to get data from same block twice
    self.last_processes_block = latest_block + 1;
    // Updating last scanned block metric
    metrics::set_last_scanned_block(self.provider.default_signer_address(), latest_block as i64);

    Ok(())
  }

  /// Start the event listener
  pub async fn start(&mut self) -> PollerResult<()> {
    log::info!("Poller: Starting polling events from RPC...");

    loop {
      select! {
          _ = self.cancellation_token.cancelled() => {
              log::info!("Poller: got cancel signal, terminating...");
              return Ok(());
          }
          _ = tokio::time::sleep(Duration::from_secs(self.poll_interval_seconds)) => {
              log::trace!("Poller: Executing tick for events listener...");
              if let Err(err) = self.poll().await {
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
              }
          }
      }
    }
  }
}
