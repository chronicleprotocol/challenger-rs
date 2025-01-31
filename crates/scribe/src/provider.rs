use std::sync::Arc;

use alloy::{
  network::{Ethereum, EthereumWallet},
  providers::{
    fillers::{
      BlobGasFiller, ChainIdFiller, FillProvider, GasFiller, JoinFill, NonceFiller, WalletFiller,
    },
    Identity, Provider, RootProvider,
  },
  rpc::types::{Filter, Log},
  transports::{
    http::{Client, Http},
    layers::RetryBackoffService,
  },
};

use crate::error::PollProviderResult;

/// The provider type used to interact with the Ethereum network with a signer.
pub type FullHTTPRetryProviderWithSigner = FillProvider<
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

/// PollProvider is a trait that defines the interface for a polling events from
/// the Ethereum network or any other network.
#[allow(async_fn_in_trait)]
pub trait PollProvider {
  /// Get logs from the network with a filter.
  async fn get_logs(&self, filter: &Filter) -> PollProviderResult<Vec<Log>>;

  /// Get the latest block number from the network.
  async fn get_block_number(&self) -> PollProviderResult<u64>;
}

/// EthereumPollProvider is a PollProvider implementation for the Ethereum network.
/// Technically it is a wrapper around the `RetryProviderWithSigner` provider.
pub struct EthereumPollProvider {
  provider: Arc<FullHTTPRetryProviderWithSigner>,
}

impl EthereumPollProvider {
  pub fn new(provider: Arc<FullHTTPRetryProviderWithSigner>) -> Self {
    Self { provider }
  }
}

impl PollProvider for EthereumPollProvider {
  async fn get_logs(&self, filter: &Filter) -> PollProviderResult<Vec<Log>> {
    let res = self.provider.get_logs(filter).await?;

    Ok(res)
  }

  async fn get_block_number(&self) -> PollProviderResult<u64> {
    let res = self.provider.get_block_number().await?;
    Ok(res)
  }
}
