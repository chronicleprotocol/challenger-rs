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

use std::{sync::Arc, time::Duration};

use alloy::{
    network::{Ethereum, EthereumWallet},
    primitives::Address,
    providers::{
        fillers::{
            BlobGasFiller, CachedNonceManager, ChainIdFiller, FillProvider, GasFiller, JoinFill,
            NonceFiller, WalletFiller,
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
use eyre::{Context, Result};
use tokio::{select, sync::mpsc::Sender};
use tokio_util::sync::CancellationToken;

use crate::{
    contract::{
        EventWithMetadata,
        ScribeOptimistic::{OpPokeChallengedSuccessfully, OpPoked},
    },
    metrics,
};

// const POLL_INTERVAL_SEC: u64 = 30;
const MAX_ADDRESS_PER_REQUEST: usize = 50;

/// The provider type used to interact with the Ethereum network.
pub type RpcRetryProvider = RetryBackoffService<Http<Client>>;

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

#[derive(Debug, Clone)]
pub struct Poller {
    addresses: Vec<Address>,
    cancelation_token: CancellationToken,
    provider: Arc<RetryProviderWithSigner>,
    last_processes_block: Option<u64>,
    tx: Sender<EventWithMetadata>,
    poll_interval_seconds: u64,
}

impl Poller {
    pub fn new(
        addresses: Vec<Address>,
        cancelation_token: CancellationToken,
        provider: Arc<RetryProviderWithSigner>,
        tx: Sender<EventWithMetadata>,
        poll_interval_seconds: u64,
    ) -> Self {
        Self {
            addresses,
            cancelation_token,
            provider,
            tx,
            last_processes_block: None,
            poll_interval_seconds,
        }
    }

    async fn query_logs(
        &self,
        chunk: Vec<Address>,
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<Log>> {
        let filter = Filter::new()
            .address(chunk.to_vec())
            .from_block(from_block)
            .to_block(to_block)
            .event_signature(vec![
                OpPoked::SIGNATURE_HASH,
                OpPokeChallengedSuccessfully::SIGNATURE_HASH,
            ]);

        log::trace!("[{:?}] Getting for new events", &chunk);

        self.provider
            .get_logs(&filter)
            .await
            .wrap_err_with(|| format!("Failed to get logs for addresses: {:?}", chunk))
    }

    // Poll for new events in block range `self.last_processes_block..latest_block`
    async fn poll(&mut self) -> Result<()> {
        log::trace!("Polling for new events");
        // Get latest block number
        let latest_block = self.provider.get_block_number().await.unwrap();
        if None == self.last_processes_block {
            self.last_processes_block = Some(latest_block);
        }
        // TODO remove this line
        if latest_block <= self.last_processes_block.unwrap_or(0) {
            log::warn!(
                "Latest block {:?} is not greater than last processed block {:?}",
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
                    self.last_processes_block.unwrap(),
                    latest_block,
                )
                .await;

            match logs {
                Ok(logs) => {
                    log::debug!("[{:?}] Received {} logs", chunk, logs.len());
                    for log in logs {
                        match EventWithMetadata::from_log(log) {
                            Ok(event) => {
                                log::debug!("[{:?}] Event received: {:?}", chunk, &event);
                                // Send event to the channel
                                self.tx.send(event).await?;
                            }
                            Err(e) => {
                                log::error!("[{:?}] Failed to parse log: {:?}", chunk, e);
                                continue;
                            }
                        };
                    }
                }
                Err(e) => {
                    log::error!("Failed to query logs: {:?}", e);
                }
            }
        }

        self.last_processes_block = Some(latest_block);
        // Updating last scanned block metric
        metrics::set_last_scanned_block(
            self.provider.default_signer_address(),
            latest_block as i64,
        );

        Ok(())
    }

    /// Start the event listener
    pub async fn start(&mut self) -> Result<()> {
        log::info!("Starting polling events from RPC...");

        loop {
            select! {
                _ = self.cancelation_token.cancelled() => {
                    log::info!("Challenger cancelled");
                    return Ok(());
                }
                _ = tokio::time::sleep(Duration::from_secs(self.poll_interval_seconds)) => {
                    log::info!("Executing tick for events listener...");
                    self.poll().await?;
                }
            }
        }
    }
}
