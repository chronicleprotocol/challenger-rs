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
use mockall::{mock, predicate::*};

use challenger_lib::contract::{
    OpPokeChallengedSuccessfullyFilter, OpPokedFilter, SchnorrData, ScribeOptimisticProvider,
};

use ethers::{
    contract::LogMeta,
    types::{Address, Block, TransactionReceipt, H256, U64},
};
use eyre::Result;

mock! {
    pub TestScribe{}

    #[async_trait]
    impl ScribeOptimisticProvider for TestScribe {
        async fn get_latest_block_number(&self) -> Result<U64>;

        async fn get_block(&self, block_number: U64) -> Result<Option<Block<H256>>>;

        async fn get_challenge_period(&self, address: Address) -> Result<u16>;

        async fn get_successful_challenges(
            &self,
            address: Address,
            from_block: U64,
            to_block: U64,
        ) -> Result<Vec<(OpPokeChallengedSuccessfullyFilter, LogMeta)>>;

        async fn get_op_pokes(
            &self,
            address: Address,
            from_block: U64,
            to_block: U64,
        ) -> Result<Vec<(OpPokedFilter, LogMeta)>>;

        async fn is_schnorr_signature_valid(&self, op_poked: OpPokedFilter) -> Result<bool>;

        async fn challenge(&self, schnorr_data: SchnorrData) -> Result<Option<TransactionReceipt>>;
    }
}

#[test]
fn test_max_failures() {}
