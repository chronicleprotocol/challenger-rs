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

use std::{fmt::Debug, future::Future, sync::Arc, time::Duration};

use alloy::{
  eips::eip2718::Encodable2718,
  network::{Ethereum, NetworkWallet, TransactionBuilder},
  primitives::{Address, TxHash},
  providers::{PendingTransactionBuilder, Provider, WalletProvider},
  rpc::types::{
    mev::{PrivateTransactionPreferences, PrivateTransactionRequest},
    BlockTransactionsKind, Log,
  },
  sol,
  transports::{
    http::{
      reqwest::header::{HeaderMap, HeaderValue},
      Client, Http,
    },
    layers::RetryBackoffService,
  },
};
// use eyre::{bail, Result, WrapErr};

use crate::{
  error::{ContractError, ContractResult},
  RetryProviderWithSigner,
};
use IScribe::SchnorrData;
use ScribeOptimistic::ScribeOptimisticInstance;

// Generate the contract bindings for the ScribeOptimistic contract
sol! {
  #[allow(missing_docs)]
  #[sol(rpc)]
  #[derive(Debug)]
  ScribeOptimistic,
  "abi/ScribeOptimistic.json"
}

// Confirmation timeout for the transaction
const TX_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(30);

// Maximum number of blocks to wait for the transaction to be mined.
const MAX_BLOCKS_TO_WAIT: u64 = 10;

/// The RPC transport type used to interact with the Ethereum network.
pub type RpcTransportWithRetry = RetryBackoffService<Http<Client>>;

pub type ChallengePendingTx = PendingTransactionBuilder<RpcTransportWithRetry, Ethereum>;

/// This trait provides methods required for `challenger` to interact with the ScribeOptimistic smart contract.
pub trait ScribeContract: Clone + Send + Sync + 'static {
  /// Returns the address of the contract.
  fn address(&self) -> &Address;

  /// Returns challenge period from ScribeOptimistic smart contract deployed to `address`.
  /// NOTE: From time to time challenger might need to refresh this value, because it might be changed by the contract owner.
  fn get_challenge_period(&self) -> impl Future<Output = ContractResult<u16>> + Send;

  /// Returns true if given `OpPoked` event is challengeable.
  /// It checks if the event is stale and if the signature is valid.
  fn is_op_poke_challangeble(
    &self,
    op_poked: &Log<ScribeOptimistic::OpPoked>,
    challenge_period: u64,
  ) -> impl Future<Output = ContractResult<bool>> + Send;

  /// Challenges given `OpPoked` event with given `schnorr_data`.
  /// See: `IScribeOptimistic::opChallenge(SchnorrData calldata schnorrData)` for more details.
  fn challenge(
    &self,
    schnorr_data: SchnorrData,
    gas_limit: u64,
  ) -> impl Future<Output = ContractResult<TxHash>> + Send;

  // /// Returns a new provider with the same signer.
  // fn get_cloned_provider(&self) -> Arc<RetryProviderWithSigner>;
}

/// ScribeOptimisticProviderInstance is a real implementation of ScribeOptimisticProvider based on raw JSON-RPC calls.
#[derive(Debug, Clone)]
pub struct ScribeContractInstance {
  // Contract based on public provider.
  contract: ScribeOptimisticInstance<RpcTransportWithRetry, Arc<RetryProviderWithSigner>>,
  // public_provider: Arc<RetryProviderWithSigner>,
  private_provider: Option<Arc<RetryProviderWithSigner>>,
}

impl ScribeContractInstance {
  /// Creates a new ScribeOptimisticInstance
  pub fn new(
    address: Address,
    public_provider: Arc<RetryProviderWithSigner>,
    private_provider: Option<Arc<RetryProviderWithSigner>>,
  ) -> Self {
    let contract = ScribeOptimistic::new(address, public_provider.clone());
    Self {
      contract,
      private_provider,
    }
  }

  // Challenges given `OpPoked` event with given `schnorr_data` using private mempool.
  // Executes `opChallenge` method on the contract and returns transaction hash if everything worked well.
  // NOTE: This method uses `eth_sendPrivateTransaction` to send the transaction, so RPC have to support it.
  async fn challenge_with_private(
    &self,
    schnorr_data: &SchnorrData,
    gas_limit: u64,
  ) -> ContractResult<ChallengePendingTx> {
    let Some(private_provider) = self.private_provider.clone() else {
      return Err(ContractError::MissingPrivateProvider {
        address: self.address().clone(),
      });
    };

    log::debug!(
      "Contract[{:?}]: Challenging OpPoke using private mempool with schnorr_data {:?}",
      self.address(),
      &schnorr_data
    );

    let tx = self
      .contract
      .opChallenge(schnorr_data.clone())
      // TODO: set gas limit properly
      // .gas(gas_limit)
      .into_transaction_request();

    let sendable = self.contract.provider().clone().fill(tx).await?;

    // let Some(builder) = sendable.as_builder() else {
    //   return Err(ContractError::PrivateTransactionBuildError {
    //     address: self.address().clone(),
    //   });
    // };

    // let tx = builder
    //   .clone()
    //   .build(private_provider.wallet())
    //   .await
    //   .map_err(|e| ContractError::PrivateChallengeError {
    //     address: self.address().clone(),
    //     source: e,
    //   })?
    let Some(envelop) = sendable.as_envelope() else {
      return Err(ContractError::PrivateTransactionBuildError {
        address: self.address().clone(),
      });
    };
    let tx = envelop.encoded_2718();

    // Getting the current block number, to calculate the max block number for the transaction.
    let block_number = self.contract.provider().get_block_number().await?;

    // TODO: set max block number properly
    let params = PrivateTransactionRequest {
      tx: tx.into(),
      max_block_number: Some(block_number + MAX_BLOCKS_TO_WAIT),
      preferences: PrivateTransactionPreferences {
        privacy: None,
        validity: None,
      },
    };

    // let rlp_hex = hex::encode_prefixed(encoded_tx)
    let tx_hash = private_provider
      .client()
      .request("eth_sendPrivateTransaction", (params,))
      .await?;

    let pending_tx =
      PendingTransactionBuilder::new(self.contract.provider().root().clone(), tx_hash);

    Ok(pending_tx)
  }

  // Challenges given `OpPoked` event with given `schnorr_data` using public mempool.
  // Executes `opChallenge` method on the contract and returns transaction hash if everything worked well.
  async fn challenge_with_public(
    &self,
    schnorr_data: &SchnorrData,
    gas_limit: u64,
  ) -> ContractResult<ChallengePendingTx> {
    log::debug!(
      "Contract[{:?}]: Challenging OpPoke using public mempool with schnorr_data {:?}",
      self.address(),
      &schnorr_data
    );

    let transaction = self.contract.opChallenge(schnorr_data.clone());
    // TODO: set gas limit properly
    // .gas(gas_limit);

    let tx = transaction
      .send()
      .await
      .map_err(|e| ContractError::AlloyContractError {
        address: self.address().clone(),
        source: e,
      })?;

    Ok(tx)
  }

  /// Validates schnorr signature from given `OpPoked`.
  /// Uses `constructPokeMessage` and `isAcceptableSchnorrSignatureNow` methods from the contract.
  /// Returns true if the signature is valid.
  async fn is_signature_valid(&self, op_poked: ScribeOptimistic::OpPoked) -> ContractResult<bool> {
    log::trace!(
      "Contract[{:?}]: Validating OpPoke signature",
      self.address()
    );

    let message = self
      .contract
      .constructPokeMessage(op_poked.pokeData)
      .call()
      .await
      .map_err(|e| ContractError::AlloyContractError {
        address: self.address().clone(),
        source: e,
      })?
      ._0;

    let acceptable = self
      .contract
      .isAcceptableSchnorrSignatureNow(message, op_poked.schnorrData)
      .call()
      .await
      .map_err(|e| ContractError::AlloyContractError {
        address: self.address().clone(),
        source: e,
      })?
      ._0;

    Ok(acceptable)
  }

  // Gets the timestamp from the block by `event.log.block_number`, if it is missing, returns an error
  async fn get_timestamp_from_block(&self, block_number: u64) -> ContractResult<u64> {
    let Some(block) = self
      .contract
      .provider()
      .get_block(block_number.into(), BlockTransactionsKind::Hashes)
      .await?
    else {
      return Err(ContractError::FailedToFetchBlock(block_number));
    };

    Ok(block.header.timestamp)
  }

  // Checks if the log is stale, i.e. if the event is outside of the challenge period.
  // If the block timestamp is missing, it is fetched from the block number.
  // If the block number is also missing, an error is returned.
  async fn is_log_stale(
    &self,
    log: &Log<ScribeOptimistic::OpPoked>,
    challenge_period: u64,
  ) -> ContractResult<bool> {
    // Check if the poke is within the challenge period
    let event_timestamp = match log.block_timestamp {
      Some(timestamp) => timestamp,
      None => {
        if log.block_number.is_none() {
          return Err(ContractError::NoBlockNumberInLog(log.transaction_hash));
        }

        self
          .get_timestamp_from_block(log.block_number.unwrap())
          .await?
      }
    };

    let current_timestamp = chrono::Utc::now().timestamp() as u64;
    log::debug!(
      "ScribeEventsProcessor[{:?}] OpPoked, event_timestamp: {:?}, current_timestamp: {:?}",
      self.contract.address(),
      event_timestamp,
      current_timestamp
    );

    Ok(current_timestamp - event_timestamp > challenge_period)
  }
}

impl ScribeContract for ScribeContractInstance {
  /// Returns the address of the contract.
  fn address(&self) -> &Address {
    self.contract.address()
  }

  /// Returns challenge period from ScribeOptimistic smart contract deployed to `address`.
  async fn get_challenge_period(&self) -> ContractResult<u16> {
    Ok(
      self
        .contract
        .opChallengePeriod()
        .call()
        .await
        .map_err(|e| ContractError::AlloyContractError {
          address: self.address().clone(),
          source: e,
        })?
        ._0,
    )
  }

  /// Challenges given `OpPoked` event with given `schnorr_data`.
  /// Executes `opChallenge` method on the contract and returns transaction hash if everything worked well.
  async fn challenge(&self, schnorr_data: SchnorrData, gas_limit: u64) -> ContractResult<TxHash> {
    // let pending_tx = match self.challenge_with_private(&schnorr_data, gas_limit).await {
    //   Ok(pending_tx) => pending_tx,
    //   Err(ContractError::MissingPrivateProvider { address }) => {
    //     log::warn!(
    //       "Contract[{:?}]: Private provider is missing, falling back to public provider",
    //       address
    //     );

    //     self.challenge_with_public(&schnorr_data, gas_limit).await?
    //   }
    //   Err(e) => {
    //     log::error!(
    //       "Contract[{:?}]: Error while challenging with private provider: {:?}",
    //       self.address(),
    //       e
    //     );

    //     self.challenge_with_public(&schnorr_data, gas_limit).await?
    //   }
    // };

    let pending_tx = self
      .challenge_with_private(&schnorr_data, gas_limit)
      .await?;

    log::debug!(
      "Contract[{:?}]: Falling back to public provider for challenge",
      self.address()
    );

    let tx_hash = pending_tx
      .with_timeout(Some(TX_CONFIRMATION_TIMEOUT))
      .watch()
      .await
      .map_err(|e| ContractError::PendingTransactionError {
        address: self.address().clone(),
        source: e,
      })?;

    Ok(tx_hash)
  }

  /// Returns true if given `OpPoked` event is challengeable.
  async fn is_op_poke_challangeble(
    &self,
    op_poked_log: &Log<ScribeOptimistic::OpPoked>,
    challenge_period: u64,
  ) -> ContractResult<bool> {
    if self.is_log_stale(&op_poked_log, challenge_period).await? {
      log::trace!(
        "Contract[{:?}]: OpPoke is stale, skipping challenge",
        self.address()
      );
      return Ok(false);
    }

    Ok(!self.is_signature_valid(op_poked_log.data().clone()).await?)
  }

  // /// Returns a new provider (clone) with the same signer.
  // fn get_cloned_provider(&self) -> Arc<RetryProviderWithSigner> {
  //   self.contract.provider().clone()
  // }
}
