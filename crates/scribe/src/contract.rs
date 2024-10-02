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
    primitives::{Address, FixedBytes, LogData}, providers::Provider, rpc::types::{Log, TransactionRequest}, sol, sol_types::SolEvent
};
use eyre::{bail, Result, WrapErr};
use IScribe::SchnorrData;
use ScribeOptimistic::{OpPoked, ScribeOptimisticInstance};

use crate::events_listener::{RetryProviderWithSigner, RpcRetryProvider};

// Generate the contract bindings for the ScribeOptimistic contract
sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    #[derive(Debug)]
    ScribeOptimistic,
    "abi/ScribeOptimistic.json"
}

// Decode a log into a specific event
// Example:
// ```rust
// let event = decode_log::<ScribeOptimistic::OpPoked>(&log)?;
// ```
fn decode_log<E: SolEvent>(log: &Log) -> Result<E> {
    let log_data: &LogData = log.as_ref();

    E::decode_raw_log(log_data.topics().iter().copied(), &log_data.data, false)
        .wrap_err_with(|| "Failed to decode log")
}

#[derive(Debug)]
pub enum Event {
    OpPoked(ScribeOptimistic::OpPoked),
    OpPokeChallengedSuccessfully(ScribeOptimistic::OpPokeChallengedSuccessfully),
}

impl Event {
    /// Creates a new GeneralPokedEvent from a Log
    pub fn from_log(log: Log) -> Result<Self> {
        let Some(topic) = log.topic0() else {
            bail!("No topic found in log for tx {:?}", log.transaction_hash)
        };

        match *topic {
            ScribeOptimistic::OpPoked::SIGNATURE_HASH => {
                let event = decode_log::<ScribeOptimistic::OpPoked>(&log)?;
                Ok(Self::OpPoked(event))
            }
            ScribeOptimistic::OpPokeChallengedSuccessfully::SIGNATURE_HASH => {
                let event = decode_log::<ScribeOptimistic::OpPokeChallengedSuccessfully>(&log)?;
                Ok(Self::OpPokeChallengedSuccessfully(event))
            }
            _ => bail!("Unknown event {:#x}", topic),
        }
    }
}

#[derive(Debug)]
pub struct EventWithMetadata {
    pub event: Event,
    log: Log,
    pub address: Address,
}

impl EventWithMetadata {
    /// Creates a new EventWithMetadata from a Log
    pub fn from_log(log: Log) -> Result<Self> {
        let event: Event = Event::from_log(log.clone())?;
        let address = log.address();

        Ok(Self {
            event,
            log,
            address,
        })
    }
}

#[allow(async_fn_in_trait)]
pub trait ScribeOptimisticProvider {
    /// Returns challenge period from ScribeOptimistic smart contract deployed to `address`.
    async fn get_challenge_period(&self) -> Result<u16>;

    /// Returns true if given `OpPoked` schnorr signature is valid.
    async fn is_schnorr_signature_valid(&self, op_poked: OpPoked) -> Result<bool>;

    /// Challenges given `OpPoked` event.
    async fn challenge(&self, schnorr_data: SchnorrData) -> Result<FixedBytes<32>>;

    /// Returns the address of the contract.
    fn address(&self) -> &Address;

     /// Returns a new provider with the same signer.
    fn get_new_provider(&self) -> Arc<RetryProviderWithSigner>;
}

#[derive(Debug, Clone)]
pub struct ScribeOptimisticProviderInstance {
    pub contract: ScribeOptimisticInstance<RpcRetryProvider, Arc<RetryProviderWithSigner>>,
}

impl ScribeOptimisticProviderInstance {
    /// Creates a new ScribeOptimisticInstance
    pub fn new(address: Address, provider: Arc<RetryProviderWithSigner>) -> Self {
        let contract = ScribeOptimistic::new(address, provider.clone());
        Self {
            contract,
        }
    }
}

impl ScribeOptimisticProvider for ScribeOptimisticProviderInstance {
    async fn get_challenge_period(&self) -> Result<u16> {
        Ok(self.contract.opChallengePeriod().call().await?._0)
    }

    async fn is_schnorr_signature_valid(&self, op_poked: OpPoked) -> Result<bool> {
        log::trace!("{:?} Validating OpPoke signature", self.contract.address());

        let message = self
            .contract
            .constructPokeMessage(op_poked.pokeData)
            .call()
            .await?
            ._0;

        let acceptable = self
            .contract
            .isAcceptableSchnorrSignatureNow(message, op_poked.schnorrData)
            .call()
            .await?
            ._0;

        Ok(acceptable)
    }

    async fn challenge(&self, schnorr_data: SchnorrData) -> Result<FixedBytes<32>> {
        log::debug!("{:?} Challenging OpPoke", self.contract.address());
        let from_address = self.contract.address();
        log::info!("Challenging from address: {:?}", from_address);
        let transaction = self.contract
        .opChallenge(schnorr_data)
        // TODO set gas limit properly
        .gas(200000);
        transaction.send()
            .await?
            .watch()
            .await
            .wrap_err("Failed to challenge")
    }

    fn address(&self) -> &Address {
        self.contract.address()
    }

    fn get_new_provider(&self) -> Arc<RetryProviderWithSigner> {
        self.contract.provider().clone()
    }

}
