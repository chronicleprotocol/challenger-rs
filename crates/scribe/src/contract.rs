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

use std::{fmt::Debug, sync::Arc};

use alloy::{
  primitives::{Address, FixedBytes},
  sol,
  transports::{
    http::{Client, Http},
    layers::RetryBackoffService,
  },
};
// use eyre::{bail, Result, WrapErr};

use crate::{
  error::{Error, Result},
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

/// The RPC transport type used to interact with the Ethereum network.
pub type RpcTransportWithRetry = RetryBackoffService<Http<Client>>;

/// This trait provides methods required for `challenger` to interact with the ScribeOptimistic smart contract.
#[allow(async_fn_in_trait)]
pub(crate) trait ScribeContract {
  /// Returns challenge period from ScribeOptimistic smart contract deployed to `address`.
  /// NOTE: From time to time challenger might need to refresh this value, because it might be changed by the contract owner.
  async fn get_challenge_period(&self) -> Result<u16>;

  /// Returns true if schnorr signature of given `OpPoked` event is valid.
  async fn is_signature_valid(&self, op_poked: ScribeOptimistic::OpPoked) -> Result<bool>;

  /// Challenges given `OpPoked` event with given `schnorr_data`.
  /// See: `IScribeOptimistic::opChallenge(SchnorrData calldata schnorrData)` for more details.
  async fn challenge(&self, schnorr_data: SchnorrData, gas_limit: u64) -> Result<FixedBytes<32>>;

  /// Returns the address of the contract.
  fn address(&self) -> &Address;

  // /// Returns a new provider with the same signer.
  // fn get_cloned_provider(&self) -> Arc<RetryProviderWithSigner>;
}

/// ScribeOptimisticProviderInstance is a real implementation of ScribeOptimisticProvider based on raw JSON-RPC calls.
#[derive(Debug, Clone)]
pub(crate) struct ScribeContractInstance {
  pub contract: ScribeOptimisticInstance<RpcTransportWithRetry, Arc<RetryProviderWithSigner>>,
}

impl ScribeContractInstance {
  /// Creates a new ScribeOptimisticInstance
  pub fn new(address: Address, provider: Arc<RetryProviderWithSigner>) -> Self {
    let contract = ScribeOptimistic::new(address, provider.clone());
    Self { contract }
  }
}

impl ScribeContract for ScribeContractInstance {
  /// Returns challenge period from ScribeOptimistic smart contract deployed to `address`.
  async fn get_challenge_period(&self) -> Result<u16> {
    Ok(
      self
        .contract
        .opChallengePeriod()
        .call()
        .await
        .map_err(|e| Error::AlloyContractError {
          address: self.address().clone(),
          source: e,
        })?
        ._0,
    )
  }

  /// Validates schnorr signature from given `OpPoked`.
  /// Uses `constructPokeMessage` and `isAcceptableSchnorrSignatureNow` methods from the contract.
  /// Returns true if the signature is valid.
  async fn is_signature_valid(&self, op_poked: ScribeOptimistic::OpPoked) -> Result<bool> {
    log::trace!("Contract {:?}: Validating OpPoke signature", self.address());

    let message = self
      .contract
      .constructPokeMessage(op_poked.pokeData)
      .call()
      .await
      .map_err(|e| Error::AlloyContractError {
        address: self.address().clone(),
        source: e,
      })?
      ._0;

    let acceptable = self
      .contract
      .isAcceptableSchnorrSignatureNow(message, op_poked.schnorrData)
      .call()
      .await
      .map_err(|e| Error::AlloyContractError {
        address: self.address().clone(),
        source: e,
      })?
      ._0;

    Ok(acceptable)
  }

  /// Challenges given `OpPoked` event with given `schnorr_data`.
  /// Executes `opChallenge` method on the contract and returns transaction hash if everything worked well.
  async fn challenge(&self, schnorr_data: SchnorrData, gas_limit: u64) -> Result<FixedBytes<32>> {
    log::warn!(
      "Contract {:?}: Challenging OpPoke with schnorr_data {:?}",
      self.address(),
      &schnorr_data
    );

    let transaction = self
      .contract
      .opChallenge(schnorr_data.clone())
      // TODO: set gas limit properly
      .gas(gas_limit);

    let tx_hash = transaction
      .send()
      .await
      .map_err(|e| Error::AlloyContractError {
        address: self.address().clone(),
        source: e,
      })?
      .watch()
      .await
      .map_err(|e| Error::PendingTransactionError {
        address: self.address().clone(),
        source: e,
      })?;

    Ok(tx_hash)
  }

  /// Returns the address of the contract.
  fn address(&self) -> &Address {
    self.contract.address()
  }

  // /// Returns a new provider (clone) with the same signer.
  // fn get_cloned_provider(&self) -> Arc<RetryProviderWithSigner> {
  //   self.contract.provider().clone()
  // }
}
