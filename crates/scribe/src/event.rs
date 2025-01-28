use crate::{contract::ScribeOptimistic, error::Error};
use alloy::{primitives::Address, rpc::types::Log, sol_types::SolEvent};

/// Events emitted by the ScribeOptimistic contract.
/// This enum is used to decode logs into specific events.
/// In `challenger` we only care about `OpPoked` and `OpPokeChallengedSuccessfully` events.
#[derive(Debug, Clone)]
pub enum Event {
  OpPoked(Log<ScribeOptimistic::OpPoked>),
  OpPokeChallengedSuccessfully(Log<ScribeOptimistic::OpPokeChallengedSuccessfully>),
}

impl Event {
  pub fn title(&self) -> String {
    match self {
      Self::OpPoked(_) => "OpPoked".to_string(),
      Self::OpPokeChallengedSuccessfully(_) => "OpPokeChallengedSuccessfully".to_string(),
    }
  }

  /// Returns the address of the contract that emitted the event.
  pub fn address(&self) -> Address {
    match self {
      Self::OpPoked(log) => log.address(),
      Self::OpPokeChallengedSuccessfully(log) => log.address(),
    }
  }
}

impl TryFrom<Log> for Event {
  type Error = Error;

  fn try_from(log: Log) -> std::result::Result<Self, Self::Error> {
    let Some(topic) = log.topic0() else {
      return Err(Error::Topic0MissingForLog {
        tx_hash: log.transaction_hash,
        address: log.address(),
      });
    };

    // Match the event by `topic0
    match *topic {
      ScribeOptimistic::OpPoked::SIGNATURE_HASH => Ok(Self::OpPoked(log.log_decode()?)),
      ScribeOptimistic::OpPokeChallengedSuccessfully::SIGNATURE_HASH => {
        Ok(Self::OpPokeChallengedSuccessfully(log.log_decode()?))
      }
      _ => Err(Error::UnknownTopic0 {
        topic: topic.to_string(),
        tx_hash: log.transaction_hash,
        address: log.address(),
      }),
    }
  }
}
