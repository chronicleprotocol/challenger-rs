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

use alloy::{
  primitives::{Address, TxHash},
  providers::PendingTransactionError,
  transports::{RpcError, TransportErrorKind},
};

/// Dynamic contract result type.
pub type ContractResult<T, E = ContractError> = core::result::Result<T, E>;

/// Error when interacting with Scribe library.
#[derive(thiserror::Error, Debug)]
pub enum ContractError {
  /// Failed to parse event from log, no `topic0` exist.
  #[error("missing `topic0` from log under tx {tx_hash:?} for address {address:?}")]
  Topic0MissingForLog {
    tx_hash: Option<TxHash>,
    address: Address,
  },

  /// Unneeded event received from log.
  #[error("unknown `topic0` {topic} for log under tx {tx_hash:?} for address {address:?}")]
  UnknownTopic0 {
    topic: String,
    tx_hash: Option<TxHash>,
    address: Address,
  },

  /// TODO: check if it used only for log decode, need rename
  #[error(transparent)]
  AlloySolTypesError(#[from] alloy::sol_types::Error),

  // TODO: tbd
  #[error("failed to execute contract method on address {address:?}: {source}")]
  AlloyContractError {
    address: Address,
    #[source]
    source: alloy::contract::Error,
  },

  // TODO: tbd
  #[error("failed to ")]
  PendingTransactionError {
    address: Address,
    #[source]
    source: PendingTransactionError,
  },
}

/// Dynamic event processor result type.
pub type ProcessorResult<T, E = ProcessorError> = core::result::Result<T, E>;

/// Error when processing events.
#[derive(thiserror::Error, Debug)]
pub enum ProcessorError {
  #[error("contract execution failed with error: {0}")]
  ContractError(#[from] ContractError),

  #[error("RPC transport error: {0}")]
  RpcError(#[from] RpcError<TransportErrorKind>),

  #[error("missing block number in log for transaction {0:?}")]
  NoBlockNumberInLog(Option<TxHash>),

  #[error("failed to fetch block with number {0}")]
  FailedToFetchBlock(u64),

  #[error("failed to execute challenge on address {address:?}: {source}")]
  ChallengeError {
    address: Address,
    #[source]
    source: ContractError,
  },

  #[error("address {address:?} has exhausted all {attempt} attempts to challenge OpPoke")]
  ChallengeAttemptsExhausted { address: Address, attempt: u16 },

  #[error("address {address:?} challenge cancelled after attempt: {attempt}")]
  ChallengeCancelled { address: Address, attempt: u16 },
}
