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
    primitives::{Address, LogData},
    rpc::types::Log,
    sol,
    sol_types::SolEvent,
};
use eyre::{bail, Result, WrapErr};

use crate::events_listener::RetryProviderWithSigner;

// Generate the contract bindings for the ScribeOptimistic contract
sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    ScribeOptimistic,
    "abi/ScribeOptimistic.json"
}

impl Debug for IScribe::SchnorrData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchnorrData")
            .field("signature", &self.signature)
            .field("commitment", &self.commitment)
            .field("signersBlob", &self.signersBlob)
            .finish()
    }
}

impl Debug for IScribe::PokeData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PokeData")
            .field("val", &self.val)
            .field("age", &self.age)
            .finish()
    }
}

impl Debug for ScribeOptimistic::OpPoked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpPoked")
            .field("caller", &self.caller)
            .field("opFeed", &self.opFeed)
            .field("schnorrData", &self.schnorrData)
            .field("pokeData", &self.pokeData)
            .finish()
    }
}

impl Debug for ScribeOptimistic::OpPokeChallengedSuccessfully {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpPokeChallengedSuccessfully")
            .field("caller", &self.caller)
            .field("schnorrErr", &self.schnorrErr)
            .finish()
    }
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
    event: Event,
    log: Log,
    address: Address,
}

impl EventWithMetadata {
    /// Creates a new EventWithMetadata from a Log
    pub fn from_log(log: Log) -> Result<Self> {
        let event = Event::from_log(log.clone())?;
        let address = log.address();

        Ok(Self {
            event,
            log,
            address,
        })
    }
}

pub struct ScribeOptimisticInstance {
    address: Address,
    provider: Arc<RetryProviderWithSigner>,
}
