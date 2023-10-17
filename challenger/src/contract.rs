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

use async_trait::async_trait;
use std::{fmt::Debug, sync::Arc};

use ethers::{
    contract::{abigen, Contract, LogMeta},
    providers::Middleware,
    types::{Address, Block, TransactionReceipt, ValueOrArray, H256, U64},
};
use eyre::{Result, WrapErr};
use log::debug;

// Yes it generates smart contract code based on given ABI
abigen!(ScribeOptimistic, "./abi/ScribeOptimistic.json");

#[async_trait]
pub trait ScribeOptimisticProvider: Send + Sync {
    /// Returns the latest block number from RPC.
    async fn get_latest_block_number(&self) -> Result<U64>;

    /// Returns block details with given `block_number` from RPC.
    async fn get_block(&self, block_number: U64) -> Result<Option<Block<H256>>>;

    /// Returns challenge period from ScribeOptimistic smart contract deployed to `address`.
    async fn get_challenge_period(&self, address: Address) -> Result<u16>;

    /// Returns list of `OpPokeChallengedSuccessfully` events and log metadata in between `from_block` and `to_block`
    /// from smart contract deployed to `address`.
    async fn get_successful_challenges(
        &self,
        address: Address,
        from_block: U64,
        to_block: U64,
    ) -> Result<Vec<(OpPokeChallengedSuccessfullyFilter, LogMeta)>>;

    /// Returns list of `OpPoked` events and log metadata in between `from_block` and `to_block` from smart contract
    /// deployed to `address`.
    async fn get_op_pokes(
        &self,
        address: Address,
        from_block: U64,
        to_block: U64,
    ) -> Result<Vec<(OpPokedFilter, LogMeta)>>;

    /// Returns true if given `OpPoked` schnorr signature is valid.
    async fn is_schnorr_signature_valid(&self, op_poked: OpPokedFilter) -> Result<bool>;

    /// Challenges given `OpPoked` event.
    async fn challenge(&self, schnorr_data: SchnorrData) -> Result<Option<TransactionReceipt>>;
}

#[derive(Debug)]
pub struct HttpScribeOptimisticProvider<M> {
    address: Address,
    client: Arc<M>,
    contract: ScribeOptimistic<M>,
}

impl<M: Middleware> HttpScribeOptimisticProvider<M> {
    /// Creates new instance of `HttpScribeOptimisticProvider` under given `client` and `address`.
    pub fn new(address: Address, client: Arc<M>) -> Self {
        let contract = ScribeOptimistic::new(address, client.clone());
        Self {
            address,
            client,
            contract,
        }
    }
}

#[async_trait]
impl<M: Middleware> ScribeOptimisticProvider for HttpScribeOptimisticProvider<M>
where
    M: 'static,
    M::Error: 'static,
{
    /// Returns the latest block number from RPC.
    async fn get_latest_block_number(&self) -> Result<U64> {
        self.client
            .get_block_number()
            .await
            .wrap_err("Failed to get latest block number")
    }

    /// Returns block details with given `block_number` from RPC.
    async fn get_block(&self, block_number: U64) -> Result<Option<Block<H256>>> {
        self.client
            .get_block(block_number)
            .await
            .wrap_err("Failed to get block details")
    }

    /// Returns challenge period from ScribeOptimistic smart contract deployed to `address`.
    async fn get_challenge_period(&self, address: Address) -> Result<u16> {
        debug!("Address {:?}: Getting challenge period", address);

        self.contract
            .op_challenge_period()
            .call()
            .await
            .wrap_err("Failed to get challenge period")
    }

    /// Returns list of `OpPokeChallengedSuccessfully` events and log metadata in between `from_block` and `to_block`
    /// from smart contract deployed to `address`.
    async fn get_successful_challenges(
        &self,
        address: Address,
        from_block: U64,
        to_block: U64,
    ) -> Result<Vec<(OpPokeChallengedSuccessfullyFilter, LogMeta)>> {
        debug!(
            "Address {:?}, searching OpPokeChallengedSuccessfully events from block {:?} to block {:?}",
            address, from_block, to_block
        );
        let event =
            Contract::event_of_type::<OpPokeChallengedSuccessfullyFilter>(self.client.clone())
                .address(ValueOrArray::Array(vec![address]))
                .from_block(from_block)
                .to_block(to_block);

        event.query_with_meta().await.wrap_err(format!(
            "Address {:?}: Failed to get OpPokeChallengedSuccessfully events from block {:?} to block {:?}",
            address, from_block, to_block
        ))
    }

    /// Returns list of `OpPoked` events and log metadata in between `from_block` and `to_block` from smart contract
    /// deployed to `address`.
    async fn get_op_pokes(
        &self,
        address: Address,
        from_block: U64,
        to_block: U64,
    ) -> Result<Vec<(OpPokedFilter, LogMeta)>> {
        debug!(
            "Address {:?}, searching OpPoked events from block {:?} to block {:?}",
            address, from_block, to_block
        );
        let event = Contract::event_of_type::<OpPokedFilter>(self.client.clone())
            .address(ValueOrArray::Array(vec![address]))
            .from_block(from_block)
            .to_block(to_block);

        event.query_with_meta().await.wrap_err(format!(
            "Address {:?}: Failed to get OpPoked events from block {:?} to block {:?}",
            address, from_block, to_block
        ))
    }

    /// Returns true if given `OpPoked` schnorr signature is valid.
    /// Validation process is based on given `OpPoked` event.
    /// Validation logic described in here: https://github.com/chronicleprotocol/scribe/blob/main/docs/Scribe.md#verifying-optimistic-pokes
    async fn is_schnorr_signature_valid(&self, op_poked: OpPokedFilter) -> Result<bool> {
        debug!(
            "Address {:?}: Validating schnorr signature for {:?}",
            self.address, op_poked
        );

        let message = self
            .contract
            .construct_poke_message(op_poked.poke_data)
            .call()
            .await?;

        self.contract
            .is_acceptable_schnorr_signature_now(message, op_poked.schnorr_data)
            .call()
            .await
            .wrap_err("Failed to validate schnorr signature for OpPoked event")
    }

    /// Challenges given `OpPoked` event.
    /// Executes `opChallenge` function from ScribeOptimistic smart contract with shnorr signature
    /// taken from `OpPoked` event.
    /// NOTE: You have to validate if schnorr signature is INVALID before calling this function !
    async fn challenge(&self, schnorr_data: SchnorrData) -> Result<Option<TransactionReceipt>> {
        debug!(
            "Address {:?}: Challenging schnorr data {:?}",
            self.address, schnorr_data
        );

        self.contract
            .op_challenge(schnorr_data)
            .send()
            .await?
            .await
            .wrap_err("Failed to challenge OpPoked event")
    }
}
