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
  RootProvider,
  Ethereum,
>;

// helper function to create a new provider with a random private key
#[cfg(test)]
pub(crate) fn new_provider(url: &str) -> Arc<FullHTTPRetryProviderWithSigner> {
  use alloy::{
    providers::ProviderBuilder, rpc::client::ClientBuilder, signers::local::PrivateKeySigner,
    transports::layers::RetryBackoffLayer,
  };

  let client = ClientBuilder::default()
    .layer(RetryBackoffLayer::new(15, 200, 300))
    .http(url.parse().unwrap());

  Arc::new(
    ProviderBuilder::new()
      // Add chain id request from rpc
      .filler(ChainIdFiller::new(Some(1)))
      // Add default signer
      .wallet(EthereumWallet::from(PrivateKeySigner::random()))
      .on_client(client),
  )
}

/// PollProvider is a trait that defines the interface for a polling events from
/// the Ethereum network or any other network.
#[allow(async_fn_in_trait)]
#[cfg_attr(test, mockall::automock)]
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
