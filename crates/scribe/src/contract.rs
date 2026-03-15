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
  fn is_op_poke_challengeable(
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
pub struct ScribeContractInstance<P: Provider> {
  // Contract based on public provider.
  contract: ScribeOptimisticInstance<P>,
  // public_provider: Arc<RetryProviderWithSigner>,
  private_provider: Option<Arc<FullHTTPRetryProviderWithSigner>>,
  signer_address: Address,
}

impl<P: Provider> ScribeContractInstance<P> {
  /// Creates a new ScribeOptimisticInstance
  pub fn new(
    address: Address,
    public_provider: P,
    private_provider: Option<Arc<FullHTTPRetryProviderWithSigner>>,
    signer_address: Address,
  ) -> Self {
    Self {
      contract: ScribeOptimistic::new(address, public_provider),
      private_provider,
      signer_address,
    }
  }

  /// Helper method to wrap contract errors with address context
  fn wrap_contract_error<T>(&self, result: Result<T, alloy::contract::Error>) -> ContractResult<T> {
    result.map_err(|e| ContractError::AlloyContractError {
      address: *self.address(),
      source: e,
    })
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
        let block_number = log
          .block_number
          .ok_or(ContractError::NoBlockNumberInLog(log.transaction_hash))?;
        self.get_timestamp_from_block(block_number).await?
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

    let message = self.wrap_contract_error(
      self
        .contract
        .constructPokeMessage(op_poked.pokeData)
        .call()
        .await,
    )?;

    let acceptable = self.wrap_contract_error(
      self
        .contract
        .isAcceptableSchnorrSignatureNow(message, op_poked.schnorrData)
        .call()
        .await,
    )?;

    Ok(acceptable)
  }

  async fn challenge_with_public(&self, schnorr_data: &SchnorrData) -> ContractResult<TxHash> {
    log::info!(
      "Contract[{:?}]: Challenging OpPoke using public mempool with schnorr_data {:?}",
      self.address(),
      &schnorr_data
    );

    // Explicitly fetch the pending nonce to avoid the stale "latest" nonce returned by
    // NonceFiller when the private tx was already included by Flashbots.
    let nonce = self
      .contract
      .provider()
      .get_transaction_count(self.signer_address)
      .pending()
      .await?;

    log::debug!(
      "Contract[{:?}]: Using pending nonce {} for public fallback tx",
      self.address(),
      nonce
    );

    let tx = self
      .wrap_contract_error(
        self
          .contract
          .opChallenge(schnorr_data.clone())
          .nonce(nonce)
          .send()
          .await,
      )?
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

    // Save the hash BEFORE consuming pending_tx into watch()
    let private_tx_hash = *pending_tx.tx_hash();

    let watch_result = pending_tx
      .with_timeout(Some(TX_CONFIRMATION_TIMEOUT))
      .watch()
      .await;

    match watch_result {
      Ok(tx_hash) => Ok(tx_hash),
      Err(e) => {
        // TxWatcher(Timeout) does NOT guarantee the tx was dropped — Flashbots may have
        // included it. Fetch the receipt before falling back to the public mempool.
        log::warn!(
          "Contract[{:?}]: Private tx watch failed ({:?}), checking receipt for {:?}",
          self.address(),
          e,
          private_tx_hash
        );
        match private_provider
          .get_transaction_receipt(private_tx_hash)
          .await
        {
          Ok(Some(_receipt)) => {
            log::info!(
              "Contract[{:?}]: Private tx confirmed despite watch error, skipping fallback",
              self.address()
            );
            Ok(private_tx_hash)
          }
          _ => Err(ContractError::PendingTransactionError {
            address: *self.address(),
            source: e,
          }),
        }
      }
    }
  }
}

impl<P: Provider> ScribeContract for ScribeContractInstance<P> {
  /// Returns the address of the contract.
  fn address(&self) -> &Address {
    self.contract.address()
  }

  /// Returns challenge period from ScribeOptimistic smart contract deployed to `address`.
  async fn get_challenge_period(&self) -> ContractResult<u16> {
    self.wrap_contract_error(self.contract.opChallengePeriod().call().await)
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
  async fn is_op_poke_challengeable(
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
    let contract =
      ScribeContractInstance::new(Address::random(), provider.clone(), None, Address::ZERO);

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

  #[tokio::test]
  async fn test_is_log_stale_boundary_exactly_at_period() {
    let provider = new_provider("http://localhost:8545");
    let contract =
      ScribeContractInstance::new(Address::random(), provider.clone(), None, Address::ZERO);

    // Event age exactly equals challenge period → should NOT be stale (uses > not >=)
    let now = chrono::Utc::now().timestamp() as u64;
    let challenge_period = 100;
    let log = Log {
      block_number: Some(1),
      block_timestamp: Some(now - challenge_period),
      ..Default::default()
    };

    assert!(
      !contract.is_log_stale(&log, challenge_period).await.unwrap(),
      "Event exactly at challenge period boundary should NOT be stale (> not >=)"
    );

    // One second past → should be stale
    let log = Log {
      block_number: Some(1),
      block_timestamp: Some(now - challenge_period - 1),
      ..Default::default()
    };
    assert!(
      contract.is_log_stale(&log, challenge_period).await.unwrap(),
      "Event one second past challenge period should be stale"
    );
  }

  #[tokio::test]
  async fn test_is_log_stale_with_block_timestamp_none_and_no_block_number() {
    let provider = new_provider("http://localhost:8545");
    let contract =
      ScribeContractInstance::new(Address::random(), provider.clone(), None, Address::ZERO);

    // Log with block_timestamp: None and block_number: None → should return NoBlockNumberInLog error
    let log = Log {
      block_number: None,
      block_timestamp: None,
      ..Default::default()
    };

    let result = contract.is_log_stale(&log, 100).await;
    assert!(result.is_err());
    assert!(
      matches!(result.unwrap_err(), ContractError::NoBlockNumberInLog(..)),
      "Should return NoBlockNumberInLog error when both timestamp and block number are None"
    );
  }

  #[tokio::test]
  async fn test_is_log_stale_with_block_timestamp_none_fetches_block() {
    use alloy::{providers::ProviderBuilder, transports::mock::Asserter};

    let asserter = Asserter::new();
    asserter.push_failure_msg("no such block");
    let provider = ProviderBuilder::new().connect_mocked_client(asserter);
    let contract = ScribeContractInstance::new(Address::random(), provider, None, Address::ZERO);

    // Log with block_timestamp: None but valid block_number → tries to fetch block via RPC
    let log = Log {
      block_number: Some(1),
      block_timestamp: None,
      ..Default::default()
    };

    let result = contract.is_log_stale(&log, 100).await;
    assert!(result.is_err(), "Should fail trying to fetch block");
  }

  #[tokio::test]
  async fn test_is_op_poke_challengeable_stale_returns_false() {
    let provider = new_provider("http://localhost:8545");
    let contract =
      ScribeContractInstance::new(Address::random(), provider.clone(), None, Address::ZERO);

    // Fresh log with old timestamp + short challenge period → stale → returns Ok(false)
    // without ever calling signature validation
    let old_timestamp = chrono::Utc::now().timestamp() as u64 - 1000;
    let log = Log {
      block_number: Some(1),
      block_timestamp: Some(old_timestamp),
      ..Default::default()
    };

    // Challenge period of 1 second means the event (1000s old) is definitely stale
    let result = contract.is_op_poke_challengeable(&log, 1).await;
    assert!(result.is_ok());
    assert!(
      !result.unwrap(),
      "Stale event should return false without calling signature validation"
    );
  }
}
