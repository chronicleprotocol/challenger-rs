use crate::{contract::ScribeOptimistic, error::ContractError};
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
  type Error = ContractError;

  fn try_from(log: Log) -> std::result::Result<Self, Self::Error> {
    let Some(topic) = log.topic0() else {
      return Err(ContractError::Topic0MissingForLog {
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
      _ => Err(ContractError::UnknownTopic0 {
        topic: topic.to_string(),
        tx_hash: log.transaction_hash,
        address: log.address(),
      }),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_parsing_invalid_logs() {
    let serialized = r#"{
			"address": "0x891e368fe81cba2ac6f6cc4b98e684c106e2ef4f",
			"topics": [
				"0x50a7dd333474394c853a6d334bd7301b1362432233dcf7f04d2b0fc10fd7200d",
				"0x0000000000000000000000001f7acda376ef37ec371235a094113df9cb4efee1"
			],
			"data": "0x0000000000000000000000000000000000000000000000000000000000000000",
			"blockNumber": "0x741c7f",
			"transactionHash": "0x0c3962d7119b674ec328756237fe4aac56c96c3710d469635f1404c196d9e266",
			"transactionIndex": "0x6f",
			"blockHash": "0x60e355d88fae859c1a8e7029147b69add09cf657e1faff0df4d61684195de26d",
			"logIndex": "0xa0",
			"removed": false
		}"#;

    let deserialized: Log = serde_json::from_str(serialized).unwrap();
    let event = Event::try_from(deserialized);

    assert!(matches!(event, Err(ContractError::UnknownTopic0 { .. })));

    let serialized = r#"{
			"address": "0x891e368fe81cba2ac6f6cc4b98e684c106e2ef4f",
			"topics": [],
			"data": "0x0000000000000000000000000000000000000000000000000000000000000000",
			"blockNumber": "0x741c7f",
			"transactionHash": "0x0c3962d7119b674ec328756237fe4aac56c96c3710d469635f1404c196d9e266",
			"transactionIndex": "0x6f",
			"blockHash": "0x60e355d88fae859c1a8e7029147b69add09cf657e1faff0df4d61684195de26d",
			"logIndex": "0xa0",
			"removed": false
		}"#;

    let deserialized: Log = serde_json::from_str(serialized).unwrap();
    let event = Event::try_from(deserialized);

    assert!(matches!(
      event,
      Err(ContractError::Topic0MissingForLog { .. })
    ));
  }

  #[test]
  fn test_op_poked_parses() {
    let serialized = r#"{
				"address": "0x891e368fe81cba2ac6f6cc4b98e684c106e2ef4f",
				"topics": [
					"0xb9dc937c5e394d0c8f76e0e324500b88251b4c909ddc56232df10e2ea42b3c63",
					"0x0000000000000000000000001f7acda376ef37ec371235a094113df9cb4efee1",
					"0x0000000000000000000000002b5ad5c4795c026514f8317c7a215e218dccd6cf"
				],
				"data": "0x0000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000004200000000000000000000000000000000000000000000000000000000679c9c5c5014fdeb8945691eced7992164c71f58912483580b6991637d3f37adf248e910000000000000000000000000e1fdc6d86826238f87bb29e5f7d2731bb2a83641000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000022b68000000000000000000000000000000000000000000000000000000000000",
				"blockNumber": "0x741c7d",
				"transactionHash": "0x53b5497d11e7682526cd996f33d6f20c81cfe3d24d0f459bfe07b6b3661d8a47",
				"transactionIndex": "0x96",
				"blockHash": "0xbc84d639931977426ca4448cec695558905ff5dc49aa82de39cad3c7033da5ce",
				"logIndex": "0x1ac",
				"removed": false
			}"#;

    let deserialized: Log = serde_json::from_str(serialized).unwrap();
    let event = Event::try_from(deserialized).unwrap();

    assert!(matches!(event, Event::OpPoked(_)));
    assert_eq!(event.title(), "OpPoked");
    assert_eq!(
      event.address(),
      "0x891e368fe81cba2ac6f6cc4b98e684c106e2ef4f"
        .parse::<Address>()
        .unwrap()
    );
  }

  #[test]
  fn test_challenge_parses() {
    let serialized = r#"{
			"address": "0x891e368fe81cba2ac6f6cc4b98e684c106e2ef4f",
			"topics": [
				"0xac50cef58b3aef7f7c30349f5e4a342a29d2325a02eafc8dacfdba391e6d5db3",
				"0x0000000000000000000000001f7acda376ef37ec371235a094113df9cb4efee1"
			],
			"data": "0x0000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000002456d7d2e80000000000000000000000003f17f1962b36e491b30a40b2405849e597ba5fb500000000000000000000000000000000000000000000000000000000",
			"blockNumber": "0x741c7f",
			"transactionHash": "0x0c3962d7119b674ec328756237fe4aac56c96c3710d469635f1404c196d9e266",
			"transactionIndex": "0x6f",
			"blockHash": "0x60e355d88fae859c1a8e7029147b69add09cf657e1faff0df4d61684195de26d",
			"logIndex": "0xa1",
			"removed": false
		}"#;

    let deserialized: Log = serde_json::from_str(serialized).unwrap();
    let event = Event::try_from(deserialized).unwrap();

    assert!(matches!(event, Event::OpPokeChallengedSuccessfully(_)));
    assert_eq!(event.title(), "OpPokeChallengedSuccessfully");
    assert_eq!(
      event.address(),
      "0x891e368fe81cba2ac6f6cc4b98e684c106e2ef4f"
        .parse::<Address>()
        .unwrap()
    );
  }
}
