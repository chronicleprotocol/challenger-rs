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
  primitives::{Address, TxHash},
  providers::Provider,
  rpc::types::Log,
  sol,
};

use crate::{
  error::{ContractError, ContractResult},
  provider::FullHTTPRetryProviderWithSigner,
};
use IScribe::SchnorrData;
use ScribeOptimistic::ScribeOptimisticInstance;

// Generate the contract bindings for the ScribeOptimistic contract
sol! {
  #[allow(missing_docs)]
  #[sol(rpc)]
  #[derive(Debug, Default)]
  ScribeOptimistic,
  "abi/ScribeOptimistic.json"
}

// Confirmation timeout for the transaction
const TX_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(300);

/// This trait provides methods required for `challenger` to interact with the ScribeOptimistic smart contract.
pub trait ScribeContract: Send + Sync {
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
  ) -> impl Future<Output = ContractResult<TxHash>> + Send;
}

/// ScribeOptimisticProviderInstance is a real implementation of ScribeOptimisticProvider based on raw JSON-RPC calls.
#[derive(Debug, Clone)]
pub struct ScribeContractInstance {
  // Contract based on public provider.
  contract: ScribeOptimisticInstance<Arc<FullHTTPRetryProviderWithSigner>>,
  // public_provider: Arc<RetryProviderWithSigner>,
  private_provider: Option<Arc<FullHTTPRetryProviderWithSigner>>,
}

impl ScribeContractInstance {
  /// Creates a new ScribeOptimisticInstance
  pub fn new(
    address: Address,
    public_provider: Arc<FullHTTPRetryProviderWithSigner>,
    private_provider: Option<Arc<FullHTTPRetryProviderWithSigner>>,
  ) -> Self {
    Self {
      contract: ScribeOptimistic::new(address, public_provider),
      private_provider,
    }
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

  // Gets the timestamp from the block by `event.log.block_number`, if it is missing, returns an error
  async fn get_timestamp_from_block(&self, block_number: u64) -> ContractResult<u64> {
    let Some(block) = self
      .contract
      .provider()
      .get_block(block_number.into())
      .await?
    else {
      return Err(ContractError::FailedToFetchBlock(block_number));
    };

    Ok(block.header.timestamp)
  }

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
        address: *self.address(),
        source: e,
      })?;

    let acceptable = self
      .contract
      .isAcceptableSchnorrSignatureNow(message, op_poked.schnorrData)
      .call()
      .await
      .map_err(|e| ContractError::AlloyContractError {
        address: *self.address(),
        source: e,
      })?;

    Ok(acceptable)
  }

  async fn challenge_with_public(&self, schnorr_data: &SchnorrData) -> ContractResult<TxHash> {
    log::info!(
      "Contract[{:?}]: Challenging OpPoke using public mempool with schnorr_data {:?}",
      self.address(),
      &schnorr_data
    );

    let tx = self
      .contract
      .opChallenge(schnorr_data.clone())
      .send()
      .await
      .map_err(|e| ContractError::AlloyContractError {
        address: *self.address(),
        source: e,
      })?
      .with_timeout(Some(TX_CONFIRMATION_TIMEOUT))
      .watch()
      .await
      .map_err(|e| ContractError::PendingTransactionError {
        address: *self.address(),
        source: e,
      })?;

    Ok(tx)
  }

  // black magic to build a transaction for flashbots rpc...
  async fn challenge_with_flashbots(&self, schnorr_data: &SchnorrData) -> ContractResult<TxHash> {
    let Some(private_provider) = self.private_provider.clone() else {
      return Err(ContractError::MissingPrivateProvider {
        address: *self.address(),
      });
    };

    log::info!(
      "Contract[{:?}]: Challenging OpPoke using private mempool with schnorr_data {:?}",
      self.address(),
      &schnorr_data
    );

    let tx = self
      .contract
      .opChallenge(schnorr_data.clone())
      .into_transaction_request();

    let sendable = private_provider.fill(tx).await?;

    let Some(envelop) = sendable.as_envelope() else {
      return Err(ContractError::PrivateTransactionBuildError {
        address: *self.address(),
      });
    };
    let tx = envelop.encoded_2718();

    // Send the raw transaction. The transaction is sent to the Flashbots relay and, if valid, will
    // be included in a block by a Flashbots builder.
    let pending_tx = private_provider.send_raw_transaction(&tx).await?;

    log::info!(
      "Contract[{:?}]: Sent private transaction with hash: {:?}",
      self.address(),
      pending_tx.tx_hash()
    );

    let tx_hash = pending_tx
      .with_timeout(Some(TX_CONFIRMATION_TIMEOUT))
      .watch()
      .await
      .map_err(|e| ContractError::PendingTransactionError {
        address: *self.address(),
        source: e,
      })?;

    Ok(tx_hash)
  }
}

impl ScribeContract for ScribeContractInstance {
  /// Returns the address of the contract.
  fn address(&self) -> &Address {
    self.contract.address()
  }

  /// Returns challenge period from ScribeOptimistic smart contract deployed to `address`.
  async fn get_challenge_period(&self) -> ContractResult<u16> {
    self
      .contract
      .opChallengePeriod()
      .call()
      .await
      .map_err(|e| ContractError::AlloyContractError {
        address: *self.address(),
        source: e,
      })
  }

  /// Challenges given `OpPoked` event with given `schnorr_data`.
  /// Executes `opChallenge` method on the contract and returns transaction hash if everything worked well.
  async fn challenge(&self, schnorr_data: SchnorrData) -> ContractResult<TxHash> {
    match self.challenge_with_flashbots(&schnorr_data).await {
      Ok(tx_hash) => return Ok(tx_hash),
      Err(ContractError::MissingPrivateProvider { address }) => {
        log::warn!(
          "Contract[{:?}]: Private provider is missing, falling back to public provider",
          address
        );
      }
      Err(e) => {
        log::error!(
          "Contract[{:?}]: Error while challenging with private provider: {:?}",
          self.address(),
          e
        );
      }
    };

    log::info!(
      "Contract[{:?}]: Falling back to public provider for challenge",
      self.address()
    );

    self.challenge_with_public(&schnorr_data).await
  }

  /// Returns true if given `OpPoked` event is challengeable.
  async fn is_op_poke_challangeble(
    &self,
    op_poked_log: &Log<ScribeOptimistic::OpPoked>,
    challenge_period: u64,
  ) -> ContractResult<bool> {
    if self.is_log_stale(op_poked_log, challenge_period).await? {
      log::trace!(
        "Contract[{:?}]: OpPoke is stale, skipping challenge",
        self.address()
      );
      return Ok(false);
    }

    Ok(!self.is_signature_valid(op_poked_log.data().clone()).await?)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::provider::new_provider;

  #[tokio::test]
  async fn test_is_log_stale() {
    let provider = new_provider("http://localhost:8545");
    let contract = ScribeContractInstance::new(Address::random(), provider.clone(), None);

    let now = chrono::Utc::now().timestamp() as u64;
    let log = Log {
      block_number: Some(1),
      block_timestamp: Some(now - 100),
      ..Default::default()
    };

    assert!(contract.is_log_stale(&log, 1).await.unwrap());

    let log = Log {
      block_number: Some(1),
      block_timestamp: Some(now - 50),
      ..Default::default()
    };
    assert!(!contract.is_log_stale(&log, 100).await.unwrap());

    // empty block number returns error
    let log = Log {
      block_number: None,
      block_timestamp: None,
      ..Default::default()
    };
    assert!(contract.is_log_stale(&log, 100).await.is_err());
  }
}
