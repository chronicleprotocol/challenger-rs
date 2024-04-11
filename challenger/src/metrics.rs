use ethers::types::{Address, H256};
use metrics::{counter, describe_counter, describe_gauge, gauge};

const LAST_SCANNED_BLOCK_GAUGE: &str = "challenger_last_scanned_block";
const ERRORS_COUNTER: &str = "challenger_errors_total";
const CHALLENGE_COUNTER: &str = "challenger_challenges_total";

/// `set_last_scanned_block` sets the last scanned block for given `address` and `from` account.
pub fn set_last_scanned_block(address: Address, from: Address, block: i64) {
    let labels = [
        ("address", format!("{:?}", address)),
        (("from"), format!("{:?}", from)),
    ];
    gauge!(LAST_SCANNED_BLOCK_GAUGE, &labels).set(block as f64);
}

/// `inc_errors_counter` increments the errors counter for given `address`, `from` account
pub fn inc_errors_counter(address: Address, from: Address, error: &str) {
    let labels = [
        ("address", format!("{:?}", address)),
        ("from", format!("{:?}", from)),
        ("error", String::from(error)),
    ];
    counter!(ERRORS_COUNTER, &labels).increment(1);
}

/// `inc_challenge_counter` increments the challenges counter for given `address`, `from` account and `tx` hash.
pub fn inc_challenge_counter(address: Address, from: Address, tx: H256) {
    let labels = [
        ("address", format!("{:?}", address)),
        ("from", format!("{:?}", from)),
        ("tx", format!("{:?}", tx)),
    ];
    counter!(CHALLENGE_COUNTER, &labels).increment(1);
}

/// `register_custom_metrics` registers custom metrics to the registry.
/// It have to be called before you start processing & if you plans to serve `/metrics` route.
pub fn describe() {
    describe_counter!(
        ERRORS_COUNTER,
        "Counts different errors in challenger process by given address"
    );
    describe_counter!(
        CHALLENGE_COUNTER,
        "Counts happened challenges for given address and from account"
    );
    describe_gauge!(
        LAST_SCANNED_BLOCK_GAUGE,
        "Keeps track of last scanned block for given address and from account"
    );
}
